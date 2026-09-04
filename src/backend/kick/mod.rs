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
    /// Try a channel Kick refused for the rate limit again, counting the
    /// attempts so this cannot become a connection that asks forever.
    Rejoin(String, u8),
    /// Stop watching one.
    Part(String),
}

/// Who somebody is, on Kick.
///
/// Kick's own moderation popup asks one endpoint for this, and it answers the
/// two things that matter in a stream chat: how long they have followed, and
/// what they have earned here. Badges are what the chat itself already shows
/// beside their name, so the profile agrees with the line above it.
pub async fn profile(state: &AppState, account_id: &str, buffer_id: &str, username: &str) -> serde_json::Value {
    let mut profile = crate::profile::pending("kick", account_id, username);
    profile["pending"] = serde_json::json!(false);

    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return profile };
    let slug = api::normalise_slug(&buffer.name);
    let token = state.accounts.get_kick(account_id).and_then(|c| c.token);
    let Ok(http) = api::client() else { return profile };

    let Ok(body) = api::channel_user(&http, token.as_deref(), &slug, username).await else { return profile };

    if let Some(id) = body["id"].as_i64() {
        profile["id"] = serde_json::json!(id.to_string());
    }
    if let Some(name) = body["username"].as_str() {
        profile["name"] = serde_json::json!(name);
    }
    if let Some(picture) = body["profile_pic"].as_str().filter(|p| !p.is_empty()) {
        profile["avatarUrl"] = serde_json::json!(picture);
    }
    // "Following since" is Kick's own phrase and its own date format.
    if let Some(since) = body["following_since"].as_str() {
        crate::profile::note(&mut profile, "Following since", since.split('T').next().unwrap_or(since));
    }
    if body["banned"].is_object() {
        crate::profile::note(&mut profile, "Banned", "yes, in this channel");
    }

    let badges: Vec<String> = body["badges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str().or_else(|| b["type"].as_str()).map(str::to_string))
        .collect();
    let moderator = body["badges"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|b| matches!(b["type"].as_str(), Some("moderator") | Some("broadcaster")));
    if !badges.is_empty() {
        profile["roles"] = serde_json::json!(badges);
    }
    profile["isModerator"] = serde_json::json!(moderator);
    profile
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

/// How many channels to resolve at once when connecting.
///
/// How many times a rate-limited channel is asked for again before giving up.
const REJOIN_ATTEMPTS: u8 = 3;

/// Asks for a refused channel again, later, and further out each time.
///
/// Each attempt is its own task with its own wait, so thirty refusals do not
/// become thirty simultaneous retries - which is the burst that caused them.
fn retry_later(state: &AppState, account_id: &str, slug: String, attempt: u8) {
    let Some(sender) = state.runtime.kick_sender(account_id) else { return };
    tokio::spawn(async move {
        // Well outside whatever window was exhausted, and staggered by the
        // channel's own name so they do not all come back at once.
        let stagger = (slug.len() % 7) as u64;
        tokio::time::sleep(Duration::from_secs(20 * attempt as u64 + stagger)).await;
        let _ = sender.send(Command::Rejoin(slug, attempt));
    });
}

/// Measured, not guessed: eight at a time drew 429s from Kick on an account
/// watching thirty channels, and the channels that were refused went missing
/// from the list entirely. Four, with the retry in `api::channel` behind it,
/// connects the same thirty in a few seconds and asks nothing twice.
const CONNECT_CONCURRENCY: usize = 3;

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
                        state.runtime.report_progress(state, account_id, &format!("opened {added} channels you follow on Kick"));
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

    // Several at a time rather than one after another.
    //
    // Each channel needs its own `/channels/<handle>` before it can be
    // subscribed to - the chatroom and channel ids the Pusher subscription is
    // addressed with are there and nowhere else, so the bulk follow lists
    // cannot stand in for it. But nothing about them has to happen in order:
    // done one at a time, an account following thirty channels spent half a
    // minute watching its own list appear a row at a time.
    //
    // Subscribing stays serial, because the socket is one thing and only one
    // subscription can be written to it at a time. It is the waiting that is
    // parallel, not the writing.
    let queue: Vec<_> = channels
        .iter()
        .cloned()
        .map(|slug| {
            let http = &http;
            async move {
                let prepared = prepare(state, http, config, account_id, &slug).await;
                (slug, prepared)
            }
        })
        .collect();
    let mut prepares = futures::stream::iter(queue).buffer_unordered(CONNECT_CONCURRENCY);

    let mut refused = Vec::new();
    while let Some((slug, prepared)) = prepares.next().await {
        match prepared {
            Ok(channel) => {
                subscribe(&mut socket, channel.chatroom_id, channel.id).await?;
                watched.add(&channel);
            }
            // One bad handle in a saved list must not stop the other twenty
            // from connecting - a channel can be renamed or banned between
            // sessions, and that is this account's problem with one buffer
            // rather than with Kick.
            Err(e) => {
                let words = format!("{e:#}");
                tracing::warn!("kick[{account_id}]: {slug}: {words}");
                // Being told to slow down is not the same as being told no.
                // A channel refused for the rate limit is asked for again in
                // a moment; without that it is simply missing from the list
                // until the client is restarted, which is how an account
                // watching thirty channels loses eight of them.
                if words.contains("429") {
                    refused.push(slug);
                } else {
                    state.runtime.report_progress(state, account_id, &format!("{slug}: {words}"));
                }
            }
        }
    }
    drop(prepares);

    if !refused.is_empty() {
        state.runtime.report_progress(
            state,
            account_id,
            &format!("Kick asked us to slow down - retrying {} channels", refused.len()),
        );
        for slug in refused {
            retry_later(state, account_id, slug, 1);
        }
    }

    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);

    // Pusher closes a socket it has not heard from. Its own timeout arrives in
    // the handshake, but a fixed interval well inside the shortest it ever
    // sends is simpler than tracking it and cannot be wrong in the direction
    // that matters.
    let mut ping = tokio::time::interval(Duration::from_secs(60));
    ping.tick().await;

    // Who is live, and how many people are watching. Kick pushes the start and
    // the stop of a stream but never the count, so the only way to keep the
    // channel list honest is to ask - once for every followed channel at a
    // time, which is what Kick's own Following panel does.
    let live_token = config.token.clone().filter(|t| !t.is_empty());
    let mut live_poll = tokio::time::interval(LIVE_POLL);
    live_poll.tick().await;

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
                Some(Command::Rejoin(handle, attempt)) => {
                    match prepare(state, &http, config, account_id, &handle).await {
                        Ok(channel) => {
                            subscribe(&mut socket, channel.chatroom_id, channel.id).await?;
                            watched.add(&channel);
                        }
                        Err(e) => {
                            let words = format!("{e:#}");
                            // Still being told to slow down: wait longer and
                            // ask again, a few times, rather than leaving a
                            // channel silently missing from the list for the
                            // rest of the session.
                            if words.contains("429") && attempt < REJOIN_ATTEMPTS {
                                retry_later(state, account_id, handle, attempt + 1);
                            } else {
                                state.runtime.report_progress(state, account_id, &format!("{handle}: {words}"));
                            }
                        }
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
            _ = live_poll.tick(), if live_token.is_some() => {
                // Not awaited in the loop: the socket has a ping to send and
                // frames to read, and neither should wait on an HTTP round
                // trip to somebody else's API.
                let (state, http) = (state.clone(), http.clone());
                let (account_id, token) = (account_id.to_string(), live_token.clone().unwrap_or_default());
                tokio::spawn(async move { refresh_followed_live(&state, &http, &account_id, &token).await });
            }
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

    // What is on air. A Kick channel is a stream as much as a chat, and moho
    // showed only the chat - so the title, the game and how many people are
    // watching, which is most of what a viewer wants to know, were nowhere.
    // Follow state is not asked for here: this runs for every channel at
    // connect, and that would be one extra request per channel before the
    // first message arrives. It is filled in when the channel is opened.
    announce_stream(state, &buffer.id, &channel, None);

    // The streamer's own picture on the channel. Parsed since the first
    // version of this backend and used by nothing, so every Kick channel drew
    // as a bare "#" among rows that all had faces.
    if let Some(avatar) = channel.avatar_url.as_deref().filter(|a| !a.is_empty()) {
        state.runtime.set_buffer_avatar(state, &buffer.id, avatar);
    }

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
        // Predictions are broadcast nowhere near the chat: Kick's own viewer
        // panel listens on this one, and this client heard none of them
        // because it never asked. Named with hyphens rather than dots, which
        // is a detail worth spelling out - the parser that reads a channel
        // id back out of a subscription has to know both shapes.
        format!("predictions-channel-{channel_id}"),
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

    if frame.event == "pusher:ping" {
        socket.send(WsMessage::Text(r#"{"event":"pusher:pong","data":{}}"#.to_string())).await?;
        return Ok(());
    }
    handle_event(state, account_id, watched, &frame.event, frame.channel.as_deref(), payload)
}

/// One Kick event, once it has been unwrapped from Pusher's envelope.
///
/// Split from handle_frame so it can be driven with a payload rather than a
/// socket. Everything a stream does that is not somebody talking - polls,
/// predictions, redemptions, raids, moderation - arrives here, and none of it
/// can be produced on demand from a real channel: a poll happens when a
/// streamer decides to run one. Being able to hand this a payload is the only
/// way any of it is testable at all.
fn handle_event(
    state: &AppState,
    account_id: &str,
    watched: &mut Watched,
    event: &str,
    channel: Option<&str>,
    payload: serde_json::Value,
) -> Result<()> {
    // Rebuilt so the arms below can keep reading `frame.channel`, which is
    // how a channel is identified for every event that does not carry an id
    // of its own.
    let frame = PusherFrame {
        event: event.to_string(),
        channel: channel.map(str::to_string),
        data: serde_json::Value::Null,
    };
    match event {
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
        e if e.ends_with("StreamerIsLive") || e.ends_with("StopStreamBroadcast") => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            let live = e.ends_with("StreamerIsLive");
            system_line(state, account_id, &slug, if live { "went live" } else { "ended the stream" }, "stream");
            // And the header, which is otherwise still describing the stream
            // that just ended. Asked of Kick rather than assembled from this
            // event: the title and the game arrive with the channel, not with
            // the notice that it went live.
            let (state, buffer_id) = (state.clone(), crate::model::buffer_id(account_id, &slug));
            // Now, not when the throttle next allows it: this is the moment
            // the answer changed.
            tokio::spawn(async move { refresh_stream_now(&state, &buffer_id).await });
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

        // A poll running on stream. Announced rather than made interactive:
        // voting goes through the player, and a chat client showing the
        // question and the options is the part somebody reading chat is
        // missing - a running poll is otherwise invisible here.
        //
        // One line per poll, rewritten as the votes come in. Kick sends this
        // event on every vote, so recording each one put a near-identical
        // line in the chat several times a second and buried the conversation
        // the poll is about.
        e if e.ends_with("PollUpdateEvent") => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            if let Some(line) = describe_poll(&payload) {
                live_line(state, account_id, &slug, &poll_message_id(&slug, &payload), &line, "poll");
            }
            // And the poll itself, for the card that can be voted in. The
            // line above is the record of what was asked; this is the thing
            // on screen while it is still being asked.
            if let Ok(poll) = serde_json::from_value::<api::Poll>(payload["poll"].clone()) {
                announce_poll(state, &crate::model::buffer_id(account_id, &slug), Some(&poll));
            }
        }

        // The poll is over. The line stays - what was asked and how it went is
        // worth keeping in the log - but stops claiming to be running.
        e if e.ends_with("PollDeleteEvent") => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            let buffer_id = crate::model::buffer_id(account_id, &slug);
            let id = poll_message_id(&slug, &payload);
            if let Ok(Some(stored)) = state.store.get_message(&buffer_id, &id) {
                let ended = stored.body.replacen("poll:", "poll ended:", 1);
                state.runtime.update_message(state, &buffer_id, &id, &ended, &[], &[]);
            }
            announce_poll(state, &buffer_id, None);
        }

        // A prediction: the same shape as a poll, with money on it. Kick's
        // payload for these is not documented and this client has never seen
        // one, so the parser is deliberately tolerant - it reads a title and
        // a set of named outcomes wherever they sit, and shows nothing at all
        // rather than something wrong if the shape is not what it expects.
        e if e.contains("Prediction") => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            let buffer_id = crate::model::buffer_id(account_id, &slug);
            if e.ends_with("PredictionDeleteEvent") || e.ends_with("PredictionDeletedEvent") {
                let id = prediction_message_id(&slug, &payload);
                if let Ok(Some(stored)) = state.store.get_message(&buffer_id, &id) {
                    let ended = stored.body.replacen("prediction:", "prediction closed:", 1);
                    state.runtime.update_message(state, &buffer_id, &id, &ended, &[], &[]);
                }
                announce_prediction_gone(state, &buffer_id);
            } else {
                if let Some(line) = describe_prediction(&payload) {
                    live_line(state, account_id, &slug, &prediction_message_id(&slug, &payload), &line, "poll");
                }
                // And the card, which is the same card a poll gets: the two
                // are one question with a set of answers, and the money is a
                // column rather than a different idea.
                announce_prediction(state, &buffer_id, &payload);
            }
        }

        // A moderator emptying the room.
        e if e.ends_with("ChatroomClearEvent") => {
            if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                system_line(state, account_id, &slug, "a moderator cleared the chat", "moderation");
            }
        }

        // The things people do rather than say.
        e => {
            if let Some((kind, line)) = describe_action(e, &payload) {
                if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                    system_line(state, account_id, &slug, &line, kind);
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

/// Tells the client what is on air in a channel, and remembers it so a window
/// opening later can be told the same thing.
///
/// Kick's own channel endpoint carries all of it and this backend was already
/// calling it - the numbers were parsed away and dropped.
fn announce_stream(state: &AppState, buffer_id: &str, channel: &api::Channel, following: Option<bool>) {
    let stream = serde_json::json!({
        "bufferId": buffer_id,
        "live": channel.live.is_some(),
        "title": channel.live.as_ref().map(|l| l.title.clone()),
        "category": channel.live.as_ref().and_then(|l| l.category.clone()),
        "viewers": channel.live.as_ref().and_then(|l| l.viewers),
        "startedTs": channel.live.as_ref().and_then(|l| l.started_ts),
        "followers": channel.followers,
        // Absent rather than false for an account that is not signed in:
        // "not following" and "cannot say" are different, and a button that
        // offers to follow when it cannot is a button that fails when pressed.
        "following": following,
    });
    state.runtime.set_kick_stream(buffer_id, stream.clone());
    state.events.emit("kickStream", stream);
}

/// The poll in a channel, or its absence, in the shape the card reads.
///
/// Absence is a message rather than silence: a poll that has been taken down
/// has to leave the screen, and a client that only ever heard about polls
/// starting would keep showing one that ended an hour ago.
pub fn announce_poll(state: &AppState, buffer_id: &str, poll: Option<&api::Poll>) {
    let card = poll.map(|poll| {
        serde_json::json!({
            "bufferId": buffer_id,
            "kind": "poll",
            "id": card_id(state, buffer_id, "poll", &poll.title, poll.duration, poll.remaining),
            "title": poll.title,
            "options": poll.options.iter().map(|o| serde_json::json!({
                "id": o.id,
                "label": o.label,
                "votes": o.votes,
            })).collect::<Vec<_>>(),
            "duration": poll.duration,
            // Seconds left when this was written. The client counts down from
            // it rather than asking Kick every second.
            "remaining": poll.remaining,
            "resultDisplayDuration": poll.result_display_duration,
            "hasVoted": poll.has_voted,
            "votedOptionId": poll.voted_option_id,
        })
    });
    publish_card(state, buffer_id, "poll", card);
}

/// Says the prediction is over, so the card stops offering to back it.
pub fn announce_prediction_gone(state: &AppState, buffer_id: &str) {
    publish_card(state, buffer_id, "prediction", None);
}

/// The same for a prediction, which is a poll with money on it.
pub fn announce_prediction(state: &AppState, buffer_id: &str, payload: &serde_json::Value) {
    // Kick sends the whole prediction on its own broadcast channel, in the
    // same shape its REST endpoints answer with - so one parser reads both.
    let found = serde_json::from_value::<api::Prediction>(payload.get("prediction").unwrap_or(payload).clone());
    let prediction = match found {
        Ok(prediction) => prediction,
        Err(e) => {
            tracing::debug!("kick: prediction payload not understood: {e}");
            return;
        }
    };

    // The broadcast carries the prediction and nothing about you: not your
    // bet, not your points. Both were known a moment ago if this is the same
    // prediction moving, so they are carried across rather than blanked -
    // otherwise somebody else's bet would wipe yours off the card.
    let known = state.runtime.live_card(buffer_id, "prediction");
    let same = known
        .as_ref()
        .and_then(|card| card["id"].as_str().map(|id| id == format!("prediction:{}", prediction.id)))
        .unwrap_or(false);
    let mut vote = None;
    let mut points = None;
    if same {
        if let Some(card) = known.as_ref() {
            vote = card["votedOptionId"].as_str().map(|outcome| api::PredictionVote {
                outcome_id: outcome.to_string(),
                total_vote_amount: card["stake"].as_f64().unwrap_or_default(),
            });
            points = card["balance"].as_i64();
        }
    }
    announce_prediction_card(state, buffer_id, &prediction, vote.as_ref(), points);

    // A prediction this client has not seen before: ask once for the two
    // things the broadcast cannot say. Once per prediction rather than once
    // per bet, which is the difference between a request and a flood.
    // Through the handle rather than `tokio::spawn`, because this is called
    // from a plain function that the tests drive with no runtime under it -
    // and a card that draws correctly in a test is worth more than a panic
    // proving there was nowhere to send the request.
    if !same {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (state, buffer_id) = (state.clone(), buffer_id.to_string());
            runtime.spawn(async move { refresh_prediction(&state, &buffer_id).await });
        }
    }
}

/// A prediction in the shape the card reads.
///
/// The two things the card cannot work out for itself are carried alongside:
/// what this account has riding on it, and what it has left to bet with.
pub fn announce_prediction_card(
    state: &AppState,
    buffer_id: &str,
    prediction: &api::Prediction,
    vote: Option<&api::PredictionVote>,
    points: Option<i64>,
) {
    publish_card(state, buffer_id, "prediction", Some(prediction_card(prediction, vote, points)));
}

/// The card, from Kick's own prediction object.
fn prediction_card(
    prediction: &api::Prediction,
    vote: Option<&api::PredictionVote>,
    points: Option<i64>,
) -> serde_json::Value {
    let open = prediction.state.eq_ignore_ascii_case("ACTIVE");
    let started = prediction.created_at.as_deref().and_then(api::parse_kick_datetime);
    // How long is left, from when it started rather than from when this
    // arrived: a prediction event carries no clock of its own.
    let remaining = match (open, started) {
        (true, Some(started)) => {
            let gone = now_secs().saturating_sub(started);
            (prediction.duration as i64 - gone).max(0) as u32
        }
        (true, None) => prediction.duration,
        // Locked, resolved or cancelled: the betting is over whatever the
        // clock says.
        _ => 0,
    };
    let staked: f64 = prediction.outcomes.iter().map(|o| o.total_vote_amount).sum();
    let options: Vec<serde_json::Value> = prediction
        .outcomes
        .iter()
        .map(|outcome| {
            serde_json::json!({
                "id": outcome.id,
                "label": outcome.title,
                // Points where a poll counts votes: the same bar, measuring
                // the thing this service measures.
                "votes": outcome.total_vote_amount,
                "backers": outcome.vote_count,
                // Kick's own page writes the rate this way rather than
                // sending it as a phrase.
                "odds": (outcome.return_rate > 0.0).then(|| format!("1:{:.1}", outcome.return_rate)),
                "winner": prediction.winning_outcome_id.as_deref() == Some(outcome.id.as_str()),
            })
        })
        .collect();

    // What this bet would come back as, at the rate the outcome is paying
    // now. Kick shows the same number and marks it as an estimate while the
    // betting is open, because every later bet moves it.
    let your_return = vote.and_then(|vote| {
        prediction
            .outcomes
            .iter()
            .find(|o| o.id == vote.outcome_id)
            .map(|o| vote.total_vote_amount * o.return_rate)
    });

    serde_json::json!({
        "kind": "prediction",
        // Kick names these, so the card takes its name rather than inventing
        // one from the clock the way a poll has to.
        "id": format!("prediction:{}", prediction.id),
        "title": prediction.title,
        "options": options,
        "duration": prediction.duration,
        "remaining": remaining,
        "resultDisplayDuration": 0,
        "hasVoted": vote.is_some(),
        "votedOptionId": vote.map(|v| v.outcome_id.clone()),
        "stake": vote.map(|v| v.total_vote_amount),
        "total": staked,
        "yourReturn": your_return,
        "state": prediction.state,
        "balance": points,
        "minBet": api::MIN_PREDICTION_BET,
    })
}

/// Reads the prediction a channel has going, for somebody opening it.
///
/// The events say when one starts and changes; this is how a window that
/// arrives mid-way learns there is one at all - and the only place the two
/// things the card cannot compute come from: this account's own bet, and the
/// points it has left.
pub async fn refresh_prediction(state: &AppState, buffer_id: &str) {
    let Some(channel) = channel_when_ready(state, buffer_id).await else { return };
    let Ok(http) = api::client() else { return };
    let token = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| state.accounts.get_kick(&b.account_id))
        .and_then(|c| c.token)
        .filter(|t| !t.is_empty());
    match api::prediction_latest(&http, token.as_deref(), &channel.slug).await {
        Ok(Some((prediction, vote))) => {
            let points = match token.as_deref() {
                Some(token) => api::points(&http, token, &channel.slug).await.ok(),
                None => None,
            };
            announce_prediction_card(state, buffer_id, &prediction, vote.as_ref(), points);
        }
        // Nothing running, and nothing to show.
        Ok(None) => publish_card(state, buffer_id, "prediction", None),
        Err(e) => tracing::debug!("kick: reading {}'s prediction: {e:#}", channel.slug),
    }
}

/// Stores a card, writes it down, and tells the client - or says it is gone.
fn publish_card(state: &AppState, buffer_id: &str, kind: &str, card: Option<serde_json::Value>) {
    // When this was true, stamped here so every card carries one.
    //
    // The clock is the whole point: `remaining` is a number of seconds that
    // was accurate at one moment, and a client told only the number counts
    // down from whenever it happened to hear it. Replay a stored card into a
    // window opened an hour later and the poll appears to have its full time
    // left - which is exactly what a poll timer must never do.
    let card = card.map(|mut card| {
        card["asOf"] = serde_json::json!(now_secs());
        card
    });
    match &card {
        None => state.runtime.forget_live_card(buffer_id, kind),
        Some(card) => {
            state.runtime.set_live_card(buffer_id, kind, card.clone());
            // Written down as it changes, so what is read back later is how
            // it finished rather than how it opened.
            let id = card["id"].as_str().unwrap_or_default().to_string();
            let title = card["title"].as_str().unwrap_or_default().to_string();
            let ts = now_secs();
            if let Err(e) = state.store.record_live_card(buffer_id, kind, &id, &title, &card.to_string(), ts) {
                tracing::debug!("kick: keeping the {kind}: {e:#}");
            }
        }
    }
    state.events.emit("pollCard", serde_json::json!({
        "bufferId": buffer_id,
        "kind": kind,
        "poll": card.unwrap_or(serde_json::Value::Null),
    }));
}

/// Whether a stored card still has anything to say.
///
/// Its own function because two places ask: the replay into a window that has
/// just opened, and the tests. A card is current while its clock is running
/// and for as long as the service leaves the result up afterwards.
pub fn card_is_current(card: &serde_json::Value) -> bool {
    let seconds = |key: &str| card[key].as_i64().unwrap_or(0);
    let as_of = seconds("asOf");
    if as_of == 0 {
        // Stamped by every card this backend makes; anything without one is
        // from a version that did not, and its clock cannot be trusted.
        return false;
    }
    let alive = seconds("remaining") + seconds("resultDisplayDuration");
    now_secs() - as_of <= alive
}

/// The channel behind a conversation, waiting a little for it to arrive.
///
/// A window restores the conversation it had open before its account has
/// finished connecting - which for Kick means before the channel's ids are
/// known - so asking straight away is asking too early. Every caller here is
/// already running in the background, and the alternative to waiting is a
/// poll or a prediction that is running right now and shows up nowhere until
/// something else changes.
async fn channel_when_ready(state: &AppState, buffer_id: &str) -> Option<crate::runtime::KickChannel> {
    for wait in [0, 3, 8, 20] {
        if wait > 0 {
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
        if let Some(channel) = state.runtime.kick_channel(buffer_id) {
            return Some(channel);
        }
    }
    None
}

/// The wall clock, in seconds, as everything here writes it down.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// What identifies one poll or prediction for as long as it runs.
///
/// Kick names neither of them, so the moment it started does: the same card
/// keeps its id while the votes come in, and the next one - even with the
/// same question - gets its own. The id already in flight wins where the
/// title matches, since a second of drift in `remaining` must not split one
/// poll into two.
fn card_id(state: &AppState, buffer_id: &str, kind: &str, title: &str, duration: u32, remaining: u32) -> String {
    if let Some(open) = state.runtime.live_card(buffer_id, kind) {
        if open["title"].as_str() == Some(title) {
            if let Some(id) = open["id"].as_str() {
                return id.to_string();
            }
        }
    }
    format!("{kind}:{}", now_secs() - (duration.saturating_sub(remaining)) as i64)
}

/// Reads the poll a channel has running, for somebody who has just opened it./// Reads the poll a channel has running, for somebody who has just opened it.
///
/// A poll that started before you arrived is the common case - they run for a
/// minute and a chat is opened at any moment in it - and the events only tell
/// you about the ones that change while you are watching.
pub async fn refresh_poll(state: &AppState, buffer_id: &str) {
    let Some(channel) = channel_when_ready(state, buffer_id).await else { return };
    let Ok(http) = api::client() else { return };
    let token = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| state.accounts.get_kick(&b.account_id))
        .and_then(|c| c.token)
        .filter(|t| !t.is_empty());
    match api::poll(&http, token.as_deref(), &channel.slug).await {
        Ok(poll) => announce_poll(state, buffer_id, poll.as_ref()),
        Err(e) => tracing::debug!("kick: reading {}'s poll: {e:#}", channel.slug),
    }
}

/// Whether this account follows the channel, where it can say.
async fn following_now(state: &AppState, http: &reqwest::Client, account_id: &str, slug: &str) -> Option<bool> {
    let token = state.accounts.get_kick(account_id)?.token.filter(|t| !t.is_empty())?;
    api::standing(http, &token, slug).await.ok().map(|s| s.following)
}

/// Re-reads whether this account follows a channel and says so.
///
/// Its own function because following is the one thing here a person changes
/// from this client - the rest of what a channel says about itself changes
/// because the streamer did something.
pub async fn refresh_standing(state: &AppState, buffer_id: &str) {
    let Some(channel) = state.runtime.kick_channel(buffer_id) else { return };
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return };
    let Ok(http) = api::client() else { return };
    let following = following_now(state, &http, &buffer.account_id, &channel.slug).await;
    if let Some(mut stream) = state.runtime.kick_stream(buffer_id) {
        stream["following"] = serde_json::json!(following);
        state.runtime.set_kick_stream(buffer_id, stream.clone());
        state.events.emit("kickStream", stream);
    }
}

