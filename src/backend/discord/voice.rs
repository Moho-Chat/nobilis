//! Discord voice connections.
//!
//! The main gateway (see gateway.rs) negotiates a voice session and hands over
//! two halves of a handshake: a session id on VOICE_STATE_UPDATE, and an
//! endpoint plus token on VOICE_SERVER_UPDATE. They arrive in either order and
//! neither is usable alone, so this waits for both and then opens the actual
//! voice connection.
//!
//! The connection itself is `voiceconn`'s. It was songbird's until cameras
//! mattered: songbird never tells Discord it can receive video, so nobody's
//! camera ever arrived. See voiceconn.rs for what replaced it.

use crate::state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use super::voiceconn::{self, VideoQuality, Ended, Handshake, Media, VoiceConn};
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
    conns: Mutex<HashMap<String, Arc<VoiceConn>>>,
    /// Reconnection attempts since the last connection that held, per account.
    retries: Mutex<HashMap<String, u32>>,
    options: Mutex<HashMap<String, VoiceOptions>>,
    /// The running microphone stream, held here because dropping it stops the
    /// capture; the driver reads from the buffer it feeds.
    captures: Mutex<HashMap<String, crate::audio::Capture>>,
    /// The speakers this session plays other people through.
    playbacks: Mutex<HashMap<String, Arc<crate::audio::Playback>>>,
    /// Who is talking right now, per account.
    speaking: Mutex<HashMap<String, Arc<SpeakingTracker>>>,
    /// Accounts whose camera is on, and at what. Kept past a connection, so
    /// one that drops and comes back brings the camera back with it.
    cameras: Mutex<HashMap<String, VideoQuality>>,
    /// The microphone and output flags last told to the gateway, which a
    /// camera turned on has to repeat: the gateway takes the three together.
    flags: Mutex<HashMap<String, (bool, bool)>>,
    /// Accounts in a stage's audience. Their microphone is held shut whatever
    /// the mute button says: Discord would drop the audio, but the speaking
    /// ring would still light on everybody's screen.
    suppressed: Mutex<std::collections::HashSet<String>>,
    /// The mute button's own state, so leaving the audience restores it
    /// rather than opening a microphone somebody had closed.
    mic_wanted_muted: Mutex<HashMap<String, bool>>,    /// The loudest the microphone was after processing, since last asked:
    /// what is actually sent, which `input_level` - the raw capture - is not.
    sent_peaks: Mutex<HashMap<String, f32>>,
    /// Soundboard sounds waiting to be mixed into the call, as 48kHz stereo.
    effects: Mutex<HashMap<String, std::collections::VecDeque<i16>>>,
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
    pub(super) fn learn(&self, ssrc: u32, user_id: String) {
        // Once per speaker per call, and at info because it is the only
        // evidence that Discord is announcing these at all - which is
        // otherwise invisible from outside, since a missing announcement and
        // a quiet room look identical in the call view. Too cheap to hide
        // behind a log level nobody runs with.
        if self.owners.lock().unwrap().insert(ssrc, user_id.clone()) != Some(user_id.clone()) {
            tracing::info!("discord voice: stream {ssrc} is user {user_id}");
        }
    }

    pub(super) fn forget(&self, ssrc: u32) {
        self.owners.lock().unwrap().remove(&ssrc);
        self.heard.lock().unwrap().remove(&ssrc);
    }

    pub(super) fn heard_from(&self, ssrc: u32, peak: f32) {
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
pub(super) fn peak_of(samples: &[i16]) -> f32 {
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

/// Sums what several people are saying into one stream.
///
/// Saturating rather than wrapping: a loud moment should clip, which sounds
/// like a loud moment, instead of wrapping around to the opposite extreme,
/// which sounds like a gunshot. Speakers whose packets were lost contribute
/// nothing rather than shortening the tick.
pub(super) fn mix(voices: &[&[i16]]) -> Vec<i16> {
    let longest = voices.iter().map(|v| v.len()).max().unwrap_or(0);
    let mut mixed = vec![0i16; longest];
    for voice in voices {
        for (slot, sample) in mixed.iter_mut().zip(voice.iter()) {
            *slot = slot.saturating_add(*sample);
        }
    }
    mixed
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
        self.conns.lock().unwrap().keys().cloned().collect()
    }

    /// Closes or opens the microphone of a live session.
    ///
    /// Reported rather than assumed: with no connection up there is nothing to
    /// mute, and a caller that believes it muted something that was never live
    /// would show the wrong thing.
    pub fn set_mic_muted(&self, account_id: &str, muted: bool) -> bool {
        self.mic_wanted_muted.lock().unwrap().insert(account_id.to_string(), muted);
        let held = self.suppressed.lock().unwrap().contains(account_id);
        let captures = self.captures.lock().unwrap();
        match captures.get(account_id) {
            Some(capture) => {
                capture.set_muted(muted || held);
                true
            }
            None => false,
        }
    }

    /// Moves this account between a stage's audience and its speakers, as
    /// far as its own microphone is concerned.
    pub fn set_suppressed(&self, account_id: &str, suppressed: bool) {
        let changed = if suppressed {
            self.suppressed.lock().unwrap().insert(account_id.to_string())
        } else {
            self.suppressed.lock().unwrap().remove(account_id)
        };
        if !changed {
            return;
        }
        let wanted = self.mic_wanted_muted.lock().unwrap().get(account_id).copied().unwrap_or(false);
        if let Some(capture) = self.captures.lock().unwrap().get(account_id) {
            capture.set_muted(wanted || suppressed);
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

    pub fn note_sent_peak(&self, account_id: &str, peak: f32) {
        let mut all = self.sent_peaks.lock().unwrap();
        let slot = all.entry(account_id.to_string()).or_insert(0.0);
        *slot = slot.max(peak);
    }

    /// The loudest sent since last asked, and reset.
    pub fn take_sent_peak(&self, account_id: &str) -> f32 {
        self.sent_peaks.lock().unwrap().insert(account_id.to_string(), 0.0).unwrap_or(0.0)
    }

    /// A soundboard sound to play into this account's call. Sounds that
    /// overlap are summed, as two people pressing at once would be heard.
    pub fn queue_effect(&self, account_id: &str, pcm: &[i16]) {
        let mut all = self.effects.lock().unwrap();
        let queue = all.entry(account_id.to_string()).or_default();
        for (i, sample) in pcm.iter().enumerate() {
            match queue.get_mut(i) {
                Some(slot) => *slot = slot.saturating_add(*sample),
                None => queue.push_back(*sample),
            }
        }
    }

    /// The next stretch of soundboard audio for this call, if any is playing.
    pub fn take_effect(&self, account_id: &str, samples: usize) -> Option<Vec<i16>> {
        let mut all = self.effects.lock().unwrap();
        let queue = all.get_mut(account_id)?;
        if queue.is_empty() {
            return None;
        }
        let n = samples.min(queue.len());
        Some(queue.drain(..n).collect())
    }

    /// Whether this account's camera is on.
    pub fn camera_on(&self, account_id: &str) -> bool {
        self.cameras.lock().unwrap().contains_key(account_id)
    }

    pub fn note_flags(&self, account_id: &str, muted: bool, deafened: bool) {
        self.flags.lock().unwrap().insert(account_id.to_string(), (muted, deafened));
    }

    /// The microphone and output flags last announced, or the cautious pair.
    pub fn flags(&self, account_id: &str) -> (bool, bool) {
        self.flags.lock().unwrap().get(account_id).copied().unwrap_or((true, true))
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
            state.voice.retries.lock().unwrap().remove(account_id);
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
    let handshake = Handshake {
        server_id: info.server_id().context("no channel to connect to")?.to_string(),
        channel_id: info.channel_id.clone().context("no channel to connect to")?,
        user_id: config.user_id.clone(),
        session_id: info.session_id.clone().context("no voice session")?,
        token: info.token.clone().context("no voice token")?,
        endpoint: info.endpoint.clone().context("no voice server")?,
    };

    // A connection already running for this account is replaced, not kept
    // beside: moving channels arrives as a fresh handshake.
    if let Some(old) = state.voice.conns.lock().unwrap().remove(account_id) {
        old.stop();
    }

    let tracker = Arc::new(SpeakingTracker::default());
    state.voice.speaking.lock().unwrap().insert(account_id.to_string(), tracker.clone());

    let prefs = state.voice_prefs.get();
    crate::audio::set_playback_muted(prefs.deafened);
    let playback = match crate::audio::start_playback(prefs.output.as_deref()) {
        Ok(playback) => {
            let playback = Arc::new(playback);
            state.voice.playbacks.lock().unwrap().insert(account_id.to_string(), playback.clone());
            Some(playback)
        }
        Err(e) => {
            tracing::warn!("discord[{account_id}]: no audio output, joining deaf: {e:#}");
            None
        }
    };

    let mut mic = None;
    if state.voice.options(account_id).transmit {
        state.voice.mic_wanted_muted.lock().unwrap().insert(account_id.to_string(), prefs.mic_muted || prefs.deafened);
        let held = state.voice.suppressed.lock().unwrap().contains(account_id);
        match start_transmitting(prefs.input.as_deref(), prefs.mic_muted || prefs.deafened || held) {
            Ok((capture, source)) => {
                state.voice.captures.lock().unwrap().insert(account_id.to_string(), capture);
                mic = Some(source);
                tracing::info!("discord[{account_id}]: transmitting microphone audio");
            }
            Err(e) => tracing::warn!("discord[{account_id}]: no microphone, joining silent: {e:#}"),
        }
    }

    let for_end = state.clone();
    let account = account_id.to_string();
    let conn = voiceconn::connect(state, account_id, &handshake, Media { playback, tracker, mic }, move |ended| {
        tokio::spawn(after_end(for_end, account, ended));
    })
    .await
    .context("opening the voice connection")?;

    // A camera that was on before the connection dropped is on after it.
    if let Some(quality) = state.voice.cameras.lock().unwrap().get(account_id).copied() {
        conn.set_camera(true, quality);
    }
    state.voice.conns.lock().unwrap().insert(account_id.to_string(), conn);
    Ok(())
}

/// Turns this account's camera on or off in the call it is in.
///
/// Two messages to two servers, both needed: the voice server is told a
/// picture is coming on the camera SSRC, and the gateway is told
/// `self_video`, which is what puts a tile up on everybody else's screen.
pub fn set_camera(state: &AppState, account_id: &str, on: bool, quality: VideoQuality) -> Result<()> {
    let conn = state.voice.conns.lock().unwrap().get(account_id).cloned().context("not in a call")?;
    if on {
        state.voice.cameras.lock().unwrap().insert(account_id.to_string(), quality);
    } else {
        state.voice.cameras.lock().unwrap().remove(account_id);
    state.voice.suppressed.lock().unwrap().remove(account_id);
    state.voice.effects.lock().unwrap().remove(account_id);
    }
    conn.set_camera(on, quality);
    let (muted, deafened) = state.voice.flags(account_id);
    super::calls::announce_voice_flags(state, account_id, muted, deafened);
    Ok(())
}

/// Whether a camera frame handed in now would reach anybody.
pub fn camera_ready(state: &AppState, account_id: &str) -> bool {
    state.voice.conns.lock().unwrap().get(account_id).is_some_and(|c| c.camera_ready())
}

/// One encoded camera frame from the window.
pub async fn send_camera_frame(state: &AppState, account_id: &str, frame: &[u8], timestamp_micros: i64) -> Result<()> {
    let conn = state.voice.conns.lock().unwrap().get(account_id).cloned().context("not in a call")?;
    conn.send_camera_frame(frame, timestamp_micros).await
}

/// `connection_ended`, boxed with its thread-safety stated.
///
/// Reconnecting opens a connection that carries this same handler, so the
/// future contains itself; spelling out `Send` here is what lets the compiler
/// stop following the loop.
fn after_end(state: AppState, account: String, ended: Ended) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move { connection_ended(&state, &account, ended).await })
}

/// How many times a dropped connection is reopened before giving up.
const MAX_RETRIES: u32 = 3;

/// A connection has stopped. Opens it again if the server's reason allows and
/// the account still means to be in that channel.
async fn connection_ended(state: &AppState, account_id: &str, ended: Ended) {
    match ended {
        Ended::Stopped => {}
        Ended::Final(reason) => {
            tracing::warn!("discord[{account_id}]: voice ended: {reason}");
            state.events.emit("discordVoiceError", json!({ "accountId": account_id, "error": reason }));
            teardown(state, account_id);
        }
        Ended::Retry(reason) => {
            teardown(state, account_id);
            let attempt = {
                let mut retries = state.voice.retries.lock().unwrap();
                let attempt = retries.entry(account_id.to_string()).or_insert(0);
                *attempt += 1;
                *attempt
            };
            let still_wanted = state.voice.pending.lock().unwrap().get(account_id).is_some_and(|p| p.complete());
            if !still_wanted {
                return;
            }
            if attempt > MAX_RETRIES {
                tracing::warn!("discord[{account_id}]: voice dropped ({reason}); gave up after {MAX_RETRIES} attempts");
                state.events.emit("discordVoiceError", json!({ "accountId": account_id, "error": reason }));
                return;
            }
            tracing::info!("discord[{account_id}]: voice dropped ({reason}); reconnecting, attempt {attempt}");
            tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
            try_connect(state, account_id).await;
        }
    }
}

/// Releases what a connection held: its capture, speakers and entry.
fn teardown(state: &AppState, account_id: &str) {
    state.voice.captures.lock().unwrap().remove(account_id);
    state.voice.playbacks.lock().unwrap().remove(account_id);
    state.voice.conns.lock().unwrap().remove(account_id);
}

fn start_transmitting(device_id: Option<&str>, muted: bool) -> Result<(crate::audio::Capture, crate::audio::MicSource)> {
    let (source, sink) = crate::audio::MicSource::new();
    let capture = crate::audio::start_capture(device_id, sink)?;
    capture.set_muted(muted);
    Ok((capture, source))
}

pub async fn disconnect(state: &AppState, account_id: &str) {
    let conn = state.voice.conns.lock().unwrap().remove(account_id);
    state.voice.cameras.lock().unwrap().remove(account_id);
    state.voice.captures.lock().unwrap().remove(account_id);
    state.voice.playbacks.lock().unwrap().remove(account_id);
    state.voice.retries.lock().unwrap().remove(account_id);
    if let Some(conn) = conn {
        conn.stop();
        tracing::info!("discord[{account_id}]: voice disconnected");
    }
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
