use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rmcp::model::{ClientConfig, Implementation, ProtocolVersion, RequestMetaObject};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::io::ReaderStream;

use crate::home::{DEFAULT_PORT, Home};

pub const FILE: &str = "network";

pub const FOLDER_HEADER: &str = "x-kanzo-folder";
pub const REPOS_HEADER: &str = "x-kanzo-repos";
pub const IMAGES_HEADER: &str = "x-kanzo-images";
pub const COMPUTER_HEADER: &str = "x-kanzo-computer";

/// Larger than any image the board takes.
const BODY_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Net {
    Private,
    Sharing,
    Joined(String),
}

impl Net {
    pub fn load(home: &Home) -> Self {
        let text = std::fs::read_to_string(home.dir.join(FILE)).unwrap_or_default();
        match text.trim() {
            "share" => Net::Sharing,
            line => match line.strip_prefix("join ") {
                Some(host) => Net::Joined(host.trim().to_string()),
                // config.toml can keep the board on the network for good, as before `kanzo share`.
                None if !home.config.listen.is_loopback() => Net::Sharing,
                None => Net::Private,
            },
        }
    }

    fn save(&self, home: &Home) -> io::Result<()> {
        let path = home.dir.join(FILE);
        match self {
            Net::Private => std::fs::remove_file(path)
                .or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) }),
            Net::Sharing => std::fs::write(path, "share\n"),
            Net::Joined(host) => std::fs::write(path, format!("join {host}\n")),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", content = "host", rename_all = "lowercase")]
pub enum Change {
    On,
    Off,
    Join(String),
    Leave,
}

pub async fn change(home: &Home, change: Change) -> Result<Net, String> {
    let now = Net::load(home);
    let next = match (change, &now) {
        (Change::On, Net::Joined(host)) => {
            return Err(format!("This computer uses the board at {host}: run kanzo leave first."));
        }
        (Change::On, _) => Net::Sharing,
        (Change::Off, _) if !home.config.listen.is_loopback() => {
            return Err(format!(
                "config.toml sets listen = \"{}\", which keeps the board on the network: change it there.",
                home.config.listen
            ));
        }
        (Change::Off, Net::Sharing) | (Change::Leave, Net::Joined(_)) => Net::Private,
        (Change::Off | Change::Leave, _) => now.clone(),
        (Change::Join(_), Net::Sharing) => return Err("This board is shared: run kanzo share off first.".into()),
        (Change::Join(host), _) => {
            let host = address(&host)?;
            if !is_kanzo(&host).await {
                return Err(format!(
                    "No shared board at {host}: share it on that computer first, on the same network."
                ));
            }
            Net::Joined(host)
        }
    };
    if next != now {
        next.save(home).map_err(|e| format!("cannot save {FILE}: {e}"))?;
    }
    Ok(next)
}

fn address(host: &str) -> Result<String, String> {
    let host = host.trim().trim_start_matches("http://").trim_end_matches('/');
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c)) {
        return Err(format!("{host:?} isn't an address: pass the one `kanzo share on` printed, like 192.168.0.12."));
    }
    let full = match host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, DEFAULT_PORT).to_string(),
        Err(_) if host.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) => host.to_string(),
        Err(_) => format!("{host}:{DEFAULT_PORT}"),
    };
    let local = full.parse::<SocketAddr>().is_ok_and(|a| a.ip().is_loopback() || a.ip().is_unspecified())
        || full.to_ascii_lowercase().starts_with("localhost:");
    if local {
        return Err("That's this computer: pass the address of the one sharing its board.".into());
    }
    Ok(full)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    state: State,
    host: Option<String>,
    addresses: Vec<String>,
    guests: Vec<Guest>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Guest {
    pub name: Option<String>,
    pub address: IpAddr,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Private,
    Sharing,
    Joined,
}

impl Status {
    pub fn new(net: &Net, port: u16, guests: Vec<Guest>) -> Self {
        let (state, host, addresses) = match net {
            Net::Private => (State::Private, None, vec![]),
            Net::Sharing => (State::Sharing, None, addresses(port)),
            Net::Joined(host) => (State::Joined, Some(host.clone()), vec![]),
        };
        Self { state, host, addresses, guests }
    }