/// How often the followed channels are asked about as a group.
const LIVE_POLL: Duration = Duration::from_secs(60);

/// How recently a channel must have been asked about directly for a second ask
/// to be pointless.
///
/// Opening a client with thirty Kick channels subscribes to thirty buffers at
/// once, and each subscription used to become its own channel fetch; the poll
/// above covers all of them for the price of one request.
const DIRECT_REFRESH: Duration = Duration::from_secs(45);

/// Live state for everything this account follows, in one request.
///
/// Only channels this client actually has open are updated: the follow list
/// can be longer than the buffers, and a stream state for a buffer that does
/// not exist has nobody to tell.
async fn refresh_followed_live(state: &AppState, http: &reqwest::Client, account_id: &str, token: &str) {
    let rows = match api::followed_live(http, token).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("kick[{account_id}]: polling live channels: {e:#}");
            return;
        }
    };
    for row in rows {
        let buffer_id = crate::model::buffer_id(account_id, &row.slug);
        // A channel with no chat connection is one this client does not have
        // open, whatever the follow list says.
        if state.runtime.kick_channel(&buffer_id).is_none() {
            continue;
        }
        let mut stream = state.runtime.kick_stream(&buffer_id).unwrap_or_else(|| {
            serde_json::json!({ "bufferId": buffer_id, "live": false })
        });
        let before = stream.clone();
        stream["live"] = serde_json::json!(row.live);
        stream["viewers"] = serde_json::json!(row.viewers);
        if row.live {
            // The full follow list carries no session title, so a title
            // already known from the channel itself is better than none.
            if let Some(title) = row.title {
                stream["title"] = serde_json::json!(title);
            }
            if let Some(category) = row.category {
                stream["category"] = serde_json::json!(category);
            }
        } else {
            stream["title"] = serde_json::Value::Null;
            stream["category"] = serde_json::Value::Null;
            stream["startedTs"] = serde_json::Value::Null;
        }
        // This list says nothing about whether the account follows the
        // channel - it is the follow list, so it does, and anything already
        // known stays as it is.
        if stream["following"].is_null() {
            stream["following"] = serde_json::json!(true);
        }
        if stream != before {
            state.runtime.set_kick_stream(&buffer_id, stream.clone());
            state.events.emit("kickStream", stream);
        }
    }
}

