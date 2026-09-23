//! Discord voice connections.
//!
//! The main gateway (see gateway.rs) negotiates a voice session and hands over
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
use std::collections::{HashMap, HashSet};
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
    ///
    /// A guild is deliberately not required. A one-to-one call happens in a DM
    /// channel, which belongs to no guild at all, and Discord sends its voice
    /// state and server update with `guild_id` absent.
    fn complete(&self) -> bool {
        self.channel_id.is_some()
            && self.session_id.is_some()
            && self.endpoint.is_some()
            && self.token.is_some()
    }

    /// What the voice websocket calls the server id.
    ///
    /// For a guild call it is the guild; for a DM call there is no guild and
    /// the channel itself plays that part. Songbird's field is named after the
    /// commoner case, but the protocol only cares that this matches what the
    /// voice server was told.
    fn server_id(&self) -> Option<&str> {
        self.guild_id.as_deref().or(self.channel_id.as_deref())
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
    captures: Mutex<HashMap<String, crate::audio::Capture>>,
    /// The speakers this session plays other people through.
    playbacks: Mutex<HashMap<String, Arc<crate::audio::Playback>>>,
    /// Who is talking right now, per account.
    speaking: Mutex<HashMap<String, Arc<SpeakingTracker>>>,
}

/// Who is audible in one call, and how loudly.
///
/// Two maps rather than one, because Discord answers the question in two
/// halves and at different times. Audio arrives keyed by SSRC - a number the
/// voice server assigns to a stream, meaningless on its own - while who that
/// SSRC belongs to arrives separately, once, when they first speak. So the
/// identities are learned as they are announced and kept until the person
/// disconnects, and every tick is looked up against them.
///
/// An SSRC whose owner has not been announced yet is heard but not named, and
/// is deliberately not reported under a placeholder: a tile in the call view
/// labelled "somebody" would be worse than the tile arriving a moment late,
/// which is what happens instead.
#[derive(Default)]
pub struct SpeakingTracker {
    /// SSRC to Discord user id, learned from SpeakingStateUpdate.
    owners: Mutex<HashMap<u32, String>>,
    /// SSRC to when it was last heard, and how loudly.
    ///
    /// Kept against the stream rather than the person, so audio that arrives
    /// before its announcement is not thrown away. Discord only names a
    /// stream when its owner starts speaking, and the announcement can be
    /// missed outright - by joining a call already in progress, or by
    /// registering for it a moment after connecting - which used to mean the
    /// audio was heard, discarded, and nobody ever lit up.
    heard: Mutex<HashMap<u32, (std::time::Instant, f32)>>,
}

/// How long after their last packet somebody still counts as talking.
///
/// Speech is not continuous - the gaps between words are real silence, and an
/// indicator that tracked the audio exactly would flicker on every syllable.
/// Long enough to bridge a pause, short enough to go out when they stop.
const SPEAKING_HOLD: std::time::Duration = std::time::Duration::from_millis(400);

impl SpeakingTracker {
    fn learn(&self, ssrc: u32, user_id: String) {
        // Once per speaker per call, and at info because it is the only
        // evidence that Discord is announcing these at all - which is
        // otherwise invisible from outside, since a missing announcement and
        // a quiet room look identical in the call view. Too cheap to hide
        // behind a log level nobody runs with.
        if self.owners.lock().unwrap().insert(ssrc, user_id.clone()) != Some(user_id.clone()) {
            tracing::info!("discord voice: stream {ssrc} is user {user_id}");
        }
    }

    fn forget(&self, ssrc: u32) {
        self.owners.lock().unwrap().remove(&ssrc);
        self.heard.lock().unwrap().remove(&ssrc);
    }

    fn heard_from(&self, ssrc: u32, peak: f32) {
        self.heard.lock().unwrap().insert(ssrc, (std::time::Instant::now(), peak));
    }

