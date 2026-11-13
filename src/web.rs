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
const ZH_CN_JS: &str = include_str!("zh-cn.js");
const LOGO: &[u8] = include_bytes!("logo.png");
const ICON: &[u8] = include_bytes!("icon.png");
const CHANGELOG: &str = include_str!("../CHANGELOG.md");
const MANIFEST: &str = r##"{"id":"/","name":"Kanzo","start_url":"/","display":"standalone","background_color":"#ffffff","icons":[{"src":"/icon.png","sizes":"192x192 512x512","type":"image/png"}]}"##;

/// Scripts only from this server, so even a bug in the Markdown renderer cannot run injected script.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                   connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

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
                    open_board(&url, port);
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
        // It waits to see if there's an app, which must not hold up the board.
        let url = url.clone();
        tokio::task::spawn_blocking(move || open_board(&url, port));
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
    #[cfg(unix)]
    tokio::spawn(restart_when_replaced());
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

/// However Kanzo was updated, from Settings, `kanzo update` or a package manager, the board
/// restarts on the new program, keeping its port and data folder. Open pages then reload.
#[cfg(unix)]
async fn restart_when_replaced() {
    let Some(exe) = update::Exe::current() else { return };
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tick.tick().await;
        if !exe.changed() {
            continue;
        }
        let check = exe.clone();
        let Ok(Some(new)) = tokio::task::spawn_blocking(move || check.replacement().map(|p| p.to_path_buf())).await
        else {
            continue;
        };
        println!("Kanzo was updated; restarting the board.");
        eprintln!("kanzo: updated, but cannot restart: {}", update::restart(&new));
        return;
    }
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
        .route("/api/events", get(events))
        .route("/api/board", get(board))
        .route("/api/search", get(search))
        .route("/api/projects", post(create_project))
        .route("/api/projects/{slug}", patch(update_project).delete(delete_project))
        .route("/api/projects/{slug}/repos", post(link_repo).delete(unlink_repo))
        .route("/api/projects/{slug}/move", post(move_project))
        .route("/api/tasks", post(create_task))
        .route("/api/tasks/bulk", post(bulk_tasks))
        .route("/api/tasks/move", post(move_tasks))
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
        .route(
            "/i18n/zh-cn.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], ZH_CN_JS) }),
        )
        .route("/logo.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], LOGO) }))
        .route("/icon.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], ICON) }))
        .route("/changelog.md", get(|| async { ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], CHANGELOG) }))
        .route(
            "/manifest.webmanifest",
            get(|| async { ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST) }),
        )
        .route("/api/ping", get(|| async { "kanzo" }))
        .route("/api/update", get(update_status).post(update_now))
        .route("/api/network", get(network).post(change_network).route_layer(middleware::from_fn(this_computer)))
        // Not forwarded by a joined board either: the files are on this computer.
        .route("/api/open", post(open_file).route_layer(middleware::from_fn(this_computer)))
        .merge(shared)
        .layer(Extension(guard.clone()))
        .layer(middleware::from_fn_with_state(guard, protect))
        .layer(middleware::map_response(stamp_version))
        .with_state(Arc::new(home))
}

