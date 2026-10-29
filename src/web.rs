//! `kanzo`: the board page, the small JSON API behind it, and MCP over HTTP at `/mcp`.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::store::{self, Error, ImagesOf, Store, TaskPatch};
use crate::{sync, update};

const INDEX: &str = include_str!("index.html");
/// The largest image the board takes from a paste.
const IMAGE_LIMIT: usize = 25 * 1024 * 1024;
const APP_JS: &str = include_str!("app.js");
const LOGO: &[u8] = include_bytes!("logo.png");

/// Scripts only from this server, so even a bug in the Markdown renderer cannot run
/// injected script; requests only back to this server.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                   connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub async fn serve(home: Home, port: Option<u16>, listen: Option<IpAddr>, open: bool) -> Result<()> {
    let port = port.unwrap_or(home.config.port);
    let listen = listen.unwrap_or(home.config.listen);
    let url = format!("http://localhost:{port}");
    let listener = match tokio::net::TcpListener::bind(SocketAddr::new(listen, port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if is_kanzo(port).await {
                println!("Kanzo is already running at {url}");
                if open {
                    open_browser(&url);
                }
                return Ok(());
            }
            bail!("port {port} is used by another program; choose another with --port or in config.toml");
        }
        Err(e) => return Err(e).context(format!("cannot listen on {listen} port {port}")),
    };
    let guard = Guard::new(port, !listen.is_loopback());
    println!("Kanzo board at {url}, data in {}", home.dir.display());
    if guard.lan {
        if let Some(name) = guard.names.iter().find(|n| n.ends_with(".local")) {
            println!("On your network: http://{name}:{port} (or this machine's IP address)");
        }
        println!("Anyone who can reach port {port} can read and change this board.");
    }
    if home.config.mcp_http {
        println!("MCP over HTTP at {url}/mcp");
    }
    if open {
        open_browser(&url);
    }
    // Cancelling this also ends open MCP streams, which would otherwise hold up shutdown.
    let shutdown = CancellationToken::new();
    let app = router(home, guard, shutdown.clone());
    let serve = |listener| {
        let stop = shutdown.clone();
        axum::serve(listener, app.clone()).with_graceful_shutdown(async move { stop.cancelled().await }).into_future()
    };
    // `localhost` is ::1 before 127.0.0.1 on many Linux systems, and not every
    // client (busybox wget, older Node) falls back, so listen on ::1 as well.
    let v6 = match listen {
        IpAddr::V4(ip) if ip.is_loopback() || ip.is_unspecified() => {
            tokio::net::TcpListener::bind(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)).await.ok()
        }
        _ => None,
    };
    let stopper = shutdown.clone();
    tokio::spawn(async move {
        stop_signal().await;
        stopper.cancel();
    });
    match v6 {
        Some(v6) => {
            let (a, b) = tokio::join!(serve(listener), serve(v6));
            a.and(b)?;
        }
        None => serve(listener).await?,
    }
    Ok(())
}

/// Ctrl-C, or SIGTERM from launchd or systemd stopping the service.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn router(home: Home, guard: Guard, shutdown: CancellationToken) -> Router {
    let mut app = Router::new();
    if home.config.mcp_http {
        app = app.nest_service("/mcp", crate::mcp::http_service(home.clone(), shutdown));
    }
    app.route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        .route("/logo.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], LOGO) }))
        .route("/images/{file}", get(image_file))
        .route("/api/ping", get(|| async { "kanzo" }))
        .route("/api/changes", get(changes))
        .route("/api/board", get(board))
        .route("/api/search", get(search))
        .route("/api/projects", post(create_project))
        .route("/api/projects/{slug}", patch(update_project).delete(delete_project))
        .route("/api/projects/{slug}/repos", post(link_repo).delete(unlink_repo))
        .route("/api/tasks", post(create_task))
        .route("/api/tasks/{id}", get(get_task).patch(update_task).delete(delete_task))
        .route("/api/tasks/{id}/move", post(move_task))
        .route("/api/tasks/{id}/comments", post(add_comment))
        .route("/api/comments/{id}", delete(delete_comment))
        .route("/api/images", post(save_image).layer(DefaultBodyLimit::max(IMAGE_LIMIT)))
        .route("/api/images/{id}", delete(delete_image))
        .route("/api/tasks/{id}/images", post(attach_image))
        .route("/api/update", get(update_status).post(update_now))
        .route("/api/sync", get(sync_status).post(sync_now).put(sync_repo))
        .route("/api/settings", patch(update_settings))
        .layer(middleware::from_fn_with_state(Arc::new(guard), protect))
        .with_state(Arc::new(home))
}