/// Asks Kick what a channel is doing now and says so.
///
/// Used when the answer has just changed - a stream starting or ending - and
/// when somebody opens the channel, since a viewer count from an hour ago is
/// worse than none.
pub async fn refresh_stream(state: &AppState, buffer_id: &str) {
    if !state.runtime.kick_stream_due(buffer_id, DIRECT_REFRESH) {
        return;
    }
    refresh_stream_now(state, buffer_id).await
}

/// The same, for a caller that has already decided the answer is stale - a
/// stream going on or off air, or the channel somebody is looking at.
pub async fn refresh_stream_now(state: &AppState, buffer_id: &str) {
    let Some(channel) = state.runtime.kick_channel(buffer_id) else { return };
    let Ok(http) = api::client() else { return };
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    match api::channel(&http, &channel.slug).await {
        Ok(fresh) => {
            let following = following_now(state, &http, &account_id, &channel.slug).await;
            announce_stream(state, buffer_id, &fresh, following);
        }
        Err(e) => tracing::debug!("kick: refreshing {}: {e:#}", channel.slug),
    }
}

/// One line that keeps being rewritten rather than repeated.
///
/// A poll or a prediction is one thing happening over a minute or two, not a
/// stream of events - so it gets one message that changes, the way an edited
/// message does, and the chat around it stays readable.
fn live_line(state: &AppState, account_id: &str, slug: &str, msg_id: &str, what: &str, kind: &str) {
    let buffer_id = crate::model::buffer_id(account_id, slug);
    if state.runtime.update_message(state, &buffer_id, msg_id, what, &[], &[]) {
        return;
    }
    state.runtime.record_message(
        state, account_id, slug, "channel", slug, what, false, kind, None, Some(msg_id.to_string()),
        false, None, Vec::new(), Vec::new(), None,
    );
}

