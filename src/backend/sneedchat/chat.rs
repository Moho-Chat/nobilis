//! The account's chat connection.
//!
//! Ephyra carries every room on one socket: the account is admitted once
//! (forum session -> ticket -> chat session), opens one socket, and joins each
//! of its rooms on it. Opening a room is a `join` on the socket that is
//! already there, closing one a `leave`.
//!
//! The socket and every request go through the web helper (net/webhelper.rs),
//! because the site's gate admits only a browser's network stack.

use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::net::webhelper::{self, WsEvent, WsSender};
use protocol::{Entry, Principal};
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// How long without any frame before the connection is taken for dead. The
/// client pings every 30 seconds and the server answers, so a live socket is
/// never this quiet.
const IDLE_TIMEOUT: Duration = Duration::from_secs(75);
const PING_EVERY: Duration = Duration::from_secs(30);

pub(super) const RECONNECT_INITIAL: Duration = Duration::from_secs(3);
pub(super) const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// What one account's connection knows, shared with what sends on it.
#[derive(Default)]
pub(super) struct Chat {
    pub(super) me: Mutex<Option<Principal>>,
    rooms: Mutex<HashMap<String, RoomState>>,
    messages: Mutex<HashMap<String, Known>>,
    waiters: Mutex<HashMap<String, oneshot::Sender<std::result::Result<(), String>>>>,
    pub(super) people: Mutex<HashMap<String, Principal>>,
    socket: Mutex<Option<WsSender>>,
    pub(super) session: Mutex<Option<Session>>,
    pub(super) host: Mutex<String>,
}

#[derive(Clone, Debug, Default)]
struct RoomState {
    name: String,
    last: Option<u64>,
    replay_to: Option<u64>,
    joined: bool,
}

/// What an edit or a removal has to name: the state the message is in,
/// which the chat refuses to change from under somebody else.
#[derive(Clone, Debug)]
pub(super) struct Known {
    pub state_seq: u64,
}

fn chats() -> &'static Mutex<HashMap<String, Arc<Chat>>> {
    static CHATS: OnceLock<Mutex<HashMap<String, Arc<Chat>>>> = OnceLock::new();
    CHATS.get_or_init(Default::default)
}

/// The account's connection state, made if there is none.
pub(super) fn chat(account_id: &str) -> Arc<Chat> {
    chats().lock().unwrap().entry(account_id.to_string()).or_default().clone()
}

/// Forgets an account's connection state (it was removed or disconnected).
pub(super) fn forget_chat(account_id: &str) {
    chats().lock().unwrap().remove(account_id);
}

impl Chat {
    /// The live socket, if there is one and it is welcomed.
    pub(super) fn socket(&self) -> Option<WsSender> {
        self.socket.lock().unwrap().clone()
    }

    fn send(&self, frame: &Value) -> bool {
        match self.socket() {
            Some(s) => s.send(&frame.to_string()).is_ok(),
            None => false,
        }
    }

    pub(super) fn joined(&self, room: &str) -> bool {
        self.rooms.lock().unwrap().get(room).is_some_and(|r| r.joined)
    }

    pub(super) fn known(&self, message: &str) -> Option<Known> {
        self.messages.lock().unwrap().get(&message.to_lowercase()).cloned()
    }

    fn remember(&self, message: &str, state_seq: u64) {
        let mut all = self.messages.lock().unwrap();
        // Bounded: a long-lived connection in busy rooms would otherwise keep
        // every message it ever saw. What falls out is old enough that an
        // edit to it looks the state up afresh.
        if all.len() > 50_000 {
            all.clear();
        }
        all.insert(message.to_string(), Known { state_seq });
    }

