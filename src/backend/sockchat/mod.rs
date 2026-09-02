//! "Sneedchat" - SockChat, the XenForo-based chat plugin used by Kiwi Farms
//! (`kiwifarms.st` / its `.onion`). Reachable only through Tor in practice;
//! see `net::tor` for the embedded-Tor/SOCKS5-proxy transport this backend
//! runs on. Ported from sockchat-rs (<https://gitgud.io/jcmoon/sockchat-rs>).
//!
//! A single websocket can only ever be joined to one room at a time - the
//! server has no "subscribe to several" verb, only `/join <room_id>`, which
//! *switches* the one active room. To show several rooms at once, this
//! backend instead opens one persistent websocket per configured room, all
//! sharing a single login/session (see `run` and `run_room`): logging in
//! once, then fanning out, rather than repeating the whole login (and its
//! proof-of-work solve) once per room. Each room's own websocket reconnects
//! independently with exponential backoff, mirroring
//! backend::discord::run_gateway_with_retry's shape.

pub mod auth;
pub mod form;
pub mod http;
pub mod pow;
pub mod protocol;
pub mod smilies;
pub mod totp;

use crate::accounts::{SockChatAccountConfig, SockChatRoom};
use crate::net::tor::{BoxStream, Transport};
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use auth::{Credentials, Session, TwoFactor};
use bytes::Bytes;
use futures::{FutureExt, SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

/// Default hidden service (Kiwi Farms). Clearnet fallback is
/// `kiwifarms.st`, but embedded Tor is used by default regardless of which
/// host is targeted.
pub const DEFAULT_ONION: &str = "kiwifarmsaaf4t2h7gc3dfc5ojhmqruw2nit3uejrpiagrxeuxiyxcyd.onion";
pub const ONION_PORT: u16 = 443;
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Spawns the background task that keeps a Sneedchat account's connection
/// alive - the real-time equivalent of backend::discord::spawn. Called both
/// right after a fresh addSockChatAccount and, from main.rs, for every
/// saved account on daemon startup.
pub fn spawn(state: AppState, config: SockChatAccountConfig) {
    let account_id = config.account_id();
    // Same guard as backend::discord::spawn - guarantees at most one live
    // connection per account (see Runtime::reset_connection's doc comment).
    state.runtime.reset_connection(&account_id);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_with_retry(&state, &config, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Keeps re-establishing the connection for as long as this task lives -
/// same shape as backend::discord::run_gateway_with_retry, including "no
/// give up permanently" - a bad password just keeps failing visibly rather
/// than settling into a silently-stuck state.
async fn run_with_retry(state: &AppState, config: &SockChatAccountConfig, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        let result = std::panic::AssertUnwindSafe(run(state, config, account_id)).catch_unwind().await;
        let detail = match result {
            Ok(Ok(())) => "connection ended".to_string(),
            Ok(Err(e)) => {
                tracing::warn!("sockchat[{account_id}]: {e:#}");
                format!("{e:#}")
            }
            Err(_) => {
                tracing::error!("sockchat[{account_id}]: connection task panicked");
                "internal error (see nobilis logs)".to_string()
            }
        };
        state.runtime.clear_sockchat_senders(account_id);
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        state.runtime.report_progress(state, account_id, &format!("{detail} - reconnecting in {}s...", delay.as_secs()));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

async fn build_transport(state: &AppState, config: &SockChatAccountConfig, account_id: &str) -> Result<Transport> {
    match config.tor_mode.as_str() {
        "proxy" => {
            let proxy = config.proxy.as_deref().ok_or_else(|| anyhow!("tor_mode is \"proxy\" but no proxy URL is configured"))?;
            Transport::socks_from_url(proxy)
        }
        _ => {
            let account_id = account_id.to_string();
            let state2 = state.clone();
            let client = state
                .tor
                .get_or_bootstrap(|msg| state2.runtime.report_progress(&state2, &account_id, msg))
                .await
                .context("bootstrapping Tor")?;
            Ok(Transport::Tor(client))
        }
    }
}

const MAX_WS_REDIRECTS: usize = 3;

/// Opens the chat websocket, following redirects manually (tungstenite
/// doesn't) and presenting the same identity headers a browser on the chat
/// page would - in particular `Origin`, without which the site's anti-bot
/// proxy silently drops the upgrade instead of completing or rejecting it
/// (confirmed live: omitting it just hangs forever with no error at all).
/// Ported from sockchat-rs's `chat/socket.rs::connect`.
async fn open_chat_websocket(transport: &Transport, host: &str, cookie_header: &str) -> Result<WebSocketStream<BoxStream>> {
    let mut url = format!("wss://{host}/chat.ws");
    let origin = format!("https://{host}");

    for _ in 0..=MAX_WS_REDIRECTS {
        let parsed = url::Url::parse(&url).with_context(|| format!("parsing {url}"))?;
        let host = parsed.host_str().with_context(|| format!("{url} has no host"))?.to_string();
        let tls = matches!(parsed.scheme(), "wss" | "https");
        let port = parsed.port().unwrap_or(if tls { 443 } else { 80 });

        let stream = transport.connect(&host, port, tls).await.context("opening chat websocket transport")?;
        let mut request = url.as_str().into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("User-Agent", HeaderValue::from_str(DEFAULT_USER_AGENT)?);
        // Same-origin request, as a browser on the chat page would send -
        // without this the anti-bot proxy in front of the site never
        // completes (or rejects) the upgrade at all.
        headers.insert("Origin", HeaderValue::from_str(&origin)?);
        if !cookie_header.is_empty() {
            headers.insert("Cookie", HeaderValue::from_str(cookie_header).context("cookie header")?);
        }

        match tokio_tungstenite::client_async(request, stream).await {
            Ok((ws, _resp)) => return Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                let status = resp.status();
                if status.as_u16() == pow::GATE_STATUS {
                    bail!("blocked by the anti-bot gate during the chat handshake");
                }
                let location = resp.headers().get(hyper::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
                match (status.is_redirection(), location) {
                    (true, Some(loc)) => {
                        url = resolve_ws_redirect(&url, &loc)?;
                        tracing::info!("sockchat: websocket redirected to {url}");
                    }
                    _ => bail!("chat handshake rejected with HTTP {status}"),
                }
            }
            Err(e) => return Err(anyhow::Error::new(e).context("chat websocket handshake")),
        }
    }
    bail!("chat handshake redirected more than {MAX_WS_REDIRECTS} times (last: {url})")
}

/// Resolves a redirect target against the current URL, normalising an
/// `http(s)` location back to `ws(s)` so the handshake stays a websocket one.
fn resolve_ws_redirect(base: &str, location: &str) -> Result<String> {
    let joined = url::Url::parse(base)?.join(location)?;
    let mut out = joined.clone();
    let scheme = match joined.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        _ => None,
    };
    if let Some(s) = scheme {
        out.set_scheme(s).map_err(|_| anyhow!("could not rewrite scheme of {joined}"))?;
    }
    Ok(out.to_string())
}

/// Logs in once, then opens one permanent connection per configured room.
/// Returns (bails) only if the login itself fails, or if every room task
/// somehow ends - under normal operation this parks forever, since each
/// room task retries its own connection internally; the account only gets
/// fully rebuilt from scratch (fresh transport, fresh login) if this
/// function returns, which `run_with_retry` treats as a failure like any
/// other.
async fn run(state: &AppState, config: &SockChatAccountConfig, account_id: &str) -> Result<()> {
    let transport = build_transport(state, config, account_id).await?;

    let base = format!("https://{}", config.host);
    let session = Session::new(transport.clone(), base, DEFAULT_USER_AGENT.to_string());
    let two_factor = match &config.totp_secret {
        Some(secret) => TwoFactor::Totp(totp::decode_secret(secret).context("stored TOTP secret is not valid base32")?),
        None => TwoFactor::None,
    };
    let creds = Credentials { username: config.username.clone(), password: config.password.clone() };

    state.runtime.report_progress(state, account_id, "logging in...");
    session.ensure_authenticated(&creds, &two_factor).await.context("logging in")?;
    if let Some(uid) = session.user_id() {
        let _ = state.accounts.set_sockchat_user_id(account_id, uid);
    }

    state.runtime.set_own_identity(account_id, &config.username);
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);

    let rooms = effective_rooms(config);
    tracing::info!("sockchat[{account_id}]: authenticated, connecting {} room(s)", rooms.len());

    // Only the first room's connection stores whispers - every room's
    // websocket appears to receive the same whisper pushes (they aren't
    // room-scoped), so storing them on all of them would show each one
    // duplicated once per connected room.
    let handles: Vec<_> = rooms
        .into_iter()
        .enumerate()
        .map(|(i, room)| {
            let state = state.clone();
            let transport = transport.clone();
            let session = session.clone();
            let host = config.host.clone();
            let username = config.username.clone();
            let password = config.password.clone();
            let totp_secret = config.totp_secret.clone();
            let account_id = account_id.to_string();
            let is_primary = i == 0;
            tokio::spawn(async move {
                let creds = Credentials { username, password };
                let two_factor = match &totp_secret {
                    Some(secret) => match totp::decode_secret(secret) {
                        Ok(bytes) => TwoFactor::Totp(bytes),
                        Err(_) => TwoFactor::None,
                    },
                    None => TwoFactor::None,
                };
                run_room(&state, &transport, &session, &creds, &two_factor, &account_id, &host, &room, is_primary).await;
            })
        })
        .collect();

    // Aborting the account-level task (disconnect/removeAccount/a newer
    // spawn() superseding this one) does not automatically cancel these
    // child tasks - tokio only cascades a JoinHandle's own cancellation,
    // not tasks it spawned - so without this guard every room's websocket
    // would leak and keep running orphaned in the background forever.
    struct AbortAllOnDrop(Vec<tokio::task::JoinHandle<()>>);
    impl Drop for AbortAllOnDrop {
        fn drop(&mut self) {
            for h in &self.0 {
                h.abort();
            }
        }
    }
    let _rooms_guard = AbortAllOnDrop(handles);

    // Each room task retries forever internally, so this never resolves on
    // its own - only ever by this whole task being aborted from outside,
    // at which point the guard above cleans up every room task too.
    std::future::pending::<()>().await;
    Ok(())
}

const ROOM_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);
const ROOM_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// One room's own indefinite reconnect loop, reusing the account-wide
/// shared `session` (cloning it is cheap - its cookie jar is Arc-shared,
/// so a refresh done here is immediately visible to every other room's
/// next attempt too).
#[allow(clippy::too_many_arguments)]
async fn run_room(
    state: &AppState,
    transport: &Transport,
    session: &Session,
    creds: &Credentials,
    two_factor: &TwoFactor,
    account_id: &str,
    host: &str,
    room: &SockChatRoom,
    is_primary: bool,
) {
    let mut delay = ROOM_RECONNECT_INITIAL_DELAY;
    loop {
        let result = std::panic::AssertUnwindSafe(run_room_once(state, transport, session, account_id, host, room, is_primary)).catch_unwind().await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("sockchat[{account_id}] room {} ({}): {e:#}", room.id, room.name);
                // A stale session is the most likely reason one specific
                // room starts failing while the others keep working -
                // refresh before retrying rather than hammering the same
                // rejected cookies every backoff cycle.
                if let Err(re) = session.refresh(creds, two_factor).await {
                    tracing::warn!("sockchat[{account_id}] room {}: session refresh failed: {re:#}", room.id);
                }
            }
            Err(_) => tracing::error!("sockchat[{account_id}] room {} ({}): task panicked", room.id, room.name),
        }
        // No cleanup call here - run_room_once's own RemoveSenderOnDrop
        // guard already ran (on every return path, including a panic) by
        // the time this line is reached, and it's the one that actually
        // knows which specific sender to remove (see its own doc comment
        // on why an unconditional remove-by-key here would be unsafe).
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(ROOM_RECONNECT_MAX_DELAY);
    }
}

