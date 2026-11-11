use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::home::Home;
use crate::share::Net;
use crate::store::{self, Comment, Project, Store, Task, short_time};

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Markdown,
    Json,
}

#[derive(Serialize)]
struct Board {
    exported_at: String,
    statuses: Vec<String>,
    projects: Vec<ProjectOut>,
}

#[derive(Serialize)]
struct ProjectOut {
    #[serde(flatten)]
    project: Project,
    tasks: Vec<TaskOut>,
}

#[derive(Serialize)]
struct TaskOut {
    id: i64,
    key: String,
    title: String,
    description: String,
    tags: Vec<String>,
    url: String,
    human: bool,
    waits_on: Vec<String>,
    images: Vec<String>,
    status: String,
    created_at: String,
    updated_at: String,
    comments: Vec<Comment>,
}

impl TaskOut {
    fn new(task: Task, comments: Vec<Comment>, images: Vec<String>, waits_on: Vec<String>) -> Self {
        Self {
            key: task.key(),
            id: task.id,
            title: task.title,
            description: task.description,
            tags: task.tags,
            url: task.url,
            human: task.human,
            waits_on,
            images,
            status: task.status,
            created_at: task.created_at,
            updated_at: task.updated_at,
            comments,
        }
    }
}

pub fn export(home: &Home, format: Format, project: Option<&str>, output: Option<&Path>) -> Result<()> {
    if let Net::Joined(host) = Net::load(home) {
        bail!("This computer uses the board at {host}: run kanzo export on that computer, or kanzo leave first.");
    }
    let store = home.store()?;
    let board = collect(&store, project, &home.images())?;
    let text = match format {
        Format::Json => serde_json::to_string_pretty(&board)? + "\n",
        Format::Markdown => markdown(&board),
    };
    match output {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
            let tasks: usize = board.projects.iter().map(|p| p.tasks.len()).sum();
            eprintln!("Exported {} projects and {tasks} tasks to {}", board.projects.len(), path.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn collect(store: &Store, only: Option<&str>, images: &Path) -> Result<Board> {
    let projects = match only {
        Some(slug) => vec![store.find_project(slug)?],
        None => store.list_projects()?,
    };
    let mut out = Vec::new();
    for project in projects {
        let mut tasks = Vec::new();
        for task in store.list_tasks(Some(project.id), None)? {
            let comments = store.list_comments(task.id)?;
            let paths = store.list_images(task.id)?.into_iter().map(|i| images.join(i.file).display().to_string());
            let waits = store.task_links(task.id, false)?.into_iter().map(|t| format!("{}#{}", t.project, t.id));
            tasks.push(TaskOut::new(task, comments, paths.collect(), waits.collect()));
        }
        out.push(ProjectOut { project, tasks });
    }
    Ok(Board { exported_at: store.now()?, statuses: store.statuses().to_vec(), projects: out })
}

fn markdown(board: &Board) -> String {
    let mut md = format!("# Kanzo board\n\nExported {}.\n", short_time(&board.exported_at));
    for p in &board.projects {
        let links: Vec<String> = p.project.repos.iter().map(|r| format!("[{r}](https://github.com/{r})")).collect();
        let repo = if links.is_empty() { String::new() } else { format!(" ({})", links.join(", ")) };
        let _ = write!(
            md,
            "\n## {} `{}`{repo}\n\n{} open of {} {}.\n",
            p.project.name,
            p.project.slug,
            p.project.open,
            p.project.total,
            store::tasks(p.project.total)
        );
        // Tasks can keep a status that is no longer in the config.
        let mut statuses = board.statuses.clone();
        for t in &p.tasks {
            if !statuses.contains(&t.status) {
                statuses.push(t.status.clone());
            }
        }
        for status in &statuses {
            let tasks: Vec<_> = p.tasks.iter().filter(|t| t.status == *status).collect();
            if tasks.is_empty() {
                continue;
            }
            let _ = write!(md, "\n### {status}\n");
            for t in tasks {
                let _ = write!(
                    md,
                    "\n#### {} {}\n\nCreated {}, updated {}.\n",
                    t.key,
                    t.title,
                    short_time(&t.created_at),
                    short_time(&t.updated_at)
                );
                if t.human {
                    md.push_str("Human task.\n");
                }
                if !t.tags.is_empty() {
                    let _ = writeln!(md, "Tags: {}.", t.tags.join(", "));
                }
                if !t.waits_on.is_empty() {
                    let _ = writeln!(md, "Waits on: {}.", t.waits_on.join(", "));
                }
                if !t.url.is_empty() {
                    let _ = writeln!(md, "URL: <{}>", t.url);
                }
                if !t.images.is_empty() {
                    let _ = writeln!(md, "Images: {}.", t.images.join(", "));
                }
                if !t.description.is_empty() {
                    let _ = write!(md, "\n{}\n", t.description);
                }
                if !t.comments.is_empty() {
                    md.push_str("\nComments:\n\n");
                    for c in &t.comments {
                        // Indent follow-on lines so a multi-line comment stays inside its list item.
                        let body = c.body.replace('\n', "\n  ");
                        let _ = writeln!(md, "- **{}**, {}: {body}", c.author, short_time(&c.created_at));
                    }
                }
            }
        }
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        let store = home.store().unwrap();
        let api = store.create_project("api", "API", Some("me/api")).unwrap();
        store.create_project("web", "Web", None).unwrap();
        let t = store.create_task(api.id, "Fix login", "Steps:\n1. log in").unwrap();
        store.add_comment(t.id, "claude-code", "Found it\nin auth.rs", &[]).unwrap();
        let done = store.create_task(api.id, "Old work", "").unwrap();
        store.move_task(done.id, "done", None).unwrap();
        let waits = store::TaskPatch { waits_on: Some(done.key()), ..Default::default() };
        store.update_task(t.id, waits).unwrap();

        let board = collect(&store, None, Path::new("/k/images")).unwrap();
        let md = markdown(&board);
        assert!(md.contains("## API `api` ([me/api](https://github.com/me/api))"), "{md}");
        assert!(md.contains("### todo\n\n#### api#1 Fix login"), "{md}");
        assert!(md.contains("Steps:\n1. log in"), "{md}");
        assert!(md.contains("Waits on: api#2."), "{md}");
        assert!(md.contains("- **claude-code**, ") && md.contains("Found it\n  in auth.rs"), "{md}");
        assert!(md.contains("### done\n\n#### api#2 Old work"), "{md}");
        assert!(md.contains("## Web `web`"), "{md}");

        let json: serde_json::Value = serde_json::to_value(&board).unwrap();
        assert_eq!(json["statuses"][0], "todo");
        assert_eq!(json["projects"][0]["slug"], "api");
        assert_eq!(json["projects"][0]["tasks"][0]["key"], "api#1");
        assert_eq!(json["projects"][0]["tasks"][0]["waits_on"][0], "api#2");
        assert_eq!(json["projects"][0]["tasks"][0]["comments"][0]["author"], "claude-code");

        let only = collect(&store, Some("web"), Path::new("/k/images")).unwrap();
        assert_eq!(only.projects.len(), 1);
        assert!(collect(&store, Some("nope"), Path::new("/k/images")).is_err());
    }

    #[test]
    fn refuses_a_joined_board() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        home.store().unwrap().create_project("local", "Local", None).unwrap();
        let out = tmp.path().join("board.json");

        std::fs::write(home.dir.join("network"), "join 192.168.0.12:4242\n").unwrap();
        let err = export(&home, Format::Json, None, Some(&out)).unwrap_err().to_string();
        assert!(err.contains("192.168.0.12:4242") && err.contains("kanzo leave"), "{err}");
        assert!(!out.exists());

        std::fs::write(home.dir.join("network"), "share\n").unwrap();
        export(&home, Format::Json, None, Some(&out)).unwrap();
        assert!(std::fs::read_to_string(&out).unwrap().contains("\"slug\": \"local\""));
    }
}