    /// Waits for the answer to something sent under `key` - a post's nonce or
    /// a command's operation id.
    pub(super) fn expect(&self, key: &str) -> oneshot::Receiver<std::result::Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(key.to_string(), tx);
        rx
    }

    pub(super) fn give_up(&self, key: &str) {
        self.waiters.lock().unwrap().remove(key);
    }

    fn answer(&self, key: &str, result: std::result::Result<(), String>) -> bool {
        match self.waiters.lock().unwrap().remove(key) {
            Some(tx) => {
                let _ = tx.send(result);
                true
            }
            None => false,
        }
    }

    /// Sends a frame that expects an answer, and waits for it.
    pub(super) async fn command(&self, key: &str, frame: Value) -> Result<()> {
        let rx = self.expect(key);
        if !self.send(&frame) {
            self.give_up(key);
            bail!("not connected to the chat right now");
        }
        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(code))) => bail!("{}", protocol::describe_error(&code)),
            Ok(Err(_)) => bail!("the chat connection dropped before it answered"),
            Err(_) => {
                self.give_up(key);
                bail!("the chat hasn't answered - it may still go through")
            }
        }
    }

    /// The rooms the account wants, as the connection tracks them: new ones
    /// added (and joined, when the socket is up), dropped ones left.
    pub(super) fn want_rooms(&self, rooms: &[SneedChatRoom]) {
        let mut joins = Vec::new();
        let mut leaves = Vec::new();
        {
            let mut tracked = self.rooms.lock().unwrap();
            tracked.retain(|uuid, state| {
                let keep = rooms.iter().any(|r| &r.id == uuid);
                if !keep && state.joined {
                    leaves.push(uuid.clone());
                }
                keep
            });
            for room in rooms.iter().filter(|r| protocol::is_uuid(&r.id)) {
                let state = tracked.entry(room.id.clone()).or_default();
                state.name = room.name.clone();
                if !state.joined {
                    joins.push((room.id.clone(), state.last));
                }
            }
        }
        for uuid in leaves {
            self.send(&json!({ "t": "leave", "chat_room_uuid": uuid }));
        }
        for (uuid, last) in joins {
            self.join(&uuid, last);
        }
    }

    fn join(&self, uuid: &str, after: Option<u64>) {
        let mut frame = json!({ "t": "join", "chat_room_uuid": uuid });
        if let Some(seq) = after {
            frame["after_seq"] = json!(seq);
        }
        self.send(&frame);
    }

    fn room_name(&self, uuid: &str) -> Option<String> {
        self.rooms.lock().unwrap().get(uuid).map(|r| r.name.clone())
    }

    /// The room a buffer shows, by uuid.
    pub(super) fn room_of_buffer(&self, buffer_name: &str) -> Option<String> {
        let name = room_name_of(buffer_name);
        self.rooms.lock().unwrap().iter().find(|(_, r)| r.name == name).map(|(uuid, _)| uuid.clone())
    }
}

/// Gets this session admitted to the chat: a ticket from the forum, traded
/// for the chat's own session cookie.
pub(super) async fn admit(session: &Session, host: &str) -> Result<()> {
    let base = format!("https://{host}");
    let page = session.fetch(&format!("{base}/")).await.context("loading the forum")?;
    let token = protocol::attr(&page.body, "data-csrf").context("the forum page carries no form token - its markup may have changed")?;
    let form = http::encode_form(&[("_xfToken", &token)]);
    let resp = session
        .http
        .request(hyper::Method::POST, &format!("{base}/ephyra-chat/ticket"), Some(form), Some("application/x-www-form-urlencoded"), &[("Accept", "application/json")])
        .await
        .context("asking the forum for a chat ticket")?;
    let ticket = serde_json::from_str::<Value>(&resp.body)
        .ok()
        .and_then(|v| v["ticket"].as_str().map(str::to_string))
        .filter(|t| !t.is_empty())
        .with_context(|| format!("the forum gave no chat ticket (HTTP {})", resp.status))?;
    let resp = session
        .http
        .request(
            hyper::Method::POST,
            &format!("{base}/chat/v1/sessions"),
            Some(json!({ "ticket": ticket }).to_string()),
            Some("application/json"),
            &[("Accept", "application/json"), ("Origin", &base)],
        )
        .await
        .context("starting a chat session")?;
    if !(200..300).contains(&resp.status) {
        bail!("the chat refused the ticket (HTTP {}): {}", resp.status, resp.body.chars().take(200).collect::<String>());
    }
    Ok(())
}

