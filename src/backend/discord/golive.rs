//! Go Live: sharing a screen into a Discord voice channel or call.
//!
//! A stream is not a flag on the voice connection already open. It is a
//! second connection of its own - Discord answers the request with a
//! different RTC server, a different token and a different session - and the
//! video goes there. That is why none of this touches the channel connection
//! songbird holds, and why it has to be hand-rolled: songbird has no video
//! path at all.
//!
//! The gateway half is here: asking for a stream, watching somebody else's,
//! pausing one, and ending it. What comes back arrives as ordinary dispatches
//! (see gateway.rs), in the same two-halves-of-a-handshake shape a voice
//! connection already uses - a `STREAM_CREATE` naming the stream, and a
//! `STREAM_SERVER_UPDATE` carrying the endpoint and token, in either order.

use crate::state::AppState;
use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Which conversation a stream belongs to, in Discord's own notation.
///
/// `guild:<guild>:<channel>:<user>` in a server, `call:<channel>:<user>` in a
/// direct message - and the difference is not cosmetic: the key is what every
/// later frame names the stream by, and a viewer asking to watch sends it
/// back verbatim. A key built with the wrong shape names a stream nobody has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamKey {
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub user_id: String,
}

impl StreamKey {
    pub fn to_wire(&self) -> String {
        match &self.guild_id {
            Some(guild) => format!("guild:{guild}:{}:{}", self.channel_id, self.user_id),
            None => format!("call:{}:{}", self.channel_id, self.user_id),
        }
    }

    /// Reads one back, for the dispatches that name a stream rather than
    /// describing it.
    pub fn parse(key: &str) -> Option<StreamKey> {
        let parts: Vec<&str> = key.split(':').collect();
        match parts.as_slice() {
            ["guild", guild, channel, user] if !guild.is_empty() && !channel.is_empty() && !user.is_empty() => {
                Some(StreamKey {
                    guild_id: Some((*guild).to_string()),
                    channel_id: (*channel).to_string(),
                    user_id: (*user).to_string(),
                })
            }
            ["call", channel, user] if !channel.is_empty() && !user.is_empty() => Some(StreamKey {
                guild_id: None,
                channel_id: (*channel).to_string(),
                user_id: (*user).to_string(),
            }),
            _ => None,
        }
    }

    /// Whose stream this is, which is the only part a roster needs.
    pub fn owner(key: &str) -> Option<String> {
        StreamKey::parse(key).map(|k| k.user_id)
    }
}

/// The gateway opcodes Go Live uses.
///
/// Named rather than written as numbers at the call sites: these differ by
/// one from each other and mean "start broadcasting", "stop", "start
/// watching" - not mistakes to make silently.
///
/// **Unverified against a live gateway.** Discord documents none of this and
/// the user gateway answers an opcode it does not know by ignoring the frame,
/// with no error - so a wrong number here is a Go Live button that does
/// nothing at all and says nothing about why. These come from the shape of
/// the protocol as other clients use it, and the first live run is what
/// confirms them. If a stream never arrives, this constant is the first place
/// to look.
pub const OP_STREAM_CREATE: u64 = 18;
pub const OP_STREAM_DELETE: u64 = 19;
pub const OP_STREAM_WATCH: u64 = 20;
pub const OP_STREAM_PING: u64 = 21;
pub const OP_STREAM_SET_PAUSED: u64 = 22;

/// Asks Discord to open a stream for this account in a conversation.
///
/// The account has to already be in the voice channel: a stream is carried
/// alongside a voice connection rather than instead of one, and asking for
/// one from outside gets a frame Discord ignores without saying why.
pub fn start(state: &AppState, account_id: &str, guild_id: Option<&str>, channel_id: &str) -> Result<()> {
    let sender = state.runtime.discord_gateway_sender(account_id).context("account is not connected")?;
    sender.send(
        json!({
            "op": OP_STREAM_CREATE,
            "d": {
                // "guild" and "call" here rather than the key's own prefix,
                // which is the same distinction spelled a second way.
                "type": if guild_id.is_some() { "guild" } else { "call" },
                "guild_id": guild_id,
                "channel_id": channel_id,
                // Left to Discord. A client that names a region picks one for
                // the viewers as well as for itself, and has no idea where
                // they are.
                "preferred_region": null,
            }
        })
        .to_string(),
    )?;
    Ok(())
}

/// Stops broadcasting.
pub fn stop(state: &AppState, account_id: &str, stream_key: &str) -> Result<()> {
    let sender = state.runtime.discord_gateway_sender(account_id).context("account is not connected")?;
    sender.send(json!({ "op": OP_STREAM_DELETE, "d": { "stream_key": stream_key } }).to_string())?;
    Ok(())
}

