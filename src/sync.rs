use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::{Home, NO_LOCKS};
use crate::store::Store;

pub enum Keep {
    Local,
    Remote,
}

#[derive(Debug)]
pub struct Diverged {
    pub message: String,
    pub first: bool,
}

impl std::fmt::Display for Diverged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Diverged {}

pub fn remote_url(home: &Home) -> Option<String> {
    remote(&home.dir).ok()?.map(|(_, url)| url)
}

pub fn set_remote(home: &Home, url: &str, prompt: bool) -> Result<String> {
    let dir = &home.dir;
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        bail!("Paste the URL of a git repository, like https://github.com/you/kanzo-data");
    }
    // Git would read it as an option, like --upload-pack, which runs a command.
    if url.starts_with('-') {
        bail!("That's not a repo URL: it starts with '-'.");
    }
    let mut failure = String::new();
    let mut found = None;
    for candidate in candidates(url) {
        match git(dir, prompt, &["ls-remote", "--symref", "--", &candidate, "HEAD"]) {
            Ok(out) => {
                found = Some((candidate, out));
                break;
            }
            Err(e) => failure = reason(&format!("{e:#}")),
        }
    }
    let Some((url, head)) = found else {
        bail!("Can't reach that repo. Check the URL, and that this machine can push to it.\n{failure}");
    };
    if !dir.join(".git").exists() {
        // The repo's own branch, so this machine joins the board already there.
        let branch =
            head.lines().find_map(|l| l.strip_prefix("ref: refs/heads/")?.split('\t').next()).unwrap_or("main");
        git(dir, prompt, &["init", "--quiet", "--initial-branch", branch])?;
    }
    let current = remote(dir)?;
    if current.as_ref().map(|(_, u)| u.as_str()) != Some(url.as_str()) {
        // Removing the old remote forgets its branches, so the next sync starts afresh.
        if let Some((old, _)) = current {
            git(dir, prompt, &["remote", "remove", &old])?;
        }
        git(dir, prompt, &["remote", "add", "--", "origin", &url])?;
    }
    Ok(url)
}

fn reason(error: &str) -> String {
    let lines: Vec<&str> = error.lines().map(str::trim).collect();
    if error.contains("terminal prompts disabled") {
        return "Git asked to sign in: sign in on this machine first (`gh auth login` for GitHub), \
                or use the repo's SSH URL."
            .into();
    }
    let pick = |prefix: &str| lines.iter().find(|l| l.starts_with(prefix)).copied();
    let line = pick("ERROR:")
        .or_else(|| pick("Permission denied"))
        .or_else(|| pick("Host key verification failed"))
        .or_else(|| pick("fatal:"))
        .unwrap_or_else(|| lines.last().copied().unwrap_or_default());
    line.trim_start_matches("ERROR:").trim_start_matches("fatal:").trim().to_string()
}

/// SSH and HTTPS for `owner/repo`, as either may be the one this machine can push with.
fn candidates(url: &str) -> Vec<String> {
    let short = url.strip_prefix("github.com/").unwrap_or(url);
    let shorthand = !short.contains(':')
        && short.split('/').filter(|p| !p.is_empty()).count() == 2
        && !short.starts_with(['.', '/', '~'])
        && !short.contains(char::is_whitespace);
    if !shorthand {
        return vec![url.to_string()];
    }
    let repo = short.trim_end_matches(".git");
    vec![format!("git@github.com:{repo}.git"), format!("https://github.com/{repo}.git")]
}

fn remote(dir: &Path) -> Result<Option<(String, String)>> {
    if !dir.join(".git").exists() {
        return Ok(None);
    }
    let out = git(dir, false, &["remote", "-v"])?;
    let mut remotes: Vec<(&str, &str)> = out
        .lines()
        .filter_map(|line| line.split_once('\t').and_then(|(n, rest)| Some((n, rest.strip_suffix(" (fetch)")?))))
        .collect();
    remotes.sort_by_key(|(name, _)| *name != "origin");
    Ok(remotes.first().map(|(name, url)| (name.to_string(), url.to_string())))
}