/// A JSON read from the chat's own API, admitted.
pub(super) async fn api_get(session: &Session, host: &str, path: &str) -> Result<Value> {
    let base = format!("https://{host}");
    let resp = session.http.request(hyper::Method::GET, &format!("{base}/chat/v1/{path}"), None, None, &[("Accept", "application/json"), ("Origin", &base)]).await?;
    if !(200..300).contains(&resp.status) {
        bail!("the chat answered HTTP {} for {path}", resp.status);
    }
    serde_json::from_str(&resp.body).with_context(|| format!("reading the chat's answer for {path}"))
}

/// Keeps the account's chat connection up, for as long as this task lives:
/// admitted, connected, its rooms joined - again after every interruption,
/// refreshing the forum session when the chat stops recognising it.
pub(super) async fn run_connection(state: &AppState, account_id: &str, session: &Session, host: &str, creds: &Credentials, two_factor: &TwoFactor) -> Result<()> {
    let chat = chat(account_id);
    *chat.session.lock().unwrap() = Some(session.clone());
    *chat.host.lock().unwrap() = host.to_string();
    let mut delay = RECONNECT_INITIAL;
    let mut first = true;
    loop {
        let started = std::time::Instant::now();
        let result = connect_once(state, account_id, &chat, session, host, first).await;
        first = false;
        *chat.socket.lock().unwrap() = None;
        for state in chat.rooms.lock().unwrap().values_mut() {
            state.joined = false;
        }
        // Whatever was waiting on this socket will not hear from it now.
        chat.waiters.lock().unwrap().clear();
        if started.elapsed() > RECONNECT_MAX {
            delay = RECONNECT_INITIAL;
        }
        match result {
            Ok(()) => tracing::info!("sneedchat[{account_id}]: chat connection closed"),
            Err(e) => {
                tracing::warn!("sneedchat[{account_id}]: chat connection: {e:#}");
                if format!("{e:#}").contains("as a guest") {
                    if let Err(re) = session.refresh(creds, two_factor).await {
                        tracing::warn!("sneedchat[{account_id}]: refreshing the forum session: {re:#}");
                    }
                }
            }
        }
        for name in chat.rooms.lock().unwrap().values().map(|r| r.name.clone()).collect::<Vec<_>>() {
            let buffer_id = crate::model::buffer_id(account_id, &room_buffer_name(&name));
            state.runtime.set_buffer_link(state, &buffer_id, Some(crate::model::BufferLink::connecting()));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX);
    }
}

async fn connect_once(state: &AppState, account_id: &str, chat: &Arc<Chat>, session: &Session, host: &str, first: bool) -> Result<()> {
    admit(session, host).await?;
    if first {
        super::rooms::settle_rooms(state, account_id, session, host).await;
    }
    let route = session.http.browser_route().cloned().unwrap_or_default();
    let cookies: Vec<webhelper::Cookie> = session
        .http
        .jar
        .snapshot()
        .into_iter()
        .map(|(name, value)| {
            let path = if name == "ephyra_chat_session" { "/chat/v1" } else { "/" }.to_string();
            webhelper::Cookie { name, value, path }
        })
        .collect();
    let mut ws = webhelper::ws_open(
        &format!("wss://{host}/chat/v1/connect"),
        &[protocol::SUBPROTOCOL],
        &format!("https://{host}/robots.txt"),
        &cookies,
        route.proxy.as_deref(),
    )
    .await
    .context("opening the chat socket")?;
    let sender = ws.sender();
    tracing::info!("sneedchat[{account_id}]: chat socket open");

    let pinger = tokio::spawn({
        let sender = sender.clone();
        async move {
            loop {
                tokio::time::sleep(PING_EVERY).await;
                if sender.send(r#"{"t":"ping"}"#).is_err() {
                    return;
                }
            }
        }
    });
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _pinger = Abort(pinger);

    let ctx = Ctx { state, account_id, chat, http: &session.http, host, sender: &sender };
    loop {
        let event = match tokio::time::timeout(IDLE_TIMEOUT, ws.next()).await {
            Ok(Some(e)) => e,
            Ok(None) => bail!("the web helper went away"),
            Err(_) => bail!("nothing from the chat for {}s", IDLE_TIMEOUT.as_secs()),
        };
        match event {
            WsEvent::Message(text) => {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                ctx.frame(&frame).await?;
            }
            WsEvent::Closed(code, reason) => bail!("the chat closed the socket ({code}{}{reason})", if reason.is_empty() { "" } else { " " }),
            WsEvent::Open => {}
        }
    }
}