/// Which Host headers the board answers to.
///
/// DNS rebinding needs a domain name the attacker controls, so the board only
/// accepts `localhost`, IP addresses and, when it listens on the network, this
/// machine's own names. It never accepts other domain names.
struct Guard {
    port: u16,
    lan: bool,
    /// This machine's names, like `pi` and `pi.local`; filled only when `lan`.
    names: Vec<String>,
}

impl Guard {
    fn new(port: u16, lan: bool) -> Self {
        let mut names = Vec::new();
        if lan {
            let run = |cmd: &str, args: &[&str]| {
                std::process::Command::new(cmd)
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_ascii_lowercase())
            };
            // macOS keeps its Bonjour name apart; `hostname` there can be a DHCP address.
            let found = [run("scutil", &["--get", "LocalHostName"]), run("hostname", &[])];
            for full in found.into_iter().flatten().filter(|n| !n.is_empty() && n.parse::<IpAddr>().is_err()) {
                let short = full.split('.').next().unwrap_or_default().to_string();
                for name in [format!("{short}.local"), full, short] {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        Self { port, lan, names }
    }

    fn allows(&self, host: &str) -> bool {
        let (name, port) = match host.rsplit_once(':') {
            Some((n, p)) if !p.contains(']') => (n, Some(p)),
            _ => (host, None),
        };
        if port.is_some_and(|p| p.parse() != Ok(self.port)) {
            return false;
        }
        let name = name.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
        let ip = name.parse::<IpAddr>().ok();
        if name.eq_ignore_ascii_case("localhost") || ip.is_some_and(|ip| ip.is_loopback()) {
            return true;
        }
        self.lan && (ip.is_some() || self.names.iter().any(|n| n.eq_ignore_ascii_case(name)))
    }
}

/// Refuses requests for a Host this board does not answer to, and browser
/// requests sent from any page other than the board itself. Adds security
/// headers to every reply.
async fn protect(State(guard): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let ok = {
        let header = |name| req.headers().get(name).and_then(|h: &HeaderValue| h.to_str().ok());
        let host = header(header::HOST).unwrap_or("");
        // The Origin a page from this board sends: http:// and the same host.
        let same_origin = |o: &str| {
            o.len() == host.len() + 7
                && o.as_bytes()[..7].eq_ignore_ascii_case(b"http://")
                && o.as_bytes()[7..].eq_ignore_ascii_case(host.as_bytes())
        };
        guard.allows(host) && header(header::ORIGIN).is_none_or(same_origin)
    };
    let mut res = if ok {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "kanzo does not answer requests from other sites").into_response()
    };
    let headers = res.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-cache"));
    res
}

type Shared = State<Arc<Home>>;

/// Runs `f` with a fresh connection on a blocking thread and answers with JSON.
async fn with_store<T: Serialize + Send + 'static>(
    home: Arc<Home>,
    f: impl FnOnce(&Home, &Store) -> store::Result<T> + Send + 'static,
) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let store = home.store()?;
        f(&home, &store)
    })
    .await;
    let error = match outcome {
        Ok(Ok(v)) => return Json(v).into_response(),
        Ok(Err(e)) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };
    let code = match error {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
        Error::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, Json(json!({ "error": error.to_string() }))).into_response()
}

/// What the page compares to see if anything changed: the change counter, plus
/// the database file's identity so a board pulled by `kanzo sync` always reloads.
fn change_token(home: &Home, s: &Store) -> store::Result<String> {
    Ok(format!("{}-{}", s.changes()?, home.db_identity()))
}

#[derive(Serialize)]
struct Changes {
    changes: String,
}

async fn changes(State(home): Shared) -> Response {
    with_store(home, |h, s| Ok(Changes { changes: change_token(h, s)? })).await
}

/// Done tasks the board gets unless it asks for all: the newest, which are all it shows.
/// The done column only grows, and every change reloads the board.
const DONE_SHOWN: usize = 50;

