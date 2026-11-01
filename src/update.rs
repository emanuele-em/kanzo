//! Updating Kanzo from the board: find the latest release, run its installer over this
//! program, then start the new one in place. It uses `curl` and `sh` (PowerShell on
//! Windows), which every system has, so Kanzo needs no HTTP client of its own.

#[cfg(unix)]
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Where releases are published; `KANZO_RELEASES_URL` points elsewhere, for tests.
const RELEASES: &str = "https://github.com/emanuele-em/kanzo/releases/latest/download";

/// How long a check for the latest version is reused: the board checks once a day.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a board waits for the service to stop it after an update, on Windows.
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

/// The latest released version, checked at most once a day unless `now`; `None` when
/// it can't be found, like offline.
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

/// True when `version` is newer than this program: `0.2.0` > `0.1.9`.
pub fn newer(version: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect() };
    parts(version) > parts(CURRENT)
}

/// Runs the latest release's installer over this program and returns its path,
/// ready to restart.
pub fn install() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).map_err(|e| e.to_string())?;
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

/// Where Windows keeps the running program while the installer replaces it. Named for
/// its version: an agent's `kanzo mcp` can keep an older one running, and locked.
fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension(format!("exe.{CURRENT}.old"))
}

/// Deletes the programs that updates replaced `exe` with on Windows (see `old_exe`),
/// except those still running.
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

/// The arguments to start again with: the same ones, without opening another browser tab.
#[cfg(unix)]
fn restart_args(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.collect();
    if !args.iter().any(|a| a == "--no-open") {
        args.push("--no-open".into());
    }
    args
}

/// Replaces this process with the program at `exe`: same arguments, same process, so
/// the port, the data folder and a running service stay. Returns only on failure.
#[cfg(unix)]
pub fn restart(exe: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    Command::new(exe).args(restart_args(std::env::args_os().skip(1))).exec()
}

/// Restarts the service, which stops this process; see `can_restart`.
#[cfg(not(unix))]
pub fn restart(_exe: &Path) -> std::io::Error {
    if let Err(e) = crate::service::restart() {
        return std::io::Error::other(e.to_string());
    }
    std::thread::sleep(SERVICE_STOP);
    std::io::Error::other("the service did not restart")
}

/// Whether `restart` can start the new program in this one's place. On Windows only the
/// service's board can: a running program can't replace itself there.
pub fn can_restart(home: &Path) -> bool {
    cfg!(unix) || crate::service::is_task_board(home)
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
    #[cfg(unix)]
    fn restarts_without_opening_a_tab() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(restart_args(args(&["--port", "4300"]).into_iter()), args(&["--port", "4300", "--no-open"]));
        assert_eq!(restart_args(args(&["--no-open"]).into_iter()), args(&["--no-open"]));
    }
}