struct Ctx<'a> {
    state: &'a AppState,
    account_id: &'a str,
    chat: &'a Arc<Chat>,
    http: &'a http::HttpClient,
    host: &'a str,
    sender: &'a WsSender,
}

impl Ctx<'_> {
    async fn frame(&self, frame: &Value) -> Result<()> {
        let (state, account_id, chat) = (self.state, self.account_id, self.chat);
        match frame["t"].as_str().unwrap_or_default() {
            "welcome" => {
                let Some(uuid) = frame["principal_uuid"].as_str() else {
                    bail!("the chat sees this account as a guest - the forum session has lapsed");
                };
                let name = frame["display_name"].as_str().unwrap_or_default().to_string();
                *chat.me.lock().unwrap() = Some(Principal { uuid: uuid.to_string(), name: name.clone(), ..Default::default() });
                if !name.is_empty() {
                    state.runtime.set_own_identity(account_id, &name);
                    let _ = state.accounts.set_sneedchat_display_name(account_id, &name);
                }
                *chat.socket.lock().unwrap() = Some(self.sender.clone());
                super::health::recovered(account_id);
                let rooms = state.accounts.get_sneedchat(account_id).map(|c| c.rooms).unwrap_or_default();
                chat.want_rooms(&rooms);
            }
            "joined" => {
                let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default();
                let head = frame["head_seq"].as_u64();
                let name = {
                    let mut rooms = chat.rooms.lock().unwrap();
                    let Some(room) = rooms.get_mut(uuid) else { return Ok(()) };
                    room.joined = true;
                    room.replay_to = head;
                    if room.last.is_none() && head == Some(0) {
                        room.last = Some(0);
                    }
                    room.name.clone()
                };
                let buffer = state.runtime.ensure_buffer(state, account_id, &room_buffer_name(&name), "channel");
                state.runtime.set_buffer_link(state, &buffer.id, None);
                tracing::info!("sneedchat[{account_id}]: joined #{name}");
            }
            "left" => {
                let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default();
                if let Some(room) = chat.rooms.lock().unwrap().get_mut(uuid) {
                    room.joined = false;
                }
                tracing::info!("sneedchat[{account_id}]: left {uuid} ({})", frame["reason"].as_str().unwrap_or("no reason given"));
            }
            "resync" => {
                let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default().to_string();
                if let Some(room) = chat.rooms.lock().unwrap().get_mut(&uuid) {
                    room.last = None;
                    room.joined = false;
                }
                chat.join(&uuid, None);
            }
            "entry" | "elided" => self.entry(frame).await,
            "presence" => {
                let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default();
                let principals: Vec<Principal> = frame["principals"].as_array().into_iter().flatten().filter_map(Principal::from_value).collect();
                {
                    let mut people = chat.people.lock().unwrap();
                    for p in &principals {
                        people.insert(p.uuid.clone(), p.clone());
                    }
                }
                if let Some(name) = chat.room_name(uuid) {
                    super::rooms::publish_presence(state, self.http, self.host, account_id, &room_buffer_name(&name), &principals);
                }
            }
            "command_ack" => {
                let key = frame["chat_operation_uuid"].as_str().unwrap_or_default();
                let result = match frame["status"].as_str() {
                    Some("applied" | "pending") => Ok(()),
                    other => Err(frame["code"].as_str().or(other).unwrap_or("refused").to_string()),
                };
                chat.answer(key, result);
            }
            "error" => {
                let key = frame["ref"].as_str().unwrap_or_default();
                let code = frame["code"].as_str().unwrap_or("refused").to_string();
                if !chat.answer(key, Err(code.clone())) {
                    if let Some(name) = chat.room_name(key) {
                        let said = format!("Couldn't join this room: {}.", protocol::describe_error(&code));
                        state.runtime.record_message(state, account_id, &room_buffer_name(&name), "channel", "", &said, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                    } else {
                        tracing::warn!("sneedchat[{account_id}]: chat error {code} (ref {key})");
                    }
                }
            }
            "link_state" => {
                tracing::info!("sneedchat[{account_id}]: chat link {}", frame["state"].as_str().unwrap_or("?"));
            }
            _ => {}
        }
        Ok(())
    }

    async fn entry(&self, frame: &Value) {
        let (state, account_id, chat) = (self.state, self.account_id, self.chat);
        let Some(entry) = Entry::from_value(frame) else { return };
        // In order, once each: a gap means something was missed, and the
        // chat replays it on a fresh join from the last seen.
        let replaying = {
            let mut rooms = chat.rooms.lock().unwrap();
            let Some(room) = rooms.get_mut(&entry.room) else { return };
            if room.last.is_some_and(|last| entry.room_seq <= last) {
                return;
            }
            if let Some(last) = room.last {
                if entry.room_seq != last + 1 {
                    room.joined = false;
                    drop(rooms);
                    chat.join(&entry.room, Some(last));
                    return;
                }
            }
            room.last = Some(entry.room_seq);
            room.replay_to.is_some_and(|head| entry.room_seq <= head)
        };
        // A post's own echo answers whoever sent it, whatever else it is.
        if let Some(nonce) = &entry.client_nonce {
            chat.answer(nonce, Ok(()));
        }
        if frame["t"] == "elided" {
            return;
        }
        let Some(name) = chat.room_name(&entry.room) else { return };
        let buffer_name = room_buffer_name(&name);
        let buffer_id = crate::model::buffer_id(account_id, &buffer_name);
        if let (Some(message), Some(seq)) = (&entry.message, entry.state_seq) {
            chat.remember(message, seq);
        }
        let Some(message) = entry.message.clone() else {
            if entry.kind == "chat_room_subtitle_set" {
                state.runtime.set_buffer_topic(state, &buffer_id, entry.subtitle.as_deref().unwrap_or_default());
            }
            return;
        };
        match entry.kind.as_str() {
            "chat_message_posted" | "chat_message_restored" => {
                if entry.row_tombstoned() {
                    state.runtime.delete_message(state, &buffer_id, &message);
                    return;
                }
                let author = entry
                    .row_author()
                    .or_else(|| entry.author.as_ref().and_then(|a| chat.people.lock().unwrap().get(a).cloned()))
                    .unwrap_or_else(|| Principal { uuid: entry.author.clone().unwrap_or_default(), name: "someone".into(), ..Default::default() });
                chat.people.lock().unwrap().entry(author.uuid.clone()).or_insert_with(|| author.clone());
                let raw = entry.body.clone().unwrap_or_default();
                let (quoted, rest) = protocol::split_reply(&raw);
                let reply = quoted.map(|q| crate::model::ReplyPreview { id: q.message, from: q.name, body: q.text, ..Default::default() });
                let body = expand_attachment_paths(rest, self.host).into_owned();
                if body.trim().is_empty() && reply.is_none() {
                    return;
                }
                let me = chat.me.lock().unwrap().clone().unwrap_or_default();
                let mine = !me.uuid.is_empty() && me.uuid == author.uuid;
                let highlight = !mine && entry.mentions.iter().any(|m| m == &me.uuid);
                if entry.kind == "chat_message_restored" && state.runtime.update_message(state, &buffer_id, &message, &body, &[], &[]) {
                    return;
                }
                let avatar = match &author.avatar {
                    Some(raw) => cached_avatar_path(self.http, self.host, &author.uuid, &mirrored(raw, self.host)).await,
                    None => None,
                };
                state.runtime.set_replaying(&buffer_id, replaying);
                let is_new = state.runtime.record_message_at(
                    state,
                    account_id,
                    &buffer_name,
                    "channel",
                    &author.name,
                    &body,
                    false,
                    "chat",
                    reply,
                    Some(message.clone()),
                    highlight,
                    avatar,
                    Vec::new(),
                    Vec::new(),
                    Some(author.uuid.clone()),
                    entry.at,
                    None,
                    None,
                );
                state.runtime.set_replaying(&buffer_id, false);
                if is_new {
                    spawn_attachment_resolve(state.clone(), self.http.clone(), self.host, buffer_id, message, body);
                }
            }
            "chat_message_revised" => {
                if let Some(raw) = &entry.body {
                    let (_, rest) = protocol::split_reply(raw);
                    let body = expand_attachment_paths(rest, self.host).into_owned();
                    state.runtime.update_message(state, &buffer_id, &message, &body, &[], &[]);
                }
            }
            "chat_message_tombstoned" | "chat_message_purged" => state.runtime.delete_message(state, &buffer_id, &message),
            _ => {}
        }
    }
}