async fn run_room_once(state: &AppState, transport: &Transport, session: &Session, account_id: &str, host: &str, room: &SockChatRoom, is_primary: bool) -> Result<()> {
    let cookie_header = session.cookie_header().unwrap_or_default();
    let ws = open_chat_websocket(transport, host, &cookie_header).await?;
    let (sink, mut incoming) = ws.split();

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let forwarder = tokio::spawn(async move {
        let mut sink = sink;
        while let Some(text) = out_rx.recv().await {
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _forwarder_guard = AbortOnDrop(forwarder);

    state.runtime.insert_sockchat_sender(account_id, room.id, out_tx.clone());
    struct RemoveSenderOnDrop<'a> {
        state: &'a AppState,
        account_id: &'a str,
        room: u32,
        sender: tokio::sync::mpsc::UnboundedSender<String>,
    }
    impl Drop for RemoveSenderOnDrop<'_> {
        fn drop(&mut self) {
            self.state.runtime.remove_sockchat_sender(self.account_id, self.room, &self.sender);
        }
    }
    let _sender_guard = RemoveSenderOnDrop { state, account_id, room: room.id, sender: out_tx.clone() };

    out_tx.send(format!("/join {}", room.id)).map_err(|_| anyhow!("chat socket closed before joining"))?;
    let buffer_name = format!("#{}", room.name);
    // Created eagerly rather than waiting for the first live message - a
    // quiet room would otherwise show no buffer/tab at all despite being
    // connected and joined.
    state.runtime.ensure_buffer(state, account_id, &buffer_name, "channel");
    // The server follows a join with the room's whole roster, so anything left
    // from a previous connection would only be stale.
    state.runtime.set_presence(&crate::model::buffer_id(account_id, &buffer_name), serde_json::json!([]));
    tracing::info!("sockchat[{account_id}]: joined room {} ({})", room.id, room.name);

    loop {
        // Bounded, not a bare `.await` - a Tor circuit that's gone quietly
        // bad (no clean Close frame, no error, just nothing ever arriving
        // again) would otherwise leave this loop parked forever with no way
        // to notice the outage at all, let alone reconnect from it. Any
        // inbound traffic at all (a real message, or even a Ping/Pong the
        // `Some(Ok(_)) => continue` arm below swallows) resets this, so a
        // genuinely quiet-but-alive room doesn't get penalized - this only
        // fires when literally nothing has come through the socket.
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, incoming.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => text.to_string(),
            Ok(Some(Ok(WsMessage::Close(frame)))) => bail!("connection closed: {frame:?}"),
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => bail!("websocket error: {e}"),
            Ok(None) => bail!("connection ended unexpectedly"),
            Err(_) => bail!("no activity for {}s, assuming the connection is dead", IDLE_TIMEOUT.as_secs()),
        };
        handle_frame(state, &session.http, host, account_id, &buffer_name, is_primary, &frame).await?;
    }
}

/// How long to go without any inbound websocket activity (a message, or
/// even just a Ping/Pong) before giving up on this connection and forcing a
/// reconnect - see the read loop above. Comfortably above any plausible
/// server-side heartbeat interval, so this only ever fires for a genuinely
/// stalled connection, not a quiet room.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

async fn handle_frame(state: &AppState, http: &http::HttpClient, host: &str, account_id: &str, buffer_name: &str, is_primary: bool, frame: &str) -> Result<()> {
    let resp = protocol::ServerResponse::parse(frame);

    if !resp.users_joined.is_empty() || !resp.users_left.is_empty() {
        update_roster(state, account_id, buffer_name, &resp.users_joined, &resp.users_left);
    }

    if let Some(text) = &resp.plaintext {
        tracing::debug!("sockchat[{account_id}]: {text}");
        let lower = text.to_lowercase();
        if lower.contains("cannot join") || lower.contains("session") {
            bail!("server rejected the connection: {text}");
        }
        return Ok(());
    }

    // The site pushes our live permission set for the room on join (and,
    // it seems, whenever it changes) - this is the one on-wire signal that
    // distinguishes "authenticated" from "silently downgraded to a guest",
    // which a reconnect using a since-expired session cookie produces:
    // the websocket handshake itself succeeds either way (the anti-bot
    // proxy in front of it doesn't check the cookie, only the chat
    // application layer does), so there's no error to bail out on there.
    // Bailing here instead - this account always has real credentials, so
    // can_send=false can only mean the session needs a fresh login, and
    // routes through the exact same reconnect-with-refresh path a real
    // connection error already takes (see run_room's catch, which calls
    // session.refresh() before the next attempt).
    if let Some(perms) = &resp.perms {
        if !perms.can_send {
            bail!("server reports we can only view, not send (likely viewing as a guest) - session needs to be refreshed");
        }
    }

    let buffer_id = crate::model::buffer_id(account_id, buffer_name);

    // A batch delete: uuids removed elsewhere (e.g. by a moderator),
    // arriving separately from any message object.
    for uuid in &resp.deleted_uuids {
        state.runtime.delete_message(state, &buffer_id, uuid);
    }

    for m in &resp.messages {
        if m.is_deleted() {
            state.runtime.delete_message(state, &buffer_id, &m.message_uuid);
            continue;
        }
        let body = protocol::unescape_html(&m.message_raw);
        if body.is_empty() {
            continue;
        }
        let avatar_url = match &m.author.avatar_url {
            Some(raw) => cached_avatar_path(http, host, &m.author.id, raw).await,
            None => None,
        };

        // A message belonging to no room is not a room's message. The site
        // delivers a whisper down the same channel as everything else and
        // marks it only by that absence - which is why whispers arrived as
        // nothing at all: the separate `Whisper` frame this used to wait for
        // is not how they come.
        //
        // Handled here rather than only in that frame, and both are kept,
        // because one of them is known to work and the other is what the
        // protocol notes have always said. Every room's socket sees the same
        // whisper, so only one connection records it.
        if m.room_id.is_none() {
            if !is_primary {
                continue;
            }
            tracing::debug!("sockchat[{account_id}]: whisper from {} (no room_id)", m.author.username);
            let msg_id = (!m.message_uuid.is_empty()).then(|| m.message_uuid.clone());
            for buffer in record_whisper(state, account_id, &m.author.username, &body, msg_id, avatar_url, true) {
                if !m.message_uuid.is_empty() {
                    spawn_attachment_resolve(state.clone(), http.clone(), buffer, m.message_uuid.clone(), body.clone());
                }
            }
            continue;
        }
        // There's no separate "edit" event on this wire - the server just
        // re-sends the same message_uuid with a bumped edit date. Try to
        // update an already-stored row first; only if that uuid was never
        // actually stored (e.g. this is the first time we've ever seen it,
        // and it just happens to already carry prior edit history) does
        // it fall through to being recorded as an ordinary new message.
        if m.message_edit_date > 0 && !m.message_uuid.is_empty() && state.runtime.update_message(state, &buffer_id, &m.message_uuid, &body, &[], &[]) {
            continue;
        }
        // Always attributed to the room this specific connection is
        // joined to, rather than trusting the wire's own room_id - this
        // connection should only ever see traffic for its own room anyway.
        let msg_id = (!m.message_uuid.is_empty()).then(|| m.message_uuid.clone());
        let is_new = state.runtime.record_message(state, account_id, buffer_name, "channel", &m.author.username, &body, false, "chat", None, msg_id, false, avatar_url, Vec::new(), Vec::new(), None);
        // Only for messages with a real, stable id - nothing to re-target
        // a later body-patch at otherwise (see spawn_attachment_resolve) -
        // and only for ones not seen before. A reconnect replays the room's
        // recent history, and looking every attachment in it up again is a
        // request per message, over Tor, for answers already on disk.
        if is_new && !m.message_uuid.is_empty() {
            spawn_attachment_resolve(state.clone(), http.clone(), buffer_id.clone(), m.message_uuid.clone(), body);
        }
    }

    if is_primary {
        if let Some(w) = &resp.whisper {
            let body = protocol::unescape_html(&w.message_raw);
            if !body.is_empty() {
                let avatar_url = match &w.author.avatar_url {
                    Some(raw) => cached_avatar_path(http, host, &w.author.id, raw).await,
                    None => None,
                };
                let msg_id = (!w.message_uuid.is_empty()).then(|| w.message_uuid.clone());
                for buffer in record_whisper(state, account_id, &w.author.username, &body, msg_id, avatar_url, true) {
                    if !w.message_uuid.is_empty() {
                        spawn_attachment_resolve(state.clone(), http.clone(), buffer, w.message_uuid.clone(), body.clone());
                    }
                }
            }
        }
    }

    Ok(())
}

