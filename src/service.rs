//! `kanzo service`: run the board in the background, started at login.
//! A LaunchAgent on macOS, a systemd user unit on Linux (a Raspberry Pi, an old laptop),
//! a Task Scheduler task on Windows.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::Home;

const LABEL: &str = "com.github.emanuele-em.kanzo";
const UNIT: &str = "kanzo.service";
const TASK: &str = "Kanzo";

pub fn install(home: &Home) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find the kanzo program")?;
    if cfg!(target_os = "macos") {
        install_launchd(home, &exe)
    } else if cfg!(target_os = "linux") {
        install_systemd(home, &exe)
    } else if cfg!(windows) {
        install_task(home, &exe)
    } else {
        bail!("kanzo service supports macOS, Linux and Windows; start `kanzo --no-open` with your own service manager")
    }
}

pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else if cfg!(target_os = "linux") {
        uninstall_systemd()
    } else if cfg!(windows) {
        uninstall_task()
    } else {
        bail!("kanzo service supports macOS, Linux and Windows")
    }
}

/// Restarts the service, if it's installed, so it runs a newly installed Kanzo; false
/// when there's none.
pub fn restart() -> Result<bool> {
    if cfg!(target_os = "macos") && plist_path()?.exists() {
        run("launchctl", &["kickstart", "-k", &format!("{}/{LABEL}", launchd_domain()?)])?;
    } else if cfg!(target_os = "linux") && unit_path()?.exists() {
        run("systemctl", &["--user", "restart", UNIT])?;
    } else if cfg!(windows) && task_exists() {
        restart_task()?;
    } else {
        return Ok(false);
    }
    Ok(true)
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("cannot find your home folder")
}

/// Runs a command and fails with its own error output.
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("cannot run {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

// macOS

fn plist_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn launchd_domain() -> Result<String> {
    let uid = Command::new("id").arg("-u").output().context("cannot run id -u")?;
    Ok(format!("gui/{}", String::from_utf8_lossy(&uid.stdout).trim()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist(exe: &Path, dir: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--no-open</string>
        <string>--home</string>
        <string>{dir}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.to_string_lossy()),
        dir = xml_escape(&dir.to_string_lossy()),
        log = xml_escape(&log.to_string_lossy()),
    )
}

fn install_launchd(home: &Home, exe: &Path) -> Result<()> {
    let log = home_dir()?.join("Library/Logs/kanzo.log");
    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().expect("plist path has a parent"))?;
    let domain = launchd_domain()?;
    // Replace an older install so a changed path or data folder takes effect.
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
    std::fs::write(&path, plist(exe, &home.dir, &log)).with_context(|| format!("cannot write {}", path.display()))?;
    run("launchctl", &["bootstrap", &domain, &path.to_string_lossy()])?;
    println!(
        "Kanzo now starts at login and is running at http://localhost:{}\nLogs: {}",
        home.config.port,
        log.display()
    );
    Ok(())
}

fn uninstall_launchd() -> Result<()> {
    let path = plist_path()?;
    let _ = Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", launchd_domain()?)]).output();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        println!("Removed the Kanzo LaunchAgent; the board no longer starts at login.");
    } else {
        println!("The Kanzo LaunchAgent was not installed.");
    }
    Ok(())
}

// Linux

fn unit_path() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir()?.join(".config"),
    };
    Ok(config.join("systemd/user").join(UNIT))
}

/// Quotes one argument for a systemd `ExecStart=` line.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

fn unit(exe: &Path, dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Kanzo board\n\
         After=network.target\n\
         \n\
         [Service]\n\
         ExecStart={} --no-open --home {}\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(&exe.to_string_lossy()),
        systemd_quote(&dir.to_string_lossy()),
    )
}

fn install_systemd(home: &Home, exe: &Path) -> Result<()> {
    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().expect("unit path has a parent"))?;
    std::fs::write(&path, unit(exe, &home.dir)).with_context(|| format!("cannot write {}", path.display()))?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", UNIT])?;
    // Restart rather than start, so a changed path or data folder takes effect.
    run("systemctl", &["--user", "restart", UNIT])?;
    println!(
        "Kanzo now starts at login and is running at http://localhost:{}\n\
         Logs: journalctl --user -u kanzo\n\
         To start it at boot without logging in (a board with no screen): sudo loginctl enable-linger $USER",
        home.config.port
    );
    Ok(())
}

fn uninstall_systemd() -> Result<()> {
    let path = unit_path()?;
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", UNIT]).output();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        println!("Removed the Kanzo service; the board no longer starts at login.");
    } else {
        println!("The Kanzo service was not installed.");
    }
    Ok(())
}

// Windows

