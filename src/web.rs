use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::share::{self, Change, Net};
use crate::store::{self, Error, ImagesOf, Store, TaskPatch};
use crate::{sync, update};

const INDEX: &str = include_str!("index.html");
const IMAGE_LIMIT: usize = 25 * 1024 * 1024;
const APP_JS: &str = include_str!("app.js");
const LOGO: &[u8] = include_bytes!("logo.png");

/// Scripts only from this server, so even a bug in the Markdown renderer cannot run injected script.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                   connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub async fn serve(home: Home, open: bool) -> Result<()> {
    let port = home.config.port;
    let guard = Arc::new(Guard::new(port, home.config.listen, Net::load(&home)));
    let url = format!("http://localhost:{port}");
    let listener = match TcpListener::bind(SocketAddr::new(guard.address(), port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if share::is_kanzo(&format!("127.0.0.1:{port}")).await {
                println!("Kanzo is already running at {url}");
                if open {
                    open_browser(&url);
                }
                return Ok(());
            }
            bail!("port {port} is used by another program; choose another with --port or in config.toml");
        }
        Err(e) => return Err(e).context(format!("cannot listen on {} port {port}", guard.address())),
    };
    #[cfg(windows)]
    exit_with_console();
    println!("Kanzo board at {url}, data in {}", home.dir.display());
    match guard.net() {
        Net::Private => {}
        Net::Joined(host) => println!("It shows the board at {host}."),
        _ => {
            println!("{}", guard.status().message());
            println!("Anyone who can reach port {port} can read and change this board.");
        }
    }
    if home.config.mcp_http {
        println!("MCP over HTTP at {url}/mcp");
    }
    if open {
        open_browser(&url);
    }
    // Cancelling this also ends open MCP streams, which would otherwise hold up shutdown.
    let shutdown = CancellationToken::new();
    let app = router(home.clone(), guard.clone(), shutdown.clone());
    let serve = |listener, stop: CancellationToken| {
        let app = app.clone().into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(
            axum::serve(listener, app).with_graceful_shutdown(async move { stop.cancelled().await }).into_future(),
        )
    };
    let mut running = Vec::new();
    // `localhost` is ::1 before 127.0.0.1 on many Linux systems, and not every
    // client (busybox wget, older Node) falls back, so listen on ::1 as well.
    if let IpAddr::V4(ip) = home.config.listen
        && (ip.is_loopback() || ip.is_unspecified())
        && let Ok(v6) = TcpListener::bind(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)).await
    {
        running.push(serve(v6, shutdown.clone()));
    }
    let stopper = shutdown.clone();
    tokio::spawn(async move {
        stop_signal().await;
        stopper.cancel();
    });
    let mut listener = listener;
    loop {
        let rebind = shutdown.child_token();
        *guard.rebind.lock().unwrap() = rebind.clone();
        running.push(serve(listener, rebind.clone()));
        rebind.cancelled().await;
        if shutdown.is_cancelled() {
            break;
        }
        listener = match listen_again(guard.address(), port).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("kanzo: cannot listen on {}: {e}", guard.address());
                listen_again(home.config.listen, port).await?
            }
        };
    }
    for task in running {
        task.await??;
    }
    Ok(())
}

/// The old listener takes a moment to let go of the port.
async fn listen_again(ip: IpAddr, port: u16) -> std::io::Result<TcpListener> {
    for _ in 0..100 {
        match TcpListener::bind(SocketAddr::new(ip, port)).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => tokio::time::sleep(Duration::from_millis(20)).await,
            bound => return bound,
        }
    }
    TcpListener::bind(SocketAddr::new(ip, port)).await
}

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