/// Says whether the picture is still moving.
///
/// Discord uses this to stop sending a stream nobody is looking at, and to
/// show viewers a paused badge rather than a frozen frame they will read as a
/// broken connection.
pub fn set_paused(state: &AppState, account_id: &str, stream_key: &str, paused: bool) -> Result<()> {
    let sender = state.runtime.discord_gateway_sender(account_id).context("account is not connected")?;
    sender
        .send(json!({ "op": OP_STREAM_SET_PAUSED, "d": { "stream_key": stream_key, "paused": paused } }).to_string())?;
    Ok(())
}

/// Asks to watch somebody else's stream.
pub fn watch(state: &AppState, account_id: &str, stream_key: &str) -> Result<()> {
    let sender = state.runtime.discord_gateway_sender(account_id).context("account is not connected")?;
    sender.send(json!({ "op": OP_STREAM_WATCH, "d": { "stream_key": stream_key } }).to_string())?;
    Ok(())
}

/// Keeps a stream this account is watching alive.
pub fn ping(state: &AppState, account_id: &str, stream_key: &str) -> Result<()> {
    let sender = state.runtime.discord_gateway_sender(account_id).context("account is not connected")?;
    sender.send(json!({ "op": OP_STREAM_PING, "d": { "stream_key": stream_key } }).to_string())?;
    Ok(())
}

/// The two halves of a stream's handshake, as they arrive.
///
/// The same shape the voice handshake uses and for the same reason: the
/// dispatch naming the stream and the one carrying its server arrive in
/// either order, and neither is usable alone.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct PendingStream {
    pub stream_key: Option<String>,
    pub rtc_server_id: Option<String>,
    pub endpoint: Option<String>,
    pub token: Option<String>,
    /// Whether Discord says the stream is paused - a stream created while
    /// nobody is watching starts paused, and sending into a paused stream is
    /// sending into nothing.
    pub paused: bool,
}

impl PendingStream {
    pub fn complete(&self) -> bool {
        self.stream_key.is_some() && self.endpoint.is_some() && self.token.is_some()
    }

    /// Takes in whatever a `STREAM_CREATE` or `STREAM_SERVER_UPDATE` said.
    ///
    /// Both may name the stream and only one carries the server, so this
    /// merges rather than replaces: a second dispatch that mentioned nothing
    /// new must not undo the first.
    pub fn absorb(&mut self, d: &Value) {
        if let Some(key) = d["stream_key"].as_str() {
            self.stream_key = Some(key.to_string());
        }
        if let Some(id) = d["rtc_server_id"].as_str() {
            self.rtc_server_id = Some(id.to_string());
        }
        if let Some(endpoint) = d["endpoint"].as_str().filter(|e| !e.is_empty()) {
            self.endpoint = Some(endpoint.to_string());
        }
        if let Some(token) = d["token"].as_str() {
            self.token = Some(token.to_string());
        }
        if let Some(paused) = d["paused"].as_bool() {
            self.paused = paused;
        }
    }
}

/// Where each account's half-built stream handshakes are kept, by stream key.
///
/// Process-wide rather than on the runtime for the same reason the voice
/// handshake's own store is: it exists only between two dispatches arriving
/// and the connection opening, and putting a transient of that kind in the
/// runtime means remembering to sweep it when an account goes.
fn pending() -> &'static std::sync::Mutex<std::collections::HashMap<String, PendingStream>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, PendingStream>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

fn slot(account_id: &str, stream_key: &str) -> String {
    format!("{account_id}|{stream_key}")
}

