//! The database uses a rollback journal (`journal_mode = TRUNCATE`), not WAL. With a
//! connection per operation, WAL would set up and tear down its `-wal` and `-shm` files
//! on every request; the rollback journal makes a read touch no extra files at all and
//! measured about 3x faster per operation.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// The oldest schema version whose Kanzo can still use a database at the latest one.
/// Adding a table or a column with a default leaves older versions working, so a board
/// synced from a newer Kanzo still opens in an older one. Raise it to the new version
/// only for a migration that older code can't live with, like a renamed column.
const COMPATIBLE_FROM: usize = 4;

const MIGRATIONS: &[&str] = &[
    include_str!("schema.sql"),
    "ALTER TABLE projects ADD COLUMN color TEXT;",
    "ALTER TABLE tasks ADD COLUMN tags TEXT NOT NULL DEFAULT '';",
    // The old projects.repo column stays, empty: SQLite can only drop it by rebuilding the table.
    "CREATE TABLE project_repos (
         repo       TEXT PRIMARY KEY,
         project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE
     );
     CREATE INDEX project_repos_by_project ON project_repos(project_id);
     INSERT INTO project_repos (repo, project_id) SELECT repo, id FROM projects WHERE repo IS NOT NULL;
     UPDATE projects SET repo = NULL;
     CREATE TRIGGER project_repos_insert AFTER INSERT ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER project_repos_delete AFTER DELETE ON project_repos BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    "CREATE TABLE images (
         id         INTEGER PRIMARY KEY AUTOINCREMENT,
         task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         comment_id INTEGER REFERENCES comments(id) ON DELETE CASCADE,
         file       TEXT NOT NULL,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
     );
     CREATE INDEX images_by_task ON images(task_id);
     CREATE TRIGGER images_insert AFTER INSERT ON images BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER images_delete AFTER DELETE ON images BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    "ALTER TABLE tasks ADD COLUMN human INTEGER NOT NULL DEFAULT 0;",
    "ALTER TABLE tasks ADD COLUMN url TEXT NOT NULL DEFAULT '';",
    "CREATE TABLE task_waits (
         task_id     INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         waits_on_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
         PRIMARY KEY (task_id, waits_on_id)
     ) WITHOUT ROWID;
     CREATE INDEX task_waits_by_blocker ON task_waits(waits_on_id);
     CREATE TRIGGER task_waits_insert AFTER INSERT ON task_waits BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;
     CREATE TRIGGER task_waits_delete AFTER DELETE ON task_waits BEGIN UPDATE meta SET value = value + 1 WHERE key = 'changes'; END;",
    "ALTER TABLE projects ADD COLUMN position REAL;",
    // What an agent's request_id remembers, so a retry replays the first outcome. No triggers: a
    // replay changes nothing, so it must not wake the board. No foreign keys: a receipt outlives
    // the task and the comment it names. Receipts are never deleted: a retry may come long after.
    "CREATE TABLE requests (
         caller     TEXT NOT NULL,
         request_id TEXT NOT NULL,
         method     TEXT NOT NULL,
         request    TEXT NOT NULL,
         task_id    INTEGER,
         comment_id INTEGER,
         reply      TEXT,
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
         PRIMARY KEY (caller, request_id)
     ) WITHOUT ROWID;",
];

const PROJECT_COLORS: &[&str] = &["gray", "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
    Busy(String),
    Db(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(m) | Error::Invalid(m) | Error::Busy(m) => f.write_str(m),
            Error::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub repos: Vec<String>,
    pub color: Option<String>,
    pub created_at: String,
    pub open: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    /// A link to anything, like the task's pull request; empty when there is none.
    pub url: String,
    pub human: bool,
    pub status: String,
    pub position: f64,
    pub created_at: String,
    pub updated_at: String,
    pub comments: i64,
    /// The tasks this one waits on that aren't done yet. Left out when there are none, as on most tasks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<i64>,
}

impl Task {
    pub fn key(&self) -> String {
        format!("{}#{}", self.project, self.id)
    }
}

/// A task another one waits on or blocks.
#[derive(Debug, Clone, Serialize)]
pub struct TaskLink {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: i64,
    pub task_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub id: i64,
    pub task_id: i64,
    pub comment_id: Option<i64>,
    pub file: String,
}

#[derive(Debug, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub project_id: Option<i64>,
    pub tags: Option<String>,
    pub url: Option<String>,
    pub human: Option<bool>,
    /// Task keys, comma-separated; it replaces the list.
    pub waits_on: Option<String>,
}

/// The longest `request_id`, in bytes.
const REQUEST_ID_MAX: usize = 128;
/// The longest client or computer name, and the longest folder, a request_id is kept with, in bytes.
const CALLER_NAME_MAX: usize = 128;
const CALLER_FOLDER_MAX: usize = 1024;

/// A task as `create_task_in` writes it, checked and cleaned.
struct NewTask {
    title: String,
    status: String,
    description: String,
    tags: String,
    human: bool,
    waits_on: Option<String>,
}

/// A `TaskPatch` as `update_task` writes it: cleaned, but not checked against the board.
///
/// `new` and `request` both name every field of a `TaskPatch` and leave none to `..`, so a field
/// added to it, like a comment or a guard on its version, doesn't compile until it is cleaned
/// here and is part of the request of a retry. It can't change what a request does without
/// changing what the request is.
struct Edit {
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    project_id: Option<i64>,
    tags: Option<String>,
    url: Option<String>,
    human: Option<bool>,
    waits_on: Option<String>,
}

impl Edit {
    fn new(patch: TaskPatch) -> Result<Self> {
        let TaskPatch { title, description, status, project_id, tags, url, human, waits_on } = patch;
        Ok(Self {
            title: title.as_deref().map(clean_title).transpose()?,
            url: url.as_deref().map(clean_url).transpose()?,
            description: description.map(|d| d.trim().to_string()),
            tags: tags.map(|t| clean_tags(&t).join(",")),
            waits_on: waits_on.as_deref().map(clean_waits),
            status,
            project_id,
            human,
        })
    }

    /// The task as the caller named it, and every field, with `-` for one not passed.
    fn request(&self, slug: Option<&str>, id: i64) -> String {
        let Self { title, description, status, project_id, tags, url, human, waits_on } = self;
        let field = |value: Option<&str>| value.map_or("-".to_string(), |v| format!("+{v}"));
        canon(&[
            "update_task",
            slug.unwrap_or(""),
            &id.to_string(),
            &field(title.as_deref()),
            &field(description.as_deref()),
            &field(status.as_deref()),
            &field(project_id.map(|p| p.to_string()).as_deref()),
            &field(tags.as_deref()),
            &field(url.as_deref()),
            &field(human.map(|h| if h { "1" } else { "0" })),
            &field(waits_on.as_deref()),
        ])
    }
}

/// An agent's `request_id` and who sent it. The same id from a different client name, computer or
/// folder is a different request: those are told apart exactly, never hashed. They are what the
/// caller says about itself, so this keeps one agent's retries apart from another's and is not
/// access control: anyone who can reach the board can change it anyway.
pub struct Retry {
    caller: String,
    id: String,
}

impl Retry {
    /// The id must be 1-128 bytes with no whitespace or control characters; one that isn't is an
    /// error, not a call without retry protection.
    pub fn new(client: &str, computer: &str, folder: &str, id: &str) -> Result<Self> {
        if id.is_empty() || id.len() > REQUEST_ID_MAX || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::Invalid(format!(
                "request_id must be 1-{REQUEST_ID_MAX} bytes, with no spaces or control characters"
            )));
        }
        let parts = [("client name", client, CALLER_NAME_MAX), ("computer name", computer, CALLER_NAME_MAX)];
        for (what, part, max) in parts.into_iter().chain([("folder", folder, CALLER_FOLDER_MAX)]) {
            if part.len() > max {
                return Err(Error::Invalid(format!(
                    "the {what} is too long to use request_id with ({max} bytes at most)"
                )));
            }
        }
        Ok(Self { caller: canon(&[client, computer, folder]), id: id.to_string() })
    }
}

/// What a request left: the task or comment it made or took, and the reply to give again. The
/// reply is saved as it was first given, unless it names paths or other things that belong to
/// whoever reads it, like a taken task's details; then it is `None`, and made again from the task.
#[derive(Debug, PartialEq)]
pub struct Receipt {
    pub task: Option<i64>,
    pub comment: Option<i64>,
    pub reply: Option<String>,
    /// Found in the receipts, not done now.
    pub replayed: bool,
}

/// Parts in one string that can't be mixed up: each starts with its length, like `3:api2:12`.
fn canon(parts: &[&str]) -> String {
    parts.iter().map(|p| format!("{}:{p}", p.len())).collect()
}

const STATUSES: [&str; 4] = ["todo", "doing", "review", "done"];

pub const DONE: &str = STATUSES[3];

/// Where `next_task` puts the task it takes.
pub const DOING: &str = STATUSES[1];

pub const BACKLOG: &str = "backlog";

/// What a task shows besides its id, title and status; each can be turned off in Settings.
const PROPERTIES: [&str; 9] =
    ["description", "tags", "url", "human", "waiting", "created", "updated", "comments", "images"];

pub struct Store {
    conn: Connection,
    statuses: Vec<String>,
    /// Declared after `conn` so it is released after the connection closes.
    _lock: Option<std::fs::File>,
}

impl Store {
    pub fn open(path: &Path, lock: Option<std::fs::File>) -> Result<Self> {
        let conn = Connection::open(path)?;
        // The board and any number of agents share this file; wait instead of failing.
        conn.busy_timeout(Duration::from_secs(5))?;
        // Per connection, not stored in the file. Also turns a WAL database from an
        // older kanzo back into a rollback-journal one once nothing else has it open.
        // `synchronous` stays at SQLite's default FULL, which keeps the file intact
        // when a board loses power.
        conn.pragma_update_and_check(None, "journal_mode", "TRUNCATE", |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Before the first migration there is no meta table, and so no backlog.
        let backlog = conn
            .query_row("SELECT value FROM meta WHERE key = 'backlog'", [], |r| r.get::<_, String>(0))
            .is_ok_and(|v| v == "1");
        let statuses = backlog.then_some(BACKLOG).into_iter().chain(STATUSES).map(String::from).collect();
        Ok(Self { conn, statuses, _lock: lock })
    }

    pub fn statuses(&self) -> &[String] {
        &self.statuses
    }

    pub fn set_backlog(&self, on: bool) -> Result<()> {
        self.write(|c| {
            let left: i64 = c.query_row("SELECT count(*) FROM tasks WHERE status = ?1", [BACKLOG], |r| r.get(0))?;
            if !on && left > 0 {
                return Err(Error::Invalid(format!("move the {left} {} out of backlog first", tasks(left))));
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('backlog', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [if on { "1" } else { "0" }],
            )?;
            // Other open boards redraw their columns.
            c.execute("UPDATE meta SET value = value + 1 WHERE key = 'changes'", [])?;
            Ok(())
        })
    }

    /// The properties turned off in Settings: off the board, and out of what agents get.
    pub fn hidden_properties(&self) -> Result<Vec<String>> {
        let hidden: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'hidden_properties'", [], |r| r.get(0))
            .optional()?;
        Ok(hidden.unwrap_or_default().split(',').filter(|p| !p.is_empty()).map(String::from).collect())
    }

    pub fn set_property(&self, property: &str, visible: bool) -> Result<()> {
        if !PROPERTIES.contains(&property) {
            return Err(Error::Invalid(format!("unknown property {property:?}")));
        }
        self.write(|c| {
            let mut hidden = self.hidden_properties()?;
            hidden.retain(|p| p != property);
            if !visible {
                hidden.push(property.to_string());
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('hidden_properties', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [hidden.join(",")],
            )?;
            // Other open boards redraw their tasks.
            c.execute("UPDATE meta SET value = value + 1 WHERE key = 'changes'", [])?;
            Ok(())
        })
    }

    /// Immediate, so concurrent writers queue up on the busy timeout instead of failing halfway.
    fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&self.conn) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// `run` and its receipt are one write, so they happen together or not at all, and when callers
    /// race with the same request, one runs it and the others find its receipt. A request that
    /// differs from the one first made with this id is refused before `run` does anything. An
    /// error from `run` rolls everything back and keeps no receipt, so the id stays free.
    fn once(
        &self,
        retry: &Retry,
        method: &str,
        request: &str,
        run: impl FnOnce(&Connection) -> Result<Receipt>,
    ) -> Result<Receipt> {
        self.write(|c| {
            let found = c
                .query_row(
                    "SELECT method, request, task_id, comment_id, reply FROM requests
                     WHERE caller = ?1 AND request_id = ?2",
                    params![retry.caller, retry.id],
                    |r| {
                        let receipt = Receipt { task: r.get(2)?, comment: r.get(3)?, reply: r.get(4)?, replayed: true };
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, receipt))
                    },
                )
                .optional()?;
            if let Some((was, same, receipt)) = found {
                if was != method || same != request {
                    return Err(Error::Invalid(format!(
                        "request_id {:?} was already used for a different request; use a new one",
                        retry.id
                    )));
                }
                return Ok(receipt);
            }
            let receipt = run(c)?;
            c.execute(
                "INSERT INTO requests (caller, request_id, method, request, task_id, comment_id, reply)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![retry.caller, retry.id, method, request, receipt.task, receipt.comment, receipt.reply],
            )?;
            Ok(receipt)
        })
    }