#[derive(Deserialize)]
struct BoardQuery {
    project: Option<String>,
    /// "all" for every done task.
    done: Option<String>,
}

/// The whole board, serialized straight from the store's rows.
#[derive(Serialize)]
struct Board {
    changes: String,
    done_hidden: usize,
    projects: Vec<store::Project>,
    statuses: Vec<String>,
    tasks: Vec<store::Task>,
    version: &'static str,
}

async fn board(State(home): Shared, Query(q): Query<BoardQuery>) -> Response {
    with_store(home, move |h, s| {
        let project = match q.project.as_deref().filter(|p| !p.is_empty() && *p != "all") {
            Some(slug) => Some(s.find_project(slug)?),
            None => None,
        };
        let keep_done = (q.done.as_deref() != Some("all")).then_some(DONE_SHOWN);
        let (tasks, done_hidden) = s.list_tasks_keeping_done(project.map(|p| p.id), None, keep_done)?;
        Ok(Board {
            changes: change_token(h, s)?,
            done_hidden,
            projects: s.list_projects()?,
            statuses: s.statuses().to_vec(),
            tasks,
            version: update::CURRENT,
        })
    })
    .await
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

/// Tasks of every project matching the search, for the search window.
async fn search(State(home): Shared, Query(q): Query<SearchQuery>) -> Response {
    with_store(home, move |_, s| s.search(&q.q, 30)).await
}

#[derive(Deserialize)]
struct NewProject {
    slug: String,
    #[serde(default)]
    name: String,
    repo: Option<String>,
}

async fn create_project(State(home): Shared, Json(p): Json<NewProject>) -> Response {
    with_store(home, move |_, s| s.create_project(&p.slug, &p.name, p.repo.as_deref())).await
}

#[derive(Deserialize)]
struct ProjectPatch {
    name: Option<String>,
    color: Option<String>,
}

async fn update_project(State(home): Shared, Path(slug): Path<String>, Json(p): Json<ProjectPatch>) -> Response {
    with_store(home, move |_, s| {
        let mut project = None;
        if let Some(name) = &p.name {
            project = Some(s.rename_project(&slug, name)?);
        }
        if let Some(color) = &p.color {
            project = Some(s.set_project_color(&slug, color)?);
        }
        project.map_or_else(|| s.find_project(&slug), Ok)
    })
    .await
}

#[derive(Deserialize)]
struct RepoLink {
    repo: String,
}

async fn link_repo(State(home): Shared, Path(slug): Path<String>, Json(r): Json<RepoLink>) -> Response {
    with_store(home, move |_, s| s.link_repo(&slug, &r.repo)).await
}

async fn unlink_repo(State(home): Shared, Path(slug): Path<String>, Json(r): Json<RepoLink>) -> Response {
    with_store(home, move |_, s| s.unlink_repo(&slug, &r.repo)).await
}

async fn delete_project(State(home): Shared, Path(slug): Path<String>) -> Response {
    with_store(home, move |h, s| {
        let project = s.find_project(&slug)?;
        h.deleting_images(s, ImagesOf::Project(project.id), || s.delete_project(&slug))
    })
    .await
}

#[derive(Deserialize)]
struct NewTask {
    project: String,
    title: String,
    #[serde(default)]
    description: String,
}

async fn create_task(State(home): Shared, Json(t): Json<NewTask>) -> Response {
    with_store(home, move |_, s| {
        let project = s.find_project(&t.project)?;
        s.create_task_in(project.id, None, &t.title, &t.description)
    })
    .await
}

/// Board settings, kept in kanzo.db so they sync with the board.
#[derive(Deserialize)]
struct Settings {
    backlog: bool,
}

async fn update_settings(State(home): Shared, Json(r): Json<Settings>) -> Response {
    with_store(home, move |_, s| s.set_backlog(r.backlog)).await
}

#[derive(Serialize)]
struct TaskReply {
    comments: Vec<store::Comment>,
    images: Vec<store::Image>,
    task: store::Task,
}

async fn get_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| {
        Ok(TaskReply { comments: s.list_comments(id)?, images: s.list_images(id)?, task: s.get_task(id)? })
    })
    .await
}

#[derive(Deserialize)]
struct Patch {
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    project: Option<String>,
    tags: Option<String>,
    human: Option<bool>,
}