pub fn sync(home: &Home, keep: Option<Keep>, prompt: bool) -> Result<String> {
    let dir = &home.dir;
    let git = |args: &[&str]| git(dir, prompt, args);
    let git_ok = |args: &[&str]| git_ok(dir, prompt, args);
    let Some((remote, _)) = remote(dir)? else {
        bail!(
            "Sync isn't set up yet. Create an empty private repo, on GitHub for example, then press the \
             sync button on the board and paste its URL, or run:\n  kanzo sync --repo <url>"
        );
    };
    git(&["fetch", "--quiet", "--prune", &remote])?;

    // Wait for open connections to finish and keep new ones out while the
    // database is staged. The lock is held only while git reads or replaces
    // kanzo.db, never across a commit (hooks, signing prompts) or the network.
    let note = {
        let lock = home.lock_exclusive()?;
        Store::open(&home.db_path(), None)?.settle()?;
        // This machine's settings, and what it does on the network, stay here. A config.toml already
        // in the board stays too: taking it out would delete the file on machines with an older Kanzo
        // when they pull, and a Pi would forget its listen.
        git(&["add", "--all", "--", ".", ":(exclude)config.toml", &format!(":(exclude){}", crate::share::FILE)])?;
        if lock.is_some() { String::new() } else { format!("\n{NO_LOCKS}") }
    };
    if !git_ok(&["diff", "--cached", "--quiet"]) {
        let message = format!("kanzo sync from {}", crate::home::hostname());
        // A machine where git doesn't know the user yet still syncs.
        let named = git_ok(&["config", "user.email"]);
        let anonymous = ["-c", "user.name=Kanzo", "-c", "user.email=kanzo@localhost"];
        let mut args: Vec<&str> = if named { vec![] } else { anonymous.to_vec() };
        args.extend(["commit", "--quiet", "--message", &message]);
        git(&args)?;
    }

    let mut branch = git(&["symbolic-ref", "--short", "HEAD"])?;
    if !git_ok(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{remote}/{branch}")]) {
        let prefix = format!("{remote}/");
        let others: Vec<String> = git(&["branch", "--remotes", "--format=%(refname:short)"])?
            .lines()
            .filter_map(|b| b.strip_prefix(&prefix))
            .filter(|b| *b != "HEAD" && !b.is_empty())
            .map(String::from)
            .collect();
        match &others[..] {
            [] => {
                git(&["push", "--quiet", "--set-upstream", &remote, &branch])?;
                return Ok(format!("Pushed the board to {remote}/{branch} for the first time.{note}"));
            }
            // The repo's board is on another branch, like master: follow it, rather than
            // start a second board next to it.
            [other] => {
                git(&["branch", "--move", other])?;
                branch = other.clone();
            }
            _ => bail!(
                "{remote} has several branches ({}), and none is {branch}, so it's not clear which holds \
                 the board. Sync with an empty repo, or one used only by Kanzo.",
                others.join(", ")
            ),
        }
    }
    let upstream = format!("{remote}/{branch}");
    let first = !git_ok(&["rev-parse", "--verify", "--quiet", "@{upstream}"]);
    if first {
        git(&["branch", "--quiet", "--set-upstream-to", &upstream])?;
    }

    let counts = git(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
    let mut numbers = counts.split_whitespace().map(str::parse::<u32>);
    let (Some(Ok(ahead)), Some(Ok(behind)), None) = (numbers.next(), numbers.next(), numbers.next()) else {
        bail!("unexpected git output: {counts}");
    };

    let done = match (ahead, behind, keep) {
        (0, 0, _) => format!("Already in sync with {upstream}."),
        (_, 0, _) => {
            git(&["push", "--quiet"])?;
            format!("Pushed this machine's changes to {upstream}.")
        }
        (0, _, _) => {
            replace(home, || git(&["merge", "--quiet", "--ff-only", "@{upstream}"]))?;
            format!("Pulled the latest board from {upstream}.")
        }
        (_, _, None) => {
            let what = if first {
                format!("{upstream} already holds a board.")
            } else {
                format!(
                    "This machine and {upstream} both changed the board since the last sync, and a database file can't be merged."
                )
            };
            return Err(Diverged {
                message: format!(
                    "{what} Keep one side:\n  \
                     kanzo sync --keep-remote  use the board in {upstream} here (this machine's is kept in kanzo.db.bak)\n  \
                     kanzo sync --keep-local   overwrite {upstream} with this machine's board"
                ),
                first,
            }
            .into());
        }
        (_, _, Some(Keep::Local)) => {
            git(&["push", "--quiet", "--force-with-lease"])?;
            format!("Overwrote {upstream} with this machine's board.")
        }
        (_, _, Some(Keep::Remote)) => {
            replace(home, || {
                std::fs::copy(home.db_path(), dir.join("kanzo.db.bak")).context("cannot back up kanzo.db")?;
                git(&["reset", "--quiet", "--hard", "@{upstream}"])
            })?;
            format!("Took the board from {upstream}; this machine's old board is in kanzo.db.bak.")
        }
    };
    Ok(format!("{done}{note}"))
}

fn replace<T>(home: &Home, pull: impl FnOnce() -> Result<T>) -> Result<()> {
    let _lock = home.lock_exclusive()?;
    // Rolls back a write a crashed process left in kanzo.db-journal, which SQLite
    // would otherwise apply to the pulled file.
    Store::open(&home.db_path(), None)?.settle()?;
    // A board an older Kanzo synced has a config.toml in it, which a pull could change, delete, or
    // stop at. This machine's own steps aside, so git never sees it, and comes back as it was.
    let config = home.dir.join("config.toml");
    let in_board = |at: &str| git_ok(&home.dir, false, &["cat-file", "-e", &format!("{at}:config.toml")]);
    let mine = if in_board("HEAD") || in_board("@{upstream}") { std::fs::read(&config).ok() } else { None };
    if mine.is_some() {
        std::fs::remove_file(&config).context("cannot move config.toml aside")?;
    }
    let pulled = pull();
    if let Some(mine) = mine {
        crate::home::write_whole(&config, &mine).context("cannot put config.toml back")?;
    }
    pulled?;
    Store::open(&home.db_path(), None)?.migrate()?;
    Ok(())
}

fn command(dir: &Path, prompt: bool, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    if !prompt {
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    cmd
}

fn git(dir: &Path, prompt: bool, args: &[&str]) -> Result<String> {
    crate::home::output(&mut command(dir, prompt, args))
}

fn git_ok(dir: &Path, prompt: bool, args: &[&str]) -> bool {
    git(dir, prompt, args).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(dir: &Path, remote: &Path) -> Home {
        let home = Home::open(dir.to_path_buf()).unwrap();
        set_remote(&home, remote.to_str().unwrap(), false).unwrap();
        home
    }

    fn two_machines(tmp: &Path) -> (Home, Home) {
        let remote = tmp.join("remote.git");
        git(tmp, false, &["init", "--quiet", "--bare", remote.to_str().unwrap()]).unwrap();
        let a = machine(&tmp.join("a"), &remote);
        sync(&a, None, false).unwrap();
        let b = machine(&tmp.join("b"), &remote);
        b.store().unwrap().create_project("local", "", None).unwrap();
        sync(&b, Some(Keep::Remote), false).unwrap();
        (a, b)
    }

    #[test]
    fn refuses_options_as_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        let err = set_remote(&home, "--upload-pack=touch pwned;", false).unwrap_err();
        assert!(err.to_string().contains("starts with '-'"));
        assert!(!tmp.path().join("pwned").exists());
    }

    #[test]
    fn keep_remote_keeps_the_old_board() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, b) = two_machines(tmp.path());
        assert!(b.store().unwrap().list_projects().unwrap().is_empty());
        let bak = Store::open(&b.dir.join("kanzo.db.bak"), None).unwrap();
        assert_eq!(bak.list_projects().unwrap()[0].slug, "local");
        let old = git(&b.dir, false, &["log", "-1", "--format=%s", "HEAD@{1}"]).unwrap();
        assert!(old.starts_with("kanzo sync from"));
    }

    #[test]
    fn a_failed_migration_leaves_the_pulled_board() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = two_machines(tmp.path());
        let db = rusqlite::Connection::open(a.db_path()).unwrap();
        db.execute("UPDATE meta SET value = '999' WHERE key IN ('schema_version', 'compatible')", []).unwrap();
        drop(db);
        sync(&a, None, false).unwrap();

        let newer = "saved by a newer version of Kanzo";
        assert!(sync(&b, None, false).unwrap_err().to_string().contains(newer));
        let db = rusqlite::Connection::open(b.db_path()).unwrap();
        let version: String =
            db.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(version, "999");
        assert!(format!("{:#}", Home::open(b.dir.clone()).unwrap_err()).contains(newer));
    }

    #[test]
    fn config_toml_stays_on_each_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let (pi, laptop) = two_machines(tmp.path());
        std::fs::write(pi.dir.join("config.toml"), "listen = \"0.0.0.0\"\n").unwrap();
        pi.store().unwrap().create_project("pi", "", None).unwrap();
        sync(&pi, None, false).unwrap();
        sync(&laptop, None, false).unwrap();

        assert_eq!(laptop.store().unwrap().list_projects().unwrap()[0].slug, "pi");
        assert!(Home::open(laptop.dir.clone()).unwrap().config.listen.is_loopback());
        assert!(!git(&pi.dir, false, &["ls-files"]).unwrap().contains("config.toml"));
    }

    /// What sync did up to 0.3.1: commit everything but network, config.toml too, then pull or push.
    fn older_sync(dir: &Path) {
        let git = |args: &[&str]| git(dir, false, args).unwrap();
        git(&["fetch", "--quiet"]);
        git(&["add", "--all", "--", ".", ":(exclude)network"]);
        if !git_ok(dir, false, &["diff", "--cached", "--quiet"]) {
            git(&["-c", "user.name=Kanzo", "-c", "user.email=kanzo@localhost", "commit", "--quiet", "-m", "older"]);
        }
        if git_ok(dir, false, &["rev-parse", "--verify", "--quiet", "@{upstream}"]) {
            git(&["merge", "--quiet", "--ff-only", "@{upstream}"]);
        }
        git(&["push", "--quiet", "--set-upstream", "origin", "HEAD"]);
    }

    /// A Pi on the network whose board an older Kanzo synced, config.toml and all.
    fn older_pi(tmp: &Path) -> (std::path::PathBuf, Home) {
        let remote = tmp.join("remote.git");
        git(tmp, false, &["init", "--quiet", "--bare", remote.to_str().unwrap()]).unwrap();
        let pi = machine(&tmp.join("pi"), &remote);
        std::fs::write(pi.dir.join("config.toml"), "listen = \"0.0.0.0\"\n").unwrap();
        older_sync(&pi.dir);
        (remote, pi)
    }

    #[test]
    fn a_pi_on_an_older_kanzo_keeps_its_listen() {
        let tmp = tempfile::tempdir().unwrap();
        let (remote, pi) = older_pi(tmp.path());
        let laptop = machine(&tmp.path().join("laptop"), &remote);
        sync(&laptop, Some(Keep::Remote), false).unwrap();
        assert!(Home::open(laptop.dir.clone()).unwrap().config.listen.is_loopback());

        laptop.store().unwrap().create_project("laptop", "", None).unwrap();
        sync(&laptop, None, false).unwrap();
        older_sync(&pi.dir);
        assert_eq!(pi.store().unwrap().list_projects().unwrap().len(), 1);
        assert!(!Home::open(pi.dir.clone()).unwrap().config.listen.is_loopback());

        // Then the Pi updates too.
        laptop.store().unwrap().create_project("later", "", None).unwrap();
        sync(&laptop, None, false).unwrap();
        sync(&pi, None, false).unwrap();
        assert_eq!(pi.store().unwrap().list_projects().unwrap().len(), 2);
        assert!(!Home::open(pi.dir.clone()).unwrap().config.listen.is_loopback());
    }

    #[test]
    fn a_pi_keeps_its_listen_when_a_board_without_config_toml_replaces_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let (remote, pi) = older_pi(tmp.path());
        let laptop = machine(&tmp.path().join("laptop"), &remote);
        sync(&laptop, Some(Keep::Local), false).unwrap();
        sync(&pi, Some(Keep::Remote), false).unwrap();

        assert!(!Home::open(pi.dir.clone()).unwrap().config.listen.is_loopback());
    }

    #[test]
    fn config_toml_from_an_older_kanzo_stays_there() {
        let tmp = tempfile::tempdir().unwrap();
        let (laptop, pi) = two_machines(tmp.path());
        // The first sync adds it to the board, the second changes it.
        for config in ["listen = \"0.0.0.0\"\n", "listen = \"0.0.0.0\"\nport = 4300\n"] {
            std::fs::write(pi.dir.join("config.toml"), config).unwrap();
            older_sync(&pi.dir);
            sync(&laptop, None, false).unwrap();
            let kept = Home::open(laptop.dir.clone()).unwrap().config;
            assert!(kept.listen.is_loopback());
            assert_eq!(kept.port, crate::home::DEFAULT_PORT);
        }
    }

    #[test]
    fn urls() {
        let ssh = "git@github.com:you/kanzo-data.git";
        let https = "https://github.com/you/kanzo-data.git";
        assert_eq!(candidates("you/kanzo-data"), [ssh, https]);
        assert_eq!(candidates("github.com/you/kanzo-data.git"), [ssh, https]);
        assert_eq!(candidates("https://github.com/you/kanzo-data"), ["https://github.com/you/kanzo-data"]);
        assert_eq!(candidates(ssh), [ssh]);
        assert_eq!(candidates("/srv/git/kanzo.git"), ["/srv/git/kanzo.git"]);
        assert_eq!(candidates("../kanzo.git"), ["../kanzo.git"]);
    }

    #[test]
    fn reasons() {
        let missing = "git ls-remote failed:\nERROR: Repository not found.\nfatal: Could not read from remote repository.\n\n\
                       Please make sure you have the correct access rights\nand the repository exists.";
        assert_eq!(reason(missing), "Repository not found.");
        assert_eq!(
            reason("git ls-remote failed:\nfatal: '/x' does not appear to be a git repository"),
            "'/x' does not appear to be a git repository"
        );
        assert!(
            reason("fatal: could not read Username for 'https://github.com': terminal prompts disabled")
                .contains("gh auth login")
        );
    }
}
