//! Chromium's network stack, borrowed.
//!
//! Some sites admit browsers and nothing else, and tell which is which by how
//! a connection looks on the wire - Kiwi Farms' Tartarus gate is one, and the
//! cookie it hands out is honoured only from the network stack that earned
//! it. For those, requests and websockets go through moho's own binary run as
//! a helper (`--moho-web-helper`, see moho's src/main/web-helper.ts): a child
//! of this daemon that carries traffic and knows nothing about any chat. All
//! the protocol work stays here.
//!
//! The client tells the daemon how to start it (`setWebHelper`) every time it
//! connects. The helper is started on first use, shared by every account that
//! needs it, and stopped once nothing has used it for a while; it also quits
//! by itself when the daemon goes away, because its stdin closes.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use bytes::Bytes;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// How long the helper stays up with nothing open and nothing asked of it.
const IDLE: Duration = Duration::from_secs(120);
const READY_TIMEOUT: Duration = Duration::from_secs(30);

fn command_slot() -> &'static Mutex<Option<Vec<String>>> {
    static SLOT: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// How to start the helper, as the client says it.
pub fn set_command(command: Vec<String>) {
    if command.is_empty() {
        return;
    }
    *command_slot().lock().unwrap() = Some(command);
    command_known().notify_waiters();
}

fn command_known() -> &'static tokio::sync::Notify {
    static KNOWN: OnceLock<tokio::sync::Notify> = OnceLock::new();
    KNOWN.get_or_init(tokio::sync::Notify::new)
}

/// Whether a helper can be started, waiting up to `limit` for the client to
/// say how.
///
/// At startup the daemon brings its accounts back before moho's window has
/// connected and sent `setWebHelper`, so asking straight away found no
/// helper: the account reported "needs the moho app open", and the health
/// check that follows a failure then called the chat down - for the second
/// or two until the window caught up. A window that is coming says so well
/// inside the limit; a daemon with no window at all still hears "no".
pub async fn available_within(limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        // Registered before checking, so a command set in between is not missed.
        let known = command_known().notified();
        if available() {
            return true;
        }
        if tokio::time::timeout_at(deadline, known).await.is_err() {
            return available();
        }
    }
}

/// Whether a helper can be started at all.
pub fn available() -> bool {
    command_slot().lock().unwrap().is_some() || std::env::var_os("MOHO_WEB_HELPER").is_some()
}

fn command() -> Option<Vec<String>> {
    if let Some(c) = command_slot().lock().unwrap().clone() {
        return Some(c);
    }
    // For a daemon started by hand: a JSON array, as the client would send.
    let raw = std::env::var("MOHO_WEB_HELPER").ok()?;
    serde_json::from_str(&raw).ok()
}

/// Something that happened on a helper websocket.
#[derive(Debug, Clone)]
pub enum WsEvent {
    Open,
    Message(String),
    Closed(u16, String),
}

/// Told how much of a request's body has gone out: sent so far, and the total.
pub type UploadFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

struct Proc {
    writer: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    /// Who hears how an upload is going, by the request's id.
    uploads: Mutex<HashMap<u64, UploadFn>>,
    sockets: Mutex<HashMap<u64, mpsc::UnboundedSender<WsEvent>>>,
    /// Events for a socket the caller has not taken yet: its `open` follows
    /// the answer naming it straight away, before `ws_open` can register it.
    early: Mutex<HashMap<u64, Vec<WsEvent>>>,
    last_used: Mutex<Instant>,
    next_id: AtomicU64,
    /// Set once the helper's output has ended. Its writer stays open until a
    /// write fails, so it is no sign the helper is gone: a call made after
    /// the helper died would go to the channel, and wait for an answer nobody
    /// was left to read.
    gone: AtomicBool,
}

impl Proc {
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    async fn call(&self, msg: Value) -> Result<Value> {
        self.call_with(msg, None).await
    }