/// Pages compare it with the version they loaded with, and reload after an update. Set on
/// the way out, so a joined board's forwarded replies carry this computer's version, not the host's.
async fn stamp_version(mut res: Response) -> Response {
    res.headers_mut().insert("x-kanzo-version", HeaderValue::from_static(update::CURRENT));
    res
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
        Some(_) => fail(StatusCode::FORBIDDEN, "only the computer with the board can do that").into_response(),
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

#[derive(Deserialize)]
struct EventsQuery {
    /// The last event id the reader has seen. Without it, the reply only says where to start.
    since: Option<i64>,
    project: Option<String>,
    /// Seconds to hold the request open until an event comes (long polling), at most 25.
    #[serde(default)]
    wait: u64,
}

#[derive(Serialize)]
struct Events {
    events: Vec<store::Event>,
    /// The id to pass as `since` next time.
    last: i64,
}

const EVENTS_PER_REPLY: usize = 500;
const EVENTS_WAIT_MAX: u64 = 25;

/// Lets a service react to the board as it changes, like starting an agent for a new task,
/// without polling the whole board: `GET /api/events?since=<last>&wait=25` in a loop.
async fn events(State(home): Shared, Query(q): Query<EventsQuery>) -> Response {
    let read = move |s: &Store, q: &EventsQuery| -> store::Result<Events> {
        let project = match &q.project {
            Some(p) => Some(s.find_project(p)?.id),
            None => None,
        };
        let Some(since) = q.since else {
            return Ok(Events { events: Vec::new(), last: s.last_event()? });
        };
        let events = s.events_since(since, project, EVENTS_PER_REPLY)?;
        let last = events.last().map_or(since, |e| e.id);
        Ok(Events { events, last })
    };
    let q = Arc::new(q);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(q.wait.min(EVENTS_WAIT_MAX));
    if let Some(since) = q.since {
        // One indexed read every half second, and only while a reader waits.
        while tokio::time::Instant::now() < deadline {
            let (h, project) = (home.clone(), q.project.clone());
            let ready = blocking(move || -> store::Result<bool> {
                let s = h.store()?;
                let pid = match &project {
                    Some(p) => Some(s.find_project(p)?.id),
                    None => None,
                };
                Ok(!s.events_since(since, pid, 1)?.is_empty())
            })
            .await;
            // Ready, or an error that the read below reports.
            if !matches!(ready, Ok(Ok(false))) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    with_store(home, move |_, s| read(s, &q)).await
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
    hidden_properties: Vec<String>,
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
            hidden_properties: s.hidden_properties()?,
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
struct ProjectMove {
    before: Option<String>,
}

async fn move_project(State(home): Shared, Path(slug): Path<String>, Json(m): Json<ProjectMove>) -> Response {
    with_store(home, move |_, s| s.move_project(&slug, m.before.as_deref())).await
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
    description: Option<String>,
}

async fn create_task(State(home): Shared, Json(t): Json<NewTask>) -> Response {
    with_store(home, move |_, s| {
        let project = s.find_project(&t.project)?;
        s.create_task_in(project.id, &t.title, TaskPatch { description: t.description, ..Default::default() })
    })
    .await
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Settings {
    Backlog { backlog: bool },
    Property { property: String, visible: bool },
}

async fn update_settings(State(home): Shared, Json(r): Json<Settings>) -> Response {
    with_store(home, move |_, s| match r {
        Settings::Backlog { backlog } => s.set_backlog(backlog),
        Settings::Property { property, visible } => s.set_property(&property, visible),
    })
    .await
}

#[derive(Serialize)]
struct TaskReply {
    blocks: Vec<store::TaskLink>,
    comments: Vec<store::Comment>,
    images: Vec<store::Image>,
    task: store::Task,
    waits_on: Vec<store::TaskLink>,
}

async fn get_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| {
        Ok(TaskReply {
            blocks: s.task_links(id, true)?,
            comments: s.list_comments(id)?,
            images: s.list_images(id)?,
            task: s.get_task(id)?,
            waits_on: s.task_links(id, false)?,
        })
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
    url: Option<String>,
    waits_on: Option<String>,
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
                url: p.url,
                waits_on: p.waits_on,
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

#[derive(Deserialize)]
struct MoveMany {
    ids: Vec<i64>,
    status: String,
    before: Option<i64>,
}

async fn move_tasks(State(home): Shared, Json(m): Json<MoveMany>) -> Response {
    with_store(home, move |_, s| s.move_tasks(&m.ids, &m.status, m.before)).await
}

async fn delete_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Task(id), || s.delete_task(id))).await
}

#[derive(Deserialize)]
struct Bulk {
    ids: Vec<i64>,
    status: Option<String>,
    #[serde(default)]
    add_tags: String,
    #[serde(default)]
    remove_tags: String,
    #[serde(default)]
    delete: bool,
}

async fn bulk_tasks(State(home): Shared, Json(b): Json<Bulk>) -> Response {
    with_store(home, move |h, s| {
        if b.delete {
            h.deleting_images(s, ImagesOf::Tasks(b.ids.clone()), || s.delete_tasks(&b.ids))
        } else {
            s.update_tasks(&b.ids, b.status.as_deref(), &b.add_tags, &b.remove_tags)
        }
    })
    .await
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
struct OpenFile {
    path: std::path::PathBuf,
}

async fn open_file(State(home): Shared, Json(o): Json<OpenFile>) -> Reply {
    blocking(move || home.open_file(&o.path)).await?.map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({})).into_response())
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

/// The installed app if there is one, or else a browser tab. An app opens the address it was
/// installed from, so only on the default port, where that's this board.
fn open_board(url: &str, port: u16) {
    let app = cfg!(target_os = "macos")
        && port == crate::home::DEFAULT_PORT
        && std::process::Command::new("open")
            .args(["-a", "Kanzo"])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if !app {
        open_browser(url);
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn changelog_is_for_this_version() {
        let heading = format!("## {}", update::CURRENT);
        assert!(
            CHANGELOG.lines().next() == Some(heading.as_str()),
            "CHANGELOG.md should start with the notes for this release"
        );
    }

    #[test]
    fn changelog_links_each_change() {
        for line in CHANGELOG.lines().filter(|l| l.starts_with("- ")) {
            assert!(
                line.ends_with(')') && line.contains("](https://github.com/emanuele-em/kanzo/"),
                "should end with a link to its pull request or commit, after any `thanks @name`: {line}"
            );
        }
    }

    /// The status of asking a board to open `path`, from `from`.
    async fn open_from(home: &Home, from: Option<IpAddr>, path: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let guard = Arc::new(Guard::new(port, Ipv4Addr::LOCALHOST.into(), Net::Sharing));
        let mut app = router(home.clone(), guard, CancellationToken::new());
        if let Some(ip) = from {
            app = app.layer(Extension(ConnectInfo(SocketAddr::new(ip, 1))));
        }
        tokio::spawn(async move { axum::serve(listener, app).await });
        let body = json!({ "path": path }).to_string();
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let head = format!(
            "POST /api/open HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        conn.write_all(format!("{head}{body}").as_bytes()).await.unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.unwrap();
        reply.split(' ').nth(1).unwrap().to_string()
    }

    /// The status and body of `GET path` on a board for this computer.
    async fn get_from(home: &Home, path: &str) -> (String, serde_json::Value) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let guard = Arc::new(Guard::new(port, Ipv4Addr::LOCALHOST.into(), Net::Private));
        let app = router(home.clone(), guard, CancellationToken::new());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await.unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        (head.split(' ').nth(1).unwrap().to_string(), serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
    }

    #[test]
    fn events_tell_a_reader_what_changed_since_it_last_asked() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let home = Home::open(tmp.path().to_path_buf()).unwrap();
            let s = home.store().unwrap();
            s.migrate().unwrap();
            let p = s.create_project("api", "", None).unwrap();
            s.create_task(p.id, "old", "").unwrap();

            let (code, start) = get_from(&home, "/api/events").await;
            assert_eq!(code, "200");
            assert_eq!(start["events"], json!([]), "a new reader starts now");
            let last = start["last"].as_i64().unwrap();
            assert!(last > 0);

            let t = s.create_task(p.id, "new", "").unwrap();
            s.move_task(t.id, "doing", None).unwrap();
            let (code, got) = get_from(&home, &format!("/api/events?since={last}")).await;
            assert_eq!(code, "200");
            let kinds: Vec<_> = got["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
            assert_eq!(kinds, ["task.created", "task.moved"]);
            assert_eq!(got["events"][1]["task"], format!("api#{}", t.id));
            assert_eq!(got["events"][1]["from"], "todo");
            assert_eq!(got["events"][1]["to"], "doing");
            let next = got["last"].as_i64().unwrap();

            let (_, none) = get_from(&home, &format!("/api/events?since={next}&wait=1")).await;
            assert_eq!(none["events"], json!([]));
            assert_eq!(none["last"].as_i64(), Some(next), "nothing new keeps the place");

            let (code, _) = get_from(&home, "/api/events?since=0&project=nope").await;
            assert_eq!(code, "404");
        });
    }

    #[test]
    fn a_waiting_reader_gets_the_event_as_it_happens() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let home = Home::open(tmp.path().to_path_buf()).unwrap();
            let s = home.store().unwrap();
            s.migrate().unwrap();
            let p = s.create_project("api", "", None).unwrap();
            let last = s.last_event().unwrap();
            drop(s);

            let writer = home.clone();
            let pid = p.id;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(700)).await;
                tokio::task::spawn_blocking(move || writer.store().unwrap().create_task(pid, "live", "").unwrap())
                    .await
                    .unwrap();
            });
            let began = Instant::now();
            let (code, got) = get_from(&home, &format!("/api/events?since={last}&wait=10")).await;
            assert_eq!(code, "200");
            assert_eq!(got["events"][0]["kind"], "task.created");
            assert!(began.elapsed() < Duration::from_secs(5), "answers when the event comes, not after the wait");
        });
    }

    #[test]
    fn only_this_computer_opens_a_file_and_only_a_file() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let mut home = Home::open(tmp.path().to_path_buf()).unwrap();
            home.config.editor = Some("true".into());
            let file = tmp.path().join("notes.md");
            std::fs::write(&file, "").unwrap();
            let file = file.to_str().unwrap();

            let guest = Some(IpAddr::from([192, 168, 1, 9]));
            assert_eq!(open_from(&home, guest, file).await, "403");
            assert_eq!(open_from(&home, None, "notes.md").await, "400");
            assert_eq!(open_from(&home, None, tmp.path().to_str().unwrap()).await, "400");
            assert_eq!(open_from(&home, None, &format!("{file}.missing")).await, "400");
            #[cfg(unix)]
            assert_eq!(open_from(&home, None, file).await, "200");
        });
    }

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
