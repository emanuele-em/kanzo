//! Replies are short plain text to save agents' tokens.

use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use axum::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Extensions, Implementation,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::share::{self, Net};
use crate::store::{self, Error, Project, Store, Task, TaskPatch, short_time};

/// An old board has thousands of done tasks.
const DONE_LISTED: usize = 20;

macro_rules! instructions {
    ($whose:literal, $project:literal) => {
        concat!(
            "The user's kanban board, on ",
            $whose,
            " machine, for all their projects. Tasks look like api#12. ",
            $project,
            " Keep it current unasked: doing when you start a task, comments on what you did, review when you \
             finish or get stuck, url for a PR you opened. Leave done and human tasks to the user."
        )
    };
}

const INSTRUCTIONS: &str =
    instructions!("this", "Without `project`, Kanzo uses the project linked to this folder's GitHub repo.");
const HTTP_INSTRUCTIONS: &str =
    instructions!("their", "Over HTTP Kanzo can't see your folder, so pass `project` (see list_projects).");

const NO_PROJECTS: &str = "There are no projects yet. Create one with create_project.";

pub async fn serve(home: Home) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repos = git_repos(&cwd);
    let images = home.images();
    let ctx = Ctx { home, cwd: Some(cwd), repos, images, host: Default::default() };
    let server = Kanzo { ctx: Arc::new(ctx), tool_router: Kanzo::tools() };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// `web::protect` checks Host and Origin, including when the board listens on the network,
/// so rmcp's localhost-only list is turned off.
pub fn http_service(home: Home, shutdown: CancellationToken) -> StreamableHttpService<Kanzo, LocalSessionManager> {
    let images = home.images();
    let ctx = Arc::new(Ctx { home, cwd: None, repos: Vec::new(), images, host: Default::default() });
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts().with_cancellation_token(shutdown);
    // rmcp makes a server per session, and for some requests; the tool list is built once.
    let tool_router = Kanzo::tools();
    StreamableHttpService::new(
        move || Ok(Kanzo { ctx: ctx.clone(), tool_router: tool_router.clone() }),
        Default::default(),
        config,
    )
}

struct Ctx {
    home: Home,
    cwd: Option<PathBuf>,
    repos: Vec<String>,
    images: PathBuf,
    host: tokio::sync::Mutex<Option<share::Upstream>>,
}

impl Ctx {
    fn scope(&self, store: &Store, name: Option<&str>) -> store::Result<Option<Project>> {
        if name.is_some_and(|p| p.trim().eq_ignore_ascii_case("all")) {
            return Ok(None);
        }
        self.project(store, name).map(Some)
    }

    fn project(&self, store: &Store, name: Option<&str>) -> store::Result<Project> {
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            return store.find_project(name);
        }
        for repo in &self.repos {
            if let Some(p) = store.project_by_repo(repo)? {
                return Ok(p);
            }
        }
        let known: Vec<String> = store.list_projects()?.into_iter().map(|p| p.slug).collect();
        let known = known.join(", ");
        let msg = match (self.cwd.as_ref().map(|c| c.display()), self.repos.first(), known.is_empty()) {
            (_, None, true) => NO_PROJECTS.to_string(),
            (None, ..) => {
                format!("Pass `project` (one of: {known}); over HTTP Kanzo cannot see which folder you work in.")
            }
            (Some(cwd), Some(repo), false) => format!(
                "No project is linked to {repo} (the git remote of {cwd}). Pass `project` (one of: {known}), \
                 link this repo to one with link_repo, or create a project for it with create_project and repo \"{repo}\"."
            ),
            (Some(cwd), Some(repo), true) => format!(
                "No project is linked to {repo} (the git remote of {cwd}), and there are no projects yet. \
                 Create one with create_project and repo \"{repo}\"."
            ),
            (Some(cwd), None, false) => {
                format!("Cannot tell the project: {cwd} has no git remote. Pass `project` (one of: {known}).")
            }
        };
        Err(Error::NotFound(msg))
    }
}