    /// A call whose progress is reported: the helper says how far the body has
    /// got as it goes, to `progress`.
    async fn call_with(&self, mut msg: Value, progress: Option<UploadFn>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        msg["id"] = json!(id);
        if let Some(progress) = progress {
            msg["progress"] = json!(true);
            self.uploads.lock().unwrap().insert(id, progress);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.touch();
        // Checked after the answer slot is in place: the reader marks the
        // helper gone before it drops the slots, so a call either finds it
        // gone here or has its slot dropped.
        if self.gone.load(Ordering::SeqCst) || self.writer.send(msg.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            self.uploads.lock().unwrap().remove(&id);
            bail!("the web helper has stopped");
        }
        let answer = rx.await.map_err(|_| anyhow!("the web helper stopped before answering"));
        self.uploads.lock().unwrap().remove(&id);
        let answer = answer?;
        self.touch();
        if answer["ok"].as_bool() != Some(true) {
            bail!("{}", answer["error"].as_str().unwrap_or("the web helper refused"));
        }
        Ok(answer)
    }
}

fn current() -> &'static tokio::sync::Mutex<Option<Arc<Proc>>> {
    static CURRENT: OnceLock<tokio::sync::Mutex<Option<Arc<Proc>>>> = OnceLock::new();
    CURRENT.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// The running helper, started if there is none.
async fn helper() -> Result<Arc<Proc>> {
    let mut slot = current().lock().await;
    if let Some(p) = slot.as_ref() {
        if !p.writer.is_closed() && !p.gone.load(Ordering::SeqCst) {
            return Ok(p.clone());
        }
    }
    let cmd = command().ok_or_else(|| anyhow!("this needs moho running: the web helper is part of the moho app"))?;
    let (program, args) = cmd.split_first().ok_or_else(|| anyhow!("empty web helper command"))?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting the web helper ({program})"))?;
    let mut stdin = child.stdin.take().context("web helper stdin")?;
    let stdout = child.stdout.take().context("web helper stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    // Ready first: until then it cannot take anything.
    let ready = tokio::time::timeout(READY_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            if serde_json::from_str::<Value>(&line).is_ok_and(|v| v["event"] == "ready") {
                return Ok::<bool, std::io::Error>(true);
            }
        }
        Ok(false)
    })
    .await;
    if !matches!(ready, Ok(Ok(true))) {
        bail!("the web helper did not start");
    }

    let (writer, mut outgoing) = mpsc::unbounded_channel::<String>();
    let proc = Arc::new(Proc {
        writer,
        pending: Mutex::new(HashMap::new()),
        uploads: Mutex::new(HashMap::new()),
        sockets: Mutex::new(HashMap::new()),
        early: Mutex::new(HashMap::new()),
        last_used: Mutex::new(Instant::now()),
        next_id: AtomicU64::new(1),
        gone: AtomicBool::new(false),
    });

    // Writing. Ends when every sender is gone or the idle check closes it;
    // dropping stdin is what tells the helper to quit.
    tokio::spawn(async move {
        while let Some(line) = outgoing.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    // Reading, and owning the child: when the helper's output ends, so has it.
    tokio::spawn({
        let proc = proc.clone();
        async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = msg["id"].as_u64() {
                    if let Some(tx) = proc.pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(msg);
                    }
                } else if let Some(req) = msg["req"].as_u64() {
                    // How far a request's body has gone.
                    let report = proc.uploads.lock().unwrap().get(&req).cloned();
                    if let (Some(report), Some(current), Some(total)) = (report, msg["current"].as_u64(), msg["total"].as_u64()) {
                        report(current, total);
                    }
                } else if let Some(ws) = msg["ws"].as_u64() {
                    let event = match msg["event"].as_str() {
                        Some("open") => WsEvent::Open,
                        Some("message") => WsEvent::Message(msg["data"].as_str().unwrap_or_default().to_string()),
                        Some("close") => WsEvent::Closed(msg["code"].as_u64().unwrap_or(1006) as u16, msg["reason"].as_str().unwrap_or_default().to_string()),
                        _ => continue,
                    };
                    let closed = matches!(event, WsEvent::Closed(..));
                    let mut sockets = proc.sockets.lock().unwrap();
                    match sockets.get(&ws) {
                        Some(tx) => {
                            let _ = tx.send(event);
                        }
                        None => proc.early.lock().unwrap().entry(ws).or_default().push(event),
                    }
                    if closed {
                        sockets.remove(&ws);
                    }
                }
            }
            // Gone: whoever was waiting hears so rather than waiting forever.
            proc.gone.store(true, Ordering::SeqCst);
            proc.pending.lock().unwrap().clear();
            for (_, tx) in proc.sockets.lock().unwrap().drain() {
                let _ = tx.send(WsEvent::Closed(1006, "the web helper stopped".into()));
            }
            let _ = child.kill().await;
            tracing::info!("web helper stopped");
        }
    });

    // Stopping it once it has been idle a while.
    tokio::spawn({
        let proc = Arc::downgrade(&proc);
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(p) = proc.upgrade() else { return };
                let idle = p.sockets.lock().unwrap().is_empty()
                    && p.pending.lock().unwrap().is_empty()
                    && p.last_used.lock().unwrap().elapsed() > IDLE;
                if idle {
                    let mut slot = current().lock().await;
                    if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &p)) {
                        *slot = None;
                    }
                    // The writer task ends when the last sender goes - this
                    // one, held only by `p` now - which closes stdin.
                    tracing::info!("web helper idle, stopping it");
                    return;
                }
            }
        }
    });

    tracing::info!("web helper started");
    *slot = Some(proc.clone());
    Ok(proc)
}