fn router(home: Home, guard: Arc<Guard>, shutdown: CancellationToken) -> Router {
    let mut shared = Router::new();
    if home.config.mcp_http {
        shared = shared.nest_service("/mcp", crate::mcp::http_service(home.clone(), shutdown));
    }
    let shared = shared
        .route("/images/{file}", get(image_file))
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
        .route("/api/sync", get(sync_status).post(sync_now).put(sync_repo))
        .route("/api/settings", patch(update_settings))
        .route_layer(middleware::from_fn_with_state(guard.clone(), forward));
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        .route("/logo.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], LOGO) }))
        .route("/api/ping", get(|| async { "kanzo" }))
        .route("/api/update", get(update_status).post(update_now))
        .route("/api/network", get(network).post(change_network).route_layer(middleware::from_fn(this_computer)))
        .merge(shared)
        .layer(Extension(guard.clone()))
        .layer(middleware::from_fn_with_state(guard, protect))
        .with_state(Arc::new(home))
}

/// DNS rebinding needs a domain name the attacker controls, so the board only accepts
/// `localhost`, IP addresses and, when it listens on the network, this machine's own names.
struct Guard {
    port: u16,
    listen: IpAddr,
    net: Mutex<Net>,
    guests: Mutex<HashMap<IpAddr, (Instant, Option<String>)>>,
    names: OnceLock<Vec<String>>,
    rebind: Mutex<CancellationToken>,
}

/// A guest's board asks every 2 seconds while it's on screen.
const GUEST_GONE: Duration = Duration::from_secs(30);

impl Guard {
    fn new(port: u16, listen: IpAddr, net: Net) -> Self {
        Self {
            port,
            listen,
            net: Mutex::new(net),
            guests: Mutex::default(),
            names: OnceLock::new(),
            rebind: Mutex::default(),
        }
    }

    fn net(&self) -> Net {
        self.net.lock().unwrap().clone()
    }

    fn lan(&self) -> bool {
        self.net() == Net::Sharing
    }

    fn address(&self) -> IpAddr {
        if self.lan() && self.listen.is_loopback() { Ipv4Addr::UNSPECIFIED.into() } else { self.listen }
    }

    fn set(&self, net: Net) {
        let before = self.address();
        *self.net.lock().unwrap() = net;
        if self.address() != before {
            self.rebind.lock().unwrap().cancel();
        }
        if !self.lan() {
            self.guests.lock().unwrap().clear();
        }
    }

    fn guests(&self) -> Vec<share::Guest> {
        let mut guests = self.guests.lock().unwrap();
        guests.retain(|_, (seen, _)| seen.elapsed() < GUEST_GONE);
        let mut list: Vec<share::Guest> =
            guests.iter().map(|(ip, (_, name))| share::Guest { name: name.clone(), address: *ip }).collect();
        list.sort_by_key(|g| g.address);
        list
    }

    fn status(&self) -> share::Status {
        share::Status::new(&self.net(), self.port, self.guests())
    }

    fn names(&self) -> &[String] {
        self.names.get_or_init(|| {
            use std::process::Command;
            let run = |cmd: &mut Command| crate::home::output(cmd).ok().map(|n| n.to_ascii_lowercase());
            // macOS keeps its Bonjour name apart; `hostname` there can be a DHCP address.
            let found =
                [run(Command::new("scutil").args(["--get", "LocalHostName"])), run(&mut Command::new("hostname"))];
            let mut names = Vec::new();
            for full in found.into_iter().flatten().filter(|n| !n.is_empty() && n.parse::<IpAddr>().is_err()) {
                let short = full.split('.').next().unwrap_or_default().to_string();
                for name in [format!("{short}.local"), full, short] {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
            names
        })
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
        self.lan() && (ip.is_some() || self.names().iter().any(|n| n.eq_ignore_ascii_case(name)))
    }
}

fn guest(req: &Request) -> Option<IpAddr> {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).filter(|ip| !ip.is_loopback())
}

async fn forward(State(guard): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    match guard.net() {
        Net::Joined(host) => share::forward(&host, req).await,
        _ => next.run(req).await,
    }
}

async fn this_computer(req: Request, next: Next) -> Response {
    match guest(&req) {
        Some(_) => (StatusCode::FORBIDDEN, "only the computer with the board can change its network").into_response(),
        None => next.run(req).await,
    }
}

async fn protect(State(guard): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let guest = guest(&req);
    let ok = {
        let header = |name| req.headers().get(name).and_then(|h: &HeaderValue| h.to_str().ok());
        let host = header(header::HOST).unwrap_or("");
        let same_origin = |o: &str| {
            o.len() == host.len() + 7
                && o.as_bytes()[..7].eq_ignore_ascii_case(b"http://")
                && o.as_bytes()[7..].eq_ignore_ascii_case(host.as_bytes())
        };
        guard.allows(host) && header(header::ORIGIN).is_none_or(same_origin) && (guest.is_none() || guard.lan())
    };
    if let Some(ip) = guest.filter(|_| ok) {
        let name =
            req.headers().get(share::COMPUTER_HEADER).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        let mut guests = guard.guests.lock().unwrap();
        let guest = guests.entry(ip).or_insert((Instant::now(), None));
        guest.0 = Instant::now();
        if name.is_some() {
            guest.1 = name;
        }
    }
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

async fn with_store<T: Serialize + Send + 'static>(
    home: Arc<Home>,
    f: impl FnOnce(&Home, &Store) -> store::Result<T> + Send + 'static,
) -> Response {
    let error = match blocking(move || f(&home, &home.store()?)).await {
        Ok(Ok(v)) => return Json(v).into_response(),
        Ok(Err(e)) => e,
        Err(e) => return e.into_response(),
    };
    let code = match error {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
        Error::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    fail(code, error).into_response()
}

type Reply = Result<Response, Fail>;

pub struct Fail(StatusCode, String);

pub fn fail(code: StatusCode, error: impl std::fmt::Display) -> Fail {
    Fail(code, error.to_string())
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Fail> {
    tokio::task::spawn_blocking(f).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// The database file's identity is part of it so a board pulled by `kanzo sync` always reloads.
fn change_token(home: &Home, s: &Store) -> store::Result<String> {
    Ok(format!("{}-{}", s.changes()?, home.db_identity()))
}

#[derive(Serialize)]
struct Changes {
    changes: String,
    guests: Option<usize>,
}

async fn changes(State(home): Shared, Extension(guard): Extension<Arc<Guard>>) -> Response {
    let guests = guard.lan().then(|| guard.guests().len());
    with_store(home, move |h, s| Ok(Changes { changes: change_token(h, s)?, guests })).await
}

/// The done column only grows, and every change reloads the board.
const DONE_SHOWN: usize = 50;

#[derive(Deserialize)]
struct BoardQuery {
    project: Option<String>,
    done: Option<String>,
    #[serde(default)]
    q: String,
}

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
        let (mut tasks, done_hidden) = s.list_tasks_keeping_done(project.map(|p| p.id), None, keep_done)?;
        if !q.q.trim().is_empty() {
            let found: HashSet<i64> = s.search(&q.q, None)?.into_iter().map(|t| t.id).collect();
            tasks.retain(|t| found.contains(&t.id));
        }
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

async fn search(State(home): Shared, Query(q): Query<SearchQuery>) -> Response {
    with_store(home, move |_, s| s.search(&q.q, Some(30))).await
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
    prs: Option<String>,
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
                prs: p.prs,
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

async fn delete_image(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Image(id), || s.delete_image(id))).await
}

fn image_saved(home: &Home, file: &str) -> store::Result<()> {
    if store::valid_image_file(file) && home.images().join(file).is_file() {
        Ok(())
    } else {
        Err(Error::Invalid(format!("no saved image {file:?}; paste it again")))
    }
}

/// Named by its content's hash, so it never changes and can be cached for good.
async fn image_file(State(home): Shared, Path(file): Path<String>) -> Response {
    let ext = file.rsplit_once('.').map(|(_, ext)| ext);
    let Some((_, mime)) = store::IMAGE_TYPES.iter().find(|(e, _)| Some(*e) == ext) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !store::valid_image_file(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = home.images().join(&file);
    match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => {
            ([(header::CONTENT_TYPE, *mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], bytes)
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_comment(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Comment(id), || s.delete_comment(id))).await
}

async fn save_image(State(home): Shared, headers: HeaderMap, body: Bytes) -> Reply {
    let mime = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let Some((ext, _)) = store::IMAGE_TYPES.iter().find(|(_, m)| Some(*m) == mime) else {
        return Err(fail(StatusCode::BAD_REQUEST, "paste a PNG, JPEG, GIF or WebP image"));
    };
    if body.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "the image is empty"));
    }
    let path = blocking(move || -> std::io::Result<std::path::PathBuf> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        let path = home.images().join(format!("{:016x}.{ext}", hasher.finish()));
        if !path.exists() {
            crate::home::write_whole(&path, &body)?;
        }
        Ok(path)
    })
    .await?
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save the image: {e}")))?;
    let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Json(json!({ "file": file, "path": path.display().to_string() })).into_response())
}

async fn network(Extension(guard): Extension<Arc<Guard>>) -> Response {
    Json(guard.status()).into_response()
}

async fn change_network(
    State(home): Shared,
    Extension(guard): Extension<Arc<Guard>>,
    Json(change): Json<Change>,
) -> Response {
    match share::change(&home, change).await {
        Ok(net) => {
            guard.set(net);
            Json(guard.status()).into_response()
        }
        Err(e) => fail(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
struct SyncRequest {
    #[serde(default)]
    keep: Option<String>,
}

async fn sync_now(State(home): Shared, Json(r): Json<SyncRequest>) -> Reply {
    let keep = match r.keep.as_deref() {
        Some("local") => Some(sync::Keep::Local),
        Some("remote") => Some(sync::Keep::Remote),
        _ => None,
    };
    let synced = blocking(move || sync::sync(&home, keep, false)).await?;
    Ok(match synced {
        Ok(message) => Json(json!({ "message": message })).into_response(),
        Err(e) => match e.downcast_ref::<sync::Diverged>() {
            Some(d) => (StatusCode::CONFLICT, Json(json!({ "error": d.message, "diverged": true, "first": d.first })))
                .into_response(),
            None => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        },
    })
}

async fn sync_status(State(home): Shared) -> Response {
    let remote = blocking(move || sync::remote_url(&home)).await.ok().flatten();
    Json(json!({ "remote": remote })).into_response()
}

#[derive(Deserialize)]
struct SyncRepo {
    url: String,
}

async fn sync_repo(State(home): Shared, Json(r): Json<SyncRepo>) -> Reply {
    let remote = blocking(move || sync::set_remote(&home, &r.url, false))
        .await?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "remote": remote })).into_response())
}