/// The id the line for this poll keeps, so later updates find it.
///
/// Kick's own poll id where there is one; the channel otherwise, since a
/// channel runs one poll at a time and a stable-but-approximate id is better
/// than a fresh line per vote.
fn poll_message_id(slug: &str, payload: &serde_json::Value) -> String {
    let poll = payload.get("poll").unwrap_or(payload);
    match poll.get("id").and_then(|v| v.as_u64()) {
        Some(id) => format!("kick-poll-{slug}-{id}"),
        None => format!("kick-poll-{slug}"),
    }
}

fn prediction_message_id(slug: &str, payload: &serde_json::Value) -> String {
    let prediction = payload.get("prediction").unwrap_or(payload);
    match prediction.get("id").and_then(|v| v.as_u64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string))) {
        Some(id) => format!("kick-prediction-{slug}-{id}"),
        None => format!("kick-prediction-{slug}"),
    }
}

/// A prediction, as one readable line.
///
/// Written from the shape a prediction has rather than from a payload anybody
/// has seen: a title, and outcomes that carry a name and some count of what
/// has been staked on them. Kick does not document this and this client has
/// never received one, so every field is optional and a payload that does not
/// match produces nothing - which shows the chat as it was rather than a line
/// of empty brackets.
fn describe_prediction(payload: &serde_json::Value) -> Option<String> {
    let prediction = payload.get("prediction").unwrap_or(payload);
    let title = prediction
        .get("title")
        .or_else(|| prediction.get("question"))
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())?;

    let outcomes: Vec<String> = prediction
        .get("outcomes")
        .or_else(|| prediction.get("options"))
        .and_then(|v| v.as_array())
        .map(|outcomes| {
            outcomes
                .iter()
                .filter_map(|o| {
                    let label = o.get("label").or_else(|| o.get("title")).or_else(|| o.get("name"))?.as_str()?;
                    let staked = ["votes", "points", "total", "amount"]
                        .iter()
                        .find_map(|field| o.get(field).and_then(|v| v.as_u64()));
                    Some(match staked {
                        Some(n) => format!("{label} ({n})"),
                        None => label.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(if outcomes.is_empty() {
        format!("prediction: {title}")
    } else {
        format!("prediction: {title} — {}", outcomes.join(", "))
    })
}

/// A poll, as one readable line.
///
/// Tolerant about where the fields sit: Kick nests this under `poll` on the
/// update event and has sent it flat, and an event whose shape has moved on
/// should cost the line rather than the connection.
fn describe_poll(payload: &serde_json::Value) -> Option<String> {
    let poll = payload.get("poll").unwrap_or(payload);
    let title = poll.get("title").and_then(|v| v.as_str()).filter(|t| !t.is_empty())?;
    let options: Vec<String> = poll
        .get("options")
        .and_then(|v| v.as_array())
        .map(|options| {
            options
                .iter()
                .filter_map(|o| {
                    let label = o.get("label").and_then(|v| v.as_str())?;
                    // The running tally where there is one, since a poll with
                    // no numbers is only half the thing being watched.
                    Some(match o.get("votes").and_then(|v| v.as_u64()) {
                        Some(votes) => format!("{label} ({votes})"),
                        None => label.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(if options.is_empty() {
        format!("poll: {title}")
    } else {
        format!("poll: {title} — {}", options.join(", "))
    })
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
        thread: false,
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
fn describe_action(event: &str, p: &serde_json::Value) -> Option<(&'static str, String)> {
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
            Some(("reward", match text("user_input") {
                Some(input) => format!("{who} redeemed {reward}: {input}"),
                None => format!("{who} redeemed {reward}"),
            }))
        }

        "GiftedSubscriptionsEvent" => {
            let gifter = text("gifter_username").or_else(who)?;
            let count = p
                .get("gifted_usernames")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .or_else(|| p.get("gifted_total").and_then(|v| v.as_u64()).map(|n| n as usize))
                .unwrap_or(1);
            Some(("sub", match count {
                0 | 1 => format!("{gifter} gifted a subscription"),
                n => format!("{gifter} gifted {n} subscriptions"),
            }))
        }

        // The one of the three that carries how long they have been
        // subscribed, which is the part worth saying.
        "SubscriptionEvent" => {
            let who = who()?;
            Some(("sub", match p.get("months").and_then(|v| v.as_u64()) {
                Some(n) if n > 1 => format!("{who} subscribed - {n} months"),
                _ => format!("{who} subscribed"),
            }))
        }

        "StreamHostEvent" | "StreamHostedEvent" => {
            let who = text("host_username").or_else(who)?;
            Some(("raid", match p.get("number_viewers").and_then(|v| v.as_u64()) {
                Some(n) if n > 0 => format!("{who} raided with {n} viewers"),
                _ => format!("{who} raided the channel"),
            }))
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
            Some(("moderation", match p.get("expires_at").and_then(|v| v.as_str()) {
                Some(_) => format!("{who} was timed out{by}"),
                None => format!("{who} was banned{by}"),
            }))
        }
        "UserUnbannedEvent" => {
            let who = p.get("user").and_then(|u| u.get("username")).and_then(|v| v.as_str())?;
            Some(("moderation", format!("{who} is allowed back")))
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
    if let Some(rest) = name.strip_prefix("predictions-channel-") {
        return rest.parse().ok().map(Subscribed::Channel);
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

    /// A whole daemon in a temporary directory, so a stream event can be fed
    /// in and the message it produces read back out.
    ///
    /// Nothing about a poll or a prediction can be produced on demand from a
    /// real channel - they happen when a streamer decides to run one - so the
    /// only way to know this code works is to hand it the payload and look at
    /// what lands in the store. That is what these do.
    fn simulated_daemon(name: &str) -> AppState {
        // The same convention the audio and Tor probes use for a scratch
        // directory - a dependency for one test would be a poor trade.
        let dir = std::env::temp_dir().join(format!("nobilis-kick-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&dir.join("scrollback.db")).expect("store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(dir.join("accounts.toml")).expect("accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord_voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::backend::audio::VoicePrefsStore::open(dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc_dcc::DccPrefsStore::open(dir.join("dcc.toml"))),
        }
    }

    /// The messages a channel's buffer holds, oldest first.
    fn lines(state: &AppState, slug: &str) -> Vec<String> {
        state
            .store
            .get_backlog(&crate::model::buffer_id("kick:tester", slug), 0, 50)
            .expect("backlog")
            .into_iter()
            .map(|m| m.body)
            .collect()
    }

    fn feed(state: &AppState, watched: &mut Watched, event: &str, payload: serde_json::Value) {
        // odablock's own chatroom, as the fixture below sets it up.
        handle_event(state, "kick:tester", watched, event, Some("chatrooms.2393554.v2"), payload).expect("handled");
    }

    /// A local rename changes what a mention of you says, and nothing else.
    ///
    /// The two halves matter equally: the line reads the way you asked to be
    /// called, and the message is still a highlight - decided against the
    /// name Kick knows you by, before the rename runs. A client where calling
    /// yourself "You" quietly stopped people reaching you would be worse than
    /// one with no rename at all.
    #[test]
    fn renaming_yourself_changes_the_words_and_not_the_ping() {
        let state = simulated_daemon("own-rename");
        let mut watched = watching_odablock();
        state
            .accounts
            .add_kick(crate::accounts::KickAccountConfig {
                username: "tester".to_string(),
                display_name: Some("You".to_string()),
                ..Default::default()
            })
            .expect("account");
        state.runtime.set_own_identity("kick:tester", "tester");

        feed(&state, &mut watched, "App\\Events\\ChatMessageEvent", serde_json::json!({
            "id": "m1",
            "chatroom_id": 2393554,
            "content": "tester textgoeshere",
            "sender": { "id": 9, "username": "someoneelse", "identity": null }
        }));

        let stored = state
            .store
            .get_backlog(&crate::model::buffer_id("kick:tester", "odablock"), 0, 50)
            .expect("backlog");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].body, "@You textgoeshere");
        assert!(stored[0].is_highlight, "still addressed, whatever it is called");
    }

    /// A poll heard about an hour ago has no time left, whatever the number
    /// it was carrying said - the number was true at a moment, and the moment
    /// is part of it.
    #[test]
    fn a_card_is_only_current_while_its_clock_is() {
        let running = serde_json::json!({ "asOf": now_secs() - 5, "remaining": 60, "resultDisplayDuration": 30 });
        assert!(card_is_current(&running));

        // Voting is over but the result is still up.
        let showing = serde_json::json!({ "asOf": now_secs() - 70, "remaining": 60, "resultDisplayDuration": 30 });
        assert!(card_is_current(&showing));

        let gone = serde_json::json!({ "asOf": now_secs() - 3600, "remaining": 60, "resultDisplayDuration": 30 });
        assert!(!card_is_current(&gone));

        // No stamp at all: written by a version that did not carry one, and
        // its clock cannot be believed.
        assert!(!card_is_current(&serde_json::json!({ "remaining": 60 })));
    }

    /// The card is built from Kick's own prediction object, copied field
    /// for field off the endpoint its viewer panel reads - ULID ids, points
    /// where a poll counts votes, and a payout rate rather than a phrase.
    #[test]
    fn a_prediction_becomes_the_same_card_a_poll_does() {
        let state = simulated_daemon("prediction-card");
        let buffer_id = crate::model::buffer_id("kick:tester", "odablock");
        announce_prediction(&state, &buffer_id, &serde_json::json!({
            "prediction": {
                "id": "01M1PQXD465ZFG2RJJ2FE4166R",
                "channel_id": 54194893,
                "title": "who will win?",
                "outcomes": [
                    { "id": "OUT1", "title": "guy (mike)", "total_vote_amount": 23800, "vote_count": 12, "return_rate": 1.5 },
                    { "id": "OUT2", "title": "dude (brandon)", "total_vote_amount": 13100, "vote_count": 7, "return_rate": 2.8 }
                ],
                "duration": 120,
                "created_at": "2026-09-04T17:36:55Z",
                "state": "RESOLVED",
                "winning_outcome_id": "OUT2"
            }
        }));
        let card = state.runtime.live_card(&buffer_id, "prediction").expect("a card");
        assert_eq!(card["kind"], "prediction");
        assert_eq!(card["id"], "prediction:01M1PQXD465ZFG2RJJ2FE4166R");
        assert_eq!(card["title"], "who will win?");
        assert_eq!(card["total"], 36900.0);
        assert_eq!(card["options"][0]["label"], "guy (mike)");
        assert_eq!(card["options"][0]["votes"], 23800.0);
        assert_eq!(card["options"][0]["backers"], 12);
        assert_eq!(card["options"][0]["odds"], "1:1.5");
        assert_eq!(card["options"][1]["winner"], true);
        // Resolved, so nothing is left to bet on however long it ran.
        assert_eq!(card["remaining"], 0);

        let kept = state.store.live_cards(&buffer_id, "prediction", 10).expect("history");
        assert_eq!(kept.len(), 1);
        assert!(kept[0].0.contains("who will win?"));
    }

    /// What this account has on it, and what it stands to get back - the two
    /// things no broadcast carries and the card cannot work out alone.
    #[test]
    fn a_bet_of_your_own_shows_what_it_would_return() {
        let state = simulated_daemon("prediction-bet");
        let buffer_id = crate::model::buffer_id("kick:tester", "odablock");
        let prediction = api::Prediction {
            id: "P1".into(),
            title: "who will win?".into(),
            outcomes: vec![api::PredictionOutcome {
                id: "OUT1".into(),
                title: "guy".into(),
                total_vote_amount: 500.0,
                vote_count: 2,
                return_rate: 2.0,
            }],
            duration: 120,
            created_at: None,
            state: "ACTIVE".into(),
            winning_outcome_id: None,
        };
        let vote = api::PredictionVote { outcome_id: "OUT1".into(), total_vote_amount: 250.0 };
        announce_prediction_card(&state, &buffer_id, &prediction, Some(&vote), Some(9_000));
        let card = state.runtime.live_card(&buffer_id, "prediction").expect("a card");
        assert_eq!(card["hasVoted"], true);
        assert_eq!(card["votedOptionId"], "OUT1");
        assert_eq!(card["stake"], 250.0);
        assert_eq!(card["yourReturn"], 500.0);
        assert_eq!(card["balance"], 9_000);
        assert_eq!(card["minBet"], 10);
    }

    /// Predictions arrive on a broadcast channel of their own, named with
    /// hyphens where the chat's are named with dots. Reading the channel id
    /// back out of it is what tells the card which conversation it belongs
    /// to - and getting it wrong is how a prediction goes unnoticed.
    #[test]
    fn a_prediction_subscription_names_its_channel() {
        assert!(matches!(id_in_subscription("predictions-channel-54194893"), Some(Subscribed::Channel(54194893))));
        assert!(matches!(id_in_subscription("channel.54194893"), Some(Subscribed::Channel(54194893))));
        assert!(matches!(id_in_subscription("chatrooms.53906513.v2"), Some(Subscribed::Chatroom(53906513))));
    }

    /// A poll is one thing happening over a minute    /// A poll is one thing happening over a minute, not a stream of events -
    /// Kick sends its update on every vote, and a line each would bury the
    /// conversation the poll is about.
    #[test]
    fn a_poll_is_one_line_that_keeps_up_with_the_votes() {
        let state = simulated_daemon("poll-votes");
        let mut watched = watching_odablock();

        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 5, "title": "next game?", "options": [
                { "label": "runescape", "votes": 3 },
                { "label": "chess", "votes": 1 }
            ]}
        }));
        assert_eq!(lines(&state, "odablock"), vec!["poll: next game? — runescape (3), chess (1)"]);

        // Somebody votes. Same poll, same line.
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 5, "title": "next game?", "options": [
                { "label": "runescape", "votes": 9 },
                { "label": "chess", "votes": 1 }
            ]}
        }));
        assert_eq!(lines(&state, "odablock"), vec!["poll: next game? — runescape (9), chess (1)"]);
    }

    /// The line stays when the poll ends - what was asked and how it went is
    /// worth keeping - but stops claiming to be running.
    #[test]
    fn a_finished_poll_says_so() {
        let state = simulated_daemon("poll-ended");
        let mut watched = watching_odablock();
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 5, "title": "next game?", "options": [{ "label": "runescape", "votes": 9 }] }
        }));
        feed(&state, &mut watched, "App\\Events\\PollDeleteEvent", serde_json::json!({ "poll": { "id": 5 } }));
        assert_eq!(lines(&state, "odablock"), vec!["poll ended: next game? — runescape (9)"]);
    }

    /// Two polls in a row are two lines: the id is part of what identifies
    /// the message, so the second does not overwrite the first.
    #[test]
    fn a_second_poll_does_not_overwrite_the_first() {
        let state = simulated_daemon("two-polls");
        let mut watched = watching_odablock();
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 1, "title": "first", "options": [] }
        }));
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 2, "title": "second", "options": [] }
        }));
        assert_eq!(lines(&state, "odablock"), vec!["poll: first", "poll: second"]);
    }

    /// Predictions have never been seen by this client and Kick documents
    /// nothing, so the parser reads a title and named outcomes wherever they
    /// sit - and produces nothing at all rather than a line of empty brackets
    /// when the shape is not what it expects.
    #[test]
    fn a_prediction_reads_whichever_way_kick_writes_it() {
        let state = simulated_daemon("prediction");
        let mut watched = watching_odablock();

        feed(&state, &mut watched, "App\\Events\\PredictionUpdateEvent", serde_json::json!({
            "prediction": { "id": 3, "title": "will he win?", "outcomes": [
                { "label": "yes", "points": 400 },
                { "label": "no", "points": 120 }
            ]}
        }));
        assert_eq!(lines(&state, "odablock"), vec!["prediction: will he win? — yes (400), no (120)"]);

        feed(&state, &mut watched, "App\\Events\\PredictionDeleteEvent", serde_json::json!({ "prediction": { "id": 3 } }));
        assert_eq!(lines(&state, "odablock"), vec!["prediction closed: will he win? — yes (400), no (120)"]);
    }

    #[test]
    fn a_payload_that_is_not_a_prediction_produces_nothing() {
        let state = simulated_daemon("prediction-empty");
        let mut watched = watching_odablock();
        feed(&state, &mut watched, "App\\Events\\PredictionUpdateEvent", serde_json::json!({ "prediction": { "id": 8 } }));
        assert!(lines(&state, "odablock").is_empty());
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
            live: None,
            followers: None,
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
    fn each_event_says_what_kind_of_thing_it_is() {
        // So a reader can turn off raids without losing redemptions. One kind
        // for all of them made the settings a single on-or-off switch.
        let kind = |event: &str, p: serde_json::Value| describe_action(event, &p).map(|(k, _)| k);
        assert_eq!(kind("RewardRedeemedEvent", serde_json::json!({ "username": "a", "reward_title": "x" })), Some("reward"));
        assert_eq!(kind("SubscriptionEvent", serde_json::json!({ "username": "a" })), Some("sub"));
        assert_eq!(kind("GiftedSubscriptionsEvent", serde_json::json!({ "gifter_username": "a" })), Some("sub"));
        assert_eq!(kind("StreamHostEvent", serde_json::json!({ "host_username": "a" })), Some("raid"));
        assert_eq!(kind("UserBannedEvent", serde_json::json!({ "user": { "username": "a" } })), Some("moderation"));
    }

    #[test]
    fn reads_a_poll_wherever_kick_puts_it() {
        let nested = serde_json::json!({
            "poll": {
                "title": "what next",
                "options": [
                    { "label": "keep going", "votes": 12 },
                    { "label": "stop", "votes": 3 }
                ]
            }
        });
        assert_eq!(
            describe_poll(&nested).as_deref(),
            Some("poll: what next — keep going (12), stop (3)")
        );

        // Flat, which Kick has also sent.
        let flat = serde_json::json!({ "title": "yes or no", "options": [{ "label": "yes" }] });
        assert_eq!(describe_poll(&flat).as_deref(), Some("poll: yes or no — yes"));

        // A poll with no options is still worth announcing; one with no title
        // is not a poll anybody can read.
        assert_eq!(describe_poll(&serde_json::json!({ "title": "hm" })).as_deref(), Some("poll: hm"));
        assert_eq!(describe_poll(&serde_json::json!({ "options": [] })), None);
        assert_eq!(describe_poll(&serde_json::json!({})), None);
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
            avatar_url: None, subscribers_only: false, followers_only: false, live: None, followers: None,
        });
        w.add(&api::Channel {
            id: 222, chatroom_id: 999, slug: "second".into(), username: "second".into(),
            avatar_url: None, subscribers_only: false, followers_only: false, live: None, followers: None,
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
            describe_action("App\\Events\\RewardRedeemedEvent", &p).map(|(_, line)| line).as_deref(),
            Some("GayHayride redeemed CAT PATS")
        );
    }

    #[test]
    fn keeps_what_they_typed_with_it() {
        // The half that says what was actually asked for, where the reward
        // takes input at all.
        let p = serde_json::json!({ "username": "Hecklephish", "reward_title": "CAT TREATS", "user_input": "for Mittens" });
        assert_eq!(
            describe_action("App\\Events\\RewardRedeemedEvent", &p).map(|(_, line)| line).as_deref(),
            Some("Hecklephish redeemed CAT TREATS: for Mittens")
        );
    }

    #[test]
    fn reads_subscriptions_and_gifts_and_raids() {
        let subbed = serde_json::json!({ "username": "someone", "months": 1 });
        assert_eq!(describe_action("SubscriptionEvent", &subbed).map(|(_, l)| l).as_deref(), Some("someone subscribed"));

        let resub = serde_json::json!({ "username": "someone", "months": 7 });
        assert_eq!(describe_action("SubscriptionEvent", &resub).map(|(_, l)| l).as_deref(), Some("someone subscribed - 7 months"));

        let gifts = serde_json::json!({ "gifter_username": "big", "gifted_usernames": ["a", "b", "c"] });
        assert_eq!(describe_action("GiftedSubscriptionsEvent", &gifts).map(|(_, l)| l).as_deref(), Some("big gifted 3 subscriptions"));

        let raid = serde_json::json!({ "host_username": "friend", "number_viewers": 42 });
        assert_eq!(describe_action("StreamHostEvent", &raid).map(|(_, l)| l).as_deref(), Some("friend raided with 42 viewers"));
    }

    #[test]
    fn takes_the_name_from_wherever_the_event_put_it() {
        // These events disagree about where the person goes, and an event
        // whose shape has moved on should be skipped rather than reported with
        // a hole in it.
        let nested = serde_json::json!({ "sender": { "username": "nested" }, "reward_title": "X" });
        assert_eq!(describe_action("RewardRedeemedEvent", &nested).map(|(_, l)| l).as_deref(), Some("nested redeemed X"));

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
            describe_action("App\\Events\\SubscriptionEvent", &sub).map(|(_, line)| line).as_deref(),
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
            describe_action("App\\Events\\UserBannedEvent", &timeout).map(|(_, line)| line).as_deref(),
            Some("someone was timed out by amod")
        );

        // No expiry is a ban, which is the difference worth reporting.
        let ban = serde_json::json!({ "user": { "username": "someone" }, "banned_by": { "username": "amod" } });
        assert_eq!(
            describe_action("App\\Events\\UserBannedEvent", &ban).map(|(_, line)| line).as_deref(),
            Some("someone was banned by amod")
        );

        // A moderator who did not name themselves still produces a line.
        let anonymous = serde_json::json!({ "user": { "username": "someone" } });
        assert_eq!(describe_action("UserBannedEvent", &anonymous).map(|(_, l)| l).as_deref(), Some("someone was banned"));

        assert_eq!(
            describe_action("App\\Events\\UserUnbannedEvent", &serde_json::json!({ "user": { "username": "someone" } })).map(|(_, l)| l).as_deref(),
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
            describe_action("GiftedSubscriptionsEvent", &p).map(|(_, line)| line).as_deref(),
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