fn reply(outcome: Result<String, String>) -> CallToolResult {
    match outcome {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
    }
}

fn git_repos(dir: &std::path::Path) -> Vec<String> {
    let Ok(text) = crate::home::output(Command::new("git").arg("-C").arg(dir).args(["remote", "-v"])) else {
        return Vec::new();
    };
    // `origin` first, so it wins when several remotes are linked to different projects.
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_by_key(|l| !l.starts_with("origin\t"));
    let mut repos = Vec::new();
    for line in lines {
        if let Some(repo) = line.split_whitespace().nth(1).and_then(store::repo_from_url)
            && !repos.contains(&repo)
        {
            repos.push(repo);
        }
    }
    repos
}

#[derive(Clone)]
pub struct Kanzo {
    ctx: Arc<Ctx>,
    tool_router: ToolRouter<Self>,
}

/// Agents are told it is a string, which keeps the tool schemas short; a bare number is accepted too.
#[derive(Deserialize)]
#[serde(untagged)]
enum TaskKey {
    Number(i64),
    Text(String),
}

impl TaskKey {
    fn find(&self, store: &Store) -> store::Result<Task> {
        match self {
            TaskKey::Number(n) => store.get_task(*n),
            TaskKey::Text(s) => store.find_task(s),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct CreateProjectArgs {
    /// Short lowercase id, like api.
    slug: String,
    name: Option<String>,
    /// GitHub owner/repo.
    repo: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListTasksArgs {
    /// Slug, or "all".
    project: Option<String>,
    /// One column, or "all". Done is left out by default.
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
}

#[derive(Deserialize, JsonSchema)]
struct CreateTaskArgs {
    title: String,
    /// Markdown.
    description: Option<String>,
    /// Slug.
    project: Option<String>,
    /// Comma-separated.
    tags: Option<String>,
    /// Column; defaults to todo.
    status: Option<String>,
    /// For the user, not for agents.
    human: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    title: Option<String>,
    /// Markdown.
    description: Option<String>,
    /// Column, like review.
    status: Option<String>,
    /// Comma-separated; "" clears.
    tags: Option<String>,
    /// For the user, not for agents.
    human: Option<bool>,
    /// A link; "" clears.
    url: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddCommentArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    /// Markdown.
    body: String,
    /// Defaults to your client's name.
    author: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct LinkRepoArgs {
    /// Slug.
    project: String,
    /// GitHub owner/repo.
    repo: String,
}

#[derive(Deserialize, JsonSchema)]
struct NextTaskArgs {
    /// Slug, or "all".
    project: Option<String>,
}

impl Kanzo {
    /// Drops rmcp's `$schema` line and the `null` of optional fields: agents read this list in
    /// every session, so each byte counts.
    fn tools() -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        for route in router.map.values_mut() {
            let mut schema = (*route.attr.input_schema).clone();
            schema.remove("$schema");
            // Optional fields are typed `["string", "null"]`; leaving them out of
            // `required` already says they're optional.
            if let Some(serde_json::Value::Object(props)) = schema.get_mut("properties") {
                for prop in props.values_mut() {
                    if let Some(kind) = prop.get_mut("type")
                        && let Some(first) = kind.as_array().and_then(|k| k.first()).cloned()
                    {
                        *kind = first;
                    }
                }
            }
            route.attr.input_schema = Arc::new(schema);
        }
        router
    }
}

#[tool_router]
impl Kanzo {
    async fn run(&self, f: impl FnOnce(&Ctx, &Store) -> store::Result<String> + Send + 'static) -> CallToolResult {
        self.run_for(&Extensions::default(), f).await
    }

    async fn run_for(
        &self,
        ext: &Extensions,
        f: impl FnOnce(&Ctx, &Store) -> store::Result<String> + Send + 'static,
    ) -> CallToolResult {
        let header = |name| {
            let value = ext.get::<Parts>()?.headers.get(name)?;
            Some(String::from_utf8_lossy(value.as_bytes()).into_owned())
        };
        let ctx = match header(share::FOLDER_HEADER) {
            Some(folder) => Arc::new(Ctx {
                home: self.ctx.home.clone(),
                cwd: Some(folder.into()),
                repos: header(share::REPOS_HEADER)
                    .unwrap_or_default()
                    .split(',')
                    .filter(|r| !r.is_empty())
                    .map(String::from)
                    .collect(),
                images: header(share::IMAGES_HEADER).map_or_else(|| self.ctx.images.clone(), PathBuf::from),
                host: Default::default(),
            }),
            None => self.ctx.clone(),
        };
        let outcome = tokio::task::spawn_blocking(move || {
            let store = ctx.home.store().map_err(|e| e.to_string())?;
            f(&ctx, &store).map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("internal error: {e}")));
        reply(outcome)
    }

    #[tool(description = "List projects with their repos and open task counts.")]
    async fn list_projects(&self, ext: Extensions) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, |ctx, store| {
                let projects = store.list_projects()?;
                if projects.is_empty() {
                    return Ok(NO_PROJECTS.into());
                }
                let here = projects.iter().find(|p| p.repos.iter().any(|r| ctx.repos.contains(r)));
                let mut out = String::new();
                for p in &projects {
                    out.push_str(&p.slug);
                    if p.name != p.slug {
                        let _ = write!(out, " \"{}\"", p.name);
                    }
                    if !p.repos.is_empty() {
                        let _ = write!(out, " ({})", p.repos.join(", "));
                    }
                    let _ = write!(out, ": {} open of {}", p.open, p.total);
                    if here.is_some_and(|h| h.id == p.id) {
                        out.push_str(" <- current repo");
                    }
                    out.push('\n');
                }
                out.pop();
                Ok(out)
            })
            .await)
    }

    #[tool(description = "Create a project.")]
    async fn create_project(
        &self,
        Parameters(args): Parameters<CreateProjectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let p = store.create_project(&args.slug, args.name.as_deref().unwrap_or(""), args.repo.as_deref())?;
                let repo = p.repos.first().map(|r| format!(", linked to {r}")).unwrap_or_default();
                Ok(format!("Created project {} ({}){repo}.", p.slug, p.name))
            })
            .await)
    }

    #[tool(description = "Link another GitHub repo to a project.")]
    async fn link_repo(&self, Parameters(args): Parameters<LinkRepoArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let p = store.link_repo(&args.project, &args.repo)?;
                Ok(format!("Project {} is linked to {}.", p.slug, p.repos.join(", ")))
            })
            .await)
    }

    #[tool(description = "List tasks in board order.")]
    async fn list_tasks(
        &self,
        Parameters(args): Parameters<ListTasksArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let project = ctx.scope(store, args.project.as_deref())?;
                let status = args.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
                let only = match status {
                    Some("all") | None => None,
                    Some(s) => {
                        store.check_status(s)?;
                        Some(s)
                    }
                };
                let keep = if status.is_none() { 0 } else { DONE_LISTED };
                let (tasks, hidden) =
                    store.list_tasks_keeping_done(project.as_ref().map(|p| p.id), only, Some(keep))?;
                let done = store::DONE;
                let properties = store.hidden_properties()?;
                let mut out = String::new();
                let mut people = 0;
                for t in &tasks {
                    if t.human {
                        people += 1;
                    } else {
                        task_line(&mut out, t, &properties);
                        out.push('\n');
                    }
                }
                if out.is_empty() {
                    let scope = project.as_ref().map_or("any project", |p| p.slug.as_str());
                    let _ = writeln!(out, "No tasks in {scope}.");
                }
                if people > 0 && shown(&properties, "human") {
                    let _ = writeln!(
                        out,
                        "({people} human {} not shown: for the user, not for agents)",
                        store::tasks(people)
                    );
                }
                if hidden > 0 && status.is_none() {
                    let _ = writeln!(out, "({hidden} {done} not shown; pass status \"{done}\" or \"all\")");
                } else if hidden > 0 {
                    let _ = writeln!(out, "({hidden} older {done} not shown)");
                }
                out.pop();
                Ok(out)
            })
            .await)
    }

    #[tool(description = "Show a task with its comments and image paths.")]
    async fn get_task(
        &self,
        Parameters(args): Parameters<TaskArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let task = args.id.find(store)?;
                task_detail(ctx, store, &task)
            })
            .await)
    }

    #[tool(description = "Add a task, to todo unless you pass status.")]
    async fn create_task(
        &self,
        Parameters(args): Parameters<CreateTaskArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let project = ctx.project(store, args.project.as_deref())?;
                let status = args.status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                let description = args.description.as_deref().unwrap_or("");
                let mut t = store.create_task_in(project.id, status.as_deref(), &args.title, description)?;
                if args.tags.is_some() || args.human.is_some() {
                    t = store
                        .update_task(t.id, TaskPatch { tags: args.tags, human: args.human, ..Default::default() })?;
                }
                Ok(format!("Created {} in {}: {}", t.key(), t.status, t.title))
            })
            .await)
    }

    #[tool(description = "Change the fields you pass.")]
    async fn update_task(&self, Parameters(args): Parameters<UpdateTaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let before = args.id.find(store)?;
                let status = args.status.map(|s| s.trim().to_ascii_lowercase());
                let after = store.update_task(
                    before.id,
                    TaskPatch {
                        title: args.title,
                        description: args.description,
                        status,
                        tags: args.tags,
                        project_id: None,
                        human: args.human,
                        url: args.url,
                    },
                )?;
                let moved = if before.status != after.status {
                    format!(", {} -> {}", before.status, after.status)
                } else {
                    String::new()
                };
                Ok(format!("Updated {}{moved}: {}", after.key(), after.title))
            })
            .await)
    }

    #[tool(description = "Comment on a task, like to report progress.")]
    async fn add_comment(
        &self,
        Parameters(args): Parameters<AddCommentArgs>,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = peer.peer_info().map(|i| i.client_info.name.clone()).filter(|n| !n.is_empty());
        let author = args.author.filter(|a| !a.trim().is_empty()).or(client).unwrap_or_else(|| "agent".into());
        Ok(self
            .run(move |_, store| {
                let task = args.id.find(store)?;
                store.add_comment(task.id, &author, &args.body, &[])?;
                Ok(format!("Comment added to {} as {author}.", task.key()))
            })
            .await)
    }

    #[tool(description = "Take the top todo task, move it to doing and show it.")]
    async fn next_task(
        &self,
        Parameters(args): Parameters<NextTaskArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let project = ctx.scope(store, args.project.as_deref())?;
                match store.next_task(project.as_ref().map(|p| p.id))? {
                    Some(task) => Ok(format!("Moved to {}:\n{}", task.status, task_detail(ctx, store, &task)?)),
                    None => {
                        let scope = project.map_or("any project".into(), |p| p.slug);
                        Ok(format!("Nothing in todo for {scope}."))
                    }
                }
            })
            .await)
    }

    #[tool(description = "Delete a task with its comments and images. Can't be undone.")]
    async fn delete_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let task = args.id.find(store)?;
                let properties = store.hidden_properties()?;
                ctx.home.deleting_images(store, store::ImagesOf::Task(task.id), || store.delete_task(task.id))?;
                let count = if shown(&properties, "comments") { task.comments } else { 0 };
                let comments = match count {
                    0 => String::new(),
                    1 => " and its comment".into(),
                    n => format!(" and its {n} comments"),
                };
                Ok(format!("Deleted {}{comments}: {}", task.key(), task.title))
            })
            .await)
    }
}

