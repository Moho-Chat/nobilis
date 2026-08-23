//! Discord voice connections.
//!
//! The main gateway (see discord.rs) negotiates a voice session and hands over
//! two halves of a handshake: a session id on VOICE_STATE_UPDATE, and an
//! endpoint plus token on VOICE_SERVER_UPDATE. They arrive in either order and
//! neither is usable alone, so this waits for both and then opens the actual
//! voice connection.
//!
//! The connection itself is Songbird's. Doing it by hand is no longer
//! reasonable: on top of the voice websocket, UDP hole punching and Opus, a
//! voice connection now has to negotiate DAVE, Discord's end-to-end
//! encryption, which became mandatory in 2026.

use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::{Config, ConnectionInfo, Driver};
use std::collections::HashMap;
use std::sync::Mutex;

/// What has been learned about a pending voice connection so far.
#[derive(Default, Clone, Debug)]
pub struct PendingHandshake {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub session_id: Option<String>,
    pub endpoint: Option<String>,
    pub token: Option<String>,
}

impl PendingHandshake {
    /// Both dispatches have arrived and the connection can be attempted.
    fn complete(&self) -> bool {
        self.guild_id.is_some()
            && self.channel_id.is_some()
            && self.session_id.is_some()
            && self.endpoint.is_some()
            && self.token.is_some()
    }
}

/// Live drivers and half-built handshakes, per account.
///
/// A Driver owns its own tasks and is not cloneable, so it lives here rather
/// than being passed around; dropping it is what tears the connection down.
#[derive(Default)]
pub struct VoiceState {
    pending: Mutex<HashMap<String, PendingHandshake>>,
    drivers: Mutex<HashMap<String, Driver>>,
}

impl VoiceState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Records the session id half of the handshake.
pub async fn note_voice_state(state: &AppState, account_id: &str, guild_id: Option<&str>, channel_id: Option<&str>, session_id: Option<&str>) {
    // Leaving clears everything rather than leaving a half-handshake that a
    // later, unrelated dispatch could complete.
    if channel_id.is_none() {
        state.voice.pending.lock().unwrap().remove(account_id);
        disconnect(state, account_id).await;
        return;
    }

    {
        let mut pending = state.voice.pending.lock().unwrap();
        let entry = pending.entry(account_id.to_string()).or_default();
        entry.guild_id = guild_id.map(String::from).or(entry.guild_id.clone());
        entry.channel_id = channel_id.map(String::from);
        entry.session_id = session_id.map(String::from).or(entry.session_id.clone());
    }
    try_connect(state, account_id).await;
}

/// Records the endpoint and token half.
pub async fn note_voice_server(state: &AppState, account_id: &str, guild_id: Option<&str>, endpoint: Option<&str>, token: Option<&str>) {
    {
        let mut pending = state.voice.pending.lock().unwrap();
        let entry = pending.entry(account_id.to_string()).or_default();
        entry.guild_id = guild_id.map(String::from).or(entry.guild_id.clone());
        // Discord sends the endpoint without a scheme and, on a voice server
        // move, as null - which means "wait for the next one" rather than
        // "connect to nothing".
        entry.endpoint = endpoint.map(String::from);
        entry.token = token.map(String::from);
    }
    try_connect(state, account_id).await;
}

/// Connects once both halves are in hand.
async fn try_connect(state: &AppState, account_id: &str) {
    let info = {
        let pending = state.voice.pending.lock().unwrap();
        let Some(entry) = pending.get(account_id) else { return };
        if !entry.complete() {
            return;
        }
        entry.clone()
    };

    match connect(state, account_id, &info).await {
        Ok(()) => {
            tracing::info!("discord[{account_id}]: voice connected");
            state.events.emit(
                "discordVoiceConnected",
                json!({ "accountId": account_id, "channelId": info.channel_id, "endpoint": info.endpoint }),
            );
        }
        Err(e) => {
            tracing::warn!("discord[{account_id}]: voice connection failed: {e:#}");
            state.events.emit(
                "discordVoiceError",
                json!({ "accountId": account_id, "error": e.to_string() }),
            );
        }
    }
}

async fn connect(state: &AppState, account_id: &str, info: &PendingHandshake) -> Result<()> {
    let config = state.accounts.get_discord(account_id).context("no such Discord account")?;

    let parse = |s: &str, what: &str| -> Result<std::num::NonZeroU64> {
        s.parse::<std::num::NonZeroU64>().map_err(|e| anyhow!("bad {what} {s:?}: {e}"))
    };

    let connection = ConnectionInfo {
        channel_id: ChannelId(parse(info.channel_id.as_ref().unwrap(), "channel id")?),
        guild_id: GuildId(parse(info.guild_id.as_ref().unwrap(), "guild id")?),
        user_id: UserId(parse(&config.user_id, "user id")?),
        session_id: info.session_id.clone().unwrap(),
        token: info.token.clone().unwrap(),
        endpoint: info.endpoint.clone().unwrap(),
    };

    let mut driver = Driver::new(Config::default());
    driver.connect(connection).await.context("opening the voice connection")?;
    state.voice.drivers.lock().unwrap().insert(account_id.to_string(), driver);
    Ok(())
}

/// Tears down a voice connection, if one is up.
pub async fn disconnect(state: &AppState, account_id: &str) {
    let driver = state.voice.drivers.lock().unwrap().remove(account_id);
    if let Some(mut driver) = driver {
        driver.leave();
        tracing::info!("discord[{account_id}]: voice disconnected");
    }
}

/// Whether a voice connection is currently up for this account.
pub fn is_connected(state: &AppState, account_id: &str) -> bool {
    state.voice.drivers.lock().unwrap().contains_key(account_id)
}