/// A response from the helper, shaped like one from a socket of our own.
pub struct Response {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Bytes,
}

/// One HTTP request, cookies and all in `headers`, not following redirects.
///
/// `timeout` is how long the helper waits (a minute when none is given), and
/// `progress` hears how much of the body has gone out, as it goes - for a file
/// somebody is watching go.
pub async fn request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    proxy: Option<&str>,
    timeout: Option<Duration>,
    progress: Option<UploadFn>,
) -> Result<Response> {
    let p = helper().await?;
    let msg = json!({
        "timeoutMs": timeout.map(|t| t.as_millis() as u64),
        "op": "request",
        "method": method,
        "url": url,
        "headers": headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "body": body.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
        "proxy": proxy,
    });
    let answer = p.call_with(msg, progress).await.with_context(|| format!("{method} {url}"))?;
    let mut map = hyper::HeaderMap::new();
    for pair in answer["headers"].as_array().into_iter().flatten() {
        let (Some(k), Some(v)) = (pair[0].as_str(), pair[1].as_str()) else { continue };
        if let (Ok(k), Ok(v)) = (hyper::header::HeaderName::from_bytes(k.as_bytes()), hyper::header::HeaderValue::from_str(v)) {
            map.append(k, v);
        }
    }
    let body = base64::engine::general_purpose::STANDARD.decode(answer["body"].as_str().unwrap_or_default()).unwrap_or_default();
    Ok(Response { status: answer["status"].as_u64().unwrap_or(0) as u16, headers: map, body: Bytes::from(body) })
}

/// A cookie as the helper takes and gives them.
#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub path: String,
}

fn cookies_json(cookies: &[Cookie]) -> Value {
    json!(cookies.iter().map(|c| json!({ "name": c.name, "value": c.value, "path": c.path })).collect::<Vec<_>>())
}

/// Loads `url` in a hidden page holding `cookies` and lets the site's own gate
/// run there; the cookies afterwards, the gate's clearance among them.
pub async fn clear_gate(url: &str, cookies: &[Cookie], proxy: Option<&str>) -> Result<Vec<Cookie>> {
    let p = helper().await?;
    let msg = json!({ "op": "clearGate", "url": url, "cookies": cookies_json(cookies), "proxy": proxy });
    let answer = p.call(msg).await.context("clearing the site's gate")?;
    Ok(answer["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Cookie {
                name: c["name"].as_str()?.to_string(),
                value: c["value"].as_str()?.to_string(),
                path: c["path"].as_str().unwrap_or("/").to_string(),
            })
        })
        .collect())
}

/// A websocket held by the helper.
pub struct WebSocket {
    id: u64,
    proc: Arc<Proc>,
    events: mpsc::UnboundedReceiver<WsEvent>,
}

impl WebSocket {
    pub fn sender(&self) -> WsSender {
        WsSender { id: self.id, writer: self.proc.writer.clone() }
    }