/// Where cached avatar images live - a genuine cache (every file here is
/// re-fetchable from the site given its raw_avatar_url, see
/// cached_avatar_path), so XDG_CACHE_HOME rather than the app's own
/// config dir (see main.rs's `parse_args`) it used to sit under - main.rs's
/// migrate_caches_to_xdg_cache_dir moves any pre-existing directory here
/// once at startup.
fn avatar_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache")).join("nobilis").join("sockchat-avatars")
}

/// Cap on the avatar cache's total size on disk - without this, every
/// distinct poster ever seen across every room accumulates its own
/// permanently-cached file forever (see cached_avatar_path's own doc
/// comment: avatars are never re-checked once cached), which over months
/// of use in busy rooms is genuinely unbounded growth, not a one-time cost.
const AVATAR_CACHE_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// How often to sweep the cache back under the cap - called from main.rs,
/// once per daemon process regardless of how many Sneedchat accounts are
/// configured (a per-account timer would just repeat the same sweep).
pub const AVATAR_CACHE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Evicts the oldest-written files in `dir` until it's back under
/// `max_bytes`. Oldest-by-mtime rather than true LRU (a cache hit doesn't
/// bump a file's timestamp) - simpler, no extra dependency, and "the files
/// nobody's referenced recently age out first" is a reasonable
/// approximation of LRU for this purpose anyway. Shared by the avatar and
/// attachment caches below - same growth problem (a permanent per-item
/// file that's never re-checked once cached, see cached_avatar_path's and
/// resolve_attachment's own doc comments), same fix.
pub async fn sweep_cache_dir(dir: &std::path::Path, max_bytes: u64, label: &str) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };

    let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        total += meta.len();
        files.push((entry.path(), meta.len(), mtime));
    }

    if total <= max_bytes {
        return;
    }

    files.sort_by_key(|(_, _, mtime)| *mtime);
    let overage = total - max_bytes;
    let mut to_free = overage;
    let mut removed = 0usize;
    for (path, size, _) in files {
        if to_free == 0 {
            break;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            to_free = to_free.saturating_sub(size);
            removed += 1;
        }
    }
    tracing::info!(
        "sockchat: {label} cache was {}MB over its {}MB cap, evicted {removed} oldest file(s)",
        overage / 1024 / 1024,
        max_bytes / 1024 / 1024
    );
}

pub async fn sweep_avatar_cache() {
    sweep_cache_dir(&avatar_cache_dir(), AVATAR_CACHE_MAX_BYTES, "avatar").await;
}

/// Where cached attachment images/video live - same `~/.cache/nobilis`
/// root every other protocol's media/avatar cache uses (see matrix/mod.rs's
/// media_cache_dir), rather than the OS temp dir this used to sit under -
/// one place to look, one sweep loop, one size cap convention for
/// "downloaded content this app can always re-fetch on a cache miss"
/// regardless of which backend produced it. main.rs's
/// migrate_caches_to_xdg_cache_dir moves any pre-existing `/tmp` directory
/// here once at startup.
fn attachment_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache")).join("nobilis").join("sockchat-attachments")
}

/// Bigger than the avatar cap - full images/clips run much larger than
/// small profile pictures.
const ATTACHMENT_CACHE_MAX_BYTES: u64 = 250 * 1024 * 1024;

pub async fn sweep_attachment_cache() {
    sweep_cache_dir(&attachment_cache_dir(), ATTACHMENT_CACHE_MAX_BYTES, "attachment").await;
}

/// The file extension an image's own leading bytes call for, or None if the
/// format isn't recognised.
///
/// Content, not the URL: the site hands out avatar links ending in .jpg and
/// its CDN answers them with WebP, so the name a link implies is not evidence
/// of what arrived.
fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        // RIFF is a container; the form is named four bytes into the payload.
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        // Likewise ISO-BMFF: AVIF, HEIC and MP4 share the ftyp box and are
        // told apart by the brand that follows it.
        [_, _, _, _, b'f', b't', b'y', b'p', b, r, a, n, ..] => match [*b, *r, *a, *n] {
            [b'a', b'v', b'i', b'f'] | [b'a', b'v', b'i', b's'] => Some("avif"),
            [b'h', b'e', b'i', b'c'] | [b'h', b'e', b'i', b'x'] => Some("heic"),
            _ => Some("mp4"),
        },
        _ => None,
    }
}

/// Every extension sniff_image_ext can produce, plus the ones older caches
/// were written with - what a lookup has to consider, since the name a
/// cached avatar ended up under depends on what its bytes turned out to be.
const CACHED_AVATAR_EXTS: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "avif", "heic", "mp4"];