    /// Who is talking now, loudest first.
    ///
    /// `participants` is everyone else in the call. It is what lets a stream
    /// nobody has claimed still be attributed: if exactly one stream is
    /// unnamed and exactly one participant is unaccounted for, there is only
    /// one person it can be, and that is a deduction rather than a guess.
    /// Any less certain and it is left out, because a ring around the wrong
    /// person is worse than a ring around nobody.
    fn current(&self, participants: &[String]) -> Vec<(String, f32)> {
        let now = std::time::Instant::now();
        let owners = self.owners.lock().unwrap();
        let live: Vec<(u32, f32)> = self
            .heard
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, (at, _))| now.duration_since(*at) < SPEAKING_HOLD)
            .map(|(ssrc, (_, peak))| (*ssrc, *peak))
            .collect();

        let mut out: Vec<(String, f32)> = Vec::new();
        let mut unclaimed: Vec<f32> = Vec::new();
        for (ssrc, peak) in live {
            match owners.get(&ssrc) {
                Some(user) => out.push((user.clone(), peak)),
                None => unclaimed.push(peak),
            }
        }

        if unclaimed.len() == 1 {
            let named: HashSet<&String> = owners.values().collect();
            let mut candidates = participants.iter().filter(|p| !named.contains(p));
            if let (Some(only), None) = (candidates.next(), candidates.next()) {
                out.push((only.clone(), unclaimed[0]));
            }
        }

        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out
    }
}

/// The loudest sample in a tick, as a fraction of full scale.
///
/// A peak rather than a mean: what this drives is a "somebody is talking"
/// indicator, and the mean over 20ms of speech - which is mostly the quiet
/// parts of a waveform - reads as near-silence even when it is not.
fn peak_of(samples: &[i16]) -> f32 {
    samples.iter().map(|s| (s.unsigned_abs() as f32) / 32768.0).fold(0.0, f32::max)
}

#[cfg(test)]
mod speaking_tests {
    use super::*;

    /// Nobody else in the call, so nothing can be deduced.
    const ALONE: &[String] = &[];

