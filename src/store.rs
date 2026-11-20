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
    // An edit version for every task, for update_task's optional `if_version` guard. A task's version is the
    // `rev` of its row here: 128 random bits from SQLite, new at every meaningful write. Not a counter, and
    // not a hash of the content: two copies of a board file that are each edited have each task's version
    // diverge on its own, so a version read from one copy is never taken by the other's different task, and
    // a task recreated under an old id never gets an old version. Triggers, not Store methods, bump it, so
    // writes by the board, bulk edits and older Kanzo versions count too. It moves only when what an edit
    // can overwrite changes: the fields below and the task's own wait list. Not the position, `updated_at`,
    // comments, other tasks, or what a task waits on getting done or renamed. The WHEN clause skips a write
    // that sets a value it already has. A new version is a DELETE then an INSERT, not INSERT OR REPLACE:
    // inside a trigger, an OR clause on the statement that fired it would override that one. No foreign key:
    // the cleanup trigger removes a deleted task's row, and a stray one is harmless because nothing reads it
    // without its task.
    "CREATE TABLE task_revs (
         task_id INTEGER PRIMARY KEY,
         rev     TEXT NOT NULL
     );
     INSERT INTO task_revs (task_id, rev) SELECT id, lower(hex(randomblob(16))) FROM tasks;
     CREATE TRIGGER tasks_rev_insert AFTER INSERT ON tasks BEGIN
         DELETE FROM task_revs WHERE task_id = NEW.id;
         INSERT INTO task_revs (task_id, rev) VALUES (NEW.id, lower(hex(randomblob(16))));
     END;
     CREATE TRIGGER tasks_rev_update AFTER UPDATE OF title, description, status, project_id, tags, url, human ON tasks
     WHEN NEW.title IS NOT OLD.title OR NEW.description IS NOT OLD.description OR NEW.status IS NOT OLD.status
         OR NEW.project_id IS NOT OLD.project_id OR NEW.tags IS NOT OLD.tags OR NEW.url IS NOT OLD.url
         OR NEW.human IS NOT OLD.human
     BEGIN
         DELETE FROM task_revs WHERE task_id = NEW.id;
         INSERT INTO task_revs (task_id, rev) VALUES (NEW.id, lower(hex(randomblob(16))));
     END;
     CREATE TRIGGER tasks_rev_before_delete BEFORE DELETE ON tasks BEGIN
         DELETE FROM task_revs WHERE task_id IN (SELECT task_id FROM task_waits WHERE waits_on_id = OLD.id);
         INSERT INTO task_revs (task_id, rev)
             SELECT task_id, lower(hex(randomblob(16))) FROM task_waits WHERE waits_on_id = OLD.id;
     END;
     CREATE TRIGGER tasks_rev_delete AFTER DELETE ON tasks BEGIN
         DELETE FROM task_revs WHERE task_id = OLD.id;
     END;
     CREATE TRIGGER task_waits_rev_insert AFTER INSERT ON task_waits BEGIN
         DELETE FROM task_revs WHERE task_id = NEW.task_id;
         INSERT INTO task_revs (task_id, rev) VALUES (NEW.task_id, lower(hex(randomblob(16))));
     END;
     CREATE TRIGGER task_waits_rev_delete AFTER DELETE ON task_waits
     WHEN EXISTS (SELECT 1 FROM tasks WHERE id = OLD.task_id)
     BEGIN
         DELETE FROM task_revs WHERE task_id = OLD.task_id;
         INSERT INTO task_revs (task_id, rev) VALUES (OLD.task_id, lower(hex(randomblob(16))));
     END;",
];