/// An existing cache entry for this user, if one is there and is what its
/// name claims.
///
/// The extension check is what lets a cache written before content sniffing
/// heal itself: an entry whose bytes disagree with its name is treated as
/// absent, so it is re-fetched and rewritten under the right one. Only the
/// first few bytes are read, so this stays cheap enough to run per message.
async fn cached_avatar_file(dir: &std::path::Path, user_id: &str) -> Option<std::path::PathBuf> {
    for ext in CACHED_AVATAR_EXTS {
        let path = dir.join(format!("{user_id}.{ext}"));
        let Ok(mut file) = tokio::fs::File::open(&path).await else { continue };
        let mut head = [0u8; 16];
        use tokio::io::AsyncReadExt;
        let Ok(n) = file.read(&mut head).await else { continue };
        match sniff_image_ext(&head[..n]) {
            // "jpeg" and "jpg" are the same thing under two names.
            Some(actual) if actual == ext || (actual == "jpg" && ext == "jpeg") => return Some(path),
            // Unrecognised bytes: nothing better to go on, so keep it.
            None => return Some(path),
            Some(_) => {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    None
}

/// Resolves a wire `avatar_url` (a relative path or an absolute URL)
/// against `host`, fetches it through the same Tor-routed client used for
/// everything else, and caches it to a local file - the frontend's plain
/// `Image` element has no route through Tor (or to a `.onion` host at
/// all) on its own, so handing back the original remote URL would simply
/// never load. A `file://` path sidesteps that, the same trick already
/// used for Discord's QR login code.
///
/// Cached permanently per user id for this daemon's lifetime, not
/// re-checked on every message - an avatar changing later isn't picked
/// up until the cache file is manually cleared, the same precedent
/// backend/discord.rs's own avatar_url handling already set ("captured
/// once at login... not refreshed on later reconnects").
async fn cached_avatar_path(http: &http::HttpClient, host: &str, user_id: &str, raw_avatar_url: &str) -> Option<String> {
    if raw_avatar_url.is_empty() {
        return None;
    }
    let url = if raw_avatar_url.starts_with('/') { format!("https://{host}{raw_avatar_url}") } else { raw_avatar_url.to_string() };

    let url_ext = url.rsplit('.').next().filter(|e| e.len() <= 4 && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or("jpg");
    let dir = avatar_cache_dir();

    if let Some(path) = cached_avatar_file(&dir, user_id).await {
        return Some(format!("file://{}", path.display()));
    }

    // Bounded rather than left open-ended: this runs inline in the room's
    // own read loop (see handle_frame), so a slow/stuck fetch (a fresh Tor
    // circuit needed for a host this session hasn't hit before, a route
    // that just hangs) would otherwise stall that entire room's message
    // processing until it resolved.
    let Ok(fetch) = tokio::time::timeout(std::time::Duration::from_secs(15), http.get_bytes(&url)).await else {
        tracing::debug!("sockchat: avatar fetch for user {user_id} timed out");
        return None;
    };

    match fetch {
        Ok((status, bytes)) if (200..300).contains(&status) && !bytes.is_empty() => {
            if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                tracing::debug!("sockchat: creating avatar cache dir: {e}");
                return None;
            }
            // Name the file after what it actually is. The site's avatar URLs
            // end in .jpg while the CDN transparently serves WebP, so trusting
            // the URL wrote WebP into a .jpg - which anything that dispatches
            // on extension then refuses to load.
            let ext = sniff_image_ext(&bytes).unwrap_or(url_ext);
            let path = dir.join(format!("{user_id}.{ext}"));
            if let Err(e) = tokio::fs::write(&path, &bytes).await {
                tracing::debug!("sockchat: caching avatar for user {user_id}: {e}");
                return None;
            }
            Some(format!("file://{}", path.display()))
        }
        Ok((status, _)) => {
            tracing::debug!("sockchat: avatar fetch for user {user_id} returned HTTP {status}");
            None
        }
        Err(e) => {
            tracing::debug!("sockchat: fetching avatar for user {user_id}: {e}");
            None
        }
    }
}

/// Recognized image/video file extensions for attachment links - shares
/// the client's own media-detection list, since this exists for the same
/// reason: deciding whether a link is worth embedding.
const ATTACHMENT_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "mp4", "webm", "mov", "mkv"];

/// Finds the first Kiwi Farms/XenForo attachment URL in `text` - either
/// host, clearnet or onion (see DEFAULT_ONION) - shaped like
/// `/attachments/<name>-<ext>.<id>/`. Returns the exact matched substring
/// (so callers can `body.replace()` it verbatim) plus the id and
/// extension, both embedded in the URL's own slug: XenForo builds it by
/// replacing the original filename's "." with "-" (so
/// "sam-consent-accident.webp" becomes the slug
/// "sam-consent-accident-webp") and appending ".<attachment id>/". No
/// network round-trip is needed just to classify the link as media - only
/// to actually fetch it (see resolve_attachment below).
fn find_attachment_url(text: &str) -> Option<(&str, &str, &str)> {
    for host in [KIWIFARMS_CLEARNET_HOST, DEFAULT_ONION] {
        let marker_start = format!("https://{host}/attachments/");
        let Some(start) = text.find(&marker_start) else { continue };
        let seg_start = start + marker_start.len();
        let rest = &text[seg_start..];
        let seg_len = rest.find(|c: char| c.is_whitespace() || matches!(c, '<' | '[' | ']')).unwrap_or(rest.len());
        let end = seg_start + seg_len;
        let segment = text[seg_start..end].trim_end_matches('/');

        let Some(dot) = segment.rfind('.') else { continue };
        let (name_part, id_part) = (&segment[..dot], &segment[dot + 1..]);
        if id_part.is_empty() || !id_part.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(dash) = name_part.rfind('-') else { continue };
        let ext = &name_part[dash + 1..];
        if !ATTACHMENT_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        return Some((&text[start..end], id_part, ext));
    }
    None
}

const KIWIFARMS_CLEARNET_HOST: &str = "kiwifarms.st";

/// Kiwi Farms' clearnet edge is frequently unstable - the reason this
/// backend runs over Tor by default in the first place - and, separately
/// from that, tends to mishandle Tor exit traffic even when it IS up,
/// where every other fetch this backend makes already goes out over Tor
/// regardless of hostname. Routing straight to the onion address instead
/// (a real hidden-service listener, not something reached via a Tor exit
/// node at all) sidesteps both problems, so any detected attachment link
/// is rewritten to that host before fetching - not just Tor-routed under
/// whatever host the message happened to contain.
///
/// Spawned as its own task rather than run inline in handle_frame's
/// per-room read loop: the message is recorded and shown immediately with
/// its original link, and the body only gets patched in place (via
/// Runtime::update_message_body_only, which - unlike a real edit - leaves
/// the "(edited)" label alone) once the local copy is ready, re-rendering
/// the same message with a working embed a moment later instead of
/// stalling the whole room's message processing on one slow Tor fetch.
fn spawn_attachment_resolve(state: AppState, http: http::HttpClient, buffer_id: String, msg_id: String, body: String) {
    let Some((matched, id, ext)) = find_attachment_url(&body) else { return };
    let (matched, id, ext) = (matched.to_string(), id.to_string(), ext.to_ascii_lowercase());

    tokio::spawn(async move {
        let dir = attachment_cache_dir();
        let path = dir.join(format!("{id}.{ext}"));

        let local_url = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            Some(format!("file://{}", path.display()))
        } else {
            let onion_url = matched.replacen(KIWIFARMS_CLEARNET_HOST, DEFAULT_ONION, 1);
            match tokio::time::timeout(std::time::Duration::from_secs(30), http.get_bytes(&onion_url)).await {
                Ok(Ok((status, bytes))) if (200..300).contains(&status) && !bytes.is_empty() => {
                    if tokio::fs::create_dir_all(&dir).await.is_ok() && tokio::fs::write(&path, &bytes).await.is_ok() {
                        Some(format!("file://{}", path.display()))
                    } else {
                        tracing::debug!("sockchat: caching attachment {id} failed");
                        None
                    }
                }
                Ok(Ok((status, _))) => {
                    tracing::debug!("sockchat: attachment {id} fetch returned HTTP {status}");
                    None
                }
                Ok(Err(e)) => {
                    tracing::debug!("sockchat: fetching attachment {id}: {e}");
                    None
                }
                Err(_) => {
                    tracing::debug!("sockchat: attachment {id} fetch timed out");
                    None
                }
            }
        };

        if let Some(local_url) = local_url {
            let new_body = body.replace(&matched, &local_url);
            state.runtime.update_message_body_only(&state, &buffer_id, &msg_id, &new_body);
        }
    });
}

/// Shared by sendMessage/editMessage/deleteMessage's SockChat branches -
/// looks up which configured room a buffer belongs to and returns that
/// room's own permanent connection (see `run_room`). There's no "switch
/// active room" step needed since every configured room is already
/// connected simultaneously.
fn room_sender_for_buffer(state: &AppState, account_id: &str, buffer_name: &str) -> Result<tokio::sync::mpsc::UnboundedSender<String>> {
    let cfg = state.accounts.get_sockchat(account_id).ok_or_else(|| anyhow!("no such account"))?;
    let room_name = buffer_name.strip_prefix('#').unwrap_or(buffer_name);
    let rooms = effective_rooms(&cfg);
    let room = rooms.iter().find(|r| r.name == room_name).ok_or_else(|| anyhow!("\"{buffer_name}\" isn't one of this account's configured rooms"))?;
    state.runtime.sockchat_sender(account_id, room.id).ok_or_else(|| anyhow!("not currently connected to this room"))
}

/// Puts a whisper in front of whoever is reading, whichever room that is.
///
/// A whisper belongs to no room, and the site shows it wherever you happen to
/// be looking. Nothing here can know which room a frontend has open, so it is
/// recorded in every room this account is in rather than one being picked and
/// hoped for.
///
/// That replaces a buffer of its own, which is what the report called "a DM
/// window". The separate buffer was the same mistake in a worse form: a
/// private message you only see by going somewhere else to look, and a
/// conversation that pulled you out of the room you were in to have it.
///
/// Storing the same message under several buffers is fine and not a
/// duplicate - the store keys on (buffer, id), so each room holds it once and
/// none of them holds it twice however many sockets saw it.
#[allow(clippy::too_many_arguments)]
fn record_whisper(
    state: &AppState,
    account_id: &str,
    from: &str,
    body: &str,
    msg_id: Option<String>,
    avatar_url: Option<String>,
    // `notify`: whether this one should raise an alert. True for a whisper
    // somebody sent us; false for one we sent, which needs no telling.
    notify: bool,
) -> Vec<String> {
    let Some(cfg) = state.accounts.get_sockchat(account_id) else { return Vec::new() };
    let mut fresh = Vec::new();
    for (index, room) in effective_rooms(&cfg).into_iter().enumerate() {
        // Highlighted in the first room only, and highlighting is what raises
        // an alert - so one whisper is one notification however many rooms it
        // is written into. The copies are still whispers and are still drawn
        // as whispers, because the client colours them by kind rather than by
        // highlight; what they are not is a second time somebody's desktop
        // says the same thing.
        let alert = notify && index == 0;
        let is_new = state.runtime.record_message(
            state,
            account_id,
            &room.name,
            "channel",
            from,
            body,
            false,
            "whisper",
            None,
            msg_id.clone(),
            alert,
            avatar_url.clone(),
            Vec::new(),
            Vec::new(),
            None,
        );
        if is_new {
            fresh.push(crate::model::buffer_id(account_id, &room.name));
        }
    }
    fresh
}

/// Any live room connection for this account.
///
/// A whisper belongs to no room, so it does not matter which carries it - but
/// there has to be one, since the only way to say anything at all is over a
/// room's socket.
fn any_room_sender(state: &AppState, account_id: &str) -> Result<tokio::sync::mpsc::UnboundedSender<String>> {
    let cfg = state.accounts.get_sockchat(account_id).ok_or_else(|| anyhow!("no such account"))?;
    effective_rooms(&cfg)
        .iter()
        .find_map(|r| state.runtime.sockchat_sender(account_id, r.id))
        .ok_or_else(|| anyhow!("not connected to Sneedchat"))
}

/// The rooms this account actually talks in.
///
/// An account with none configured still connects - to #general, which is
/// where a Sneedchat session lands by default and what makes a freshly added
/// account usable before anybody has been to Settings to choose rooms.
///
/// Shared with the send path deliberately. The connect side had this fallback
/// and the send side read the stored list directly, so an account with no
/// rooms configured connected to #general, received messages there, and then
/// refused to send with "#general isn't one of this account's configured
/// rooms" - true of the stored config and plainly untrue of the connection
/// the user was looking at. One definition of "which rooms" means the two
/// cannot disagree again.
fn effective_rooms(config: &SockChatAccountConfig) -> Vec<SockChatRoom> {
    if config.rooms.is_empty() {
        vec![SockChatRoom { id: 1, name: "general".to_string() }]
    } else {
        config.rooms.clone()
    }
}

/// Sends a message, optionally as a reply to someone.
///
/// Sneedchat has no reply field: the site's own client answers somebody by
/// opening the message with an `@Name,` mention, which is what its users and
/// its own notification rules recognise as being replied to. So a reply here
/// is that mention, added by the daemon rather than left to each frontend to
/// know the convention - and skipped when the message already opens with it,
/// so replying twice to the same person does not stack them up.
pub fn send_message(state: &AppState, account_id: &str, buffer_name: &str, body: &str, reply_to: Option<&str>) -> Result<()> {
    // Typed rather than chosen from a menu, which is what somebody used to the
    // site will do. Routed through the same path either way, so it is recorded
    // as a whisper here too - passed straight through it would be a real
    // whisper that this client never saw, since the site echoes none back.
    if let Some((target, text)) = protocol::parse_whisper_command(body) {
        return send_whisper(state, account_id, &target, &text);
    }
    let body = as_reply(body, reply_to);
    let Some(text) = protocol::prepare_outgoing(&body) else { return Ok(()) };
    let sender = room_sender_for_buffer(state, account_id, buffer_name)?;
    sender.send(text).map_err(|_| anyhow!("chat socket closed"))?;
    Ok(())
}

/// Sends a private message to one person.
///
/// Goes out on whichever room connection is to hand: a whisper is not part of
/// any room's conversation, so any open socket carries it.
///
/// Recorded locally as well as sent. Unlike a room message, the site does not
/// echo a whisper back to whoever sent it, so without this you would see only
/// one side of your own conversation.
///
/// Recorded from *us*, with the target written into the line. There is no
/// field on a message for "and this went to Someone", and a whisper filed
/// under the recipient's name reads as something they said - which is what it
/// used to do. The `@name` is what the site puts there too, so the line reads
/// the way the same whisper reads on the site.
pub fn send_whisper(state: &AppState, account_id: &str, target: &str, body: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        bail!("no one to whisper to");
    }
    let Some(text) = protocol::prepare_outgoing(body) else { return Ok(()) };
    let sender = any_room_sender(state, account_id)?;
    sender
        .send(protocol::prepare_whisper(target, &text))
        .map_err(|_| anyhow!("chat socket closed"))?;

    let me = state
        .accounts
        .get_sockchat(account_id)
        .map(|c| c.display_name.filter(|n| !n.is_empty()).unwrap_or(c.username))
        .unwrap_or_default();
    let at = if target.starts_with('@') { "" } else { "@" };
    record_whisper(state, account_id, &me, &format!("{at}{target} {text}"), None, None, false);
    Ok(())
}

