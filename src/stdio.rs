//! An agent starts `kanzo mcp` once per session and talks to it over stdin and stdout for
//! hours or days. When an update replaces the program, this process runs the new one in its
//! place: same process, same pipes, and the session's handshake handed over, so the agent
//! keeps its connection and gets the new tools.

use std::ffi::OsStr;
use std::io::Cursor;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use rmcp::ServiceExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};

use crate::mcp::Kanzo;
use crate::update::{self, Exe};

/// The agent's `initialize` request and `initialized` notification, one line each, then
/// what it sent that the old program hadn't handled yet.
const HANDOVER: &str = "KANZO_MCP_HANDOVER";

/// Enough of a JSON-RPC message to route it.
#[derive(Deserialize, Default)]
struct Head {
    id: Option<Value>,
    method: Option<String>,
}

impl Head {
    fn of(line: &[u8]) -> Head {
        serde_json::from_slice(line).unwrap_or_default()
    }

    fn is_request(&self) -> bool {
        self.method.is_some() && self.id.is_some()
    }

    fn is_reply(&self) -> bool {
        self.method.is_none() && self.id.is_some()
    }
}

pub async fn serve(server: Kanzo) -> Result<()> {
    let handover = std::env::var_os(HANDOVER).map(OsStringExt::into_vec).unwrap_or_default();
    let (handshake, unread) = split_handover(&handover);
    let (input, mut to_server) = tokio::io::duplex(1 << 16);
    let (from_server, output) = tokio::io::duplex(1 << 16);
    // Requests the agent is still waiting on: the program is replaced only when there are none.
    let waiting = Arc::new(AtomicUsize::new(0));
    // The new program answers the handed-over `initialize` too, but the agent had its answer.
    let answered = Head::of(handshake.split(|b| *b == b'\n').next().unwrap_or_default()).id;
    tokio::spawn(write_replies(from_server, answered, waiting.clone()));
    to_server.write_all(handshake).await?;
    tokio::spawn(read_requests(to_server, handshake.to_vec(), unread.to_vec(), waiting));
    let service = server.serve((input, output)).await?;
    if !handshake.is_empty() {
        let _ = service.peer().notify_tool_list_changed().await;
    }
    service.waiting().await?;
    Ok(())
}

/// The handshake ends after the second line.
fn split_handover(handover: &[u8]) -> (&[u8], &[u8]) {
    let mut ends = handover.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1);
    match ends.nth(1) {
        Some(end) => handover.split_at(end),
        None => (&[], &[]),
    }
}

async fn read_requests(
    mut to_server: DuplexStream,
    mut handshake: Vec<u8>,
    unread: Vec<u8>,
    waiting: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let mut exe = Exe::current();
    let mut shook = handshake.iter().filter(|b| **b == b'\n').count();
    let mut agent = BufReader::new(Cursor::new(unread).chain(tokio::io::stdin()));
    let mut line = Vec::new();
    loop {
        line.clear();
        if agent.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        let head = Head::of(&line);
        if shook < 2 && matches!(head.method.as_deref(), Some("initialize" | "notifications/initialized")) {
            handshake.extend_from_slice(&line);
            shook += 1;
        } else if shook == 2 && exe.as_ref().is_some_and(Exe::changed) {
            // Every answer is out, and stdin was read only up to the end of this line plus
            // what the buffers hold, so the new program gets all the agent sent.
            while waiting.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if let Some(new) = exe.as_ref().and_then(Exe::replacement) {
                let mut handover = handshake.clone();
                handover.extend_from_slice(&line);
                handover.extend_from_slice(agent.buffer());
                let (unread, _) = agent.get_ref().get_ref();
                handover.extend_from_slice(&unread.get_ref()[unread.position() as usize..]);
                let e = update::exec(new, Some((HANDOVER, OsStr::from_bytes(&handover))));
                eprintln!("kanzo: updated, but cannot restart: {e}");
                exe = None;
            }
        }
        if head.is_request() {
            waiting.fetch_add(1, Ordering::SeqCst);
        }
        to_server.write_all(&line).await?;
    }
}

async fn write_replies(
    from_server: DuplexStream,
    mut answered: Option<Value>,
    waiting: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let mut from_server = BufReader::new(from_server);
    let mut stdout = tokio::io::stdout();
    let mut line = Vec::new();
    loop {
        line.clear();
        if from_server.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        let head = Head::of(&line);
        if head.is_reply() && answered.is_some() && head.id == answered {
            answered = None;
            continue;
        }
        stdout.write_all(&line).await?;
        stdout.flush().await?;
        // Only this task lowers the count, so it can't drop to zero between the check and the change.
        if head.is_reply() && waiting.load(Ordering::SeqCst) > 0 {
            waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handover_is_the_handshake_then_the_unread_input() {
        let handover =
            b"{\"id\":0,\"method\":\"initialize\"}\n{\"method\":\"notifications/initialized\"}\n{\"id\":5,\"me";
        let (handshake, unread) = split_handover(handover);
        assert_eq!(handshake.iter().filter(|b| **b == b'\n').count(), 2);
        assert_eq!(unread, b"{\"id\":5,\"me");
        assert_eq!(split_handover(b""), (&b""[..], &b""[..]));
        assert_eq!(split_handover(b"{\"id\":0}\n"), (&b""[..], &b""[..]));
    }

    #[test]
    fn tells_requests_from_replies() {
        assert!(Head::of(br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}}"#).is_request());
        assert!(Head::of(br#"{"jsonrpc":"2.0","id":3,"result":{}}"#).is_reply());
        let note = Head::of(br#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#);
        assert!(!note.is_request() && !note.is_reply());
        assert!(!Head::of(b"not json").is_reply());
    }
}