async fn update_task(State(home): Shared, Path(id): Path<i64>, Json(p): Json<Patch>) -> Response {
    with_store(home, move |_, s| {
        let project_id = p.project.map(|slug| s.find_project(&slug)).transpose()?.map(|p| p.id);
        s.update_task(
            id,
            TaskPatch {
                title: p.title,
                description: p.description,
                status: p.status,
                project_id,
                tags: p.tags,
                human: p.human,
            },
        )
    })
    .await
}

#[derive(Deserialize)]
struct Move {
    status: String,
    before: Option<i64>,
}

async fn move_task(State(home): Shared, Path(id): Path<i64>, Json(m): Json<Move>) -> Response {
    with_store(home, move |_, s| s.move_task(id, &m.status, m.before)).await
}

async fn delete_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Task(id), || s.delete_task(id))).await
}

#[derive(Deserialize)]
struct NewComment {
    #[serde(default)]
    body: String,
    /// Files saved by `/api/images`, attached to the comment.
    #[serde(default)]
    images: Vec<String>,
}

async fn add_comment(State(home): Shared, Path(id): Path<i64>, Json(c): Json<NewComment>) -> Response {
    with_store(home, move |h, s| {
        for file in &c.images {
            image_saved(h, file)?;
        }
        s.add_comment(id, "me", &c.body, &c.images)
    })
    .await
}

#[derive(Deserialize)]
struct Attach {
    file: String,
}

async fn attach_image(State(home): Shared, Path(id): Path<i64>, Json(a): Json<Attach>) -> Response {
    with_store(home, move |h, s| {
        image_saved(h, &a.file)?;
        s.add_image(id, &a.file)
    })
    .await
}

/// Removes an image from its task or comment, and its file once nothing uses it.
async fn delete_image(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Image(id), || s.delete_image(id))).await
}

/// Fails unless `file` is an image saved by `/api/images`.
fn image_saved(home: &Home, file: &str) -> store::Result<()> {
    if store::valid_image_file(file) && home.dir.join("images").join(file).is_file() {
        Ok(())
    } else {
        Err(Error::Invalid(format!("no saved image {file:?}; paste it again")))
    }
}

/// An attached image. Its name is its content's hash, so it never changes and can be cached for good.
async fn image_file(State(home): Shared, Path(file): Path<String>) -> Response {
    let mime = match file.rsplit_once('.').map(|(_, ext)| ext) {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if !store::valid_image_file(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = home.dir.join("images").join(&file);
    match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => {
            ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], bytes)
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_comment(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Comment(id), || s.delete_comment(id))).await
}

/// Saves a pasted or dropped image in the data folder's `images/` and answers with its
/// file name and path; the board then attaches it to a task or a comment. Files are
/// named by their content, so the same image saved twice is stored once.
async fn save_image(State(home): Shared, headers: HeaderMap, body: Bytes) -> Response {
    let ext = match headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        Some("image/png") => "png",
        Some("image/jpeg") => "jpg",
        Some("image/gif") => "gif",
        Some("image/webp") => "webp",
        _ => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "paste a PNG, JPEG, GIF or WebP image" })))
                .into_response();
        }
    };
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "the image is empty" }))).into_response();
    }
    let saved = tokio::task::spawn_blocking(move || -> std::io::Result<std::path::PathBuf> {
        use std::hash::{Hash, Hasher};
        let dir = home.dir.join("images");
        std::fs::create_dir_all(&dir)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        let path = dir.join(format!("{:016x}.{ext}", hasher.finish()));
        if !path.exists() {
            // Write to a temporary name first, so a half-written image never has the real one.
            let partial = path.with_extension("part");
            std::fs::write(&partial, &body)?;
            std::fs::rename(&partial, &path)?;
        }
        Ok(path)
    })
    .await;
    match saved {
        Ok(Ok(path)) => {
            let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            Json(json!({ "file": file, "path": path.display().to_string() })).into_response()
        }
        Ok(Err(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("cannot save the image: {e}") })))
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct SyncRequest {
    /// "local" or "remote", once the board has asked which side to keep.
    #[serde(default)]
    keep: Option<String>,
}

