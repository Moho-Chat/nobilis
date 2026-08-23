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
use songbird::driver::{DecodeMode, DecodeConfig};
use songbird::{Config, ConnectionInfo, Driver, Event, EventContext, EventHandler};
use std::sync::Arc;
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

/// How a particular voice session is meant to behave.
///
/// Both of these are per-join rather than fixed policy. "Only empty channels"
/// is the right default against a server full of strangers, but it is a rule
/// about a place, not about voice: in a guild the user owns, being ejected the
/// moment they join to listen makes testing impossible. Likewise a session
/// that carries no audio should say so by being muted, and one that does
/// should not.
#[derive(Clone, Copy, Debug)]
pub struct VoiceOptions {
    /// Refuse an occupied channel, and leave if anyone arrives.
    pub solo: bool,
    /// Send microphone audio, rather than joining muted and deafened.
    pub transmit: bool,
}

impl Default for VoiceOptions {
    fn default() -> Self {
        // The cautious pair: an empty channel, and silence.
        Self { solo: true, transmit: false }
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
    options: Mutex<HashMap<String, VoiceOptions>>,
    /// The running microphone stream, held here because dropping it stops the
    /// capture; the driver reads from the buffer it feeds.
    captures: Mutex<HashMap<String, crate::backend::audio::Capture>>,
    /// The speakers this session plays other people through.
    playbacks: Mutex<HashMap<String, Arc<crate::backend::audio::Playback>>>,
}

/// Plays what everyone else says.
///
/// Songbird hands over each 20ms tick already decoded and per-speaker, so the
/// mixing is ours to do: several people talking at once is the normal case,
/// not an error, and their audio has to be summed rather than interleaved or
/// dropped. Saturating addition means a loud room clips rather than wrapping
/// around into noise.
struct Speakers {
    playback: Arc<crate::backend::audio::Playback>,
}

/// Sums what several people are saying into one stream.
///
/// Saturating rather than wrapping: a loud moment should clip, which sounds
/// like a loud moment, instead of wrapping around to the opposite extreme,
/// which sounds like a gunshot. Speakers whose packets were lost contribute
/// nothing rather than shortening the tick.
fn mix(voices: &[&[i16]]) -> Vec<i16> {
    let longest = voices.iter().map(|v| v.len()).max().unwrap_or(0);
    let mut mixed = vec![0i16; longest];
    for voice in voices {
        for (slot, sample) in mixed.iter_mut().zip(voice.iter()) {
            *slot = slot.saturating_add(*sample);
        }
    }
    mixed
}

#[async_trait::async_trait]
impl EventHandler for Speakers {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::VoiceTick(tick) = ctx else { return None };

        let voices: Vec<&[i16]> = tick.speaking.values().filter_map(|d| d.decoded_voice.as_deref()).collect();
        let mixed = mix(&voices);
        if !mixed.is_empty() {
            self.playback.push(&mixed);
        }
        None
    }
}

