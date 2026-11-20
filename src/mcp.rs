//! Replies are short plain text to save agents' tokens.

use std::collections::HashSet;
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
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::share::{self, Net};
use crate::store::{self, Error, Project, Receipt, Retry, Store, Task, TaskPatch, short_time};

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
             finish or get stuck, url for a PR you opened. On review, comment how to resume this session. Leave done and human tasks to the user. \
             Link files as [name](file:///path)."
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
    serve_stdio(server).await
}

#[cfg(unix)]
use crate::stdio::serve as serve_stdio;

/// Windows can't replace a running program, so there an agent's `kanzo mcp` keeps its version.
#[cfg(not(unix))]
async fn serve_stdio(server: Kanzo) -> Result<()> {
    use rmcp::ServiceExt;
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

/// Clients on protocol 2026-07-28 name themselves on every request; older ones did once, in `initialize`.
fn caller(context: &RequestContext<RoleServer>) -> Option<Implementation> {
    match context.meta.protocol_version() {
        Some(_) => context.meta.client_info(),
        None => context.peer.peer_info().map(|p| p.client_info.clone()),
    }
}

/// What a `request_id` is kept with besides the id itself: the client's name, and the computer a
/// joined board's call came from. The folder comes from the `Ctx`, which has the guest's own for a
/// joined board's call.
struct Sender {
    client: String,
    computer: String,
}

impl Sender {
    fn of(context: &RequestContext<RoleServer>) -> Self {
        let client = caller(context).map(|i| i.name).unwrap_or_default();
        let computer = context
            .extensions
            .get::<Parts>()
            .and_then(|p| p.headers.get(share::COMPUTER_HEADER))
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .unwrap_or_default();
        Self { client, computer }
    }

    /// `None` without a `request_id`. A given one that isn't valid, even an empty one, is an error.
    fn retry(&self, ctx: &Ctx, request_id: Option<&str>) -> store::Result<Option<Retry>> {
        let folder = ctx.cwd.as_ref().map(|c| c.display().to_string()).unwrap_or_default();
        request_id.map(|id| Retry::new(&self.client, &self.computer, &folder, id)).transpose()
    }
}

/// The reply a receipt kept, for the tools that make theirs inside the write.
fn reply_of(receipt: Receipt) -> store::Result<String> {
    receipt.reply.ok_or_else(|| Error::Invalid("this request_id has no saved result".into()))
}

/// Why `next_task` took nothing.
fn nothing_ready(store: &Store, project: Option<&Project>, filter: &str) -> store::Result<String> {
    let waiting = store.waiting_in_todo(project.map(|p| p.id), filter)?;
    let scope = project.map_or("any project", |p| p.slug.as_str());
    Ok(match waiting {
        0 => format!("Nothing in todo for {scope}."),
        1 => format!("Nothing ready in todo for {scope}: 1 task waits on others."),
        n => format!("Nothing ready in todo for {scope}: {n} tasks wait on others."),
    })
}

/// What a retry of a request that took a task says when the task is gone. The task it took stays
/// the one it took: a retry never takes another.
fn taken_and_deleted(id: i64) -> String {
    format!("Already taken earlier: task #{id}, which has since been deleted.")
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
    /// As the caller wrote it.
    fn text(&self) -> String {
        match self {
            TaskKey::Number(n) => n.to_string(),
            TaskKey::Text(s) => s.clone(),
        }
    }

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
    /// Words, or tag: status: project: title: description: url: comment: id: comments: images: created: updated:
    /// is:human is:waiting has: no:, like tag:bug -status:done. Includes done.
    filter: Option<String>,
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
    /// Tasks it waits on, like api#12.
    waits_on: Option<String>,
    /// Your retry key; reusing it replays the first result.
    request_id: Option<String>,
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
    /// Tasks it waits on, like api#12; "" clears.
    waits_on: Option<String>,
    /// Your retry key; reusing it replays the first result.
    request_id: Option<String>,
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
    /// Your retry key; reusing it replays the first result.
    request_id: Option<String>,
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
    /// As in list_tasks, like tag:bug.
    filter: Option<String>,
    /// Your retry key; reusing it replays the first result.
    request_id: Option<String>,
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
                let status = args.status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                let only = match status.as_deref() {
                    Some("all") | None => None,
                    Some(s) => {
                        store.check_status(s)?;
                        Some(s)
                    }
                };
                let filter = args.filter.filter(|f| !f.trim().is_empty());
                let keep = match (&filter, &status) {
                    (Some(_), _) => None,
                    (None, None) => Some(0),
                    (None, Some(_)) => Some(DONE_LISTED),
                };
                let (mut tasks, hidden) = store.list_tasks_keeping_done(project.as_ref().map(|p| p.id), only, keep)?;
                if let Some(filter) = &filter {
                    let found: HashSet<i64> = store.search_for_agent(filter)?.into_iter().map(|t| t.id).collect();
                    tasks.retain(|t| found.contains(&t.id));
                }
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
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let sender = Sender::of(&context);
        Ok(self
            .run_for(&context.extensions, move |ctx, store| {
                let CreateTaskArgs { title, description, project, tags, status, human, waits_on, request_id } = args;
                let retry = sender.retry(ctx, request_id.as_deref())?;
                let project = ctx.project(store, project.as_deref())?;
                let status = status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                let patch = TaskPatch { description, status, tags, human, waits_on, ..Default::default() };
                let created = |t: &Task| -> store::Result<String> {
                    Ok(format!("Created {} in {}: {}{}", t.key(), t.status, t.title, waits_note(store, t)?))
                };
                match retry {
                    None => created(&store.create_task_in(project.id, &title, patch)?),
                    Some(retry) => reply_of(store.create_task_retry(project.id, &title, patch, &retry, created)?),
                }
            })
            .await)
    }

    #[tool(description = "Change the fields you pass.")]
    async fn update_task(
        &self,
        Parameters(args): Parameters<UpdateTaskArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let sender = Sender::of(&context);
        Ok(self
            .run_for(&context.extensions, move |ctx, store| {
                let UpdateTaskArgs { id, title, description, status, tags, human, url, waits_on, request_id } = args;
                let retry = sender.retry(ctx, request_id.as_deref())?;
                let status = status.map(|s| s.trim().to_ascii_lowercase());
                let patch = TaskPatch { title, description, status, tags, project_id: None, human, url, waits_on };
                let updated = |before: &Task, after: &Task| -> store::Result<String> {
                    let moved = if before.status != after.status {
                        format!(", {} -> {}", before.status, after.status)
                    } else {
                        String::new()
                    };
                    Ok(format!("Updated {}{moved}: {}{}", after.key(), after.title, waits_note(store, after)?))
                };
                match retry {
                    None => {
                        let before = id.find(store)?;
                        let after = store.update_task(before.id, patch)?;
                        updated(&before, &after)
                    }
                    Some(retry) => {
                        let key = id.text();
                        reply_of(store.update_task_retry(&key, patch, &retry, updated)?)
                    }
                }
            })
            .await)
    }

    #[tool(description = "Comment on a task, like to report progress.")]
    async fn add_comment(
        &self,
        Parameters(args): Parameters<AddCommentArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = caller(&context).map(|i| i.name).filter(|n| !n.is_empty());
        let author = args.author.filter(|a| !a.trim().is_empty()).or(client).unwrap_or_else(|| "agent".into());
        let sender = Sender::of(&context);
        let request_id = args.request_id;
        Ok(self
            .run_for(&context.extensions, move |ctx, store| {
                let Some(retry) = sender.retry(ctx, request_id.as_deref())? else {
                    let task = args.id.find(store)?;
                    store.add_comment(task.id, &author, &args.body, &[])?;
                    return Ok(format!("Comment added to {} as {author}.", task.key()));
                };
                let key = args.id.text();
                let added =
                    |t: &Task| -> store::Result<String> { Ok(format!("Comment added to {} as {author}.", t.key())) };
                reply_of(store.add_comment_retry(&key, &author, &args.body, &[], &retry, added)?)
            })
            .await)
    }

    #[tool(description = "Take the top todo task, move it to doing and show it.")]
    async fn next_task(
        &self,
        Parameters(args): Parameters<NextTaskArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let sender = Sender::of(&context);
        Ok(self
            .run_for(&context.extensions, move |ctx, store| {
                let retry = sender.retry(ctx, args.request_id.as_deref())?;
                let project = ctx.scope(store, args.project.as_deref())?;
                let id = project.as_ref().map(|p| p.id);
                let filter = args.filter.as_deref().unwrap_or("");
                let nothing = || nothing_ready(store, project.as_ref(), filter);
                let Some(retry) = retry else {
                    return match store.next_task_matching(id, filter)? {
                        Some(task) => Ok(format!("Moved to {}:\n{}", task.status, task_detail(ctx, store, &task)?)),
                        None => nothing(),
                    };
                };
                let receipt = store.next_task_retry(id, filter, &retry, nothing)?;
                match (receipt.task, receipt.reply) {
                    // It found none, and said so then.
                    (_, Some(text)) => Ok(text),
                    // The task it took, shown as it is now, for the receipt doesn't keep the details:
                    // the first call says it moved it, a retry that it was taken, which is all it knows.
                    (Some(task), None) => match store.get_task(task) {
                        Ok(task) if receipt.replayed => {
                            Ok(format!("Already taken earlier:\n{}", task_detail(ctx, store, &task)?))
                        }
                        Ok(task) => Ok(format!("Moved to {}:\n{}", task.status, task_detail(ctx, store, &task)?)),
                        Err(Error::NotFound(_)) => Ok(taken_and_deleted(task)),
                        Err(e) => Err(e),
                    },
                    (None, None) => Err(Error::Invalid("this request_id has no saved result".into())),
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
            let client = share::handshake(context.peer.peer_info().as_deref(), &context.meta);
            let upstream = upstream.as_mut().expect("set above");
            let outcome = upstream.call(client, &request.name, request.arguments).await;
            return Ok(reply(outcome).into());
        }
        self.tool_router.call(ToolCallContext::new(self, request, context)).await
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
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
    if shown(hidden, "waiting") {
        write_waits(out, t);
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

fn write_waits(out: &mut String, t: &Task) {
    if !t.waiting_on.is_empty() {
        let ids: Vec<String> = t.waiting_on.iter().map(|id| format!("#{id}")).collect();
        let _ = write!(out, " (waits on {})", ids.join(", "));
    }
}

/// Tells an agent that moved or changed a task that it's still waiting; moving one anyway is allowed.
fn waits_note(store: &Store, t: &Task) -> store::Result<String> {
    let mut note = String::new();
    if !t.waiting_on.is_empty() && shown(&store.hidden_properties()?, "waiting") {
        write_waits(&mut note, t);
    }
    Ok(note)
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
    if shown(&hidden, "waiting") {
        write_waits(&mut out, t);
    }
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::io::{
        AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
    };

    use rmcp::ServiceExt;

    use super::*;

    const STATELESS: &str = "2026-07-28";

    /// The client side of a `kanzo mcp` on stdio.
    struct Agent {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        write: WriteHalf<DuplexStream>,
    }

    impl Agent {
        fn start(home: Home, folder: &std::path::Path) -> Self {
            let images = home.images();
            let ctx = Ctx { home, cwd: Some(folder.into()), repos: Vec::new(), images, host: Default::default() };
            let server = Kanzo { ctx: Arc::new(ctx), tool_router: Kanzo::tools() };
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move { server.serve(tokio::io::split(theirs)).await.unwrap().waiting().await });
            let (read, write) = tokio::io::split(ours);
            Self { lines: BufReader::new(read).lines(), write }
        }

        /// The reply to `message`, or `None` for a notification.
        async fn send(&mut self, message: Value) -> Option<Value> {
            self.write.write_all(format!("{message}\n").as_bytes()).await.unwrap();
            let id = message.get("id")?;
            loop {
                let reply: Value = serde_json::from_str(&self.lines.next_line().await.unwrap().unwrap()).unwrap();
                if &reply["id"] == id {
                    return Some(reply);
                }
            }
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn board() -> (tempfile::TempDir, Home, Task) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path().to_path_buf()).unwrap();
        let store = home.store().unwrap();
        let project = store.create_project("api", "", None).unwrap();
        let task = store.create_task(project.id, "Ship it", "").unwrap();
        (dir, home, task)
    }

    /// Serves `home`'s MCP over HTTP on a free port, as a shared board does.
    async fn shared(home: Home) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().nest_service("/mcp", http_service(home, CancellationToken::new()));
        tokio::spawn(async move { axum::serve(listener, app).await });
        port
    }

    fn authors(home: &Home, task: &Task) -> Vec<String> {
        home.store().unwrap().list_comments(task.id).unwrap().into_iter().map(|c| c.author).collect()
    }

    fn comment(task: &Task, meta: Value) -> Value {
        let args = json!({ "id": task.key(), "body": "done" });
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "add_comment", "arguments": args, "_meta": meta } })
    }

    fn stateless(client: Option<&str>) -> Value {
        let mut meta = json!({
            "io.modelcontextprotocol/protocolVersion": STATELESS,
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        if let Some(name) = client {
            meta["io.modelcontextprotocol/clientInfo"] = json!({ "name": name, "version": "1" });
        }
        meta
    }

    #[test]
    fn agents_see_what_a_task_waits_on_and_why_nothing_is_ready() {
        block_on(async {
            let (dir, home, blocker) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let mut call = async |name: &str, args: Value| -> String {
                let params = json!({ "name": name, "arguments": args, "_meta": stateless(None) });
                let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
                let reply = agent.send(message).await.unwrap();
                reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
            };
            let waiting =
                call("create_task", json!({ "title": "Then this", "project": "api", "waits_on": blocker.key() })).await;
            assert_eq!(waiting, "Created api#2 in todo: Then this (waits on #1)");
            assert!(call("list_tasks", json!({ "project": "api" })).await.contains("Then this (waits on #1)"));
            assert!(call("get_task", json!({ "id": "api#2" })).await.contains("status: todo (waits on #1)"));

            call("update_task", json!({ "id": "api#1", "status": "doing" })).await;
            let next = call("next_task", json!({ "project": "api" })).await;
            assert_eq!(next, "Nothing ready in todo for api: 1 task waits on others.");
            let moved = call("update_task", json!({ "id": "api#2", "status": "doing" })).await;
            assert_eq!(moved, "Updated api#2, todo -> doing: Then this (waits on #1)");
            let next = call("next_task", json!({ "project": "api" })).await;
            assert_eq!(next, "Nothing in todo for api.");

            home.store().unwrap().set_property("waiting", false).unwrap();
            assert!(!call("get_task", json!({ "id": "api#2" })).await.contains("waits on"));
        });
    }

    #[test]
    fn a_joined_board_takes_calls_from_a_client_that_never_initialized() {
        block_on(async {
            let (_host_dir, host, task) = board();
            let port = shared(host.clone()).await;
            let guest_dir = tempfile::tempdir().unwrap();
            std::fs::write(guest_dir.path().join(share::FILE), format!("join 127.0.0.1:{port}\n")).unwrap();
            let mut agent = Agent::start(Home::open(guest_dir.path().to_path_buf()).unwrap(), guest_dir.path());

            let reply = agent.send(comment(&task, stateless(Some("claude-code")))).await.unwrap();
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            assert_eq!(authors(&host, &task), ["claude-code"]);
        });
    }

    #[test]
    fn comments_are_signed_with_the_name_a_client_initialized_with() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let client = json!({ "name": "codex-mcp-client", "version": "1" });
            let params = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client });
            agent.send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": params })).await;
            agent.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;

            let meta = json!({ "io.modelcontextprotocol/clientInfo": { "name": "from-meta", "version": "1" } });
            agent.send(comment(&task, meta)).await;
            assert_eq!(authors(&home, &task), ["codex-mcp-client"]);
        });
    }

    #[test]
    fn a_comment_over_http_from_a_client_that_gives_no_name_is_signed_agent() {
        block_on(async {
            let (_dir, home, task) = board();
            let port = shared(home.clone()).await;
            let body = comment(&task, stateless(None)).to_string();
            let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let head = format!(
                "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
                 Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: {STATELESS}\r\n\
                 Mcp-Method: tools/call\r\nMcp-Name: add_comment\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            conn.write_all(format!("{head}{body}").as_bytes()).await.unwrap();
            let mut reply = String::new();
            conn.read_to_string(&mut reply).await.unwrap();

            assert!(reply.contains("Comment added"), "{reply}");
            assert_eq!(authors(&home, &task), ["agent"]);
        });
    }

    /// Whether the call failed, and what it said. `meta` is how the client names itself: `None` for
    /// one that sent `initialize` first.
    async fn call(agent: &mut Agent, meta: Option<Value>, name: &str, args: Value) -> (bool, String) {
        let mut params = json!({ "name": name, "arguments": args });
        if let Some(meta) = meta {
            params["_meta"] = meta;
        }
        let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
        let reply = agent.send(message).await.unwrap();
        (reply["result"]["isError"] == true, reply["result"]["content"][0]["text"].as_str().unwrap().to_string())
    }

    async fn call_as(agent: &mut Agent, client: &str, name: &str, args: Value) -> (bool, String) {
        call(agent, Some(stateless(Some(client))), name, args).await
    }

    fn said(text: &str) -> (bool, String) {
        (false, text.to_string())
    }

    fn refused(reply: &(bool, String)) -> bool {
        reply.0 && reply.1.contains("different request")
    }

    #[test]
    fn a_repeated_request_id_makes_one_task_and_replies_as_the_first_time() {
        block_on(async {
            let (dir, home, _task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let tasks = || home.store().unwrap().list_tasks(None, None).unwrap().len();
            let mut create = async |args: Value| call_as(&mut agent, "claude-code", "create_task", args).await;

            // Without a request_id nothing changes: every call makes a task, and says what it always said.
            let plain = json!({ "title": "Plain", "project": "api" });
            assert_eq!(create(plain.clone()).await, said("Created api#2 in todo: Plain"));
            assert_eq!(create(plain).await, said("Created api#3 in todo: Plain"));
            // A null is no request_id.
            let null = json!({ "title": "Null", "project": "api", "request_id": null });
            assert_eq!(create(null.clone()).await, said("Created api#4 in todo: Null"));
            assert_eq!(create(null).await, said("Created api#5 in todo: Null"));
            assert_eq!(tasks(), 5);

            let once = json!({ "title": "Once", "project": "api", "request_id": "create-1", "waits_on": "api#1" });
            let first = create(once.clone()).await;
            assert_eq!(first, said("Created api#6 in todo: Once (waits on #1)"));
            assert_eq!(create(once.clone()).await, first);
            assert_eq!(create(once.clone()).await, first);
            assert_eq!(tasks(), 6);

            // Not the same request: the title, the project, or anything else it says.
            home.store().unwrap().create_project("web", "", None).unwrap();
            let with = |key: &str, value: Value| {
                let mut changed = once.clone();
                changed[key] = value;
                changed
            };
            let changed = [
                with("title", json!("Twice")),
                with("project", json!("web")),
                with("waits_on", Value::Null),
                with("tags", json!("x")),
                with("human", json!(true)),
                with("status", json!("doing")),
            ];
            for changed in changed {
                let reply = create(changed.clone()).await;
                assert!(refused(&reply), "{changed}: {reply:?}");
            }
            assert_eq!(tasks(), 6);

            // A request_id that isn't valid is an error, not a call that goes unprotected.
            let long = "x".repeat(129);
            for bad in ["", "  ", "a b", "a\nb", "a\u{7f}", long.as_str()] {
                let reply = create(json!({ "title": "Bad", "project": "api", "request_id": bad })).await;
                assert!(reply.0 && reply.1.contains("request_id"), "{bad:?}: {reply:?}");
            }
            assert_eq!(tasks(), 6);
        });
    }

    #[test]
    fn a_request_id_belongs_to_the_client_folder_and_task_it_was_sent_with() {
        block_on(async {
            let (dir, home, task) = board();
            let args = json!({ "id": task.key(), "body": "Done", "request_id": "same" });
            let sent = || home.store().unwrap().list_comments(task.id).unwrap();
            let mut agent = Agent::start(home.clone(), dir.path());
            let first = call_as(&mut agent, "claude-code", "add_comment", args.clone()).await;
            assert_eq!(first, said("Comment added to api#1 as claude-code."));
            assert_eq!(call_as(&mut agent, "claude-code", "add_comment", args.clone()).await, first);
            assert_eq!(sent().len(), 1);

            // The same client in a new process, as after an interruption, in the same folder.
            let mut restarted = Agent::start(home.clone(), dir.path());
            assert_eq!(call_as(&mut restarted, "claude-code", "add_comment", args.clone()).await, first);
            assert_eq!(sent().len(), 1);

            // Another client is another caller, however it names itself.
            let codex = call_as(&mut restarted, "codex", "add_comment", args.clone()).await;
            assert_eq!(codex, said("Comment added to api#1 as codex."));
            let client = json!({ "name": "codex-mcp-client", "version": "1" });
            let params = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client });
            let mut initialized = Agent::start(home.clone(), dir.path());
            initialized.send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": params })).await;
            initialized.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
            let named = call(&mut initialized, None, "add_comment", args.clone()).await;
            assert_eq!(named, said("Comment added to api#1 as codex-mcp-client."));
            assert_eq!(call(&mut initialized, None, "add_comment", args.clone()).await, named);
            assert_eq!(authors(&home, &task), ["claude-code", "codex", "codex-mcp-client"]);

            // Another folder is another caller too.
            let elsewhere = tempfile::tempdir().unwrap();
            let mut other = Agent::start(home.clone(), elsewhere.path());
            let there = call_as(&mut other, "claude-code", "add_comment", args.clone()).await;
            assert_eq!(there, first);
            assert_eq!(sent().len(), 4);

            // The same caller can't change what the id stands for: the comment, the task or the author.
            home.store().unwrap().create_task(1, "Other", "").unwrap();
            let changed = [("api#1", "Different", None), ("api#2", "Done", None), ("api#1", "Done", Some("me"))];
            for (id, body, author) in changed {
                let args = json!({ "id": id, "body": body, "author": author, "request_id": "same" });
                let reply = call_as(&mut agent, "claude-code", "add_comment", args).await;
                assert!(refused(&reply), "{id} {body} {author:?}: {reply:?}");
            }
            assert_eq!(sent().len(), 4);

            let bad = json!({ "id": task.key(), "body": "Done", "request_id": " " });
            let reply = call_as(&mut agent, "claude-code", "add_comment", bad).await;
            assert!(reply.0 && reply.1.contains("request_id"), "{reply:?}");
            assert_eq!(sent().len(), 4);

            // The comment and its task are gone, and the retry says it was done, adding nothing.
            let store = home.store().unwrap();
            store.delete_task(task.id).unwrap();
            assert_eq!(call_as(&mut agent, "claude-code", "add_comment", args.clone()).await, first);
            assert!(sent().is_empty());
            let new = json!({ "id": "api#1", "body": "Done", "request_id": "new" });
            let reply = call_as(&mut agent, "claude-code", "add_comment", new).await;
            assert!(reply.0, "a new request still needs its task: {reply:?}");
        });
    }

    #[test]
    fn a_repeated_next_task_takes_one_task_and_an_empty_answer_stays_empty() {
        block_on(async {
            let (dir, home, ship) = board();
            let store = home.store().unwrap();
            let web = store.create_project("web", "", None).unwrap();
            let mut agent = Agent::start(home.clone(), dir.path());
            let mut next = async |args: Value| call_as(&mut agent, "claude-code", "next_task", args).await;
            let status = |id: i64| store.get_task(id).unwrap().status;

            let empty = json!({ "project": "web", "request_id": "n-1" });
            let first = next(empty.clone()).await;
            assert_eq!(first, said("Nothing in todo for web."));
            let late = store.create_task(web.id, "late", "").unwrap();
            assert_eq!(next(empty.clone()).await, first, "a task that came later isn't taken by the retry");
            assert_eq!(status(late.id), "todo");
            // Not the same request.
            let other = json!({ "project": "web", "request_id": "n-1", "filter": "tag:bug" });
            assert!(refused(&next(other).await));
            assert_eq!(status(late.id), "todo");
            // Without a request_id it takes the task, as ever.
            let took = next(json!({ "project": "web" })).await;
            assert!(took.1.starts_with("Moved to doing:\nweb#2 late"), "{took:?}");

            let args = json!({ "project": "api", "request_id": "n-2" });
            let first = next(args.clone()).await;
            assert!(!first.0 && first.1.starts_with("Moved to doing:\napi#1 Ship it"), "{first:?}");
            let api = store.find_project("api").unwrap();
            let spare = store.create_task(api.id, "spare", "").unwrap();
            // A retry says it was taken earlier, and shows the same task as it is now.
            let again = next(args.clone()).await;
            assert_eq!(again.1.strip_prefix("Already taken earlier:\n"), first.1.strip_prefix("Moved to doing:\n"));
            assert_eq!((status(ship.id), status(spare.id)), ("doing".to_string(), "todo".to_string()));

            // Moved on by someone else and renamed since: still that task, said as it is now, and it
            // doesn't claim the other or move anything.
            let patch =
                TaskPatch { status: Some("review".into()), title: Some("Shipped".into()), ..Default::default() };
            store.update_task(ship.id, patch).unwrap();
            let now = next(args.clone()).await;
            assert!(!now.0 && now.1.starts_with("Already taken earlier:\napi#1 Shipped"), "{now:?}");
            assert!(now.1.contains("status: review") && !now.1.contains("Moved to"), "{now:?}");
            assert_eq!((status(ship.id), status(spare.id)), ("review".to_string(), "todo".to_string()));
            store.delete_task(ship.id).unwrap();
            let gone = next(args).await;
            assert_eq!(gone, said("Already taken earlier: task #1, which has since been deleted."));
            assert_eq!(status(spare.id), "todo");
        });
    }

    #[test]
    fn a_joined_board_runs_a_retried_comment_once_even_from_a_new_session() {
        block_on(async {
            let (_host_dir, host, task) = board();
            let port = shared(host.clone()).await;
            let guest_dir = tempfile::tempdir().unwrap();
            std::fs::write(guest_dir.path().join(share::FILE), format!("join 127.0.0.1:{port}\n")).unwrap();
            let guest = || Agent::start(Home::open(guest_dir.path().to_path_buf()).unwrap(), guest_dir.path());
            let args = json!({ "id": task.key(), "body": "Done", "request_id": "joined-1" });

            let first = call_as(&mut guest(), "claude-code", "add_comment", args.clone()).await;
            assert_eq!(first, said("Comment added to api#1 as claude-code."));
            // A new `kanzo mcp`, so a new session on the host, retrying.
            assert_eq!(call_as(&mut guest(), "claude-code", "add_comment", args.clone()).await, first);
            assert_eq!(authors(&host, &task), ["claude-code"]);
            // Another client of the same computer is its own caller.
            let other = call_as(&mut guest(), "codex", "add_comment", args.clone()).await;
            assert_eq!(other, said("Comment added to api#1 as codex."));
            let changed = json!({ "id": task.key(), "body": "Other", "request_id": "joined-1" });
            assert!(refused(&call_as(&mut guest(), "claude-code", "add_comment", changed).await));
            assert_eq!(authors(&host, &task), ["claude-code", "codex"]);

            // An update retried from a new session answers as the first time, though the task has moved on.
            let update = json!({ "id": task.key(), "status": "doing", "request_id": "joined-2" });
            let first = call_as(&mut guest(), "claude-code", "update_task", update.clone()).await;
            assert_eq!(first, said("Updated api#1, todo -> doing: Ship it"));
            let patch = TaskPatch { status: Some("review".into()), ..Default::default() };
            host.store().unwrap().update_task(task.id, patch).unwrap();
            assert_eq!(call_as(&mut guest(), "claude-code", "update_task", update.clone()).await, first);
            assert_eq!(host.store().unwrap().get_task(task.id).unwrap().status, "review");
            let changed = json!({ "id": task.key(), "status": "done", "request_id": "joined-2" });
            assert!(refused(&call_as(&mut guest(), "claude-code", "update_task", changed).await));
        });
    }

    #[test]
    fn a_repeated_update_task_changes_the_task_once_and_replies_as_the_first_time() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let store = home.store().unwrap();
            let mut update = async |args: Value| call_as(&mut agent, "claude-code", "update_task", args).await;
            let current = || {
                let t = store.get_task(task.id).unwrap();
                (t.status, t.title, t.tags)
            };

            // Without a request_id, or with a null one, it is as it always was.
            assert_eq!(update(json!({ "id": "api#1", "title": "Ship" })).await, said("Updated api#1: Ship"));
            let null = json!({ "id": "api#1", "title": "Ship it", "request_id": null });
            assert_eq!(update(null.clone()).await, said("Updated api#1: Ship it"));
            assert_eq!(update(null).await, said("Updated api#1: Ship it"));

            let args = json!({ "id": "api#1", "status": "doing", "tags": "bug", "request_id": "u-1" });
            let first = update(args.clone()).await;
            assert_eq!(first, said("Updated api#1, todo -> doing: Ship it"));
            assert_eq!(update(args.clone()).await, first);
            assert_eq!(current(), ("doing".to_string(), "Ship it".to_string(), vec!["bug".to_string()]));

            // Someone else changes it; the retry still gets the first answer and changes nothing.
            let patch = TaskPatch { status: Some("review".into()), tags: Some("".into()), ..Default::default() };
            store.update_task(task.id, patch).unwrap();
            let changes = store.changes().unwrap();
            assert_eq!(update(args.clone()).await, first);
            assert_eq!(current(), ("review".to_string(), "Ship it".to_string(), Vec::<String>::new()));
            assert_eq!(store.changes().unwrap(), changes);

            // Not the same request: another status, tags passed empty, another way to name the task, or
            // the same number in another project. None of them is applied.
            store.create_project("web", "", None).unwrap();
            let changes = [("status", json!("done")), ("tags", json!("")), ("id", json!("1")), ("id", json!("web#1"))];
            for (key, value) in changes {
                let mut changed = args.clone();
                changed[key] = value;
                let reply = update(changed.clone()).await;
                assert!(refused(&reply), "{changed}: {reply:?}");
            }
            assert_eq!(current().0, "review");

            // A request_id that isn't valid is an error, and the task isn't touched.
            for bad in ["", " ", "a b", "a\nb"] {
                let reply = update(json!({ "id": "api#1", "status": "done", "request_id": bad })).await;
                assert!(reply.0 && reply.1.contains("request_id"), "{bad:?}: {reply:?}");
            }
            assert_eq!(current().0, "review");

            // A new request can't be applied to a status that doesn't exist: no change, no receipt.
            let wrong = json!({ "id": "api#1", "status": "nope", "request_id": "u-2" });
            assert!(update(wrong.clone()).await.0);
            assert!(update(wrong).await.0);
            let right = json!({ "id": "api#1", "status": "done", "request_id": "u-2" });
            assert_eq!(update(right).await, said("Updated api#1, review -> done: Ship it"));

            // The task is gone: the first request still says what it did, and nothing comes back.
            store.delete_task(task.id).unwrap();
            assert_eq!(update(args).await, first);
            let new = json!({ "id": "api#1", "status": "done", "request_id": "u-3" });
            assert!(update(new).await.0, "a new request still needs its task");
        });
    }

    #[test]
    fn only_the_four_tools_that_change_tasks_take_a_request_id() {
        let tools = Kanzo::tools();
        assert_eq!(tools.map.len(), 10, "no tool was added");
        for route in tools.map.values() {
            let name: &str = &route.attr.name;
            let schema = &route.attr.input_schema;
            let property = schema.get("properties").and_then(|p| p.get("request_id"));
            let takes = ["create_task", "update_task", "add_comment", "next_task"].contains(&name);
            assert_eq!(property.is_some(), takes, "{name}");
            if let Some(property) = property {
                assert_eq!(property["type"], "string", "{name}");
                let required = schema.get("required").and_then(Value::as_array);
                assert!(required.is_none_or(|r| !r.contains(&json!("request_id"))), "{name}: optional");
            }
        }
    }
}