// Serve the router built once in `tools()`; by default the macro builds a new,
// untrimmed one for every request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Kanzo {
    /// Checked on every call, so agents already running switch boards when this computer joins or leaves.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Over HTTP, the board itself sends calls on when it joined another.
        if let Some(cwd) = &self.ctx.cwd
            && let Net::Joined(host) = Net::load(&self.ctx.home)
        {
            let mut upstream = self.ctx.host.lock().await;
            if upstream.as_ref().is_none_or(|u| u.host() != host) {
                *upstream = Some(share::Upstream::new(host, cwd, &self.ctx.repos, &self.ctx.images));
            }
            let client = context.peer.peer_info().and_then(|p| serde_json::to_value(&*p).ok());
            let upstream = upstream.as_mut().expect("set above");
            let outcome = upstream.call(client, &request.name, request.arguments).await;
            return Ok(reply(outcome).into());
        }
        self.tool_router.call(ToolCallContext::new(self, request, context)).await
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kanzo", env!("CARGO_PKG_VERSION")))
            .with_instructions(if self.ctx.cwd.is_some() { INSTRUCTIONS } else { HTTP_INSTRUCTIONS })
    }
}

fn shown(hidden: &[String], property: &str) -> bool {
    !hidden.iter().any(|p| p == property)
}