    pub fn message(&self) -> String {
        match self.state {
            State::Sharing => {
                let mut out = String::from("Shared on your network. On another computer, run");
                out += if self.addresses.len() > 1 { " the line for its network:" } else { ":" };
                if self.addresses.is_empty() {
                    out += "\n  kanzo join <this computer's IP address>";
                }
                for address in &self.addresses {
                    let _ = write!(out, "\n  kanzo join {address}");
                }
                if !self.guests.is_empty() {
                    let guests: Vec<String> = self
                        .guests
                        .iter()
                        .map(|g| g.name.as_ref().map_or(g.address.to_string(), |n| format!("{n} ({})", g.address)))
                        .collect();
                    let _ = write!(out, "\nUsing it now: {}", guests.join(", "));
                }
                out
            }
            State::Joined => format!(
                "Using the board at {}. Your agents use it too, from their next call.",
                self.host.as_deref().unwrap_or_default()
            ),
            State::Private => "Not shared: the board is on this computer only.".into(),
        }
    }
}

/// One per network, like the home network and a VPN: which one works depends on where the other computer is.
fn addresses(port: u16) -> Vec<String> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    interfaces
        .iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback() && i.ip().is_ipv4())
        .map(|i| if port == DEFAULT_PORT { i.ip().to_string() } else { format!("{}:{port}", i.ip()) })
        .collect()
}

pub async fn request(home: &Home, change: Option<Change>) -> Result<(Status, bool)> {
    let port = home.config.port;
    let (method, body) = match &change {
        Some(c) => ("POST", serde_json::to_vec(c)?),
        None => ("GET", Vec::new()),
    };
    let json = [("content-type", "application/json")];
    match send(&format!("127.0.0.1:{port}"), method, "/api/network", &json, &body).await {
        Ok(mut reply) => {
            let mut text = String::new();
            reply.body.read_to_string(&mut text).await?;
            if reply.status == 200 {
                return Ok((serde_json::from_str(&text).context("unexpected answer from the board")?, true));
            }
            match serde_json::from_str::<Value>(&text).ok().and_then(|v| Some(v["error"].as_str()?.to_string())) {
                Some(error) => bail!(error),
                None => bail!("the board on port {port} answered {}: restart it so it runs this version", reply.status),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            let net = match change {
                Some(c) => self::change(home, c).await.map_err(anyhow::Error::msg)?,
                None => Net::load(home),
            };
            Ok((Status::new(&net, port, Vec::new()), false))
        }
        Err(e) => Err(e).with_context(|| format!("cannot reach the board on port {port}")),
    }
}

/// Requests are HTTP/1.0, so the body is all that comes until the host closes the connection, with no chunks to decode.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: BufReader<TcpStream>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

async fn send(host: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> io::Result<Reply> {
    let connect = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(host));
    let mut conn = connect.await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let mut head = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");
    // One write: a separate one for the body can wait on a delayed ACK.
    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    conn.write_all(&request).await?;
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
    let status = status.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an HTTP reply"))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await?;
        let Some((name, value)) = line.trim_end().split_once(':') else { break };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(Reply { status, headers, body: reader })
}

pub async fn is_kanzo(host: &str) -> bool {
    let probe = async {
        let mut reply = send(host, "GET", "/api/ping", &[], b"").await.ok()?;
        let mut body = String::new();
        reply.body.read_to_string(&mut body).await.ok()?;
        Some(reply.status == 200 && body == "kanzo")
    };
    tokio::time::timeout(Duration::from_secs(2), probe).await.ok().flatten().unwrap_or(false)
}

fn computer() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(crate::home::hostname)
}

fn unreachable(host: &str) -> String {
    format!("The shared board at {host} isn't reachable; run kanzo leave to use this computer's board.")
}