/// The board's sync button: the same as `kanzo sync`, without git ever prompting. When
/// both sides changed, answers 409 with `diverged`, and the board asks which side to keep.
async fn sync_now(State(home): Shared, Json(r): Json<SyncRequest>) -> Response {
    let keep = match r.keep.as_deref() {
        Some("local") => Some(sync::Keep::Local),
        Some("remote") => Some(sync::Keep::Remote),
        _ => None,
    };
    match tokio::task::spawn_blocking(move || sync::sync(&home, keep, false)).await {
        Ok(Ok(message)) => Json(json!({ "message": message })).into_response(),
        Ok(Err(e)) => match e.downcast_ref::<sync::Diverged>() {
            Some(d) => (StatusCode::CONFLICT, Json(json!({ "error": d.message, "diverged": true, "first": d.first })))
                .into_response(),
            None => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") }))).into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// The repo the board syncs with, `null` before sync is set up.
async fn sync_status(State(home): Shared) -> Response {
    let remote = tokio::task::spawn_blocking(move || sync::remote_url(&home)).await.ok().flatten();
    Json(json!({ "remote": remote })).into_response()
}

#[derive(Deserialize)]
struct SyncRepo {
    url: String,
}

/// Sets up sync with a repo, or moves it to another one.
async fn sync_repo(State(home): Shared, Json(r): Json<SyncRepo>) -> Response {
    match tokio::task::spawn_blocking(move || sync::set_remote(&home, &r.url, false)).await {
        Ok(Ok(remote)) => Json(json!({ "remote": remote })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("{e:#}") }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateQuery {
    /// Present to check GitHub now rather than reuse today's answer.
    now: Option<String>,
}

/// This version, the latest release (`null` when GitHub can't be reached) and, when
/// that one is newer, `available`.
async fn update_status(Query(q): Query<UpdateQuery>) -> Response {
    let now = q.now.is_some();
    let latest = tokio::task::spawn_blocking(move || update::latest(now)).await.ok().flatten();
    let available = latest.clone().filter(|v| update::newer(v));
    Json(json!({ "current": update::CURRENT, "latest": latest, "available": available })).into_response()
}

/// Installs the latest release over this program, answers, then starts it in place.
async fn update_now() -> Response {
    match tokio::task::spawn_blocking(update::install).await {
        Ok(Ok(exe)) => {
            tokio::spawn(async move {
                // Let the answer reach the board first.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let e = update::restart(&exe);
                eprintln!("kanzo: updated, but cannot restart: {e}");
            });
            Json(json!({ "restarting": true })).into_response()
        }
        Ok(Err(error)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": error }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// True when the program on `port` answers like a Kanzo board.
async fn is_kanzo(port: u16) -> bool {
    let probe = async {
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        conn.write_all(b"GET /api/ping HTTP/1.0\r\nHost: localhost\r\n\r\n").await.ok()?;
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.ok()?;
        Some(reply.ends_with("kanzo"))
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), probe).await.ok().flatten().unwrap_or(false)
}

fn open_browser(url: &str) {
    if cfg!(target_os = "macos") {
        if std::process::Command::new("open").arg(url).spawn().is_err() {
            eprintln!("Open {url} in your browser.");
        }
        return;
    }
    // A board without a screen has nothing to open; the URL is already printed.
    let desktop = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()));
    if desktop && std::process::Command::new("xdg-open").arg(url).spawn().is_err() {
        eprintln!("Open {url} in your browser.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        let local = Guard { port: 4242, lan: false, names: vec![] };
        for ok in ["localhost", "localhost:4242", "127.0.0.1:4242", "[::1]:4242", "LOCALHOST:4242"] {
            assert!(local.allows(ok), "{ok}");
        }
        for bad in ["evil.com", "evil.com:4242", "localhost:80", "192.168.1.5:4242", "", "pi.local:4242"] {
            assert!(!local.allows(bad), "{bad}");
        }

        let lan = Guard { port: 4242, lan: true, names: vec!["pi".into(), "pi.local".into()] };
        for ok in ["localhost:4242", "192.168.1.5:4242", "[fe80::1]:4242", "pi.local:4242", "pi:4242", "PI.LOCAL."] {
            assert!(lan.allows(ok), "{ok}");
        }
        for bad in ["evil.com:4242", "pi.evil.com:4242", "192.168.1.5:80"] {
            assert!(!lan.allows(bad), "{bad}");
        }
    }
}