fn task_line(out: &mut String, t: &Task, hidden: &[String]) {
    let _ = write!(out, "{} [{}] {}", t.key(), t.status, t.title);
    if shown(hidden, "tags") {
        write_tags(out, t);
    }
    match if shown(hidden, "comments") { t.comments } else { 0 } {
        0 => {}
        1 => out.push_str(" (1 comment)"),
        n => {
            let _ = write!(out, " ({n} comments)");
        }
    }
}

fn write_tags(out: &mut String, t: &Task) {
    if !t.tags.is_empty() {
        let _ = write!(out, " {{{}}}", t.tags.join(", "));
    }
}

fn task_detail(ctx: &Ctx, store: &Store, t: &Task) -> store::Result<String> {
    let hidden = store.hidden_properties()?;
    let comments = if shown(&hidden, "comments") { store.list_comments(t.id)? } else { Vec::new() };
    let images = if shown(&hidden, "images") { store.list_images(t.id)? } else { Vec::new() };
    let dir = &ctx.images;
    let paths = |comment: Option<i64>| -> Vec<String> {
        images.iter().filter(|i| i.comment_id == comment).map(|i| dir.join(&i.file).display().to_string()).collect()
    };
    let (created, updated) = (short_time(&t.created_at), short_time(&t.updated_at));
    let mut out = format!("{} {}", t.key(), t.title);
    if shown(&hidden, "tags") {
        write_tags(&mut out, t);
    }
    let _ = write!(out, "\nstatus: {}", t.status);
    if t.human && shown(&hidden, "human") {
        out.push_str(", human: for the user, not for agents");
    }
    if shown(&hidden, "created") {
        let _ = write!(out, ", created {created}");
    }
    if updated != created && shown(&hidden, "updated") {
        let _ = write!(out, ", updated {updated}");
    }
    if !t.url.is_empty() && shown(&hidden, "url") {
        let _ = write!(out, "\nurl: {}", t.url);
    }
    out.push('\n');
    if !t.description.is_empty() && shown(&hidden, "description") {
        let _ = writeln!(out, "\n{}", t.description);
    }
    let own = paths(None);
    if !own.is_empty() {
        let _ = writeln!(out, "\nImages: {}", own.join(", "));
    }
    if !comments.is_empty() {
        let _ = writeln!(out, "\nComments ({}):", comments.len());
        for c in &comments {
            let _ = write!(out, "- {}, {}: {}", c.author, short_time(&c.created_at), c.body);
            let attached = paths(Some(c.id));
            if !attached.is_empty() {
                let sep = if c.body.is_empty() { "" } else { " " };
                let _ = write!(out, "{sep}[images: {}]", attached.join(", "));
            }
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    Ok(out)
}