/// Takes in one of the stream dispatches.
///
/// Ours and everybody else's arrive here: a `STREAM_CREATE` for somebody else
/// in the channel is how a client learns there is something to watch, and is
/// passed to frontends rather than acted on. Only this account's own stream
/// has a handshake to complete.
pub async fn note_stream(state: &AppState, account_id: &str, dispatch: &str, d: &Value) {
    // Logged whole, at a level somebody will actually be running with. None
    // of this is documented, so the first time a real stream goes past is the
    // only chance to see what Discord actually sends - and a dispatch read
    // once from a log beats a field guessed at twice.
    tracing::info!("discord[{account_id}]: {dispatch} {}", redacted(d));

    let Some(stream_key) = d["stream_key"].as_str() else { return };
    let owner = StreamKey::owner(stream_key);
    let own = state.accounts.get_discord(account_id).map(|a| a.user_id);
    let mine = matches!((&owner, &own), (Some(o), Some(u)) if o == u);

    // Said to the window whoever it belongs to. A stream somebody else starts
    // is a thing to offer to watch; one of ours is a thing to show as live.
    state.events.emit(
        "discordStream",
        json!({
            "accountId": account_id,
            "streamKey": stream_key,
            "userId": owner,
            "own": mine,
            "paused": d["paused"].as_bool(),
            "viewers": d["viewer_ids"].as_array().map(|v| v.len()),
            "gone": false,
        }),
    );

    if !mine {
        return;
    }
    let ready = {
        let mut all = pending().lock().unwrap();
        let entry = all.entry(slot(account_id, stream_key)).or_default();
        entry.absorb(d);
        entry.complete().then(|| entry.clone())
    };
    let Some(ready) = ready else {
        tracing::debug!("discord[{account_id}]: {dispatch} for {stream_key}, still waiting for the other half");
        return;
    };
    tracing::info!("discord[{account_id}]: stream {stream_key} is ready to connect");

    // The account's own session, not a new one: a stream is one account in
    // one place sending a second thing, and it identifies with the session
    // the voice connection already made.
    let Some(session_id) = state.voice.session_id(account_id) else {
        tracing::warn!("discord[{account_id}]: a stream arrived with no voice session to attach it to");
        return;
    };
    let Some(user) = own else { return };
    // The server the stream is on, which is its own rather than the guild's.
    let server_id = ready
        .rtc_server_id
        .clone()
        .or_else(|| StreamKey::parse(stream_key).map(|k| k.guild_id.unwrap_or(k.channel_id)))
        .unwrap_or_default();

    match super::streamconn::connect(
        state,
        account_id,
        ready.endpoint.as_deref().unwrap_or_default(),
        ready.token.as_deref().unwrap_or_default(),
        &server_id,
        &session_id,
        &user,
    )
    .await
    {
        Ok(sender) => {
            senders().lock().unwrap().insert(account_id.to_string(), sender);
            state.events.emit(
                "discordStream",
                json!({ "accountId": account_id, "streamKey": stream_key, "own": true, "ready": true }),
            );
            // A stream created while nobody is watching starts paused, and
            // sending into a paused stream is sending into nothing - so this
            // says outright that there is a picture to have.
            let _ = set_paused(state, account_id, stream_key, false);
        }
        Err(e) => {
            tracing::warn!("discord[{account_id}]: the stream connection failed: {e:#}");
            state.events.emit(
                "discordStream",
                json!({ "accountId": account_id, "streamKey": stream_key, "own": true, "error": e.to_string() }),
            );
        }
    }
}

/// The live stream connections, by account.
///
/// One at a time: Discord's own client shares one screen, and a second stream
/// from one account would need a second connection and a second key for no
/// gain anybody has asked for.
fn senders() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<super::streamconn::StreamSender>>>
{
    static SENDERS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<super::streamconn::StreamSender>>>,
    > = std::sync::OnceLock::new();
    SENDERS.get_or_init(Default::default)
}

/// Puts one encoded frame on the wire.
///
/// The encoding happens in the window - Chromium has the encoders and this
/// process has none - so what arrives here is already a VP8 frame and all
/// that is left is to packetise, seal and send it. The same division the
/// Matrix calls draw from the other side.
pub async fn send_frame(account_id: &str, frame: &[u8], timestamp_micros: i64) -> Result<()> {
    let sender = senders().lock().unwrap().get(account_id).cloned();
    let sender = sender.context("no stream is running for this account")?;
    sender.send_frame(frame, timestamp_micros).await
}

/// Whether this account has a stream connection ready for frames.
pub fn sending(account_id: &str) -> bool {
    senders().lock().unwrap().contains_key(account_id)
}

/// Closes the connection, without telling the gateway - `stop` does that.
pub fn close(account_id: &str) {
    if let Some(sender) = senders().lock().unwrap().remove(account_id) {
        sender.stop();
    }
}

/// A dispatch with its credentials taken out.
///
/// `STREAM_SERVER_UPDATE` carries the token that opens the stream's
/// connection. It is exactly as much of a secret as a password, and a log is
/// the one place it must never be - so the field is replaced rather than
/// trimmed, which also leaves the log saying that there *was* one.
fn redacted(d: &Value) -> String {
    let mut copy = d.clone();
    if let Some(object) = copy.as_object_mut() {
        for secret in ["token", "access_token"] {
            if object.contains_key(secret) {
                object.insert(secret.to_string(), Value::from("<redacted>"));
            }
        }
    }
    copy.to_string()
}

/// A stream that has ended, ours or anybody's.
pub fn note_stream_gone(state: &AppState, account_id: &str, d: &Value) {
    tracing::info!("discord[{account_id}]: STREAM_DELETE {}", redacted(d));
    let Some(stream_key) = d["stream_key"].as_str() else { return };
    pending().lock().unwrap().remove(&slot(account_id, stream_key));
    state.events.emit(
        "discordStream",
        json!({
            "accountId": account_id,
            "streamKey": stream_key,
            "userId": StreamKey::owner(stream_key),
            "gone": true,
        }),
    );
}