/// Opens a message with the mention Sneedchat treats as a reply.
fn as_reply(body: &str, reply_to: Option<&str>) -> String {
    match reply_to {
        Some(nick) if !nick.is_empty() && !body.trim_start().starts_with(&format!("@{nick},")) => {
            format!("@{nick}, {}", body.trim_start())
        }
        _ => body.to_string(),
    }
}

/// postimg.cc's own anonymous upload endpoint - undocumented (its
/// advertised "official API" at api.postimages.org requires registration
/// and returns an empty body when probed anonymously), reverse-engineered
/// from a real browser session's HAR capture instead. The plain form POST
/// a browser submits (`postimages.org/json`, the same fields the site's own
/// upload page sends - see the `fields` array below) 403s with "Automated
/// uploads are not allowed via the website" unless `Origin`/`Referer` also
/// look like they came from the site itself (confirmed live: adding just
/// those two headers, nothing else, is what turns the 403 into a real
/// upload) - see `post_multipart`'s `extra_headers`.
const POSTIMG_UPLOAD_URL: &str = "https://postimages.org/json";
const POSTIMG_ORIGIN: &str = "https://postimages.org";
/// postimg.cc's own free-tier cap, read directly out of its upload page's
/// JS init (`maxFilesize:33554432`) - checked here for the same "fail fast
/// with a clear reason" rationale qu.ax's old cap had.
const POSTIMG_MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

#[derive(serde::Deserialize)]
struct PostimgUploadResponse {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<PostimgErrorBody>,
}

#[derive(serde::Deserialize)]
struct PostimgErrorBody {
    message: String,
}

/// postimg.cc only accepts images (its own `accept` list is image formats
/// plus PDF/postscript/raw-camera formats, no video) - unlike qu.ax, which
/// hosted anything. `None` for a file extension this project's own
/// attachment picker would otherwise let through (see ATTACHMENT_EXTS)
/// means "ask for an image instead" rather than guessing a content type
/// postimg.cc would reject anyway.
/// Every extension postimg.cc will take, and what to send it as.
///
/// One table rather than a match, because two things need it: this, to fill
/// in the multipart content type, and the host listing a client draws its
/// menu from. They were separate before, and disagreed - the listing counted
/// avif an image while this refused it, so an .avif routed to postimg was
/// accepted by the menu and rejected by the site.
pub const POSTIMG_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
];

pub fn guess_postimg_content_type(file_name: &str) -> Option<&'static str> {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    POSTIMG_TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t)
}

/// Matches the site's own `new Date().getTime()+Math.random().toString().
/// substring(1)` (read out of its upload page's JS) closely enough to pass
/// as a real one - a millisecond timestamp directly followed by a
/// "0."-stripped random fraction, e.g. "1786561259146.7777739930052086".
/// Nothing observed depends on the exact shape (it reads as a per-upload
/// de-dup/session key, not a validated token), but there's no reason to
/// diverge from it either.
fn postimg_upload_session() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let frac = format!("{:.16}", rand::random::<f64>());
    format!("{millis}{}", frac.trim_start_matches('0'))
}

/// postimg.cc's result page (`https://postimg.cc/<slug>/<hash>`) embeds a
/// `<div class="col" data-image="..." data-hash="..." data-hotlink="..."
/// data-name="..." data-ext="...">` for the just-uploaded image - `data-
/// name`/`data-ext` are the server's own (possibly sanitized - e.g. an
/// underscore in the original filename comes back as a dash) stored
/// filename, needed to build the `i.postimg.cc/<slug>/<name>.<ext>`
/// "thumbnail" link (see send_attachment's own doc comment for why this
/// variant over the page's other "hotlink"/direct-link slug). Reusing
/// form::attr rather than a one-off parser here since the shape (name/
/// value pairs on a single tag) is exactly what it already handles.
///
/// `data-image=` isn't unique on the page - confirmed live, the page's
/// own outer `<div class="container mb-5" data-image="...">` wrapper
/// carries it too, with none of the other `data-*` attributes alongside
/// it, and appears first in the HTML. Taking the very first match found
/// nothing there and returned early instead of continuing on to the real
/// `class="col"` one right after it, so every candidate tag is checked in
/// order instead of stopping at the first.
fn extract_postimg_thumb_name(html: &str) -> Option<(String, String)> {
    let mut search_from = 0;
    while let Some(rel) = html[search_from..].find("data-image=") {
        let start = search_from + rel;
        let tag_start = html[..start].rfind('<')?;
        let tag_end = tag_start + html[tag_start..].find('>')?;
        let tag = &html[tag_start..tag_end];
        search_from = tag_end;

        if let (Some(name), Some(ext)) = (form::attr(tag, "data-name"), form::attr(tag, "data-ext")) {
            return Some((name, ext));
        }
    }
    None
}

/// Both links postimg.cc gives back for one upload.
pub struct PostimgLinks {
    /// The page about the image, for a click-through.
    pub page: String,
    /// The image itself, for anywhere that renders a bare URL.
    pub direct: String,
}