    fn who(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_stream_nobody_has_claimed_names_nobody_on_its_own() {
        let t = SpeakingTracker::default();
        t.heard_from(1234, 0.9);
        assert!(t.current(ALONE).is_empty(), "an unowned SSRC must not invent a speaker");
    }

    #[test]
    fn somebody_who_just_spoke_is_talking() {
        let t = SpeakingTracker::default();
        t.learn(7, "42".into());
        t.heard_from(7, 0.5);
        assert_eq!(t.current(ALONE), vec![("42".to_string(), 0.5)]);
    }

    #[test]
    fn the_loudest_comes_first() {
        let t = SpeakingTracker::default();
        t.learn(1, "quiet".into());
        t.learn(2, "loud".into());
        t.heard_from(1, 0.1);
        t.heard_from(2, 0.8);
        assert_eq!(t.current(ALONE).first().unwrap().0, "loud");
    }

    #[test]
    fn leaving_stops_you_talking() {
        let t = SpeakingTracker::default();
        t.learn(7, "42".into());
        t.heard_from(7, 0.5);
        t.forget(7);
        assert!(t.current(ALONE).is_empty(), "a disconnect must not leave them stuck mid-word");
    }

    #[test]
    fn silence_falls_out_of_the_hold() {
        let t = SpeakingTracker::default();
        t.learn(7, "42".into());
        // Older than the hold, which is what a pause between words is not.
        t.heard
            .lock()
            .unwrap()
            .insert(7, (std::time::Instant::now() - SPEAKING_HOLD * 2, 0.5));
        assert!(t.current(ALONE).is_empty());
    }

    #[test]
    fn the_only_person_it_could_be_is_named_without_an_announcement() {
        // The whole point: a two-person call where Discord never said whose
        // stream this is. There is one other person in the room.
        let t = SpeakingTracker::default();
        t.heard_from(1234, 0.9);
        assert_eq!(t.current(&who(&["them"])), vec![("them".to_string(), 0.9)]);
    }

    #[test]
    fn two_could_be_either_so_neither_is_named() {
        let t = SpeakingTracker::default();
        t.heard_from(1234, 0.9);
        assert!(
            t.current(&who(&["one", "other"])).is_empty(),
            "a ring on the wrong person is worse than a ring on nobody"
        );
    }

    #[test]
    fn two_unclaimed_streams_name_nobody_even_with_one_candidate() {
        // Two people talking and only one unaccounted for: whichever way it
        // is paired, one of them would be wrong.
        let t = SpeakingTracker::default();
        t.heard_from(1, 0.9);
        t.heard_from(2, 0.4);
        assert!(t.current(&who(&["them"])).is_empty());
    }

    #[test]
    fn deduction_only_considers_people_not_already_spoken_for() {
        // Three in the room, two of them announced - so the unclaimed stream
        // belongs to the third, and saying so is forced rather than guessed.
        let t = SpeakingTracker::default();
        t.learn(1, "known".into());
        t.learn(2, "also-known".into());
        t.heard_from(9, 0.7);
        assert_eq!(
            t.current(&who(&["known", "also-known", "silent-until-now"])),
            vec![("silent-until-now".to_string(), 0.7)]
        );
    }

    #[test]
    fn an_announcement_that_arrives_late_still_names_the_audio() {
        // Audio first, name second - the order that used to lose the stream
        // entirely, because it was discarded before anyone claimed it.
        let t = SpeakingTracker::default();
        t.heard_from(7, 0.6);
        t.learn(7, "42".into());
        assert_eq!(t.current(ALONE), vec![("42".to_string(), 0.6)]);
    }

    #[test]
    fn a_peak_is_the_loudest_sample_not_the_average() {
        // One loud sample among quiet ones is somebody talking, and a mean
        // would report it as near-silence.
        assert!((peak_of(&[0, 0, 0, 16384]) - 0.5).abs() < 0.01);
        assert_eq!(peak_of(&[]), 0.0);
    }
}

/// Plays what everyone else says.
///
/// Songbird hands over each 20ms tick already decoded and per-speaker, so the
/// mixing is ours to do: several people talking at once is the normal case,
/// not an error, and their audio has to be summed rather than interleaved or
/// dropped. Saturating addition means a loud room clips rather than wrapping
/// around into noise.
struct Speakers {
    /// None on a machine whose audio output would not open. The call still
    /// runs - you can talk, and you can see who else is - it is just silent.
    playback: Option<Arc<crate::audio::Playback>>,
    tracker: Arc<SpeakingTracker>,
}

/// Learns which stream belongs to whom, and forgets it when they leave.
///
/// A separate handler because these are separate events from the audio and
/// arrive on their own schedule - the mapping is announced once, when someone
/// first speaks, long before or after any particular tick.
struct Identities {
    tracker: Arc<SpeakingTracker>,
}

#[async_trait::async_trait]
impl EventHandler for Identities {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                if let Some(user) = speaking.user_id {
                    self.tracker.learn(speaking.ssrc, user.0.to_string());
                }
            }
            EventContext::ClientDisconnect(who) => {
                // Keyed by user rather than SSRC here, so the whole entry goes
                // rather than leaving them stuck mid-word in the view.
                let user = who.user_id.0.to_string();
                let ssrcs: Vec<u32> = self
                    .tracker
                    .owners
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, u)| **u == user)
                    .map(|(s, _)| *s)
                    .collect();
                // Nothing to forget by SSRC if they never spoke - which is
                // exactly when the deduction above was covering for them, and
                // it stops on its own once they are out of the roster.
                for ssrc in ssrcs {
                    self.tracker.forget(ssrc);
                }
            }
            _ => {}
        }
        None
    }
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

        // Noted per speaker before the mix throws the identities away - which
        // is the only place they exist, since what comes out of mix() is one
        // stream that nobody in particular said.
        for (ssrc, data) in &tick.speaking {
            if let Some(voice) = data.decoded_voice.as_deref() {
                self.tracker.heard_from(*ssrc, peak_of(voice));
            }
        }

        let Some(playback) = self.playback.as_ref() else { return None };
        let voices: Vec<&[i16]> = tick.speaking.values().filter_map(|d| d.decoded_voice.as_deref()).collect();
        let mixed = mix(&voices);
        if !mixed.is_empty() {
            playback.push(&mixed);
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

    /// Where this account is in voice: its guild, if any, and its channel.
    ///
    /// The guild is optional because a one-to-one call has none - it happens
    /// in a DM channel. Callers that need something to identify the session by
    /// should use the channel, which every call has.
    pub fn current_channel(&self, account_id: &str) -> Option<(Option<String>, String)> {
        let pending = self.pending.lock().unwrap();
        let entry = pending.get(account_id)?;
        Some((entry.guild_id.clone(), entry.channel_id.clone()?))
    }

    /// The gateway session this account's voice connection was made with.
    ///
    /// A Go Live stream identifies with the same session id the voice
    /// connection did - it is one account in one place, sending two things -
    /// so the stream connection has to ask for it rather than start its own.
    pub fn session_id(&self, account_id: &str) -> Option<String> {
        self.pending.lock().unwrap().get(account_id)?.session_id.clone()
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

    /// What has been heard from everyone else since this was last asked.
    pub fn output_level(&self, account_id: &str) -> Option<(f32, u64)> {
        let playbacks = self.playbacks.lock().unwrap();
        playbacks.get(account_id).map(|p| p.take_level())
    }

    /// Who is talking in this account's call right now, loudest first.
    pub fn speakers(&self, account_id: &str, participants: &[String]) -> Vec<(String, f32)> {
        let speaking = self.speaking.lock().unwrap();
        speaking.get(account_id).map(|t| t.current(participants)).unwrap_or_default()
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
        guild_id: GuildId(parse(info.server_id().context("no channel to connect to")?, "server id")?),
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

    // One tracker per session, registered whether or not there are speakers to
    // play through: who is talking is worth knowing even on a machine with no
    // audio output, and the call view is the only thing that shows it.
    let tracker = Arc::new(SpeakingTracker::default());
    state.voice.speaking.lock().unwrap().insert(account_id.to_string(), tracker.clone());
    driver.add_global_event(
        songbird::CoreEvent::SpeakingStateUpdate.into(),
        Identities { tracker: tracker.clone() },
    );
    driver.add_global_event(
        songbird::CoreEvent::ClientDisconnect.into(),
        Identities { tracker: tracker.clone() },
    );

    let prefs = state.voice_prefs.get();
    crate::audio::set_playback_muted(prefs.deafened);
    let playback = match crate::audio::start_playback(prefs.output.as_deref()) {
        Ok(playback) => {
            let playback = Arc::new(playback);
            state.voice.playbacks.lock().unwrap().insert(account_id.to_string(), playback.clone());
            Some(playback)
        }
        // No speakers is a worse call, not a failed one - and someone who
        // only wants to talk should still be able to.
        Err(e) => {
            tracing::warn!("discord[{account_id}]: no audio output, joining deaf: {e:#}");
            None
        }
    };
    // Registered whether or not there is anywhere to play the audio, which is
    // what the tracker above promises. It used to sit inside the branch that
    // opened the speakers, so on a machine with no working output nobody ever
    // lit up as talking - and every tick is also where a stream is noticed at
    // all, not only where it is heard.
    driver.add_global_event(songbird::CoreEvent::VoiceTick.into(), Speakers { playback, tracker: tracker.clone() });

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
) -> Result<crate::audio::Capture> {
    use crate::audio::{self, MicSource, TARGET_CHANNELS, TARGET_RATE};

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
