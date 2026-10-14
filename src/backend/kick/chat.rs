//! What is said in a Kick channel, and who said it.
//!
//! A chat message with badges on it, a reply to another one, or one of the
//! several things Kick reports as chat that nobody typed - a subscription, a
//! raid, a gift, a moderator's action.

use super::*;

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

/// The channels this connection is following, by both of the numbers Kick
/// identifies them with.
///
/// Two maps rather than one keyed by "some id", because the two number spaces
/// are unrelated: nothing stops one streamer's chatroom id from equalling
/// another's channel id, and a single map would then deliver one streamer's
/// subscriptions into the other's buffer. Which map to ask is never a guess -
/// the subscription name says which kind of number it carries.
#[derive(Default)]
pub(super) struct Watched {
    pub(super) by_chatroom: HashMap<u64, String>,
    pub(super) by_channel: HashMap<u64, String>,
    /// Who has spoken in each channel, most recent first.
    ///
    /// Not a viewer list, and the client says so. Kick has no endpoint for
    /// one - every plausible path answers 404, and Kick's own page shows none
    /// either, because a livestream chat has no roster the way a channel does.
    /// What it does have is people talking, which is what somebody actually
    /// wants the panel for: to mention them, whisper them, or see who the
    /// moderators are.
    pub(super) speakers: HashMap<String, Vec<Speaker>>,
    /// Every name that has spoken in each channel, for counting.
    pub(super) chatters: HashMap<String, std::collections::HashSet<String>>,
}

/// Somebody who has said something, and how they looked saying it.
#[derive(Clone, Debug)]
pub(super) struct Speaker {
    pub(super) nick: String,
    pub(super) user_id: Option<String>,
    /// The strongest badge they carry, which is what the list marks them
    /// with - a moderator is worth finding in a list of two hundred names.
    pub(super) badge: Option<String>,
}

/// How many to list per channel.
///
/// A busy stream produces thousands of distinct names an hour, and the list is
/// sent whole every time a new one joins it and drawn row by row - a few
/// hundred is what that stays cheap for. It is the most recent to have
/// spoken, which is the window somebody is actually reading. How many people
/// have spoken is counted apart from it (`Watched::chatters`) and is not cut
/// off.
pub(super) const SPEAKERS_REMEMBERED: usize = 500;

