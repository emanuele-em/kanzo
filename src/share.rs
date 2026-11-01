//! Sharing the board on your network: `kanzo share on|off`, `kanzo join <host>` and `kanzo leave`.
//!
//! A shared board listens on the network as well as on this computer. A computer that
//! joined one keeps its own board as a window onto it: the board hands the page's
//! requests to the host (`forward`).

use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::io::ReaderStream;

use crate::home::{DEFAULT_PORT, Home};

/// The file in the data folder that says what this computer does on the network:
/// `share`, or `join <host:port>`; none for neither. `kanzo sync` leaves it out.
pub const FILE: &str = "network";

/// Sent by a guest's board, so the host can list who's using it.
pub const COMPUTER_HEADER: &str = "x-kanzo-computer";

/// Larger than any image the board takes.
const BODY_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Net {
    Private,
    Sharing,
    /// Using another computer's board, at `host:port`.
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

/// What the board's settings, `kanzo share`, `kanzo join` and `kanzo leave` ask for,
/// as the board's `/api/network` takes it: `{"action": "join", "host": "192.168.0.12"}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", content = "host", rename_all = "lowercase")]
pub enum Change {
    On,
    Off,
    Join(String),
    Leave,
}

/// Checks `change` against what this computer does now, and saves the outcome. The
/// running board calls this, then listens as it says; without one, the next board does.
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

/// `192.168.0.12` or `mac-mini.local` as `host:port`, with Kanzo's port unless one is given.
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

/// What this computer does on the network, as `/api/network` answers.
#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    state: State,
    /// Once joined, the host's address.
    host: Option<String>,
    /// While sharing, this computer's addresses for `kanzo join`, one per network it is on.
    addresses: Vec<String>,
    /// While sharing, the other computers using the board now.
    guests: Vec<Guest>,
}

/// Another computer using this board.
#[derive(Debug, Serialize, Deserialize)]
pub struct Guest {
    /// Like "Ada's MacBook Air"; `None` for a browser opened straight at this board.
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

/// This computer's addresses, as others pass them to `kanzo join`: one per network it is
/// on, like the home network and a VPN. Which one works depends on where the other computer is.
fn addresses(port: u16) -> Vec<String> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    interfaces
        .iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback() && i.ip().is_ipv4())
        .map(|i| if port == DEFAULT_PORT { i.ip().to_string() } else { format!("{}:{port}", i.ip()) })
        .collect()
}

/// Asks the running board for `change`, which then takes effect at once, or only for its
/// state; with no board running, saves the change for the next one. Also says whether a
/// board is running.
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

/// One HTTP reply. Requests are HTTP/1.0, so the body is all that comes until the host
/// closes the connection, with no chunks to decode.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: BufReader<TcpStream>,
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

/// True when `host` answers like a Kanzo board.
pub async fn is_kanzo(host: &str) -> bool {
    let probe = async {
        let mut reply = send(host, "GET", "/api/ping", &[], b"").await.ok()?;
        let mut body = String::new();
        reply.body.read_to_string(&mut body).await.ok()?;
        Some(reply.status == 200 && body == "kanzo")
    };
    tokio::time::timeout(Duration::from_secs(2), probe).await.ok().flatten().unwrap_or(false)
}

/// This computer's name, as guests send it.
fn computer() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(crate::sync::hostname)
}

fn unreachable(host: &str) -> String {
    format!("The shared board at {host} isn't reachable; run kanzo leave to use this computer's board.")
}

/// Hands one of the page's requests to the board this computer joined, and streams back its reply.
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
        return (StatusCode::BAD_GATEWAY, Json(json!({ "error": unreachable(host) }))).into_response();
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

#[cfg(test)]
mod tests {
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

        // A board config.toml opens to the network is shared, and can't join another.
        std::fs::write(tmp.path().join("config.toml"), "listen = \"0.0.0.0\"\n").unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!(Net::load(&home), Net::Sharing);
        assert!(rt.block_on(change(&home, Change::Join("192.168.0.12".into()))).unwrap_err().contains("share off"));
        assert!(rt.block_on(change(&home, Change::Off)).unwrap_err().contains("config.toml"));
    }

    #[test]
    fn change_json() {
        assert_eq!(serde_json::to_string(&Change::On).unwrap(), r#"{"action":"on"}"#);
        let join: Change = serde_json::from_str(r#"{"action":"join","host":"pi.local"}"#).unwrap();
        assert!(matches!(join, Change::Join(h) if h == "pi.local"));
    }
}