const PROJECT_COLORS: &[&str] = &["gray", "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
    Busy(String),
    /// A guarded edit built on a task version that is no longer the current one.
    Conflict(String),
    Db(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(m) | Error::Invalid(m) | Error::Busy(m) | Error::Conflict(m) => f.write_str(m),
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

const STATUSES: [&str; 4] = ["todo", "doing", "review", "done"];

pub const DONE: &str = STATUSES[3];

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
        let title = clean_title(title)?;
        let status = patch.status.as_deref().unwrap_or(STATUSES[0]);
        self.check_status(status)?;
        let description = patch.description.as_deref().unwrap_or("").trim();
        let tags = clean_tags(patch.tags.as_deref().unwrap_or("")).join(",");
        let id = self.write(|c| {
            let position = Self::end_of_column(c, status, 0)?;
            c.execute(
                "INSERT INTO tasks (project_id, title, description, status, position, tags, human) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![project_id, title, description, status, position, tags, patch.human.unwrap_or(false)],
            )?;
            let id = c.last_insert_rowid();
            if let Some(keys) = &patch.waits_on {
                self.set_waits(c, id, keys)?;
            }
            Ok(id)
        })?;
        self.get_task(id)
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
        Ok(self.update_task_if(id, patch, None)?.0)
    }

    /// `update_task`, and with `expect`, only if the task is still at that version (see `task_version`):
    /// otherwise `Error::Conflict` and nothing changes. The check and the edit are one write, and the check
    /// comes first, so two writers holding the same version can't both get in. Also returns the task's new
    /// version when `expect` was given, read in the same write.
    pub fn update_task_if(&self, id: i64, patch: TaskPatch, expect: Option<&str>) -> Result<(Task, Option<String>)> {
        self.update_task_as(|| self.get_task(id), patch, expect, |_, after, version| Ok((after, version)))
    }

    /// `update_task_if` for a caller that also tells what happened. `find` names the task, and `render` gets
    /// the task as it was, as it is now, and its new version when `expect` was given. Both run inside the
    /// write, so the key is resolved, the guard checked, the edit made and the reply built from one moment:
    /// a later writer can't change what the reply says. `find` and `render` must only read. If either
    /// fails, nothing is written.
    pub fn update_task_as<T>(
        &self,
        find: impl FnOnce() -> Result<Task>,
        patch: TaskPatch,
        expect: Option<&str>,
        render: impl FnOnce(&Task, Task, Option<String>) -> Result<T>,
    ) -> Result<T> {
        let title = patch.title.as_deref().map(clean_title).transpose()?;
        let url = patch.url.as_deref().map(clean_url).transpose()?;
        if let Some(s) = &patch.status {
            self.check_status(s)?;
        }
        self.write(|c| {
            let before = find()?;
            self.apply_edit(c, &before, &patch, (title.as_deref(), url.as_deref()), expect)?;
            let version = expect.map(|_| self.task_version(before.id)).transpose()?;
            let after = self.get_task(before.id)?;
            render(&before, after, version)
        })
    }

    /// The edit of `update_task_as` inside a write the caller already holds, for edits that also write
    /// something else, like a comment, which then follow the guard in the same transaction. `current` is
    /// the task just read in that write. Nothing is written before the guard passes. `(title, url)` are
    /// the patch's, already cleaned and checked.
    fn apply_edit(
        &self,
        c: &Connection,
        current: &Task,
        patch: &TaskPatch,
        (title, url): (Option<&str>, Option<&str>),
        expect: Option<&str>,
    ) -> Result<()> {
        let id = current.id;
        if let Some(given) = expect {
            self.check_version(current, given)?;
        }
        // Only what the patch changes is written, and only then is the task newer.
        let tags = patch.tags.as_deref().map(|t| clean_tags(t).join(","));
        c.execute(
            &format!(
                "UPDATE tasks SET title = coalesce(?1, title), project_id = coalesce(?2, project_id),
                   human = coalesce(?3, human), tags = coalesce(?4, tags),
                   description = coalesce(?5, description), url = coalesce(?7, url), updated_at = {NOW}
                 WHERE id = ?6 AND (coalesce(?1, title), coalesce(?2, project_id), coalesce(?3, human),
                   coalesce(?4, tags), coalesce(?5, description), coalesce(?7, url))
                   <> (title, project_id, human, tags, description, url)"
            ),
            params![title, patch.project_id, patch.human, tags, patch.description.as_deref().map(str::trim), id, url],
        )?;
        if let Some(s) = &patch.status
            && *s != current.status
        {
            let position = Self::end_of_column(c, s, id)?;
            c.execute(
                &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                params![s, position, id],
            )?;
        }
        if let Some(keys) = &patch.waits_on {
            self.set_waits(c, id, keys)?;
        }
        Ok(())
    }

    /// Runs `f` as one consistent read: the task, its links and its version all from the same moment.
    /// Inside a write that is already open, `f` just joins it; it never starts a transaction in one.
    /// `f` must only read.
    pub fn read<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return f();
        }
        self.conn.execute_batch("BEGIN")?;
        let out = f();
        let end = self.conn.execute_batch("COMMIT");
        let out = out?;
        end?;
        Ok(out)
    }

    /// The task's edit version, for `update_task_if`: `task.rev`, like `12.9c1f0a7e3b5d42688d0e1a4c7f2b9e35`.
    /// `rev` is 128 random bits that change only when what an edit can overwrite does: title, description,
    /// status, project, tags, url, human, and the task's own wait list; see migration 10. It names a version,
    /// not a content: an edit and its undo are two new versions. Copies of a board file that are edited
    /// apart get different ones, and a copy that wasn't edited has the same version and the same task.
    pub fn task_version(&self, id: i64) -> Result<String> {
        let rev: Option<String> = self
            .conn
            .query_row(
                "SELECT coalesce((SELECT rev FROM task_revs WHERE task_id = t.id), '') FROM tasks t WHERE t.id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let rev = rev.ok_or_else(|| Error::NotFound(format!("no task #{id}")))?;
        let rev = if rev.is_empty() { "0".repeat(32) } else { rev };
        Ok(format!("{id}.{rev}"))
    }

    /// Passes only the task's current version. A made-up one, or another task's, is `Invalid`, and an old
    /// one is a `Conflict`.
    fn check_version(&self, current: &Task, given: &str) -> Result<()> {
        let given = given.trim();
        if given == self.task_version(current.id)? {
            return Ok(());
        }
        let Some((task, _)) = parse_version(given) else {
            return Err(Error::Invalid("if_version is not a version from get_task with include_version".into()));
        };
        if task != current.id {
            return Err(Error::Invalid(format!(
                "if_version is from another task, not {}; get this task's with get_task and include_version",
                current.key()
            )));
        }
        Err(Error::Conflict(format!(
            "{} changed since you read it, so nothing was changed. Reread it with get_task and include_version, \
             reconcile, and retry.",
            current.key()
        )))
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
        let to = STATUSES[1];
        let id = self.write(|c| {
            let sql = format!("SELECT t.id {todo} AND NOT {WAITING} ORDER BY t.position LIMIT 1");
            let id: Option<i64> = c.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0)).optional()?;
            if let Some(id) = id {
                let position = Self::end_of_column(c, to, id)?;
                c.execute(
                    &format!("UPDATE tasks SET status = ?1, position = ?2, updated_at = {NOW} WHERE id = ?3"),
                    params![to, position, id],
                )?;
            }
            Ok(id)
        })?;
        id.map(|id| self.get_task(id)).transpose()
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
        let body = body.trim();
        if body.is_empty() && images.is_empty() {
            return Err(Error::Invalid("comment body is empty".into()));
        }
        images.iter().try_for_each(|f| check_image(f))?;
        let author = match author.trim() {
            "" => "me",
            a => a,
        };
        self.get_task(task_id)?;
        let id = self.write(|c| {
            c.execute(
                "INSERT INTO comments (task_id, author, body) VALUES (?1, ?2, ?3)",
                params![task_id, author, body],
            )?;
            let id = c.last_insert_rowid();
            let mut insert = c.prepare("INSERT INTO images (task_id, comment_id, file) VALUES (?1, ?2, ?3)")?;
            for file in images {
                insert.execute(params![task_id, id, file])?;
            }
            Ok(id)
        })?;
        self.get_comment(id)
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