#[derive(Deserialize)]
struct UpdateQuery {
    now: Option<String>,
}

async fn update_status(Query(q): Query<UpdateQuery>) -> Response {
    let now = q.now.is_some();
    let latest = blocking(move || update::latest(now)).await.ok().flatten();
    let available = latest.clone().filter(|v| update::newer(v));
    Json(json!({ "current": update::CURRENT, "latest": latest, "available": available })).into_response()
}

async fn update_now(State(home): Shared) -> Reply {
    // `can_restart` runs schtasks on Windows, so it waits off the async thread too.
    let dir = home.dir.clone();
    let (restart, installed) = blocking(move || (update::can_restart(&dir), update::install())).await?;
    let exe = installed.map_err(|error| fail(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    if restart {
        tokio::spawn(async move {
            // Let the answer reach the board first.
            tokio::time::sleep(Duration::from_millis(300)).await;
            // Blocking: on Windows it waits for the service to stop this process.
            if let Ok(e) = tokio::task::spawn_blocking(move || update::restart(&exe)).await {
                eprintln!("kanzo: updated, but cannot restart: {e}");
            }
        });
    }
    Ok(Json(json!({ "restarting": restart })).into_response())
}

/// Windows doesn't stop the program when its console goes away: the service's conhost
/// when the task is stopped, or the window the board was started in.
#[cfg(windows)]
fn exit_with_console() {
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    // SAFETY: room for one process id, as `count` says; it returns 0 without a console.
    let attached = || unsafe { GetConsoleProcessList(&mut 0, 1) } != 0;
    if attached() {
        std::thread::spawn(move || {
            while attached() {
                std::thread::sleep(Duration::from_secs(1));
            }
            std::process::exit(0);
        });
    }
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        let desktop = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()));
        if !desktop {
            return;
        }
        "xdg-open"
    };
    if std::process::Command::new(opener).arg(url).spawn().is_err() {
        eprintln!("Open {url} in your browser.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        let local = Guard::new(4242, Ipv4Addr::LOCALHOST.into(), Net::Private);
        for ok in ["localhost", "localhost:4242", "127.0.0.1:4242", "[::1]:4242", "LOCALHOST:4242"] {
            assert!(local.allows(ok), "{ok}");
        }
        for bad in ["evil.com", "evil.com:4242", "localhost:80", "192.168.1.5:4242", "", "pi.local:4242"] {
            assert!(!local.allows(bad), "{bad}");
        }

        let lan = Guard::new(4242, Ipv4Addr::LOCALHOST.into(), Net::Sharing);
        lan.names.set(vec!["pi".into(), "pi.local".into()]).unwrap();
        for ok in ["localhost:4242", "192.168.1.5:4242", "[fe80::1]:4242", "pi.local:4242", "pi:4242", "PI.LOCAL."] {
            assert!(lan.allows(ok), "{ok}");
        }
        for bad in ["evil.com:4242", "pi.evil.com:4242", "192.168.1.5:80"] {
            assert!(!lan.allows(bad), "{bad}");
        }

        lan.set(Net::Private);
        assert!(!lan.allows("192.168.1.5:4242"));
        assert_eq!(lan.address(), IpAddr::from(Ipv4Addr::LOCALHOST));
    }
}
