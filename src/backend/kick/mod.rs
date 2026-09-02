//! Kick - livestream chat, read over the same Pusher websocket kick.com's own
//! player page uses.
//!
//! Two things make this backend shaped differently from the others here.
//!
//! **A channel is a streamer, not a room you are a member of.** There is no
//! join to perform and no membership to hold: naming a handle is the whole of
//! it, and the chat is public to anybody with a browser. So a Kick account
//! with no credential at all is useful - it reads every channel it is pointed
//! at - and signing in buys exactly two things, sending and knowing what you
//! are subscribed to. That is why the token is optional throughout rather than
//! a precondition for connecting.
//!
//! **One websocket carries every channel.** Pusher multiplexes: each channel
//! is a `chatrooms.<id>.v2` subscription on the one connection, so watching
//! twenty streamers costs one socket rather than twenty. Sneedchat next door
//! opens one per room because its protocol has no such verb; this one does,
//! and joining a channel while connected is a subscribe frame rather than a
//! reconnect.

pub mod api;
pub mod emotes;

use crate::accounts::KickAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{Context, Result};
use futures::{FutureExt, SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Kick's own Pusher application, as served to every visitor of kick.com.
///
/// Public by construction: a Pusher *key* is the identifier a browser needs to
/// subscribe to public channels, which is why it appears in their page source
/// and why reading a public chat needs no credential. It is not a secret and
/// grants nothing beyond what a browser on the channel page already has.
const PUSHER_KEY: &str = "32cbd69e4b950bf97679";
const PUSHER_CLUSTER: &str = "us2";

fn pusher_url() -> String {
    format!(
        "wss://ws-{PUSHER_CLUSTER}.pusher.com/app/{PUSHER_KEY}?protocol=7&client=moho&version=8.4.0&flash=false"
    )
}

/// Asked of the connection from elsewhere in the daemon, over a channel,
/// because the socket is owned by its own task and these arrive from RPC.
#[derive(Debug)]
pub enum Command {
    /// Watch a channel, by whatever the user typed as a handle.
    Join(String),
    /// Stop watching one.
    Part(String),
}

pub fn spawn(state: AppState, config: KickAccountConfig) {
    let account_id = config.account_id();
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

/// How many followed channels to open on a first connect.
///
/// A cap rather than all of them, because somebody can follow hundreds and
/// each one costs a buffer, four subscriptions and its own emote table. Fifty
/// is more channels than anyone watches at once and still opens quickly; the
/// rest are a handle away in the "+" box.
pub const MAX_FOLLOWED: usize = 50;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

async fn run_with_retry(state: &AppState, config: &KickAccountConfig, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        // Re-read the account rather than reusing the one this task was
        // spawned with. Everything about it can have changed since - and one
        // thing routinely has: the channels being watched, because watching a
        // streamer is a live command that also writes itself to disk. Running
        // from the spawn-time copy meant a reconnect resubscribed to whatever
        // was configured minutes ago, so a channel joined during the session
        // survived exactly until the first blip and then went quiet for good,
        // with the buffer still sitting there looking connected.
        let current = state.accounts.get_kick(account_id);
        let config = current.as_ref().unwrap_or(config);
        let result = std::panic::AssertUnwindSafe(run(state, config, account_id)).catch_unwind().await;
        let detail = match result {
            Ok(Ok(())) => "connection ended".to_string(),
            Ok(Err(e)) => {
                tracing::warn!("kick[{account_id}]: {e:#}");
                format!("{e:#}")
            }
            Err(_) => {
                tracing::error!("kick[{account_id}]: connection task panicked");
                "internal error (see nobilis logs)".to_string()
            }
        };
        state.runtime.clear_kick_sender(account_id);
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        state.runtime.report_progress(state, account_id, &format!("{detail} - reconnecting in {}s...", delay.as_secs()));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

async fn run(state: &AppState, config: &KickAccountConfig, account_id: &str) -> Result<()> {
    let http = api::client()?;

    // Signed in or not is settled first, because everything downstream reads
    // differently depending: with no token this is a viewer, and the UI should
    // say so rather than failing at the moment somebody tries to talk.
    if let Some(token) = config.token.as_deref().filter(|t| !t.is_empty()) {
        match api::identity(&http, token).await {
            Ok(who) => {
                if let Some(name) = who.username {
                    state.runtime.set_own_identity(account_id, &name);
                }
            }
            // Not fatal. A rejected token still leaves a working reader, and
            // saying so beats a reconnect loop that never explains itself.
            Err(e) => state.runtime.report_progress(state, account_id, &format!("signed out: {e:#}")),
        }
    }

    // What this account already follows on Kick, the first time it connects.
    //
    // Once, and never again - the flag is persisted. A follow list re-read on
    // every connect would put back every channel the person had closed, which
    // is the one thing closing a channel is supposed to mean; after this, the
    // list is theirs.
    let mut channels = config.channels.clone();
    if let Some(token) = config.token.as_deref().filter(|t| !t.is_empty()) {
        if !config.followed_synced {
            match api::followed(&http, token).await {
                Ok(followed) => {
                    let before = channels.len();
                    for slug in followed.into_iter().take(MAX_FOLLOWED) {
                        if !channels.contains(&slug) {
                            channels.push(slug);
                        }
                    }
                    let added = channels.len() - before;
                    let _ = state.accounts.set_kick_channels(account_id, channels.clone());
                    let _ = state.accounts.mark_kick_follows_synced(account_id);
                    if added > 0 {
                        state.runtime.report_progress(state, account_id, &format!("following {added} channels you already follow on Kick"));
                    }
                }
                // Not fatal, and not retried on the next connect either -
                // marking it done anyway would be worse, since a person who
                // has since closed those channels would get them all back.
                // Reported, so a failure is visible rather than silent.
                Err(e) => state.runtime.report_progress(state, account_id, &format!("could not read your Kick follows: {e:#}")),
            }
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    state.runtime.set_kick_sender(account_id, tx);

    let (mut socket, _) = tokio_tungstenite::connect_async(pusher_url())
        .await
        .context("connecting to Kick's chat")?;

    // Two maps because Kick uses two numbers. A chat message names the
    // chatroom; the events on `channel.<id>` name the channel, which is a
    // different number for every streamer except the oldest few - where the
    // two happen to be equal, which is exactly the kind of coincidence that
    // makes one map look like it works.
    let mut watched = Watched::default();

    for slug in &channels {
        match prepare(state, &http, config, account_id, slug).await {
            Ok(channel) => {
                subscribe(&mut socket, channel.chatroom_id, channel.id).await?;
                watched.add(&channel);
            }
            // One bad handle in a saved list must not stop the other twenty
            // from connecting - a channel can be renamed or banned between
            // sessions, and that is this account's problem with one buffer
            // rather than with Kick.
            Err(e) => {
                tracing::warn!("kick[{account_id}]: {slug}: {e:#}");
                state.runtime.report_progress(state, account_id, &format!("{slug}: {e:#}"));
            }
        }
    }

    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);

    // Pusher closes a socket it has not heard from. Its own timeout arrives in
    // the handshake, but a fixed interval well inside the shortest it ever
    // sends is simpler than tracking it and cannot be wrong in the direction
    // that matters.
    let mut ping = tokio::time::interval(Duration::from_secs(60));
    ping.tick().await;

    loop {
        tokio::select! {
            command = rx.recv() => match command {
                None => return Ok(()),
                Some(Command::Join(handle)) => {
                    match prepare(state, &http, config, account_id, &handle).await {
                        Ok(channel) => {
                            // Propagated, not reported: a subscribe that will
                            // not go out means this socket is finished, and
                            // reconnecting re-subscribes everything including
                            // the channel just asked for.
                            subscribe(&mut socket, channel.chatroom_id, channel.id).await?;
                            watched.add(&channel);
                        }
                        Err(e) => state.runtime.report_progress(state, account_id, &format!("{e:#}")),
                    }
                }
                Some(Command::Part(slug)) => {
                    for name in watched.remove(&slug) {
                        let _ = socket.send(WsMessage::Text(serde_json::json!({
                            "event": "pusher:unsubscribe",
                            "data": { "channel": name }
                        }).to_string())).await;
                    }
                    state.runtime.forget_kick_channel(&format!("{account_id}|{slug}"));
                }
            },
            _ = ping.tick() => {
                socket.send(WsMessage::Text(r#"{"event":"pusher:ping","data":{}}"#.to_string())).await
                    .context("Kick's chat connection went away")?;
            }
            frame = socket.next() => match frame {
                None => anyhow::bail!("Kick closed the chat connection"),
                Some(Err(e)) => return Err(e).context("reading from Kick's chat"),
                Some(Ok(WsMessage::Text(text))) => handle_frame(state, account_id, &mut watched, &text, &mut socket).await?,
                Some(Ok(WsMessage::Close(_))) => anyhow::bail!("Kick closed the chat connection"),
                Some(Ok(_)) => {}
            },
        }
    }
}

/// The channels this connection is following, by both of the numbers Kick
/// identifies them with.
///
/// Two maps rather than one keyed by "some id", because the two number spaces
/// are unrelated: nothing stops one streamer's chatroom id from equalling
/// another's channel id, and a single map would then deliver one streamer's
/// subscriptions into the other's buffer. Which map to ask is never a guess -
/// the subscription name says which kind of number it carries.
#[derive(Default)]
struct Watched {
    by_chatroom: HashMap<u64, String>,
    by_channel: HashMap<u64, String>,
    /// Who has spoken in each channel, most recent first.
    ///
    /// Not a viewer list, and the client says so. Kick has no endpoint for
    /// one - every plausible path answers 404, and Kick's own page shows none
    /// either, because a livestream chat has no roster the way a channel does.
    /// What it does have is people talking, which is what somebody actually
    /// wants the panel for: to mention them, whisper them, or see who the
    /// moderators are.
    speakers: HashMap<String, Vec<Speaker>>,
}

/// Somebody who has said something, and how they looked saying it.
#[derive(Clone, Debug)]
struct Speaker {
    nick: String,
    user_id: Option<String>,
    /// The strongest badge they carry, which is what the list marks them
    /// with - a moderator is worth finding in a list of two hundred names.
    badge: Option<String>,
}

/// How many to remember per channel.
///
/// A busy stream produces hundreds of distinct names an hour, and a list
/// nobody can scan is no better than an empty one. This is the last two
/// hundred to have spoken, which is the window somebody is actually reading.
const SPEAKERS_REMEMBERED: usize = 200;

impl Watched {
    /// Records that somebody spoke, and answers whether the list changed
    /// enough to be worth re-announcing.
    ///
    /// Somebody already in the list is not a change: a busy channel would
    /// otherwise emit a roster every few hundred milliseconds, all of them
    /// nearly identical. Their badges changing is - somebody subscribes, or
    /// is given moderator, and the list should follow.
    fn heard(&mut self, slug: &str, speaker: Speaker) -> bool {
        let list = self.speakers.entry(slug.to_string()).or_default();
        if let Some(existing) = list.iter_mut().find(|s| s.nick == speaker.nick) {
            let changed = existing.badge != speaker.badge;
            existing.badge = speaker.badge;
            return changed;
        }
        list.insert(0, speaker);
        list.truncate(SPEAKERS_REMEMBERED);
        true
    }

    fn roster(&self, slug: &str) -> Vec<serde_json::Value> {
        self.speakers
            .get(slug)
            .map(|list| {
                list.iter()
                    .map(|s| {
                        serde_json::json!({
                            "nick": s.nick,
                            "userId": s.user_id.clone().unwrap_or_default(),
                            "prefix": s.badge.clone().unwrap_or_default(),
                            "away": false,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn add(&mut self, channel: &api::Channel) {
        self.by_chatroom.insert(channel.chatroom_id, channel.slug.clone());
        self.by_channel.insert(channel.id, channel.slug.clone());
    }

    /// Forgets a channel, and names every subscription it was using so the
    /// caller can cancel each one. All four, or the socket keeps delivering
    /// events for a conversation that has been closed.
    fn remove(&mut self, slug: &str) -> Vec<String> {
        self.speakers.remove(slug);
        let mut names = Vec::new();
        if let Some(room) = self.by_chatroom.iter().find(|(_, s)| *s == slug).map(|(i, _)| *i) {
            self.by_chatroom.remove(&room);
            names.push(format!("chatrooms.{room}.v2"));
            names.push(format!("chatrooms.{room}"));
            names.push(format!("chatroom_{room}"));
        }
        if let Some(channel) = self.by_channel.iter().find(|(_, s)| *s == slug).map(|(i, _)| *i) {
            self.by_channel.remove(&channel);
            names.push(format!("channel.{channel}"));
        }
        names
    }

    fn chatroom(&self, id: u64) -> Option<&String> {
        self.by_chatroom.get(&id)
    }
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Everything about starting to follow a channel except the subscription:
/// resolve the handle, make its buffer, learn its emotes.
///
/// Split from the subscribe on purpose, because the two fail for opposite
/// reasons and want opposite answers. A handle that does not resolve is this
/// one channel's problem and the connection carries on without it; a socket
/// that will not carry the subscription is the connection's problem, and
/// swallowing that leaves a buffer sitting there looking joined while nothing
/// will ever arrive in it.
///
/// The buffer exists before the subscription does, so the first message to
/// arrive has somewhere to land rather than racing the thing that would have
/// created it.
async fn prepare(
    state: &AppState,
    http: &reqwest::Client,
    config: &KickAccountConfig,
    account_id: &str,
    handle: &str,
) -> Result<api::Channel> {
    let channel = api::channel(http, handle).await?;

    // No set_buffer_group here. ensure_buffer already files a new buffer under
    // its own account's rail entry, which is exactly where a Kick channel
    // belongs - there is no guild or space above it. Saying so again with the
    // bare account id wrote a group that does not exist, and a buffer in a
    // group with no rail tile is a conversation with no way to reach it:
    // receiving fine, listed by the daemon, and invisible.
    let buffer = state.runtime.ensure_buffer(state, account_id, &channel.slug, "channel");
    state.runtime.set_kick_channel(
        &buffer.id,
        crate::runtime::KickChannel {
            slug: channel.slug.clone(),
            channel_id: channel.id,
            history_cursor: None,
            chatroom_id: channel.chatroom_id,
            subscribed: false,
            followers_only: channel.followers_only,
            subscribers_only: channel.subscribers_only,
            slow_seconds: None,
            emotes: Vec::new(),
        },
    );

    // How the chat is restricted, said before anybody tries to speak into it.
    // Both values were already being parsed and then read by nothing, so moho
    // knew a channel was subscribers-only and let somebody type into it
    // anyway, learning otherwise from a refused send.
    state.runtime.set_kick_chat_mode(
        state,
        &buffer.id,
        channel.followers_only,
        channel.subscribers_only,
        None,
    );

    // The past, before the present starts arriving. Backgrounded with the
    // rest so a channel's first messages are not held up by it, and recorded
    // with their own timestamps so they sort into place whichever lands first.
    //
    // The cursor is kept so scrolling up continues where this stopped rather
    // than fetching the same page again.
    {
        let state = state.clone();
        let http = http.clone();
        let slug = channel.slug.clone();
        let buffer_id = buffer.id.clone();
        let account_id = account_id.to_string();
        let channel_id = channel.id;
        tokio::spawn(async move {
            let next = backfill(&state, &http, &account_id, &slug, channel_id, None).await;
            state.runtime.set_kick_history_cursor(&buffer_id, next);
        });
    }

    // The emote table and this account's standing in the channel are fetched
    // in the background rather than before the subscription.
    //
    // They are two more round trips per channel, and connecting can open fifty
    // at once now that a first connect brings in what the account follows -
    // which made "connecting" take as long as a hundred and fifty requests
    // while no messages arrived at all. Nothing needs them to be there yet:
    // the room id above is what a message needs to land, and the emote picker
    // is opened seconds later at the earliest.
    tokio::spawn({
        let state = state.clone();
        let http = http.clone();
        let token = config.token.clone().filter(|t| !t.is_empty());
        let slug = channel.slug.clone();
        let buffer_id = buffer.id.clone();
        async move {
            // Whether this account may *use* the streamer's subscriber emotes.
            // Treated as "no" if Kick will not say, which costs a greyed-out
            // emote rather than a conversation.
            let subscribed = match token.as_deref() {
                None => false,
                Some(token) => api::standing(&http, token, &slug).await.map(|s| s.subscribed).unwrap_or(false),
            };
            let emotes = api::emotes(&http, &slug).await.unwrap_or_default();
            state.runtime.set_kick_emotes(&buffer_id, emotes, subscribed);
        }
    });

    Ok(channel)
}

/// Asks the socket for everything that happens in a channel's chat.
///
/// Four subscriptions, not one, because Kick scatters them and only the first
/// carries what people say. Watching that one alone is why a chat can look
/// quiet while the stream is visibly reacting to something. Measured against a
/// live subathon rather than guessed - each name here is one an event was
/// actually observed arriving on:
///
///   - `chatrooms.<room>.v2`  what people say, and plain subscriptions
///   - `chatrooms.<room>`     rewards, and the info lines Kick's own UI draws
///   - `chatroom_<room>`      gifted subscriptions, on an older name entirely
///   - `channel.<channel>`    going live, and the channel's own subscriptions
///
/// The overlap is real and deliberate on Kick's part: one subscription arrives
/// three times over three of these. Which of the three is believed is settled
/// in `describe_action`, not here - subscribing to fewer channels would lose
/// whole categories of event to avoid a duplicate that is cheaper to filter.
///
/// Empty auth: all four are public, and this backend subscribes to nothing
/// that is not.
async fn subscribe(socket: &mut Socket, chatroom_id: u64, channel_id: u64) -> Result<()> {
    for channel in [
        format!("chatrooms.{chatroom_id}.v2"),
        format!("chatrooms.{chatroom_id}"),
        format!("chatroom_{chatroom_id}"),
        format!("channel.{channel_id}"),
    ] {
        socket
            .send(WsMessage::Text(
                serde_json::json!({ "event": "pusher:subscribe", "data": { "auth": "", "channel": channel } })
                    .to_string(),
            ))
            .await
            .context("subscribing to the channel's chat")?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct PusherFrame {
    event: String,
    #[serde(default)]
    data: serde_json::Value,
    /// Which subscription this arrived on. The most reliable way to know
    /// whose channel an event belongs to: a chat message names its room in the
    /// payload, but the doing-things events name variously the channel, the
    /// chatroom, or nothing at all - while Pusher always says which
    /// subscription delivered it.
    #[serde(default)]
    channel: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    id: String,
    chatroom_id: u64,
    content: String,
    #[serde(default)]
    created_at: Option<String>,
    sender: Sender,
    /// Present on a reply, carrying the whole of what was replied to.
    ///
    /// Kick sends the original's text along with its id, which is worth more
    /// than it sounds: every other protocol here has to look the original up
    /// in scrollback and shows a bare "in reply to" when it has scrolled past
    /// the cap. Kick's replies are quotable however old the original is.
    #[serde(default)]
    metadata: Option<ReplyMetadata>,
}

#[derive(Deserialize)]
struct ReplyMetadata {
    #[serde(default)]
    original_sender: Option<Sender>,
    #[serde(default)]
    original_message: Option<OriginalMessage>,
}

#[derive(Deserialize)]
struct OriginalMessage {
    id: String,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Sender {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    id: Option<u64>,
    /// Their colour and badges. Present on every message and previously read
    /// by nothing, so a moderator, a two-year subscriber and a stranger all
    /// drew identically.
    #[serde(default)]
    identity: Option<api::ChatIdentity>,
}

async fn handle_frame(
    state: &AppState,
    account_id: &str,
    watched: &mut Watched,
    text: &str,
    socket: &mut Socket,
) -> Result<()> {
    let frame: PusherFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };

    // Pusher nests the real payload as a JSON *string* inside the frame, so
    // every event needs unwrapping twice.
    let payload = match &frame.data {
        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(s).unwrap_or(serde_json::Value::Null),
        other => other.clone(),
    };

    match frame.event.as_str() {
        "pusher:ping" => {
            socket.send(WsMessage::Text(r#"{"event":"pusher:pong","data":{}}"#.to_string())).await?;
        }
        "pusher:error" => {
            let msg = payload.get("message").and_then(|m| m.as_str()).unwrap_or("Kick refused the connection");
            anyhow::bail!("{msg}");
        }
        e if e.ends_with("ChatMessageEvent") => {
            let Ok(msg) = serde_json::from_value::<ChatMessage>(payload) else { return Ok(()) };
            let Some(slug) = watched.chatroom(msg.chatroom_id).cloned() else { return Ok(()) };
            let from = msg.sender.username.clone().unwrap_or_else(|| "someone".to_string());
            let reply_to = reply_preview(msg.metadata.as_ref());
            let style = style_of(msg.sender.identity.as_ref());
            // Kick has no viewer list to ask for, so the panel is built from
            // who is talking - which is who somebody wants to reach anyway.
            let slug = slug.clone();
            if watched.heard(
                &slug,
                Speaker {
                    nick: from.clone(),
                    user_id: msg.sender.id.map(|i| i.to_string()),
                    badge: strongest_badge(&style.badges),
                },
            ) {
                let buffer_id = crate::model::buffer_id(account_id, &slug);
                let roster = serde_json::json!(watched.roster(&slug));
                state.runtime.set_presence(&buffer_id, roster.clone());
                state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer_id, "members": roster }));
            }
            state.runtime.record_message_at(
                state,
                account_id,
                &slug,
                "channel",
                &from,
                &msg.content,
                false,
                "chat",
                reply_to,
                // Kick's own message id, so the same message replayed after a
                // reconnect is recognised rather than shown twice.
                Some(msg.id),
                false,
                None,
                Vec::new(),
                Vec::new(),
                msg.sender.id.map(|i| i.to_string()),
                msg.created_at.as_deref().and_then(parse_timestamp),
                None,
                // How Kick says this person looks: their colour and what they
                // have earned. Both arrive on every message and were both
                // being dropped at the struct boundary, which is why every
                // line looked the same.
                Some(style),
            );
        }
        // Worth a line in the channel it happened in: somebody watching a
        // handful of streamers is largely watching for this.
        e if e.ends_with("StreamerIsLive") => {
            if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                system_line(state, account_id, &slug, "went live", "system");
            }
        }
        e if e.ends_with("StopStreamBroadcast") => {
            if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                system_line(state, account_id, &slug, "ended the stream", "system");
            }
        }
        // A message taken down, by its author or by a moderator. Removed here
        // too, or moho shows a different chat from the one everybody else is
        // looking at - which on a stream where moderation is public is the
        // wrong way round entirely.
        e if e.ends_with("MessageDeletedEvent") => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            // The id is nested under `message` on this event and flat on
            // others Kick has sent; both name the same thing.
            let id = payload
                .get("message")
                .and_then(|m| m.get("id"))
                .or_else(|| payload.get("id"))
                .and_then(|v| v.as_str());
            if let Some(id) = id {
                state.runtime.delete_message(state, &crate::model::buffer_id(account_id, &slug), id);
            }
        }

        // The chat's rules changing under everybody - followers-only going
        // on mid-stream is the usual reason a message stops sending.
        e if e.ends_with("ChatroomUpdatedEvent") => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            let flag = |name: &str| payload.get(name).and_then(|v| v.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(false);
            let slow = payload
                .get("slow_mode")
                .filter(|m| m.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
                .and_then(|m| m.get("message_interval").and_then(|v| v.as_u64()))
                .map(|s| s as u32);
            state.runtime.set_kick_chat_mode(
                state,
                &crate::model::buffer_id(account_id, &slug),
                flag("followers_mode"),
                flag("subscribers_mode"),
                slow,
            );
        }

        // A moderator emptying the room.
        e if e.ends_with("ChatroomClearEvent") => {
            if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                system_line(state, account_id, &slug, "a moderator cleared the chat", "system");
            }
        }

        // The things people do rather than say.
        e => {
            if let Some(line) = describe_action(e, &payload) {
                if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                    system_line(state, account_id, &slug, &line, "reward");
                }
            }
        }
    }
    Ok(())
}

/// Fills a channel's scrollback from what was said before we arrived.
///
/// Kick's chat is a live stream and nothing else - a channel opened for the
/// first time showed an empty log until somebody spoke, which on a quiet
/// stream is a conversation that appears not to exist.
///
/// Recorded oldest-first so the log reads in order, and with Kick's own ids so
/// anything already stored is recognised rather than duplicated. That matters
/// more here than elsewhere: this runs on every join, and a reconnect joins
/// again.
pub async fn backfill(
    state: &AppState,
    http: &reqwest::Client,
    account_id: &str,
    slug: &str,
    channel_id: u64,
    cursor: Option<&str>,
) -> Option<String> {
    let (messages, next) = match api::history(http, channel_id, cursor).await {
        Ok(page) => page,
        // Not fatal and not worth interrupting anybody over: the channel still
        // works, it just starts empty, which is what it did before.
        Err(e) => {
            tracing::debug!("kick[{account_id}]: history for {slug}: {e:#}");
            return None;
        }
    };
    for msg in messages.iter().rev() {
        if msg.content.is_empty() {
            continue;
        }
        let from = msg.sender.username.clone().unwrap_or_else(|| "someone".to_string());
        state.runtime.record_message_at(
            state,
            account_id,
            slug,
            "channel",
            &from,
            &msg.content,
            false,
            "chat",
            reply_preview_from_value(msg.metadata.as_ref()),
            Some(msg.id.clone()),
            false,
            None,
            Vec::new(),
            Vec::new(),
            msg.sender.id.map(|i| i.to_string()),
            msg.created_at.as_deref().and_then(parse_timestamp),
            None,
            Some(style_of(msg.sender.identity.as_ref())),
        );
    }
    next
}

/// The same reply metadata the live path reads, out of an untyped value.
///
/// History carries it in the same shape, so this is the one parser rather than
/// a second one that could drift.
fn reply_preview_from_value(metadata: Option<&serde_json::Value>) -> Option<crate::model::ReplyPreview> {
    let parsed: ReplyMetadata = serde_json::from_value(metadata?.clone()).ok()?;
    reply_preview(Some(&parsed))
}

/// The one badge worth marking somebody with in a list.
///
/// A name can carry four; a list showing all of them is a list of badges with
/// names attached. Ranked by what a reader is scanning for - who runs the
/// channel, who moderates it - rather than by what is rarest.
fn strongest_badge(badges: &[api::Badge]) -> Option<String> {
    for wanted in ["broadcaster", "moderator", "vip", "og", "founder", "subscriber"] {
        if badges.iter().any(|b| b.kind == wanted) {
            return Some(wanted.to_string());
        }
    }
    None
}

/// How a sender should be drawn, out of what Kick sent with the message.
///
/// Badges are filtered to the ones that say something about the person rather
/// than about Kick: a "level" badge is an engagement number and a row of them
/// beside every nick is noise, while moderator, subscriber and verified are
/// the ones a reader is actually scanning for.
fn style_of(identity: Option<&api::ChatIdentity>) -> crate::model::SenderStyle {
    let Some(identity) = identity else { return crate::model::SenderStyle::default() };
    crate::model::SenderStyle {
        color: identity.color.clone().filter(|c| !c.is_empty()),
        badges: identity
            .badges
            .iter()
            .filter(|b| !matches!(b.kind.as_str(), "level" | ""))
            .cloned()
            .collect(),
    }
}

/// What a reply was replying to, out of the metadata Kick attaches to it.
///
/// Both halves are required rather than filled in with blanks: a preview with
/// no author and no text is drawn as an empty quote above the message, which
/// reads as something failing to load rather than as a reply. Better to show
/// the message plainly than to show a hole above it.
fn reply_preview(metadata: Option<&ReplyMetadata>) -> Option<crate::model::ReplyPreview> {
    let metadata = metadata?;
    let original = metadata.original_message.as_ref()?;
    let from = metadata.original_sender.as_ref()?.username.clone()?;
    Some(crate::model::ReplyPreview {
        id: original.id.clone(),
        from,
        body: original.content.clone().unwrap_or_default(),
    })
}

/// One line for something somebody did in a channel, or None if this event is
/// not one of those.
///
/// These are not chatter and are not system noise either: a redemption or a
/// gifted sub is somebody spending something, and it is usually the thing the
/// stream is about to react to. So they are recorded with their own kind and
/// the client draws them on a plate of their own - visible when scrolling past
/// at speed, which is the entire point of them.
///
/// Written against the field names Kick uses and tolerant of the ones it has
/// used: `username` and `sender.username` both appear across these events, and
/// an event whose shape has moved on should degrade to being skipped rather
/// than to a line with "unknown" in it.
fn describe_action(event: &str, p: &serde_json::Value) -> Option<String> {
    // Matched exactly, on the last segment, rather than by suffix. Kick sends
    // one real subscription as three events on three subscriptions - measured
    // on a live subathon: `SubscriptionEvent`, `ChannelSubscriptionEvent` and
    // an info-line `ChatMessageSentEvent`, all for the same person subscribing
    // once. A suffix test reported it twice, because "ChannelSubscriptionEvent"
    // ends with "SubscriptionEvent". So each name is listed and the duplicates
    // are named as the duplicates they are.
    let name = event.rsplit('\\').next().unwrap_or(event);

    let text = |key: &str| p.get(key).and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
    let who = || {
        text("username")
            .or_else(|| p.get("sender").and_then(|s| s.get("username")).and_then(|v| v.as_str()).map(str::to_string))
            .or_else(|| p.get("user").and_then(|s| s.get("username")).and_then(|v| v.as_str()).map(str::to_string))
    };

    match name {
        "RewardRedeemedEvent" => {
            let who = who()?;
            let reward = text("reward_title").or_else(|| text("title"))?;
            // What they typed with it, where the reward takes input - it is
            // the half that says what they actually asked for.
            Some(match text("user_input") {
                Some(input) => format!("{who} redeemed {reward}: {input}"),
                None => format!("{who} redeemed {reward}"),
            })
        }

        "GiftedSubscriptionsEvent" => {
            let gifter = text("gifter_username").or_else(who)?;
            let count = p
                .get("gifted_usernames")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .or_else(|| p.get("gifted_total").and_then(|v| v.as_u64()).map(|n| n as usize))
                .unwrap_or(1);
            Some(match count {
                0 | 1 => format!("{gifter} gifted a subscription"),
                n => format!("{gifter} gifted {n} subscriptions"),
            })
        }

        // The one of the three that carries how long they have been
        // subscribed, which is the part worth saying.
        "SubscriptionEvent" => {
            let who = who()?;
            Some(match p.get("months").and_then(|v| v.as_u64()) {
                Some(n) if n > 1 => format!("{who} subscribed - {n} months"),
                _ => format!("{who} subscribed"),
            })
        }

        "StreamHostEvent" | "StreamHostedEvent" => {
            let who = text("host_username").or_else(who)?;
            Some(match p.get("number_viewers").and_then(|v| v.as_u64()) {
                Some(n) if n > 0 => format!("{who} raided with {n} viewers"),
                _ => format!("{who} raided the channel"),
            })
        }

        // Somebody timed out or banned. Said in the channel, because it is
        // moderation everybody in the room can see happening.
        "UserBannedEvent" => {
            let who = p.get("user").and_then(|u| u.get("username")).and_then(|v| v.as_str())?;
            let by = p
                .get("banned_by")
                .and_then(|u| u.get("username"))
                .and_then(|v| v.as_str())
                .map(|b| format!(" by {b}"))
                .unwrap_or_default();
            // A ban carries no expiry and a timeout does, which is the whole
            // of what a reader wants to know from the line.
            Some(match p.get("expires_at").and_then(|v| v.as_str()) {
                Some(_) => format!("{who} was timed out{by}"),
                None => format!("{who} was banned{by}"),
            })
        }
        "UserUnbannedEvent" => {
            let who = p.get("user").and_then(|u| u.get("username")).and_then(|v| v.as_str())?;
            Some(format!("{who} is allowed back"))
        }

        // Deliberately nothing. Both are the same subscription reported again
        // on another subscription - `ChatMessageSentEvent` is the info line
        // Kick's own page draws from, and `LuckyUsers...` is the companion to
        // a gift that has already been announced.
        "ChannelSubscriptionEvent" | "ChatMessageSentEvent" | "LuckyUsersWhoGotGiftSubscriptionsEvent" => None,

        _ => None,
    }
}

/// Which watched channel an event belongs to.
///
/// The subscription it arrived on is asked first and is the reliable answer:
/// this connection subscribed per room, so the room is in the name. The
/// payload is the fallback, because these events disagree about what they name
/// - some carry the chatroom, some the channel, some neither - and a fallback
/// that is occasionally right beats an event dropped for lack of a field.
fn channel_of(watched: &Watched, subscription: &Option<String>, payload: &serde_json::Value) -> Option<String> {
    if let Some(name) = subscription.as_deref() {
        match id_in_subscription(name) {
            Some(Subscribed::Chatroom(id)) => {
                if let Some(slug) = watched.by_chatroom.get(&id) {
                    return Some(slug.clone());
                }
            }
            Some(Subscribed::Channel(id)) => {
                if let Some(slug) = watched.by_channel.get(&id) {
                    return Some(slug.clone());
                }
            }
            None => {}
        }
    }
    // Each id is looked up only in its own map, for the same reason there are
    // two of them.
    if let Some(id) = payload.get("chatroom_id").and_then(|v| v.as_u64()) {
        if let Some(slug) = watched.by_chatroom.get(&id) {
            return Some(slug.clone());
        }
    }
    let channel = payload
        .get("livestream")
        .and_then(|l| l.get("channel_id"))
        .or_else(|| payload.get("channel_id"))
        .and_then(|v| v.as_u64())?;
    watched.by_channel.get(&channel).cloned()
}

/// Which of Kick's two numbers a subscription name carries.
#[derive(Debug, PartialEq, Eq)]
enum Subscribed {
    Chatroom(u64),
    Channel(u64),
}

fn id_in_subscription(name: &str) -> Option<Subscribed> {
    if let Some(rest) = name.strip_prefix("chatrooms.").or_else(|| name.strip_prefix("chatroom_")) {
        return rest.split('.').next()?.parse().ok().map(Subscribed::Chatroom);
    }
    if let Some(rest) = name.strip_prefix("channel.") {
        return rest.split('.').next()?.parse().ok().map(Subscribed::Channel);
    }
    None
}

/// A line that nobody said.
///
/// `kind` is what separates "the stream started" from "somebody redeemed
/// something": both are events rather than chatter, but only one of them is a
/// person doing something, and the client draws that one on a plate.
fn system_line(state: &AppState, account_id: &str, slug: &str, what: &str, kind: &str) {
    state.runtime.record_message(
        state, account_id, slug, "channel", slug, what, false, kind, None, None, false, None,
        Vec::new(), Vec::new(), None,
    );
}

/// Kick's timestamps, as unix seconds.
///
/// They arrive as RFC 3339 (`2026-01-08T23:32:57.000000Z`). Without this every
/// message from a reconnect's replay would be dated to the reconnect - the same
/// problem `record_message_at` exists to solve everywhere else.
fn parse_timestamp(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|t| t.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_kicks_timestamps() {
        assert_eq!(parse_timestamp("2026-01-08T23:32:57.000000Z"), Some(1767915177));
        assert_eq!(parse_timestamp("2026-01-08T23:32:57Z"), Some(1767915177));
        assert_eq!(parse_timestamp("not a time"), None);
    }

    /// odablock's real numbers: the chatroom and the channel differ, which is
    /// the ordinary case and the one a single map gets wrong.
    fn watching_odablock() -> Watched {
        let mut w = Watched::default();
        w.add(&api::Channel {
            id: 2401072,
            chatroom_id: 2393554,
            slug: "odablock".into(),
            username: "odablock".into(),
            avatar_url: None,
            subscribers_only: false,
            followers_only: false,
        });
        w
    }

    #[test]
    fn finds_the_channel_by_the_subscription_it_arrived_on() {
        let w = watching_odablock();
        let none = serde_json::json!({});
        // All four names this connection subscribes to resolve to the same
        // streamer, across both of Kick's number spaces.
        for name in ["chatrooms.2393554.v2", "chatrooms.2393554", "chatroom_2393554", "channel.2401072"] {
            assert_eq!(channel_of(&w, &Some(name.into()), &none).as_deref(), Some("odablock"), "for {name}");
        }
    }

    #[test]
    fn falls_back_to_whichever_id_the_payload_carries() {
        let w = watching_odablock();
        for payload in [
            serde_json::json!({ "livestream": { "channel_id": 2401072 } }),
            serde_json::json!({ "channel_id": 2401072 }),
            serde_json::json!({ "chatroom_id": 2393554 }),
        ] {
            assert_eq!(channel_of(&w, &None, &payload).as_deref(), Some("odablock"), "for {payload}");
        }
        // A channel this account is not watching is not ours to report.
        assert_eq!(channel_of(&w, &None, &serde_json::json!({ "channel_id": 1 })), None);
    }

    #[test]
    fn remembers_who_spoke_without_announcing_every_line() {
        let mut w = Watched::default();
        let speaker = |nick: &str, badge: Option<&str>| Speaker {
            nick: nick.into(),
            user_id: None,
            badge: badge.map(str::to_string),
        };

        assert!(w.heard("chan", speaker("first", None)), "a new name is worth announcing");
        assert!(!w.heard("chan", speaker("first", None)), "the same name saying more is not");
        assert!(w.heard("chan", speaker("first", Some("moderator"))), "but their badges changing is");
        assert!(w.heard("chan", speaker("second", None)));

        // Most recent first, so the newest arrival is at the top.
        let roster = w.roster("chan");
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0]["nick"], "second");
        assert_eq!(roster[1]["prefix"], "moderator");
    }

    #[test]
    fn forgets_the_oldest_rather_than_growing_without_end() {
        let mut w = Watched::default();
        for i in 0..(SPEAKERS_REMEMBERED + 50) {
            w.heard("chan", Speaker { nick: format!("nick{i}"), user_id: None, badge: None });
        }
        assert_eq!(w.roster("chan").len(), SPEAKERS_REMEMBERED);
        // The most recent survive; a list nobody can scan is no better than
        // an empty one, and the newest names are the ones being read.
        assert_eq!(w.roster("chan")[0]["nick"], format!("nick{}", SPEAKERS_REMEMBERED + 49));
    }

    #[test]
    fn marks_somebody_by_the_badge_worth_finding_them_by() {
        let badge = |kind: &str| api::Badge { kind: kind.into(), text: kind.into(), count: None };
        // Ranked by what a reader is scanning for, not by what is rarest: a
        // moderator who is also a subscriber shows as a moderator.
        assert_eq!(strongest_badge(&[badge("subscriber"), badge("moderator")]).as_deref(), Some("moderator"));
        assert_eq!(strongest_badge(&[badge("subscriber")]).as_deref(), Some("subscriber"));
        assert_eq!(strongest_badge(&[badge("broadcaster"), badge("moderator")]).as_deref(), Some("broadcaster"));
        assert_eq!(strongest_badge(&[]), None);
        // A badge nobody ranks marks nobody, rather than marking everybody.
        assert_eq!(strongest_badge(&[badge("verified")]), None);
    }

    #[test]
    fn closing_a_channel_forgets_who_was_talking_in_it() {
        let mut w = watching_odablock();
        w.heard("odablock", Speaker { nick: "someone".into(), user_id: None, badge: None });
        w.remove("odablock");
        assert!(w.roster("odablock").is_empty());
    }

    #[test]
    fn never_confuses_one_streamers_numbers_for_anothers() {
        // The bug two maps exist to prevent: nothing stops one streamer's
        // chatroom id from being another's channel id, and a single map keyed
        // by "some id" would deliver one's events into the other's buffer.
        let mut w = Watched::default();
        w.add(&api::Channel {
            id: 999, chatroom_id: 111, slug: "first".into(), username: "first".into(),
            avatar_url: None, subscribers_only: false, followers_only: false,
        });
        w.add(&api::Channel {
            id: 222, chatroom_id: 999, slug: "second".into(), username: "second".into(),
            avatar_url: None, subscribers_only: false, followers_only: false,
        });
        // 999 is first's channel and second's chatroom. Which one is meant is
        // never a guess - the subscription name says which kind it is.
        assert_eq!(channel_of(&w, &Some("channel.999".into()), &serde_json::json!({})).as_deref(), Some("first"));
        assert_eq!(channel_of(&w, &Some("chatrooms.999.v2".into()), &serde_json::json!({})).as_deref(), Some("second"));
    }

    #[test]
    fn closing_a_channel_cancels_every_subscription_it_had() {
        let mut w = watching_odablock();
        let mut names = w.remove("odablock");
        names.sort();
        assert_eq!(
            names,
            vec!["channel.2401072", "chatroom_2393554", "chatrooms.2393554", "chatrooms.2393554.v2"]
        );
        // And it is gone from both maps, so a straggling event has nowhere to
        // land rather than reopening the buffer that was just closed.
        assert_eq!(channel_of(&w, &Some("chatrooms.2393554".into()), &serde_json::json!({})), None);
        assert!(w.remove("odablock").is_empty());
    }

    #[test]
    fn reads_what_a_reply_was_answering() {
        // Copied from the wire. Kick sends the original's text alongside its
        // id, so a reply quotes properly however long ago the original was
        // said - unlike every other protocol here, which has to find it in
        // scrollback and comes up empty once it has scrolled past the cap.
        let metadata: ReplyMetadata = serde_json::from_value(serde_json::json!({
            "original_sender": { "id": 1150141, "username": "Greg" },
            "original_message": { "id": "ddeccb11-29bc-4cf1-ab2a-9b409293de76", "content": "L0L0WERFLGEWRUGHBER" }
        }))
        .unwrap();
        let preview = reply_preview(Some(&metadata)).unwrap();
        assert_eq!(preview.from, "Greg");
        assert_eq!(preview.id, "ddeccb11-29bc-4cf1-ab2a-9b409293de76");
        assert_eq!(preview.body, "L0L0WERFLGEWRUGHBER");
    }

    #[test]
    fn a_whole_reply_frame_off_the_wire_parses() {
        // The exact payload Kick sent, badges and all - not a hand-written
        // subset. The parse is the part that can silently stop working when
        // Kick adds a field, and the reply half is nested three deep inside a
        // message whose other half is decoration.
        // r##, not r#: the colour in the payload is `"#75FD46"`, and `"#`
        // closes a single-hash raw string right in the middle of the JSON.
        let raw = r##"{
          "id": "026bd0f5-085c-4571-962e-556371b63ded",
          "chatroom_id": 2393554,
          "content": "[emote:37226:KEKW]",
          "type": "reply",
          "created_at": "2026-09-02T00:16:25+00:00",
          "sender": { "id": 15383321, "username": "L3galizeWee", "slug": "l3galizewee",
            "identity": { "color": "#75FD46",
              "badges": [{ "type": "subscriber", "text": "Subscriber", "count": 11, "sort_order": 9 }],
              "badges_v2": [{ "name": "level", "badge_type": "global", "image_url": "https://ext.cdn.kick.com/x.png",
                "metadata": { "level": 47 }, "selected": true, "sort_order": 1 }] } },
          "metadata": {
            "original_sender": { "id": 1150141, "username": "Greg" },
            "original_message": { "id": "ddeccb11-29bc-4cf1-ab2a-9b409293de76", "content": "L0L0WERFLGEWRUGHBER" } }
        }"##;
        let msg: ChatMessage = serde_json::from_str(raw).expect("a real reply frame must parse");
        assert_eq!(msg.chatroom_id, 2393554);
        assert_eq!(msg.sender.username.as_deref(), Some("L3galizeWee"));
        let preview = reply_preview(msg.metadata.as_ref()).expect("and carry what it answered");
        assert_eq!(preview.from, "Greg");
        assert_eq!(preview.body, "L0L0WERFLGEWRUGHBER");
    }

    #[test]
    fn an_ordinary_messages_metadata_is_not_mistaken_for_a_reply() {
        // Plain messages carry metadata too - a `message_ref` and nothing
        // else - so "has metadata" is not the same question as "is a reply".
        let raw = r#"{ "id": "x", "chatroom_id": 1, "content": "hi", "type": "message",
          "sender": { "id": 2, "username": "someone" }, "metadata": { "message_ref": "1788305786864" } }"#;
        let msg: ChatMessage = serde_json::from_str(raw).unwrap();
        assert!(reply_preview(msg.metadata.as_ref()).is_none());
    }

    #[test]
    fn an_ordinary_message_is_not_a_reply() {
        assert!(reply_preview(None).is_none());
    }

    #[test]
    fn half_a_reply_is_no_reply() {
        // An empty quote above a message reads as something failing to load,
        // which is worse than showing the message plainly.
        let no_sender: ReplyMetadata =
            serde_json::from_value(serde_json::json!({ "original_message": { "id": "x", "content": "y" } })).unwrap();
        assert!(reply_preview(Some(&no_sender)).is_none());
        let no_message: ReplyMetadata =
            serde_json::from_value(serde_json::json!({ "original_sender": { "username": "Greg" } })).unwrap();
        assert!(reply_preview(Some(&no_message)).is_none());
    }

    #[test]
    fn reads_a_redemption() {
        let p = serde_json::json!({ "username": "GayHayride", "reward_title": "CAT PATS", "user_input": "" });
        assert_eq!(
            describe_action("App\\Events\\RewardRedeemedEvent", &p).as_deref(),
            Some("GayHayride redeemed CAT PATS")
        );
    }

    #[test]
    fn keeps_what_they_typed_with_it() {
        // The half that says what was actually asked for, where the reward
        // takes input at all.
        let p = serde_json::json!({ "username": "Hecklephish", "reward_title": "CAT TREATS", "user_input": "for Mittens" });
        assert_eq!(
            describe_action("App\\Events\\RewardRedeemedEvent", &p).as_deref(),
            Some("Hecklephish redeemed CAT TREATS: for Mittens")
        );
    }

    #[test]
    fn reads_subscriptions_and_gifts_and_raids() {
        let subbed = serde_json::json!({ "username": "someone", "months": 1 });
        assert_eq!(describe_action("SubscriptionEvent", &subbed).as_deref(), Some("someone subscribed"));

        let resub = serde_json::json!({ "username": "someone", "months": 7 });
        assert_eq!(describe_action("SubscriptionEvent", &resub).as_deref(), Some("someone subscribed - 7 months"));

        let gifts = serde_json::json!({ "gifter_username": "big", "gifted_usernames": ["a", "b", "c"] });
        assert_eq!(describe_action("GiftedSubscriptionsEvent", &gifts).as_deref(), Some("big gifted 3 subscriptions"));

        let raid = serde_json::json!({ "host_username": "friend", "number_viewers": 42 });
        assert_eq!(describe_action("StreamHostEvent", &raid).as_deref(), Some("friend raided with 42 viewers"));
    }

    #[test]
    fn takes_the_name_from_wherever_the_event_put_it() {
        // These events disagree about where the person goes, and an event
        // whose shape has moved on should be skipped rather than reported with
        // a hole in it.
        let nested = serde_json::json!({ "sender": { "username": "nested" }, "reward_title": "X" });
        assert_eq!(describe_action("RewardRedeemedEvent", &nested).as_deref(), Some("nested redeemed X"));

        let nameless = serde_json::json!({ "reward_title": "X" });
        assert_eq!(describe_action("RewardRedeemedEvent", &nameless), None);
    }

    #[test]
    fn ignores_events_that_are_not_somebody_doing_something() {
        let p = serde_json::json!({ "username": "someone" });
        // These two are handled in the frame loop rather than here - one
        // removes a message and the other reports a chat mode - so this is
        // still the right answer for both.
        assert_eq!(describe_action("App\\Events\\MessageDeletedEvent", &p), None);
        assert_eq!(describe_action("App\\Events\\ChatroomUpdatedEvent", &p), None);
    }

    #[test]
    fn one_subscription_is_reported_once() {
        // Measured on a live subathon: subscribing once produces all three of
        // these, on three different subscriptions. Only the first says
        // anything the other two do not, so only the first is believed - and
        // the other two are named rather than left to a suffix test, which is
        // what reported the same person subscribing twice.
        let sub = serde_json::json!({ "chatroom_id": 2393554, "username": "doni433", "months": 1 });
        assert_eq!(
            describe_action("App\\Events\\SubscriptionEvent", &sub).as_deref(),
            Some("doni433 subscribed")
        );
        let same = serde_json::json!({ "user_ids": [584331], "username": "doni433", "channel_id": 2401072 });
        assert_eq!(describe_action("App\\Events\\ChannelSubscriptionEvent", &same), None);
        let info = serde_json::json!({ "message": { "action": "subscribe" }, "user": { "username": "doni433" } });
        assert_eq!(describe_action("App\\Events\\ChatMessageSentEvent", &info), None);
    }

    #[test]
    fn tells_a_timeout_from_a_ban() {
        let timeout = serde_json::json!({
            "user": { "username": "someone" },
            "banned_by": { "username": "amod" },
            "expires_at": "2026-09-02T04:00:00Z"
        });
        assert_eq!(
            describe_action("App\\Events\\UserBannedEvent", &timeout).as_deref(),
            Some("someone was timed out by amod")
        );

        // No expiry is a ban, which is the difference worth reporting.
        let ban = serde_json::json!({ "user": { "username": "someone" }, "banned_by": { "username": "amod" } });
        assert_eq!(
            describe_action("App\\Events\\UserBannedEvent", &ban).as_deref(),
            Some("someone was banned by amod")
        );

        // A moderator who did not name themselves still produces a line.
        let anonymous = serde_json::json!({ "user": { "username": "someone" } });
        assert_eq!(describe_action("UserBannedEvent", &anonymous).as_deref(), Some("someone was banned"));

        assert_eq!(
            describe_action("App\\Events\\UserUnbannedEvent", &serde_json::json!({ "user": { "username": "someone" } })).as_deref(),
            Some("someone is allowed back")
        );
    }

    #[test]
    fn reads_a_real_gifted_subscription_payload() {
        // Copied from the wire, prefix and all - this one arrives with no
        // `App\Events\` on it at all, which is why the name is taken from the
        // last segment rather than assumed to have one.
        let p = serde_json::json!({
            "chatroom_id": 2393554,
            "gifted_usernames": ["Interstelar", "Ron_OSRS", "Auzzzii", "MeowtimeJay", "stradivarius31"],
            "gifter_username": "ItsDez",
            "gifted_total": 5
        });
        assert_eq!(
            describe_action("GiftedSubscriptionsEvent", &p).as_deref(),
            Some("ItsDez gifted 5 subscriptions")
        );
    }

    #[test]
    fn reads_the_id_out_of_a_subscription_name() {
        assert_eq!(id_in_subscription("chatrooms.123.v2"), Some(Subscribed::Chatroom(123)));
        assert_eq!(id_in_subscription("chatrooms.123"), Some(Subscribed::Chatroom(123)));
        assert_eq!(id_in_subscription("chatroom_123"), Some(Subscribed::Chatroom(123)));
        assert_eq!(id_in_subscription("channel.123"), Some(Subscribed::Channel(123)));
        assert_eq!(id_in_subscription("chatrooms.abc"), None);
        assert_eq!(id_in_subscription("something-else"), None);
    }
}