/// Splits a version, `task.rev`, where the rev is 32 lowercase hex digits.
fn parse_version(version: &str) -> Option<(i64, &str)> {
    let (task, rev) = version.split_once('.')?;
    let hex = rev.len() == 32 && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then_some(())?;
    Some((task.parse().ok()?, rev))
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

    fn retitle(title: &str) -> TaskPatch {
        TaskPatch { title: Some(title.into()), ..Default::default() }
    }

    #[test]
    fn a_guarded_edit_needs_the_version_it_read() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "one", "").unwrap();
        let v1 = s.task_version(t.id).unwrap();
        assert!(parse_version(&v1).is_some(), "{v1}");
        assert_eq!(s.task_version(t.id).unwrap(), v1, "reading changes nothing");
        assert!(matches!(s.task_version(999), Err(Error::NotFound(_))));

        let (two, v2) = s.update_task_if(t.id, retitle("two"), Some(&v1)).unwrap();
        let v2 = v2.unwrap();
        assert_eq!(two.title, "two");
        assert_eq!(s.task_version(t.id).unwrap(), v2);
        assert_ne!(v2, v1);

        // Made in the same second, from the same read: the second writer loses and changes nothing.
        let changes = s.changes().unwrap();
        let err = s.update_task_if(t.id, retitle("three"), Some(&v1)).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        assert!(err.to_string().contains(&t.key()), "{err}");
        assert_eq!(s.get_task(t.id).unwrap().title, "two");
        assert_eq!(s.changes().unwrap(), changes, "a conflict is no write, so no change to announce");
        assert_eq!(s.task_version(t.id).unwrap(), v2);

        // The version an edit gives back guards the next one; an edit with no guard still works, and counts.
        let (_, v3) = s.update_task_if(t.id, retitle("three"), Some(&v2)).unwrap();
        let v3 = v3.unwrap();
        assert_eq!(s.update_task(t.id, retitle("four")).unwrap().title, "four");
        assert!(matches!(s.update_task_if(t.id, retitle("five"), Some(&v3)), Err(Error::Conflict(_))));
        assert_eq!(s.get_task(t.id).unwrap().title, "four");
    }

    #[test]
    fn the_version_moves_only_when_what_an_edit_overwrites_changes() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let web = s.create_project("web", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let t = s.create_task(p.id, "task", "text").unwrap();
        let other = s.create_task(p.id, "other", "").unwrap();
        let version = || s.task_version(t.id).unwrap();

        let patches = [
            ("title", retitle("renamed")),
            ("description", TaskPatch { description: Some("new text".into()), ..Default::default() }),
            ("status", TaskPatch { status: Some("review".into()), ..Default::default() }),
            ("project", TaskPatch { project_id: Some(web.id), ..Default::default() }),
            ("tags", TaskPatch { tags: Some("bug".into()), ..Default::default() }),
            ("url", TaskPatch { url: Some("https://example.com/1".into()), ..Default::default() }),
            ("human", TaskPatch { human: Some(true), ..Default::default() }),
            ("waits", waits(&blocker.id.to_string())),
            ("waits cleared", waits("")),
        ];
        for (what, patch) in patches {
            let before = version();
            s.update_task(t.id, patch).unwrap();
            assert_ne!(version(), before, "{what}");
        }

        let unmoved = |what: &str, act: &dyn Fn()| {
            let before = version();
            act();
            assert_eq!(version(), before, "{what}");
        };
        s.create_task_in(web.id, "mate", TaskPatch { status: Some("review".into()), ..Default::default() }).unwrap();
        let position = s.get_task(t.id).unwrap().position;
        unmoved("a move within its column", &|| {
            s.move_task(t.id, "review", None).unwrap();
        });
        assert_ne!(s.get_task(t.id).unwrap().position, position, "it did move");
        unmoved("a comment", &|| {
            s.add_comment(t.id, "me", "hi", &[]).unwrap();
        });
        unmoved("another task", &|| {
            s.update_task(other.id, retitle("other renamed")).unwrap();
        });
        unmoved("another task starting to wait on it", &|| {
            s.update_task(other.id, waits(&t.id.to_string())).unwrap();
        });

        // What it waits on is its blocker's own business: finishing or renaming it is not an edit of the task.
        s.update_task(t.id, waits(&blocker.id.to_string())).unwrap();
        unmoved("a blocker finishing", &|| {
            s.move_task(blocker.id, "done", None).unwrap();
        });
        unmoved("a blocker renamed", &|| {
            s.update_task(blocker.id, retitle("blocker renamed")).unwrap();
        });

        let moved = |what: &str, act: &dyn Fn()| {
            let before = version();
            act();
            assert_ne!(version(), before, "{what}");
        };
        moved("a move to another column", &|| {
            s.move_task(t.id, "doing", None).unwrap();
        });
        moved("a bulk tag", &|| s.update_tasks(&[t.id], None, "extra", "").unwrap());
        moved("a bulk status", &|| s.update_tasks(&[t.id], Some("todo"), "", "").unwrap());

        let solo = s.create_project("solo", "", None).unwrap();
        let first = s.create_task(solo.id, "first", "").unwrap();
        let before = s.task_version(first.id).unwrap();
        assert_eq!(s.next_task(Some(solo.id)).unwrap().map(|t| t.id), Some(first.id));
        assert_ne!(s.task_version(first.id).unwrap(), before, "next_task");
    }

    #[test]
    fn a_guarded_edit_that_changes_nothing_keeps_the_version_and_a_stale_one_conflicts() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let t = s.create_task_in(p.id, "same", TaskPatch { description: Some("d".into()), ..Default::default() });
        let t = t.unwrap();
        s.update_task(t.id, waits(&blocker.id.to_string())).unwrap();
        let v = s.task_version(t.id).unwrap();
        let (changes, updated) = (s.changes().unwrap(), s.get_task(t.id).unwrap().updated_at);

        let same = TaskPatch {
            title: Some("same".into()),
            description: Some(" d ".into()),
            status: Some("todo".into()),
            project_id: Some(p.id),
            tags: Some("".into()),
            url: Some("".into()),
            human: Some(false),
            waits_on: Some(format!("#{}", blocker.id)),
        };
        for patch in [same, TaskPatch::default(), retitle("same")] {
            let (_, now) = s.update_task_if(t.id, patch, Some(&v)).unwrap();
            assert_eq!(now.as_deref(), Some(v.as_str()));
        }
        assert_eq!(s.task_version(t.id).unwrap(), v);
        assert_eq!((s.changes().unwrap(), s.get_task(t.id).unwrap().updated_at), (changes, updated));

        s.update_task(t.id, retitle("changed")).unwrap();
        let err = s.update_task_if(t.id, retitle("changed"), Some(&v)).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "a stale version conflicts even when the edit would change nothing");
    }

    #[test]
    fn a_restored_task_is_a_new_version() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap();
        let t = s.create_task(p.id, "a", "").unwrap();
        let start = s.task_version(t.id).unwrap();

        // Changed and changed back: the same content, but not the version it was read at.
        s.update_task(t.id, retitle("b")).unwrap();
        s.update_task(t.id, retitle("a")).unwrap();
        s.update_task(t.id, waits(&blocker.id.to_string())).unwrap();
        s.update_task(t.id, waits("")).unwrap();
        let back = s.get_task(t.id).unwrap();
        assert_eq!((back.title.as_str(), back.waiting_on.is_empty()), ("a", true));
        assert_ne!(s.task_version(t.id).unwrap(), start);
        assert!(matches!(s.update_task_if(t.id, retitle("c"), Some(&start)), Err(Error::Conflict(_))));
        assert_eq!(s.get_task(t.id).unwrap().title, "a");
    }

    #[test]
    fn a_version_of_another_task_or_none_at_all_changes_nothing() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let a = s.create_task(p.id, "a", "").unwrap();
        let b = s.create_task(p.id, "b", "").unwrap();
        let (va, vb) = (s.task_version(a.id).unwrap(), s.task_version(b.id).unwrap());
        let (id, rev) = va.split_once('.').unwrap();
        assert_eq!(rev.len(), 32, "128 bits");
        let changes = s.changes().unwrap();

        let invalid = [
            vb.clone(),
            format!("{}.{rev}", b.id),
            String::new(),
            " ".into(),
            "nope".into(),
            "1.2".into(),
            format!("{va}.9"),
            format!("{id}.{}g", &rev[..31]),
            format!("{id}.{}", &rev[..31]),
            format!("x.{rev}"),
        ];
        for given in &invalid {
            let err = s.update_task_if(a.id, retitle("hijacked"), Some(given)).unwrap_err();
            assert!(matches!(err, Error::Invalid(_)), "{given:?}: {err}");
        }
        let made_up = format!("{id}.{}", "0".repeat(32));
        assert!(matches!(s.update_task_if(a.id, retitle("hijacked"), Some(&made_up)), Err(Error::Conflict(_))));
        assert!(matches!(s.update_task_if(999, retitle("x"), Some(&va)), Err(Error::NotFound(_))));

        assert_eq!(s.get_task(a.id).unwrap().title, "a");
        assert_eq!((s.changes().unwrap(), s.task_version(a.id).unwrap()), (changes, va.clone()));
        assert_eq!(s.task_version(b.id).unwrap(), vb);
        // Whitespace around a good one is fine.
        let padded = format!(" {va}\n");
        s.update_task_if(a.id, retitle("a2"), Some(&padded)).unwrap();
    }

    #[test]
    fn a_refused_guarded_patch_changes_nothing_and_keeps_the_version() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let a = s.create_task(p.id, "a", "").unwrap();
        let b = s.create_task(p.id, "b", "").unwrap();
        s.update_task(a.id, waits(&b.id.to_string())).unwrap();
        let v = s.task_version(b.id).unwrap();
        let changes = s.changes().unwrap();

        let looping = TaskPatch { title: Some("new".into()), waits_on: Some(a.id.to_string()), ..Default::default() };
        let patches = [
            looping,
            TaskPatch { title: Some("new".into()), status: Some("nope".into()), ..Default::default() },
            TaskPatch { title: Some("new".into()), url: Some("javascript:1".into()), ..Default::default() },
            TaskPatch { title: Some("new".into()), waits_on: Some("999".into()), ..Default::default() },
            retitle("  "),
        ];
        for patch in patches {
            assert!(s.update_task_if(b.id, patch, Some(&v)).is_err());
            assert_eq!(s.get_task(b.id).unwrap().title, "b");
        }
        assert_eq!((s.changes().unwrap(), s.task_version(b.id).unwrap()), (changes, v.clone()));
        s.update_task_if(b.id, retitle("fine"), Some(&v)).unwrap();
    }

    /// What the board, bulk edits and older Kanzo versions do in SQL, with nothing from `Store`.
    #[test]
    fn writes_by_the_board_and_by_older_versions_count_too() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let [t, o, z, w] = ["t", "o", "z", "w"].map(|title| s.create_task(p.id, title, "").unwrap().id);
        let project = p.id;
        let run = |statement: String, moves: bool, what: &str| {
            let before = s.task_version(t).unwrap();
            s.conn.execute_batch(&statement).unwrap();
            assert_eq!(s.task_version(t).unwrap() != before, moves, "{what}");
        };
        let old = "'2001-01-01T00:00:00Z'";
        run(
            format!(
                "UPDATE tasks SET title = title, description = description, status = status, project_id = project_id,
                   tags = tags, url = url, human = human, updated_at = {old} WHERE id = {t}"
            ),
            false,
            "an older version writing the values it already has",
        );
        run(format!("UPDATE tasks SET position = position + 5 WHERE id = {t}"), false, "position");
        run(format!("UPDATE tasks SET updated_at = {old} WHERE id = {t}"), false, "updated_at");
        run(format!("INSERT INTO comments (task_id, author, body) VALUES ({t}, 'me', 'hi')"), false, "a comment");
        run(format!("UPDATE tasks SET title = 'other', status = 'done' WHERE id = {o}"), false, "another task");
        run(format!("INSERT INTO task_waits (task_id, waits_on_id) VALUES ({z}, {t})"), false, "a task waiting on it");
        run(format!("UPDATE tasks SET title = 'legacy' WHERE id = {t}"), true, "title");
        run(format!("UPDATE tasks SET description = 'legacy' WHERE id = {t}"), true, "description");
        run(format!("UPDATE tasks SET status = 'doing', position = 9 WHERE id = {t}"), true, "status");
        run(format!("UPDATE tasks SET tags = 'a,b' WHERE id = {t}"), true, "tags");
        run(format!("UPDATE tasks SET url = 'https://x.test' WHERE id = {t}"), true, "url");
        run(format!("UPDATE tasks SET human = 1 WHERE id = {t}"), true, "human");
        run(format!("UPDATE tasks SET project_id = {project}, title = 'again' WHERE id = {t}"), true, "mixed");
        run(format!("UPDATE OR REPLACE tasks SET title = 'replaced' WHERE id = {t}"), true, "an OR REPLACE update");
        run(format!("INSERT INTO task_waits (task_id, waits_on_id) VALUES ({t}, {o})"), true, "a wait edge");
        run(format!("INSERT OR IGNORE INTO task_waits (task_id, waits_on_id) VALUES ({t}, {w})"), true, "an OR IGNORE");
        run(format!("DELETE FROM task_waits WHERE task_id = {t} AND waits_on_id = {o}"), true, "a wait edge removed");
        run(format!("DELETE FROM task_waits WHERE task_id = {t}"), true, "all of them removed");

        // A task an older version adds is versioned too, and never like another's.
        s.conn
            .execute("INSERT INTO tasks (project_id, title, status, position) VALUES (?1, 'new', 'todo', 1)", [project])
            .unwrap();
        let new = s.conn.last_insert_rowid();
        let all: Vec<String> = [t, o, z, w, new].iter().map(|id| s.task_version(*id).unwrap()).collect();
        let distinct: std::collections::HashSet<_> = all.iter().map(|v| v.rsplit('.').next().unwrap()).collect();
        assert_eq!(distinct.len(), all.len(), "{all:?}");
    }

    #[test]
    fn deleting_tasks_with_wait_edges_works_and_moves_the_waiters() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|title| s.create_task(p.id, title, "").unwrap().id);
        s.update_task(a, waits(&format!("{b}, {d}"))).unwrap();
        s.update_task(c, waits(&d.to_string())).unwrap();
        let version = |id| s.task_version(id).unwrap();
        let (va, vc) = (version(a), version(c));

        s.delete_task(b).unwrap();
        assert_ne!(version(a), va, "what it waited on is gone");
        assert_eq!(version(c), vc);
        s.delete_task(a).unwrap();
        assert!(matches!(s.task_version(a), Err(Error::NotFound(_))));
        assert_eq!(version(c), vc);
        s.delete_task(d).unwrap();
        assert_ne!(version(c), vc);
        let revs: i64 = s.conn.query_row("SELECT count(*) FROM task_revs", [], |r| r.get(0)).unwrap();
        assert_eq!(revs, 1, "a deleted task leaves no version behind");

        let two = s.create_project("two", "", None).unwrap();
        let x = s.create_task(two.id, "x", "").unwrap();
        s.create_task_in(two.id, "y", waits(&x.id.to_string())).unwrap();
        s.update_task(c, waits(&x.id.to_string())).unwrap();
        s.delete_project("two").unwrap();
        assert!(s.get_task(c).unwrap().waiting_on.is_empty());
        let unversioned: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM tasks t WHERE NOT EXISTS (SELECT 1 FROM task_revs r WHERE r.task_id = t.id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unversioned, 0);
    }

    #[test]
    fn a_recreated_task_never_takes_an_old_version() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "old", "").unwrap();
        let v = s.task_version(t.id).unwrap();
        s.delete_task(t.id).unwrap();
        assert!(matches!(s.update_task_if(t.id, retitle("x"), Some(&v)), Err(Error::NotFound(_))));

        // Ids aren't reused, but a restored or imported board could do it.
        s.conn
            .execute(
                "INSERT INTO tasks (id, project_id, title, status, position) VALUES (?1, ?2, 'reborn', 'todo', 1)",
                [t.id, p.id],
            )
            .unwrap();
        assert_ne!(s.task_version(t.id).unwrap(), v);
        assert!(matches!(s.update_task_if(t.id, retitle("x"), Some(&v)), Err(Error::Conflict(_))));
        assert_eq!(s.get_task(t.id).unwrap().title, "reborn");
    }

    #[test]
    fn a_guarded_edit_joins_a_write_that_is_already_open() {
        let (_dir, s) = store();
        let p = s.create_project("api", "", None).unwrap();
        let t = s.create_task(p.id, "a", "").unwrap();
        let v = s.task_version(t.id).unwrap();
        let edit = |c: &Connection, title: &str, expect: &str| -> Result<()> {
            let current = s.get_task(t.id)?;
            s.apply_edit(c, &current, &TaskPatch::default(), (Some(title), None), Some(expect))
        };

        // Two guarded edits in one write, the second on the version the first made, and a read inside it.
        let title = s
            .write(|c| {
                edit(c, "b", &v)?;
                let v2 = s.task_version(t.id)?;
                edit(c, "c", &v2)?;
                s.read(|| Ok(s.get_task(t.id)?.title))
            })
            .unwrap();
        assert_eq!(title, "c");

        // The second one holds the version the first already used up: both are undone.
        let v = s.task_version(t.id).unwrap();
        let err = s.write(|c| {
            edit(c, "first", &v)?;
            edit(c, "second", &v)
        });
        assert!(matches!(err, Err(Error::Conflict(_))));
        assert_eq!(s.get_task(t.id).unwrap().title, "c");
        assert_eq!(s.task_version(t.id).unwrap(), v);

        let failed = s.read::<()>(|| Err(Error::Invalid("x".into())));
        assert!(failed.is_err() && s.conn.is_autocommit(), "a failed read leaves no transaction open");
    }

    #[test]
    fn a_read_sees_one_moment_while_a_writer_waits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        let p = s.create_project("api", "", None).unwrap();
        let id = s.create_task(p.id, "before", "").unwrap().id;
        let writer = Store::open(&path, None).unwrap();
        let (started, wait) = std::sync::mpsc::channel();

        let (first, second, handle) = s
            .read(|| {
                let first = (s.get_task(id)?.title, s.task_version(id)?);
                let handle = std::thread::spawn(move || {
                    started.send(()).unwrap();
                    writer.update_task(id, retitle("after")).unwrap();
                });
                wait.recv().unwrap();
                std::thread::sleep(Duration::from_millis(300));
                let second = (s.get_task(id)?.title, s.task_version(id)?);
                Ok((first, second, handle))
            })
            .unwrap();
        assert_eq!(first, second, "the task and its version came from one moment");
        handle.join().unwrap();
        assert_eq!(s.get_task(id).unwrap().title, "after");
        assert_ne!(s.task_version(id).unwrap(), first.1);
    }

    #[test]
    fn two_writers_with_the_same_version_cannot_both_win() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        let p = s.create_project("api", "", None).unwrap();
        let id = s.create_task(p.id, "start", "").unwrap().id;

        for round in 0..5 {
            let v = s.task_version(id).unwrap();
            let writers: Vec<Store> = (0..2).map(|_| Store::open(&path, None).unwrap()).collect();
            let barrier = std::sync::Barrier::new(2);
            let won: Vec<bool> = std::thread::scope(|scope| {
                let handles: Vec<_> = writers
                    .into_iter()
                    .enumerate()
                    .map(|(i, writer)| {
                        let (barrier, v) = (&barrier, v.as_str());
                        scope.spawn(move || {
                            barrier.wait();
                            match writer.update_task_if(id, retitle(&format!("w{round}-{i}")), Some(v)) {
                                Ok(_) => true,
                                Err(Error::Conflict(_)) => false,
                                Err(e) => panic!("{e}"),
                            }
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert_eq!(won.iter().filter(|w| **w).count(), 1, "round {round}: {won:?}");
            assert_ne!(s.task_version(id).unwrap(), v);
        }
    }

    #[test]
    fn migration_10_versions_the_tasks_a_board_already_has() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
            for sql in &MIGRATIONS[..9] {
                c.execute_batch(sql).unwrap();
            }
            c.execute_batch(
                "INSERT INTO meta VALUES ('schema_version', '9'), ('compatible', '4');
                 INSERT INTO projects (slug, name) VALUES ('api', 'api');
                 INSERT INTO tasks (project_id, title, status, position) VALUES (1, 'a', 'todo', 1), (1, 'b', 'todo', 2);
                 INSERT INTO task_waits (task_id, waits_on_id) VALUES (2, 1);
                 INSERT INTO comments (task_id, author, body) VALUES (1, 'me', 'hi');",
            )
            .unwrap();
            assert!(c.execute_batch("SELECT 1 FROM task_revs").is_err(), "version 9 has no versions");
        }
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        let (a, b) = (s.task_version(1).unwrap(), s.task_version(2).unwrap());
        assert!(parse_version(&a).is_some() && parse_version(&b).is_some() && a != b, "{a} {b}");
        assert_eq!((s.get_task(2).unwrap().waiting_on, s.get_task(1).unwrap().comments), (vec![1], 1));

        let (_, v) = s.update_task_if(1, retitle("A"), Some(&a)).unwrap();
        assert_ne!(v.unwrap(), a);
        assert_eq!(s.task_version(2).unwrap(), b, "another task's version is its own");
        s.migrate().unwrap();
        assert_eq!(s.task_version(2).unwrap(), b);
        // Every statement above the Store ran against what version 9 left, so an older Kanzo's writes
        // reach the triggers the same way: see `writes_by_the_board_and_by_older_versions_count_too`.
    }

    #[test]
    fn copies_of_a_board_edited_apart_never_share_a_version() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (dir.path().join("one.db"), dir.path().join("two.db"));
        let first = Store::open(&one, None).unwrap();
        first.migrate().unwrap();
        let p = first.create_project("api", "", None).unwrap();
        let id = first.create_task(p.id, "start", "").unwrap().id;
        let read = first.task_version(id).unwrap();
        drop(first);
        std::fs::copy(&one, &two).unwrap();
        assert_eq!(Store::open(&two, None).unwrap().task_version(id).unwrap(), read, "a copy is the same snapshot");

        // The same task, edited once in each copy, from the same read: the same number of writes each.
        let edit = |path: &Path, title: &str| -> String {
            let s = Store::open(path, None).unwrap();
            s.update_task_if(id, retitle(title), Some(&read)).unwrap().1.unwrap()
        };
        let (from_one, from_two) = (edit(&one, "from one"), edit(&two, "from two"));
        assert_ne!(from_one, from_two);

        // Sync swaps the other copy in between an agent's read of this one and its write.
        std::fs::copy(&two, &one).unwrap();
        let s = Store::open(&one, None).unwrap();
        let changes = s.changes().unwrap();
        let err = s.update_task_if(id, retitle("late"), Some(&from_one)).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        assert_eq!((s.get_task(id).unwrap().title.as_str(), s.changes().unwrap()), ("from two", changes));
        // A version read from the very state now on the board is still good: it is that snapshot.
        s.update_task_if(id, retitle("fine"), Some(&from_two)).unwrap();

        // Also when the two copies are each edited by older code, in SQL.
        std::fs::copy(&one, &two).unwrap();
        let (a, b) = (Store::open(&one, None).unwrap(), Store::open(&two, None).unwrap());
        let before = a.task_version(id).unwrap();
        a.conn.execute("UPDATE tasks SET title = 'legacy a' WHERE id = ?1", [id]).unwrap();
        b.conn.execute("UPDATE tasks SET title = 'legacy b' WHERE id = ?1", [id]).unwrap();
        let (va, vb) = (a.task_version(id).unwrap(), b.task_version(id).unwrap());
        assert!(va != vb && va != before && vb != before);
    }

    #[test]
    fn a_guarded_reply_is_built_inside_the_write_it_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanzo.db");
        let s = Store::open(&path, None).unwrap();
        s.migrate().unwrap();
        let p = s.create_project("api", "", None).unwrap();
        let blocker = s.create_task(p.id, "blocker", "").unwrap().id;
        let task = s.create_task_in(p.id, "t", waits(&blocker.to_string())).unwrap().id;
        let v = s.task_version(task).unwrap();
        let writer = Store::open(&path, None).unwrap();
        let (started, wait) = std::sync::mpsc::channel();
        let key = format!("api#{task}");

        // While the edit's reply is built, another writer tries to finish the blocker and hide waiting.
        let seen = s
            .update_task_as(
                || s.find_task(&key),
                retitle("renamed"),
                Some(&v),
                |before, after, version| {
                    let handle = std::thread::spawn(move || {
                        started.send(()).unwrap();
                        let done = TaskPatch { status: Some("done".into()), ..Default::default() };
                        writer.update_task(blocker, done).unwrap();
                        writer.set_property("waiting", false).unwrap();
                    });
                    wait.recv().unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    let live = (s.get_task(after.id)?.waiting_on, s.hidden_properties()?);
                    Ok((before.title.clone(), after.waiting_on, version, live, handle))
                },
            )
            .unwrap();
        let (before, waiting, version, live, handle) = seen;
        assert_eq!((before.as_str(), waiting), ("t", vec![blocker]));
        assert_eq!(live, (vec![blocker], Vec::<String>::new()), "the reply saw the board as the edit did");
        assert_eq!(version.as_deref(), Some(s.task_version(task).unwrap().as_str()));

        handle.join().unwrap();
        assert!(s.get_task(task).unwrap().waiting_on.is_empty(), "the other writer came after");
        assert_eq!(s.hidden_properties().unwrap(), ["waiting"]);

        // A key that names another task's project, or no task, fails in the write and changes nothing.
        let changes = s.changes().unwrap();
        let v = s.task_version(task).unwrap();
        for key in [format!("web#{task}"), "api#999".to_string()] {
            let outcome = s.update_task_as(|| s.find_task(&key), retitle("x"), Some(&v), |_, after, _| Ok(after.title));
            assert!(matches!(outcome, Err(Error::NotFound(_))), "{key}");
        }
        assert_eq!((s.get_task(task).unwrap().title.as_str(), s.changes().unwrap()), ("renamed", changes));
    }
}
