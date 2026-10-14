//! An account's connection to Twitch.
//!
//! Two things run side by side for each account, ended together:
//!
//! - **keeping the sign-in good.** The access token lasts four hours; this
//!   renews it before it stops, with the refresh token, which is single-use -
//!   so what comes back is written down before anything else is done with it -
//!   and checks who it belongs to when it starts and every so often after. A
//!   token Twitch will not accept any more (revoked, or thirty days unused)
//!   stops the account in the one state that says "sign in again", where
//!   retrying could never help.
//! - **the chat connection** (`session`): Twitch's IRC over a websocket, read
//!   anonymously or signed in, with every channel the account has open joined
//!   on it, and the commands the window gives it - join, part, say - handled on
//!   the same connection. It reconnects with a growing wait and rejoins.
//!
//! An anonymous account has no token to keep good and only the second runs.

use super::auth::{self, Renewal};
use super::chat::{self, Rooms};
use super::irc;
use crate::accounts::TwitchAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// How often a good token is checked again with Twitch, in seconds. Twitch
/// asks apps to validate from time to time, and it is how a token revoked on
/// twitch.tv is noticed before it is next needed.
const VALIDATE_EVERY: i64 = 60 * 60;

/// Where Twitch's chat is.
pub const IRC_URL: &str = "wss://irc-ws.chat.twitch.tv:443";

/// Who to tell when a join has come to something.
pub type JoinReply = oneshot::Sender<std::result::Result<(), String>>;

/// What the window asks the connection to do.
pub enum Command {
    /// Open a channel: join it, make its conversation, remember it. Answers
    /// when Twitch has said the channel is there, or that it is not.
    Join(String, Option<JoinReply>),
    /// Close one.
    Part(String),
    /// Say something in one, optionally as a reply to a message by its id.
    Say { channel: String, text: String, reply_to: Option<String>, respond: oneshot::Sender<std::result::Result<(), String>> },
}