    /// Writes nothing when up to date, so starting `kanzo mcp` for every agent session
    /// costs no disk write.
    pub fn migrate(&self) -> Result<()> {
        let meta = |c: &Connection, key: &str| -> rusqlite::Result<Option<usize>> {
            Ok(c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get::<_, String>(0))
                .optional()?
                .and_then(|v| v.parse().ok()))
        };
        let latest = MIGRATIONS.len();
        let current =
            (meta(&self.conn, "schema_version").unwrap_or(None), meta(&self.conn, "compatible").unwrap_or(None));
        if current == (Some(latest), Some(COMPATIBLE_FROM)) {
            return Ok(());
        }
        self.write(|c| {
            c.execute("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", [])?;
            let version = meta(c, "schema_version")?.unwrap_or(0);
            if version > latest {
                // From a newer Kanzo, maybe synced from another machine: use it as it is
                // when this version can, without touching its schema.
                if meta(c, "compatible")?.is_some_and(|oldest| oldest <= latest) {
                    return Ok(());
                }
                return Err(Error::Invalid(
                    "this board was saved by a newer version of Kanzo; update this one with `kanzo update`".into(),
                ));
            }
            for sql in &MIGRATIONS[version..] {
                c.execute_batch(sql)?;
            }
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('compatible', ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [latest.to_string(), COMPATIBLE_FROM.to_string()],
            )?;
            Ok(())
        })
    }

    pub fn changes(&self) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'changes'")?
            .query_row([], |r| r.get(0))?)
    }

    pub fn now(&self) -> Result<String> {
        Ok(self.conn.query_row(&format!("SELECT {NOW}"), [], |r| r.get(0))?)
    }

    /// Makes `kanzo.db` complete on its own: rolls back a write that a crashed process
    /// left half done, and folds in a WAL left by an older kanzo.
    pub fn settle(&self) -> Result<()> {
        self.conn.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))?;
        Ok(())
    }

    const PROJECT_SELECT: &str = "SELECT p.id, p.slug, p.name,
            (SELECT group_concat(repo, ',' ORDER BY r.rowid) FROM project_repos r WHERE r.project_id = p.id),
            p.color, p.created_at,
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id AND t.status <> ?1),
            (SELECT count(*) FROM tasks t WHERE t.project_id = p.id)
        FROM projects p";

    fn project_row(r: &Row) -> rusqlite::Result<Project> {
        Ok(Project {
            id: r.get(0)?,
            slug: r.get(1)?,
            name: r.get(2)?,
            repos: r.get::<_, Option<String>>(3)?.map(|s| s.split(',').map(String::from).collect()).unwrap_or_default(),
            color: r.get(4)?,
            created_at: r.get(5)?,
            open: r.get(6)?,
            total: r.get(7)?,
        })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let sql = format!("{} ORDER BY p.position IS NULL, p.position, p.slug", Self::PROJECT_SELECT);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([DONE], Self::project_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn project_where(&self, clause: &str, value: &str) -> Result<Option<Project>> {
        let sql = format!("{} WHERE {clause}", Self::PROJECT_SELECT);
        Ok(self.conn.query_row(&sql, params![DONE, value], Self::project_row).optional()?)
    }

    pub fn project_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        self.project_where("p.slug = ?2", &slug.trim().to_ascii_lowercase())
    }

    pub fn project_by_repo(&self, repo: &str) -> Result<Option<Project>> {
        match repo_from_url(repo) {
            Some(repo) => self.project_where("p.id = (SELECT project_id FROM project_repos WHERE repo = ?2)", &repo),
            None => Ok(None),
        }
    }

    pub fn find_project(&self, name: &str) -> Result<Project> {
        let found = match self.project_by_slug(name)? {
            Some(p) => Some(p),
            None if name.contains('/') => self.project_by_repo(name)?,
            None => None,
        };
        found.ok_or_else(|| {
            let known = self.list_projects().unwrap_or_default();
            let names: Vec<_> = known.iter().map(|p| p.slug.as_str()).collect();
            let hint = if names.is_empty() {
                "there are no projects yet".to_string()
            } else {
                format!("projects: {}", names.join(", "))
            };
            Error::NotFound(format!("no project {name:?} ({hint})"))
        })
    }

    pub fn create_project(&self, slug: &str, name: &str, repo: Option<&str>) -> Result<Project> {
        let slug = slug.trim().to_ascii_lowercase();
        if !valid_slug(&slug) {
            return Err(Error::Invalid(format!(
                "project slug {slug:?} must be 1-32 lowercase letters, digits or -, starting with a letter or digit"
            )));
        }
        let name = match name.trim() {
            "" => slug.clone(),
            n => n.to_string(),
        };
        let repo = repo.map(str::trim).filter(|r| !r.is_empty()).map(clean_repo).transpose()?;
        // Checked in the write's transaction, so nobody can take the slug or the repo in between.
        self.write(|c| {
            if self.project_by_slug(&slug)?.is_some() {
                return Err(Error::Invalid(format!("project {slug} already exists")));
            }
            if let Some(r) = &repo {
                self.check_repo_free(r)?;
            }
            c.execute("INSERT INTO projects (slug, name) VALUES (?1, ?2)", params![slug, name])?;
            if let Some(r) = &repo {
                c.execute(
                    "INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)",
                    params![r, c.last_insert_rowid()],
                )?;
            }
            Ok(())
        })?;
        self.find_project(&slug)
    }

    pub fn link_repo(&self, slug: &str, repo: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        let repo = clean_repo(repo)?;
        if project.repos.contains(&repo) {
            return Ok(project);
        }
        self.write(|c| {
            self.check_repo_free(&repo)?;
            c.execute("INSERT INTO project_repos (repo, project_id) VALUES (?1, ?2)", params![repo, project.id])?;
            Ok(())
        })?;
        self.find_project(&project.slug)
    }

    pub fn unlink_repo(&self, slug: &str, repo: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        let repo = repo_from_url(repo).unwrap_or_else(|| repo.trim().to_ascii_lowercase());
        let removed = self
            .conn
            .execute("DELETE FROM project_repos WHERE repo = ?1 AND project_id = ?2", params![repo, project.id])?;
        if removed == 0 {
            return Err(Error::NotFound(format!("repo {repo} is not linked to project {}", project.slug)));
        }
        self.find_project(&project.slug)
    }

    fn check_repo_free(&self, repo: &str) -> Result<()> {
        match self.project_by_repo(repo)? {
            Some(other) => Err(Error::Invalid(format!("repo {repo} is already linked to project {}", other.slug))),
            None => Ok(()),
        }
    }

    pub fn rename_project(&self, slug: &str, name: &str) -> Result<Project> {
        let name = squash(name);
        if name.is_empty() {
            return Err(Error::Invalid("project name is empty".into()));
        }
        let project = self.find_project(slug)?;
        self.conn.execute("UPDATE projects SET name = ?1 WHERE id = ?2", params![name, project.id])?;
        self.find_project(&project.slug)
    }

    pub fn set_project_color(&self, slug: &str, color: &str) -> Result<Project> {
        if !PROJECT_COLORS.contains(&color) {
            return Err(Error::Invalid(format!("unknown color {color:?}; use one of: {}", PROJECT_COLORS.join(", "))));
        }
        let project = self.find_project(slug)?;
        self.conn.execute("UPDATE projects SET color = ?1 WHERE id = ?2", params![color, project.id])?;
        self.find_project(&project.slug)
    }

    /// Before `before`, or last. A project never moved has no position and comes after those that
    /// have one, by slug, so only a move that lands among those has to number them all.
    pub fn move_project(&self, slug: &str, before: Option<&str>) -> Result<Project> {
        let project = self.find_project(slug)?;
        let before = before.map(|b| self.find_project(b)).transpose()?;
        if before.as_ref().is_some_and(|b| b.id == project.id) {
            return Ok(project);
        }
        self.write(|c| {
            let mut others: Vec<(i64, Option<f64>)> = c
                .prepare("SELECT id, position FROM projects WHERE id <> ?1 ORDER BY position IS NULL, position, slug")?
                .query_map([project.id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let at = before.and_then(|b| others.iter().position(|o| o.0 == b.id)).unwrap_or(others.len());
            let position = match (at.checked_sub(1).map(|i| others[i].1), others.get(at).map(|o| o.1)) {
                (None, Some(Some(next))) => Some(next - 1.0),
                (None, _) => Some(1.0),
                (Some(Some(prev)), Some(Some(next))) if next - prev >= 1e-9 => Some((prev + next) / 2.0),
                (Some(Some(prev)), Some(None) | None) => Some(prev + 1.0),
                _ => None,
            };
            let mut update = c.prepare("UPDATE projects SET position = ?1 WHERE id = ?2")?;
            if let Some(position) = position {
                update.execute(params![position, project.id])?;
            } else {
                others.insert(at, (project.id, None));
                for (i, (id, _)) in others.iter().enumerate() {
                    update.execute(params![(i + 1) as f64, id])?;
                }
            }
            Ok(())
        })?;
        self.find_project(&project.slug)
    }

    pub fn delete_project(&self, slug: &str) -> Result<Project> {
        let project = self.find_project(slug)?;
        // Tasks and comments go with it through ON DELETE CASCADE.
        self.conn.execute("DELETE FROM projects WHERE id = ?1", [project.id])?;
        Ok(project)
    }

    const TASK_SELECT: &str = "SELECT t.id, p.slug, t.title, t.description, t.status, t.position,
            t.created_at, t.updated_at, (SELECT count(*) FROM comments c WHERE c.task_id = t.id), t.tags, t.human,
            t.url
        FROM tasks t JOIN projects p ON p.id = t.project_id";

    fn task_row(r: &Row) -> rusqlite::Result<Task> {
        Ok(Task {
            id: r.get(0)?,
            project: r.get(1)?,
            title: r.get(2)?,
            description: r.get(3)?,
            tags: r.get::<_, String>(9)?.split(',').filter(|t| !t.is_empty()).map(String::from).collect(),
            url: r.get(11)?,
            human: r.get(10)?,
            status: r.get(4)?,
            position: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            comments: r.get(8)?,
            waiting_on: Vec::new(),
        })
    }

    /// One query for all of them, not a subquery per task: on a board of 3,000 tasks that took
    /// /api/board from 5.3 ms to 7.3 ms even with nothing waiting. Now tasks pay only for actual links.
    fn add_waiting(&self, tasks: &mut [Task]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT w.task_id, w.waits_on_id FROM task_waits w JOIN tasks b ON b.id = w.waits_on_id
             WHERE b.status <> 'done' ORDER BY w.waits_on_id",
        )?;
        let mut rows = stmt.query([])?;
        let mut waiting: HashMap<i64, Vec<i64>> = HashMap::new();
        while let Some(r) = rows.next()? {
            waiting.entry(r.get(0)?).or_default().push(r.get(1)?);
        }
        if !waiting.is_empty() {
            for t in tasks {
                t.waiting_on = waiting.remove(&t.id).unwrap_or_default();
            }
        }
        Ok(())
    }

    pub fn list_tasks(&self, project_id: Option<i64>, status: Option<&str>) -> Result<Vec<Task>> {
        Ok(self.list_tasks_keeping_done(project_id, status, None)?.0)
    }

    /// The done column only grows, so only its newest `keep_done` tasks are read.
    pub fn list_tasks_keeping_done(
        &self,
        project_id: Option<i64>,
        status: Option<&str>,
        keep_done: Option<usize>,
    ) -> Result<(Vec<Task>, usize)> {
        let done = DONE;
        // SQLite reads a negative LIMIT as no limit.
        let limit = keep_done.map_or(-1, |n| n as i64);
        let sql = format!(
            "{} WHERE (?1 IS NULL OR t.project_id = ?1) AND (?2 IS NULL OR t.status = ?2)
               AND (t.status <> ?3 OR t.id IN (SELECT id FROM tasks WHERE status = ?3
                    AND (?1 IS NULL OR project_id = ?1) ORDER BY position DESC LIMIT ?4))
             ORDER BY t.position",
            Self::TASK_SELECT
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut tasks: Vec<Task> = stmt
            .query_map(params![project_id, status, done, limit], Self::task_row)?
            .collect::<rusqlite::Result<_>>()?;
        self.add_waiting(&mut tasks)?;
        let column = |t: &Task| self.statuses().iter().position(|s| *s == t.status).unwrap_or(usize::MAX);
        tasks.sort_by_key(column);
        let hidden = match keep_done {
            Some(keep) if status.is_none_or(|s| s == done) => {
                let all: i64 = self.conn.query_row(
                    "SELECT count(*) FROM tasks WHERE status = ?1 AND (?2 IS NULL OR project_id = ?2)",
                    params![done, project_id],
                    |r| r.get(0),
                )?;
                usize::try_from(all).unwrap_or(0).saturating_sub(keep)
            }
            _ => 0,
        };
        Ok((tasks, hidden))
    }

    // SQLite's LIKE ignores case only for ASCII. Registered per query, not in open, because
    // every operation opens a connection and only filters need this.
    fn add_has_word(&self) -> Result<()> {
        let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
        self.conn.create_scalar_function("has_word", 2, flags, |c| {
            Ok(c.get_raw(0).as_str()?.to_lowercase().contains(c.get_raw(1).as_str()?))
        })?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<Vec<Task>> {
        self.search_seeing(query, limit, &[])
    }

    /// Finds nothing through a property Settings hides from agents.
    pub fn search_for_agent(&self, query: &str) -> Result<Vec<Task>> {
        self.search_seeing(query, None, &self.hidden_properties()?)
    }

    fn search_seeing(&self, query: &str, limit: Option<usize>, hidden: &[String]) -> Result<Vec<Task>> {
        self.add_has_word()?;
        let filter = Filter::new(query, 2, hidden);
        // SQLite reads a negative LIMIT as no limit.
        let limit = limit.map_or(-1, |n| n as i64);
        let sql = format!(
            "{} WHERE 1{} ORDER BY t.status = ?1, t.updated_at DESC, t.id DESC LIMIT {limit}",
            Self::TASK_SELECT,
            filter.sql
        );
        let mut values: Vec<String> = vec![DONE.to_string()];
        values.extend(filter.values);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), Self::task_row)?;
        let mut tasks: Vec<Task> = rows.collect::<rusqlite::Result<_>>()?;
        self.add_waiting(&mut tasks)?;
        Ok(tasks)
    }

    pub fn get_task(&self, id: i64) -> Result<Task> {
        let sql = format!("{} WHERE t.id = ?1", Self::TASK_SELECT);
        let mut task = self
            .conn
            .prepare_cached(&sql)?
            .query_row([id], Self::task_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no task #{id}")))?;
        task.waiting_on = self.task_links(id, false)?.into_iter().filter(|t| t.status != DONE).map(|t| t.id).collect();
        Ok(task)
    }

    pub fn find_task(&self, key: &str) -> Result<Task> {
        let (slug, id) = parse_task_key(key)?;
        let task = self.get_task(id)?;
        if let Some(slug) = slug
            && slug != task.project
        {
            return Err(Error::NotFound(format!("no task {key} (#{id} is {})", task.key())));
        }
        Ok(task)
    }

    pub fn check_status(&self, status: &str) -> Result<()> {
        if self.statuses().iter().any(|s| s == status) {
            Ok(())
        } else {
            Err(Error::Invalid(format!("unknown status {status:?}; use one of: {}", self.statuses().join(", "))))
        }
    }

    fn end_of_column(c: &Connection, status: &str, except: i64) -> Result<f64> {
        let max: Option<f64> = c.query_row(
            "SELECT max(position) FROM tasks WHERE status = ?1 AND id <> ?2",
            params![status, except],
            |r| r.get(0),
        )?;
        Ok(max.map_or(1.0, |m| m.floor() + 1.0))
    }

    #[cfg(test)]
    pub fn create_task(&self, project: i64, title: &str, description: &str) -> Result<Task> {
        self.create_task_in(project, title, TaskPatch { description: Some(description.into()), ..Default::default() })
    }

    // One write, or an agent could take a task before it's human, or before it waits.
    // Uses description, status, tags, human, waits_on.
    pub fn create_task_in(&self, project_id: i64, title: &str, patch: TaskPatch) -> Result<Task> {
        let new = Self::new_task(title, patch)?;
        self.check_status(&new.status)?;
        let id = self.write(|c| self.insert_task(c, project_id, &new))?;
        self.get_task(id)
    }

    /// As `create_task_in`, for a request with an id: the task is made once, however often it is sent.
    /// `render` makes the reply, in the same write, and it is kept as it is. A request is the same
    /// as an earlier one when it has the same project, the same cleaned title, description, status,
    /// tags and human, and the same `waits_on` keys, cleaned, in the same order. What depends on
    /// the board, like whether the status exists, is checked only when the request is new, so a
    /// retry never fails on what has changed since.
    pub fn create_task_retry(
        &self,
        project_id: i64,
        title: &str,
        patch: TaskPatch,
        retry: &Retry,
        render: impl FnOnce(&Task) -> Result<String>,
    ) -> Result<Receipt> {
        let new = Self::new_task(title, patch)?;
        let request = canon(&[
            "create_task",
            &project_id.to_string(),
            &new.title,
            &new.description,
            &new.status,
            &new.tags,
            if new.human { "1" } else { "0" },
            new.waits_on.as_deref().unwrap_or(""),
        ]);
        self.once(retry, "create_task", &request, |c| {
            self.check_status(&new.status)?;
            let id = self.insert_task(c, project_id, &new)?;
            let reply = render(&self.get_task(id)?)?;
            Ok(Receipt { task: Some(id), comment: None, reply: Some(reply), replayed: false })
        })
    }

    /// Cleaned, but not checked against the board: the status may not exist.
    fn new_task(title: &str, patch: TaskPatch) -> Result<NewTask> {
        let title = clean_title(title)?;
        Ok(NewTask {
            title,
            status: patch.status.unwrap_or_else(|| STATUSES[0].to_string()),
            description: patch.description.as_deref().unwrap_or("").trim().to_string(),
            tags: clean_tags(patch.tags.as_deref().unwrap_or("")).join(","),
            human: patch.human.unwrap_or(false),
            waits_on: patch.waits_on.as_deref().map(clean_waits),
        })
    }

    /// Inside a write, which the caller has begun.
    fn insert_task(&self, c: &Connection, project_id: i64, new: &NewTask) -> Result<i64> {
        let position = Self::end_of_column(c, &new.status, 0)?;
        c.execute(
            "INSERT INTO tasks (project_id, title, description, status, position, tags, human) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![project_id, new.title, new.description, new.status, position, new.tags, new.human],
        )?;
        let id = c.last_insert_rowid();
        if let Some(keys) = &new.waits_on {
            self.set_waits(c, id, keys)?;
        }
        Ok(id)
    }

    /// Replaces what a task waits on. Part of the task's write, so a refused list changes nothing.
    fn set_waits(&self, c: &Connection, id: i64, keys: &str) -> Result<()> {
        let mut ids = Vec::new();
        for key in keys.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            ids.push(self.find_task(key)?.id);
        }
        ids.sort_unstable();
        ids.dedup();
        if ids.iter().copied().eq(self.task_links(id, false)?.iter().map(|t| t.id)) {
            return Ok(());
        }
        c.execute("DELETE FROM task_waits WHERE task_id = ?1", [id])?;
        for other in ids {
            let loops: bool = c.query_row(
                "WITH RECURSIVE chain(id) AS (
                     SELECT ?1 UNION SELECT w.waits_on_id FROM task_waits w JOIN chain ON w.task_id = chain.id
                 ) SELECT EXISTS (SELECT 1 FROM chain WHERE id = ?2)",
                [other, id],
                |r| r.get(0),
            )?;
            if loops {
                return Err(Error::Invalid(format!("#{id} can't wait on #{other}: that would make a loop")));
            }
            c.execute("INSERT INTO task_waits (task_id, waits_on_id) VALUES (?1, ?2)", [id, other])?;
        }
        c.execute(&format!("UPDATE tasks SET updated_at = {NOW} WHERE id = ?1"), [id])?;
        Ok(())
    }

    /// What a task waits on, done or not, or with `blocks`, the tasks waiting on it.
    pub fn task_links(&self, id: i64, blocks: bool) -> Result<Vec<TaskLink>> {
        let (this, other) = if blocks { ("waits_on_id", "task_id") } else { ("task_id", "waits_on_id") };
        let sql = format!(
            "SELECT t.id, p.slug, t.title, t.status FROM task_waits w JOIN tasks t ON t.id = w.{other}
             JOIN projects p ON p.id = t.project_id WHERE w.{this} = ?1 ORDER BY t.id"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([id], |r| {
            Ok(TaskLink { id: r.get(0)?, project: r.get(1)?, title: r.get(2)?, status: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_task(&self, id: i64, patch: TaskPatch) -> Result<Task> {
        let edit = Edit::new(patch)?;
        if let Some(s) = &edit.status {
            self.check_status(s)?;
        }
        self.write(|c| {
            let current = self.get_task(id)?;
            self.apply_edit(c, &current, &edit)
        })?;
        self.get_task(id)
    }

    /// As `update_task`, for a request with an id: the task is changed once, however often it is
    /// sent, the task being the one `key` names, like `api#12` or `12`. `render` is given the task
    /// before and after, and makes the reply, in the same write; it is kept as it is, so it is not
    /// made from what another writer did next.
    ///
    /// A request is the same as an earlier one when it has the same task key as written (`api#12`,
    /// `12` and `web#12` are three, even where the number is the same task), and each field the
    /// same, cleaned, with a field not passed told apart from one passed empty. The receipt is
    /// looked up first and replayed before anything about the task's present state is looked at:
    /// the checks that depend on it, which are the task existing, the status existing and whatever
    /// else is added, run only for a new request. The request has one place, `Edit::request`, to
    /// take in every field a patch gets.
    pub fn update_task_retry(
        &self,
        key: &str,
        patch: TaskPatch,
        retry: &Retry,
        render: impl FnOnce(&Task, &Task) -> Result<String>,
    ) -> Result<Receipt> {
        let edit = Edit::new(patch)?;
        let (slug, id) = parse_task_key(key)?;
        let request = edit.request(slug.as_deref(), id);
        self.once(retry, "update_task", &request, |c| {
            if let Some(s) = &edit.status {
                self.check_status(s)?;
            }
            let before = self.find_task(key)?;
            self.apply_edit(c, &before, &edit)?;
            let reply = render(&before, &self.get_task(id)?)?;
            Ok(Receipt { task: Some(id), comment: None, reply: Some(reply), replayed: false })
        })
    }

    /// Inside a write, which the caller has begun, with `current` read in it.
    fn apply_edit(&self, c: &Connection, current: &Task, edit: &Edit) -> Result<()> {
        let id = current.id;
        // Only what the patch changes is written, and only then is the task newer.
        c.execute(
            &format!(
                "UPDATE tasks SET title = coalesce(?1, title), project_id = coalesce(?2, project_id),
                   human = coalesce(?3, human), tags = coalesce(?4, tags),
                   description = coalesce(?5, description), url = coalesce(?7, url), updated_at = {NOW}
                 WHERE id = ?6 AND (coalesce(?1, title), coalesce(?2, project_id), coalesce(?3, human),
                   coalesce(?4, tags), coalesce(?5, description), coalesce(?7, url))
                   <> (title, project_id, human, tags, description, url)"
            ),
            params![edit.title, edit.project_id, edit.human, edit.tags, edit.description, id, edit.url],
        )?;
        if let Some(s) = &edit.status
            && *s != current.status
        {
            let position = Self::end_of_column(c, s, id)?;
            c.execute(
                &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                params![s, position, id],
            )?;
        }
        if let Some(keys) = &edit.waits_on {
            self.set_waits(c, id, keys)?;
        }
        Ok(())
    }

    /// In one write: all or nothing, and about 60 times faster than one write per task. Tags are
    /// added and removed, not replaced, so each task keeps its others.
    pub fn update_tasks(&self, ids: &[i64], status: Option<&str>, add_tags: &str, remove_tags: &str) -> Result<()> {
        if let Some(s) = status {
            self.check_status(s)?;
        }
        let (add, remove) = (clean_tags(add_tags), clean_tags(remove_tags));
        self.write(|c| {
            for &id in ids {
                let task = self.get_task(id)?;
                let mut tags = task.tags.clone();
                tags.retain(|t| !remove.contains(t));
                for tag in &add {
                    if !tags.contains(tag) {
                        tags.push(tag.clone());
                    }
                }
                if tags != task.tags {
                    c.execute(
                        &format!("UPDATE tasks SET tags = ?1, updated_at = {NOW} WHERE id = ?2"),
                        params![tags.join(","), id],
                    )?;
                }
                if let Some(s) = status.filter(|s| *s != task.status) {
                    let position = Self::end_of_column(c, s, id)?;
                    c.execute(
                        &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                        params![s, position, id],
                    )?;
                }
            }
            Ok(())
        })
    }

    pub fn move_task(&self, id: i64, status: &str, before: Option<i64>) -> Result<Task> {
        self.check_status(status)?;
        self.write(|c| self.place(c, id, status, before))?;
        self.get_task(id)
    }

    /// In one write, so nobody sees the group half moved. They keep the order of `ids`.
    pub fn move_tasks(&self, ids: &[i64], status: &str, before: Option<i64>) -> Result<()> {
        self.check_status(status)?;
        let before = before.filter(|b| !ids.contains(b));
        self.write(|c| ids.iter().try_for_each(|&id| self.place(c, id, status, before)))
    }

    fn place(&self, c: &Connection, id: i64, status: &str, before: Option<i64>) -> Result<()> {
        let current = self.get_task(id)?;
        let before = before.filter(|b| *b != id);
        let anchor: Option<(i64, f64)> = match before {
            Some(b) => c
                .query_row("SELECT position FROM tasks WHERE id = ?1 AND status = ?2", params![b, status], |r| r.get(0))
                .optional()?
                .map(|p| (b, p)),
            None => None,
        };
        let position = match anchor {
            None => Self::end_of_column(c, status, id)?,
            Some((before, mut next)) => {
                let prev_of = |next: f64| -> Result<Option<f64>> {
                    Ok(c.query_row(
                        "SELECT max(position) FROM tasks WHERE status = ?1 AND position < ?2 AND id <> ?3",
                        params![status, next, id],
                        |r| r.get(0),
                    )?)
                };
                let mut prev = prev_of(next)?;
                if prev.is_some_and(|p| next - p < 1e-9) {
                    // Out of float room between two neighbours.
                    Self::renumber(c, status)?;
                    next = c.query_row("SELECT position FROM tasks WHERE id = ?1", [before], |r| r.get(0))?;
                    prev = prev_of(next)?;
                }
                match prev {
                    Some(p) => (p + next) / 2.0,
                    None => next - 1.0,
                }
            }
        };
        let touched = if current.status != status { format!(", updated_at = {NOW}") } else { String::new() };
        c.execute(
            &format!("UPDATE tasks SET status = ?1, position = ?2{touched} WHERE id = ?3"),
            params![status, position, id],
        )?;
        Ok(())
    }

    fn renumber(c: &Connection, status: &str) -> Result<()> {
        let ids: Vec<i64> = c
            .prepare("SELECT id FROM tasks WHERE status = ?1 ORDER BY position")?
            .query_map([status], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut update = c.prepare("UPDATE tasks SET position = ?1 WHERE id = ?2")?;
        for (i, id) in ids.iter().enumerate() {
            update.execute(params![(i + 1) as f64, id])?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn next_task(&self, project_id: Option<i64>) -> Result<Option<Task>> {
        self.next_task_matching(project_id, "")
    }

    /// The todo tasks for agents that match `query`, from FROM on, with its values.
    fn agents_todo(&self, project_id: Option<i64>, query: &str) -> Result<(String, Vec<Value>)> {
        let filter = if query.trim().is_empty() {
            Filter::default()
        } else {
            self.add_has_word()?;
            Filter::new(query, 3, &self.hidden_properties()?)
        };
        let sql = format!(
            "FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE t.status = ?1 AND t.human = 0 AND (?2 IS NULL OR t.project_id = ?2){}",
            filter.sql
        );
        let mut values: Vec<Value> = vec![STATUSES[0].to_string().into(), project_id.into()];
        values.extend(filter.values.into_iter().map(Value::from));
        Ok((sql, values))
    }

    /// How many tasks `next_task` skips because they wait on others.
    pub fn waiting_in_todo(&self, project_id: Option<i64>, query: &str) -> Result<i64> {
        let (todo, values) = self.agents_todo(project_id, query)?;
        let sql = format!("SELECT count(*) {todo} AND {WAITING}");
        Ok(self.conn.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0))?)
    }

    pub fn next_task_matching(&self, project_id: Option<i64>, query: &str) -> Result<Option<Task>> {
        let (todo, values) = self.agents_todo(project_id, query)?;
        let id = self.write(|c| Self::claim_next(c, &todo, values))?;
        id.map(|id| self.get_task(id)).transpose()
    }

    /// As `next_task_matching`, for a request with an id: it takes a task once, however often it is
    /// sent, and when it found none, that stays its answer however many tasks come after. The
    /// receipt holds the task's id, and `render_empty`'s reply when there was none. A request is the
    /// same as an earlier one when it has the same project (or all) and the same filter, trimmed.
    pub fn next_task_retry(
        &self,
        project_id: Option<i64>,
        query: &str,
        retry: &Retry,
        render_empty: impl FnOnce() -> Result<String>,
    ) -> Result<Receipt> {
        let (todo, values) = self.agents_todo(project_id, query)?;
        let scope = project_id.map_or("all".to_string(), |p| p.to_string());
        let request = canon(&["next_task", &scope, query.trim()]);
        self.once(retry, "next_task", &request, |c| match Self::claim_next(c, &todo, values)? {
            Some(id) => Ok(Receipt { task: Some(id), comment: None, reply: None, replayed: false }),
            None => Ok(Receipt { task: None, comment: None, reply: Some(render_empty()?), replayed: false }),
        })
    }

    /// Inside a write, which the caller has begun.
    fn claim_next(c: &Connection, todo: &str, values: Vec<Value>) -> Result<Option<i64>> {
        let sql = format!("SELECT t.id {todo} AND NOT {WAITING} ORDER BY t.position LIMIT 1");
        let id: Option<i64> = c.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0)).optional()?;
        if let Some(id) = id {
            let position = Self::end_of_column(c, DOING, id)?;
            c.execute(
                &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                params![DOING, position, id],
            )?;
        }
        Ok(id)
    }

    pub fn delete_task(&self, id: i64) -> Result<Task> {
        let task = self.get_task(id)?;
        self.conn.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        Ok(task)
    }

    pub fn delete_tasks(&self, ids: &[i64]) -> Result<()> {
        self.write(|c| {
            for id in ids {
                c.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
            }
            Ok(())
        })
    }

    pub fn list_comments(&self, task_id: i64) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare_cached(&format!("{} WHERE task_id = ?1 ORDER BY id", Self::COMMENT_SELECT))?;
        let rows = stmt.query_map([task_id], Self::comment_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    const COMMENT_SELECT: &str = "SELECT id, task_id, author, body, created_at FROM comments";

    fn comment_row(r: &Row) -> rusqlite::Result<Comment> {
        Ok(Comment { id: r.get(0)?, task_id: r.get(1)?, author: r.get(2)?, body: r.get(3)?, created_at: r.get(4)? })
    }

    pub fn add_comment(&self, task_id: i64, author: &str, body: &str, images: &[String]) -> Result<Comment> {
        let (author, body) = Self::new_comment(author, body, images)?;
        self.get_task(task_id)?;
        let id = self.write(|c| Self::insert_comment(c, task_id, &author, &body, images))?;
        self.get_comment(id)
    }

    /// As `add_comment`, for a request with an id: the comment is added once, however often it is
    /// sent, to the task `key` names, like `api#12` or `12`. The receipt holds the comment's id and
    /// `render`'s reply, kept as it was, so it still answers after the task or comment is deleted,
    /// and then nothing is added again. A request is the same as an earlier one when it has the
    /// same task key as written, the project in it lowercased (`api#12`, `12` and `web#12` are
    /// three, even where the number is the same task, for a number says nothing of which project
    /// the caller meant), and the same cleaned author and body, and images.
    pub fn add_comment_retry(
        &self,
        key: &str,
        author: &str,
        body: &str,
        images: &[String],
        retry: &Retry,
        render: impl FnOnce(&Task) -> Result<String>,
    ) -> Result<Receipt> {
        let (author, body) = Self::new_comment(author, body, images)?;
        let (slug, task_id) = parse_task_key(key)?;
        let request = canon(&[
            "add_comment",
            slug.as_deref().unwrap_or(""),
            &task_id.to_string(),
            &author,
            &body,
            &images.join(","),
        ]);
        self.once(retry, "add_comment", &request, |c| {
            // Only when there is no receipt yet: by a retry, the task may be gone.
            let task = self.find_task(key)?;
            let comment = Self::insert_comment(c, task.id, &author, &body, images)?;
            let reply = render(&task)?;
            Ok(Receipt { task: Some(task.id), comment: Some(comment), reply: Some(reply), replayed: false })
        })
    }

    /// The author and body as they are written, or what is wrong with them.
    fn new_comment(author: &str, body: &str, images: &[String]) -> Result<(String, String)> {
        let body = body.trim();
        if body.is_empty() && images.is_empty() {
            return Err(Error::Invalid("comment body is empty".into()));
        }
        images.iter().try_for_each(|f| check_image(f))?;
        let author = match author.trim() {
            "" => "me",
            a => a,
        };
        Ok((author.to_string(), body.to_string()))
    }

    /// Inside a write, which the caller has begun.
    fn insert_comment(c: &Connection, task_id: i64, author: &str, body: &str, images: &[String]) -> Result<i64> {
        c.execute("INSERT INTO comments (task_id, author, body) VALUES (?1, ?2, ?3)", params![task_id, author, body])?;
        let id = c.last_insert_rowid();
        let mut insert = c.prepare("INSERT INTO images (task_id, comment_id, file) VALUES (?1, ?2, ?3)")?;
        for file in images {
            insert.execute(params![task_id, id, file])?;
        }
        Ok(id)
    }

    fn get_comment(&self, id: i64) -> Result<Comment> {
        self.conn
            .query_row(&format!("{} WHERE id = ?1", Self::COMMENT_SELECT), [id], Self::comment_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no comment {id}")))
    }

    pub fn delete_comment(&self, id: i64) -> Result<Comment> {
        let comment = self.get_comment(id)?;
        self.conn.execute("DELETE FROM comments WHERE id = ?1", [id])?;
        Ok(comment)
    }

    pub fn add_image(&self, task_id: i64, file: &str) -> Result<Image> {
        check_image(file)?;
        self.get_task(task_id)?;
        self.conn.execute("INSERT INTO images (task_id, file) VALUES (?1, ?2)", params![task_id, file])?;
        Ok(Image { id: self.conn.last_insert_rowid(), task_id, comment_id: None, file: file.into() })
    }

    pub fn list_images(&self, task_id: i64) -> Result<Vec<Image>> {
        let mut stmt = self.conn.prepare_cached(&format!("{} WHERE task_id = ?1 ORDER BY id", Self::IMAGE_SELECT))?;
        let rows = stmt.query_map([task_id], Self::image_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    const IMAGE_SELECT: &str = "SELECT id, task_id, comment_id, file FROM images";

    fn image_row(r: &Row) -> rusqlite::Result<Image> {
        Ok(Image { id: r.get(0)?, task_id: r.get(1)?, comment_id: r.get(2)?, file: r.get(3)? })
    }

    pub fn delete_image(&self, id: i64) -> Result<Image> {
        let image = self
            .conn
            .query_row(&format!("{} WHERE id = ?1", Self::IMAGE_SELECT), [id], Self::image_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no image {id}")))?;
        self.conn.execute("DELETE FROM images WHERE id = ?1", [id])?;
        Ok(image)
    }

    pub fn image_files(&self, of: ImagesOf) -> Result<Vec<String>> {
        let (sql, id) = match of {
            ImagesOf::Tasks(ids) => {
                let mut files = Vec::new();
                for id in ids {
                    files.extend(self.image_files(ImagesOf::Task(id))?);
                }
                return Ok(files);
            }
            ImagesOf::Image(id) => ("SELECT file FROM images WHERE id = ?1", id),
            ImagesOf::Comment(id) => ("SELECT DISTINCT file FROM images WHERE comment_id = ?1", id),
            ImagesOf::Task(id) => ("SELECT DISTINCT file FROM images WHERE task_id = ?1", id),
            ImagesOf::Project(id) => {
                ("SELECT DISTINCT i.file FROM images i JOIN tasks t ON t.id = i.task_id WHERE t.project_id = ?1", id)
            }
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unused_images(&self, files: Vec<String>) -> Result<Vec<String>> {
        let mut used = self.conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM images WHERE file = ?1)")?;
        let mut unused = Vec::new();
        for file in files {
            if !used.query_row([&file], |r| r.get::<_, bool>(0))? {
                unused.push(file);
            }
        }
        Ok(unused)
    }
}

pub enum ImagesOf {
    Image(i64),
    Comment(i64),
    Task(i64),
    Tasks(Vec<i64>),
    Project(i64),
}

pub const IMAGE_TYPES: [(&str, &str); 4] =
    [("png", "image/png"), ("jpg", "image/jpeg"), ("gif", "image/gif"), ("webp", "image/webp")];

pub fn tasks(n: i64) -> &'static str {
    if n == 1 { "task" } else { "tasks" }
}

pub fn valid_image_file(file: &str) -> bool {
    match file.split_once('.') {
        Some((hash, ext)) => {
            hash.len() == 16
                && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                && IMAGE_TYPES.iter().any(|(e, _)| *e == ext)
        }
        None => false,
    }
}

fn check_image(file: &str) -> Result<()> {
    if valid_image_file(file) { Ok(()) } else { Err(Error::Invalid(format!("{file:?} is not an image Kanzo saved"))) }
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn short_time(ts: &str) -> String {
    match (ts.get(..10), ts.get(11..16)) {
        (Some(d), Some(t)) => format!("{d} {t} UTC"),
        _ => ts.to_string(),
    }
}

const COMMENT_COUNT: &str = "(SELECT count(*) FROM comments WHERE task_id = t.id)";
const IMAGE_COUNT: &str = "(SELECT count(*) FROM images WHERE task_id = t.id)";
/// Computed, never stored: reopening a task blocks what waits on it again, with nothing to keep in step.
const WAITING: &str = "EXISTS (SELECT 1 FROM task_waits w JOIN tasks b ON b.id = w.waits_on_id
    WHERE w.task_id = t.id AND b.status <> 'done')";
const QUALIFIERS: [&str; 15] = [
    "tag",
    "status",
    "project",
    "title",
    "description",
    "url",
    "comment",
    "id",
    "comments",
    "images",
    "created",
    "updated",
    "is",
    "has",
    "no",
];

/// A query as SQL to add to a WHERE clause: every part must match. A part is a word,
/// `"some words"`, or a qualifier from the README's table, which `-` negates. Anything that
/// isn't a complete qualifier, like `http://x` or `created:soon`, is a word.
#[derive(Default)]
struct Filter {
    sql: String,
    values: Vec<String>,
    first: usize,
}

impl Filter {
    fn new(query: &str, first: usize, hidden: &[String]) -> Self {
        let mut filter = Self { first, ..Default::default() };
        for (part, quoted) in query_parts(query).into_iter().take(10) {
            let part = part.to_lowercase();
            let qualifier = if quoted { None } else { filter.qualifier(&part, hidden) };
            let clause = qualifier.unwrap_or_else(|| filter.word(&part, hidden));
            if !clause.is_empty() {
                filter.sql.push_str(&format!(" AND {clause}"));
            }
        }
        filter
    }

    fn bind(&mut self, value: &str) -> String {
        self.values.push(value.to_string());
        format!("?{}", self.first + self.values.len() - 1)
    }

    fn word(&mut self, word: &str, hidden: &[String]) -> String {
        let seen = |property: &str| !hidden.iter().any(|h| h == property);
        let n = self.bind(word);
        let mut text = String::from("p.slug || '#' || t.id || ' ' || t.title");
        if seen("description") {
            text.push_str(" || ' ' || t.description");
        }
        if seen("tags") {
            text.push_str(" || ' ' || t.tags");
        }
        let mut sql = format!("has_word({text}, {n})");
        if seen("comments") {
            sql.push_str(&format!(" OR t.id IN (SELECT task_id FROM comments WHERE has_word(body, {n}))"));
        }
        format!("({sql})")
    }

    /// `None` when `part` is a plain word. Empty while its value is still being typed.
    fn qualifier(&mut self, part: &str, hidden: &[String]) -> Option<String> {
        let (negated, part) = part.strip_prefix('-').map_or((false, part), |rest| (true, rest));
        let (name, value) = part.split_once(':')?;
        let property = match name {
            "tag" => "tags",
            "comment" => "comments",
            "is" | "has" | "no" => value,
            other => other,
        };
        if hidden.iter().any(|h| h == property) {
            return None;
        }
        if value.is_empty() && QUALIFIERS.contains(&name) {
            return Some(String::new());
        }
        let sql = match (name, value) {
            ("tag", tag) => format!("has_word(',' || t.tags || ',', {})", self.bind(&format!(",{tag},"))),
            ("status", status) => format!("t.status = {}", self.bind(status)),
            ("project", slug) => format!("p.slug = {}", self.bind(slug)),
            ("title", word) => format!("has_word(t.title, {})", self.bind(word)),
            ("description", word) => format!("has_word(t.description, {})", self.bind(word)),
            ("url", word) => format!("has_word(t.url, {})", self.bind(word)),
            ("comment", word) => {
                format!("t.id IN (SELECT task_id FROM comments WHERE has_word(body, {}))", self.bind(word))
            }
            ("id", range) => range_sql("t.id", range, number)?,
            ("comments", range) => range_sql(COMMENT_COUNT, range, number)?,
            ("images", range) => range_sql(IMAGE_COUNT, range, number)?,
            ("created", range) => range_sql("substr(t.created_at, 1, 10)", range, date)?,
            ("updated", range) => range_sql("substr(t.updated_at, 1, 10)", range, date)?,
            ("is", "human") => "t.human".to_string(),
            ("is", "waiting") => WAITING.to_string(),
            ("has" | "no", "description") => "t.description <> ''".to_string(),
            ("has" | "no", "tags") => "t.tags <> ''".to_string(),
            ("has" | "no", "url") => "t.url <> ''".to_string(),
            ("has" | "no", "comments") => format!("{COMMENT_COUNT} > 0"),
            ("has" | "no", "images") => format!("{IMAGE_COUNT} > 0"),
            _ => return None,
        };
        Some(if negated != (name == "no") { format!("NOT ({sql})") } else { sql })
    }
}

/// Splits on spaces outside double quotes, and drops the quotes: `tag:"needs review"` is one
/// part. True when the part starts with a quote, which makes it a word whatever it says.
fn query_parts(query: &str) -> Vec<(String, bool)> {
    let mut parts = Vec::new();
    let (mut part, mut quoted, mut starts_quoted) = (String::new(), false, false);
    for c in query.chars() {
        match c {
            '"' => {
                starts_quoted |= part.is_empty() && !quoted;
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => {
                if !part.is_empty() {
                    parts.push((std::mem::take(&mut part), starts_quoted));
                }
                starts_quoted = false;
            }
            c => part.push(c),
        }
    }
    if !part.is_empty() {
        parts.push((part, starts_quoted));
    }
    parts
}

/// `5`, `>5`, `>=5`, `<5`, `<=5` or `5..9`. `side` only lets through what's safe in SQL.
fn range_sql(column: &str, range: &str, side: fn(&str) -> Option<String>) -> Option<String> {
    if let Some((from, to)) = range.split_once("..") {
        return Some(format!("{column} BETWEEN {} AND {}", side(from)?, side(to)?));
    }
    for op in [">=", "<=", ">", "<"] {
        if let Some(rest) = range.strip_prefix(op) {
            return Some(format!("{column} {op} {}", side(rest)?));
        }
    }
    Some(format!("{column} = {}", side(range)?))
}

fn number(text: &str) -> Option<String> {
    text.parse::<i64>().ok().map(|n| n.to_string())
}

fn date(text: &str) -> Option<String> {
    let dashes = |i: usize, b: u8| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() };
    (text.len() == 10 && text.bytes().enumerate().all(|(i, b)| dashes(i, b))).then(|| format!("'{text}'"))
}

fn clean_tags(input: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in input.split(',') {
        let tag = squash(tag).to_lowercase();
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

/// The keys of a `waits_on`, trimmed, without the empty ones, joined by commas.
fn clean_waits(keys: &str) -> String {
    keys.split(',').map(str::trim).filter(|k| !k.is_empty()).collect::<Vec<_>>().join(",")
}

/// Any site will do, but only http and https: the board makes it a link people click.
fn clean_url(input: &str) -> Result<String> {
    let url = input.trim();
    if url.is_empty() {
        return Ok(String::new());
    }
    let host = url
        .split_once("://")
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next());
    if host.is_none_or(str::is_empty) || url.chars().any(char::is_whitespace) {
        return Err(Error::Invalid(format!("URL {url:?} should start with http:// or https://")));
    }
    Ok(url.to_string())
}

fn clean_repo(repo: &str) -> Result<String> {
    repo_from_url(repo).ok_or_else(|| Error::Invalid(format!("repo {:?} should look like owner/repo", repo.trim())))
}

fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn clean_title(title: &str) -> Result<String> {
    let title = squash(title);
    if title.is_empty() {
        return Err(Error::Invalid("task title is empty".into()));
    }
    Ok(title)
}

fn parse_task_key(key: &str) -> Result<(Option<String>, i64)> {
    let key = key.trim();
    let (slug, num) = match key.rsplit_once('#') {
        Some((s, n)) => (Some(s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()), n),
        None => (None, key),
    };
    let id = num
        .trim()
        .parse::<i64>()
        .map_err(|_| Error::Invalid(format!("{key:?} is not a task id; use a form like api#12 or 12")))?;
    Ok((slug, id))
}

pub fn repo_from_url(input: &str) -> Option<String> {
    let s = input.trim();
    let (host, path) = if let Some((_, rest)) = s.split_once("://") {
        rest.split_once('/')?
    } else {
        s.split_once(':').unwrap_or(("", s))
    };
    // A repo is its whole path, as GitLab nests them. A pasted page goes past it: on GitLab
    // after /-/, and on GitHub after owner/repo.
    let mut parts: Vec<&str> = path.split("/-/").next()?.split('/').filter(|p| !p.is_empty()).collect();
    if host.to_ascii_lowercase().ends_with("github.com") {
        parts.truncate(2);
    }
    let last = parts.last_mut()?;
    *last = last.strip_suffix(".git").unwrap_or(*last);
    let ok = |p: &str| p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (parts.len() >= 2 && parts.iter().all(|p| ok(p))).then(|| parts.join("/").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("kanzo.db"), None).unwrap();
        store.migrate().unwrap();
        (dir, store)
    }

    fn titles(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn migrate_is_idempotent() {
        let (_dir, s) = store();
        s.migrate().unwrap();
        let v: String =
            s.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len().to_string());
    }

    fn waits(keys: &str) -> TaskPatch {
        TaskPatch { waits_on: Some(keys.into()), ..Default::default() }
    }

    #[test]
    fn next_task_skips_a_task_until_what_it_waits_on_is_done() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocked = s.create_task_in(p.id, "blocked", TaskPatch { tags: Some("bug".into()), ..Default::default() });
        let blocked = blocked.unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let other = s.create_task(p.id, "other", "").unwrap();
        s.move_task(blocker.id, "review", None).unwrap();
        s.update_task(blocked.id, waits(&format!("#{}, api#{}", blocker.id, other.id))).unwrap();
        assert_eq!(s.get_task(blocked.id).unwrap().waiting_on, [blocker.id, other.id]);

        assert_eq!(s.next_task_matching(None, "tag:bug").unwrap().map(|t| t.id), None);
        assert_eq!(s.waiting_in_todo(None, "tag:bug").unwrap(), 1);
        assert_eq!(s.waiting_in_todo(None, "tag:nope").unwrap(), 0);
        assert_eq!(s.next_task(None).unwrap().map(|t| t.id), Some(other.id));
        s.move_task(other.id, "done", None).unwrap();
        assert_eq!(s.get_task(blocked.id).unwrap().waiting_on, [blocker.id]);
        assert_eq!(s.next_task(None).unwrap().map(|t| t.id), None, "review isn't done");

        s.move_task(blocker.id, "done", None).unwrap();
        assert!(s.get_task(blocked.id).unwrap().waiting_on.is_empty());
        s.move_task(blocker.id, "doing", None).unwrap();
        assert_eq!(s.next_task(None).unwrap().map(|t| t.id), None, "reopening blocks it again");
        s.move_task(blocker.id, "done", None).unwrap();
        assert_eq!(s.next_task(None).unwrap().map(|t| t.id), Some(blocked.id));
    }

    #[test]
    fn a_loop_or_a_bad_key_changes_nothing() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let [a, b, c] = ["a", "b", "c"].map(|t| s.create_task(p.id, t, "").unwrap().id);
        s.update_task(a, waits(&b.to_string())).unwrap();
        s.update_task(b, waits(&c.to_string())).unwrap();
        for keys in [a.to_string(), format!("{c}"), "999".to_string(), format!("nope#{a}")] {
            assert!(s.update_task(c, waits(&keys)).is_err(), "{keys}");
            assert!(s.get_task(c).unwrap().waiting_on.is_empty(), "{keys}");
        }
        assert!(s.update_task(a, waits(&format!("{c}, {a}"))).is_err());
        assert_eq!(s.get_task(a).unwrap().waiting_on, [b], "a refused list keeps the old one");

        let new = TaskPatch { waits_on: Some("999".into()), ..Default::default() };
        assert!(s.create_task_in(p.id, "bad", new).is_err());
        assert_eq!(s.list_tasks(None, None).unwrap().len(), 3, "a bad key creates no task");

        s.update_task(a, waits("")).unwrap();
        assert!(s.get_task(a).unwrap().waiting_on.is_empty());
    }

    #[test]
    fn deleting_what_a_task_waits_on_frees_it() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        s.move_task(blocker.id, "doing", None).unwrap();
        let new = TaskPatch { waits_on: Some(blocker.id.to_string()), ..Default::default() };
        let task = s.create_task_in(p.id, "task", new).unwrap();
        assert_eq!(task.waiting_on, [blocker.id]);
        assert_eq!(s.task_links(blocker.id, true).unwrap()[0].id, task.id);
        assert_eq!(s.task_links(task.id, false).unwrap()[0].id, blocker.id);
        s.delete_task(blocker.id).unwrap();
        assert_eq!(s.next_task(None).unwrap().map(|t| t.id), Some(task.id));
    }

    #[test]
    fn editing_the_same_list_changes_nothing() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let task = s.create_task(p.id, "task", "").unwrap();
        s.update_task(task.id, waits(&blocker.id.to_string())).unwrap();
        let before = s.changes().unwrap();
        s.update_task(task.id, waits(&format!("#{}", blocker.id))).unwrap();
        assert_eq!(s.changes().unwrap(), before);
    }

    #[test]
    fn filters_by_waiting_and_leaves_it_out_of_json_when_empty() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let task = s.create_task(p.id, "task", "").unwrap();
        s.update_task(task.id, waits(&blocker.id.to_string())).unwrap();
        assert_eq!(titles(&s.search("is:waiting", None).unwrap()), ["task"]);
        assert_eq!(s.search("is:waiting", None).unwrap()[0].waiting_on, [blocker.id]);
        assert_eq!(s.list_tasks(None, None).unwrap()[1].waiting_on, [blocker.id]);
        assert!(s.list_tasks(None, None).unwrap()[0].waiting_on.is_empty());
        assert_eq!(titles(&s.search("-is:waiting", None).unwrap()), ["blocker"]);
        let json = serde_json::to_value(s.get_task(blocker.id).unwrap()).unwrap();
        assert!(json.get("waiting_on").is_none(), "{json}");
        let json = serde_json::to_value(s.get_task(task.id).unwrap()).unwrap();
        assert_eq!(json["waiting_on"], serde_json::json!([blocker.id]));

        s.set_property("waiting", false).unwrap();
        assert_eq!(s.search_for_agent("is:waiting").unwrap().len(), 0, "a hidden property finds nothing");
    }

    #[test]
    fn changes_many_tasks_at_once_or_not_at_all() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let task = |title, tags: &str| {
            let tags = Some(tags.to_string());
            s.create_task_in(p.id, title, TaskPatch { tags, ..Default::default() }).unwrap().id
        };
        let (a, b, c) = (task("a", "bug, ui"), task("b", "ui"), task("c", "keep"));

        s.update_tasks(&[a, b], Some("doing"), "New, bug", "ui").unwrap();
        let tags = |id| s.get_task(id).unwrap().tags;
        assert_eq!(
            (tags(a), tags(b), tags(c)),
            (vec!["bug".into(), "new".into()], vec!["new".into(), "bug".into()], vec!["keep".into()])
        );
        assert_eq!(titles(&s.list_tasks(None, Some("doing")).unwrap()), ["a", "b"], "they keep their order");

        let before = s.changes().unwrap();
        assert!(s.update_tasks(&[c, 999], Some("done"), "x", "").is_err());
        assert!(s.update_tasks(&[c], Some("nope"), "", "").is_err());
        assert_eq!(
            (s.get_task(c).unwrap().status.as_str(), tags(c), s.changes().unwrap()),
            ("todo", vec!["keep".into()], before)
        );

        let before = s.get_task(a).unwrap();
        s.update_tasks(&[a], Some("doing"), "bug", "absent").unwrap();
        assert_eq!(s.get_task(a).unwrap().position, before.position, "nothing to change moves nothing");

        s.delete_tasks(&[a, 999, b]).unwrap();
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["c"]);
    }

    #[test]
    fn projects_keep_the_order_they_are_moved_to() {
        let (_dir, s) = store();
        for slug in ["a", "b", "c", "d"] {
            s.create_project(slug, "", None).unwrap();
        }
        let order = || s.list_projects().unwrap().into_iter().map(|p| p.slug).collect::<Vec<_>>().join("");
        let move_to = |slug, before| s.move_project(slug, before).unwrap();
        assert_eq!(order(), "abcd", "never moved: by slug");

        move_to("c", Some("a"));
        assert_eq!(order(), "cabd", "first");
        let positioned: i64 = s.conn.query_row("SELECT count(position) FROM projects", [], |r| r.get(0)).unwrap();
        assert_eq!(positioned, 1, "only the moved project needed a position");

        move_to("d", Some("b"));
        assert_eq!(order(), "cadb", "among projects with no position");
        move_to("c", None);
        assert_eq!(order(), "adbc", "last");
        move_to("b", Some("d"));
        assert_eq!(order(), "abdc", "between two");
        move_to("b", Some("b"));
        assert_eq!(order(), "abdc", "onto itself");

        s.create_project("0new", "", None).unwrap();
        assert_eq!(order(), "abdc0new", "a new project goes last");
        move_to("0new", Some("b"));
        assert_eq!(order(), "a0newbdc");

        for _ in 0..80 {
            // Out of float room between two neighbours: numbers them again.
            move_to("c", Some("b"));
            move_to("d", Some("b"));
        }
        assert_eq!(order(), "a0newcdb");
        assert!(s.move_project("a", Some("nope")).is_err());
        assert!(s.move_project("nope", None).is_err());
    }

    #[test]
    fn keeps_only_the_newest_done_tasks() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        s.create_task(p.id, "open", "").unwrap();
        for title in ["d1", "d2", "d3", "d4"] {
            let t = s.create_task(p.id, title, "").unwrap();
            s.move_task(t.id, "done", None).unwrap();
        }
        let (tasks, hidden) = s.list_tasks_keeping_done(None, None, Some(2)).unwrap();
        assert_eq!((titles(&tasks), hidden), (vec!["open", "d3", "d4"], 2));
        let (tasks, hidden) = s.list_tasks_keeping_done(Some(p.id), Some("done"), Some(0)).unwrap();
        assert_eq!((tasks.len(), hidden), (0, 4));
        assert_eq!(s.list_tasks_keeping_done(None, Some("todo"), Some(0)).unwrap().1, 0);
        let (all, hidden) = s.list_tasks_keeping_done(None, None, None).unwrap();
        assert_eq!((titles(&all), hidden), (vec!["open", "d1", "d2", "d3", "d4"], 0));
    }

    #[test]
    fn opens_a_board_from_a_newer_kanzo_when_it_can() {
        let (_dir, s) = store();
        let set = |version: usize, compatible: usize| {
            s.conn
                .execute_batch(&format!(
                    "UPDATE meta SET value = '{version}' WHERE key = 'schema_version';
                     UPDATE meta SET value = '{compatible}' WHERE key = 'compatible';"
                ))
                .unwrap();
        };
        let newer = MIGRATIONS.len() + 1;
        set(newer, MIGRATIONS.len());
        s.migrate().unwrap();
        s.create_project("api", "", None).unwrap();
        let v: String =
            s.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, newer.to_string());
        set(newer, newer);
        assert!(s.migrate().unwrap_err().to_string().contains("kanzo update"));
    }

    #[test]
    fn projects() {
        let (_dir, s) = store();
        let p = s.create_project("API", "API server", Some("https://github.com/Me/Api.git")).unwrap();
        assert_eq!((p.slug.as_str(), p.repos.as_slice()), ("api", ["me/api".to_string()].as_slice()));
        assert_eq!(s.find_project("me/api").unwrap().id, p.id);
        assert_eq!(s.project_by_repo("git@github.com:me/api.git").unwrap().unwrap().id, p.id);
        assert!(s.create_project("api", "", None).unwrap_err().to_string().contains("already exists"));
        assert!(s.create_project("web", "", Some("me/api")).unwrap_err().to_string().contains("already linked"));
        assert!(s.create_project("bad slug", "", None).is_err());
        let web = s.create_project("web", "", None).unwrap();
        assert_eq!((web.name.as_str(), web.color.as_deref()), ("web", None));
        assert_eq!(s.set_project_color("web", "teal").unwrap().color.as_deref(), Some("teal"));
        assert!(s.set_project_color("web", "#ff0000").is_err());
        let renamed = s.rename_project("web", "  Web   site ").unwrap();
        assert_eq!((renamed.slug.as_str(), renamed.name.as_str()), ("web", "Web site"));
        assert!(s.rename_project("web", "  ").is_err());
        assert!(s.find_project("nope").unwrap_err().to_string().contains("api, web"));
    }

    #[test]
    fn many_repos_per_project() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("me/api")).unwrap();
        s.create_project("web", "", None).unwrap();
        assert_eq!(s.link_repo("api", "https://github.com/Me/API-docs.git").unwrap().repos, ["me/api", "me/api-docs"]);
        assert_eq!(s.find_project("me/api-docs").unwrap().id, api.id);
        assert_eq!(s.link_repo("api", "me/api").unwrap().repos.len(), 2, "linking twice changes nothing");
        assert!(s.link_repo("web", "me/api-docs").unwrap_err().to_string().contains("already linked to project api"));
        assert!(s.link_repo("api", "not a repo").is_err());
        assert_eq!(s.unlink_repo("api", "me/api").unwrap().repos, ["me/api-docs"]);
        assert!(s.project_by_repo("me/api").unwrap().is_none());
        assert!(s.unlink_repo("api", "me/api").is_err());
        s.delete_project("api").unwrap();
        assert_eq!(s.link_repo("web", "me/api-docs").unwrap().repos, ["me/api-docs"]);
    }

    #[test]
    fn search() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let login = s.create_task(api.id, "Fix login", "Users loop on /login").unwrap();
        let old = s.create_task(web.id, "Login page copy", "").unwrap();
        s.update_task(old.id, TaskPatch { status: Some("done".into()), ..Default::default() }).unwrap();
        let other = s.create_task(web.id, "Pricing 100%", "").unwrap();
        s.update_task(other.id, TaskPatch { tags: Some("ui".into()), ..Default::default() }).unwrap();
        let titles = |q: &str| s.search(q, Some(10)).unwrap().into_iter().map(|t| t.title).collect::<Vec<_>>();
        assert_eq!(titles("LOGIN"), ["Fix login", "Login page copy"], "any project, open first, done too");
        assert_eq!(titles("login api"), ["Fix login"], "every word must match, the project too");
        assert_eq!(titles(&format!("web#{}", other.id)), ["Pricing 100%"], "by key");
        assert_eq!(titles("ui"), ["Pricing 100%"], "by tag");
        s.add_comment(old.id, "you", "Needs the new logo", &[]).unwrap();
        assert_eq!(titles("logo login"), ["Login page copy"], "by comment, with the task's own words");
        assert_eq!(titles("100%"), ["Pricing 100%"], "% is a plain character");
        assert!(titles("nothing-like-this").is_empty());
        assert_eq!(s.search("", Some(10)).unwrap().len(), 3, "no words: the latest changes");
        assert_eq!(s.search("login", Some(1)).unwrap()[0].id, login.id);
    }

    #[test]
    fn filter_by_any_property() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let patch = |tags: &str, status: &str| TaskPatch {
            tags: Some(tags.into()),
            status: Some(status.into()),
            description: Some("Users loop on /login".into()),
            ..Default::default()
        };
        let login = s.create_task_in(api.id, "Fix login", patch("Bug, needs review", "doing")).unwrap();
        let debug = s.create_task_in(api.id, "Debug tools", patch("debugger", "todo")).unwrap();
        let human = s.create_task(web.id, "Call the Bank", "").unwrap();
        s.update_task(
            human.id,
            TaskPatch { human: Some(true), url: Some("https://x.dev/1".into()), ..Default::default() },
        )
        .unwrap();
        s.add_comment(debug.id, "you", "Needs the new logo", &[]).unwrap();
        s.add_comment(debug.id, "you", "Soon", &[]).unwrap();
        s.conn.execute("INSERT INTO images (task_id, file) VALUES (?1, 'a.png')", [human.id]).unwrap();
        s.conn
            .execute("UPDATE tasks SET created_at = '2026-01-15T10:00:00Z', updated_at = '2026-03-01T10:00:00Z'", [])
            .unwrap();
        s.conn.execute("UPDATE tasks SET created_at = '2026-02-01T10:00:00Z' WHERE id = ?1", [login.id]).unwrap();

        let ids = |q: &str| {
            let mut ids: Vec<i64> = s.search(q, None).unwrap().into_iter().map(|t| t.id).collect();
            ids.sort_unstable();
            ids
        };
        let (l, d, h) = (login.id, debug.id, human.id);
        for (q, want) in [
            ("tag:bug", vec![l]),
            ("TAG:BUG", vec![l]),
            ("tag:debug", vec![]),
            ("tag:\"needs review\"", vec![l]),
            ("-tag:bug", vec![d, h]),
            ("status:doing", vec![l]),
            ("status:DOING", vec![l]),
            ("project:api", vec![l, d]),
            ("-project:api", vec![h]),
            ("title:login", vec![l]),
            ("title:loop", vec![]),
            ("description:loop", vec![l, d]),
            ("url:x.dev", vec![h]),
            ("comment:logo", vec![d]),
            (&format!("id:{l}"), vec![l]),
            (&format!("id:>{l}"), vec![d, h]),
            (&format!("id:{l}..{d}"), vec![l, d]),
            ("comments:2", vec![d]),
            ("comments:>=1", vec![d]),
            ("comments:<1", vec![l, h]),
            ("images:1", vec![h]),
            ("created:2026-02-01", vec![l]),
            ("created:<2026-02-01", vec![d, h]),
            ("created:>=2026-02-01", vec![l]),
            ("created:2026-01-01..2026-01-31", vec![d, h]),
            ("updated:2026-03-01", vec![l, d, h]),
            ("is:human", vec![h]),
            ("-is:human", vec![l, d]),
            ("has:url", vec![h]),
            ("no:url", vec![l, d]),
            ("-has:url", vec![l, d]),
            ("-no:url", vec![h]),
            ("has:tags", vec![l, d]),
            ("has:description", vec![l, d]),
            ("has:comments", vec![d]),
            ("no:images", vec![l, d]),
            ("tag:bug status:doing", vec![l]),
            ("tag:bug status:todo", vec![]),
            ("project:api fix", vec![l]),
            ("project:api -status:doing", vec![d]),
            // Being typed: filters nothing yet.
            ("tag:", vec![l, d, h]),
            ("-status:", vec![l, d, h]),
            ("has:", vec![l, d, h]),
            // Not a complete qualifier: a word.
            ("created:soon", vec![]),
            ("comments:many", vec![]),
            ("is:nothing", vec![]),
            ("has:nothing", vec![]),
            ("http://x.dev", vec![]),
            ("-login", vec![]),
            // Quotes make a phrase, or text that only looks like a qualifier.
            ("\"loop on\"", vec![l, d]),
            ("\"tag:bug\"", vec![]),
        ] {
            assert_eq!(ids(q), want, "{q}");
        }
        s.update_task(l, TaskPatch { description: Some("status:doing is the text".into()), ..Default::default() })
            .unwrap();
        assert_eq!(ids("\"status:doing\""), [l]);
    }

    #[test]
    fn filter_values_cannot_change_the_sql() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        s.create_task(api.id, "Keep me", "").unwrap();
        for q in [
            "tag:x'OR'1'='1",
            "created:2026-01-01'OR'1'='1",
            "created:'OR'1'='1'--",
            "id:1)OR(1=1",
            "comments:1;DROP",
            "created:2026-01-0'",
            "title:'",
            "status:'--",
        ] {
            assert!(s.search(q, None).unwrap().is_empty(), "{q}");
        }
        assert_eq!(s.search("", None).unwrap().len(), 1);
    }

    #[test]
    fn next_task_takes_the_top_match_and_never_a_human_one() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let tagged = |title: &str, human: bool| {
            let patch = TaskPatch { tags: Some("billing".into()), human: Some(human), ..Default::default() };
            s.create_task_in(api.id, title, patch).unwrap();
        };
        s.create_task(api.id, "Not in the epic", "").unwrap();
        tagged("For the user", true);
        tagged("First in the epic", false);
        tagged("Second in the epic", false);

        let next = |q: &str| s.next_task_matching(Some(api.id), q).unwrap().map(|t| t.title);
        assert_eq!(next("is:human"), None);
        assert_eq!(next("tag:nope"), None);
        assert_eq!(next("tag:billing").as_deref(), Some("First in the epic"));
        assert_eq!(next("TAG:billing second").as_deref(), Some("Second in the epic"));
        assert_eq!(next("tag:billing"), None, "the human one stays");
        assert_eq!(next("  ").as_deref(), Some("Not in the epic"));
    }

    #[test]
    fn agents_cannot_filter_through_hidden_properties() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let patch = TaskPatch { tags: Some("secret".into()), ..Default::default() };
        let t = s.create_task_in(api.id, "Plain title", patch).unwrap();
        s.add_comment(t.id, "you", "whisper", &[]).unwrap();
        let found = |q: &str| s.search_for_agent(q).unwrap().len();
        assert_eq!((found("tag:secret"), found("secret"), found("comment:whisper"), found("whisper")), (1, 1, 1, 1));

        s.set_property("tags", false).unwrap();
        s.set_property("comments", false).unwrap();
        assert_eq!((found("tag:secret"), found("secret"), found("comment:whisper"), found("whisper")), (0, 0, 0, 0));
        assert_eq!(found("has:tags"), 0);
        assert_eq!(s.next_task_matching(None, "tag:secret").unwrap().map(|t| t.id), None);
        assert_eq!(found("plain"), 1, "what stays visible still matches");
        assert_eq!(s.search("tag:secret", None).unwrap().len(), 1, "the board still sees them");
    }

    #[test]
    fn search_ignores_case_beyond_ascii() {
        let (_dir, s) = store();
        let web = s.create_project("web", "", None).unwrap();
        s.create_task(web.id, "Über café", "").unwrap();
        for q in ["über", "ÜBER", "Über", "café", "CAFÉ", "web#1"] {
            assert_eq!(s.search(q, None).unwrap().len(), 1, "{q}");
        }
    }

    #[test]
    fn images() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "Bug", "").unwrap();
        let shot = "0123456789abcdef.png".to_string();
        let on_task = s.add_image(t.id, &shot).unwrap();
        assert!(s.add_image(t.id, "../../etc/passwd").is_err());
        assert!(s.add_image(t.id + 1, &shot).is_err());
        let c = s.add_comment(t.id, "me", "", &["fedcba9876543210.jpg".into(), shot.clone()]).unwrap();
        assert!(s.add_comment(t.id, "me", "x", &["evil.svg".into()]).is_err());
        let all: Vec<_> = s.list_images(t.id).unwrap().into_iter().map(|i| (i.comment_id, i.file)).collect();
        assert_eq!(
            all,
            [(None, shot.clone()), (Some(c.id), "fedcba9876543210.jpg".into()), (Some(c.id), shot.clone())]
        );
        assert_eq!(s.image_files(ImagesOf::Task(t.id)).unwrap(), [shot.clone(), "fedcba9876543210.jpg".into()]);
        assert_eq!(s.image_files(ImagesOf::Project(p.id)).unwrap().len(), 2);
        s.delete_image(on_task.id).unwrap();
        assert!(s.unused_images(vec![shot.clone()]).unwrap().is_empty());
        let files = s.image_files(ImagesOf::Comment(c.id)).unwrap();
        s.delete_comment(c.id).unwrap();
        assert!(s.list_images(t.id).unwrap().is_empty());
        assert_eq!(s.unused_images(files).unwrap(), ["fedcba9876543210.jpg".to_string(), shot.clone()]);
        assert!(valid_image_file("6fec521a01297ab5.webp"));
        assert!(!valid_image_file("6FEC521A01297AB5.png") && !valid_image_file("6fec521a01297ab5.svg"));
    }

    #[test]
    fn migration_moves_repos_to_their_own_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..3] {
                c.execute_batch(sql).unwrap();
            }
            c.execute_batch(
                "INSERT INTO meta VALUES ('schema_version', '3');
                 INSERT INTO projects (slug, name, repo) VALUES ('api', 'api', 'me/api'), ('web', 'web', NULL);",
            )
            .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        assert_eq!(s.find_project("api").unwrap().repos, ["me/api"]);
        assert!(s.find_project("web").unwrap().repos.is_empty());
        assert_eq!(s.find_project("me/api").unwrap().slug, "api");
    }

    #[test]
    fn task_lifecycle() {
        let (_dir, s) = store();
        let p = s.create_project("api", "API", None).unwrap();
        let t = s.create_task(p.id, "  Fix   login ", "Steps:\n1. log in\n").unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str(), t.key()), ("Fix login", "todo", format!("api#{}", t.id)));
        assert_eq!(s.find_task(&t.key()).unwrap().id, t.id);
        assert!(s.find_task(&format!("web#{}", t.id)).is_err());

        let t = s
            .update_task(
                t.id,
                TaskPatch { status: Some("review".into()), title: Some("Fix login bug".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("Fix login bug", "review"));
        assert!(s.update_task(t.id, TaskPatch { status: Some("nope".into()), ..Default::default() }).is_err());
        assert!(s.update_task(t.id, TaskPatch { title: Some("  ".into()), ..Default::default() }).is_err());
        let web = s.create_project("web", "", None).unwrap();
        let moved = s.update_task(t.id, TaskPatch { project_id: Some(web.id), ..Default::default() }).unwrap();
        assert_eq!((moved.key(), moved.status.as_str()), (format!("web#{}", t.id), "review"));
        let t = s.update_task(t.id, TaskPatch { project_id: Some(p.id), ..Default::default() }).unwrap();

        s.add_comment(t.id, "claude-code", "Done, see PR", &[]).unwrap();
        s.add_comment(t.id, "", "thanks", &[]).unwrap();
        let comments = s.list_comments(t.id).unwrap();
        assert_eq!(comments.iter().map(|c| c.author.as_str()).collect::<Vec<_>>(), ["claude-code", "me"]);
        assert!(s.add_comment(t.id, "me", " ", &[]).is_err());
        assert_eq!(s.get_task(t.id).unwrap().comments, 2);
        let tagged =
            s.update_task(t.id, TaskPatch { tags: Some(" Bug, UI,bug ,, needs  review".into()), ..Default::default() });
        assert_eq!(tagged.unwrap().tags, ["bug", "ui", "needs review"]);
        assert!(
            s.update_task(t.id, TaskPatch { tags: Some(" , ".into()), ..Default::default() }).unwrap().tags.is_empty()
        );
        let gone = s.add_comment(t.id, "me", "typo", &[]).unwrap();
        assert_eq!(s.delete_comment(gone.id).unwrap().body, "typo");
        assert!(s.delete_comment(gone.id).is_err());
        assert_eq!(s.get_task(t.id).unwrap().comments, 2);
        assert_eq!(s.find_project("api").unwrap().open, 1);

        s.delete_task(t.id).unwrap();
        assert!(s.get_task(t.id).is_err());
        let orphans: i64 = s.conn.query_row("SELECT count(*) FROM comments", [], |r| r.get(0)).unwrap();
        assert_eq!(orphans, 0);
        // Ids are never reused, so api#12 always means the same task.
        let t2 = s.create_task(p.id, "Another", "").unwrap();
        assert!(t2.id > t.id);
    }

    #[test]
    fn delete_project_removes_its_tasks_and_comments() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("me/api")).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let t = s.create_task(api.id, "a", "").unwrap();
        s.add_comment(t.id, "me", "hi", &[]).unwrap();
        let kept = s.create_task(web.id, "b", "").unwrap();
        s.add_comment(kept.id, "me", "hello", &[]).unwrap();

        assert_eq!(s.delete_project("api").unwrap().id, api.id);
        assert!(s.find_project("api").is_err());
        assert!(s.get_task(t.id).is_err());
        let count = |table: &str| -> i64 {
            s.conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        assert_eq!((count("projects"), count("tasks"), count("comments")), (1, 1, 1));
        assert!(s.delete_project("api").is_err());
        s.create_project("api", "", Some("me/api")).unwrap();
    }

    #[test]
    fn ordering_and_moves() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let a = s.create_task(p.id, "a", "").unwrap();
        let b = s.create_task(p.id, "b", "").unwrap();
        let c = s.create_task(p.id, "c", "").unwrap();
        assert_eq!(titles(&s.list_tasks(Some(p.id), None).unwrap()), ["a", "b", "c"]);

        s.move_task(c.id, "todo", Some(a.id)).unwrap();
        assert_eq!(titles(&s.list_tasks(None, Some("todo")).unwrap()), ["c", "a", "b"]);
        s.move_task(c.id, "todo", None).unwrap();
        assert_eq!(titles(&s.list_tasks(None, Some("todo")).unwrap()), ["a", "b", "c"]);
        s.move_task(a.id, "doing", None).unwrap();
        s.move_task(b.id, "doing", Some(a.id)).unwrap();
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["c", "b", "a"]);

        // Many inserts at the same spot run out of float room and trigger a renumber.
        let mut moving = [b.id, c.id];
        for _ in 0..80 {
            s.move_task(moving[0], "doing", Some(a.id)).unwrap();
            moving.swap(0, 1);
        }
        assert_eq!(titles(&s.list_tasks(None, Some("doing")).unwrap()).last(), Some(&"a"));
        assert!(s.move_task(a.id, "nope", None).is_err());
    }

    #[test]
    fn next_task_takes_top_todo() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        s.create_task(web.id, "web first", "").unwrap();
        let changes = s.changes().unwrap();
        let patch = TaskPatch { human: Some(true), tags: Some("Bug, bug".into()), ..Default::default() };
        let human = s.create_task_in(api.id, "Call the bank", patch).unwrap();
        assert!(human.human && human.tags == ["bug"] && s.changes().unwrap() == changes + 1, "all in one write");
        s.create_task(api.id, "api first", "").unwrap();
        s.create_task(api.id, "api second", "").unwrap();

        let t = s.next_task(Some(api.id)).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("api first", "doing"));
        assert_eq!(s.next_task(None).unwrap().unwrap().title, "web first");
        assert_eq!(s.next_task(Some(api.id)).unwrap().unwrap().title, "api second");
        assert!(s.next_task(Some(api.id)).unwrap().is_none());
        assert_eq!(s.get_task(human.id).unwrap().status, "todo");
        assert!(!s.update_task(human.id, TaskPatch { human: Some(false), ..Default::default() }).unwrap().human);
    }

    #[test]
    fn backlog_is_a_column_agents_skip() {
        let (dir, s) = store();
        assert_eq!(s.statuses(), STATUSES);
        s.set_backlog(true).unwrap();
        let s = Store::open(&dir.path().join("kanzo.db"), None).unwrap();
        assert_eq!(s.statuses(), ["backlog", "todo", "doing", "review", "done"]);
        let api = s.create_project("api", "", None).unwrap();
        let status = |s: &str| TaskPatch { status: Some(s.into()), ..Default::default() };
        let backlog = s.create_task_in(api.id, "later", status("backlog")).unwrap();
        assert_eq!(backlog.status, "backlog");
        assert!(s.create_task_in(api.id, "x", status("nope")).is_err());
        assert_eq!(s.create_task(api.id, "now", "").unwrap().status, "todo");

        let t = s.next_task(None).unwrap().unwrap();
        assert_eq!((t.title.as_str(), t.status.as_str()), ("now", "doing"));
        assert!(s.next_task(None).unwrap().is_none());
        assert_eq!(s.get_task(backlog.id).unwrap().status, "backlog");

        assert!(s.set_backlog(false).is_err());
        s.delete_task(backlog.id).unwrap();
        s.set_backlog(false).unwrap();
    }

    #[test]
    fn hidden_properties_keep_their_data() {
        let (_dir, s) = store();
        assert!(s.hidden_properties().unwrap().is_empty());
        let api = s.create_project("api", "", None).unwrap();
        let t = s.create_task(api.id, "x", "Why").unwrap();
        let before = s.changes().unwrap();

        s.set_property("description", false).unwrap();
        s.set_property("tags", false).unwrap();
        s.set_property("tags", false).unwrap();
        assert_eq!(s.hidden_properties().unwrap(), ["description", "tags"]);
        assert!(s.changes().unwrap() > before, "other open boards redraw");
        assert_eq!(s.get_task(t.id).unwrap().description, "Why");

        s.set_property("description", true).unwrap();
        assert_eq!(s.hidden_properties().unwrap(), ["tags"]);
        for always in ["title", "status", "nope"] {
            assert!(s.set_property(always, false).is_err(), "{always}");
        }
    }

    #[test]
    fn change_counter_moves_on_every_write() {
        let (_dir, s) = store();
        let c0 = s.changes().unwrap();
        let p = s.create_project("api", "", None).unwrap();
        let c1 = s.changes().unwrap();
        let t = s.create_task(p.id, "x", "").unwrap();
        let c2 = s.changes().unwrap();
        s.add_comment(t.id, "me", "hi", &[]).unwrap();
        let c3 = s.changes().unwrap();
        assert!(c0 < c1 && c1 < c2 && c2 < c3);
    }

    #[test]
    fn a_task_links_to_one_url_on_any_site() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let t = s.create_task(api.id, "Fix login", "").unwrap();
        assert_eq!(t.url, "");
        let set = |url: &str| s.update_task(t.id, TaskPatch { url: Some(url.into()), ..Default::default() });
        for url in [
            "https://github.com/Me/Api/pull/12",
            "https://gitlab.example.com/a/b/-/merge_requests/3?x=1#y",
            "http://intranet",
        ] {
            assert_eq!(set(&format!("  {url} ")).unwrap().url, url);
        }
        for bad in [
            "#12",
            "owner/repo#12",
            "github.com/a/b",
            "javascript:alert(1)",
            "javascript://x",
            "https://",
            "https://a b",
        ] {
            assert!(set(bad).unwrap_err().to_string().contains("should start with"), "{bad}");
        }
        assert_eq!(s.get_task(t.id).unwrap().url, "http://intranet", "a bad URL changes nothing");
        assert_eq!(
            s.update_task(t.id, TaskPatch { title: Some("Fix".into()), ..Default::default() }).unwrap().url,
            "http://intranet"
        );
        assert_eq!(set("").unwrap().url, "");
    }

    #[test]
    fn task_keys() {
        assert_eq!(parse_task_key("api#12").unwrap(), (Some("api".into()), 12));
        assert_eq!(parse_task_key("#12").unwrap(), (None, 12));
        assert_eq!(parse_task_key(" 12 ").unwrap(), (None, 12));
        assert!(parse_task_key("api#x").is_err());
    }

    #[test]
    fn repos() {
        for url in [
            "owner/repo",
            "Owner/Repo",
            "https://github.com/owner/repo",
            "https://github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "git@github-work:owner/repo.git",
            "https://github.com/owner/repo/",
            "https://github.com/owner/repo/tree/main",
            "https://github.com/owner/repo/pull/3",
            "https://gitea.example.com/owner/repo.git",
            "git@gitea.example.com:owner/repo.git",
            "ssh://git@gitea.example.com:2222/owner/repo.git",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("owner/repo"), "{url}");
        }
        for url in [
            "acme/backend/api",
            "https://gitlab.com/acme/backend/api",
            "https://gitlab.com/acme/backend/api.git",
            "git@gitlab.com:acme/backend/api.git",
            "ssh://git@gitlab.com/acme/backend/api.git",
            "https://gitlab.com/acme/backend/api/-/merge_requests/3",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("acme/backend/api"), "{url}");
        }
        assert_eq!(repo_from_url("repo"), None);
        assert_eq!(repo_from_url("own er/repo"), None);
    }

    #[test]
    fn repos_nested_in_one_group_are_different_repos() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", Some("https://gitlab.com/acme/backend/api.git")).unwrap();
        let web = s.create_project("web", "", Some("git@gitlab.com:acme/backend/web.git")).unwrap();
        assert_eq!(api.repos, ["acme/backend/api"]);
        assert_eq!(s.project_by_repo("acme/backend/web").unwrap().unwrap().id, web.id);
        assert!(s.create_project("api2", "", Some("acme/backend/api")).is_err(), "still one project per repo");
    }

    #[test]
    fn moves_several_tasks_in_one_write() {
        let (_tmp, s) = store();
        let p = s.create_project("api", "", None).unwrap().id;
        let task = |title: &str| s.create_task(p, title, "").unwrap().id;
        let (a, b, c, d, x, y) = (task("a"), task("b"), task("c"), task("d"), task("x"), task("y"));
        s.move_tasks(&[x, y], "doing", None).unwrap();
        let order =
            |status: &str| -> Vec<i64> { s.list_tasks(None, Some(status)).unwrap().iter().map(|t| t.id).collect() };

        s.move_tasks(&[c, a], "doing", Some(y)).unwrap();
        assert_eq!(order("doing"), [x, c, a, y]);
        assert_eq!(order("todo"), [b, d]);

        // To the end, within a column, and past an anchor that's moving too.
        s.move_tasks(&[x, c], "doing", None).unwrap();
        assert_eq!(order("doing"), [a, y, x, c]);
        s.move_tasks(&[a, y], "doing", Some(a)).unwrap();
        assert_eq!(order("doing"), [x, c, a, y]);

        // More moves than there is float room between two neighbours.
        let many: Vec<i64> = (0..80).map(|i| task(&format!("m{i}"))).collect();
        s.move_tasks(&many, "doing", Some(y)).unwrap();
        let doing = order("doing");
        assert_eq!(&doing[3..83], &many[..]);
        assert_eq!(doing.last(), Some(&y));

        // All or nothing.
        assert!(s.move_tasks(&[b, 99999], "doing", None).is_err());
        assert!(order("todo").contains(&b));
        assert!(s.move_tasks(&[b], "nope", None).is_err());
    }

    fn retry(id: &str) -> Retry {
        Retry::new("claude-code", "", "/work/api", id).unwrap()
    }

    fn made(t: &Task) -> Result<String> {
        Ok(format!("made {} {}", t.key(), t.title))
    }

    fn receipts(s: &Store) -> i64 {
        s.conn.query_row("SELECT count(*) FROM requests", [], |r| r.get(0)).unwrap()
    }

    fn with(change: impl FnOnce(&mut TaskPatch)) -> TaskPatch {
        let mut patch = TaskPatch::default();
        change(&mut patch);
        patch
    }

    fn refused(error: Error) -> bool {
        matches!(&error, Error::Invalid(m) if m.contains("different request"))
    }

    #[test]
    fn a_retried_create_makes_one_task_and_replays_its_first_reply() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let first = s.create_task_retry(p.id, "Ship it", TaskPatch::default(), &retry("k1"), made).unwrap();
        assert!(!first.replayed);
        assert_eq!(first.reply.as_deref(), Some("made api#1 Ship it"));

        // Edited since: the retry still says what was said then, and changes nothing.
        let patch = TaskPatch { title: Some("Renamed".into()), ..Default::default() };
        s.update_task(first.task.unwrap(), patch).unwrap();
        let changes = s.changes().unwrap();
        // The same request spelled with other spaces is the same request.
        let again = s.create_task_retry(p.id, "  Ship   it ", TaskPatch::default(), &retry("k1"), made).unwrap();
        assert!(again.replayed);
        assert_eq!((again.task, again.reply), (first.task, first.reply));
        assert_eq!(s.list_tasks(None, None).unwrap().len(), 1);
        assert_eq!(s.changes().unwrap(), changes, "a replay doesn't wake the board");
        assert_eq!(receipts(&s), 1);
    }

    #[test]
    fn a_retried_create_after_the_task_is_deleted_creates_nothing() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let first = s.create_task_retry(p.id, "Ship it", TaskPatch::default(), &retry("k1"), made).unwrap();
        s.delete_task(first.task.unwrap()).unwrap();
        let changes = s.changes().unwrap();
        let again = s.create_task_retry(p.id, "Ship it", TaskPatch::default(), &retry("k1"), made).unwrap();
        assert!(again.replayed);
        assert_eq!((again.task, again.reply), (first.task, first.reply));
        assert!(s.list_tasks(None, None).unwrap().is_empty());
        assert_eq!(s.changes().unwrap(), changes);
    }

    #[test]
    fn a_changed_request_under_the_same_id_is_refused_before_anything_happens() {
        let (_dir, s) = store();
        let api = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let first = s.create_task_retry(api.id, "One", TaskPatch::default(), &retry("k"), made).unwrap();
        let task = first.task.unwrap();
        let changes = s.changes().unwrap();
        let changed: [(&str, i64, TaskPatch); 7] = [
            ("Two", api.id, TaskPatch::default()),
            ("One", web.id, TaskPatch::default()),
            ("One", api.id, with(|p| p.description = Some("more".into()))),
            ("One", api.id, with(|p| p.status = Some("doing".into()))),
            ("One", api.id, with(|p| p.tags = Some("bug".into()))),
            ("One", api.id, with(|p| p.human = Some(true))),
            ("One", api.id, with(|p| p.waits_on = Some(task.to_string()))),
        ];
        for (title, project, patch) in changed {
            let error = s.create_task_retry(project, title, patch, &retry("k"), made).unwrap_err();
            assert!(refused(error), "{title}");
        }
        // Another method with the same id, for what happens to be the same task or scope.
        let comment = s.add_comment_retry(&task.to_string(), "me", "hi", &[], &retry("k"), made).unwrap_err();
        assert!(refused(comment));
        assert!(refused(s.next_task_retry(None, "", &retry("k"), || Ok("none".into())).unwrap_err()));

        assert_eq!(s.list_tasks(None, None).unwrap().len(), 1);
        assert!(s.list_comments(task).unwrap().is_empty());
        assert_eq!(s.get_task(task).unwrap().status, "todo", "next_task took nothing");
        assert_eq!(s.changes().unwrap(), changes);
        assert_eq!(receipts(&s), 1);
    }

    #[test]
    fn the_same_id_is_a_different_request_for_a_different_caller() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let callers = [
            ("claude-code", "", "/a"),
            ("codex", "", "/a"),
            ("claude-code", "mac-mini", "/a"),
            ("claude-code", "", "/b"),
            // Parts that would run together without their lengths.
            ("ab", "c", "/a"),
            ("a", "bc", "/a"),
        ];
        let mut ids = Vec::new();
        for (client, computer, folder) in callers {
            let retry = Retry::new(client, computer, folder, "same").unwrap();
            let sent = s.create_task_retry(p.id, "One", TaskPatch::default(), &retry, made).unwrap();
            assert!(!sent.replayed, "{client} {computer} {folder}");
            ids.push(sent.task.unwrap());
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), callers.len());
        assert_eq!(s.list_tasks(None, None).unwrap().len(), callers.len());

        let first = Retry::new("claude-code", "", "/a", "same").unwrap();
        let again = s.create_task_retry(p.id, "One", TaskPatch::default(), &first, made).unwrap();
        assert!(again.replayed);
        assert_eq!(s.list_tasks(None, None).unwrap().len(), callers.len());
    }

    #[test]
    fn a_request_id_that_is_not_valid_is_an_error() {
        let long = "x".repeat(REQUEST_ID_MAX + 1);
        for id in ["", " ", "a b", " a", "a\tb", "a\nb", "a\u{7f}", "a\u{85}", "a\u{a0}b", long.as_str()] {
            assert!(Retry::new("c", "", "", id).is_err(), "{id:?}");
        }
        assert!(Retry::new("c", "", "", &"x".repeat(REQUEST_ID_MAX)).is_ok());
        assert!(Retry::new("c", "", "", "0b3f-ünï").is_ok());
        let name = "n".repeat(CALLER_NAME_MAX + 1);
        let folder = "/".repeat(CALLER_FOLDER_MAX + 1);
        assert!(Retry::new(&name, "", "", "k").is_err());
        assert!(Retry::new("c", &name, "", "k").is_err());
        assert!(Retry::new("c", "", &folder, "k").is_err());
        assert!(Retry::new(&name[1..], "", &folder[1..], "k").is_ok());
    }

    #[test]
    fn a_failed_request_keeps_no_receipt_and_leaves_the_id_free() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let todo = s.create_task(p.id, "waiting for someone", "").unwrap();
        let tasks = || s.list_tasks(None, None).unwrap().len();
        let create = |title: &str, patch: TaskPatch| s.create_task_retry(p.id, title, patch, &retry("k"), made);

        // Refused before the write, or inside it after the task was made.
        assert!(create(" ", TaskPatch::default()).is_err());
        assert!(create("T", TaskPatch { waits_on: Some("999".into()), ..Default::default() }).is_err());
        let no_reply = |_: &Task| -> Result<String> { Err(Error::Invalid("no".into())) };
        assert!(s.create_task_retry(p.id, "T", TaskPatch::default(), &retry("k"), no_reply).is_err());
        assert!(s.add_comment_retry("999", "me", "hi", &[], &retry("k"), made).is_err());
        assert!(s.add_comment_retry(&todo.id.to_string(), "me", " ", &[], &retry("k"), made).is_err());
        assert_eq!((tasks(), receipts(&s)), (1, 0));

        // The receipt itself can't be saved: nothing the request did stays.
        s.conn
            .execute_batch("CREATE TRIGGER no_receipts BEFORE INSERT ON requests BEGIN SELECT RAISE(ABORT, 'no'); END;")
            .unwrap();
        let changes = s.changes().unwrap();
        assert!(create("T", TaskPatch::default()).is_err());
        let said = |_: &Task| -> Result<String> { Ok("added".to_string()) };
        assert!(s.add_comment_retry(&todo.id.to_string(), "me", "hi", &[], &retry("k"), said).is_err());
        assert!(s.next_task_retry(None, "", &retry("k"), || Ok("none".into())).is_err());
        assert_eq!(tasks(), 1);
        assert!(s.list_comments(todo.id).unwrap().is_empty());
        assert_eq!(s.get_task(todo.id).unwrap().status, "todo");
        assert_eq!(s.changes().unwrap(), changes);

        s.conn.execute_batch("DROP TRIGGER no_receipts").unwrap();
        let sent = create("T", TaskPatch::default()).unwrap();
        assert!(!sent.replayed, "the id was never reserved");
        assert_eq!((tasks(), receipts(&s)), (2, 1));
    }

    #[test]
    fn a_retried_comment_is_added_once_and_still_answers_after_its_task_is_deleted() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let task = s.create_task(p.id, "Ship it", "").unwrap();
        let other = s.create_task(p.id, "Other", "").unwrap();
        let said = |t: &Task| -> Result<String> { Ok(format!("added to {}", t.key())) };
        let key = task.key();

        let first = s.add_comment_retry(&key, "bot", " done ", &[], &retry("c1"), said).unwrap();
        let comment = first.comment.unwrap();
        assert_eq!((first.replayed, first.reply.as_deref()), (false, Some("added to api#1")));
        assert_eq!(s.list_comments(task.id).unwrap()[0].id, comment);
        let changes = s.changes().unwrap();

        // The same key, the project in capitals or not, with other spacing in the body.
        let again = s.add_comment_retry("API#1", "bot", "done", &[], &retry("c1"), said).unwrap();
        assert!(again.replayed);
        assert_eq!((again.task, again.comment, again.reply), (first.task, first.comment, first.reply.clone()));
        assert_eq!(s.list_comments(task.id).unwrap().len(), 1);
        assert_eq!(s.changes().unwrap(), changes);

        // Another task, another way to name it, another project with the same number, another
        // author or body: all other requests. The number alone isn't the api project's, so it is
        // refused too, and the one that names the wrong project is never accepted as a match.
        s.create_project("web", "", None).unwrap();
        let changed = [
            (other.key(), "bot", "done"),
            (task.id.to_string(), "bot", "done"),
            (format!("web#{}", task.id), "bot", "done"),
            (key.clone(), "me", "done"),
            (key.clone(), "bot", "x"),
        ];
        for (named, author, body) in &changed {
            let error = s.add_comment_retry(named, author, body, &[], &retry("c1"), said).unwrap_err();
            assert!(refused(error), "{named} {author} {body}");
        }
        assert_eq!(s.list_comments(task.id).unwrap().len(), 1);
        assert!(s.list_comments(other.id).unwrap().is_empty());

        s.delete_comment(comment).unwrap();
        s.delete_task(task.id).unwrap();
        let changes = s.changes().unwrap();
        let after = s.add_comment_retry(&key, "bot", "done", &[], &retry("c1"), said).unwrap();
        assert!(after.replayed);
        assert_eq!((after.comment, after.reply), (first.comment, first.reply));
        assert!(s.list_comments(task.id).unwrap().is_empty());
        assert_eq!(s.changes().unwrap(), changes, "nothing came back");
        // A new request to a task that's gone is still an error, and keeps no receipt.
        assert!(matches!(s.add_comment_retry(&key, "bot", "done", &[], &retry("c2"), said), Err(Error::NotFound(_))));
        assert_eq!(receipts(&s), 1);
    }

    #[test]
    fn a_retried_next_task_takes_one_task_and_an_empty_answer_stays_empty() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let nothing = || -> Result<String> { Ok("nothing".into()) };
        let before = s.changes().unwrap();
        let empty = s.next_task_retry(None, "", &retry("n1"), nothing).unwrap();
        assert_eq!((empty.task, empty.reply.as_deref(), empty.replayed), (None, Some("nothing"), false));
        assert_eq!(s.changes().unwrap(), before, "taking nothing changes nothing on the board");

        // A task arrives; the same request still found nothing.
        let late = s.create_task(p.id, "late", "").unwrap();
        let changes = s.changes().unwrap();
        let again = s.next_task_retry(None, "  ", &retry("n1"), || Ok("changed".into())).unwrap();
        assert_eq!((again.task, again.reply.as_deref(), again.replayed), (None, Some("nothing"), true));
        assert_eq!(s.get_task(late.id).unwrap().status, "todo");
        assert_eq!(s.changes().unwrap(), changes);

        // A new one takes it, and keeps taking only it.
        let took = s.next_task_retry(None, "", &retry("n2"), nothing).unwrap();
        assert_eq!((took.task, took.reply, took.replayed), (Some(late.id), None, false));
        let next = s.create_task(p.id, "next", "").unwrap();
        let changes = s.changes().unwrap();
        let again = s.next_task_retry(None, "", &retry("n2"), nothing).unwrap();
        assert_eq!((again.task, again.replayed), (Some(late.id), true));
        assert_eq!(s.get_task(next.id).unwrap().status, "todo");
        assert_eq!(s.changes().unwrap(), changes);

        // Another project or filter is another request.
        assert!(refused(s.next_task_retry(Some(p.id), "", &retry("n2"), nothing).unwrap_err()));
        assert!(refused(s.next_task_retry(None, "tag:bug", &retry("n2"), nothing).unwrap_err()));
        assert_eq!(s.get_task(next.id).unwrap().status, "todo");

        // The task it took is gone, and still it takes no other.
        s.delete_task(late.id).unwrap();
        let again = s.next_task_retry(None, "", &retry("n2"), nothing).unwrap();
        assert_eq!((again.task, again.replayed), (Some(late.id), true));
        assert_eq!(s.get_task(next.id).unwrap().status, "todo");
    }

    #[test]
    fn duplicate_requests_on_separate_connections_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        let project = {
            let s = Store::open(&path, None).unwrap();
            s.migrate().unwrap();
            let p = s.create_project("api", "", None).unwrap();
            for title in ["a", "b", "c"] {
                s.create_task(p.id, title, "").unwrap();
            }
            p.id
        };
        let sent: Vec<(Receipt, Receipt, Receipt)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let s = Store::open(&path, None).unwrap();
                        let created =
                            s.create_task_retry(project, "New", TaskPatch::default(), &retry("same-c"), made).unwrap();
                        let commented =
                            s.add_comment_retry("1", "bot", "hi", &[], &retry("same-m"), |_| Ok("added".into()));
                        let took = s.next_task_retry(None, "", &retry("same-n"), || Ok("none".into()));
                        (created, commented.unwrap(), took.unwrap())
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        assert_eq!(sent.iter().filter(|r| !r.0.replayed).count(), 1);
        assert_eq!(sent.iter().filter(|r| !r.1.replayed).count(), 1);
        assert_eq!(sent.iter().filter(|r| !r.2.replayed).count(), 1);
        assert!(sent.iter().all(|r| r.0.task == sent[0].0.task && r.0.reply == sent[0].0.reply));
        assert!(sent.iter().all(|r| r.1.comment == sent[0].1.comment));
        assert!(sent.iter().all(|r| r.2.task == sent[0].2.task));

        let s = Store::open(&path, None).unwrap();
        assert_eq!(s.list_tasks(None, None).unwrap().len(), 4);
        assert_eq!(s.list_comments(1).unwrap().len(), 1);
        assert_eq!(s.list_tasks(None, Some("doing")).unwrap().len(), 1);
        assert_eq!(receipts(&s), 3);
    }

    #[test]
    fn receipts_survive_reopening_and_are_never_expired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        let first = {
            let s = Store::open(&path, None).unwrap();
            s.migrate().unwrap();
            let p = s.create_project("api", "", None).unwrap();
            let first = s.create_task_retry(p.id, "One", TaskPatch::default(), &retry("k"), made).unwrap();
            // Long ago, and many keys since.
            s.conn.execute("UPDATE requests SET created_at = '2001-01-01T00:00:00Z'", []).unwrap();
            for i in 0..20 {
                s.create_task_retry(p.id, "More", TaskPatch::default(), &retry(&format!("more-{i}")), made).unwrap();
            }
            first
        };
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        let p = s.find_project("api").unwrap();
        let again = s.create_task_retry(p.id, "One", TaskPatch::default(), &retry("k"), made).unwrap();
        assert!(again.replayed);
        assert_eq!((again.task, again.reply), (first.task, first.reply));
        assert_eq!((receipts(&s), s.list_tasks(None, None).unwrap().len()), (21, 21));
    }

    fn moved(before: &Task, after: &Task) -> Result<String> {
        Ok(format!("{} -> {}: {}", before.status, after.status, after.title))
    }

    #[test]
    fn a_retried_update_changes_the_task_once_and_replays_before_looking_at_the_task() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        s.create_project("web", "", None).unwrap();
        let task = s.create_task(p.id, "Ship it", "").unwrap();
        let blocker = s.create_task(p.id, "Blocker", "").unwrap();
        let patch =
            || TaskPatch { status: Some("doing".into()), title: Some(" Shipped ".into()), ..Default::default() };

        let first = s.update_task_retry("api#1", patch(), &retry("u1"), moved).unwrap();
        assert!(!first.replayed);
        let said = (first.task, first.comment, first.reply.as_deref());
        assert_eq!(said, (Some(task.id), None, Some("todo -> doing: Shipped")));
        assert_eq!(s.get_task(task.id).unwrap().status, "doing");

        // Someone else changes it further, which is what a version guard would see: the retry is
        // still answered from its receipt, before anything about the task is looked at.
        let next = TaskPatch { status: Some("review".into()), title: Some("Other".into()), ..Default::default() };
        s.update_task(task.id, next).unwrap();
        let waits = TaskPatch { human: Some(true), waits_on: Some(blocker.id.to_string()), ..Default::default() };
        s.update_task(task.id, waits).unwrap();
        let changes = s.changes().unwrap();
        let again = s.update_task_retry("api#1", patch(), &retry("u1"), moved).unwrap();
        assert!(again.replayed);
        assert_eq!((again.task, again.comment, again.reply), (first.task, first.comment, first.reply.clone()));
        let now = s.get_task(task.id).unwrap();
        assert_eq!((now.status.as_str(), now.title.as_str(), now.human), ("review", "Other", true));
        assert_eq!(s.changes().unwrap(), changes, "a replay doesn't wake the board");
        assert_eq!(receipts(&s), 1);

        // Another way to name the task, even for the same task, another project, or any other
        // field, or a field passed empty that wasn't passed, is another request.
        let with_title = |title: &str| TaskPatch { title: Some(title.into()), ..patch() };
        let requests = [
            ("1", patch()),
            ("web#1", patch()),
            ("api#2", patch()),
            ("api#1", with_title("Other")),
            ("api#1", TaskPatch { status: Some("review".into()), ..patch() }),
            ("api#1", TaskPatch { description: Some("".into()), ..patch() }),
            ("api#1", TaskPatch { tags: Some("".into()), ..patch() }),
            ("api#1", TaskPatch { url: Some("".into()), ..patch() }),
            ("api#1", TaskPatch { human: Some(false), ..patch() }),
            ("api#1", TaskPatch { waits_on: Some("".into()), ..patch() }),
            ("api#1", TaskPatch { status: None, ..patch() }),
        ];
        for (key, patch) in requests {
            let error = s.update_task_retry(key, patch, &retry("u1"), moved).unwrap_err();
            assert!(refused(error), "{key}");
        }
        // The same task as written, whatever the capitals of its project, and of what's cleaned.
        let spaced =
            TaskPatch { title: Some("  Shipped  ".into()), status: Some("doing".into()), ..Default::default() };
        assert!(s.update_task_retry(" API#1 ", spaced, &retry("u1"), moved).unwrap().replayed);
        let kept = s.get_task(task.id).unwrap();
        assert_eq!((kept.status.as_str(), kept.title.as_str(), kept.project.as_str()), ("review", "Other", "api"));
        assert_eq!(s.changes().unwrap(), changes);

        // Deleted since: still the same answer, and nothing comes back.
        s.delete_task(task.id).unwrap();
        let changes = s.changes().unwrap();
        let after = s.update_task_retry("api#1", patch(), &retry("u1"), moved).unwrap();
        assert!(after.replayed);
        assert_eq!((after.task, after.reply), (first.task, first.reply));
        assert_eq!(s.changes().unwrap(), changes);
        // A new request for the task is an error, and keeps no receipt.
        assert!(matches!(s.update_task_retry("api#1", patch(), &retry("u2"), moved), Err(Error::NotFound(_))));
        assert_eq!(receipts(&s), 1);
    }

    #[test]
    fn a_failed_update_request_changes_nothing_and_keeps_no_receipt() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let task = s.create_task(p.id, "Ship it", "").unwrap();
        let unchanged = || {
            let t = s.get_task(task.id).unwrap();
            (t.title, t.status, t.human, t.waiting_on.len(), s.list_comments(task.id).unwrap().len(), receipts(&s))
        };
        let before = unchanged();
        let changes = s.changes().unwrap();
        let patch = |change: fn(&mut TaskPatch)| {
            let mut patch = TaskPatch { title: Some("Changed".into()), human: Some(true), ..Default::default() };
            change(&mut patch);
            patch
        };

        // Refused before the write, or inside it after other fields were written.
        let refused_patches = [
            patch(|p| p.title = Some(" ".into())),
            patch(|p| p.url = Some("ftp://x".into())),
            patch(|p| p.status = Some("nope".into())),
            patch(|p| p.waits_on = Some("999".into())),
            // Itself, which would make a loop.
            patch(|p| p.waits_on = Some("1".into())),
        ];
        assert_eq!(task.id, 1);
        for refused_patch in refused_patches {
            assert!(s.update_task_retry("api#1", refused_patch, &retry("k"), moved).is_err());
            assert_eq!(unchanged(), before);
        }
        let no_reply = |_: &Task, _: &Task| -> Result<String> { Err(Error::Invalid("no".into())) };
        assert!(s.update_task_retry("api#1", patch(|_| {}), &retry("k"), no_reply).is_err());
        assert_eq!(unchanged(), before);

        // The receipt can't be saved: the fields written before it are rolled back.
        s.conn
            .execute_batch("CREATE TRIGGER no_receipts BEFORE INSERT ON requests BEGIN SELECT RAISE(ABORT, 'no'); END;")
            .unwrap();
        assert!(s.update_task_retry("api#1", patch(|_| {}), &retry("k"), moved).is_err());
        assert_eq!(unchanged(), before);
        assert_eq!(s.changes().unwrap(), changes);
        s.conn.execute_batch("DROP TRIGGER no_receipts").unwrap();

        let sent = s.update_task_retry("api#1", patch(|_| {}), &retry("k"), moved).unwrap();
        assert!(!sent.replayed, "the key was never reserved");
        let after = s.get_task(task.id).unwrap();
        assert_eq!((after.title.as_str(), after.human, receipts(&s)), ("Changed", true, 1));
    }

    #[test]
    fn a_retry_is_answered_even_when_what_it_asked_for_can_no_longer_be_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        let open = || {
            let s = Store::open(&path, None).unwrap();
            s.migrate().unwrap();
            s
        };
        let to = |status: &str| TaskPatch { status: Some(status.into()), ..Default::default() };
        let (create, update) = {
            let s = open();
            let p = s.create_project("api", "", None).unwrap();
            s.set_backlog(true).unwrap();
            // The board is read when it opens, so the status exists for this one.
            let s = open();
            let create = s.create_task_retry(p.id, "Idea", to("backlog"), &retry("c"), made).unwrap();
            let task = s.create_task(p.id, "Other", "").unwrap();
            let update = s.update_task_retry("api#2", to("backlog"), &retry("u"), moved).unwrap();
            assert_eq!((create.task, update.task), (Some(1), Some(task.id)));
            s.update_task(1, to("todo")).unwrap();
            s.update_task(task.id, to("todo")).unwrap();
            s.set_backlog(false).unwrap();
            (create, update)
        };
        let s = open();
        assert!(s.check_status("backlog").is_err(), "no backlog now");
        let again = s.create_task_retry(1, "Idea", to("backlog"), &retry("c"), made).unwrap();
        assert!(again.replayed);
        assert_eq!((again.task, again.reply), (create.task, create.reply));
        let again = s.update_task_retry("api#2", to("backlog"), &retry("u"), moved).unwrap();
        assert!(again.replayed);
        assert_eq!((again.task, again.reply), (update.task, update.reply));
        assert_eq!(s.get_task(2).unwrap().status, "todo");
        // A new request is checked against the board as it is, and keeps no receipt.
        assert!(s.create_task_retry(1, "Idea", to("backlog"), &retry("c2"), made).is_err());
        assert!(s.update_task_retry("api#2", to("backlog"), &retry("u2"), moved).is_err());
        assert_eq!(receipts(&s), 2);
    }

    #[test]
    fn a_board_from_before_receipts_gains_them_and_keeps_working_for_older_kanzo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        let before = MIGRATIONS.len() - 1;
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..before] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('compatible', ?2)",
                [before.to_string(), COMPATIBLE_FROM.to_string()],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO projects (slug, name) VALUES ('api', 'api');
                 INSERT INTO tasks (project_id, title, status, position) VALUES (1, 'kept', 'todo', 1);",
            )
            .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        let meta = |key: &str| -> String {
            s.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).unwrap()
        };
        assert_eq!(meta("schema_version"), MIGRATIONS.len().to_string());
        // The oldest version that can still use this board doesn't move, so an older Kanzo opens it as it is.
        assert_eq!(meta("compatible"), "4");
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["kept"]);
        assert_eq!(receipts(&s), 0);

        // What an older Kanzo writes, with the statements it has, still works, and the board sees it.
        let changes = s.changes().unwrap();
        s.conn
            .execute(
                "INSERT INTO tasks (project_id, title, status, position, tags) VALUES (1, 'older', 'todo', 2, '')",
                [],
            )
            .unwrap();
        s.conn.execute("UPDATE tasks SET title = 'older!' WHERE title = 'older'", []).unwrap();
        assert_eq!(titles(&s.list_tasks(None, None).unwrap()), ["kept", "older!"]);
        assert!(s.changes().unwrap() > changes);
        // And it knows nothing of requests, so its tasks have no receipts.
        assert_eq!(receipts(&s), 0);
    }
}