/// A picture's address on the host this account uses.
///
/// The chat writes its pictures on kiwifarms.net; the site serves the same
/// files on each of its hosts, and a cookie that gets past the gate on one
/// host is no use on another. The page itself does the same swap
/// (`data-chat-image-origins`).
pub(super) fn mirrored(url: &str, host: &str) -> String {
    if host == "kiwifarms.net" || !host.starts_with("kiwifarms.") {
        return url.to_string();
    }
    let suffix = host.trim_start_matches("kiwifarms.");
    for sub in ["https://kiwifarms.net/", "https://uploads.kiwifarms.net/", "https://no-cookie.kiwifarms.net/"] {
        if let Some(rest) = url.strip_prefix(sub) {
            let swapped = sub.replace("kiwifarms.net", &format!("kiwifarms.{suffix}"));
            return format!("{swapped}{rest}");
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_move_to_the_accounts_host() {
        assert_eq!(mirrored("https://kiwifarms.net/data/avatars/m/7/7421.jpg?1", "kiwifarms.st"), "https://kiwifarms.st/data/avatars/m/7/7421.jpg?1");
        assert_eq!(mirrored("https://uploads.kiwifarms.net/a.png", "kiwifarms.st"), "https://uploads.kiwifarms.st/a.png");
        assert_eq!(mirrored("https://example.com/a.png", "kiwifarms.st"), "https://example.com/a.png");
        // The onion has no mirror of its own named here: left alone.
        assert_eq!(mirrored("https://kiwifarms.net/a.png", "abc.onion"), "https://kiwifarms.net/a.png");
    }

    #[test]
    fn wanting_rooms_tracks_them_by_uuid() {
        let chat = Chat::default();
        let room = |id: &str, name: &str| SneedChatRoom { id: id.into(), name: name.into() };
        chat.want_rooms(&[room("01a1f0f8-2d29-7ac3-b779-b3b0904d6dd6", "fishtank"), room("16", "old")]);
        // A room still under an old number is not tracked until it has a uuid.
        assert_eq!(chat.rooms.lock().unwrap().len(), 1);
        assert_eq!(chat.room_of_buffer("#fishtank").as_deref(), Some("01a1f0f8-2d29-7ac3-b779-b3b0904d6dd6"));
        chat.want_rooms(&[]);
        assert!(chat.rooms.lock().unwrap().is_empty());
    }
}