impl VoiceState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records how the next connection for this account should behave.
    pub fn set_options(&self, account_id: &str, options: VoiceOptions) {
        self.options.lock().unwrap().insert(account_id.to_string(), options);
    }

    /// The options in force, defaulting to the cautious pair if a session was
    /// established by something other than an explicit join.
    pub fn options(&self, account_id: &str) -> VoiceOptions {
        self.options.lock().unwrap().get(account_id).copied().unwrap_or_default()
    }

    /// The guild and channel this account is currently in voice in.
    pub fn current_channel(&self, account_id: &str) -> Option<(String, String)> {
        let pending = self.pending.lock().unwrap();
        let entry = pending.get(account_id)?;
        Some((entry.guild_id.clone()?, entry.channel_id.clone()?))
    }

    /// Every account with a live voice connection.
    pub fn connected_accounts(&self) -> Vec<String> {
        self.drivers.lock().unwrap().keys().cloned().collect()
    }

    /// Closes or opens the microphone of a live session.
    ///
    /// Reported rather than assumed: with no connection up there is nothing to
    /// mute, and a caller that believes it muted something that was never live
    /// would show the wrong thing.
    pub fn set_mic_muted(&self, account_id: &str, muted: bool) -> bool {
        let captures = self.captures.lock().unwrap();
        match captures.get(account_id) {
            Some(capture) => {
                capture.set_muted(muted);
                true
            }
            None => false,
        }
    }

    /// Peak microphone level of a transmitting session, for a level meter.
    pub fn input_level(&self, account_id: &str) -> Option<f32> {
        let captures = self.captures.lock().unwrap();
        captures.get(account_id).map(|c| *c.level.lock().unwrap())
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

    // Decoding has to be asked for: the default merely decrypts, which is
    // enough to know somebody is talking and not enough to hear them.
    let config = Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()));
    let mut driver = Driver::new(config);
    driver.connect(connection).await.context("opening the voice connection")?;

    let prefs = state.voice_prefs.get();
    crate::backend::audio::set_playback_muted(prefs.deafened);
    match crate::backend::audio::start_playback(prefs.output.as_deref()) {
        Ok(playback) => {
            let playback = Arc::new(playback);
            driver.add_global_event(
                songbird::CoreEvent::VoiceTick.into(),
                Speakers { playback: playback.clone() },
            );
            state.voice.playbacks.lock().unwrap().insert(account_id.to_string(), playback);
        }
        // No speakers is a worse call, not a failed one - and someone who
        // only wants to talk should still be able to.
        Err(e) => tracing::warn!("discord[{account_id}]: no audio output, joining deaf: {e:#}"),
    }

    if state.voice.options(account_id).transmit {
        match start_transmitting(&mut driver, prefs.input.as_deref(), prefs.mic_muted || prefs.deafened) {
            Ok(capture) => {
                state.voice.captures.lock().unwrap().insert(account_id.to_string(), capture);
                tracing::info!("discord[{account_id}]: transmitting microphone audio");
            }
            // A missing or busy microphone should not tear down a connection
            // that is otherwise fine; the session simply carries no audio.
            Err(e) => tracing::warn!("discord[{account_id}]: no microphone, joining silent: {e:#}"),
        }
    }

    state.voice.drivers.lock().unwrap().insert(account_id.to_string(), driver);
    Ok(())
}

/// Opens the microphone and hands it to the driver as a live track.
///
/// Songbird plays an input by reading from it, so the microphone is presented
/// as a byte stream of interleaved f32 samples at the rate Discord expects
/// (see `audio::MicSource`), which the raw adapter labels and the driver then
/// encodes to Opus.
fn start_transmitting(
    driver: &mut Driver,
    device_id: Option<&str>,
    muted: bool,
) -> Result<crate::backend::audio::Capture> {
    use crate::backend::audio::{self, MicSource, TARGET_CHANNELS, TARGET_RATE};

    let (source, sink) = MicSource::new();
    let capture = audio::start_capture(device_id, sink)?;
    // Joining already muted has to happen before any audio can be sent, not
    // after the connection settles.
    capture.set_muted(muted);
    let input = songbird::input::RawAdapter::new(source, TARGET_RATE, TARGET_CHANNELS as u32);
    let handle = driver.play_input(input.into());
    // Looping is meaningless for a live source, but an explicit play makes the
    // intent clear and surfaces a rejected track immediately.
    let _ = handle.play();
    Ok(capture)
}

/// Tears down a voice connection, if one is up.
pub async fn disconnect(state: &AppState, account_id: &str) {
    state.voice.captures.lock().unwrap().remove(account_id);
    state.voice.playbacks.lock().unwrap().remove(account_id);
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_speaker_is_carried_unchanged() {
        let voice = [100i16, -100, 0];
        assert_eq!(mix(&[&voice]), vec![100, -100, 0]);
    }

    #[test]
    fn several_people_talking_at_once_are_summed() {
        // The normal case in any group call, and the one that sounds wrong if
        // the audio is interleaved or the loudest speaker simply wins.
        let a = [100i16, 200];
        let b = [50i16, -200];
        assert_eq!(mix(&[&a, &b]), vec![150, 0]);
    }

    #[test]
    fn a_loud_room_clips_rather_than_wrapping() {
        // Wrapping turns a loud moment into a full-scale discontinuity, which
        // is heard as a bang rather than as loudness.
        let a = [i16::MAX, i16::MIN];
        let b = [i16::MAX, i16::MIN];
        assert_eq!(mix(&[&a, &b]), vec![i16::MAX, i16::MIN]);
    }

    #[test]
    fn a_lost_packet_does_not_shorten_the_tick() {
        // Songbird reports a speaker with no decoded audio when a packet is
        // lost. Taking the shortest length would cut everyone else off.
        let full = [1i16; 960];
        let partial = [1i16; 10];
        assert_eq!(mix(&[&full, &partial]).len(), 960);
    }

    #[test]
    fn silence_is_silence() {
        assert!(mix(&[]).is_empty());
    }
}
