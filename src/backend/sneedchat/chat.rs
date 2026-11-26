//! The account's chat connection.
//!
//! Ephyra carries every room on one socket: the account is admitted once
//! (forum session -> ticket -> chat session), opens one socket, and joins each
//! of its rooms on it. Opening a room is a `join` on the socket that is
//! already there, closing one a `leave`.
//!
//! Two kinds of room share it. *Listed* rooms are the site's own (#fishtank),
//! chosen on the Join page and kept in the account's settings. *Side* rooms
//! are private conversations - what the site calls a whisper - which the chat
//! itself remembers: named in `welcome`, listed by `GET /v1/side_rooms` (read
//! after every welcome), and announced as they open, close or get a message by
//! `side_room_changed`. Each is a conversation of its own here, named after
//! the people in it - or, for a group chat somebody named, its title.
//!
//! The socket and every request go through the web helper (net/webhelper.rs),
//! because the site's gate admits only a browser's network stack.

use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Refusals that mean "not now" rather than "no" - worth one more try.
pub(super) const TRANSIENT: [&str; 3] = ["rate_limited", "unavailable", "internal_error"];

/// What one account's connection knows, shared with what sends on it.
#[derive(Default)]
pub(super) struct Chat {
    pub(super) me: Mutex<Option<Principal>>,
    rooms: Mutex<HashMap<String, RoomState>>,
    messages: Mutex<HashMap<String, Known>>,
    waiters: Mutex<HashMap<String, oneshot::Sender<std::result::Result<(), String>>>>,
    pub(super) people: Mutex<HashMap<String, Principal>>,
    /// This account's own reaction on each message it has been told about:
    /// a message the chat covered and named nothing for maps to None.
    own_reactions: Mutex<HashMap<String, Option<String>>>,
    /// Who is sanctioned, by principal: the sanction's id, which lifting it
    /// has to name.
    sanctions: Mutex<HashMap<String, String>>,
    frozen: AtomicBool,
    socket: Mutex<Option<WsSender>>,
    pub(super) session: Mutex<Option<Session>>,
    pub(super) host: Mutex<String>,
    /// The edge's uploads bit: whether the chat's own file hosting is taking
    /// files right now. None until the chat has said.
    pub(super) uploads: Mutex<Option<bool>>,
    /// Messages carrying a file the chat is still processing, by message, with
    /// their text as the chat wrote it (file tags and all). Taken out when the
    /// chat says the file is ready - on its `rendered` frame or on a re-read.
    pending_files: Mutex<HashMap<String, String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum RoomKind {
    #[default]
    Listed,
    Side,
}

#[derive(Clone, Debug, Default)]
struct RoomState {
    /// The conversation it is shown as: `#slug` for a listed room, the other
    /// people's names for a side room.
    buffer: String,
    kind: RoomKind,
    /// A side room's members, ourselves included.
    members: Vec<Principal>,
    /// A side room with more than one other person in it: a group chat,
    /// shown with its member list, rather than a direct message.
    group: bool,
    /// The chat calls it a group (`side_room_kind: "group"`), whoever is in
    /// it - a group with one other person left is still a group.
    declared_group: bool,
    /// A group chat's title, when somebody gave it one.
    title: Option<String>,
    last: Option<u64>,
    /// The oldest entry held, where history paging continues from.
    first: Option<u64>,
    replay_to: Option<u64>,
    joined: bool,
}

impl RoomState {
    fn buffer_kind(&self) -> &'static str {
        match self.kind {
            RoomKind::Listed => "channel",
            RoomKind::Side if self.group => "channel",
            RoomKind::Side => "dm",
        }
    }

    /// Whether a side room has more than one other person in it.
    fn is_group(members: &[Principal], me: &str) -> bool {
        members.iter().filter(|m| m.uuid != me).count() > 1
    }

