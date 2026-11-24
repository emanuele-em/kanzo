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
             finish or get stuck, url for a PR you opened. Move to review with update_task's comment saying how \
             to resume this session, and escalated if you need the user. Leave done and human tasks to the user. \
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

/// What a comment from this client is signed with, when it has named itself.
fn client_name(context: &RequestContext<RoleServer>) -> Option<String> {
    caller(context).map(|i| i.name).filter(|n| !n.is_empty())
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
struct GetTaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    /// Also show its version, for update_task's if_version.
    include_version: Option<bool>,
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
    /// New for each call; resend it only to retry that call, which then runs once.
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
    /// Slug to move it to.
    project: Option<String>,
    /// Comma-separated; "" clears.
    tags: Option<String>,
    /// For the user, not for agents.
    human: Option<bool>,
    /// A link; "" clears.
    url: Option<String>,
    /// Tasks it waits on, like api#12; "" clears.
    waits_on: Option<String>,
    /// Saved with the change, or not at all; up to 16 KiB. To hand off: Completed, Remaining, Verified, Next step.
    comment: Option<String>,
    /// Asks the user to look; the task's next move clears it.
    escalated: Option<bool>,
    /// A version from get_task; refuses the change if the task changed since.
    if_version: Option<String>,
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
    /// As in list_tasks, like tag:bug.
    filter: Option<String>,
    /// New for each call; resend it only to retry that call, which then runs once.
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
        Parameters(args): Parameters<GetTaskArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                if args.include_version != Some(true) {
                    let task = args.id.find(store)?;
                    return task_detail(ctx, store, &task, None);
                }
                // The task, its comments and its version from one moment.
                store.read(|| {
                    let task = args.id.find(store)?;
                    let version = store.task_version(task.id)?;
                    task_detail(ctx, store, &task, Some(&version))
                })
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
                let CreateTaskArgs { title, description, project, tags, status, human, waits_on, request_id } = args;
                let project = ctx.project(store, project.as_deref())?;
                let status = status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                let patch = TaskPatch { description, status, tags, human, waits_on, ..Default::default() };
                let (t, again) = store.create_task_once(project.id, &title, patch, request_id.as_deref())?;
                let done = if again { "Already created earlier:" } else { "Created" };
                Ok(format!("{done} {} in {}: {}{}", t.key(), t.status, t.title, waits_note(store, &t)?))
            })
            .await)
    }

    #[tool(description = "Change the fields you pass.")]
    async fn update_task(
        &self,
        Parameters(args): Parameters<UpdateTaskArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = client_name(&context).unwrap_or_else(|| "agent".into());
        Ok(self
            .run(move |_, store| {
                let status = args.status.map(|s| s.trim().to_ascii_lowercase());
                let project = args.project.as_deref().map(str::trim).filter(|p| !p.is_empty());
                let project_id = project.map(|p| store.find_project(p)).transpose()?.map(|p| p.id);
                // A comment that is present but empty or too long is refused, with the rest of the update.
                let comment = args.comment.as_deref().map(|body| store::NewComment { author: &author, body });
                let noted = comment.is_some();
                let patch = TaskPatch {
                    title: args.title,
                    description: args.description,
                    status,
                    tags: args.tags,
                    project_id,
                    human: args.human,
                    url: args.url,
                    waits_on: args.waits_on,
                    escalated: args.escalated,
                };
                // The key, the guard, the edit, the comment and the reply are one write, so the reply, with
                // its new version, tells what that edit saw, whatever others write next.
                store.update_task_as(
                    || args.id.find(store),
                    patch,
                    comment,
                    args.if_version.as_deref(),
                    |before, after, version| {
                        let was = if before.project != after.project {
                            format!("{} -> ", before.key())
                        } else {
                            String::new()
                        };
                        let moved = if before.status != after.status {
                            format!(", {} -> {}", before.status, after.status)
                        } else {
                            String::new()
                        };
                        let waits = waits_note(store, &after)?;
                        let mut out = format!("Updated {was}{}{moved}: {}{waits}", after.key(), after.title);
                        if noted {
                            let _ = write!(out, "\nComment added as {author}.");
                        }
                        // Only a guarded edit gets its new version back, to chain the next one.
                        if let Some(version) = version {
                            let _ = write!(out, "\nversion: {version}");
                        }
                        Ok(out)
                    },
                )
            })
            .await)
    }

    #[tool(description = "Comment on a task, like to report progress.")]
    async fn add_comment(
        &self,
        Parameters(args): Parameters<AddCommentArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&context);
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
                let id = project.as_ref().map(|p| p.id);
                let filter = args.filter.as_deref().unwrap_or("");
                match store.next_task_once(id, filter, args.request_id.as_deref())? {
                    // A retry shows the task the first call took, as it is now.
                    Some((task, true)) => {
                        Ok(format!("Already taken earlier:\n{}", task_detail(ctx, store, &task, None)?))
                    }
                    Some((task, false)) => {
                        Ok(format!("Moved to {}:\n{}", task.status, task_detail(ctx, store, &task, None)?))
                    }
                    None => {
                        let waiting = store.waiting_in_todo(id, filter)?;
                        let scope = project.map_or("any project".into(), |p| p.slug);
                        Ok(match waiting {
                            0 => format!("Nothing in todo for {scope}."),
                            1 => format!("Nothing ready in todo for {scope}: 1 task waits on others."),
                            n => format!("Nothing ready in todo for {scope}: {n} tasks wait on others."),
                        })
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

/// With a `version`, a line for update_task's `if_version`; without one, the reply is as it always was.
fn task_detail(ctx: &Ctx, store: &Store, t: &Task, version: Option<&str>) -> store::Result<String> {
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
    if t.escalated {
        out.push_str(", escalated to the user");
    }
    if shown(&hidden, "created") {
        let _ = write!(out, ", created {created}");
    }
    if updated != created && shown(&hidden, "updated") {
        let _ = write!(out, ", updated {updated}");
    }
    if let Some(version) = version {
        let _ = write!(out, "\nversion: {version}");
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

    fn call_message(name: &str, args: Value, meta: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": args, "_meta": meta } })
    }

    fn comment(task: &Task, meta: Value) -> Value {
        call_message("add_comment", json!({ "id": task.key(), "body": "done" }), meta)
    }

    fn text(reply: &Value) -> &str {
        reply["result"]["content"][0]["text"].as_str().unwrap()
    }

    const HANDOFF: &str = "Completed: the API\nRemaining: the tests\nVerified: cargo check\nNext step: run cargo test";

    /// A `tools/call` over HTTP as a client that never opened a session sends it; the raw reply.
    async fn post(port: u16, tool: &str, message: Value) -> String {
        let body = message.to_string();
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let head = format!(
            "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: {STATELESS}\r\n\
             Mcp-Method: tools/call\r\nMcp-Name: {tool}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        conn.write_all(format!("{head}{body}").as_bytes()).await.unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.unwrap();
        reply
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
    fn agents_move_a_task_to_another_project() {
        block_on(async {
            let (dir, home, task) = board();
            home.store().unwrap().create_project("web", "", None).unwrap();
            let mut agent = Agent::start(home.clone(), dir.path());
            let mut call = async |name: &str, args: Value| -> String {
                let params = json!({ "name": name, "arguments": args, "_meta": stateless(None) });
                let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
                let result = &agent.send(message).await.unwrap()["result"];
                let text = result["content"][0]["text"].as_str().unwrap();
                if result["isError"] == true { format!("Error: {text}") } else { text.to_string() }
            };

            let moved = call("update_task", json!({ "id": task.key(), "project": "web", "status": "doing" })).await;
            assert_eq!(moved, "Updated api#1 -> web#1, todo -> doing: Ship it");
            let saved = home.store().unwrap().get_task(task.id).unwrap();
            assert_eq!((saved.key(), saved.status.as_str()), ("web#1".into(), "doing"));
            assert_eq!(call("get_task", json!({ "id": "api#1" })).await, "Error: no task api#1 (#1 is web#1)");

            // An unknown project refuses the whole update, and says which projects there are.
            let args = json!({ "id": "web#1", "project": "nope", "status": "review", "title": "Renamed" });
            assert_eq!(call("update_task", args).await, r#"Error: no project "nope" (projects: api, web)"#);
            let kept = home.store().unwrap().get_task(task.id).unwrap();
            assert_eq!((kept.key(), kept.status.as_str(), kept.title.as_str()), ("web#1".into(), "doing", "Ship it"));

            // Blank, or the project it's already in, changes nothing and says nothing extra.
            assert_eq!(call("update_task", json!({ "id": "web#1", "project": " " })).await, "Updated web#1: Ship it");
            assert_eq!(call("update_task", json!({ "id": "web#1", "project": "WEB" })).await, "Updated web#1: Ship it");
        });
    }

    #[test]
    fn only_get_task_and_update_task_offer_the_version_guard() {
        let router = Kanzo::tools();
        let props = |tool: &str| -> Vec<String> {
            let route = router.map.values().find(|r| r.attr.name == tool).unwrap();
            route.attr.input_schema["properties"].as_object().unwrap().keys().cloned().collect()
        };
        assert!(props("get_task").contains(&"include_version".to_string()));
        assert!(props("update_task").contains(&"if_version".to_string()));
        for tool in ["delete_task", "add_comment", "create_task", "next_task", "list_tasks"] {
            assert!(!props(tool).iter().any(|p| p.contains("version")), "{tool}");
        }
        assert_eq!(props("delete_task"), ["id"]);
    }

    #[test]
    fn a_stale_guarded_update_is_refused_and_an_unguarded_one_is_as_before() {
        block_on(async {
            let (dir, home, _) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let mut call = async |name: &str, args: Value| -> (bool, String) {
                let params = json!({ "name": name, "arguments": args, "_meta": stateless(None) });
                let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
                let reply = agent.send(message).await.unwrap();
                let text = reply["result"]["content"][0]["text"].as_str().unwrap().to_string();
                (reply["result"]["isError"].as_bool().unwrap(), text)
            };

            // Asking for no version, or not asking, gives the reply there always was.
            let (_, plain) = call("get_task", json!({ "id": "api#1" })).await;
            let (_, off) = call("get_task", json!({ "id": "api#1", "include_version": false })).await;
            assert_eq!(off, plain);
            assert!(!plain.contains("version"), "{plain}");
            let (_, shown) = call("get_task", json!({ "id": "api#1", "include_version": true })).await;
            let version = shown.lines().find_map(|l| l.strip_prefix("version: ")).unwrap().to_string();
            let rest: Vec<&str> = shown.lines().filter(|l| !l.starts_with("version: ")).collect();
            assert_eq!(rest.join("\n"), plain, "the version is one added line");

            let args = |title: &str, version: &str| json!({ "id": "api#1", "title": title, "if_version": version });
            let (error, text) = call("update_task", args("New", &version)).await;
            assert!(!error, "{text}");
            let next = text.strip_prefix("Updated api#1: New\nversion: ").expect(&text).to_string();
            assert_ne!(next, version);
            assert_eq!(home.store().unwrap().task_version(1).unwrap(), next);

            // The first read is stale now: refused, in a few words, and nothing moves.
            let changes = home.store().unwrap().changes().unwrap();
            let (error, text) = call("update_task", args("Stale", &version)).await;
            assert!(error && text.contains("api#1 changed since you read it"), "{text}");
            assert!(text.len() < 200, "{text}");
            let (error, text) = call("update_task", args("Bogus", "bogus")).await;
            assert!(error && text.contains("not a version"), "{text}");
            let wrong = json!({ "id": "web#1", "title": "Wrong", "if_version": next });
            let (error, text) = call("update_task", wrong).await;
            assert!(error && text.contains("no task web#1"), "{text}");
            let store = home.store().unwrap();
            assert_eq!((store.get_task(1).unwrap().title.as_str(), store.changes().unwrap()), ("New", changes));

            // Without if_version the edit goes through whatever the version, and says nothing more.
            let (error, text) = call("update_task", json!({ "id": "api#1", "title": "Plain" })).await;
            assert_eq!((error, text.as_str()), (false, "Updated api#1: Plain"));
            // The guarded retry with the version it did get back is stale too: the plain edit counted.
            let (error, _) = call("update_task", args("Late", &next)).await;
            assert!(error);
            assert_eq!(home.store().unwrap().get_task(1).unwrap().title, "Plain");
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
            let reply = post(port, "add_comment", comment(&task, stateless(None))).await;

            assert!(reply.contains("Comment added"), "{reply}");
            assert_eq!(authors(&home, &task), ["agent"]);
        });
    }

    #[test]
    fn a_handoff_saves_status_escalation_and_comment_in_one_call() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let args = json!({ "id": task.key(), "status": "review", "escalated": true, "comment": HANDOFF });
            let reply = agent.send(call_message("update_task", args, stateless(Some("claude-code")))).await.unwrap();
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            assert_eq!(text(&reply), "Updated api#1, todo -> review: Ship it\nComment added as claude-code.");

            // What a restarted board reads back, and an agent that asks.
            let store = home.store().unwrap();
            let saved = store.get_task(task.id).unwrap();
            assert_eq!(
                (saved.status.as_str(), saved.escalated, saved.human, saved.comments),
                ("review", true, false, 1)
            );
            let comments = store.list_comments(task.id).unwrap();
            assert_eq!((comments[0].author.as_str(), comments[0].body.as_str()), ("claude-code", HANDOFF));
            let get = call_message("get_task", json!({ "id": task.key() }), stateless(None));
            let shown = agent.send(get).await.unwrap();
            assert!(text(&shown).contains("status: review, escalated to the user"), "{shown}");
            assert!(text(&shown).contains("Next step: run cargo test"), "{shown}");

            // The user drags it back to todo for more work: the escalation ends, and an agent takes it.
            store.move_task(task.id, "todo", None).unwrap();
            let next = call_message("next_task", json!({ "project": "api" }), stateless(None));
            let taken = agent.send(next).await.unwrap();
            assert!(text(&taken).starts_with("Moved to doing:\napi#1 Ship it"), "{taken}");
            assert!(!text(&taken).contains("escalated"), "{taken}");
            assert_eq!(authors(&home, &task), ["claude-code"]);
        });
    }

    #[test]
    fn an_update_without_a_comment_replies_and_saves_as_before() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let args = json!({ "id": task.key(), "status": "doing" });
            let reply = agent.send(call_message("update_task", args, stateless(None))).await.unwrap();
            assert_eq!(text(&reply), "Updated api#1, todo -> doing: Ship it");

            // A null comment is no comment.
            let args = json!({ "id": task.key(), "status": "review", "comment": null });
            let reply = agent.send(call_message("update_task", args, stateless(None))).await.unwrap();
            assert_eq!(text(&reply), "Updated api#1, doing -> review: Ship it");
            assert!(authors(&home, &task).is_empty());
        });
    }

    #[test]
    fn a_comment_that_is_empty_or_too_long_refuses_the_whole_update() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let before = home.store().unwrap().changes().unwrap();
            // 8193 characters but 16386 bytes.
            let wide = "é".repeat(8193);
            for comment in ["", "  \n ", wide.as_str()] {
                let args = json!({ "id": task.key(), "status": "review", "escalated": true, "comment": comment });
                let reply = agent.send(call_message("update_task", args, stateless(None))).await.unwrap();
                assert_eq!(reply["result"]["isError"], true, "{reply}");
                assert!(text(&reply).starts_with("comment is "), "{reply}");

                let store = home.store().unwrap();
                let saved = store.get_task(task.id).unwrap();
                assert_eq!((saved.status.as_str(), saved.escalated), ("todo", false));
                assert!(store.list_comments(task.id).unwrap().is_empty());
                assert_eq!(store.changes().unwrap(), before, "not even the change counter moved");
            }
        });
    }

    #[test]
    fn a_handoff_is_signed_with_the_name_a_client_initialized_with() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let client = json!({ "name": "codex-mcp-client", "version": "1" });
            let params = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client });
            agent.send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": params })).await;
            agent.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;

            let meta = json!({ "io.modelcontextprotocol/clientInfo": { "name": "from-meta", "version": "1" } });
            let args = json!({ "id": task.key(), "comment": HANDOFF });
            agent.send(call_message("update_task", args, meta)).await;
            assert_eq!(authors(&home, &task), ["codex-mcp-client"]);
        });
    }

    #[test]
    fn a_handoff_over_http_from_a_client_that_gives_no_name_is_signed_agent() {
        block_on(async {
            let (_dir, home, task) = board();
            let port = shared(home.clone()).await;
            let args = json!({ "id": task.key(), "status": "review", "comment": HANDOFF });
            let reply = post(port, "update_task", call_message("update_task", args, stateless(None))).await;

            assert!(reply.contains("Comment added as agent."), "{reply}");
            assert_eq!(authors(&home, &task), ["agent"]);
            assert_eq!(home.store().unwrap().get_task(task.id).unwrap().status, "review");
        });
    }

    #[test]
    fn a_joined_board_signs_a_handoff_with_the_clients_name() {
        block_on(async {
            let (_host_dir, host, task) = board();
            let port = shared(host.clone()).await;
            let guest_dir = tempfile::tempdir().unwrap();
            std::fs::write(guest_dir.path().join(share::FILE), format!("join 127.0.0.1:{port}\n")).unwrap();
            let mut agent = Agent::start(Home::open(guest_dir.path().to_path_buf()).unwrap(), guest_dir.path());

            let args = json!({ "id": task.key(), "status": "review", "escalated": true, "comment": HANDOFF });
            let reply = agent.send(call_message("update_task", args, stateless(Some("claude-code")))).await.unwrap();
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            assert_eq!(authors(&host, &task), ["claude-code"]);
            let saved = host.store().unwrap().get_task(task.id).unwrap();
            assert_eq!((saved.status.as_str(), saved.escalated), ("review", true));
        });
    }

    #[test]
    fn a_handoff_keeps_hidden_properties_hidden() {
        block_on(async {
            let (dir, home, blocker) = board();
            let store = home.store().unwrap();
            let project = store.find_project("api").unwrap();
            let waits = TaskPatch { waits_on: Some(blocker.key()), ..Default::default() };
            let task = store.create_task_in(project.id, "Then this", waits).unwrap();
            for property in ["waiting", "comments"] {
                store.set_property(property, false).unwrap();
            }
            let mut agent = Agent::start(home.clone(), dir.path());

            let args = json!({ "id": task.key(), "status": "review", "escalated": true, "comment": HANDOFF });
            let reply = agent.send(call_message("update_task", args, stateless(None))).await.unwrap();
            assert_eq!(text(&reply), "Updated api#2, todo -> review: Then this\nComment added as agent.");
            let get = call_message("get_task", json!({ "id": task.key() }), stateless(None));
            let shown = agent.send(get).await.unwrap();
            let shown = text(&shown);
            assert!(!shown.contains("Next step") && !shown.contains("Comments ("), "{shown}");
            assert!(!shown.contains("waits on") && shown.contains("escalated to the user"), "{shown}");
            assert_eq!(store.list_comments(task.id).unwrap().len(), 1, "hidden, not dropped");
        });
    }

    #[test]
    fn a_guarded_handoff_saves_its_comment_only_while_the_version_holds() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let read = home.store().unwrap().task_version(task.id).unwrap();
            let args = json!({ "id": task.key(), "status": "review", "comment": HANDOFF, "if_version": read });
            let reply = agent.send(call_message("update_task", args.clone(), stateless(None))).await.unwrap();
            let next = home.store().unwrap().task_version(task.id).unwrap();
            let expected = format!("Updated api#1, todo -> review: Ship it\nComment added as agent.\nversion: {next}");
            assert_eq!(text(&reply), expected);

            // Sent again on the same read, it is stale: no second comment.
            let reply = agent.send(call_message("update_task", args, stateless(None))).await.unwrap();
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            assert!(text(&reply).contains("api#1 changed since you read it"), "{reply}");
            assert_eq!(authors(&home, &task), ["agent"]);
        });
    }

    #[test]
    fn update_task_takes_an_optional_comment_and_escalation_and_the_tool_list_keeps_ten_tools() {
        let router = Kanzo::tools();
        assert_eq!(router.map.len(), 10);
        let route = router.map.values().find(|r| r.attr.name == "update_task").unwrap();
        let schema = &route.attr.input_schema;
        assert_eq!(schema["properties"]["comment"]["type"], "string");
        assert_eq!(schema["properties"]["escalated"]["type"], "boolean");
        let required = schema["required"].as_array().unwrap();
        assert!(!required.iter().any(|r| r == "comment" || r == "escalated"));
    }

    #[test]
    fn create_task_and_next_task_run_once_per_request_id() {
        block_on(async {
            let (dir, home, _task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let create = json!({ "title": "Once", "project": "api", "request_id": "c-1" });
            let reply = agent.send(call_message("create_task", create.clone(), stateless(None))).await.unwrap();
            assert_eq!(text(&reply), "Created api#2 in todo: Once");
            let reply = agent.send(call_message("create_task", create, stateless(None))).await.unwrap();
            assert_eq!(text(&reply), "Already created earlier: api#2 in todo: Once");

            let next = json!({ "project": "api", "request_id": "n-1" });
            let reply = agent.send(call_message("next_task", next.clone(), stateless(None))).await.unwrap();
            assert!(text(&reply).starts_with("Moved to doing:\napi#1 Ship it\n"), "{reply}");
            let reply = agent.send(call_message("next_task", next, stateless(None))).await.unwrap();
            assert!(text(&reply).starts_with("Already taken earlier:\napi#1 Ship it\nstatus: doing"), "{reply}");
            assert_eq!(home.store().unwrap().get_task(2).unwrap().status, "todo", "the retry took no other task");

            let reused = json!({ "title": "Other", "project": "api", "request_id": "c-1" });
            let reply = agent.send(call_message("create_task", reused, stateless(None))).await.unwrap();
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            assert!(text(&reply).contains("send a new one with every call"), "{reply}");
        });
    }

    #[test]
    fn a_joined_board_passes_request_ids_to_its_host() {
        block_on(async {
            let (_host_dir, host, _task) = board();
            let port = shared(host.clone()).await;
            let guest_dir = tempfile::tempdir().unwrap();
            std::fs::write(guest_dir.path().join(share::FILE), format!("join 127.0.0.1:{port}\n")).unwrap();
            let mut agent = Agent::start(Home::open(guest_dir.path().to_path_buf()).unwrap(), guest_dir.path());
            let create = json!({ "title": "Once", "project": "api", "request_id": "joined-1" });
            for said in ["Created api#2 in todo: Once", "Already created earlier: api#2 in todo: Once"] {
                let reply = agent.send(call_message("create_task", create.clone(), stateless(None))).await.unwrap();
                assert_eq!(text(&reply), said);
            }
            assert_eq!(host.store().unwrap().list_tasks(None, None).unwrap().len(), 2);
        });
    }

    #[test]
    fn only_create_task_and_next_task_take_a_request_id() {
        for route in Kanzo::tools().map.values() {
            let name: &str = route.attr.name.as_ref();
            let id = &route.attr.input_schema["properties"]["request_id"];
            assert_eq!(!id.is_null(), ["create_task", "next_task"].contains(&name), "{name}");
            if !id.is_null() {
                assert!(id["description"].as_str().unwrap().starts_with("New for each call"), "{name}");
            }
        }
    }
}
