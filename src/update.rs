//! Uses `curl` and `sh` (PowerShell on Windows), which every system has, so Kanzo
//! needs no HTTP client of its own.

#[cfg(unix)]
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

const RELEASES: &str = "https://github.com/emanuele-em/kanzo/releases/latest/download";

const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[cfg(not(unix))]
const SERVICE_STOP: Duration = Duration::from_secs(10);

static LATEST: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

fn releases() -> String {
    std::env::var("KANZO_RELEASES_URL").unwrap_or_else(|_| RELEASES.into())
}

fn curl(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl").args(["-fsSL", "--max-time", "20", url]).output().ok()?;
    out.status.success().then_some(out.stdout)
}

pub fn latest(now: bool) -> Option<String> {
    let mut cached = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, version)) = &*cached
        && !now
        && at.elapsed() < CHECK_EVERY
    {
        return version.clone();
    }
    let version = curl(&format!("{}/dist-manifest.json", releases())).and_then(|body| {
        let manifest: serde_json::Value = serde_json::from_slice(&body).ok()?;
        let release = manifest["releases"].as_array()?.iter().find(|r| r["app_name"] == "kanzo")?;
        release["app_version"].as_str().map(String::from)
    });
    *cached = Some((Instant::now(), version.clone()));
    version
}

pub fn newer(version: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect() };
    parts(version) > parts(CURRENT)
}

pub fn install() -> Result<PathBuf, String> {
    let exe = program().map_err(|e| e.to_string())?;
    // The installer writes to <dir>/bin, so it can only replace a program kept in a bin folder.
    let dir = match exe.parent() {
        Some(bin) if bin.file_name().is_some_and(|n| n == "bin") => bin.parent().unwrap_or(bin).to_path_buf(),
        _ => {
            return Err(format!(
                "Kanzo runs from {}, which the installer can't update; reinstall it with the command in the README",
                exe.display()
            ));
        }
    };
    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let script = curl(&format!("{}/kanzo-installer.{ext}", releases())).ok_or("cannot download the installer")?;
    // Windows can't replace a running program, but can rename it; see `remove_old_exes`.
    let old = old_exe(&exe);
    if cfg!(windows) {
        std::fs::rename(&exe, &old).map_err(|e| format!("cannot move {}: {e}", exe.display()))?;
    }
    let out = run_installer(&script, &dir);
    if cfg!(windows) && !out.as_ref().is_ok_and(|o| o.status.success()) {
        let _ = std::fs::rename(&old, &exe);
    }
    let out = out?;
    if !out.status.success() {
        let log = String::from_utf8_lossy(&out.stderr);
        let last = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
        return Err(format!("the installer failed: {}", last.trim()));
    }
    Ok(exe)
}

/// Where this program is, past any symlink. On Windows that comes back as `\\?\C:\…`, which
/// the PowerShell installer can't install to, so it goes back to `C:\…`.
fn program() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    match exe.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(plain) if plain.as_bytes().get(1) == Some(&b':') => Ok(PathBuf::from(plain)),
        _ => Ok(exe),
    }
}

/// Named for its version: an agent's `kanzo mcp` can keep an older one running, and locked.
fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension(format!("exe.{CURRENT}.old"))
}

pub fn remove_old_exes(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else { return };
    let prefix = format!("{}.", name.to_string_lossy());
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if file.starts_with(&prefix) && file.ends_with(".old") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn run_installer(script: &[u8], dir: &Path) -> Result<Output, String> {
    // PowerShell runs a script from a file, not from stdin.
    let file = std::env::temp_dir().join("kanzo-installer.ps1");
    let mut cmd = if cfg!(windows) {
        std::fs::write(&file, script).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        let mut ps = Command::new("powershell");
        ps.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&file).stdin(Stdio::null());
        ps
    } else {
        let mut sh = Command::new("sh");
        sh.arg("-s").stdin(Stdio::piped());
        sh
    };
    let out = (|| {
        let mut child = cmd
            .env("KANZO_INSTALL_DIR", dir)
            .env("KANZO_NO_MODIFY_PATH", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run the installer: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script).map_err(|e| e.to_string())?;
        }
        child.wait_with_output().map_err(|e| e.to_string())
    })();
    if cfg!(windows) {
        let _ = std::fs::remove_file(&file);
    }
    out
}

