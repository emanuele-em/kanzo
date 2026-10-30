//! The data folder: where it is, what it holds, and the settings in `config.toml`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::store::{self, ImagesOf, Store};

const DEFAULT_PORT: u16 = 4242;

const CONFIG_TEMPLATE: &str = r#"# Kanzo settings.

# Port for the board at http://localhost:<port>.
port = 4242

# Address the board listens on. 127.0.0.1 keeps it on this machine. Use
# "0.0.0.0" to open it from other devices, for example when Kanzo runs on a
# Raspberry Pi with no screen. There are no logins: anyone who can reach the
# port can read and change the board, and use its MCP endpoint.
listen = "127.0.0.1"

# Also serve MCP over HTTP at http://localhost:<port>/mcp, for agents
# that cannot start `kanzo mcp` themselves.
mcp_http = true
"#;

const GITIGNORE: &str = "\
# SQLite temporary files; kanzo sync makes sure kanzo.db is complete without them.
kanzo.db-journal
kanzo.db-wal
kanzo.db-shm
# Local to this machine.
kanzo.lock
*.bak
# macOS metadata (AppleDouble files appear on exFAT drives).
._*
.DS_Store
";

/// How long an operation waits for `kanzo sync` before giving up with "busy".
const LOCK_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });

pub const NO_LOCKS: &str = "This folder doesn't support file locks: don't sync while agents are working.";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub listen: IpAddr,
    /// Before 0.2 this set the columns; they're fixed now, with an optional backlog
    /// switched on from the board. Still read, so older files keep working.
    #[serde(rename = "statuses")]
    _statuses: Option<toml::Value>,
    pub mcp_http: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, listen: IpAddr::V4(Ipv4Addr::LOCALHOST), _statuses: None, mcp_http: true }
    }
}

/// An opened data folder.
#[derive(Debug, Clone)]
pub struct Home {
    pub dir: PathBuf,
    pub config: Config,
}

impl Home {
    /// `--home`, then `KANZO_HOME`, then `~/kanzo`.
    pub fn resolve(flag: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(dir) = flag {
            return Ok(dir);
        }
        if let Some(dir) = std::env::var_os("KANZO_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        let home = dirs::home_dir().context("cannot find your home folder; set KANZO_HOME")?;
        Ok(home.join("kanzo"))
    }

    /// Opens the data folder, creating it and its files on first use.
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let dir = dir.canonicalize().unwrap_or(dir);
        write_if_missing(&dir.join("config.toml"), CONFIG_TEMPLATE)?;
        write_if_missing(&dir.join(".gitignore"), GITIGNORE)?;

        let path = dir.join("config.toml");
        let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let config: Config = toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;

        let home = Self { dir, config };
        home.store()?.migrate()?;
        Ok(home)
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join("kanzo.db")
    }

    /// Changes when the database file itself is replaced, as `kanzo sync` does
    /// when it pulls, so the board reloads even if the pulled change counter
    /// happens to equal the old one.
    pub fn db_identity(&self) -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(self.db_path()).map(|m| m.ino()).unwrap_or(0)
        }
        // No inode on Windows; a replaced file has a new modified time. Not the creation
        // time: NTFS keeps it for a file replaced under the same name.
        #[cfg(not(unix))]
        {
            let modified = fs::metadata(self.db_path()).and_then(|m| m.modified()).ok();
            let since_epoch = modified.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
            since_epoch.map_or(0, |d| d.as_nanos() as u64)
        }
    }

    /// Opens a fresh connection. Callers keep it for one operation only, so a
    /// database replaced by `kanzo sync` is picked up on the next one. It holds
    /// a shared lock on `kanzo.lock`, so sync can wait for it to finish; while
    /// sync holds the lock, this waits up to `LOCK_WAIT` and then reports busy.
    pub fn store(&self) -> store::Result<Store> {
        let lock = self.lock_file().map_err(|e| store::Error::Invalid(format!("{e:#}")))?;
        let deadline = Instant::now() + LOCK_WAIT;
        let lock = loop {
            match lock.try_lock_shared() {
                Ok(()) => break Some(lock),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(store::Error::Busy("kanzo sync is updating the board; try again in a moment".into()));
                }
                Err(TryLockError::Error(e)) if unsupported(&e) => {
                    static WARN: std::sync::Once = std::sync::Once::new();
                    WARN.call_once(|| eprintln!("kanzo: {NO_LOCKS}"));
                    break None;
                }
                Err(TryLockError::Error(e)) => {
                    return Err(store::Error::Invalid(format!("cannot lock kanzo.lock: {e}")));
                }
            }
        };
        Store::open(&self.db_path(), lock)
    }

    /// Runs `delete`, then deletes the image files of `of` that nothing uses any more:
    /// the same image can be attached in several places, and is stored once.
    pub fn deleting_images<T>(
        &self,
        store: &Store,
        of: ImagesOf,
        delete: impl FnOnce() -> store::Result<T>,
    ) -> store::Result<T> {
        let files = store.image_files(of)?;
        let deleted = delete()?;
        for file in store.unused_images(files)? {
            let _ = fs::remove_file(self.dir.join("images").join(file));
        }
        Ok(deleted)
    }

    /// Blocks until no connection is open, and keeps new ones out until the file is dropped.
    /// `None` when the filesystem has no locks.
    pub fn lock_exclusive(&self) -> Result<Option<File>> {
        let lock = self.lock_file()?;
        match lock.lock() {
            Ok(()) => Ok(Some(lock)),
            Err(e) if unsupported(&e) => Ok(None),
            Err(e) => Err(e).context("cannot lock kanzo.lock"),
        }
    }

    fn lock_file(&self) -> Result<File> {
        let path = self.dir.join("kanzo.lock");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))
    }
}

/// Some filesystems (exFAT, network shares) have no file locks: Kanzo goes on without.
fn unsupported(e: &io::Error) -> bool {
    #[cfg(unix)]
    if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::ENOLCK)) {
        return true;
    }
    e.kind() == io::ErrorKind::Unsupported
}

fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_open_creates_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let home = Home::open(dir.clone()).unwrap();
        assert!(dir.join("config.toml").exists());
        assert!(dir.join(".gitignore").exists());
        assert!(dir.join("kanzo.db").exists());
        assert_eq!(home.config.port, DEFAULT_PORT);
        assert!(home.config.mcp_http);
        assert!(home.config.listen.is_loopback());
    }

    #[test]
    fn statuses_from_before_0_2_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), "statuses = [\"todo\", \"doing\", \"done\"]\n").unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!(home.store().unwrap().statuses(), ["todo", "doing", "review", "done"]);
    }

    #[test]
    fn waits_for_sync_then_reports_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        let lock = home.lock_exclusive().unwrap();
        assert!(matches!(home.store(), Err(store::Error::Busy(_))));

        // An operation started during a replacement waits, then goes on once it's done.
        let waiting = std::thread::spawn({
            let home = home.clone();
            move || home.store().map(|s| s.statuses().len())
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiting.is_finished());
        drop(lock);
        assert_eq!(waiting.join().unwrap().unwrap(), 4);
    }

    #[test]
    fn no_lock_only_when_unsupported() {
        assert!(unsupported(&io::Error::from(io::ErrorKind::Unsupported)));
        #[cfg(unix)]
        assert!(unsupported(&io::Error::from_raw_os_error(libc::ENOLCK)));
        assert!(!unsupported(&io::Error::from(io::ErrorKind::PermissionDenied)));
    }
}
