//! The Pusher socket Kick's chat rides on.
//!
//! Kick does not run a chat server of its own so much as a channel on a
//! commercial pub/sub service, which means connecting is subscribing: this
//! module owns the websocket, the subscription list, and the frame envelope
//! everything else arrives inside.

use super::*;

/// Kick's own Pusher application, as served to every visitor of kick.com.
///
/// Public by construction: a Pusher *key* is the identifier a browser needs to
/// subscribe to public channels, which is why it appears in their page source
/// and why reading a public chat needs no credential. It is not a secret and
/// grants nothing beyond what a browser on the channel page already has.
pub(super) const PUSHER_KEY: &str = "32cbd69e4b950bf97679";

pub(super) const PUSHER_CLUSTER: &str = "us2";

pub(super) fn pusher_url() -> String {
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
pub(super) const REJOIN_ATTEMPTS: u8 = 3;

/// Asks for a refused channel again, later, and further out each time.
///
/// Each attempt is its own task with its own wait, so thirty refusals do not
/// become thirty simultaneous retries - which is the burst that caused them.
pub(super) fn retry_later(state: &AppState, account_id: &str, slug: String, attempt: u8) {
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
pub(super) const CONNECT_CONCURRENCY: usize = 3;

/// How many followed channels to open on a first connect.
///
/// A cap rather than all of them, because somebody can follow hundreds and
/// each one costs a buffer, four subscriptions and its own emote table. Fifty
/// is more channels than anyone watches at once and still opens quickly; the
/// rest are a handle away in the "+" box.
pub const MAX_FOLLOWED: usize = 50;

pub(super) const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);

pub(super) const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

pub(super) async fn run_with_retry(state: &AppState, config: &KickAccountConfig, account_id: &str) {
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

pub(super) async fn run(state: &AppState, config: &KickAccountConfig, account_id: &str) -> Result<()> {
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

pub(super) type Socket = tokio_tungstenite::WebSocketStream<
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
        let account_id = account_id.to_string();
        let avatar_url = channel.avatar_url.clone();
        async move {
            // Whether this account may *use* the streamer's subscriber emotes.
            // Treated as "no" if Kick will not say, which costs a greyed-out
            // emote rather than a conversation.
            let subscribed = match token.as_deref() {
                None => false,
                Some(token) => api::standing(&http, token, &slug).await.map(|s| s.subscribed).unwrap_or(false),
            };
            let emotes = api::emotes(&http, &slug).await.unwrap_or_default();

            // The same emotes, as a place they come from rather than as this
            // room's list. A Kick subscription buys the right to use that
            // channel's emotes across the whole site, so whether these reach
            // beyond this buffer is the subscription itself - which is why the
            // flag is set from it rather than being false everywhere but the
            // room on screen (see #205).
            if !emotes.is_empty() {
                let entries: Vec<crate::model::EmojiEntry> = emotes
                    .iter()
                    .map(|e| crate::model::EmojiEntry {
                        id: emotes::token(&e.id, &e.name),
                        name: e.name.clone(),
                        url: Some(e.url.clone()),
                        animated: false,
                        locked: e.subscribers_only && !subscribed,
                    })
                    .collect();
                state.runtime.set_emoji_source(
                    &account_id,
                    crate::model::EmojiSource {
                        id: format!("kick:{slug}"),
                        name: slug.clone(),
                        service: "kick".to_string(),
                        kind: "text".to_string(),
                        icon_url: avatar_url.clone(),
                        buffers: vec![buffer_id.clone()],
                        sendable_anywhere: subscribed,
                        emoji: entries,
                    },
                );
            }

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
pub(super) async fn subscribe(socket: &mut Socket, chatroom_id: u64, channel_id: u64) -> Result<()> {
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
pub(super) struct PusherFrame {
    pub(super) event: String,
    #[serde(default)]
    pub(super) data: serde_json::Value,
    /// Which subscription this arrived on. The most reliable way to know
    /// whose channel an event belongs to: a chat message names its room in the
    /// payload, but the doing-things events name variously the channel, the
    /// chatroom, or nothing at all - while Pusher always says which
    /// subscription delivered it.
    #[serde(default)]
    pub(super) channel: Option<String>,
}

pub(super) async fn handle_frame(
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

/// The channel behind a conversation, waiting a little for it to arrive.
///
/// A window restores the conversation it had open before its account has
/// finished connecting - which for Kick means before the channel's ids are
/// known - so asking straight away is asking too early. Every caller here is
/// already running in the background, and the alternative to waiting is a
/// poll or a prediction that is running right now and shows up nowhere until
/// something else changes.
pub(super) async fn channel_when_ready(state: &AppState, buffer_id: &str) -> Option<crate::runtime::KickChannel> {
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

/// Which watched channel an event belongs to.
///
/// The subscription it arrived on is asked first and is the reliable answer:
/// this connection subscribed per room, so the room is in the name. The
/// payload is the fallback, because these events disagree about what they name
/// - some carry the chatroom, some the channel, some neither - and a fallback
/// that is occasionally right beats an event dropped for lack of a field.
pub(super) fn channel_of(watched: &Watched, subscription: &Option<String>, payload: &serde_json::Value) -> Option<String> {
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
pub(super) enum Subscribed {
    Chatroom(u64),
    Channel(u64),
}

pub(super) fn id_in_subscription(name: &str) -> Option<Subscribed> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::*;





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
    fn never_confuses_one_streamers_numbers_for_anothers() {
        // The bug two maps exist to prevent: nothing stops one streamer's
        // chatroom id from being another's channel id, and a single map keyed
        // by "some id" would deliver one's events into the other's buffer.
        let mut w = Watched::default();
        w.add(&api::Channel {
            id: 999, chatroom_id: 111, slug: "first".into(), username: "first".into(),
            avatar_url: None, subscribers_only: false, followers_only: false, live: None, followers: None,
            playback_url: None,
        });
        w.add(&api::Channel {
            id: 222, chatroom_id: 999, slug: "second".into(), username: "second".into(),
            avatar_url: None, subscribers_only: false, followers_only: false, live: None, followers: None,
            playback_url: None,
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
    fn reads_the_id_out_of_a_subscription_name() {
        assert_eq!(id_in_subscription("chatrooms.123.v2"), Some(Subscribed::Chatroom(123)));
        assert_eq!(id_in_subscription("chatrooms.123"), Some(Subscribed::Chatroom(123)));
        assert_eq!(id_in_subscription("chatroom_123"), Some(Subscribed::Chatroom(123)));
        assert_eq!(id_in_subscription("channel.123"), Some(Subscribed::Channel(123)));
        assert_eq!(id_in_subscription("chatrooms.abc"), None);
        assert_eq!(id_in_subscription("something-else"), None);
    }
}