/// Puts an image on postimg.cc and returns where it landed.
///
/// Shared rather than private to Sneedchat, because the two callers want
/// different halves of the same answer: Sneedchat wraps both in the BBCode
/// the site renders, and IRC sends the direct link on its own, having no
/// markup to wrap anything in. The mechanics below are the same either way
/// and were hard enough won that a second copy of them would be a liability.
pub async fn upload_to_postimg(file_path: &str) -> Result<PostimgLinks> {
    let bytes = tokio::fs::read(file_path).await.with_context(|| format!("reading {file_path}"))?;
    if bytes.is_empty() {
        bail!("file is empty");
    }
    if bytes.len() > POSTIMG_MAX_UPLOAD_BYTES {
        bail!("file is over postimg.cc's {}MB upload limit", POSTIMG_MAX_UPLOAD_BYTES / 1024 / 1024);
    }

    let file_name = std::path::Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or("upload").to_string();
    let Some(content_type) = guess_postimg_content_type(&file_name) else {
        bail!("postimg.cc only accepts images (png/jpg/gif/webp/bmp), not \"{file_name}\"");
    };

    let http = http::HttpClient::new(Transport::Direct, http::CookieJar::new(), DEFAULT_USER_AGENT.to_string());
    let session = postimg_upload_session();
    let bytes = Bytes::from(bytes);
    let fields = [
        http::MultipartField::Text { name: "gallery", value: "" },
        http::MultipartField::Text { name: "optsize", value: "0" },
        http::MultipartField::Text { name: "expire", value: "604800" },
        http::MultipartField::Text { name: "numfiles", value: "1" },
        http::MultipartField::Text { name: "upload_session", value: &session },
        http::MultipartField::File { name: "file", file_name: &file_name, content_type, bytes: &bytes },
    ];
    let extra_headers = [("Origin", POSTIMG_ORIGIN), ("Referer", &format!("{POSTIMG_ORIGIN}/"))];

    let (status, resp_bytes) = http.post_multipart(POSTIMG_UPLOAD_URL, &fields, &extra_headers).await.context("uploading to postimg.cc")?;
    if !(200..300).contains(&status) {
        let snippet = String::from_utf8_lossy(&resp_bytes[..resp_bytes.len().min(300)]);
        bail!("postimg.cc upload failed with HTTP {status}: {snippet}");
    }
    let parsed: PostimgUploadResponse = serde_json::from_slice(&resp_bytes).context("parsing postimg.cc response")?;
    if let Some(err) = parsed.error {
        bail!("postimg.cc upload failed: {}", err.message);
    }
    // The upload response's own url carries a delete-hash suffix
    // (`https://postimg.cc/<slug>/<hash>`) that the result page needs to
    // actually render (the bare `/<slug>` alone serves something else -
    // confirmed live, scraping came back empty without it). The outer
    // `[url=]` wrapper below uses the slug-only form instead, matching
    // postimg.cc's own "Thumbnail for forums" BBCode preset shown on that
    // page - the hash is a one-time delete credential, not part of the
    // link anyone else needs to view the image.
    let full_page_url = parsed.url.ok_or_else(|| anyhow!("postimg.cc response had no url"))?;
    let slug = full_page_url.strip_prefix("https://postimg.cc/").and_then(|rest| rest.split('/').next()).ok_or_else(|| anyhow!("unexpected postimg.cc url shape: {full_page_url}"))?;
    let short_page_url = format!("https://postimg.cc/{slug}");

    let page = http.get(&full_page_url).await.context("fetching postimg.cc result page")?;
    let (name, ext) = extract_postimg_thumb_name(&page.body).ok_or_else(|| anyhow!("couldn't find the uploaded image's filename on {full_page_url}"))?;
    let direct_url = format!("https://i.postimg.cc/{slug}/{name}.{ext}");


    Ok(PostimgLinks { page: short_page_url, direct: direct_url })
}

/// The "+" attachment button's SockChat backend. Unlike Discord (whose API
/// natively accepts a file alongside a message), Sneedchat's own chat
/// protocol is text-only - there's no upload endpoint on the site itself.
/// This does what regulars already do by hand: upload the file to a
/// separate anonymous image host, then post the resulting URL as a normal
/// chat message wrapped in the [img] BBCode the site itself expects for a
/// picture to actually render there - the same `[img]...[/img]` shape
/// find_attachment_url/normalizeBBCode already know how to unwrap on the
/// receiving end. Previously qu.ax; switched to postimg.cc after qu.ax
/// stopped reliably serving uploaded images back out.
///
/// Wrapped as `[url=<page>][img]<direct>[/img][/url]` rather than a bare
/// `[img]...[/img]` - postimg.cc's own "Thumbnail for forums" BBCode
/// preset (the one shown on its own result page) uses exactly this shape,
/// giving the posted image a click-through back to the postimg.cc page
/// alongside the inline preview, which a bare [img] tag wouldn't.
///
/// Two requests, not one: the upload POST only returns the *page* URL
/// (`https://postimg.cc/<slug>/<hash>`), not a direct image link - the
/// actual `i.postimg.cc/...` link (and the server's own possibly-
/// sanitized version of the filename) only appears in that page's own
/// HTML, so it has to be fetched and scraped same as the reference
/// upload flow this was ported from does.
///
/// Deliberately `Transport::Direct` (plain clearnet), not the account's own
/// Tor transport: postimg.cc is a general image host, not part of Kiwi
/// Farms - nothing it sees is tied to the Sneedchat account or identity at
/// all (it's a plain anonymous upload, no auth). Following qu.ax's own
/// precedent of not routing this over Tor - free upload hosts commonly
/// block Tor exit traffic outright.
///
/// A fresh, throwaway HttpClient rather than the account's own logged-in
/// session: postimg.cc needs no authentication at all for an anonymous
/// upload, so there's nothing to gain from reusing the Sneedchat session's
/// cookies, and building a new client here means this doesn't need any new
/// account-level state threaded through Runtime just for this one feature.
/// Which host to put it on. `None` keeps postimg.cc, which is what this
/// always did and what the site's own regulars use.
///
/// Honouring the caller's choice is the whole point: the uploads setting
/// promised it covered "anything Sneedchat's own uploader refuses", and it
/// reached IRC only - a Sneedchat send called straight into the postimg path
/// below, whose signature had nowhere to put a host. Attaching a video there
/// failed with "postimg.cc only accepts images" and no way to pick something
/// that would take it.
pub async fn send_attachment(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    caption: &str,
    file_path: &str,
    host: Option<crate::upload::Host>,
) -> Result<()> {
    // Only used to confirm the account is real before spending any time on
    // the upload - the transport below is deliberately unrelated to it.
    state.accounts.get_sockchat(account_id).ok_or_else(|| anyhow!("no such account"))?;

    match host {
        None | Some(crate::upload::Host::Postimg) => {}
        Some(host) => return send_via_upload_host(state, account_id, buffer_name, caption, file_path, host).await,
    }

    let links = upload_to_postimg(file_path).await?;

    let wrapped = format!("[url={}][img]{}[/img][/url]", links.page, links.direct);
    let text = if caption.trim().is_empty() { wrapped } else { format!("{caption}\n{wrapped}") };
    // The caption already carries any mention the caller wanted; an image
    // post is not separately a reply.
    send_message(state, account_id, buffer_name, &text, None)
}

/// Posting a file that went to one of the shared upload hosts.
///
/// The link still has to be wrapped in `[img]` for a picture to render
/// rather than sit there as a URL - that is the site's own markup, and what
/// find_attachment_url already unwraps on the way back in. Anything that is
/// not a picture goes as a bare link, because `[img]` around a video would
/// render as a broken image instead of something clickable.
///
/// No `[url=]` wrapper here, unlike the postimg path: these hosts serve the
/// file itself rather than a page about it, so there is nothing to click
/// through to that the image is not already showing.
async fn send_via_upload_host(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    caption: &str,
    file_path: &str,
    host: crate::upload::Host,
) -> Result<()> {
    let link = crate::upload::upload(host, file_path, None).await?;
    let file_name = std::path::Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let posted = posted_markup(file_name, &link);
    let text = if caption.trim().is_empty() { posted } else { format!("{caption}\n{posted}") };
    send_message(state, account_id, buffer_name, &text, None)
}

/// How a finished upload is written into a message.
fn posted_markup(file_name: &str, link: &str) -> String {
    if guess_postimg_content_type(file_name).is_some() {
        format!("[img]{link}[/img]")
    } else {
        link.to_string()
    }
}

/// `editMessage`'s SockChat branch (see rpc/methods.rs). The server has no
/// direct "edit accepted" reply - the edited message just comes back
/// through the normal live stream with a bumped message_edit_date, which
/// handle_frame already detects and applies via Runtime::update_message.
pub fn edit_message(state: &AppState, account_id: &str, buffer_name: &str, msg_id: &str, body: &str) -> Result<()> {
    let sender = room_sender_for_buffer(state, account_id, buffer_name)?;
    sender.send(protocol::prepare_edit(msg_id, body)).map_err(|_| anyhow!("chat socket closed"))?;
    Ok(())
}