/// What is known about one of this account's streams, if anything.
pub fn held(account_id: &str, stream_key: &str) -> Option<PendingStream> {
    pending().lock().unwrap().get(&slot(account_id, stream_key)).cloned()
}

/// Forgets everything held for an account.
pub fn forget(account_id: &str) {
    let prefix = format!("{account_id}|");
    pending().lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
    close(account_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is what every later frame names the stream by, and a viewer
    /// sends it back verbatim - so the two shapes have to be exact.
    #[test]
    fn a_key_says_which_kind_of_conversation_it_is() {
        let in_guild = StreamKey {
            guild_id: Some("111".into()),
            channel_id: "222".into(),
            user_id: "333".into(),
        };
        assert_eq!(in_guild.to_wire(), "guild:111:222:333");

        let in_dm = StreamKey { guild_id: None, channel_id: "222".into(), user_id: "333".into() };
        assert_eq!(in_dm.to_wire(), "call:222:333");

        // And back again, unchanged.
        assert_eq!(StreamKey::parse("guild:111:222:333"), Some(in_guild));
        assert_eq!(StreamKey::parse("call:222:333"), Some(in_dm));
    }

    /// A roster only ever wants the owner, and asking for it must not depend
    /// on knowing which shape the key is.
    #[test]
    fn the_owner_comes_out_of_either_shape() {
        assert_eq!(StreamKey::owner("guild:111:222:333").as_deref(), Some("333"));
        assert_eq!(StreamKey::owner("call:222:333").as_deref(), Some("333"));
        assert_eq!(StreamKey::owner("nonsense"), None);
    }

    /// Anything else names a stream nobody has, and is better refused than
    /// half-read - a key with an empty field parses as a key to a channel
    /// called "".
    #[test]
    fn a_malformed_key_is_not_a_key() {
        assert_eq!(StreamKey::parse(""), None);
        assert_eq!(StreamKey::parse("guild:111:222"), None, "too few parts for a guild");
        assert_eq!(StreamKey::parse("call:222:333:444"), None, "too many for a call");
        assert_eq!(StreamKey::parse("guild::222:333"), None, "an empty guild id");
        assert_eq!(StreamKey::parse("call:222:"), None, "an empty user id");
        assert_eq!(StreamKey::parse("stream:222:333"), None, "a prefix that is neither");
    }

    /// The two dispatches arrive in either order and neither is usable
    /// alone, so absorbing one must not undo the other.
    #[test]
    fn the_two_halves_of_a_handshake_merge_in_either_order() {
        let created = json!({ "stream_key": "call:222:333", "rtc_server_id": "rtc-9", "paused": true });
        let server = json!({ "stream_key": "call:222:333", "endpoint": "eu.discord.media", "token": "t0k" });

        let mut first = PendingStream::default();
        first.absorb(&created);
        assert!(!first.complete(), "a stream with no server to reach is not ready");
        first.absorb(&server);
        assert!(first.complete());

        let mut second = PendingStream::default();
        second.absorb(&server);
        second.absorb(&created);
        assert_eq!(first, second, "the order the two arrive in cannot matter");

        // A stream created while nobody is watching starts paused, and
        // sending into a paused stream is sending into nothing.
        assert!(first.paused);

        // A later dispatch mentioning nothing new leaves what is held alone.
        let mut held = first.clone();
        held.absorb(&json!({}));
        assert_eq!(held, first);

        // An empty endpoint is Discord saying the server has gone away, not
        // an endpoint called "".
        let mut dropped = first.clone();
        dropped.absorb(&json!({ "endpoint": "" }));
        assert_eq!(dropped.endpoint, first.endpoint);
    }
    /// The stream server's token opens the stream's connection and is as
    /// much of a secret as a password. A log is the one place it must never
    /// reach - and the field is replaced rather than removed, so the log
    /// still says there was one.
    #[test]
    fn a_logged_dispatch_has_no_token_in_it() {
        let d = json!({
            "stream_key": "call:222:333",
            "endpoint": "eu.discord.media",
            "token": "a-real-secret",
            "paused": false
        });
        let line = redacted(&d);
        assert!(!line.contains("a-real-secret"), "{line}");
        assert!(line.contains("<redacted>"), "{line}");
        // And everything that is not a secret survives, or the log says
        // nothing worth reading.
        assert!(line.contains("call:222:333"), "{line}");
        assert!(line.contains("eu.discord.media"), "{line}");
        assert!(line.contains("paused"), "{line}");

        // A dispatch with nothing to hide is unchanged apart from key order.
        let plain = json!({ "stream_key": "call:1:2" });
        assert!(!redacted(&plain).contains("redacted"));
    }

}