pub fn spawn(state: AppState, config: TwitchAccountConfig) {
    let account_id = config.account_id();
    let routed = state.accounts.all_twitch().iter().any(|a| a.use_tor) || config.use_tor;
    crate::net::route::router().mark(super::SHARED_KEY, routed);
    state.runtime.reset_connection(&account_id);
    // A sender that outlives the connection: a join asked for while it is
    // reconnecting waits in the queue and is made when it is back.
    let (tx, rx) = mpsc::unbounded_channel::<Command>();
    state.runtime.set_twitch_sender(&account_id, tx);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            // Ended together: a sign-in Twitch will not accept stops the chat
            // too, and an account with nothing to keep good just chats.
            tokio::select! {
                _ = keeper(&state, &account_id, auth::ID_ROOT) => {}
                _ = chat_loop(&state, &account_id, rx) => {}
            }
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

/// What keeping a sign-in good came to.
#[derive(Debug, PartialEq)]
pub enum Kept {
    /// Good until the given time (Unix seconds); look again then.
    Good(i64),
    /// Twitch will not accept it. Sign in again.
    SignInAgain(String),
}

/// Makes the account's token good, renewing it if it is due or Twitch has
/// stopped accepting it, and saves what it learned.
pub async fn keep_good(state: &AppState, account_id: &str, root: &str) -> Result<Kept> {
    let http = super::client()?;
    let Some(cfg) = state.accounts.get_twitch(account_id) else {
        return Ok(Kept::SignInAgain("this account was removed".into()));
    };
    let now = chrono::Utc::now().timestamp();
    let mut token = cfg.access_token.clone();
    let mut expires_at = cfg.expires_at;

    // Due: renewed before anything is asked of the old one.
    let mut due = now >= expires_at - auth::RENEW_BEFORE;
    if !due {
        // Not due by the clock. Twitch may still have ended it - asked now, so that is found here.
        match auth::validate(&http, root, &token).await? {
            Some(who) => {
                state.accounts.mutate_twitch(account_id, |a| {
                    a.user_id = who.user_id.clone();
                    if !who.scopes.is_empty() {
                        a.scopes = who.scopes.clone();
                    }
                })?;
                let renew = auth::renew_at(now, who.expires_in);
                return Ok(Kept::Good(renew.min(now + VALIDATE_EVERY)));
            }
            None => due = true,
        }
    }
    if due {
        match auth::refresh(&http, root, &cfg.refresh_token).await {
            Ok(tokens) => {
                token = tokens.access_token.clone();
                expires_at = now + tokens.expires_in;
                // Written before it is used: the old refresh token is spent, and
                // a crash between here and the write would lose the account.
                state.accounts.mutate_twitch(account_id, |a| {
                    a.access_token = tokens.access_token.clone();
                    a.refresh_token = tokens.refresh_token.clone();
                    a.expires_at = expires_at;
                    if !tokens.scope.is_empty() {
                        a.scopes = tokens.scope.clone();
                    }
                })?;
            }
            Err(Renewal::Refused(why)) => return Ok(Kept::SignInAgain(if why.is_empty() { "Twitch no longer accepts this sign-in".into() } else { why })),
            Err(Renewal::Failed(e)) => return Err(e),
        }
        // And who it is, now that it is new.
        if let Some(who) = auth::validate(&http, root, &token).await? {
            state.accounts.mutate_twitch(account_id, |a| a.user_id = who.user_id.clone())?;
        }
    }
    Ok(Kept::Good(auth::renew_at(now, (expires_at - now).max(0)).min(now + VALIDATE_EVERY)))
}

/// Keeps the sign-in good for as long as the account exists. Never returns
/// for an anonymous account, and returns for one Twitch will not accept.
async fn keeper(state: &AppState, account_id: &str, root: &str) {
    let mut wait = 5u64;
    loop {
        let Some(cfg) = state.accounts.get_twitch(account_id) else { return };
        if !cfg.is_signed_in() {
            // Anonymous: nothing to keep good.
            std::future::pending::<()>().await;
        }
        match keep_good(state, account_id, root).await {
            Ok(Kept::Good(until)) => {
                wait = 5;
                let now = chrono::Utc::now().timestamp();
                tokio::time::sleep(Duration::from_secs((until - now).max(30) as u64)).await;
            }
            Ok(Kept::SignInAgain(why)) => {
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Sign in to Twitch again from Accounts.")));
                return;
            }
            // The network, or Twitch having a bad moment: the token may be fine.
            Err(e) => {
                tracing::debug!("twitch[{account_id}]: {e:#}");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                wait = (wait * 2).min(300);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The chat connection
// ---------------------------------------------------------------------------

/// Twitch's limit on joining channels is twenty in ten seconds for an account
/// that is signed in; one every half second and a bit is inside it.
const JOIN_EVERY: Duration = Duration::from_millis(550);

/// How long a join may wait for Twitch to say the channel is there. A channel
/// that does not exist is never answered at all, which is how it is told.
const JOIN_PATIENCE: Duration = Duration::from_secs(10);

/// How long the connection may be silent. Twitch pings every five minutes or
/// so; a connection that has heard nothing for longer than that is gone
/// without having said so.
const SILENCE: Duration = Duration::from_secs(7 * 60);

/// Twitch's limit on what one account may send, and the window it is counted
/// over: twenty messages in thirty seconds, a hundred for a moderator.
const SEND_WINDOW: Duration = Duration::from_secs(30);
const SEND_LIMIT: usize = 20;
const SEND_LIMIT_MODERATOR: usize = 100;

/// The longest message Twitch takes, in characters.
pub const MAX_MESSAGE: usize = 500;

/// Reads whatever a person typed as a channel: a login, with or without the
/// `#`, or an address of the channel's page. `None` for anything Twitch could
/// not have a channel called.
pub fn normalise_login(input: &str) -> Option<String> {
    let mut text = input.trim().to_lowercase();
    if let Some(rest) = text.split_once("twitch.tv/").map(|(_, r)| r.to_string()) {
        text = rest;
    }
    let text = text.trim_start_matches(['#', '@']).split(['/', '?', '#']).next().unwrap_or("").to_string();
    let valid = !text.is_empty() && text.len() <= 25 && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some(text)
}

async fn chat_loop(state: &AppState, account_id: &str, mut rx: mpsc::UnboundedReceiver<Command>) {
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    let mut wait = 3u64;
    loop {
        if state.accounts.get_twitch(account_id).is_none() {
            return;
        }
        match session(state, account_id, &mut rx, IRC_URL, Timing::default()).await {
            Ok(()) => wait = 3,
            Err(e) => {
                tracing::debug!("twitch[{account_id}]: chat ended: {e:#}");
                state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
                state.runtime.report_progress(state, account_id, &format!("Twitch chat dropped ({e}) - reconnecting"));
            }
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        wait = (wait * 2).min(120);
    }
}

/// Somebody asked for a channel and is waiting to hear it is there: its login,
/// when to give up, and who to tell.
type Waiting = (String, tokio::time::Instant, Option<oneshot::Sender<std::result::Result<(), String>>>);

/// What a limit on sending says to a person, in seconds to wait.
fn send_wait(sent: &VecDeque<std::time::Instant>, limit: usize) -> Option<u64> {
    if sent.len() < limit {
        return None;
    }
    let oldest = sent.front()?;
    Some(SEND_WINDOW.saturating_sub(oldest.elapsed()).as_secs() + 1)
}

/// How fast a connection joins and how long it waits - the constants above,
/// as a value so a test need not wait ten seconds to see a channel is absent.
#[derive(Clone, Copy)]
struct Timing {
    join_every: Duration,
    join_patience: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { join_every: JOIN_EVERY, join_patience: JOIN_PATIENCE }
    }
}

/// One chat connection, from connecting to its end. `Ok` when Twitch asked us
/// to reconnect; an error when it dropped.
async fn session(state: &AppState, account_id: &str, rx: &mut mpsc::UnboundedReceiver<Command>, url: &str, timing: Timing) -> Result<()> {
    let Some(cfg) = state.accounts.get_twitch(account_id) else { return Ok(()) };
    let token = cfg.is_signed_in().then(|| cfg.access_token.clone());
    let (mut socket, _) = crate::net::route::websocket(super::SHARED_KEY, url).await.context("connecting to Twitch's chat")?;
    for line in irc::greeting(&cfg.login, token.as_deref()) {
        socket.send(WsMessage::Text(format!("{line}\r\n"))).await?;
    }

    let mut rooms = Rooms::new(&cfg.login, &cfg.user_id);
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut joined: HashSet<String> = HashSet::new();
    // Joins waiting to hear that the channel is there, with who asked.
    let mut waiting: Vec<Waiting> = Vec::new();
    let mut sent: VecDeque<std::time::Instant> = VecDeque::new();
    let mut welcomed = false;
    let mut pace = tokio::time::interval(timing.join_every);
    let mut last_heard = tokio::time::Instant::now();

    loop {
        tokio::select! {
            frame = socket.next() => {
                let Some(frame) = frame else { anyhow::bail!("the connection closed") };
                match frame.context("reading from Twitch's chat")? {
                    WsMessage::Text(text) => {
                        last_heard = tokio::time::Instant::now();
                        for raw in text.split("\r\n").filter(|l| !l.is_empty()) {
                            if raw.starts_with("PING") {
                                socket.send(WsMessage::Text("PONG :tmi.twitch.tv\r\n".to_string())).await?;
                                continue;
                            }
                            let Some(line) = irc::parse(raw) else { continue };
                            if line.command == "001" && !welcomed {
                                welcomed = true;
                                state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                                // What was open last time.
                                let channels = state.accounts.get_twitch(account_id).map(|a| a.channels).unwrap_or_default();
                                for channel in channels {
                                    state.runtime.ensure_buffer(state, account_id, &channel, "channel");
                                    waiting.push((channel.clone(), tokio::time::Instant::now() + timing.join_patience + timing.join_every * queue.len() as u32, None));
                                    queue.push_back(channel);
                                }
                                continue;
                            }
                            let outcome = chat::handle(state, account_id, &mut rooms, &line, false);
                            if let Some(why) = outcome.refused {
                                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Sign in to Twitch again from Accounts.")));
                                anyhow::bail!("Twitch refused the sign-in: {why}");
                            }
                            if outcome.reconnect {
                                return Ok(());
                            }
                            if let Some(channel) = outcome.joined {
                                joined.insert(channel.clone());
                                remember(state, account_id, &channel);
                                for (_, _, reply) in waiting.iter_mut().filter(|w| w.0 == channel) {
                                    if let Some(reply) = reply.take() {
                                        let _ = reply.send(Ok(()));
                                    }
                                }
                                waiting.retain(|w| w.0 != channel);
                            }
                            if let Some((channel, why)) = outcome.barred {
                                for (_, _, reply) in waiting.iter_mut().filter(|w| w.0 == channel) {
                                    if let Some(reply) = reply.take() {
                                        let _ = reply.send(Err(why.clone()));
                                    }
                                }
                                waiting.retain(|w| w.0 != channel);
                            }
                        }
                    }
                    WsMessage::Ping(data) => socket.send(WsMessage::Pong(data)).await?,
                    WsMessage::Close(_) => anyhow::bail!("Twitch closed the connection"),
                    _ => {}
                }
            }

            command = rx.recv() => {
                let Some(command) = command else { return Ok(()) };
                match command {
                    Command::Join(login, reply) => {
                        let Some(channel) = normalise_login(&login) else {
                            if let Some(reply) = reply { let _ = reply.send(Err(format!("\"{login}\" cannot be a Twitch channel"))); }
                            continue;
                        };
                        if joined.contains(&channel) {
                            state.runtime.ensure_buffer(state, account_id, &channel, "channel");
                            if let Some(reply) = reply { let _ = reply.send(Ok(())); }
                            continue;
                        }
                        state.runtime.ensure_buffer(state, account_id, &channel, "channel");
                        waiting.push((channel.clone(), tokio::time::Instant::now() + timing.join_patience + timing.join_every * queue.len() as u32, reply));
                        queue.push_back(channel);
                    }
                    Command::Part(login) => {
                        let Some(channel) = normalise_login(&login) else { continue };
                        queue.retain(|c| *c != channel);
                        waiting.retain(|w| w.0 != channel);
                        if joined.remove(&channel) {
                            socket.send(WsMessage::Text(format!("PART #{channel}\r\n"))).await?;
                        }
                        rooms.forget_channel(&channel);
                        let _ = state.accounts.mutate_twitch(account_id, |a| a.channels.retain(|c| *c != channel));
                    }
                    Command::Say { channel, text, reply_to, respond } => {
                        let moderator = rooms.moderator_in.contains(&channel);
                        let result = say(state, account_id, &mut rooms, &joined, &mut sent, moderator, &channel, &text, reply_to.as_deref(), &mut socket).await;
                        let _ = respond.send(result);
                    }
                }
            }

            _ = pace.tick() => {
                if welcomed {
                    if let Some(channel) = queue.pop_front() {
                        socket.send(WsMessage::Text(format!("JOIN #{channel}\r\n"))).await?;
                    }
                }
                // A join Twitch never answered is a channel that is not there.
                let now = tokio::time::Instant::now();
                let due: Vec<String> = waiting.iter().filter(|w| w.1 <= now && !queue.contains(&w.0)).map(|w| w.0.clone()).collect();
                for channel in due {
                    let mut asked = false;
                    for (_, _, reply) in waiting.iter_mut().filter(|w| w.0 == channel) {
                        if let Some(reply) = reply.take() {
                            asked = true;
                            let _ = reply.send(Err(format!("Twitch has no channel called {channel}")));
                        }
                    }
                    waiting.retain(|w| w.0 != channel);
                    if asked {
                        // Asked for by a person: it is not a channel, so it is not kept.
                        state.runtime.remove_buffer(state, &crate::model::buffer_id(account_id, &channel));
                        let _ = socket.send(WsMessage::Text(format!("PART #{channel}\r\n"))).await;
                    } else {
                        // Remembered from before: left alone, and said once.
                        state.runtime.report_progress(state, account_id, &format!("Twitch did not answer for {channel}"));
                    }
                }
                if last_heard.elapsed() > SILENCE {
                    anyhow::bail!("nothing heard from Twitch for {} minutes", SILENCE.as_secs() / 60);
                }
            }
        }
    }
}

/// Saves a channel once Twitch has said it is there, so it comes back tomorrow.
fn remember(state: &AppState, account_id: &str, channel: &str) {
    if let Some(cfg) = state.accounts.get_twitch(account_id) {
        if !cfg.channels.iter().any(|c| c == channel) {
            let mut channels = cfg.channels;
            channels.push(channel.to_string());
            let _ = state.accounts.mutate_twitch(account_id, |a| a.channels = channels.clone());
        }
    }
}

/// What it takes to send a message and show it as sent.
#[allow(clippy::too_many_arguments)]
async fn say<S>(
    state: &AppState,
    account_id: &str,
    rooms: &mut Rooms,
    joined: &HashSet<String>,
    sent: &mut VecDeque<std::time::Instant>,
    moderator: bool,
    channel: &str,
    text: &str,
    reply_to: Option<&str>,
    socket: &mut S,
) -> std::result::Result<(), String>
where
    S: futures::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let Some(cfg) = state.accounts.get_twitch(account_id) else { return Err("this account is gone".into()) };
    if !cfg.is_signed_in() {
        return Err("Sign in to Twitch from Accounts to talk in chat. Reading needs no account.".into());
    }
    if !joined.contains(channel) {
        return Err(format!("This account has not joined {channel} yet"));
    }
    // Twitch's chat is one line: a break typed in the box becomes a space.
    let text: String = text.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err("There is nothing to send".into());
    }
    if text.chars().count() > MAX_MESSAGE {
        return Err(format!("Twitch takes at most {MAX_MESSAGE} characters in a message; this one has {}", text.chars().count()));
    }
    while sent.front().is_some_and(|t| t.elapsed() > SEND_WINDOW) {
        sent.pop_front();
    }
    if let Some(wait) = send_wait(sent, if moderator { SEND_LIMIT_MODERATOR } else { SEND_LIMIT }) {
        return Err(format!("Twitch lets an account send {} messages in 30 seconds. Wait {wait} seconds.", if moderator { SEND_LIMIT_MODERATOR } else { SEND_LIMIT }));
    }

    // The reply, quoted from what is stored: the window has it, and Twitch
    // shows a reply's parent from the tags it carries.
    let parent = reply_to.and_then(|id| state.store.get_message(&crate::model::buffer_id(account_id, channel), id).ok().flatten());
    let (tags, shown_text) = match (&parent, reply_to) {
        (Some(parent), Some(id)) => {
            // Twitch's own clients put the name in front of a reply's text.
            let login = parent.from.to_lowercase();
            let named = login.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            let body = if named && !text.starts_with('@') { format!("@{login} {text}") } else { text.clone() };
            (format!("@reply-parent-msg-id={id} "), body)
        }
        _ => (String::new(), text.clone()),
    };
    socket
        .send(WsMessage::Text(format!("{tags}PRIVMSG #{channel} :{shown_text}\r\n")))
        .await
        .map_err(|e| format!("Twitch's chat is not answering: {e}"))?;
    sent.push_back(std::time::Instant::now());

    // Twitch does not send a message back to the one who sent it over IRC, so
    // it is shown as sent, in the colour and badges Twitch last said we have.
    let (action, plain) = match text.strip_prefix("/me ") {
        Some(rest) => (true, rest.to_string()),
        None => (false, text.clone()),
    };
    let shown = if rooms.me_display.is_empty() { cfg.login.clone() } else { rooms.me_display.clone() };
    let style = crate::model::SenderStyle { color: rooms.me_color.clone(), badges: rooms.me_badges.get(channel).cloned().unwrap_or_default() };
    let reply = parent.as_ref().map(|p| {
        let root = p.reply_to.as_ref().and_then(|r| r.thread_root.clone()).unwrap_or_else(|| p.id.clone());
        crate::model::ReplyPreview { id: p.id.clone(), from: p.from.clone(), body: p.body.clone(), thread: false, forwarded: false, thread_root: Some(root) }
    });
    state.runtime.record_message_at(
        state,
        account_id,
        channel,
        "channel",
        &shown,
        &plain,
        action,
        "chat",
        reply,
        None,
        false,
        None,
        Vec::new(),
        Vec::new(),
        Some(cfg.user_id.clone()).filter(|i| !i.is_empty()),
        None,
        None,
        Some(style),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A Twitch that answers requests by what they are, with a count of each.
    async fn twitch(refresh: (u16, &'static str), valid: bool) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = text.lines().next().unwrap_or("").to_string();
                    log.lock().unwrap().push(format!("{line} | {}", text.split("\r\n\r\n").nth(1).unwrap_or("")));
                    let (status, body) = if line.starts_with("POST /oauth2/token") {
                        (refresh.0, refresh.1.to_string())
                    } else if line.starts_with("GET /oauth2/validate") {
                        if valid {
                            (200, r#"{"client_id":"c","login":"someone","scopes":["chat:read","chat:edit"],"user_id":"42","expires_in":10000}"#.to_string())
                        } else {
                            (401, r#"{"message":"invalid access token"}"#.to_string())
                        }
                    } else {
                        (404, "{}".to_string())
                    };
                    let reply = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (root, seen)
    }

    const NEW: &str = r#"{"access_token":"new-acc","refresh_token":"new-ref","expires_in":14000,"scope":["chat:read"],"token_type":"bearer"}"#;

    fn signed_in(state: &AppState, expires_in: i64) {
        state
            .accounts
            .add_twitch(TwitchAccountConfig {
                login: "someone".into(),
                user_id: "42".into(),
                access_token: "old-acc".into(),
                refresh_token: "old-ref".into(),
                expires_at: chrono::Utc::now().timestamp() + expires_in,
                ..Default::default()
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_token_with_time_left_is_checked_and_left_alone() {
        let state = testkit::simulated_daemon("twitch-keep-good");
        signed_in(&state, 10_000);
        let (root, seen) = twitch((500, "{}"), true).await;
        let kept = keep_good(&state, "twitch:someone", &root).await.unwrap();
        assert!(matches!(kept, Kept::Good(_)));
        assert_eq!(state.accounts.get_twitch("twitch:someone").unwrap().access_token, "old-acc");
        assert!(seen.lock().unwrap().iter().all(|l| !l.contains("/oauth2/token")), "no renewal when none is due");
    }

    #[tokio::test]
    async fn a_token_that_is_nearly_out_is_renewed_and_the_new_pair_is_kept() {
        let state = testkit::simulated_daemon("twitch-renew");
        signed_in(&state, 100);
        let (root, seen) = twitch((200, NEW), true).await;
        let kept = keep_good(&state, "twitch:someone", &root).await.unwrap();
        assert!(matches!(kept, Kept::Good(_)));
        let saved = state.accounts.get_twitch("twitch:someone").unwrap();
        assert_eq!((saved.access_token.as_str(), saved.refresh_token.as_str()), ("new-acc", "new-ref"));
        assert!(saved.expires_at > chrono::Utc::now().timestamp() + 13_000);
        assert!(seen.lock().unwrap().iter().any(|l| l.contains("refresh_token=old-ref")));
    }

    #[tokio::test]
    async fn a_token_twitch_has_ended_is_renewed_before_its_time_and_a_dead_refresh_means_sign_in_again() {
        let state = testkit::simulated_daemon("twitch-revoked");
        signed_in(&state, 10_000);
        // Not due by the clock, but Twitch will not have it: renewed, and the renewal is refused too.
        let (root, _) = twitch((400, r#"{"status":400,"message":"Invalid refresh token"}"#), false).await;
        match keep_good(&state, "twitch:someone", &root).await.unwrap() {
            Kept::SignInAgain(why) => assert_eq!(why, "Invalid refresh token"),
            other => panic!("{other:?}"),
        }
        // The account is kept: it is the sign-in that has to be redone, not the account.
        assert!(state.accounts.get_twitch("twitch:someone").is_some());
    }

    #[tokio::test]
    async fn twitch_being_unreachable_is_an_error_to_retry_and_not_a_verdict() {
        let state = testkit::simulated_daemon("twitch-down");
        signed_in(&state, 100);
        assert!(keep_good(&state, "twitch:someone", "http://127.0.0.1:9").await.is_err());
        assert_eq!(state.accounts.get_twitch("twitch:someone").unwrap().refresh_token, "old-ref", "nothing was spent");
    }

    #[tokio::test]
    async fn an_account_that_was_removed_is_not_kept() {
        let state = testkit::simulated_daemon("twitch-gone");
        assert!(matches!(keep_good(&state, "twitch:nobody", "http://127.0.0.1:9").await.unwrap(), Kept::SignInAgain(_)));
    }

    // -----------------------------------------------------------------------
    // The chat connection, against a Twitch that is a websocket in a test
    // -----------------------------------------------------------------------

    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message as Ws;

    /// What a fake Twitch is told to do, and what it was sent.
    #[derive(Default)]
    struct Fake {
        /// Channels that exist: joining one is answered with a room state and a message.
        exists: Vec<&'static str>,
        /// Lines to send as soon as the greeting is done, ahead of anything.
        after_welcome: Vec<String>,
        /// Every line the client sent.
        got: Arc<Mutex<Vec<String>>>,
        /// Sends RECONNECT after the first join.
        reconnect: bool,
        /// Refuses the login.
        refuse: bool,
    }

    async fn fake_twitch(fake: Fake) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut greeted = 0;
            while let Some(Ok(frame)) = ws.next().await {
                let Ws::Text(text) = frame else { continue };
                for line in text.split("\r\n").filter(|l| !l.is_empty()) {
                    fake.got.lock().unwrap().push(line.to_string());
                    if line.starts_with("NICK") {
                        greeted += 1;
                        if fake.refuse {
                            let _ = ws.send(Ws::Text(":tmi.twitch.tv NOTICE * :Login authentication failed\r\n".into())).await;
                            continue;
                        }
                        let _ = ws.send(Ws::Text(":tmi.twitch.tv 001 me :Welcome, GLHF!\r\n".into())).await;
                        let _ = ws.send(Ws::Text("@color=#8A2BE2;display-name=Someone;user-id=42 :tmi.twitch.tv GLOBALUSERSTATE\r\n".into())).await;
                        for l in &fake.after_welcome {
                            let _ = ws.send(Ws::Text(format!("{l}\r\n"))).await;
                        }
                    }
                    if let Some(channel) = line.strip_prefix("JOIN #") {
                        if fake.exists.contains(&channel) {
                            let _ = ws.send(Ws::Text(format!("@emote-only=0;followers-only=-1;r9k=0;room-id=100;slow=0;subs-only=1 :tmi.twitch.tv ROOMSTATE #{channel}\r\n"))).await;
                            let _ = ws.send(Ws::Text(format!("@badge-info=;badges=moderator/1;color=#8A2BE2;display-name=Someone;mod=1 :tmi.twitch.tv USERSTATE #{channel}\r\n"))).await;
                            let _ = ws.send(Ws::Text(format!("@display-name=Fan;id=m1;room-id=100;tmi-sent-ts=1791949381404;user-id=7 :fan!fan@fan.tmi.twitch.tv PRIVMSG #{channel} :hello chat\r\n"))).await;
                            if fake.reconnect {
                                let _ = ws.send(Ws::Text(":tmi.twitch.tv RECONNECT\r\n".into())).await;
                            }
                        }
                    }
                    if line.starts_with("PRIVMSG") {
                        // Twitch answers a sent message with the sender's state, never with the message.
                        let _ = ws.send(Ws::Text("@badge-info=;badges=moderator/1;color=#8A2BE2;display-name=Someone;mod=1 :tmi.twitch.tv USERSTATE #xqc\r\n".into())).await;
                    }
                }
            }
            let _ = greeted;
        });
        url
    }

    fn quick() -> Timing {
        Timing { join_every: Duration::from_millis(20), join_patience: Duration::from_millis(400) }
    }

    fn anonymous(state: &AppState) {
        state.accounts.add_twitch(TwitchAccountConfig { anonymous: true, ..Default::default() }).unwrap();
    }

    fn signed_in_chat(state: &AppState) {
        state
            .accounts
            .add_twitch(TwitchAccountConfig { login: "someone".into(), user_id: "42".into(), access_token: "tok".into(), refresh_token: "r".into(), expires_at: chrono::Utc::now().timestamp() + 10_000, ..Default::default() })
            .unwrap();
    }

    async fn join(tx: &mpsc::UnboundedSender<Command>, channel: &str) -> std::result::Result<(), String> {
        let (reply, answer) = oneshot::channel();
        tx.send(Command::Join(channel.into(), Some(reply))).unwrap();
        tokio::time::timeout(Duration::from_secs(5), answer).await.expect("an answer").expect("a reply")
    }

    async fn say_in(tx: &mpsc::UnboundedSender<Command>, channel: &str, text: &str, reply_to: Option<&str>) -> std::result::Result<(), String> {
        let (respond, answer) = oneshot::channel();
        tx.send(Command::Say { channel: channel.into(), text: text.into(), reply_to: reply_to.map(str::to_string), respond }).unwrap();
        tokio::time::timeout(Duration::from_secs(5), answer).await.expect("an answer").expect("a reply")
    }

    #[tokio::test]
    async fn an_anonymous_reader_joins_a_channel_and_reads_it() {
        let state = testkit::simulated_daemon("twitch-session-read");
        anonymous(&state);
        let got = Arc::new(Mutex::new(Vec::new()));
        let url = fake_twitch(Fake { exists: vec!["xqc"], got: got.clone(), ..Default::default() }).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, id) = (state.clone(), "twitch:~anonymous");
        let task = tokio::spawn(async move { session(&s, id, &mut rx, &url, quick()).await });

        join(&tx, "XQC").await.expect("a channel that exists");
        // Its conversation exists, it said hello, and the room's modes are in its header.
        for _ in 0..50 {
            if !state.store.get_backlog("twitch:~anonymous|xqc", 0, 10).unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let log = state.store.get_backlog("twitch:~anonymous|xqc", 0, 10).unwrap();
        assert_eq!((log[0].from.as_str(), log[0].body.as_str()), ("Fan", "hello chat"));
        assert_eq!(state.runtime.get_buffer("twitch:~anonymous|xqc").unwrap().channel_modes.as_deref(), Some("subscribers only"));
        // It is remembered, so it is open again tomorrow, and the greeting was anonymous and in order.
        assert_eq!(state.accounts.get_twitch("twitch:~anonymous").unwrap().channels, vec!["xqc"]);
        let sent = got.lock().unwrap().clone();
        assert_eq!(sent[0], "CAP REQ :twitch.tv/tags twitch.tv/commands");
        assert!(sent[2].starts_with("NICK justinfan"), "{sent:?}");
        assert!(sent.iter().any(|l| l == "JOIN #xqc"));
        // An anonymous reader cannot talk, and is told what to do.
        let refused = say_in(&tx, "xqc", "hi", None).await.unwrap_err();
        assert!(refused.contains("Sign in"), "{refused}");
        // Closing the channel parts it and forgets it.
        tx.send(Command::Part("xqc".into())).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(got.lock().unwrap().iter().any(|l| l == "PART #xqc"));
        assert!(state.accounts.get_twitch("twitch:~anonymous").unwrap().channels.is_empty());
        task.abort();
    }

    #[tokio::test]
    async fn a_channel_twitch_never_answers_for_is_said_not_to_exist_and_is_not_kept() {
        let state = testkit::simulated_daemon("twitch-session-absent");
        anonymous(&state);
        let url = fake_twitch(Fake { exists: vec![], ..Default::default() }).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, id) = (state.clone(), "twitch:~anonymous");
        let task = tokio::spawn(async move { session(&s, id, &mut rx, &url, quick()).await });
        let e = join(&tx, "nosuchchannel").await.unwrap_err();
        assert!(e.contains("no channel called nosuchchannel"), "{e}");
        assert!(state.runtime.get_buffer("twitch:~anonymous|nosuchchannel").is_none(), "no empty conversation is left behind");
        assert!(state.accounts.get_twitch("twitch:~anonymous").unwrap().channels.is_empty());
        // And what is not a login at all is refused at once.
        assert!(join(&tx, "not a channel!").await.unwrap_err().contains("cannot be a Twitch channel"));
        task.abort();
    }

    #[tokio::test]
    async fn a_signed_in_account_sends_a_message_and_sees_it_as_its_own() {
        let state = testkit::simulated_daemon("twitch-session-say");
        signed_in_chat(&state);
        let got = Arc::new(Mutex::new(Vec::new()));
        let url = fake_twitch(Fake { exists: vec!["xqc"], got: got.clone(), ..Default::default() }).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, id) = (state.clone(), "twitch:someone");
        let task = tokio::spawn(async move { session(&s, id, &mut rx, &url, quick()).await });
        join(&tx, "xqc").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        say_in(&tx, "xqc", "hello\nthere", None).await.unwrap();
        say_in(&tx, "xqc", "/me waves", None).await.unwrap();
        // A reply to what Fan said: Twitch's tag, and the name in front as its own clients write it.
        say_in(&tx, "xqc", "same", Some("m1")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let sent = got.lock().unwrap().clone();
        assert!(sent.iter().any(|l| l == "PASS oauth:tok"), "{sent:?}");
        assert!(sent.iter().any(|l| l == "NICK someone"));
        assert!(sent.iter().any(|l| l == "PRIVMSG #xqc :hello there"), "a line break is a space: {sent:?}");
        assert!(sent.iter().any(|l| l == "PRIVMSG #xqc :/me waves"));
        assert!(sent.iter().any(|l| l == "@reply-parent-msg-id=m1 PRIVMSG #xqc :@fan same"), "{sent:?}");

        // Shown as sent: under the name Twitch writes it, in our colour, with the badge we were just told we have.
        let log = state.store.get_backlog("twitch:someone|xqc", 0, 20).unwrap();
        let mine: Vec<_> = log.iter().filter(|m| m.is_own).collect();
        assert_eq!(mine.len(), 3);
        assert_eq!((mine[0].from.as_str(), mine[0].body.as_str(), mine[0].sender_color.as_deref()), ("Someone", "hello there", Some("#8A2BE2")));
        assert!(mine[0].badges.iter().any(|b| b.kind == "moderator"));
        assert!(mine[1].is_action && mine[1].body == "waves");
        let reply = mine[2].reply_to.clone().expect("a reply");
        assert_eq!((reply.id.as_str(), reply.from.as_str(), reply.body.as_str()), ("m1", "Fan", "hello chat"));
        task.abort();
    }

    #[tokio::test]
    async fn what_twitch_would_refuse_is_refused_here_in_words() {
        let state = testkit::simulated_daemon("twitch-session-refuse");
        signed_in_chat(&state);
        let url = fake_twitch(Fake { exists: vec!["xqc"], ..Default::default() }).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, id) = (state.clone(), "twitch:someone");
        let task = tokio::spawn(async move { session(&s, id, &mut rx, &url, quick()).await });
        join(&tx, "xqc").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(say_in(&tx, "xqc", "   ", None).await.unwrap_err().contains("nothing to send"));
        assert!(say_in(&tx, "xqc", &"x".repeat(501), None).await.unwrap_err().contains("at most 500"));
        assert!(say_in(&tx, "other", "hi", None).await.unwrap_err().contains("not joined"));
        // The twenty-first message inside thirty seconds waits - unless we moderate, which the fake said we do.
        for i in 0..21 {
            say_in(&tx, "xqc", &format!("m{i}"), None).await.unwrap();
        }
        task.abort();
    }

    #[tokio::test]
    async fn an_ordinary_account_is_told_to_wait_after_twenty_messages() {
        let state = testkit::simulated_daemon("twitch-session-limit");
        signed_in_chat(&state);
        // No USERSTATE says moderator here.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(Ws::Text(text))) = ws.next().await {
                for line in text.split("\r\n") {
                    if line.starts_with("NICK") {
                        let _ = ws.send(Ws::Text(":tmi.twitch.tv 001 me :Welcome\r\n".into())).await;
                    }
                    if let Some(c) = line.strip_prefix("JOIN #") {
                        let _ = ws.send(Ws::Text(format!("@room-id=1 :tmi.twitch.tv ROOMSTATE #{c}\r\n"))).await;
                    }
                }
            }
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, id) = (state.clone(), "twitch:someone");
        let task = tokio::spawn(async move { session(&s, id, &mut rx, &url, quick()).await });
        join(&tx, "xqc").await.unwrap();
        for i in 0..SEND_LIMIT {
            say_in(&tx, "xqc", &format!("m{i}"), None).await.unwrap();
        }
        let e = say_in(&tx, "xqc", "one too many", None).await.unwrap_err();
        assert!(e.contains("20 messages in 30 seconds"), "{e}");
        task.abort();
    }

    #[tokio::test]
    async fn twitch_saying_reconnect_ends_the_session_cleanly_and_a_refused_login_is_an_error() {
        let state = testkit::simulated_daemon("twitch-session-reconnect");
        anonymous(&state);
        let url = fake_twitch(Fake { exists: vec!["xqc"], reconnect: true, ..Default::default() }).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, id) = (state.clone(), "twitch:~anonymous");
        let task = tokio::spawn(async move { session(&s, id, &mut rx, &url, quick()).await });
        let _ = join(&tx, "xqc").await;
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await.expect("it ends").unwrap();
        assert!(ended.is_ok(), "RECONNECT is not a failure");

        let state = testkit::simulated_daemon("twitch-session-refused");
        signed_in_chat(&state);
        let url = fake_twitch(Fake { refuse: true, ..Default::default() }).await;
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let e = session(&state, "twitch:someone", &mut rx, &url, quick()).await.unwrap_err();
        assert!(format!("{e}").contains("authentication failed"), "{e}");
    }

    #[test]
    fn what_people_type_for_a_channel_is_read_as_a_login() {
        for (typed, login) in [("xqc", "xqc"), ("#XQC", "xqc"), ("@Xqc", "xqc"), ("https://www.twitch.tv/xqc", "xqc"), ("twitch.tv/xqc?sr=a", "xqc"), ("  some_one  ", "some_one")] {
            assert_eq!(normalise_login(typed).as_deref(), Some(login), "{typed}");
        }
        for typed in ["", "   ", "has space", "bang!", "a".repeat(26).as_str(), "twitch.tv/"] {
            assert_eq!(normalise_login(typed), None, "{typed}");
        }
    }
}