pub async fn forward(host: &str, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, BODY_LIMIT).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    // The host checks its own Host; the page's Origin is this board's, checked already.
    let skip = ["host", "origin", "referer", "connection", "content-length", "transfer-encoding", "keep-alive"];
    let mut headers: Vec<(&str, &str)> = parts
        .headers
        .iter()
        .filter(|(name, _)| !skip.contains(&name.as_str()))
        .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
        .collect();
    headers.push((COMPUTER_HEADER, computer()));
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let Ok(reply) = send(host, parts.method.as_str(), path, &headers, &body).await else {
        return crate::web::fail(StatusCode::BAD_GATEWAY, unreachable(host)).into_response();
    };
    let mut res = Response::new(Body::from_stream(ReaderStream::new(reply.body)));
    *res.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (name, value) in &reply.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value))
            && name != "connection"
        {
            res.headers_mut().append(name, value);
        }
    }
    res
}

/// The `initialize` the host opens a session with. Clients on protocol 2026-07-28 never send
/// one: they name themselves in every request's `_meta` instead.
pub fn handshake(initialized: Option<&ClientConfig>, meta: &RequestMetaObject) -> ClientConfig {
    initialized.cloned().unwrap_or_else(|| {
        let info = meta.client_info().unwrap_or_else(|| Implementation::new("agent", ""));
        ClientConfig::new(meta.client_capabilities().unwrap_or_default(), info)
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    })
}

pub struct Upstream {
    host: String,
    session: Option<String>,
    folder: String,
    repos: String,
    images: PathBuf,
}