/// `deleteMessage`'s SockChat branch - same "no direct reply" story as
/// edit_message; the deletion gets applied locally once it's echoed back
/// (either as a top-level `delete` batch or a `deleted`/`is_deleted` flag).
pub fn delete_message(state: &AppState, account_id: &str, buffer_name: &str, msg_id: &str) -> Result<()> {
    let sender = room_sender_for_buffer(state, account_id, buffer_name)?;
    sender.send(protocol::prepare_delete(msg_id)).map_err(|_| anyhow!("chat socket closed"))?;
    Ok(())
}

/// Kicks off a fresh account's first login in the background - unlike
/// spawn() (used for accounts that already have saved credentials and are
/// reconnecting), this is what addSockChatAccount calls, since Tor
/// bootstrap + login + a possible proof-of-work solve can take anywhere
/// from instant to over a minute and must not block the RPC response.
/// Progress/result arrive via sockChatLoginStatus/sockChatLoginResult
/// events tagged with `login_id`, mirroring backend::discord::start_qr_login.
pub fn start_login(state: AppState, login_id: String, config: SockChatAccountConfig) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(try_login(&state, &login_id, &config)).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("sockchat login[{login_id}]: {error}");
        state.events.emit("sockChatLoginResult", serde_json::json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

async fn try_login(state: &AppState, login_id: &str, config: &SockChatAccountConfig) -> Result<()> {
    let emit_progress = |msg: &str| {
        state.events.emit("sockChatLoginStatus", serde_json::json!({ "loginId": login_id, "detail": msg }));
    };

    // Saved before anything can fail, not after everything has succeeded.
    //
    // This used to persist only on the far side of a successful login, so a
    // wrong password - or a Tor bootstrap that never completed - threw the
    // account away along with everything typed into the form. The next
    // attempt started from an empty form, which is the worst moment to ask
    // somebody to retype a password: they have just been told it might be
    // wrong.
    //
    // The account id is derived from the username, so re-adding the same
    // account updates it in place rather than accumulating duplicates, and a
    // corrected password simply overwrites the stored one. What lands here is
    // an account that exists, holds what was entered, and is disconnected -
    // which is exactly the state Connect knows how to retry.
    state.accounts.add_sockchat(config.clone())?;

    let transport = match config.tor_mode.as_str() {
        "proxy" => {
            let proxy = config.proxy.as_deref().ok_or_else(|| anyhow!("tor_mode is \"proxy\" but no proxy URL is configured"))?;
            Transport::socks_from_url(proxy)?
        }
        _ => {
            let client = state.tor.get_or_bootstrap(emit_progress).await.context("bootstrapping Tor")?;
            Transport::Tor(client)
        }
    };

    let base = format!("https://{}", config.host);
    let session = Session::new(transport, base, DEFAULT_USER_AGENT.to_string());
    let two_factor = match &config.totp_secret {
        Some(secret) => TwoFactor::Totp(totp::decode_secret(secret).context("TOTP secret is not valid base32")?),
        None => TwoFactor::None,
    };
    let creds = Credentials { username: config.username.clone(), password: config.password.clone() };

    emit_progress("logging in...");
    session.ensure_authenticated(&creds, &two_factor).await.context("logging in")?;

    let mut saved_config = config.clone();
    saved_config.user_id = session.user_id();
    let saved = state.accounts.add_sockchat(saved_config)?;
    let account = crate::accounts::sockchat_account_to_json(&saved, "connecting");
    spawn(state.clone(), saved);

    state.events.emit("sockChatLoginResult", serde_json::json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}

#[cfg(test)]
mod tests {

    /// A picture has to be wrapped for the site to render it; a video must
    /// not be, because [img] around an .mp4 draws a broken image instead of
    /// something you can click.
    #[test]
    fn only_pictures_are_wrapped_for_display() {
        assert_eq!(posted_markup("cat.png", "https://files.catbox.moe/a.png"), "[img]https://files.catbox.moe/a.png[/img]");
        assert_eq!(posted_markup("cat.JPEG", "https://x/a.jpeg"), "[img]https://x/a.jpeg[/img]");
        assert_eq!(posted_markup("clip.mp4", "https://files.catbox.moe/b.mp4"), "https://files.catbox.moe/b.mp4");
        assert_eq!(posted_markup("notes.pdf", "https://x/c.pdf"), "https://x/c.pdf");
        // No extension at all is not a picture.
        assert_eq!(posted_markup("README", "https://x/d"), "https://x/d");
    }
    #[test]
    fn a_reply_opens_with_the_mention_sneedchat_understands() {
        // Sneedchat has no reply field; answering somebody by name is what
        // the site's own client does and what its notifications look for.
        assert_eq!(super::as_reply("sure", Some("Alexcellence")), "@Alexcellence, sure");
        // Leading space would otherwise land between the comma and the text.
        assert_eq!(super::as_reply("   sure", Some("Bob")), "@Bob, sure");
    }

    #[test]
    fn replying_twice_does_not_stack_mentions() {
        // Somebody who types the mention themselves, or replies again to the
        // same person, should not end up with "@Bob, @Bob, ...".
        assert_eq!(super::as_reply("@Bob, already there", Some("Bob")), "@Bob, already there");
    }

    #[test]
    fn a_message_that_is_not_a_reply_is_untouched() {
        assert_eq!(super::as_reply("plain", None), "plain");
        // A reply to somebody whose name is unknown - dropped from scrollback -
        // still sends rather than being lost to a missing mention.
        assert_eq!(super::as_reply("plain", Some("")), "plain");
    }

    use super::{cached_avatar_file, find_attachment_url, posted_markup, sniff_image_ext};

    #[test]
    fn matches_the_real_reported_url() {
        let body = "look at this https://kiwifarms.st/attachments/sam-consent-accident-webp.5648955/ what a guy";
        let (matched, id, ext) = find_attachment_url(body).expect("should match");
        assert_eq!(matched, "https://kiwifarms.st/attachments/sam-consent-accident-webp.5648955/");
        assert_eq!(id, "5648955");
        assert_eq!(ext, "webp");
    }

    #[test]
    fn matches_without_a_trailing_slash() {
        let (matched, id, ext) = find_attachment_url("https://kiwifarms.st/attachments/foo-bar-png.42").expect("should match");
        assert_eq!(matched, "https://kiwifarms.st/attachments/foo-bar-png.42");
        assert_eq!(id, "42");
        assert_eq!(ext, "png");
    }

    #[test]
    fn matches_the_onion_host_too() {
        let url = format!("https://{}/attachments/clip-mp4.99/", super::DEFAULT_ONION);
        let (matched, id, ext) = find_attachment_url(&url).expect("should match");
        assert_eq!(matched, url);
        assert_eq!(id, "99");
        assert_eq!(ext, "mp4");
    }

    #[test]
    fn stops_at_a_bracket_or_whitespace() {
        let (matched, ..) = find_attachment_url("[url=https://kiwifarms.st/attachments/x-webp.1/]click[/url]").expect("should match");
        assert_eq!(matched, "https://kiwifarms.st/attachments/x-webp.1/");
    }

    #[test]
    fn rejects_a_non_media_extension() {
        assert!(find_attachment_url("https://kiwifarms.st/attachments/report-txt.7/").is_none());
    }

    #[test]
    fn rejects_a_non_numeric_id() {
        assert!(find_attachment_url("https://kiwifarms.st/attachments/foo-webp.abc/").is_none());
    }

    #[test]
    fn rejects_an_unrelated_host() {
        assert!(find_attachment_url("https://example.com/attachments/foo-webp.5/").is_none());
    }

    #[test]
    fn rejects_a_path_with_no_extension_suffix() {
        // No "-<ext>" segment before the numeric id at all.
        assert!(find_attachment_url("https://kiwifarms.st/attachments/justaname.5/").is_none());
    }

    #[test]
    fn extracts_the_postimg_thumb_name_from_a_real_result_page() {
        use super::extract_postimg_thumb_name;
        // The page's own outer wrapper carries a bare data-image= with none
        // of the other data-* attributes, appearing before the real
        // class="col" one - confirmed live to trip up a "stop at the first
        // match" parser (see extract_postimg_thumb_name's own doc comment).
        let html = r#"<div class="container mb-5" data-image="PNzTLBBt">
<div class="row g-4">
<div class="col-12 col-md-4" style="max-width: 240px;">
<div class="col" data-image="PNzTLBBt" data-hash="ea20f286" data-hotlink="05hQ4vnJ" data-name="2026-08-09-14-45-58" data-ext="png" data-homepage="0"><div class="card h-100">"#;
        let (name, ext) = extract_postimg_thumb_name(html).expect("should find the data-image div");
        assert_eq!(name, "2026-08-09-14-45-58");
        assert_eq!(ext, "png");
    }

    #[test]
    fn postimg_thumb_name_is_none_without_a_matching_tag() {
        use super::extract_postimg_thumb_name;
        assert!(extract_postimg_thumb_name("<html><body>nothing here</body></html>").is_none());
    }

    #[test]
    fn postimg_accepts_common_image_extensions_only() {
        use super::guess_postimg_content_type;
        assert_eq!(guess_postimg_content_type("photo.PNG"), Some("image/png"));
        assert_eq!(guess_postimg_content_type("photo.jpeg"), Some("image/jpeg"));
        assert_eq!(guess_postimg_content_type("clip.mp4"), None);
        assert_eq!(guess_postimg_content_type("noext"), None);
    }

    #[test]
    fn smilie_bundled_files_are_unique_across_the_table() {
        use std::collections::HashSet;
        let names: HashSet<&str> = super::smilies::SMILIES.iter().map(|s| s.file).collect();
        assert_eq!(names.len(), super::smilies::SMILIES.len(), "two distinct smilies point at the same bundled asset file");
    }

    // Catches a typo'd filename or a missing/forgotten `git add` for a
    // bundled asset at test time rather than as a silent broken image the
    // first time someone actually opens the emoji picker or hits that
    // shortcode in a live message.
    #[test]
    fn smilie_bundled_files_all_exist_on_disk() {
        let assets_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/sockchat-smilies");
        for s in super::smilies::SMILIES {
            assert!(assets_dir.join(s.file).is_file(), "missing bundled asset for {:?}: {}", s.label, s.file);
        }
    }

    #[test]
    fn names_an_image_by_its_own_bytes() {
        assert_eq!(sniff_image_ext(b"\x89PNG\r\n\x1a\n....."), Some("png"));
        assert_eq!(sniff_image_ext(b"\xff\xd8\xff\xe0 jfif"), Some("jpg"));
        assert_eq!(sniff_image_ext(b"GIF89a......"), Some("gif"));
        assert_eq!(sniff_image_ext(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("webp"));
        assert_eq!(sniff_image_ext(b"\x00\x00\x00\x20ftypavif...."), Some("avif"));
        assert_eq!(sniff_image_ext(b"\x00\x00\x00\x20ftypisom...."), Some("mp4"));
    }

    #[test]
    fn a_riff_that_is_not_a_webp_is_not_claimed() {
        // RIFF also fronts WAV and AVI; only the WEBP form is an image here.
        assert_eq!(sniff_image_ext(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_image_ext(b"nothing recognisable"), None);
        assert_eq!(sniff_image_ext(b""), None);
    }

    /// The regression this whole change exists for: the site serves WebP from
    /// a .jpg URL, so entries cached before sniffing carry the wrong name.
    #[tokio::test]
    async fn a_mislabelled_cache_entry_is_discarded_so_it_gets_refetched() {
        let dir = std::env::temp_dir().join(format!("nobilis-avatar-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("42.jpg");
        std::fs::write(&stale, b"RIFF\x00\x00\x00\x00WEBPVP8 payload").unwrap();

        assert!(cached_avatar_file(&dir, "42").await.is_none(), "kept a WebP named .jpg");
        assert!(!stale.exists(), "left the mislabelled file behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_correctly_named_entry_is_reused() {
        let dir = std::env::temp_dir().join(format!("nobilis-avatar-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("7.webp");
        std::fs::write(&good, b"RIFF\x00\x00\x00\x00WEBPVP8 payload").unwrap();

        assert_eq!(cached_avatar_file(&dir, "7").await.as_deref(), Some(good.as_path()));
        // A real JPEG under .jpg must also survive - this must not re-fetch
        // every avatar on every message.
        let jpg = dir.join("8.jpg");
        std::fs::write(&jpg, b"\xff\xd8\xff\xe0 jfif payload").unwrap();
        assert_eq!(cached_avatar_file(&dir, "8").await.as_deref(), Some(jpg.as_path()));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod live_probe {
    use super::auth::{Credentials, Session, TwoFactor};
    use super::http::{CookieJar, HttpClient};
    use super::pow;
    use crate::net::tor::{Transport, TorManager};

    /// Phase-2 live check: fetch the real login page through embedded Tor
    /// and confirm the KiwiFlare gate (if hit) is solved for real, ending
    /// with a normal page response. Not run by default; run explicitly:
    ///   cargo test --release -- --ignored --nocapture sockchat_http_probe
    #[tokio::test]
    #[ignore]
    async fn sockchat_http_probe() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let http = HttpClient::new(Transport::Tor(client), CookieJar::new(), super::DEFAULT_USER_AGENT.to_string());
        let url = format!("https://{}/", super::DEFAULT_ONION);

        let resp = http.get(&url).await.expect("initial GET failed");
        println!("initial GET {url} -> HTTP {}", resp.status);

        let final_resp = if resp.status == pow::GATE_STATUS {
            println!("hit the KiwiFlare gate, solving...");
            let solved = pow::clear(&http, &url, 8).await.expect("failed to clear the gate");
            println!("solved {solved} challenge(s)");
            http.get(&url).await.expect("GET after clearing gate failed")
        } else {
            resp
        };

        println!("final status: HTTP {}, body length {} bytes", final_resp.status, final_resp.body.len());
        assert!((200..400).contains(&final_resp.status), "expected a normal page response, got HTTP {}", final_resp.status);
    }

    /// Phase-3 live check: a real login. Reads credentials from the
    /// environment rather than taking them as literals anywhere in this
    /// repo or conversation - set them in your own shell before running:
    ///
    ///   SOCKCHAT_USERNAME=... SOCKCHAT_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture sockchat_login_probe
    ///
    /// Add SOCKCHAT_TOTP_SECRET=... too if the account has 2FA enabled.
    /// Skips (rather than failing) if SOCKCHAT_USERNAME/PASSWORD aren't set,
    /// so this is safe to leave in the suite without real credentials
    /// present in CI or anyone else's environment.
    #[tokio::test]
    #[ignore]
    async fn sockchat_login_probe() {
        let Ok(username) = std::env::var("SOCKCHAT_USERNAME") else {
            println!("SOCKCHAT_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("SOCKCHAT_PASSWORD") else {
            println!("SOCKCHAT_PASSWORD not set, skipping");
            return;
        };
        let two_factor = match std::env::var("SOCKCHAT_TOTP_SECRET") {
            Ok(secret) => TwoFactor::Totp(super::totp::decode_secret(&secret).expect("SOCKCHAT_TOTP_SECRET is not valid base32")),
            Err(_) => TwoFactor::None,
        };

        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let base = format!("https://{}", super::DEFAULT_ONION);
        let session = Session::new(Transport::Tor(client), base, super::DEFAULT_USER_AGENT.to_string());
        let creds = Credentials { username, password };

        println!("logging in...");
        session.ensure_authenticated(&creds, &two_factor).await.expect("login failed");
        println!("authenticated. user id: {:?}", session.user_id());
        assert!(session.is_authenticated().await.expect("post-login check failed"), "session reports not authenticated right after logging in");
    }
}

/// Applies a roster delta to a room and republishes it.
///
/// The server sends the full roster on join and single entries afterwards,
/// both under the same key and in the same shape, so both are simply merged -
/// the roster is emptied when the room is joined, which is what makes that
/// safe. Departures arrive separately, keyed by id with a presence flag.
///
/// Ordering is by name here rather than left to the client: every other
/// backend hands over a sorted roster, and a five-hundred-name list arriving
/// in map order would be unreadable.
/// The site owner's forum account. SockChat carries no rank information at
/// all - the roster's user objects are id, name, avatar and last activity, and
/// the only `permissions` frame describes our *own* ability to view and send -
/// so there is no wire signal to derive staff from. This one id is a fact
/// about the site rather than something the protocol tells us, which is why it
/// is the only such marking: guessing at moderators without data would be
/// worse than showing everyone as an ordinary member.
const SITE_OWNER_ID: &str = "1";

fn update_roster(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    joined: &[protocol::WireUser],
    left: &[String],
) {
    let buffer_id = crate::model::buffer_id(account_id, buffer_name);
    let mut by_id: std::collections::BTreeMap<String, String> = state
        .runtime
        .get_presence(&buffer_id)
        .and_then(|v| serde_json::from_value::<Vec<serde_json::Value>>(v).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let id = m.get("userId")?.as_str()?.to_string();
            let nick = m.get("nick")?.as_str()?.to_string();
            Some((id, nick))
        })
        .collect();

    for user in joined {
        by_id.insert(user.id.clone(), user.username.clone());
    }
    for id in left {
        by_id.remove(id);
    }

    let mut members: Vec<(String, String)> = by_id.into_iter().collect();
    members.sort_by(|(_, a), (_, b)| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
    let member_list = serde_json::json!(members
        .into_iter()
        .map(|(id, nick)| {
            // "~" is the owner prefix the frontend already ranks by, shared
            // with IRC rather than inventing a Sneedchat-only convention.
            let prefix = if id == SITE_OWNER_ID { "~" } else { "" };
            serde_json::json!({ "nick": nick, "userId": id, "prefix": prefix, "away": false })
        })
        .collect::<Vec<_>>());

    // Persisted as well as broadcast, so a client that subscribes later gets
    // the roster from subscribe's replay rather than waiting for the next
    // arrival or departure - which in a quiet room could be a long wait.
    state.runtime.set_presence(&buffer_id, member_list.clone());
    state.events.emit(
        "presenceChange",
        serde_json::json!({ "bufferId": buffer_id, "members": member_list }),
    );
}