    pub async fn next(&mut self) -> Option<WsEvent> {
        let event = self.events.recv().await;
        self.proc.touch();
        event
    }
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        self.proc.sockets.lock().unwrap().remove(&self.id);
        let _ = self.proc.writer.send(json!({ "op": "wsClose", "ws": self.id }).to_string());
    }
}

/// The sending half of a helper websocket, cheap to clone.
#[derive(Clone)]
pub struct WsSender {
    id: u64,
    writer: mpsc::UnboundedSender<String>,
}

impl WsSender {
    pub fn send(&self, text: &str) -> Result<()> {
        self.writer.send(json!({ "op": "wsSend", "ws": self.id, "data": text }).to_string()).map_err(|_| anyhow!("the web helper has stopped"))
    }

}

/// Opens a websocket from a hidden page at `page`, so it carries that page's
/// origin, holding `cookies`. Waits for it to open.
pub async fn ws_open(url: &str, protocols: &[&str], page: &str, cookies: &[Cookie], proxy: Option<&str>) -> Result<WebSocket> {
    let p = helper().await?;
    let msg = json!({ "op": "wsOpen", "url": url, "protocols": protocols, "page": page, "cookies": cookies_json(cookies), "proxy": proxy });
    // Registered before the answer can arrive: the socket's first events may
    // come straight after it.
    let (tx, rx) = mpsc::unbounded_channel();
    let answer = {
        let id = p.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = msg;
        msg["id"] = json!(id);
        let (atx, arx) = oneshot::channel();
        p.pending.lock().unwrap().insert(id, atx);
        p.writer.send(msg.to_string()).map_err(|_| anyhow!("the web helper has stopped"))?;
        arx.await.map_err(|_| anyhow!("the web helper stopped before answering"))?
    };
    if answer["ok"].as_bool() != Some(true) {
        bail!("opening {url}: {}", answer["error"].as_str().unwrap_or("refused"));
    }
    let id = answer["ws"].as_u64().ok_or_else(|| anyhow!("the web helper gave no socket"))?;
    {
        let mut sockets = p.sockets.lock().unwrap();
        let early = p.early.lock().unwrap().remove(&id).unwrap_or_default();
        let closed = early.iter().any(|e| matches!(e, WsEvent::Closed(..)));
        for event in early {
            let _ = tx.send(event);
        }
        if !closed {
            sockets.insert(id, tx);
        }
    }
    let mut ws = WebSocket { id, proc: p, events: rx };
    match tokio::time::timeout(Duration::from_secs(30), ws.next()).await {
        Ok(Some(WsEvent::Open)) => Ok(ws),
        Ok(Some(WsEvent::Closed(code, reason))) => bail!("{url} closed as it opened ({code} {reason})"),
        Ok(_) => bail!("{url} did not open"),
        Err(_) => bail!("{url} took too long to open"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A helper that says it is ready and dies on its first instruction. Both
    /// tests set this one, as the command is the daemon's single slot.
    fn short_lived_helper() -> Vec<String> {
        vec!["sh".into(), "-c".into(), r#"echo '{"event":"ready"}'; read line"#.into()]
    }

    // A helper that has died is noticed by the next call, which fails at once
    // instead of waiting for an answer nobody will read - an account sat at
    // "signing in..." for good after its helper was killed - and the next
    // caller gets a new helper.
    #[tokio::test]
    async fn a_call_after_the_helper_died_fails_instead_of_hanging() {
        set_command(short_lived_helper());
        let p = helper().await.unwrap();
        assert!(p.call(json!({ "op": "request" })).await.is_err());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !p.gone.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the helper's death went unnoticed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let again = tokio::time::timeout(Duration::from_secs(5), p.call(json!({ "op": "request" }))).await;
        assert!(matches!(again, Ok(Err(_))), "a call to a dead helper hung");
        let next = helper().await.unwrap();
        assert!(!Arc::ptr_eq(&p, &next), "a dead helper was handed out again");
    }

    // A command that arrives while an account is already waiting - the window
    // connecting a moment after the daemon started - is noticed, not missed.
    #[tokio::test]
    async fn a_command_sent_while_waiting_is_heard() {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            set_command(short_lived_helper());
        });
        let started = Instant::now();
        assert!(available_within(Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_secs(2), "it waited out the limit instead of hearing the command");
    }
}