#[cfg(unix)]
fn restart_args(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.collect();
    // First, since it isn't an option of a subcommand like `network on`.
    if !args.iter().any(|a| a == "--no-open") {
        args.insert(0, "--no-open".into());
    }
    args
}

/// Same process, so the port, the data folder and a running service stay.
#[cfg(unix)]
pub fn restart(exe: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    Command::new(exe).args(restart_args(std::env::args_os().skip(1))).exec()
}

#[cfg(not(unix))]
pub fn restart(_exe: &Path) -> std::io::Error {
    if let Err(e) = crate::service::restart() {
        return std::io::Error::other(e.to_string());
    }
    std::thread::sleep(SERVICE_STOP);
    std::io::Error::other("the service did not restart")
}

/// On Windows only the service's board can restart: a running program can't replace itself there.
pub fn can_restart(home: &Path) -> bool {
    cfg!(unix) || crate::service::is_task_board(home)
}

/// The program file this process started from. `kanzo update`, the installer, Homebrew or
/// `cargo install` replace it with a new file, and a running process keeps the old one, so
/// each long-running process watches for that and runs the new program in its place.
#[cfg(unix)]
#[derive(Clone)]
pub struct Exe {
    path: PathBuf,
    stamp: (u64, std::time::SystemTime),
}

#[cfg(unix)]
impl Exe {
    pub fn current() -> Option<Exe> {
        let path = std::env::current_exe().ok()?;
        let stamp = stamp(&path)?;
        Some(Exe { path, stamp })
    }

    /// One `stat`, cheap enough to check on every message.
    pub fn changed(&self) -> bool {
        stamp(&self.path).is_some_and(|s| s != self.stamp)
    }

    /// The new program, once it runs: a file still being written doesn't, and the next check tries again.
    pub fn replacement(&self) -> Option<&Path> {
        let runs = Command::new(&self.path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        (self.changed() && runs).then_some(&self.path)
    }
}

#[cfg(unix)]
fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.ino(), meta.modified().ok()?))
}

/// Runs the new program as this process, with the same arguments.
#[cfg(unix)]
pub fn exec(exe: &Path, env: Option<(&str, &std::ffi::OsStr)>) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    if let Some((key, value)) = env {
        cmd.env(key, value);
    }
    cmd.exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("99.0.0"));
        assert!(!newer(CURRENT));
        assert!(!newer("0.0.1"));
        assert!(!newer("not a version"));
    }

    #[test]
    fn removes_only_old_programs() {
        let tmp = tempfile::tempdir().unwrap();
        let keep = ["kanzo.exe", "kanzo.exe.bak", "other.exe.old", "kanzo.old"];
        for f in keep.iter().chain(&["kanzo.exe.0.2.1.old", "kanzo.exe.0.3.0.old"]) {
            std::fs::write(tmp.path().join(f), "").unwrap();
        }
        remove_old_exes(&tmp.path().join("kanzo.exe"));
        let mut left: Vec<_> =
            std::fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["kanzo.exe", "kanzo.exe.bak", "kanzo.old", "other.exe.old"]);
    }

    #[test]
    fn the_installer_gets_a_folder_it_can_use() {
        let exe = program().unwrap();
        assert!(exe.is_absolute() && exe.is_file(), "{}", exe.display());
        assert!(!exe.to_string_lossy().starts_with(r"\\?\"), "{}", exe.display());
    }

    #[test]
    #[cfg(unix)]
    fn restarts_without_opening_a_tab() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(restart_args(args(&["--port", "4300"]).into_iter()), args(&["--no-open", "--port", "4300"]));
        assert_eq!(restart_args(args(&["network", "on"]).into_iter()), args(&["--no-open", "network", "on"]));
        assert_eq!(restart_args(args(&["--no-open"]).into_iter()), args(&["--no-open"]));
    }
}