    /// Takes in what the chat says of a side room - a listing row, or its
    /// `joined` - for whichever of kind, title and members it carries.
    fn take_side_row(&mut self, row: &Value, me: &str) {
        self.kind = RoomKind::Side;
        if let Some(kind) = row["side_room_kind"].as_str() {
            self.declared_group = kind == "group";
        }
        if row.get("title").is_some() {
            self.title = row["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        }
        let members: Vec<Principal> = row["members"].as_array().into_iter().flatten().filter_map(Principal::from_value).collect();
        if !members.is_empty() {
            self.members = members;
        }
        self.group = self.declared_group || Self::is_group(&self.members, me);
    }
}

/// What an edit or a removal has to name: the state the message is in,
/// which the chat refuses to change from under somebody else.
#[derive(Clone, Debug)]
pub(super) struct Known {
    pub state_seq: u64,
    pub room: String,
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

    pub(super) fn send(&self, frame: &Value) -> bool {
        match self.socket() {
            Some(s) => s.send(&frame.to_string()).is_ok(),
            None => false,
        }
    }

    pub(super) fn frozen(&self) -> bool {
        self.frozen.load(Ordering::Relaxed)
    }

    pub(super) fn joined(&self, room: &str) -> bool {
        self.rooms.lock().unwrap().get(room).is_some_and(|r| r.joined)
    }

    pub(super) fn known(&self, message: &str) -> Option<Known> {
        self.messages.lock().unwrap().get(&message.to_lowercase()).cloned()
    }

    fn remember(&self, message: &str, room: &str, state_seq: u64) {
        let mut all = self.messages.lock().unwrap();
        // Bounded: a long-lived connection in busy rooms would otherwise keep
        // every message it ever saw. What falls out is old enough that an
        // edit to it looks the state up afresh.
        if all.len() > 50_000 {
            all.clear();
        }
        all.insert(message.to_string(), Known { state_seq, room: room.to_string() });
    }

    pub(super) fn own_reaction(&self, message: &str) -> Option<String> {
        self.own_reactions.lock().unwrap().get(message).cloned().flatten()
    }

    pub(super) fn sanction_of(&self, principal: &str) -> Option<String> {
        self.sanctions.lock().unwrap().get(principal).cloned()
    }

    /// Waits for the answer to something sent under `key` - a post's nonce, a
    /// command's operation id, or a message's uuid for a reaction.
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

    /// Sends a frame that expects an answer under `key`, and waits for it.
    ///
    /// A refusal that means "not now" is tried once more after a moment. A
    /// durable command (one with an operation id) whose answer never comes is
    /// asked after with `query_operation`, which answers with what became of
    /// it - so a lost answer is not reported as a failure that went through.
    pub(super) async fn command(&self, key: &str, frame: Value) -> Result<()> {
        if self.frozen() && frame["t"] != "query_operation" {
            bail!("{}", protocol::describe_error("frozen"));
        }
        let durable = frame.get("chat_operation_uuid").is_some();
        let mut retried = false;
        loop {
            let rx = self.expect(key);
            if !self.send(&frame) {
                self.give_up(key);
                bail!("not connected to the chat right now");
            }
            let outcome = match tokio::time::timeout(Duration::from_secs(15), rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => bail!("the chat connection dropped before it answered"),
                Err(_) => {
                    self.give_up(key);
                    if !durable {
                        bail!("the chat hasn't answered - it may still go through");
                    }
                    let rx = self.expect(key);
                    if !self.send(&json!({ "t": "query_operation", "chat_operation_uuid": key })) {
                        self.give_up(key);
                        bail!("the chat hasn't answered - it may still go through");
                    }
                    match tokio::time::timeout(Duration::from_secs(10), rx).await {
                        Ok(Ok(result)) => result,
                        _ => {
                            self.give_up(key);
                            bail!("the chat hasn't answered - it may still go through");
                        }
                    }
                }
            };
            match outcome {
                Ok(()) => return Ok(()),
                // Asked after and never heard of: it was lost on the way, so
                // it goes again (same id, so it cannot be done twice).
                Err(code) if code == "operation_not_found" && !retried => retried = true,
                Err(code) if TRANSIENT.contains(&code.as_str()) && !retried => {
                    retried = true;
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
                Err(code) => bail!("{}", protocol::describe_error(&code)),
            }
        }
    }

    /// The listed rooms the account wants, as the connection tracks them: new
    /// ones added (and joined, when the socket is up), dropped ones left. Side
    /// rooms are the chat's to list and are left alone.
    pub(super) fn want_rooms(&self, rooms: &[SneedChatRoom]) {
        let mut joins = Vec::new();
        let mut leaves = Vec::new();
        {
            let mut tracked = self.rooms.lock().unwrap();
            tracked.retain(|uuid, state| {
                let keep = state.kind == RoomKind::Side || rooms.iter().any(|r| &r.id == uuid);
                if !keep && state.joined {
                    leaves.push(uuid.clone());
                }
                keep
            });
            for room in rooms.iter().filter(|r| protocol::is_uuid(&r.id)) {
                let state = tracked.entry(room.id.clone()).or_default();
                state.buffer = room_buffer_name(&room.name);
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

    /// A side room the chat has told us of, tracked and joined.
    fn want_side_room(&self, uuid: &str, after: Option<u64>) {
        let join = {
            let mut tracked = self.rooms.lock().unwrap();
            let state = tracked.entry(uuid.to_string()).or_insert_with(|| RoomState { kind: RoomKind::Side, ..Default::default() });
            state.kind = RoomKind::Side;
            !state.joined
        };
        if join {
            self.join(uuid, after);
        }
    }

    /// Every side room the chat lists for this account, page by page, each
    /// taken in and joined: what `welcome` names is not the whole of it.
    async fn list_side_rooms(&self, session: &Session, host: &str) -> Result<()> {
        let me = self.me.lock().unwrap().as_ref().map(|m| m.uuid.clone()).unwrap_or_default();
        let mut cursor: Option<String> = None;
        // A bound, not a limit anybody should meet: 20 pages of 100.
        for _ in 0..20 {
            let path = match &cursor {
                Some(c) => format!("side_rooms?limit=100&cursor={}", http::percent_encode(c)),
                None => "side_rooms?limit=100".to_string(),
            };
            let page = api_get(session, host, &path).await?;
            for row in page["rooms"].as_array().into_iter().flatten() {
                let Some(uuid) = row["chat_room_uuid"].as_str().map(str::to_lowercase) else { continue };
                {
                    let mut rooms = self.rooms.lock().unwrap();
                    let room = rooms.entry(uuid.clone()).or_insert_with(|| RoomState { kind: RoomKind::Side, ..Default::default() });
                    room.take_side_row(row, &me);
                }
                let after = self.rooms.lock().unwrap().get(&uuid).and_then(|r| r.last);
                self.want_side_room(&uuid, after);
            }
            cursor = page["page"]["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(())
    }

    fn join(&self, uuid: &str, after: Option<u64>) {
        let mut frame = json!({ "t": "join", "chat_room_uuid": uuid });
        if let Some(seq) = after {
            frame["after_seq"] = json!(seq);
        }
        self.send(&frame);
    }

    /// The conversation a room is shown as, and its kind.
    fn buffer_of(&self, uuid: &str) -> Option<(String, &'static str)> {
        self.rooms.lock().unwrap().get(uuid).filter(|r| !r.buffer.is_empty()).map(|r| (r.buffer.clone(), r.buffer_kind()))
    }

    /// The room a conversation shows, by uuid.
    pub(super) fn room_of_buffer(&self, buffer_name: &str) -> Option<String> {
        self.rooms.lock().unwrap().iter().find(|(_, r)| r.buffer == buffer_name).map(|(uuid, _)| uuid.clone())
    }

    /// Leaves the side room a conversation shows, if it is one: the chat is
    /// told, and the room is no longer tracked, so nothing in it reopens the
    /// conversation. Whether it was a side room.
    /// Whether a conversation is a side room (a whisper or group chat) rather
    /// than a listed channel. The chat's file hosting refuses side rooms.
    pub(super) fn is_side_room(&self, buffer_name: &str) -> bool {
        let Some(room) = self.room_of_buffer(buffer_name) else { return false };
        self.rooms.lock().unwrap().get(&room).is_some_and(|r| r.kind == RoomKind::Side)
    }

    pub(super) fn leave_side_room(&self, buffer_name: &str) -> bool {
        let uuid = {
            let mut rooms = self.rooms.lock().unwrap();
            let Some(uuid) = rooms.iter().find(|(_, r)| r.kind == RoomKind::Side && r.buffer == buffer_name).map(|(u, _)| u.clone()) else {
                return false;
            };
            rooms.remove(&uuid);
            uuid
        };
        self.send(&json!({ "t": "leave", "chat_room_uuid": uuid }));
        true
    }

    /// This account's side rooms: (uuid, conversation name, whether a group).
    pub(super) fn side_rooms(&self) -> Vec<(String, String, bool)> {
        let mut out: Vec<(String, String, bool)> = self
            .rooms
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| r.kind == RoomKind::Side && !r.buffer.is_empty())
            .map(|(u, r)| (u.clone(), r.buffer.clone(), r.group))
            .collect();
        out.sort_by_key(|r| r.1.to_lowercase());
        out
    }

    /// Whether somebody is already in a side room.
    pub(super) fn in_room(&self, room: &str, principal: &str) -> bool {
        self.rooms.lock().unwrap().get(room).is_some_and(|r| r.members.iter().any(|m| m.uuid == principal))
    }

    /// The side room that is a conversation with exactly this person, if any.
    pub(super) fn side_room_with(&self, principal: &str) -> Option<(String, String)> {
        let me = self.me.lock().unwrap().as_ref().map(|m| m.uuid.clone()).unwrap_or_default();
        self.rooms
            .lock()
            .unwrap()
            .iter()
            .find(|(_, r)| {
                r.kind == RoomKind::Side && {
                    let others: Vec<&str> = r.members.iter().map(|m| m.uuid.as_str()).filter(|u| *u != me).collect();
                    others == [principal]
                }
            })
            .map(|(uuid, r)| (uuid.clone(), r.buffer.clone()))
    }

    /// Somebody, by the name shown for them.
    pub(super) fn person_named(&self, name: &str) -> Option<Principal> {
        let name = name.trim().trim_start_matches('@');
        self.people.lock().unwrap().values().find(|p| p.name.eq_ignore_ascii_case(name)).cloned()
    }

    /// Everyone known, as (uuid, name) for writing mentions.
    pub(super) fn known_names(&self) -> Vec<(String, String)> {
        self.people.lock().unwrap().values().map(|p| (p.uuid.clone(), p.name.clone())).collect()
    }

    /// The oldest entry held in a room and whether more is before it.
    pub(super) fn history_cursor(&self, room: &str) -> Option<u64> {
        self.rooms.lock().unwrap().get(room).and_then(|r| r.first).filter(|f| *f > 1)
    }

    fn note_first(&self, room: &str, seq: u64) {
        if let Some(r) = self.rooms.lock().unwrap().get_mut(room) {
            if r.first.is_none_or(|f| seq < f) {
                r.first = Some(seq);
            }
        }
    }
}

/// Gets this session admitted to the chat: a ticket from the forum, traded
/// for the chat's own session cookie. The chat's own page is the one asked
/// for the ticket's form token, because it also carries what each room lets
/// this account do and the reactions the site offers.
pub(super) async fn admit(session: &Session, host: &str) -> Result<()> {
    let base = format!("https://{host}");
    let page = session.fetch(&format!("{base}/ephyra-chat/")).await.context("loading the chat page")?;
    let page = if page.body.contains("data-csrf=\"") { page } else { session.fetch(&format!("{base}/")).await.context("loading the forum")? };
    let token = protocol::attr(&page.body, "data-csrf").context("the forum page carries no form token - its markup may have changed")?;
    super::catalogue::take_page(&session.http, host, &page.body).await;
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
pub(super) async fn run_connection(state: &AppState, account_id: &str, session: &Session, host: &str) -> Result<()> {
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
                    if let Err(re) = session.refresh().await {
                        tracing::warn!("sneedchat[{account_id}]: refreshing the forum session: {re:#}");
                    }
                }
            }
        }
        let buffers: Vec<String> = chat.rooms.lock().unwrap().values().map(|r| r.buffer.clone()).filter(|b| !b.is_empty()).collect();
        for name in buffers {
            let buffer_id = crate::model::buffer_id(account_id, &name);
            state.runtime.set_buffer_link(state, &buffer_id, Some(crate::model::BufferLink::connecting()));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX);
    }
}

async fn connect_once(state: &AppState, account_id: &str, chat: &Arc<Chat>, session: &Session, host: &str, first: bool) -> Result<()> {
    admit(session, host).await?;
    // The reaction set is read with the chat page; a window that asked
    // before then got none, and is told to ask again.
    state.events.emit("sneedchatReactions", json!({}));
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

    let ctx = Ctx { state, account_id, chat, session, http: &session.http, host, sender: &sender };
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

pub(super) struct Ctx<'a> {
    pub(super) state: &'a AppState,
    pub(super) account_id: &'a str,
    pub(super) chat: &'a Arc<Chat>,
    pub(super) session: &'a Session,
    pub(super) http: &'a http::HttpClient,
    pub(super) host: &'a str,
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
                self.set_frozen(frame["link_state"].as_str() == Some("frozen"));
                if let Some(available) = frame["uploads_available"].as_bool() {
                    *chat.uploads.lock().unwrap() = Some(available);
                }
                super::health::recovered(account_id);
                let rooms = state.accounts.get_sneedchat(account_id).map(|c| c.rooms).unwrap_or_default();
                chat.want_rooms(&rooms);
                for room in frame["side_rooms"].as_array().into_iter().flatten() {
                    if let Some(uuid) = room["chat_room_uuid"].as_str().map(str::to_lowercase) {
                        let after = chat.rooms.lock().unwrap().get(&uuid).and_then(|r| r.last);
                        chat.want_side_room(&uuid, after);
                    }
                }
                // The site's own client reads the list after every welcome
                // rather than trusting what welcome names; so does this.
                tokio::spawn({
                    let (chat, session, host, account_id) = (chat.clone(), self.session.clone(), self.host.to_string(), account_id.to_string());
                    async move {
                        if let Err(e) = chat.list_side_rooms(&session, &host).await {
                            tracing::warn!("sneedchat[{account_id}]: reading the side rooms: {e:#}");
                        }
                    }
                });
                let revision = frame["emoticon_revision"].as_str().map(str::to_string);
                self.refresh_emoticons(revision);
            }
            "emoticons_changed" => self.refresh_emoticons(frame["emoticon_revision"].as_str().map(str::to_string)),
            "link_state" => self.set_frozen(frame["state"].as_str() == Some("frozen")),
            "uploads" => {
                if let Some(available) = frame["available"].as_bool() {
                    *chat.uploads.lock().unwrap() = Some(available);
                }
            }
            "joined" => self.joined(frame),
            "side_room_changed" => self.side_room_changed(frame),
            "left" => {
                let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default();
                let reason = frame["reason"].as_str().unwrap_or("no reason given").to_string();
                let buffer = {
                    let mut rooms = chat.rooms.lock().unwrap();
                    rooms.get_mut(uuid).map(|room| {
                        room.joined = false;
                        (room.buffer.clone(), room.buffer_kind())
                    })
                };
                tracing::info!("sneedchat[{account_id}]: left {uuid} ({reason})");
                // Put out of a room by the chat rather than leaving it: said in it.
                if let Some((name, kind)) = buffer.filter(|(b, _)| !b.is_empty()) {
                    if reason.contains("sanction") || reason.contains("ban") || reason.contains("kick") {
                        let said = format!("You were removed from this room ({}).", reason.replace('_', " "));
                        state.runtime.record_message(state, account_id, &name, kind, "", &said, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                    }
                }
            }
            "resync" => {
                let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default().to_string();
                if let Some(room) = chat.rooms.lock().unwrap().get_mut(&uuid) {
                    room.last = None;
                    room.joined = false;
                }
                chat.join(&uuid, None);
            }
            "entry" | "elided" => self.entry(frame, false).await,
            "own_reactions" => {
                // The replay is over: what it named holds this reaction of
                // ours, and anything it covered without naming holds none.
                let room = frame["chat_room_uuid"].as_str().unwrap_or_default();
                let named: Vec<(String, String)> = frame["reactions"]
                    .as_object()
                    .map(|m| m.iter().filter_map(|(k, v)| Some((k.to_lowercase(), v.as_str()?.to_string()))).collect())
                    .unwrap_or_default();
                let buffer = chat.buffer_of(room);
                {
                    let mut own = chat.own_reactions.lock().unwrap();
                    for (message, code) in &named {
                        own.insert(message.clone(), Some(code.clone()));
                    }
                }
                if let Some((name, _)) = buffer {
                    let buffer_id = crate::model::buffer_id(account_id, &name);
                    for (message, _) in named {
                        self.mark_own(&buffer_id, &message);
                    }
                }
            }
            "rendered" => {
                // A row drawn again - its reactions, or a file that was processing, are what can have changed.
                if let (Some(message), Some(html)) = (frame["chat_message_uuid"].as_str(), frame["html"].as_str()) {
                    let room = frame["chat_room_uuid"].as_str().map(str::to_string).or_else(|| chat.known(message).map(|k| k.room));
                    if let Some((name, _)) = room.and_then(|r| chat.buffer_of(&r)) {
                        let buffer_id = crate::model::buffer_id(account_id, &name);
                        self.set_reactions(&buffer_id, &message.to_lowercase(), html);
                        // And a file that was still processing may be ready now.
                        pending_file_ready(state, chat, self.http, self.host, &buffer_id, &message.to_lowercase(), html);
                    }
                }
            }
            "presence" => {
                let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default();
                let principals: Vec<Principal> = frame["principals"].as_array().into_iter().flatten().filter_map(Principal::from_value).collect();
                {
                    let mut people = chat.people.lock().unwrap();
                    for p in &principals {
                        people.insert(p.uuid.clone(), p.clone());
                    }
                }
                if let Some((name, _)) = chat.buffer_of(uuid) {
                    super::rooms::publish_presence(state, self.http, self.host, account_id, &name, &principals);
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
                let key = frame["ref"].as_str().unwrap_or_default().to_lowercase();
                let code = frame["code"].as_str().unwrap_or("refused").to_string();
                let answered = chat.answer(&key, Err(code.clone()))
                    || chat.answer(frame["ref"].as_str().unwrap_or_default(), Err(code.clone()))
                    || chat.answer(&format!("react:{key}"), Err(code.clone()))
                    || chat.answer(&format!("invite:{key}"), Err(code.clone()));
                if !answered {
                    if let Some((name, kind)) = chat.buffer_of(&key) {
                        let said = format!("Couldn't join this room: {}.", protocol::describe_error(&code));
                        state.runtime.record_message(state, account_id, &name, kind, "", &said, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                    } else {
                        tracing::warn!("sneedchat[{account_id}]: chat error {code} (ref {key})");
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A side room opened, closed, renamed or written in, said to the whole
    /// connection (`side_room_changed`). A room this account was not told of
    /// before - somebody opened one with us, or wrote in one not joined - is
    /// joined now, and its `joined` makes the conversation; a closed one is
    /// let go of, the conversation kept with a line saying so.
    fn side_room_changed(&self, frame: &Value) {
        let (state, account_id, chat) = (self.state, self.account_id, self.chat);
        let Some(uuid) = frame["chat_room_uuid"].as_str().map(str::to_lowercase) else { return };
        match frame["change"].as_str().unwrap_or_default() {
            "closed" => {
                let gone = chat.rooms.lock().unwrap().remove(&uuid);
                if let Some(room) = gone.filter(|r| !r.buffer.is_empty()) {
                    state.runtime.record_message(state, account_id, &room.buffer, room.buffer_kind(), "", "This conversation was closed.", false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                }
            }
            _ => {
                let after = chat.rooms.lock().unwrap().get(&uuid).and_then(|r| r.last);
                chat.want_side_room(&uuid, after);
            }
        }
    }

    /// A side room shown as what it is: a direct message with one other
    /// person, or - with more - a group chat under its own heading, with its
    /// member list. Called when it is joined and whenever its members change.
    fn settle_side_room(&self, uuid: &str) {
        let (state, account_id, chat) = (self.state, self.account_id, self.chat);
        let Some((name, kind, members)) = chat.rooms.lock().unwrap().get(uuid).filter(|r| r.kind == RoomKind::Side && !r.buffer.is_empty()).map(|r| (r.buffer.clone(), r.buffer_kind(), r.members.clone())) else {
            return;
        };
        let buffer_id = crate::model::buffer_id(account_id, &name);
        state.runtime.set_buffer_kind(state, &buffer_id, kind);
        state.runtime.set_buffer_category(state, &buffer_id, (kind == "channel").then_some("Group chats"), 0);
        super::rooms::publish_presence(state, self.http, self.host, account_id, &name, &members);
    }

    fn refresh_emoticons(&self, revision: Option<String>) {
        let (state, session, host) = (self.state.clone(), self.session.clone(), self.host.to_string());
        tokio::spawn(async move { super::catalogue::take_emoticons(&state, &session, &host, revision.as_deref()).await });
    }

    /// The chat stopped (or started again) taking anything but reads. Said in
    /// each room once, when it changes.
    fn set_frozen(&self, frozen: bool) {
        let was = self.chat.frozen.swap(frozen, Ordering::Relaxed);
        if was == frozen {
            return;
        }
        let said = if frozen { "The chat is frozen: nothing can be sent until it is back." } else { "The chat is back: messages can be sent again." };
        let buffers: Vec<(String, &'static str)> = self.chat.rooms.lock().unwrap().values().filter(|r| r.joined && !r.buffer.is_empty()).map(|r| (r.buffer.clone(), r.buffer_kind())).collect();
        for (name, kind) in buffers {
            self.state.runtime.record_message(self.state, self.account_id, &name, kind, "", said, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
        }
    }

    fn joined(&self, frame: &Value) {
        let (state, account_id, chat) = (self.state, self.account_id, self.chat);
        let uuid = frame["chat_room_uuid"].as_str().unwrap_or_default();
        let head = frame["head_seq"].as_u64();
        let side = frame["chat_room_kind"].as_str() == Some("side");
        let members: Vec<Principal> = frame["members"].as_array().into_iter().flatten().filter_map(Principal::from_value).collect();
        if !members.is_empty() {
            let mut people = chat.people.lock().unwrap();
            for p in &members {
                people.insert(p.uuid.clone(), p.clone());
            }
        }
        let me = chat.me.lock().unwrap().as_ref().map(|m| m.uuid.clone()).unwrap_or_default();
        let (name, kind) = {
            let mut rooms = chat.rooms.lock().unwrap();
            let room = match rooms.get_mut(uuid) {
                Some(r) => r,
                // A side room nobody here asked for: somebody opened one with us.
                None if side => rooms.entry(uuid.to_string()).or_insert_with(|| RoomState { kind: RoomKind::Side, ..Default::default() }),
                None => return,
            };
            room.joined = true;
            room.replay_to = head;
            if room.last.is_none() && head == Some(0) {
                room.last = Some(0);
            }
            if side {
                room.take_side_row(frame, &me);
                if room.buffer.is_empty() {
                    room.buffer = side_room_name(&room.members, &me, uuid, room.group.then_some(room.title.as_deref()).flatten());
                }
            }
            (room.buffer.clone(), room.buffer_kind())
        };
        if name.is_empty() {
            return;
        }
        let buffer = state.runtime.ensure_buffer(state, account_id, &name, kind);
        state.runtime.set_buffer_link(state, &buffer.id, None);
        if side {
            self.settle_side_room(uuid);
        } else if let Some(subtitle) = super::catalogue::subtitle(uuid) {
            state.runtime.set_buffer_topic(state, &buffer.id, &subtitle);
        }
        // Whoever opened a side room with us is waiting on this.
        for m in chat.rooms.lock().unwrap().get(uuid).map(|r| r.members.clone()).unwrap_or_default() {
            chat.answer(&format!("side:{}", m.uuid), Ok(()));
        }
        tracing::info!("sneedchat[{account_id}]: joined {name}");
    }

    /// One log entry, live or from a history page (`paged`): history is never
    /// news, and is not held to the live sequence.
    pub(super) async fn entry(&self, frame: &Value, paged: bool) {
        let (state, account_id, chat) = (self.state, self.account_id, self.chat);
        let Some(mut entry) = Entry::from_value(frame) else { return };
        if paged && entry.room.is_empty() {
            entry.room = frame["_room"].as_str().unwrap_or_default().to_string();
        }
        let replaying = if paged {
            chat.note_first(&entry.room, entry.room_seq);
            true
        } else {
            // In order, once each: a gap means something was missed, and the
            // chat replays it on a fresh join from the last seen.
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
            if room.first.is_none_or(|f| entry.room_seq < f) {
                room.first = Some(entry.room_seq);
            }
            room.replay_to.is_some_and(|head| entry.room_seq <= head)
        };
        // A post's own echo answers whoever sent it, whatever else it is.
        if let Some(nonce) = &entry.client_nonce {
            chat.answer(nonce, Ok(()));
        }
        if frame["t"] == "elided" || frame["elided"].as_bool() == Some(true) {
            return;
        }
        let Some((buffer_name, buffer_kind)) = chat.buffer_of(&entry.room) else { return };
        let buffer_id = crate::model::buffer_id(account_id, &buffer_name);
        if let (Some(message), Some(seq)) = (&entry.message, entry.state_seq) {
            chat.remember(message, &entry.room, seq);
        }
        let system = |text: String| {
            state.runtime.set_replaying(&buffer_id, replaying);
            let id = frame["entry_uuid"].as_str().map(|e| format!("entry:{e}"));
            state.runtime.record_message_at(state, account_id, &buffer_name, buffer_kind, "", &text, false, "system", None, id, false, None, Vec::new(), Vec::new(), None, entry.at, None, None);
            state.runtime.set_replaying(&buffer_id, false);
        };
        let who = |key: &str| -> String {
            frame[key]
                .as_str()
                .and_then(|u| chat.people.lock().unwrap().get(u).map(|p| p.name.clone()))
                .or_else(|| frame["display_name"].as_str().map(str::to_string))
                .unwrap_or_else(|| "someone".to_string())
        };
        match entry.kind.as_str() {
            "chat_room_subtitle_set" => {
                state.runtime.set_buffer_topic(state, &buffer_id, entry.subtitle.as_deref().unwrap_or_default());
                return;
            }
            "chat_side_room_member_added" | "chat_side_room_member_left" | "chat_side_room_member_removed" => {
                let added = entry.kind.ends_with("added");
                let removed = entry.kind.ends_with("removed");
                let name = who("principal_uuid");
                if let Some(p) = frame["principal_uuid"].as_str().map(str::to_string) {
                    let me = chat.me.lock().unwrap().as_ref().map(|m| m.uuid.clone()).unwrap_or_default();
                    let mut rooms = chat.rooms.lock().unwrap();
                    if let Some(room) = rooms.get_mut(&entry.room) {
                        if added && !room.members.iter().any(|m| m.uuid == p) {
                            let known = chat.people.lock().unwrap().get(&p).cloned();
                            room.members.push(known.unwrap_or(Principal { uuid: p, name: name.clone(), ..Default::default() }));
                        } else if !added {
                            room.members.retain(|m| m.uuid != p);
                        }
                        room.group = room.declared_group || RoomState::is_group(&room.members, &me);
                    }
                }
                if added {
                    chat.answer(&format!("invite:{}", entry.room), Ok(()));
                }
                system(format!("{name} {} the conversation.", if added { "joined" } else if removed { "was removed from" } else { "left" }));
                if !replaying {
                    self.settle_side_room(&entry.room);
                }
                return;
            }
            "chat_side_room_title_set" => {
                let title = frame["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
                if let Some(room) = chat.rooms.lock().unwrap().get_mut(&entry.room) {
                    room.title = title.clone();
                }
                let name = who("actor_principal_uuid");
                system(match title {
                    Some(t) => format!("{name} renamed the conversation to {t}."),
                    None => format!("{name} cleared the conversation's name."),
                });
                return;
            }
            "chat_side_room_owner_set" => {
                system(format!("{} is now the conversation's owner.", who("principal_uuid")));
                return;
            }
            "chat_sanction_applied" | "chat_sanction_lifted" => {
                let applied = entry.kind.ends_with("applied");
                let target = frame["principal_uuid"].as_str().unwrap_or_default().to_string();
                if let Some(id) = frame["chat_sanction_uuid"].as_str() {
                    let mut sanctions = chat.sanctions.lock().unwrap();
                    if applied {
                        sanctions.insert(target.clone(), id.to_string());
                    } else {
                        sanctions.remove(&target);
                    }
                }
                let kind = frame["sanction_kind"].as_str().unwrap_or("mute");
                let name = who("principal_uuid");
                let text = if applied {
                    let until = frame["expires_at"].as_i64().map(|t| format!(" until {}", chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default())).unwrap_or_default();
                    format!("{name} was {}{until}.", if kind == "ban" { "banned" } else { "muted" })
                } else {
                    format!("{name} is no longer {}.", if kind == "ban" { "banned" } else { "muted" })
                };
                system(text);
                return;
            }
            "chat_frozen" => {
                self.set_frozen(true);
                return;
            }
            _ => {}
        }
        let Some(message) = entry.message.clone() else { return };
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
                let raw = protocol::mentions_to_names(&entry.body.clone().unwrap_or_default());
                let (quoted, rest) = protocol::split_reply(&raw);
                let reply = quoted.map(|q| crate::model::ReplyPreview { id: q.message, from: q.name, body: q.text, ..Default::default() });
                // Files the chat hosts arrive as tags; until they are fetched
                // the message reads as their names.
                let tiles = entry.html.as_deref().map(media::file_tiles).unwrap_or_default();
                let rest_with_tags = rest.to_string();
                let body = expand_attachment_paths(&media::file_tags_with_progress(rest, &tiles), self.host).into_owned();
                if body.trim().is_empty() && reply.is_none() && tiles.is_empty() {
                    return;
                }
                let me = chat.me.lock().unwrap().clone().unwrap_or_default();
                let mine = !me.uuid.is_empty() && me.uuid == author.uuid;
                let highlight = !mine && (entry.mentions.iter().any(|m| m == &me.uuid) || buffer_kind == "dm");
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
                    buffer_kind,
                    &author.name,
                    &body,
                    false,
                    "chat",
                    reply,
                    Some(message.clone()),
                    highlight,
                    avatar,
                    Vec::new(),
                    // A file still being processed holds its place until it is ready.
                    tiles.iter().filter(|t| t.pending).map(media::processing_placeholder).collect(),
                    Some(author.uuid.clone()),
                    entry.at,
                    None,
                    None,
                );
                state.runtime.set_replaying(&buffer_id, false);
                if let Some(html) = &entry.html {
                    self.set_reactions(&buffer_id, &message, html);
                }
                if is_new {
                    if tiles.is_empty() {
                        spawn_attachment_resolve(state.clone(), self.http.clone(), self.host, buffer_id, message, body);
                    } else {
                        // A file still being processed has no address yet: the
                        // chat re-renders the row when it is ready, and this
                        // also asks, in case that frame is missed.
                        if tiles.iter().any(|t| t.pending) {
                            chat.pending_files.lock().unwrap().insert(message.clone(), rest_with_tags.clone());
                            watch_pending_file(state.clone(), chat.clone(), self.session.clone(), self.http.clone(), self.host.to_string(), buffer_id.clone(), message.clone());
                        }
                        spawn_file_resolve(state.clone(), self.http.clone(), self.host, buffer_id, message, rest_with_tags, tiles);
                    }
                }
            }
            "chat_message_revised" => {
                if let Some(raw) = &entry.body {
                    let raw = protocol::mentions_to_names(raw);
                    let (_, rest) = protocol::split_reply(&raw);
                    let body = expand_attachment_paths(&media::file_tags_as_labels(rest), self.host).into_owned();
                    state.runtime.update_message(state, &buffer_id, &message, &body, &[], &[]);
                }
            }
            "chat_message_reaction_set" => {
                let me = chat.me.lock().unwrap().as_ref().map(|m| m.uuid.clone()).unwrap_or_default();
                if frame["principal_uuid"].as_str().is_some_and(|p| p == me) {
                    let code = frame["reaction_code"].as_str().map(str::to_string);
                    chat.own_reactions.lock().unwrap().insert(message.clone(), code);
                    chat.answer(&format!("react:{message}"), Ok(()));
                }
                match &entry.html {
                    Some(html) => self.set_reactions(&buffer_id, &message, html),
                    None => self.adjust_reaction(&buffer_id, &message, frame),
                }
            }
            "chat_message_tombstoned" | "chat_message_purged" => state.runtime.delete_message(state, &buffer_id, &message),
            _ => {}
        }
    }

    /// A message's reactions as its rendered row tallies them, with ours marked.
    fn set_reactions(&self, buffer_id: &str, message: &str, html: &str) {
        let chips = protocol::chips(html);
        let own = self.chat.own_reaction(message);
        let held = self.state.store.get_message(buffer_id, message).ok().flatten().map(|m| m.reactions).unwrap_or_default();
        if chips.is_empty() && held.is_empty() {
            return;
        }
        let reactions: Vec<crate::model::Reaction> = chips
            .into_iter()
            .map(|(code, count)| crate::model::Reaction { emoji: super::catalogue::reaction_token(&code), count, me: own.as_deref() == Some(code.as_str()), animated: false })
            .collect();
        if self.state.store.set_reactions(buffer_id, message, &reactions).unwrap_or(false) {
            self.state.events.emit("reactionsChanged", json!({ "bufferId": buffer_id, "id": message, "reactions": reactions }));
        }
    }

    /// A reaction change with no row to read: one person's reaction moved, so
    /// one tally goes down and another up.
    fn adjust_reaction(&self, buffer_id: &str, message: &str, frame: &Value) {
        let Some(stored) = self.state.store.get_message(buffer_id, message).ok().flatten() else { return };
        let mut held = stored.reactions;
        let me = self.chat.me.lock().unwrap().as_ref().map(|m| m.uuid.clone()).unwrap_or_default();
        let mine = frame["principal_uuid"].as_str() == Some(me.as_str());
        if let Some(previous) = frame["previous_reaction_code"].as_str() {
            let token = super::catalogue::reaction_token(previous);
            if let Some(r) = held.iter_mut().find(|r| super::catalogue::reaction_code(&r.emoji).as_deref() == Some(previous)) {
                r.count -= 1;
                r.emoji = token;
                if mine {
                    r.me = false;
                }
            }
        }
        if let Some(code) = frame["reaction_code"].as_str() {
            match held.iter_mut().find(|r| super::catalogue::reaction_code(&r.emoji).as_deref() == Some(code)) {
                Some(r) => {
                    r.count += 1;
                    r.me |= mine;
                }
                None => held.push(crate::model::Reaction { emoji: super::catalogue::reaction_token(code), count: 1, me: mine, animated: false }),
            }
        }
        held.retain(|r| r.count > 0);
        if self.state.store.set_reactions(buffer_id, message, &held).unwrap_or(false) {
            self.state.events.emit("reactionsChanged", json!({ "bufferId": buffer_id, "id": message, "reactions": held }));
        }
    }

    /// Marks our own reaction on a message whose tallies are already held.
    fn mark_own(&self, buffer_id: &str, message: &str) {
        let Some(stored) = self.state.store.get_message(buffer_id, message).ok().flatten() else { return };
        let own = self.chat.own_reaction(message);
        let mut held = stored.reactions;
        let mut changed = false;
        for r in held.iter_mut() {
            let me = own.as_deref().is_some_and(|c| super::catalogue::reaction_code(&r.emoji).as_deref() == Some(c));
            if r.me != me {
                r.me = me;
                changed = true;
            }
        }
        if changed && self.state.store.set_reactions(buffer_id, message, &held).unwrap_or(false) {
            self.state.events.emit("reactionsChanged", json!({ "bufferId": buffer_id, "id": message, "reactions": held }));
        }
    }
}

/// What a side room is called: a group chat's title when it has one, else
/// the other people in it, by name.
fn side_room_name(members: &[Principal], me: &str, uuid: &str, title: Option<&str>) -> String {
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        return t.to_string();
    }
    let others: Vec<&str> = members.iter().filter(|m| m.uuid != me).map(|m| m.name.as_str()).filter(|n| !n.is_empty()).collect();
    if others.is_empty() {
        format!("whisper-{}", &uuid[uuid.len().saturating_sub(6)..])
    } else {
        others.join(", ")
    }
}

/// What to do with a message's re-rendered row when it carried a file that was
/// still processing: once none is, fetch them and attach them.
fn pending_file_ready(state: &AppState, chat: &Arc<Chat>, http: &http::HttpClient, host: &str, buffer_id: &str, message: &str, html: &str) {
    let tiles = media::file_tiles(html);
    if tiles.is_empty() || tiles.iter().any(|t| t.pending) {
        return;
    }
    let Some(body) = chat.pending_files.lock().unwrap().remove(message) else { return };
    spawn_file_resolve(state.clone(), http.clone(), host, buffer_id.to_string(), message.to_string(), body, tiles);
}

/// Asks the chat now and then whether a processing file is ready, the way the
/// site's own page does (`GET /v1/messages/<id>`, 5 seconds after the row and
/// then doubling), for the half hour after which a failed conversion is left as
/// it is. The `rendered` frame usually gets there first, but it is sent once
/// and never replayed, so a socket that was away would otherwise never hear.
fn watch_pending_file(state: AppState, chat: Arc<Chat>, session: Session, http: http::HttpClient, host: String, buffer_id: String, message: String) {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut wait = Duration::from_secs(5);
        loop {
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(60));
            if !chat.pending_files.lock().unwrap().contains_key(&message) {
                return;
            }
            if started.elapsed() > Duration::from_secs(30 * 60) {
                chat.pending_files.lock().unwrap().remove(&message);
                return;
            }
            let Ok(read) = api_get(&session, &host, &format!("messages/{message}")).await else { continue };
            if let Some(html) = read["html"].as_str() {
                pending_file_ready(&state, &chat, &http, &host, &buffer_id, &message, html);
            }
        }
    });
}

/// Reads the page of a room's log before the oldest entry held, as history:
/// no alerts, the same messages as ever.
pub(super) async fn older(state: &AppState, account_id: &str, buffer_name: &str) -> Result<()> {
    let chat = chat(account_id);
    let room = chat.room_of_buffer(buffer_name).ok_or_else(|| anyhow!("not one of this account's rooms"))?;
    let Some(before) = chat.history_cursor(&room) else { return Ok(()) };
    let session = chat.session.lock().unwrap().clone().ok_or_else(|| anyhow!("not connected to the chat right now"))?;
    let host = chat.host.lock().unwrap().clone();
    let page = api_get(&session, &host, &format!("rooms/{room}/log?before={before}&limit=100&unit=messages")).await?;
    let sender = chat.socket().ok_or_else(|| anyhow!("not connected to the chat right now"))?;
    let ctx = Ctx { state, account_id, chat: &chat, session: &session, http: &session.http, host: &host, sender: &sender };
    let entries = page["entries"].as_array().cloned().unwrap_or_default();
    if let Some(own) = page["own_reactions"].as_object() {
        let mut held = chat.own_reactions.lock().unwrap();
        for (k, v) in own {
            held.insert(k.to_lowercase(), v.as_str().map(str::to_string));
        }
    }
    for mut e in entries {
        e["_room"] = json!(room);
        ctx.entry(&e, true).await;
    }
    // Nothing older than this page: paging stops here.
    if page["page"]["prev_before"].is_null() {
        if let Some(r) = chat.rooms.lock().unwrap().get_mut(&room) {
            r.first = Some(1);
        }
    }
    Ok(())
}

/// A picture's address on the host this account uses.
///
/// The chat writes its pictures on kiwifarms.net; the site serves the same
/// files on each of its hosts, and a cookie that gets past the gate on one
/// host is no use on another. The page itself does the same swap
/// (`data-chat-image-origins`).
pub(super) fn mirrored(url: &str, host: &str) -> String {
    // Through Tor the account is on the onion, which serves the forum's own
    // files (pictures included) under its own name.
    if host.ends_with(".onion") {
        for clear in ["https://kiwifarms.net/", "https://kiwifarms.st/"] {
            if let Some(rest) = url.strip_prefix(clear) {
                return format!("https://{host}/{rest}");
            }
        }
        return url.to_string();
    }
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
        // Through Tor, the forum's own files come from the onion.
        assert_eq!(mirrored("https://kiwifarms.net/data/a.png", "abc.onion"), "https://abc.onion/data/a.png");
        assert_eq!(mirrored("https://uploads.kiwifarms.net/a.png", "abc.onion"), "https://uploads.kiwifarms.net/a.png");
    }

    #[test]
    fn wanting_rooms_tracks_listed_rooms_by_uuid_and_keeps_side_rooms() {
        let chat = Chat::default();
        let room = |id: &str, name: &str| SneedChatRoom { id: id.into(), name: name.into() };
        chat.want_rooms(&[room("01a1f0f8-2d29-7ac3-b779-b3b0904d6dd6", "fishtank"), room("16", "old")]);
        // A room still under an old number is not tracked until it has a uuid.
        assert_eq!(chat.rooms.lock().unwrap().len(), 1);
        assert_eq!(chat.room_of_buffer("#fishtank").as_deref(), Some("01a1f0f8-2d29-7ac3-b779-b3b0904d6dd6"));
        chat.want_side_room("side-1", None);
        chat.want_rooms(&[]);
        // The listed room goes; the side room is the chat's, and stays.
        let rooms = chat.rooms.lock().unwrap();
        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains_key("side-1"));
    }

    #[test]
    fn closing_a_side_room_leaves_it() {
        let chat = Chat::default();
        chat.want_side_room("side-1", None);
        chat.rooms.lock().unwrap().get_mut("side-1").unwrap().buffer = "Blackstar".into();
        assert!(!chat.leave_side_room("#fishtank"));
        assert!(chat.leave_side_room("Blackstar"));
        assert!(chat.rooms.lock().unwrap().is_empty());
    }

    #[test]
    fn a_side_room_with_several_others_is_a_group_chat() {
        let p = |u: &str| Principal { uuid: u.into(), name: u.into(), ..Default::default() };
        let mut room = RoomState { kind: RoomKind::Side, ..Default::default() };
        room.group = RoomState::is_group(&[p("me"), p("a")], "me");
        assert_eq!(room.buffer_kind(), "dm");
        room.group = RoomState::is_group(&[p("me"), p("a"), p("b")], "me");
        assert_eq!(room.buffer_kind(), "channel");
        assert_eq!(RoomState::default().buffer_kind(), "channel");
    }

    #[test]
    fn a_side_room_is_named_for_the_others_in_it() {
        let p = |u: &str, n: &str| Principal { uuid: u.into(), name: n.into(), ..Default::default() };
        assert_eq!(side_room_name(&[p("me", "Ancient Pioneer"), p("u2", "Ryo Ma")], "me", "abcdef-123456", None), "Ryo Ma");
        assert_eq!(side_room_name(&[p("me", "A"), p("u2", "B"), p("u3", "C")], "me", "x", None), "B, C");
        assert_eq!(side_room_name(&[p("me", "A")], "me", "abcdef-123456", None), "whisper-123456");
        // A group chat somebody named goes by its name.
        assert_eq!(side_room_name(&[p("me", "A"), p("u2", "B"), p("u3", "C")], "me", "x", Some(" Lolcow Watch ")), "Lolcow Watch");
        assert_eq!(side_room_name(&[p("me", "A"), p("u2", "B")], "me", "x", Some("  ")), "B");
    }

    #[test]
    fn a_side_room_takes_in_what_the_chat_says_of_it() {
        let mut room = RoomState::default();
        // A listing row: a group of two that the chat calls a group, named.
        room.take_side_row(&json!({ "chat_room_uuid": "r", "side_room_kind": "group", "title": "Plans",
            "members": [{ "principal_uuid": "me", "display_name": "A" }, { "principal_uuid": "u2", "display_name": "B" }] }), "me");
        assert_eq!(room.kind, RoomKind::Side);
        assert!(room.group, "a group with one other person is still a group");
        assert_eq!(room.title.as_deref(), Some("Plans"));
        assert_eq!(room.members.len(), 2);
        // A later frame without members or title leaves both; a cleared title clears it.
        room.take_side_row(&json!({ "side_room_kind": "group" }), "me");
        assert_eq!((room.members.len(), room.title.as_deref()), (2, Some("Plans")));
        room.take_side_row(&json!({ "title": null }), "me");
        assert_eq!(room.title, None);
        // A pair is a direct message.
        let mut pair = RoomState::default();
        pair.take_side_row(&json!({ "side_room_kind": "pair", "members": [{ "principal_uuid": "me", "display_name": "A" }, { "principal_uuid": "u2", "display_name": "B" }] }), "me");
        assert_eq!(pair.buffer_kind(), "dm");
    }

    #[tokio::test]
    async fn a_command_refused_for_good_says_why() {
        let chat = Chat::default();
        // Without a socket nothing can go out: said, not hung on.
        let err = chat.command("op-1", json!({ "t": "revise", "chat_operation_uuid": "op-1" })).await.unwrap_err();
        assert!(format!("{err:#}").contains("not connected"));
        chat.frozen.store(true, Ordering::Relaxed);
        let err = chat.command("op-2", json!({ "t": "revise", "chat_operation_uuid": "op-2" })).await.unwrap_err();
        assert!(format!("{err:#}").contains("frozen"));
    }
}