/// Stops the board, which may be this process (cmd outlives it), waits 3 s for it to let
/// go of its port (`timeout` needs a console), then starts it. `/end` stops conhost; the
/// board then exits with its console (see `web::exit_with_console`).
fn restart_task() -> Result<()> {
    let line = format!("schtasks /end /tn {TASK} & ping -n 4 127.0.0.1 >nul & schtasks /run /tn {TASK}");
    let mut cmd = Command::new("cmd");
    cmd.args(["/c", &line]);
    // A console of its own (CREATE_NO_WINDOW): the board's goes away with `/end`, and
    // takes the processes in it along.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.output().context("cannot run cmd")?;
    if !out.status.success() {
        bail!("cannot restart the Kanzo task: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Whether this process is the board the Windows task starts: the task's arguments, and
/// the task is installed.
pub fn is_task_board(dir: &Path) -> bool {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    args == ["--no-open".into(), "--home".into(), dir.as_os_str().to_owned()] && task_exists()
}

fn task_exists() -> bool {
    Command::new("schtasks").args(["/query", "/tn", TASK]).output().is_ok_and(|o| o.status.success())
}

/// A logon task that runs the board in the user's session, so it can use their saved
/// git credentials, through `conhost --headless`, so it has no window. S4U has no
/// window either, but can't read Credential Manager and needs an elevated shell.
fn task(exe: &Path, dir: &Path, user: &str) -> String {
    let esc = |s: &str| xml_escape(s).replace('"', "&quot;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger><UserId>{user}</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType></Principal>
  </Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
  </Settings>
  <Actions>
    <Exec><Command>conhost.exe</Command><Arguments>--headless {exe} --no-open --home {dir}</Arguments></Exec>
  </Actions>
</Task>
"#,
        user = esc(user),
        exe = esc(&format!("\"{}\"", exe.display())),
        dir = esc(&format!("\"{}\"", dir.display())),
    )
}

/// `domain\user`, from Windows in UTF-8: `whoami` prints in the console's code page, and
/// %USERDOMAIN% is WORKGROUP over SSH.
fn account() -> Result<String> {
    let name =
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8; [Security.Principal.WindowsIdentity]::GetCurrent().Name";
    let out =
        Command::new("powershell").args(["-NoProfile", "-Command", name]).output().context("cannot run powershell")?;
    let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || user.is_empty() {
        bail!("cannot find your Windows account: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(user)
}

fn install_task(home: &Home, exe: &Path) -> Result<()> {
    let user = account()?;
    let xml = std::env::temp_dir().join("kanzo-task.xml");
    // schtasks reads the XML as UTF-16, as the declaration says.
    let utf16: Vec<u8> =
        std::iter::once(0xFEFF).chain(task(exe, &home.dir, &user).encode_utf16()).flat_map(u16::to_le_bytes).collect();
    std::fs::write(&xml, utf16).with_context(|| format!("cannot write {}", xml.display()))?;
    let created = run("schtasks", &["/create", "/tn", TASK, "/xml", &xml.to_string_lossy(), "/f"]);
    let _ = std::fs::remove_file(&xml);
    created?;
    // Restart rather than start, so a changed path or data folder takes effect.
    restart_task()?;
    println!(
        "Kanzo now starts at login and is running at http://localhost:{}\n\
         It has no window and no log; to see its errors, run `kanzo service uninstall`, then `kanzo --no-open`.",
        home.config.port
    );
    Ok(())
}

fn uninstall_task() -> Result<()> {
    if !task_exists() {
        println!("The Kanzo task was not installed.");
        return Ok(());
    }
    let _ = Command::new("schtasks").args(["/end", "/tn", TASK]).output();
    run("schtasks", &["/delete", "/tn", TASK, "/f"])?;
    println!("Removed the Kanzo task; the board no longer starts at login.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn plist_is_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("agent.plist");
        let text = plist(Path::new("/usr/local/bin/kanzo"), Path::new("/Users/a & b/kanzo"), Path::new("/tmp/k.log"));
        std::fs::write(&file, &text).unwrap();
        let lint = Command::new("plutil").arg("-lint").arg(&file).output().unwrap();
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
        assert!(text.contains("/Users/a &amp; b/kanzo"));
    }

    #[test]
    fn unit_quotes_paths() {
        let text = unit(Path::new("/home/pi/.cargo/bin/kanzo"), Path::new("/home/pi/my \"kanzo\" 100%"));
        assert!(
            text.contains(r#"ExecStart="/home/pi/.cargo/bin/kanzo" --no-open --home "/home/pi/my \"kanzo\" 100%%""#),
            "{text}"
        );
        assert!(text.contains("WantedBy=default.target"));
    }

    #[test]
    fn task_quotes_paths() {
        let text = task(Path::new(r"C:\Users\a&b\.cargo\bin\kanzo.exe"), Path::new(r"C:\Users\a&b\kanzo"), r"pc\a&b");
        assert!(
            text.contains(
                r"<Arguments>--headless &quot;C:\Users\a&amp;b\.cargo\bin\kanzo.exe&quot; --no-open --home &quot;C:\Users\a&amp;b\kanzo&quot;</Arguments>"
            ),
            "{text}"
        );
        assert!(text.contains(r"<UserId>pc\a&amp;b</UserId>"));
    }
}