#[derive(Deserialize)]
pub(super) struct ChatMessage {
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
    /// The message at the top of the chain this one is part of, on a reply.
    /// Empty or absent on anything that is not one.
    #[serde(default)]
    thread_parent_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ReplyMetadata {
    #[serde(default)]
    original_sender: Option<Sender>,
    #[serde(default)]
    original_message: Option<OriginalMessage>,
}

#[derive(Deserialize)]
pub(super) struct OriginalMessage {
    id: String,
    #[serde(default)]
    content: Option<String>,
    /// Where the original itself sits: empty when it began the chain, and the
    /// chain's first message when it was a reply. It is how a reply read from
    /// history, which does not carry the thread on the reply itself, finds its
    /// thread.
    #[serde(default)]
    thread_parent_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Sender {
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

/// One Kick event, once it has been unwrapped from Pusher's envelope.
///
/// Split from handle_frame so it can be driven with a payload rather than a
/// socket. Everything a stream does that is not somebody talking - polls,
/// predictions, redemptions, raids, moderation - arrives here, and none of it
/// can be produced on demand from a real channel: a poll happens when a
/// streamer decides to run one. Being able to hand this a payload is the only
/// way any of it is testable at all.
pub(super) fn handle_event(
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
    let frame = Delivery { channel: channel.map(str::to_string) };
    match event {
        "pusher:error" => {
            let msg = payload.get("message").and_then(|m| m.as_str()).unwrap_or("Kick refused the connection");
            anyhow::bail!("{msg}");
        }
        e if e.ends_with("ChatMessageEvent") => {
            let Ok(msg) = serde_json::from_value::<ChatMessage>(payload) else { return Ok(()) };
            let Some(slug) = watched.chatroom(msg.chatroom_id).cloned() else { return Ok(()) };
            let from = msg.sender.username.clone().unwrap_or_else(|| "someone".to_string());
            let reply_to = reply_preview(msg.metadata.as_ref(), msg.thread_parent_id.as_deref());
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
                let total = watched.chatters(&slug);
                state.runtime.set_presence(&buffer_id, roster.clone());
                state.runtime.set_presence_total(&buffer_id, total);
                state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer_id, "members": roster, "total": total }));
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
            let what = if live { "went live" } else { "ended the stream" };
            event_card(state, account_id, &slug, what, "stream", crate::model::Embed {
                kind: Some("stream".to_string()),
                author: Some(slug.clone()),
                title: Some(what.to_string()),
                icon: Some(if live { "live" } else { "offline" }.to_string()),
                ..Default::default()
            });
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
        // Not a pin coming down, which Kick calls `PinnedMessageDeletedEvent` and
        // which ends the same way: that is the pin's, below, and taking it for a
        // message deleted left a pin on screen for ever after it was removed.
        e if e.ends_with("MessageDeletedEvent") && !pins::is_pin_event(e) => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            // The id is nested under `message` on this event and flat on
            // others Kick has sent; both name the same thing.
            let id = payload
                .get("message")
                .and_then(|m| m.get("id"))
                .or_else(|| payload.get("id"))
                .and_then(|v| v.as_str());
            if let Some(id) = id {
                // Kept and marked, not removed: what it said is worth reading
                // beside the fact that it is gone, and how a removed message
                // is drawn is the window's to decide.
                state.runtime.moderate_message(state, &crate::model::buffer_id(account_id, &slug), id, &moderation_of(&payload));
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

        // A poll running on stream, written into the log as a line and drawn
        // on a card that can be voted in - the line is the record of what was
        // asked, the card is the thing somebody can answer.
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

        // The one line a channel wants everybody to have read.
        //
        // Matched by shape rather than by exact name, unlike the subscription
        // events below. Those are matched exactly because three of them mean
        // the same thing and had to be told apart; these are the opposite
        // case - Kick documents neither, the names come from the wrappers
        // that reverse-engineered this socket, and a rename would silently
        // turn the feature off. Any event about a pin is handled, and its
        // direction is read from the name.
        e if pins::is_pin_event(e) => {
            let Some(slug) = channel_of(watched, &frame.channel, &payload) else { return Ok(()) };
            let buffer_id = crate::model::buffer_id(account_id, &slug);
            if pins::is_unpin(e) {
                pins::announce(state, &buffer_id, None);
            } else if let Some(pin) = pins::read(&payload) {
                pins::announce(state, &buffer_id, Some(pin));
            }
            // A pin event carrying nothing readable is left alone rather than
            // treated as an unpin: the shape having moved on is not the
            // channel having taken its pin down, and clearing on it would
            // wipe a pin that is still up.
        }

        // A moderator emptying the room.
        e if e.ends_with("ChatroomClearEvent") => {
            if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                event_card(state, account_id, &slug, "a moderator cleared the chat", "moderation", crate::model::Embed {
                    kind: Some("moderation".to_string()),
                    author: Some("A moderator".to_string()),
                    title: Some("cleared the chat".to_string()),
                    ..Default::default()
                });
            }
        }

        // The things people do rather than say.
        e => {
            if let Some((kind, line)) = describe_action(e, &payload) {
                if let Some(slug) = channel_of(watched, &frame.channel, &payload) {
                    // A KICKs gift or a gifted subscription is drawn as the
                    // card it is, from what the line cannot hold: who it was
                    // for, how much it was, what was said with it.
                    match action_card(e, &payload) {
                        Some(card) if !already_seen(&slug, &line, &payload) => {
                            event_card(state, account_id, &slug, &line, kind, card)
                        }
                        Some(_) => {}
                        None => system_line(state, account_id, &slug, &line, kind),
                    }
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
            reply_preview_from_value(msg.metadata.as_ref(), msg.thread_parent_id.as_deref()),
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
///
/// An object on the live socket and a string of JSON in history, which is
/// the same object written down: a reply read back from history lost its quote
/// because only the first was understood.
pub(super) fn reply_preview_from_value(metadata: Option<&serde_json::Value>, thread_parent: Option<&str>) -> Option<crate::model::ReplyPreview> {
    let metadata = metadata?;
    let parsed: ReplyMetadata = match metadata {
        serde_json::Value::String(text) => serde_json::from_str(text).ok()?,
        other => serde_json::from_value(other.clone()).ok()?,
    };
    reply_preview(Some(&parsed), thread_parent)
}

/// Who took a message down and for what, as the event says it.
///
/// `aiModerated` and `violatedRules` are on the payload of the event for a
/// message Kick's automatic moderation removed; a person's removal has
/// neither, which reads as not automatic with nothing to say about rules.
pub(super) fn moderation_of(payload: &serde_json::Value) -> crate::model::Moderated {
    crate::model::Moderated {
        ai: payload.get("aiModerated").and_then(|v| v.as_bool()).unwrap_or(false),
        rules: payload
            .get("violatedRules")
            .and_then(|v| v.as_array())
            .map(|rules| rules.iter().filter_map(|r| r.as_str()).map(str::to_string).collect())
            .unwrap_or_default(),
    }
}

/// The one badge worth marking somebody with in a list.
///
/// A name can carry four; a list showing all of them is a list of badges with
/// names attached. Ranked by what a reader is scanning for - who runs the
/// channel, who moderates it - rather than by what is rarest.
pub(super) fn strongest_badge(badges: &[api::Badge]) -> Option<String> {
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
pub(super) fn style_of(identity: Option<&api::ChatIdentity>) -> crate::model::SenderStyle {
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
///
/// `thread_parent` is what the message itself says its chain began with. When
/// it says nothing - a reply out of history - the original's own place in its
/// chain gives the same answer, and an original that began one is the root.
pub(super) fn reply_preview(metadata: Option<&ReplyMetadata>, thread_parent: Option<&str>) -> Option<crate::model::ReplyPreview> {
    let metadata = metadata?;
    let original = metadata.original_message.as_ref()?;
    let from = metadata.original_sender.as_ref()?.username.clone()?;
    let filled = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
    let thread_root = filled(thread_parent).or_else(|| filled(original.thread_parent_id.as_deref())).unwrap_or_else(|| original.id.clone());
    Some(crate::model::ReplyPreview {
        id: original.id.clone(),
        from,
        body: original.content.clone().unwrap_or_default(),
        thread: false,
        forwarded: false,
        thread_root: Some(thread_root),
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
pub(super) fn describe_action(event: &str, p: &serde_json::Value) -> Option<(&'static str, String)> {
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

        // KICKs sent to a streamer: Kick's own money, spent in the chat. The
        // payload is not documented; what is read is what its webhook form
        // (`kicks.gifted`) names, and an event with no sender or no amount is
        // skipped rather than drawn as "someone sent 0".
        "KicksGifted" | "KicksGiftedEvent" => {
            let (sender, amount) = kicks_of(p)?;
            Some(("kicks", format!("{sender} sent {amount} {}", if amount == 1 { "Kick" } else { "Kicks" })))
        }

        // Kick sends one raid as two events on two subscriptions, measured
        // on a live channel: `StreamHostEvent`, with the host and how many
        // came, and `StreamHostedEvent`, the same raid with the viewer count
        // under another name. The first is the one believed; reading both
        // drew every raid twice, once without its number.
        "StreamHostedEvent" => None,

        "StreamHostEvent" => {
            let who = text("host_username").or_else(who)?;
            Some(("raid", match p.get("number_viewers").and_then(|v| v.as_u64()) {
                Some(1) => format!("{who} raided with 1 viewer"),
                Some(n) if n > 1 => format!("{who} raided with {n} viewers"),
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

/// Who sent KICKs and how many, from the payload wherever Kick puts them.
fn kicks_of(p: &serde_json::Value) -> Option<(String, u64)> {
    let name = |v: &serde_json::Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let sender = p
        .get("sender")
        .and_then(|s| s.get("username"))
        .and_then(name)
        .or_else(|| p.get("username").and_then(name))
        .or_else(|| p.get("sender_username").and_then(name))
        .or_else(|| p.get("user").and_then(|u| u.get("username")).and_then(name))?;
    let number = |v: &serde_json::Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()));
    let amount = p
        .get("gift")
        .and_then(|g| g.get("amount"))
        .and_then(number)
        .or_else(|| p.get("amount").and_then(number))
        .or_else(|| p.get("kicks").and_then(number))
        .or_else(|| p.get("kicks_amount").and_then(number))
        .filter(|n| *n > 0)?;
    Some((sender, amount))
}

/// The colour of a gift's tier, as the decimal RGB an embed carries. Kick's
/// green for the ordinary kind; the others are told apart by hue, because
/// what matters at a glance is that one is bigger than the rest.
fn tier_colour(tier: &str) -> i64 {
    match tier.to_ascii_uppercase().as_str() {
        "MID" | "MEDIUM" | "STANDARD" => 0x2E9BFF,
        "HIGH" | "PREMIUM" | "RARE" => 0xB45CFF,
        "LEGENDARY" | "EPIC" | "ULTRA" => 0xFFA800,
        _ => 0x53FC18,
    }
}

/// The card a KICKs gift or a gifted subscription is drawn as.
///
/// An embed, because that is the shape every frontend already has for "more
/// than a line": the line stays as the text of the message - what a search, a
/// copy or a frontend that draws no cards sees - and the card carries the parts.
///
/// - `kicks`: `author` sends, `title` is the amount, `provider` the gift's name
///   ("Full Send"), `description` what was said with it, `color` its tier.
/// - `gift-sub`: `author` gifts, `title` is how many, and `description` is who
///   received them, one name to a line.
/// - `raid`: `author` raids, `title` is how many came (absent when unsaid), and
///   `description` what they said.
/// - `subscribe`: `author` subscribes, `title` is how many months when more than one.
/// - `moderation`: `author` is who it happened to, `title` what happened ("was banned",
///   "was timed out", "is allowed back") and `description` who did it, where it says.
/// - `stream`: `author` is the channel, `title` what it did, and `icon` is "live" or "offline".
/// - `redeem`: `author` redeems, `title` is the reward, `description` what they
///   typed with it, and `color` the streamer's colour for the reward.
pub(super) fn action_card(event: &str, p: &serde_json::Value) -> Option<crate::model::Embed> {
    let name = event.rsplit('\\').next().unwrap_or(event);
    let text = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    match name {
        "KicksGifted" | "KicksGiftedEvent" => {
            let (sender, amount) = kicks_of(p)?;
            let gift = p.get("gift");
            let tier = text(gift.and_then(|g| g.get("tier"))).or_else(|| text(gift.and_then(|g| g.get("type"))));
            Some(crate::model::Embed {
                kind: Some("kicks".to_string()),
                author: Some(sender),
                author_color: text(p.get("sender").and_then(|s| s.get("username_color"))),
                title: Some(amount.to_string()),
                provider: text(gift.and_then(|g| g.get("name"))),
                description: text(gift.and_then(|g| g.get("message"))).or_else(|| text(p.get("message"))),
                color: Some(tier.as_deref().map_or(0x53FC18, tier_colour)),
                ..Default::default()
            })
        }
        "SubscriptionEvent" => {
            let who = text(p.get("username"))
                .or_else(|| text(p.get("sender").and_then(|s| s.get("username"))))
                .or_else(|| text(p.get("user").and_then(|s| s.get("username"))))?;
            Some(crate::model::Embed {
                kind: Some("subscribe".to_string()),
                author: Some(who),
                // Months, where they are more than the first one.
                title: p.get("months").and_then(|v| v.as_u64()).filter(|n| *n > 1).map(|n| n.to_string()),
                ..Default::default()
            })
        }
        "UserBannedEvent" => {
            let who = text(p.get("user").and_then(|u| u.get("username")))?;
            Some(crate::model::Embed {
                kind: Some("moderation".to_string()),
                author: Some(who),
                title: Some(if p.get("expires_at").and_then(|v| v.as_str()).is_some() { "was timed out" } else { "was banned" }.to_string()),
                description: text(p.get("banned_by").and_then(|u| u.get("username"))),
                ..Default::default()
            })
        }
        "UserUnbannedEvent" => {
            let who = text(p.get("user").and_then(|u| u.get("username")))?;
            Some(crate::model::Embed {
                kind: Some("moderation".to_string()),
                author: Some(who),
                title: Some("is allowed back".to_string()),
                ..Default::default()
            })
        }
        "StreamHostEvent" => {
            let host = text(p.get("host_username")).or_else(|| text(p.get("username")))?;
            Some(crate::model::Embed {
                kind: Some("raid".to_string()),
                author: Some(host),
                title: p.get("number_viewers").and_then(|v| v.as_u64()).filter(|n| *n > 0).map(|n| n.to_string()),
                description: text(p.get("optional_message")),
                ..Default::default()
            })
        }
        "RewardRedeemedEvent" => {
            let who = text(p.get("username"))?;
            let reward = text(p.get("reward_title")).or_else(|| text(p.get("title")))?;
            Some(crate::model::Embed {
                kind: Some("redeem".to_string()),
                author: Some(who),
                title: Some(reward),
                description: text(p.get("user_input")),
                // The streamer's own colour for the reward, which Kick draws
                // the card's bar and the name in.
                color: text(p.get("reward_background_color"))
                    .and_then(|c| i64::from_str_radix(c.trim_start_matches('#'), 16).ok()),
                ..Default::default()
            })
        }
        "GiftedSubscriptionsEvent" => {
            let gifter = text(p.get("gifter_username")).or_else(|| text(p.get("username")))?;
            let names: Vec<String> = p
                .get("gifted_usernames")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|n| text(Some(n))).collect())
                .unwrap_or_default();
            let count = if names.is_empty() {
                p.get("gifted_total").and_then(|v| v.as_u64()).unwrap_or(1).max(1)
            } else {
                names.len() as u64
            };
            Some(crate::model::Embed {
                kind: Some("gift-sub".to_string()),
                author: Some(gifter),
                title: Some(count.to_string()),
                description: (!names.is_empty()).then(|| names.join("\n")),
                ..Default::default()
            })
        }
        _ => None,
    }
}

/// Kick sends an event more than once when it reaches more than one of a
/// channel's subscriptions, so the same gift is recognised for a few seconds
/// rather than drawn twice.
fn already_seen(slug: &str, line: &str, p: &serde_json::Value) -> bool {
    static RECENT: std::sync::LazyLock<std::sync::Mutex<HashMap<String, std::time::Instant>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
    let now = std::time::Instant::now();
    let key = format!("{slug}\u{0}{line}\u{0}{}", p.get("gift_transaction_id").or_else(|| p.get("created_at")).or_else(|| p.get("message")).map(|v| v.to_string()).unwrap_or_default());
    let mut recent = RECENT.lock().unwrap();
    recent.retain(|_, at| now.duration_since(*at) < Duration::from_secs(10));
    if recent.contains_key(&key) {
        return true;
    }
    recent.insert(key, now);
    false
}

/// A line that carries a card.
pub(super) fn event_card(state: &AppState, account_id: &str, slug: &str, what: &str, kind: &str, card: crate::model::Embed) {
    state.runtime.record_message(
        state, account_id, slug, "channel", slug, what, false, kind, None, None, false, None,
        vec![card], Vec::new(), None,
    );
}

/// A line that nobody said.
///
/// `kind` is what separates "the stream started" from "somebody redeemed
/// something": both are events rather than chatter, but only one of them is a
/// person doing something, and the client draws that one on a plate.
pub(super) fn system_line(state: &AppState, account_id: &str, slug: &str, what: &str, kind: &str) {
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
pub(super) fn parse_timestamp(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|t| t.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::*;

    #[test]
    fn reads_kicks_timestamps() {
        assert_eq!(parse_timestamp("2026-01-08T23:32:57.000000Z"), Some(1767915177));
        assert_eq!(parse_timestamp("2026-01-08T23:32:57Z"), Some(1767915177));
        assert_eq!(parse_timestamp("not a time"), None);
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
        // The count is of everybody, not of those listed.
        assert_eq!(w.chatters("chan"), SPEAKERS_REMEMBERED + 50);
        // And somebody who spoke long ago and speaks again is not counted twice.
        w.heard("chan", Speaker { nick: "nick0".into(), user_id: None, badge: None });
        assert_eq!(w.chatters("chan"), SPEAKERS_REMEMBERED + 50);
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
        assert_eq!(w.roster("odablock").len(), 1, "somebody was heard to begin with");
        w.remove("odablock");
        assert!(w.roster("odablock").is_empty());
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
        let preview = reply_preview(Some(&metadata), None).unwrap();
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
        let preview = reply_preview(msg.metadata.as_ref(), None).expect("and carry what it answered");
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
        assert!(reply_preview(msg.metadata.as_ref(), None).is_none());
    }

    #[test]
    fn an_ordinary_message_is_not_a_reply() {
        assert!(reply_preview(None, None).is_none());
    }

    #[test]
    fn half_a_reply_is_no_reply() {
        // An empty quote above a message reads as something failing to load,
        // which is worse than showing the message plainly.
        let no_sender: ReplyMetadata =
            serde_json::from_value(serde_json::json!({ "original_message": { "id": "x", "content": "y" } })).unwrap();
        assert!(reply_preview(Some(&no_sender), None).is_none());
        let no_message: ReplyMetadata =
            serde_json::from_value(serde_json::json!({ "original_sender": { "username": "Greg" } })).unwrap();
        assert!(reply_preview(Some(&no_message), None).is_none());
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
    fn a_gifted_subscription_keeps_who_received_it() {
        let p = serde_json::json!({
            "chatroom_id": 2393554,
            "gifted_usernames": ["Interstelar", "Ron_OSRS", "Auzzzii"],
            "gifter_username": "ItsDez",
            "gifted_total": 3
        });
        let card = action_card("GiftedSubscriptionsEvent", &p).unwrap();
        assert_eq!(card.kind.as_deref(), Some("gift-sub"));
        assert_eq!(card.author.as_deref(), Some("ItsDez"));
        assert_eq!(card.title.as_deref(), Some("3"));
        assert_eq!(card.description.as_deref(), Some("Interstelar\nRon_OSRS\nAuzzzii"));
        // One with no names - the count alone - is still a card.
        let bare = action_card("GiftedSubscriptionsEvent", &serde_json::json!({ "gifter_username": "a", "gifted_total": 10 })).unwrap();
        assert_eq!(bare.title.as_deref(), Some("10"));
        assert_eq!(bare.description, None);
    }

    #[test]
    fn kicks_are_read_wherever_kick_puts_them() {
        let nested = serde_json::json!({
            "sender": { "id": 7, "username": "Currington" },
            "gift": { "amount": 100, "name": "Full Send", "tier": "BASIC", "message": "lets go" }
        });
        assert_eq!(describe_action("KicksGifted", &nested), Some(("kicks", "Currington sent 100 Kicks".to_string())));
        let card = action_card("App\\Events\\KicksGifted", &nested).unwrap();
        assert_eq!(card.kind.as_deref(), Some("kicks"));
        assert_eq!(card.author.as_deref(), Some("Currington"));
        assert_eq!(card.title.as_deref(), Some("100"));
        assert_eq!(card.provider.as_deref(), Some("Full Send"));
        assert_eq!(card.description.as_deref(), Some("lets go"));
        assert_eq!(card.color, Some(0x53FC18));
        // One Kick is singular, and a flat shape reads the same.
        let flat = serde_json::json!({ "username": "a", "amount": "1", "message": "hi" });
        assert_eq!(describe_action("KicksGifted", &flat).map(|(_, l)| l).as_deref(), Some("a sent 1 Kick"));
        assert_eq!(action_card("KicksGifted", &flat).unwrap().description.as_deref(), Some("hi"));
        // Nobody, or nothing, is not a gift.
        assert!(describe_action("KicksGifted", &serde_json::json!({ "gift": { "amount": 5 } })).is_none());
        assert!(describe_action("KicksGifted", &serde_json::json!({ "sender": { "username": "a" }, "gift": { "amount": 0 } })).is_none());
    }

    #[test]
    fn reads_a_real_kicks_payload() {
        // Copied from a live channel's `channel_<id>` subscription.
        let p = serde_json::json!({
            "gift_transaction_id": "2bd6023b-020b-4c91-a47b-e43eff66656f",
            "sender": { "id": 93750218, "username": "BlestRS", "username_color": "#FF2C56", "profile_picture": "https://kick.com/img/x.webp" },
            "gift": { "gift_id": "full_send", "name": "Full Send", "amount": 100, "type": "BASIC", "tier": "BASIC", "character_limit": 0, "pinned_time": 0 },
            "created_at": "2026-10-11T00:42:57.551767966Z"
        });
        assert_eq!(describe_action("KicksGifted", &p).map(|(_, l)| l).as_deref(), Some("BlestRS sent 100 Kicks"));
        let card = action_card("KicksGifted", &p).unwrap();
        assert_eq!(card.author.as_deref(), Some("BlestRS"));
        assert_eq!(card.author_color.as_deref(), Some("#FF2C56"));
        assert_eq!(card.provider.as_deref(), Some("Full Send"));
        assert_eq!(card.description, None);
    }

    #[test]
    fn a_gift_arriving_twice_is_drawn_once() {
        let p = serde_json::json!({ "created_at": "2026-10-10T20:00:00Z" });
        assert!(!already_seen("dupe-channel", "a sent 5 Kicks", &p));
        assert!(already_seen("dupe-channel", "a sent 5 Kicks", &p));
        assert!(!already_seen("dupe-channel", "a sent 6 Kicks", &p));
    }

    #[test]
    fn a_raid_is_one_card_and_a_redeem_keeps_its_colour() {
        let raid = serde_json::json!({ "chatroom_id": 1, "optional_message": null, "number_viewers": 25, "host_username": "kreationX" });
        let card = action_card("App\\Events\\StreamHostEvent", &raid).unwrap();
        assert_eq!((card.kind.as_deref(), card.author.as_deref(), card.title.as_deref()), (Some("raid"), Some("kreationX"), Some("25")));
        // The same raid, said again on another subscription, is not another line.
        let again = serde_json::json!({ "message": { "numberOfViewers": 25 }, "user": { "username": "kreationX" } });
        assert!(describe_action("App\\Events\\StreamHostedEvent", &again).is_none());

        let redeem = serde_json::json!({ "reward_title": "5 TONGUE TWISTERS", "user_id": 1, "username": "onkhell", "user_input": "", "reward_background_color": "#FF55B3" });
        let card = action_card("RewardRedeemedEvent", &redeem).unwrap();
        assert_eq!((card.kind.as_deref(), card.author.as_deref(), card.title.as_deref()), (Some("redeem"), Some("onkhell"), Some("5 TONGUE TWISTERS")));
        assert_eq!(card.color, Some(0xFF55B3));
        assert_eq!(card.description, None);
    }

    #[test]
    fn kicks_and_gifted_subs_land_in_the_store_as_cards() {
        let state = testkit::simulated_daemon("cards");
        let mut watched = testkit::watching_odablock();
        // On the subscription they were measured arriving on.
        let kicks = serde_json::json!({
            "gift_transaction_id": "t-1",
            "sender": { "id": 1, "username": "BlestRS", "username_color": "#FF2C56" },
            "gift": { "gift_id": "full_send", "name": "Full Send", "amount": 100, "type": "BASIC", "tier": "BASIC" },
            "created_at": "2026-10-11T00:42:57.551767966Z"
        });
        for _ in 0..2 {
            handle_event(&state, "kick:tester", &mut watched, "KicksGifted", Some("channel_2401072"), kicks.clone()).unwrap();
        }
        let gift = serde_json::json!({ "chatroom_id": 2393554, "gifted_usernames": ["a", "b"], "gifter_username": "kufallen", "gifted_total": 2 });
        handle_event(&state, "kick:tester", &mut watched, "GiftedSubscriptionsEvent", Some("chatroom_2393554"), gift).unwrap();

        let rows = state.store.get_backlog(&crate::model::buffer_id("kick:tester", "odablock"), 0, 50).unwrap();
        // The same gift on two subscriptions is one row.
        assert_eq!(rows.len(), 2, "{:?}", rows.iter().map(|m| &m.body).collect::<Vec<_>>());
        assert_eq!((rows[0].kind.as_str(), rows[0].body.as_str()), ("kicks", "BlestRS sent 100 Kicks"));
        assert_eq!(rows[0].embeds[0].kind.as_deref(), Some("kicks"));
        assert_eq!(rows[0].embeds[0].author_color.as_deref(), Some("#FF2C56"));
        assert_eq!((rows[1].kind.as_str(), rows[1].body.as_str()), ("sub", "kufallen gifted 2 subscriptions"));
        assert_eq!(rows[1].embeds[0].description.as_deref(), Some("a\nb"));
    }

    fn spoken(state: &AppState, watched: &mut Watched, id: &str, text: &str) {
        feed(state, watched, "App\\Events\\ChatMessageEvent", serde_json::json!({
            "id": id, "chatroom_id": 2393554, "content": text, "sender": { "id": 9, "username": "bob", "identity": null }
        }));
    }

    #[tokio::test]
    async fn going_live_and_ending_are_said_in_the_channel() {
        let state = testkit::simulated_daemon("live-lines");
        let mut watched = testkit::watching_odablock();
        feed(&state, &mut watched, "App\\Events\\StreamerIsLive", serde_json::json!({}));
        feed(&state, &mut watched, "App\\Events\\StopStreamBroadcast", serde_json::json!({}));
        assert_eq!(testkit::lines(&state, "odablock"), vec!["went live", "ended the stream"]);
    }

    fn backlog(state: &AppState) -> Vec<crate::model::Message> {
        state.store.get_backlog(&crate::model::buffer_id("kick:tester", "odablock"), 0, 50).expect("backlog")
    }

    #[test]
    fn a_message_taken_down_stays_in_the_log_marked() {
        let state = testkit::simulated_daemon("deleted");
        let mut watched = testkit::watching_odablock();
        spoken(&state, &mut watched, "keep", "stays");
        spoken(&state, &mut watched, "nested", "goes");
        spoken(&state, &mut watched, "flat", "goes too");
        // Kick nests the id under `message` on one event and sends it flat on another.
        feed(&state, &mut watched, "App\\Events\\MessageDeletedEvent", serde_json::json!({ "message": { "id": "nested" } }));
        feed(&state, &mut watched, "App\\Events\\MessageDeletedEvent", serde_json::json!({ "id": "flat" }));
        let log = backlog(&state);
        // Nothing left, and the words of the ones taken down are still there to read.
        assert_eq!(log.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), vec!["stays", "goes", "goes too"]);
        assert_eq!(log.iter().map(|m| m.moderated.is_some()).collect::<Vec<_>>(), vec![false, true, true]);
    }

    #[test]
    fn what_the_automatic_moderator_said_is_kept_with_the_message() {
        let state = testkit::simulated_daemon("moderated");
        let mut watched = testkit::watching_odablock();
        spoken(&state, &mut watched, "bad", "something vile");
        // As measured on a live channel: the event has its own id, and the message's is nested.
        feed(
            &state,
            &mut watched,
            "App\\Events\\MessageDeletedEvent",
            serde_json::json!({ "id": "event-id", "message": { "id": "bad" }, "aiModerated": true, "violatedRules": ["hate"] }),
        );
        let log = backlog(&state);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].moderated, Some(crate::model::Moderated { ai: true, rules: vec!["hate".into()] }));
    }

    #[test]
    fn a_person_removing_a_message_is_not_the_automatic_moderator() {
        let why = moderation_of(&serde_json::json!({ "message": { "id": "x" } }));
        assert_eq!(why, crate::model::Moderated { ai: false, rules: vec![] });
    }

    #[test]
    fn a_reply_names_the_thread_it_belongs_to() {
        // The top-level field when there is one: this is a reply to a reply, so what it quotes and
        // the thread it is in are different messages.
        let metadata: ReplyMetadata = serde_json::from_value(serde_json::json!({
            "original_sender": { "id": 1, "username": "Greg" },
            "original_message": { "id": "middle", "content": "hello" }
        }))
        .unwrap();
        let preview = reply_preview(Some(&metadata), Some("top")).unwrap();
        assert_eq!((preview.id.as_str(), preview.thread_root.as_deref()), ("middle", Some("top")));
        // Without it, a reply to a message that began nothing starts the thread itself.
        let preview = reply_preview(Some(&metadata), Some("")).unwrap();
        assert_eq!(preview.thread_root.as_deref(), Some("middle"));
    }

    #[test]
    fn a_reply_out_of_history_finds_its_thread_in_what_it_quoted() {
        // History writes the metadata down as a string and leaves the thread off the reply; the
        // quoted message knows its own.
        let written = serde_json::Value::String(
            serde_json::json!({
                "original_sender": { "id": 1, "username": "Greg" },
                "original_message": { "id": "middle", "content": "hello", "thread_parent_id": "top" }
            })
            .to_string(),
        );
        let preview = reply_preview_from_value(Some(&written), None).expect("a string of JSON is the same metadata");
        assert_eq!((preview.id.as_str(), preview.from.as_str(), preview.thread_root.as_deref()), ("middle", "Greg", Some("top")));
    }

    #[test]
    fn replies_are_stored_with_their_thread_and_read_back_whole() {
        let state = testkit::simulated_daemon("threads");
        let mut watched = testkit::watching_odablock();
        feed(&state, &mut watched, "App\\Events\\ChatMessageEvent", serde_json::json!({
            "id": "top", "chatroom_id": 2393554, "content": "the question", "created_at": "2026-10-14T00:59:00+00:00",
            "sender": { "id": 9, "username": "bob", "identity": null }
        }));
        for (id, to, root) in [("r1", "top", "top"), ("r2", "r1", "top"), ("elsewhere", "x", "x")] {
            feed(
                &state,
                &mut watched,
                "App\\Events\\ChatMessageEvent",
                serde_json::json!({
                    "id": id, "chatroom_id": 2393554, "content": id, "type": "reply",
                    "created_at": "2026-10-14T01:00:00+00:00",
                    "sender": { "id": 7, "username": "someone" },
                    "thread_parent_id": root,
                    "metadata": { "original_sender": { "id": 1, "username": "Greg" }, "original_message": { "id": to, "content": "quoted" } }
                }),
            );
        }
        let buffer = crate::model::buffer_id("kick:tester", "odablock");
        let thread = state.store.thread_messages(&buffer, "top").unwrap();
        assert_eq!(thread.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["top", "r1", "r2"]);
        let r2 = thread.iter().find(|m| m.id == "r2").unwrap().reply_to.clone().unwrap();
        // It still quotes what it answered, and says which thread that is part of.
        assert_eq!((r2.id.as_str(), r2.thread, r2.thread_root.as_deref()), ("r1", false, Some("top")));
    }

    #[test]
    fn a_moderator_emptying_the_room_is_said() {
        let state = testkit::simulated_daemon("cleared");
        let mut watched = testkit::watching_odablock();
        feed(&state, &mut watched, "App\\Events\\ChatroomClearEvent", serde_json::json!({}));
        assert_eq!(testkit::lines(&state, "odablock"), vec!["a moderator cleared the chat"]);
    }

    #[test]
    fn the_chat_rules_changing_are_kept_for_the_next_message_to_be_explained_by() {
        let state = testkit::simulated_daemon("chat-modes");
        let mut watched = testkit::watching_odablock();
        let buffer = crate::model::buffer_id("kick:tester", "odablock");
        state.runtime.set_kick_channel(&buffer, crate::runtime::KickChannel {
            slug: "odablock".into(), channel_id: 2401072, history_cursor: None, chatroom_id: 2393554, subscribed: false,
            followers_only: false, subscribers_only: false, slow_seconds: None, emotes: Vec::new(),
        });
        feed(&state, &mut watched, "App\\Events\\ChatroomUpdatedEvent", serde_json::json!({
            "followers_mode": { "enabled": true },
            "subscribers_mode": { "enabled": false },
            "slow_mode": { "enabled": true, "message_interval": 30 }
        }));
        let now = state.runtime.kick_channel(&buffer).unwrap();
        assert_eq!((now.followers_only, now.subscribers_only, now.slow_seconds), (true, false, Some(30)));
        // A slow mode that is switched off has no interval, whatever it still says.
        feed(&state, &mut watched, "App\\Events\\ChatroomUpdatedEvent", serde_json::json!({
            "subscribers_mode": { "enabled": true }, "slow_mode": { "enabled": false, "message_interval": 30 }
        }));
        let now = state.runtime.kick_channel(&buffer).unwrap();
        assert_eq!((now.followers_only, now.subscribers_only, now.slow_seconds), (false, true, None));
    }

    #[test]
    fn a_pin_and_its_removal_reach_the_channel() {
        let state = testkit::simulated_daemon("pins");
        let mut watched = testkit::watching_odablock();
        let buffer = crate::model::buffer_id("kick:tester", "odablock");
        feed(&state, &mut watched, "App\\Events\\PinnedMessageCreatedEvent", serde_json::json!({
            "message": { "id": "p1", "content": "read the rules", "sender": { "username": "mod" } }
        }));
        assert_eq!(state.runtime.kick_pin(&buffer).map(|p| p.body), Some("read the rules".to_string()));
        // An event about a pin that says nothing readable leaves the pin up.
        feed(&state, &mut watched, "App\\Events\\PinnedMessageCreatedEvent", serde_json::json!({}));
        assert!(state.runtime.kick_pin(&buffer).is_some());
        feed(&state, &mut watched, "App\\Events\\PinnedMessageDeletedEvent", serde_json::json!({}));
        assert!(state.runtime.kick_pin(&buffer).is_none());
    }

    #[test]
    fn a_gifts_tier_is_a_colour_and_an_ordinary_one_is_green() {
        assert_eq!(tier_colour("BASIC"), 0x53FC18);
        assert_eq!(tier_colour("anything else"), 0x53FC18);
        assert_eq!(tier_colour("mid"), 0x2E9BFF);
        assert_eq!(tier_colour("PREMIUM"), 0xB45CFF);
        assert_eq!(tier_colour("Legendary"), 0xFFA800);
    }

    #[test]
    fn a_raid_with_nobody_in_it_says_only_that_it_came() {
        let none = serde_json::json!({ "host_username": "friend", "number_viewers": 0 });
        assert_eq!(describe_action("StreamHostEvent", &none).map(|(_, l)| l).as_deref(), Some("friend raided the channel"));
        let one = serde_json::json!({ "host_username": "friend", "number_viewers": 1 });
        assert_eq!(describe_action("StreamHostEvent", &one).map(|(_, l)| l).as_deref(), Some("friend raided with 1 viewer"));
        assert_eq!(action_card("StreamHostEvent", &none).unwrap().title, None, "no count to put on the card");
    }

    #[test]
    fn a_sender_is_drawn_in_their_colour_with_the_badges_that_say_something() {
        let badge = |kind: &str| api::Badge { kind: kind.into(), text: kind.into(), count: None };
        assert_eq!(style_of(None).color, None);
        let style = style_of(Some(&api::ChatIdentity {
            color: Some("#75FD46".into()),
            badges: vec![badge("moderator"), badge("level"), badge(""), badge("subscriber")],
        }));
        assert_eq!(style.color.as_deref(), Some("#75FD46"));
        // An engagement level and a nameless badge say nothing about the person.
        assert_eq!(style.badges.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>(), vec!["moderator", "subscriber"]);
        // A colour that is empty is no colour.
        assert_eq!(style_of(Some(&api::ChatIdentity { color: Some(String::new()), badges: vec![] })).color, None);
    }

    #[test]
    fn a_reply_needs_both_who_and_what() {
        let whole = serde_json::json!({
            "original_sender": { "id": 1, "username": "Greg" },
            "original_message": { "id": "abc", "content": "hello" }
        });
        let preview = reply_preview_from_value(Some(&whole), None).unwrap();
        assert_eq!((preview.id.as_str(), preview.from.as_str(), preview.body.as_str()), ("abc", "Greg", "hello"));
        // Half of one is a hole above the message, and is left out.
        assert!(reply_preview_from_value(Some(&serde_json::json!({ "original_message": { "id": "abc", "content": "hello" } })), None).is_none());
        assert!(reply_preview_from_value(Some(&serde_json::json!({ "original_sender": { "id": 1, "username": "Greg" } })), None).is_none());
        assert!(reply_preview_from_value(Some(&serde_json::json!({ "original_sender": { "id": 1 }, "original_message": { "id": "abc" } })), None).is_none());
        assert!(reply_preview_from_value(None, None).is_none());
        assert!(reply_preview_from_value(Some(&serde_json::json!("not metadata")), None).is_none());
    }

    #[test]
    fn every_kind_of_kick_event_is_a_card() {
        let sub = serde_json::json!({ "username": "bigkhernandez", "months": 21 });
        assert_eq!(describe_action("SubscriptionEvent", &sub).map(|(_, l)| l).as_deref(), Some("bigkhernandez subscribed - 21 months"));
        let card = action_card("SubscriptionEvent", &sub).unwrap();
        assert_eq!((card.kind.as_deref(), card.author.as_deref(), card.title.as_deref()), (Some("subscribe"), Some("bigkhernandez"), Some("21")));
        // The first month has none to say.
        assert_eq!(action_card("SubscriptionEvent", &serde_json::json!({ "username": "x", "months": 1 })).unwrap().title, None);
        assert_eq!(action_card("SubscriptionEvent", &serde_json::json!({ "username": "x" })).unwrap().title, None);

        let ban = serde_json::json!({ "user": { "username": "troll" }, "banned_by": { "username": "mod" } });
        let card = action_card("App\\Events\\UserBannedEvent", &ban).unwrap();
        assert_eq!((card.kind.as_deref(), card.author.as_deref(), card.title.as_deref(), card.description.as_deref()),
            (Some("moderation"), Some("troll"), Some("was banned"), Some("mod")));
        let timeout = serde_json::json!({ "user": { "username": "troll" }, "expires_at": "2026-10-11T00:10:00Z" });
        let card = action_card("UserBannedEvent", &timeout).unwrap();
        assert_eq!((card.title.as_deref(), card.description.as_deref()), (Some("was timed out"), None));
        let back = action_card("UserUnbannedEvent", &serde_json::json!({ "user": { "username": "troll" } })).unwrap();
        assert_eq!((back.author.as_deref(), back.title.as_deref()), (Some("troll"), Some("is allowed back")));
        // Nobody named is nobody to draw a card about.
        assert!(action_card("UserBannedEvent", &serde_json::json!({})).is_none());
    }

    #[tokio::test]
    async fn the_stream_and_the_room_are_cards_too() {
        let state = testkit::simulated_daemon("event-cards");
        let mut watched = testkit::watching_odablock();
        feed(&state, &mut watched, "App\\Events\\StreamerIsLive", serde_json::json!({}));
        feed(&state, &mut watched, "App\\Events\\StopStreamBroadcast", serde_json::json!({}));
        feed(&state, &mut watched, "App\\Events\\ChatroomClearEvent", serde_json::json!({}));
        feed(&state, &mut watched, "SubscriptionEvent", serde_json::json!({ "username": "newcomer", "months": 3 }));
        let rows = state.store.get_backlog(&crate::model::buffer_id("kick:tester", "odablock"), 0, 50).unwrap();
        let cards: Vec<_> = rows.iter().map(|m| (m.kind.as_str(), m.embeds[0].kind.as_deref().unwrap(), m.embeds[0].title.as_deref().unwrap())).collect();
        assert_eq!(cards, vec![
            ("stream", "stream", "went live"),
            ("stream", "stream", "ended the stream"),
            ("moderation", "moderation", "cleared the chat"),
            ("sub", "subscribe", "3"),
        ]);
        assert_eq!(rows[0].embeds[0].icon.as_deref(), Some("live"));
        assert_eq!(rows[1].embeds[0].icon.as_deref(), Some("offline"));
        assert_eq!(rows[0].embeds[0].author.as_deref(), Some("odablock"));
    }
}