impl Upstream {
    pub fn new(host: String, folder: &Path, repos: &[String], images: &Path) -> Self {
        let folder = folder.display().to_string();
        Self { host, session: None, folder, repos: repos.join(","), images: images.to_path_buf() }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub async fn call(
        &mut self,
        client: ClientConfig,
        name: &str,
        args: Option<Map<String, Value>>,
    ) -> Result<String, String> {
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": client }).to_string();
        let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let call =
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
        let call = call.to_string();
        // A host that restarted forgot the session: open a new one, once.
        let mut reply = Err(io::ErrorKind::NotConnected.into());
        for _ in 0..2 {
            if self.session.is_none() {
                self.post(&init).await.map_err(|_| unreachable(&self.host))?;
                let _ = self.post(initialized).await;
            }
            reply = self.post(&call).await;
            match reply {
                Ok((404, _)) => self.session = None,
                _ => break,
            }
        }
        let (status, messages) = reply.map_err(|_| unreachable(&self.host))?;
        let Some(answer) = messages.into_iter().find(|m| m["id"] == 1) else {
            return Err(format!("The shared board answered {status}."));
        };
        if let Some(error) = answer["error"]["message"].as_str() {
            return Err(error.to_string());
        }
        let result = &answer["result"];
        let text =
            result["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>();
        let text = text.join("\n");
        self.fetch_images(&text).await;
        if result["isError"] == true { Err(text) } else { Ok(text) }
    }

    /// Names are their content's hash, so a copy never goes stale.
    async fn fetch_images(&self, text: &str) {
        let folder = self.images.display().to_string();
        for (at, _) in text.match_indices(&folder) {
            let rest = text[at + folder.len()..].trim_start_matches(['/', '\\']);
            let file: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.').collect();
            let path = self.images.join(&file);
            if !crate::store::valid_image_file(&file) || path.exists() {
                continue;
            }
            let Ok(mut reply) = send(&self.host, "GET", &format!("/images/{file}"), &[], b"").await else { continue };
            let mut bytes = Vec::new();
            if reply.status == 200 && reply.body.read_to_end(&mut bytes).await.is_ok() {
                let _ = crate::home::write_whole(&path, &bytes);
            }
        }
    }

    async fn post(&mut self, line: &str) -> io::Result<(u16, Vec<Value>)> {
        let images = self.images.display().to_string();
        let mut headers = vec![
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
            (FOLDER_HEADER, self.folder.as_str()),
            (REPOS_HEADER, self.repos.as_str()),
            (IMAGES_HEADER, images.as_str()),
            (COMPUTER_HEADER, computer()),
        ];
        if let Some(id) = &self.session {
            headers.push(("mcp-session-id", id));
        }
        let mut reply = send(&self.host, "POST", "/mcp", &headers, line.as_bytes()).await?;
        if let Some(id) = reply.header("mcp-session-id") {
            self.session = Some(id.to_string());
        }
        let mut body = String::new();
        reply.body.read_to_string(&mut body).await?;
        let messages = if body.trim_start().starts_with('{') {
            serde_json::from_str(&body).into_iter().collect()
        } else {
            body.lines().filter_map(|l| serde_json::from_str(l.strip_prefix("data:")?.trim()).ok()).collect()
        };
        Ok((reply.status, messages))
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::ClientCapabilities;

    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(address("192.168.0.12").unwrap(), "192.168.0.12:4242");
        assert_eq!(address(" http://192.168.0.12:4300/ ").unwrap(), "192.168.0.12:4300");
        assert_eq!(address("mac-mini.local").unwrap(), "mac-mini.local:4242");
        assert_eq!(address("fe80::1").unwrap(), "[fe80::1]:4242");
        for bad in ["", "127.0.0.1", "localhost", "localhost:4300", "0.0.0.0", "a b", "evil.com/x", "u@host"] {
            assert!(address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn changes_follow_the_rules() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        let apply = |c| rt.block_on(change(&home, c));

        assert_eq!(apply(Change::On).unwrap(), Net::Sharing);
        assert_eq!(Net::load(&home), Net::Sharing);
        assert!(apply(Change::Join("192.168.0.12".into())).unwrap_err().contains("share off"));
        assert_eq!(apply(Change::Leave).unwrap(), Net::Sharing);
        assert_eq!(apply(Change::Off).unwrap(), Net::Private);
        assert!(!tmp.path().join(FILE).exists());

        std::fs::write(tmp.path().join(FILE), "join 192.168.0.12:4242\n").unwrap();
        assert!(apply(Change::On).unwrap_err().contains("kanzo leave"));
        assert_eq!(apply(Change::Off).unwrap(), Net::Joined("192.168.0.12:4242".into()));
        assert_eq!(apply(Change::Leave).unwrap(), Net::Private);

        std::fs::write(tmp.path().join("config.toml"), "listen = \"0.0.0.0\"\n").unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!(Net::load(&home), Net::Sharing);
        assert!(rt.block_on(change(&home, Change::Join("192.168.0.12".into()))).unwrap_err().contains("share off"));
        assert!(rt.block_on(change(&home, Change::Off)).unwrap_err().contains("config.toml"));
    }

    #[test]
    fn handshake_names_a_client_that_never_initialized() {
        let roots: ClientCapabilities = serde_json::from_value(json!({ "roots": { "listChanged": true } })).unwrap();
        let mut meta = RequestMetaObject::new();
        meta.set_protocol_version(ProtocolVersion::V_2026_07_28);
        meta.set_client_capabilities(roots.clone());
        let init = handshake(None, &meta);
        assert_eq!(init.protocol_version, ProtocolVersion::V_2025_11_25);
        assert_eq!(init.capabilities, roots);
        assert_eq!(init.client_info.name, "agent");

        meta.set_client_info(Implementation::new("claude-code", "2.1.309"));
        assert_eq!(handshake(None, &meta).client_info.name, "claude-code");
    }

    #[test]
    fn handshake_repeats_what_a_client_initialized_with() {
        let client = ClientConfig::new(ClientCapabilities::default(), Implementation::new("codex-mcp-client", "1.0"))
            .with_protocol_version(ProtocolVersion::V_2025_06_18);
        let mut meta = RequestMetaObject::new();
        meta.set_client_info(Implementation::new("from-meta", "1"));
        assert_eq!(handshake(Some(&client), &meta), client);
    }

    #[test]
    fn change_json() {
        assert_eq!(serde_json::to_string(&Change::On).unwrap(), r#"{"action":"on"}"#);
        let join: Change = serde_json::from_str(r#"{"action":"join","host":"pi.local"}"#).unwrap();
        assert!(matches!(join, Change::Join(h) if h == "pi.local"));
    }
}
