//! Sound devices, and getting a microphone into a voice connection.
//!
//! This lives in the daemon for the same reason the voice connection does:
//! the mixer and the encoder are Songbird's, in this process, and moving raw
//! audio across the RPC socket to be encoded somewhere else would add latency
//! and buffering to solve a problem that does not exist if they sit together.
//! A frontend asks to join, leave, mute, or use a different device.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What Discord's voice protocol expects, and therefore what everything is
/// converted to before it reaches Songbird.
pub const TARGET_RATE: u32 = 48_000;
pub const TARGET_CHANNELS: u16 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AudioDevice {
    /// The sound server's own name for the device. Stable across reboots and
    /// across renaming, which is what a stored preference must key on.
    pub id: String,
    /// What to show a person: "HyperX QuadCast S Analog Stereo", not
    /// "alsa_input.usb-HP__Inc_HyperX_QuadCast_S-00.analog-stereo".
    pub name: String,
    /// "input" or "output".
    pub kind: String,
    /// Whether the sound server currently routes here by default. Named
    /// rather than assumed: it is a moving target the user changes elsewhere.
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

/// Which devices voice should use, and whether it is currently silenced.
///
/// Kept apart from accounts.toml deliberately: that file holds credentials at
/// mode 0600, and these are preferences that belong to the machine rather than
/// to any account.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VoicePrefs {
    /// Device ids, or None for "whatever the sound server considers default",
    /// which is the right behaviour for someone who has never chosen.
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    /// Microphone closed. Persisted because arriving in a call unexpectedly
    /// live, after having deliberately muted, is the failure people mind.
    #[serde(default)]
    pub mic_muted: bool,
    /// Output silenced.
    #[serde(default)]
    pub deafened: bool,
    /// How loud a call is overall, 1.0 being as it arrives. Up to 2.0, as
    /// Discord's own slider goes.
    #[serde(default = "unity")]
    pub output_volume: f32,
    /// Somebody turned up or down, by Discord user id. Kept across calls, as
    /// Discord keeps it: the person who is too loud is too loud every time.
    #[serde(default)]
    pub user_volumes: std::collections::HashMap<String, f32>,
    /// Taking what the speakers play back out of the microphone, so somebody
    /// on speakers does not send the call back into itself. On by default,
    /// as it is in Discord and in every browser.
    #[serde(default = "on")]
    pub echo_cancellation: bool,
    /// Taking steady background noise - a fan, a hum - out of the microphone.
    #[serde(default = "on")]
    pub noise_suppression: bool,
}

fn on() -> bool {
    true
}

fn unity() -> f32 {
    1.0
}

impl Default for VoicePrefs {
    fn default() -> Self {
        VoicePrefs {
            input: None,
            output: None,
            mic_muted: false,
            deafened: false,
            output_volume: 1.0,
            user_volumes: Default::default(),
            echo_cancellation: true,
            noise_suppression: true,
        }
    }
}

impl VoicePrefs {
    /// The gain for one person, as set or as it arrived.
    pub fn user_volume(&self, user_id: &str) -> f32 {
        self.user_volumes.get(user_id).copied().unwrap_or(1.0)
    }
}

/// The loudest a volume may be set: twice as it arrived, Discord's ceiling.
pub const MAX_VOLUME: f32 = 2.0;

/// Scales samples by a gain, clipping rather than wrapping where it goes
/// past the edge - a clip sounds loud, a wrap sounds like a gunshot.
pub fn apply_gain(samples: &mut [i16], gain: f32) {
    if (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    for s in samples.iter_mut() {
        *s = (*s as f32 * gain).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

/// The preferences file at ~/.config/nobilis/voice.toml.
pub struct VoicePrefsStore {
    path: std::path::PathBuf,
    inner: Mutex<VoicePrefs>,
}

impl VoicePrefsStore {
    pub fn open(path: std::path::PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            // A corrupt or unreadable preferences file should not stop the
            // daemon starting; defaults are a working configuration.
            .unwrap_or_default();
        Self { path, inner: Mutex::new(inner) }
    }

    pub fn get(&self) -> VoicePrefs {
        self.inner.lock().unwrap().clone()
    }

    /// Applies `edit` and writes the result out.
    pub fn update(&self, edit: impl FnOnce(&mut VoicePrefs)) -> VoicePrefs {
        let mut prefs = self.inner.lock().unwrap();
        edit(&mut prefs);
        let out = prefs.clone();
        if let Ok(text) = toml::to_string_pretty(&out) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = self.path.with_extension("toml.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
        out
    }
}

/// The devices this machine offers.
///
/// Asked of the sound server rather than of ALSA. On any current Linux desktop
/// ALSA presents its whole plugin chain as devices - rate converters, channel
/// mixers, a null sink, over a hundred entries here - and cpal reports them
/// all classified "Unknown", giving real hardware the same treatment, so there
/// is nothing in that data to pick out a microphone by. PipeWire and
/// PulseAudio both answer `pactl` with the real devices and their human
/// descriptions, which is what a person needs to choose between.
///
/// Where there is no sound server, this falls back to naming the defaults cpal
/// reports. A machine with no sound card at all is a perfectly ordinary thing
/// to run a chat client on, and everything except voice still works.
pub fn list_devices() -> Result<Vec<AudioDevice>> {
    let mut out = Vec::new();
    if let Ok(devices) = server_devices() {
        if !devices.is_empty() {
            return Ok(devices);
        }
    }

    // Off Linux there is no sound server to ask, and that is the ordinary
    // case rather than a degraded one: WASAPI and CoreAudio both enumerate
    // real hardware with real names, so cpal's own list is the right answer.
    //
    // Not on Linux, even when pactl is missing. ALSA presents its whole
    // plugin chain here - rate converters, mixers, a null sink, over a
    // hundred entries - all reported "Unknown", so listing them would bury
    // the microphone rather than offer it. A machine there with no sound
    // server keeps the two defaults below.
    #[cfg(not(target_os = "linux"))]
    {
        let listed = host_devices();
        if !listed.is_empty() {
            return Ok(listed);
        }
    }

    let host = cpal::default_host();
    if let Some(device) = host.default_input_device() {
        out.push(AudioDevice { id: DEFAULT_ID.into(), name: device.to_string(), kind: "input".into(), is_default: true });
    }
    if let Some(device) = host.default_output_device() {
        out.push(AudioDevice { id: DEFAULT_ID.into(), name: device.to_string(), kind: "output".into(), is_default: true });
    }
    Ok(out)
}

/// Everything the audio host itself reports.
///
/// The device's name doubles as its id, because cpal has no stable
/// identifier to offer. Renaming a device in the OS therefore loses a stored
/// choice, which is worth the trade for being able to make one at all -
/// before this there was no way to pick anything off Linux, since the only
/// enumeration was pactl and the only fallback was the two defaults.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn host_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    // cpal 0.18 has no name() - a Device is Display, and that is the name.
    let default_in = host.default_input_device().map(|d| d.to_string());
    let default_out = host.default_output_device().map(|d| d.to_string());
    let mut out = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for name in devices.map(|d| d.to_string()) {
            let is_default = Some(&name) == default_in.as_ref();
            out.push(AudioDevice { id: name.clone(), name, kind: "input".into(), is_default });
        }
    }
    if let Ok(devices) = host.output_devices() {
        for name in devices.map(|d| d.to_string()) {
            let is_default = Some(&name) == default_out.as_ref();
            out.push(AudioDevice { id: name.clone(), name, kind: "output".into(), is_default });
        }
    }
    out
}

/// The host device with this name, if there is one.
///
/// A pactl id ("alsa_input.usb-...") never matches a cpal device name, so on
/// Linux this finds nothing and the caller falls back to the default - which
/// is correct there, because the choice is applied by moving the stream
/// afterwards instead. It matches only where the ids came from cpal in the
/// first place, which is exactly where moving is not possible.
fn host_device_named(id: &str, kind: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    let devices: Box<dyn Iterator<Item = cpal::Device>> = match kind {
        "input" => Box::new(host.input_devices().ok()?),
        _ => Box::new(host.output_devices().ok()?),
    };
    devices.into_iter().find(|d| d.to_string() == id)
}

/// The id meaning "let the sound server decide".
pub const DEFAULT_ID: &str = "";

fn pactl(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("pactl")
        .args(args)
        .output()
        .context("running pactl")?;
    if !out.status.success() {
        return Err(anyhow!("pactl {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn server_devices() -> Result<Vec<AudioDevice>> {
    let mut out = parse_devices(&pactl(&["-f", "json", "list", "sources"])?, "input", pactl(&["get-default-source"]).unwrap_or_default().trim())?;
    out.extend(parse_devices(&pactl(&["-f", "json", "list", "sinks"])?, "output", pactl(&["get-default-sink"]).unwrap_or_default().trim())?);
    Ok(out)
}

/// Turns one `pactl -f json list` reply into devices worth offering.
///
/// Monitor sources are dropped: every output has one, they are loopbacks of
/// what is already playing, and offering "Monitor of Headphones" as a
/// microphone would mean transmitting your own audio back into a call.
fn parse_devices(json: &str, kind: &str, default_name: &str) -> Result<Vec<AudioDevice>> {
    let parsed: serde_json::Value = serde_json::from_str(json).context("parsing pactl output")?;
    let entries = parsed.as_array().context("pactl did not return a list")?;
    let mut out = Vec::new();
    for entry in entries {
        let Some(id) = entry["name"].as_str() else { continue };
        if kind == "input" && (id.ends_with(".monitor") || entry["monitor_of_sink_name"].is_string()) {
            continue;
        }
        let name = entry["description"].as_str().filter(|d| !d.is_empty()).unwrap_or(id);
        out.push(AudioDevice {
            id: id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            is_default: id == default_name,
        });
    }
    Ok(out)
}

/// Points this process's capture stream at `device_id`.
///
/// Routing is done by moving the stream rather than by opening the device
/// directly: on PipeWire an application is a stream the server routes, and
/// moving it is both what the desktop's own mixer does and what survives the
/// device disappearing.
pub fn route_input(device_id: &str) -> Result<bool> {
    route_stream(Stream::Input, device_id)
}

/// Which of this process's streams to move.
#[derive(Clone, Copy)]
enum Stream {
    Input,
    Output,
}

impl Stream {
    fn list(self) -> &'static str {
        match self {
            Stream::Input => "source-outputs",
            Stream::Output => "sink-inputs",
        }
    }

    fn move_command(self) -> &'static str {
        match self {
            Stream::Input => "move-source-output",
            Stream::Output => "move-sink-input",
        }
    }

    /// How the ALSA plugin names the node it creates.
    fn node_prefix(self) -> &'static str {
        match self {
            Stream::Input => "alsa_capture",
            Stream::Output => "alsa_playback",
        }
    }

    fn default_query(self) -> &'static str {
        match self {
            Stream::Input => "get-default-source",
            Stream::Output => "get-default-sink",
        }
    }
}

/// The name PipeWire gives this process's streams.
///
/// Identifying our own streams by name is not the obvious choice - a process
/// id would be exact - but the ALSA plugin every cpal stream goes through
/// publishes no process id at all: the only identifying property it sets is
/// `node.name`, built from the executable's own file name. Two daemons cannot
/// collide here in practice because nobilis holds a single-instance lock (see
/// main.rs), and the exact match keeps a differently-named build, such as a
/// test binary, from being mistaken for the daemon.
fn stream_node_name(stream: Stream) -> String {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "nobilis".to_string());
    format!("{}.{binary}", stream.node_prefix())
}

/// The device the sound server currently routes to by default.
fn default_device(stream: Stream) -> Result<String> {
    let name = pactl(&[stream.default_query()])?.trim().to_string();
    if name.is_empty() {
        return Err(anyhow!("the sound server named no default device"));
    }
    Ok(name)
}

/// Moves this process's stream to `device_id`, or to the current default when
/// that is empty.
///
/// Routing to the default is deliberately an action rather than an absence of
/// one. Doing nothing looks equivalent and is not: the sound server remembers
/// the last device an application was moved to and puts it back there, so
/// "system default" would silently mean "whatever was chosen last time",
/// leaving audio playing into a device nobody is listening to.
/// The sound server's indices for every stream of this kind this process has
/// open.
///
/// Every stream the ALSA plugin opens for us carries the same node name, so
/// the name alone cannot tell a microphone from a recording of what the
/// computer is playing. Which of them a capture owns is worked out by taking
/// this before the capture opens and again after: the new one is its own.
fn our_streams(stream: Stream) -> Result<std::collections::HashSet<String>> {
    let json = pactl(&["-f", "json", "list", stream.list()])?;
    let streams: serde_json::Value = serde_json::from_str(&json).context("parsing pactl output")?;
    let want = stream_node_name(stream);
    Ok(streams
        .as_array()
        .map(|s| s.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|s| s["properties"]["node.name"].as_str() == Some(want.as_str()))
        .filter_map(|s| s["index"].as_u64().map(|i| i.to_string()))
        .collect())
}

/// Recordings of what the computer plays, which a microphone change must never
/// move: rerouting "everything this process records" to a new microphone would
/// turn a shared screen's sound into that microphone.
fn system_capture_streams() -> &'static Mutex<std::collections::HashSet<String>> {
    static HELD: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    HELD.get_or_init(Default::default)
}

fn route_stream(stream: Stream, device_id: &str) -> Result<bool> {
    route_stream_except(stream, device_id, &Default::default())
}

/// Moves this process's streams to `device_id`, leaving alone the ones in
/// `existing` - there before the capture being routed opened, so somebody
/// else's - and any recording of the computer's own sound.
fn route_stream_except(stream: Stream, device_id: &str, existing: &std::collections::HashSet<String>) -> Result<bool> {
    let device_id = &if device_id.is_empty() { default_device(stream)? } else { device_id.to_string() };
    let protected = system_capture_streams().lock().unwrap().clone();
    let mine: Vec<String> = our_streams(stream)?
        .into_iter()
        .filter(|index| !existing.contains(index) && !protected.contains(index))
        .collect();

    let mut moved = false;
    for index in mine {
        pactl(&[stream.move_command(), &index, device_id])?;
        moved = true;
    }
    Ok(moved)
}

fn input_device(device_id: Option<&str>) -> Result<cpal::Device> {
    // Where there is a sound server, the default: a chosen device is
    // honoured by moving the stream once it exists (see `route_input`), not
    // by opening that device here. Opening directly would bypass the
    // server's routing and lose the choice the moment the device is
    // unplugged and returns.
    //
    // Where there is not, moving is not on offer, and opening the device is
    // the only way a choice means anything. host_device_named only matches
    // ids that came from cpal, so this cannot fire on the pactl path.
    if let Some(found) = device_id.filter(|d| !d.is_empty()).and_then(|id| host_device_named(id, "input")) {
        return Ok(found);
    }
    cpal::default_host().default_input_device().context("no default input device")
}

/// A running capture: a microphone, or what the computer is playing.
///
/// Dropping it stops the stream. The captured audio is pushed into `sink` as
/// 48kHz stereo f32.
pub struct Capture {
    _stream: cpal::Stream,
    pub level: Arc<Mutex<f32>>,
    pub active: Arc<AtomicBool>,
    /// The sound server's indices for a system-audio recording, released when
    /// it closes so a later microphone reusing the index is routable again.
    system_streams: Vec<String>,
    /// The private sink a Linux system recording gathers applications into;
    /// taken down with it.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    _mix: Option<ShareMix>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.system_streams.is_empty() {
            let mut held = system_capture_streams().lock().unwrap();
            for index in &self.system_streams {
                held.remove(index);
            }
        }
    }
}

/// Starts capturing a microphone, handing each converted chunk to `sink`.
///
/// `device_id` names a device from `list_devices`, or is empty for the sound
/// server's default.
///
/// The conversion is deliberately simple: nearest-neighbour resampling and
/// channel duplication. It is enough to carry speech correctly and keeps this
/// dependency-free; anything better belongs behind a resampler crate if the
/// difference ever proves audible.
pub fn start_capture<F>(device_id: Option<&str>, sink: F) -> Result<Capture>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    let device = input_device(device_id)?;
    let config = device.default_input_config().context("querying the input device")?;
    open_input(device, config, sink, Route::Device(device_id.unwrap_or("").to_string()))
}

/// Where a freshly opened recording is pointed once it exists.
enum Route {
    /// A microphone: the one chosen, or the default for an empty id.
    Device(String),
    /// The monitor of the default output - everything the computer plays.
    /// Linux, where a recording is moved rather than opened on a device.
    #[cfg_attr(any(target_os = "windows", target_os = "macos"), allow(dead_code))]
    Monitor,
    /// Nowhere: the device opened is already the right one.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    AsOpened,
}

/// Records what the computer is playing, to share along with a screen.
///
/// The same on every desktop in what it delivers - 48kHz stereo, whatever the
/// device runs at - and different in how it gets it, because each hands its
/// own sound out differently:
///
/// - **Linux** (PipeWire or PulseAudio, X11 or Wayland alike): an ordinary
///   recording, moved onto the monitor of the default output. The monitor is
///   the sound server's copy of everything being played.
/// - **Windows**: a recording opened on the output device itself, which is
///   how WASAPI offers loopback.
/// - **macOS** has no loopback a program can open without ScreenCaptureKit,
///   so it says so rather than sharing silence.
///
/// What is recorded is everything - this program's own playback of a call
/// included, which is what a monitor is. On Linux `NOBILIS_SHARE_AUDIO_SOURCE`
/// names a different source to record instead.
pub fn start_system_capture<F>(sink: F) -> Result<Capture>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    #[cfg(target_os = "macos")]
    {
        let _ = sink;
        Err(anyhow!("sharing the computer's sound is not supported on macOS yet"))
    }
    #[cfg(target_os = "windows")]
    {
        let device = cpal::default_host().default_output_device().context("no default output device to record")?;
        let config = device.default_output_config().context("querying the output device")?;
        open_input(device, config, sink, Route::AsOpened)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let device = cpal::default_host().default_input_device().context("no recording device to point at the output")?;
        let config = device.default_input_config().context("querying the input device")?;
        open_input(device, config, sink, Route::Monitor)
    }
}

/// The private mix's monitor, where there is a mix - which is Linux only.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn mixed_monitor(mix: &Option<ShareMix>) -> Option<String> {
    mix.as_ref().map(ShareMix::monitor)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn mixed_monitor(_mix: &Option<()>) -> Option<String> {
    None
}

/// Opens a recording, converts what it delivers to 48kHz stereo, and points it
/// where `route` says.
fn open_input<F>(device: cpal::Device, config: cpal::SupportedStreamConfig, mut sink: F, route: Route) -> Result<Capture>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    // On Linux a system recording is of a private mix rather than of the
    // speakers - see ShareMix for why the speakers' monitor is not enough.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mix = match route {
        Route::Monitor => match ShareMix::open() {
            Ok(mix) => Some(mix),
            Err(e) => {
                tracing::warn!("audio: no private mix ({e:#}); sharing the speakers' monitor, which follows their volume");
                None
            }
        },
        _ => None,
    };
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mix: Option<()> = None;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let _ = &mix;

    // Which of our recordings already exist, so the one about to open is the
    // only one moved.
    let existing = match route {
        Route::AsOpened => Default::default(),
        _ => our_streams(Stream::Input).unwrap_or_default(),
    };
    let rate = config.sample_rate();
    let channels = config.channels();
    tracing::info!("audio: capturing from {:?} at {rate}Hz {channels}ch", device.to_string());

    let level = Arc::new(Mutex::new(0.0f32));
    let active = Arc::new(AtomicBool::new(true));
    let (level_w, active_w) = (level.clone(), active.clone());

    let mut convert = move |samples: &[f32]| {
        if !active_w.load(Ordering::Relaxed) {
            return;
        }
        // Peak level, so a caller can show that a microphone is actually
        // picking something up rather than being silently muted or unplugged.
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        *level_w.lock().unwrap() = peak;

        let frames = samples.len() / channels.max(1) as usize;
        let out_frames = (frames as u64 * TARGET_RATE as u64 / rate.max(1) as u64) as usize;
        let mut out = Vec::with_capacity(out_frames * TARGET_CHANNELS as usize);
        for i in 0..out_frames {
            let src = (i as u64 * rate as u64 / TARGET_RATE as u64) as usize;
            let base = src * channels as usize;
            let l = samples.get(base).copied().unwrap_or(0.0);
            let r = if channels > 1 { samples.get(base + 1).copied().unwrap_or(l) } else { l };
            out.push(l.clamp(-1.0, 1.0));
            out.push(r.clamp(-1.0, 1.0));
        }
        sink(&out);
    };

    let err = |e| tracing::warn!("audio: capture stream error: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config.into(),
            move |data: &[f32], _: &_| convert(data),
            err,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config.into(),
            move |data: &[i16], _: &_| {
                let f: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                convert(&f)
            },
            err,
            None,
        ),
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    }
    .context("opening the input stream")?;

    stream.play().context("starting the input stream")?;

    let mut system_streams = Vec::new();
    match route {
        Route::Device(want) => match route_when_ready(Stream::Input, &want, &existing) {
            Ok(on) => tracing::info!("audio: input routed to {on:?}"),
            // Being recorded on a different microphone than the one chosen is
            // worse than being told the choice did not take - but only a
            // choice can be betrayed. Failing to reach the default leaves the
            // stream wherever the sound server put it, which is a working call.
            Err(e) if !want.is_empty() => return Err(e.context(format!("routing input to {want:?}"))),
            Err(e) => tracing::warn!("audio: could not route input to the default device: {e:#}"),
        },
        Route::Monitor => {
            // Three places the sound can come from, best first: a source
            // named outright; the private mix of every application but this
            // one; the default output's monitor, where there is no PipeWire
            // to build a mix with.
            let monitor = match std::env::var("NOBILIS_SHARE_AUDIO_SOURCE") {
                Ok(named) if !named.is_empty() => named,
                _ => match mixed_monitor(&mix) {
                    Some(monitor) => monitor,
                    None => format!("{}.monitor", default_device(Stream::Output)?),
                },
            };
            route_when_ready(Stream::Input, &monitor, &existing)
                .with_context(|| format!("pointing the recording at {monitor}"))?;
            // Remembered, so that changing microphones mid-call leaves it
            // where it is.
            system_streams = our_streams(Stream::Input)?.into_iter().filter(|i| !existing.contains(i)).collect();
            system_capture_streams().lock().unwrap().extend(system_streams.iter().cloned());
            tracing::info!("audio: recording what is played, from {monitor}");
        }
        Route::AsOpened => {}
    }

    Ok(Capture {
        _stream: stream,
        level,
        active,
        system_streams,
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        _mix: mix,
    })
}

impl Capture {
    /// Closes or opens the microphone.
    ///
    /// The stream keeps running either way: stopping and restarting it would
    /// make unmuting take as long as opening a device, and some hardware
    /// clicks audibly when it does. A muted capture simply stops handing
    /// anything on, so the source it feeds falls back to silence.
    pub fn set_muted(&self, muted: bool) {
        self.active.store(!muted, Ordering::Relaxed);
    }

}


/// Whether the speakers are silenced.
///
/// Process-wide rather than per-connection because there is one set of
/// speakers: someone in two calls who presses deafen means both.
static PLAYBACK_MUTED: AtomicBool = AtomicBool::new(false);

pub fn set_playback_muted(muted: bool) {
    PLAYBACK_MUTED.store(muted, Ordering::Relaxed);
}

pub fn playback_muted() -> bool {
    PLAYBACK_MUTED.load(Ordering::Relaxed)
}

/// Sound coming out of the machine.
///
/// Dropping it closes the output stream.
pub struct Playback {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<std::collections::VecDeque<i16>>>,
    /// Loudest sample handed to the speakers since anyone last asked, and how
    /// much has arrived in total. Enough to tell "nobody is talking" from
    /// "their audio is not reaching us", which sound identical otherwise.
    peak: Mutex<f32>,
    received: std::sync::atomic::AtomicU64,
}

impl Playback {
    /// Queues decoded audio for the speakers.
    ///
    /// Dropped while deafened rather than queued and skipped, so unmuting
    /// resumes with what is being said then, not with a backlog of what was
    /// said while silenced.
    pub fn push(&self, pcm: &[i16]) {
        if playback_muted() {
            return;
        }
        let loudest = pcm.iter().fold(0i16, |m, s| m.max(s.saturating_abs()));
        {
            let mut peak = self.peak.lock().unwrap();
            *peak = peak.max(loudest as f32 / i16::MAX as f32);
        }
        self.received.fetch_add(pcm.len() as u64, Ordering::Relaxed);

        let mut buf = self.buffer.lock().unwrap();
        buf.extend(pcm.iter().copied());
        // The same reasoning as the capture backlog: if the output device
        // stalls, drop old audio rather than play a growing delay.
        const MAX_SAMPLES: usize = TARGET_RATE as usize * TARGET_CHANNELS as usize / 2; // half a second
        let backlog = buf.len();
        if backlog > MAX_SAMPLES {
            buf.drain(..backlog - MAX_SAMPLES);
        }
    }

    /// The loudest thing heard since this was last called, and the running
    /// total of samples received. Reading resets the peak, so successive
    /// readings describe successive intervals rather than the whole call.
    pub fn take_level(&self) -> (f32, u64) {
        let mut peak = self.peak.lock().unwrap();
        let seen = *peak;
        *peak = 0.0;
        (seen, self.received.load(Ordering::Relaxed))
    }
}

/// Opens the speakers, playing whatever is pushed into the returned handle.
///
/// As with capture, a chosen device is honoured by moving the stream once it
/// exists rather than by opening that device directly.
pub fn start_playback(device_id: Option<&str>) -> Result<Playback> {
    let device = match device_id.filter(|d| !d.is_empty()).and_then(|id| host_device_named(id, "output")) {
        Some(found) => found,
        None => cpal::default_host().default_output_device().context("no default output device")?,
    };
    let config = device.default_output_config().context("querying the output device")?;
    let rate = config.sample_rate();
    let channels = config.channels();
    tracing::info!("audio: playing to {:?} at {rate}Hz {channels}ch", device.to_string());
    let existing = our_streams(Stream::Output).unwrap_or_default();

    let buffer = Arc::new(Mutex::new(std::collections::VecDeque::<i16>::new()));
    let reader = buffer.clone();

    // Voice arrives as 48kHz stereo; the device may want something else.
    let fill = move |out_frames: usize, mut write: Box<dyn FnMut(usize, f32) + '_>| {
        let mut buf = reader.lock().unwrap();
        for frame in 0..out_frames {
            // Nearest-neighbour again, and for the same reason: enough to
            // carry speech, and no dependency to keep current.
            let src_frame = (frame as u64 * TARGET_RATE as u64 / rate.max(1) as u64) as usize;
            let base = src_frame * TARGET_CHANNELS as usize;
            for ch in 0..channels as usize {
                // Mono output takes the left channel; anything wider than
                // stereo repeats what there is rather than leaving silence in
                // the surround channels.
                let sample = buf.get(base + ch.min(TARGET_CHANNELS as usize - 1)).copied().unwrap_or(0);
                write(frame * channels as usize + ch, sample as f32 / i16::MAX as f32);
            }
        }
        let consumed = (out_frames as u64 * TARGET_RATE as u64 / rate.max(1) as u64) as usize * TARGET_CHANNELS as usize;
        let drop_n = consumed.min(buf.len());
        buf.drain(..drop_n);
    };

    let err = |e| tracing::warn!("audio: output stream error: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config.into(),
            move |data: &mut [f32], _: &_| {
                let frames = data.len() / channels.max(1) as usize;
                fill(frames, Box::new(|i, v| data[i] = v));
            },
            err,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &_| {
                let frames = data.len() / channels.max(1) as usize;
                fill(frames, Box::new(|i, v| data[i] = (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16));
            },
            err,
            None,
        ),
        other => return Err(anyhow!("unsupported output sample format {other:?}")),
    }
    .context("opening the output stream")?;

    stream.play().context("starting the output stream")?;

    // Unlike a misrouted microphone this is merely wrong rather than a privacy
    // problem, so a failure here is a warning: hearing the call from the wrong
    // speakers beats not hearing it at all.
    match route_when_ready(Stream::Output, device_id.unwrap_or(""), &existing) {
        Ok(on) => tracing::info!("audio: output routed to {on:?}"),
        Err(e) => tracing::warn!("audio: could not route output: {e:#}"),
    }

    Ok(Playback { _stream: stream, buffer, peak: Mutex::new(0.0), received: std::sync::atomic::AtomicU64::new(0) })
}

/// Every application's sound except this program's, gathered into a sink of
/// its own for sharing with a screen.
///
/// Why not simply record the speakers' monitor: on PipeWire the monitor can
/// carry the output's volume - here it carried it twice, so at 28% volume a
/// shared video arrived at 0.05% of its level, which is silence - and it
/// carries this program's own playback, so everybody in the call would hear
/// themselves come back through the stream. This is how Vesktop's `venmic`
/// does Linux stream audio: a private sink, each application's output linked
/// into it *as well as* to wherever it already plays, and the private sink's
/// monitor recorded at full level whatever the speakers are set to.
///
/// Applications that start playing after the share begins are linked as they
/// appear, by a thread that looks every second. Nothing anybody hears changes:
/// links are added, never moved.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub struct ShareMix {
    module: String,
    sink: String,
    stop: Arc<AtomicBool>,
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl ShareMix {
    pub fn open() -> Result<Self> {
        for tool in ["pw-dump", "pw-link"] {
            std::process::Command::new(tool)
                .arg("--version")
                .output()
                .with_context(|| format!("{tool} is not installed, so there is no PipeWire to build a mix with"))?;
        }
        let sink = format!("nobilis_share_{}", std::process::id());
        let before = default_device(Stream::Output).ok();
        let module = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={sink}"),
            "sink_properties=device.description=moho-screen-share",
        ])?
        .trim()
        .to_string();
        // A new sink can be chosen as the default by a session manager that
        // likes new devices. The share must never take somebody's speakers.
        if let Some(before) = before {
            if default_device(Stream::Output).ok().as_deref() != Some(before.as_str()) {
                let _ = pactl(&["set-default-sink", &before]);
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (looking, target) = (stop.clone(), sink.clone());
        let own_node = stream_node_name(Stream::Output);
        std::thread::spawn(move || {
            let mut linked = 0usize;
            while !looking.load(Ordering::Relaxed) {
                match link_into(&target, &own_node) {
                    Ok(made) if made > 0 => {
                        linked += made;
                        tracing::info!("audio: share mix now takes {linked} application channel(s)");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!("audio: share mix: {e:#}"),
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });
        tracing::info!("audio: gathering applications' sound into {sink}");
        Ok(Self { module, sink, stop })
    }

    pub fn monitor(&self) -> String {
        format!("{}.monitor", self.sink)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl Drop for ShareMix {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The links go with the sink.
        if let Err(e) = pactl(&["unload-module", &self.module]) {
            tracing::warn!("audio: could not take down {}: {e:#}", self.sink);
        }
    }
}

/// Links every application playing sound into `sink`, except this process's
/// own playback. Returns how many links it made.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn link_into(sink: &str, own_node: &str) -> Result<usize> {
    let dump = std::process::Command::new("pw-dump").output().context("running pw-dump")?;
    let graph: serde_json::Value = serde_json::from_slice(&dump.stdout).context("reading pw-dump")?;
    let plan = links_to_make(&graph, sink, own_node, std::process::id());
    for (from, to) in &plan {
        let _ = std::process::Command::new("pw-link").args([from.to_string(), to.to_string()]).output();
    }
    Ok(plan.len())
}

/// Which (output port, input port) links are missing, read from a `pw-dump`.
///
/// Pure, so the rules can be tested without a sound server: application
/// output streams only; not our own (by node name or process); monitor ports
/// never; a mono channel into both sides; and nothing already linked.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn links_to_make(graph: &serde_json::Value, sink: &str, own_node: &str, own_pid: u32) -> Vec<(u64, u64)> {
    let objects = graph.as_array().map(|a| a.as_slice()).unwrap_or_default();
    let kind = |o: &serde_json::Value| o["type"].as_str().unwrap_or("").rsplit(':').next().unwrap_or("").to_string();
    let props = |o: &serde_json::Value| o["info"]["props"].clone();

    let mut sink_node: Option<u64> = None;
    let mut sources: std::collections::HashSet<u64> = Default::default();
    for o in objects.iter().filter(|o| kind(o) == "Node") {
        let p = props(o);
        let Some(id) = o["id"].as_u64() else { continue };
        let name = p["node.name"].as_str().unwrap_or("");
        if name == sink && p["media.class"].as_str() == Some("Audio/Sink") {
            sink_node = Some(id);
            continue;
        }
        if p["media.class"].as_str() != Some("Stream/Output/Audio") {
            continue;
        }
        let pid = p["application.process.id"]
            .as_u64()
            .or_else(|| p["application.process.id"].as_str().and_then(|s| s.parse().ok()));
        if name.starts_with(own_node) || pid == Some(own_pid as u64) {
            continue;
        }
        sources.insert(id);
    }
    let Some(sink_node) = sink_node else { return Vec::new() };

    let mut inputs: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut outputs: Vec<(u64, String)> = Vec::new();
    for o in objects.iter().filter(|o| kind(o) == "Port") {
        let p = props(o);
        let Some(id) = o["id"].as_u64() else { continue };
        let node = p["node.id"].as_u64();
        let channel = p["audio.channel"].as_str().unwrap_or("").to_string();
        let monitor = p["port.monitor"].as_bool().unwrap_or(false);
        match p["port.direction"].as_str() {
            Some("in") if node == Some(sink_node) => {
                inputs.insert(channel, id);
            }
            Some("out") if !monitor && node.is_some_and(|n| sources.contains(&n)) => outputs.push((id, channel)),
            _ => {}
        }
    }
    let existing: std::collections::HashSet<(u64, u64)> = objects
        .iter()
        .filter(|o| kind(o) == "Link")
        .filter_map(|o| Some((o["info"]["output-port-id"].as_u64()?, o["info"]["input-port-id"].as_u64()?)))
        .collect();

    let mut plan = Vec::new();
    for (port, channel) in outputs {
        let targets: Vec<&str> = match channel.as_str() {
            "FR" => vec!["FR"],
            "MONO" => vec!["FL", "FR"],
            _ => vec!["FL"],
        };
        for target in targets {
            if let Some(&to) = inputs.get(target) {
                if !existing.contains(&(port, to)) {
                    plan.push((port, to));
                }
            }
        }
    }
    plan
}

/// Routes a freshly opened stream, waiting for it to exist first.
///
/// A stream is only something the sound server can move once it is playing,
/// and it takes a moment to appear - measured at 0.3 to 0.4 seconds here - so
/// concluding it is missing straight away would fail every time.
fn route_when_ready(stream: Stream, device_id: &str, existing: &std::collections::HashSet<String>) -> Result<String> {
    let target = if device_id.is_empty() { default_device(stream)? } else { device_id.to_string() };
    let mut last = Ok(false);
    for _ in 0..40 {
        last = route_stream_except(stream, &target, existing);
        if matches!(last, Ok(true)) {
            return Ok(target);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    match last {
        Ok(_) => Err(anyhow!("the stream never appeared to the sound server")),
        Err(e) => Err(e),
    }
}


/// A live microphone, read in 20ms frames by whatever is sending it.
///
/// Filled by the capture callback on the audio thread and drained by the voice
/// connection's clock. The two run at the same nominal rate and never in step,
/// so the buffer absorbs the difference - and is capped, because a sender that
/// stalls for a second must not come back to a second of old speech.
pub struct MicSource {
    buffer: Arc<Mutex<std::collections::VecDeque<f32>>>,
}

impl MicSource {
    /// The source, and the sink a capture callback feeds it through.
    pub fn new() -> (Self, impl FnMut(&[f32]) + Send + 'static) {
        let buffer = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let writer = buffer.clone();
        let sink = move |pcm: &[f32]| {
            let mut buf = writer.lock().unwrap();
            buf.extend(pcm.iter().copied());
            const MAX_SAMPLES: usize = TARGET_RATE as usize * TARGET_CHANNELS as usize; // one second
            let backlog = buf.len();
            if backlog > MAX_SAMPLES {
                buf.drain(..backlog - MAX_SAMPLES);
            }
        };
        (Self { buffer }, sink)
    }

    /// The next `samples` interleaved samples, or None if fewer have arrived.
    ///
    /// Nothing is handed out partially: a frame that is half speech and half
    /// padding is a click, and waiting one more tick for the rest is not.
    pub fn take(&self, samples: usize) -> Option<Vec<f32>> {
        let mut buf = self.buffer.lock().unwrap();
        if buf.len() < samples {
            return None;
        }
        Some(buf.drain(..samples).collect())
    }
}

#[cfg(test)]
mod tests {

    /// A gain past the edge clips rather than wrapping round to the other
    /// extreme.
    #[test]
    fn a_gain_clips_rather_than_wraps() {
        let mut loud = [20_000i16, -20_000, 100];
        apply_gain(&mut loud, 2.0);
        assert_eq!(loud, [i16::MAX, i16::MIN, 200]);
        let mut quiet = [1000i16];
        apply_gain(&mut quiet, 0.5);
        assert_eq!(quiet, [500]);
    }

    /// Preferences written before volumes existed read as everything at 100%.
    #[test]
    fn old_preferences_read_as_unchanged_volume() {
        let prefs: VoicePrefs = toml::from_str("micMuted = true").unwrap();
        assert_eq!(prefs.output_volume, 1.0);
        assert_eq!(prefs.user_volume("42"), 1.0);
    }
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn the_default_devices_are_offered() {
        // Not asserting that any exist: a machine with no sound card is a
        // legitimate thing to run a chat client on. Only that asking does not
        // fail, and that anything reported is coherent.
        let devices = list_devices().expect("listing devices should not fail");
        for d in &devices {
            assert!(!d.name.is_empty(), "a device with no name is not selectable");
            assert!(d.kind == "input" || d.kind == "output");
        }
        // At most one default per direction, or a picker cannot show which
        // one is in force.
        for kind in ["input", "output"] {
            let defaults = devices.iter().filter(|d| d.kind == kind && d.is_default).count();
            assert!(defaults <= 1, "{defaults} devices claim to be the default {kind}");
        }
    }

    /// Ignored by default, for the same reason as `microphone_delivers_audio`
    /// below: it needs a sound server to mean anything.
    ///
    /// The check it makes is about *routing* - the stream opens on the
    /// default device and is then moved to the chosen one, and it is the move
    /// that must fail when the chosen one does not exist. Reaching that point
    /// requires a default device to open first, so on a machine with no audio
    /// at all this fails one step earlier with "no default input device" and
    /// tests nothing it was written to test.
    ///
    /// Run it deliberately, on a machine with a microphone:
    ///   cargo test -- --ignored a_missing_device
    #[test]
    #[ignore]
    fn a_missing_device_is_an_error_rather_than_a_substitution() {
        // Being recorded on a different microphone than the one chosen is
        // worse than being told the choice did not take, so routing to a
        // device the server does not have must fail rather than fall back.
        let text = match start_capture(Some("no such device"), |_| {}) {
            Ok(_) => panic!("routing to a device that does not exist was accepted"),
            Err(e) => format!("{e:#}"),
        };
        assert!(text.contains("no such device"), "unexpected: {text}");
    }

    /// A capture of a real pactl reply, so the parser is tested against what
    /// the sound server actually says rather than what it is assumed to say.
    const SOURCES_JSON: &str = r#"[
      {"index": 63, "name": "alsa_output.usb-HyperX.analog-stereo.monitor",
       "description": "Monitor of HyperX QuadCast S Analog Stereo"},
      {"index": 64, "name": "alsa_input.usb-HyperX.analog-stereo",
       "description": "HyperX QuadCast S Analog Stereo"},
      {"index": 68, "name": "alsa_input.usb-Generic_USB_Audio-00.HiFi__Mic__source",
       "description": ""}
    ]"#;

    #[test]
    fn monitors_are_not_offered_as_microphones() {
        // Every output has a monitor source. Offering one as a microphone
        // means transmitting your own audio back into the call.
        let devices = parse_devices(SOURCES_JSON, "input", "alsa_input.usb-HyperX.analog-stereo").unwrap();
        assert!(
            !devices.iter().any(|d| d.id.ends_with(".monitor")),
            "a monitor source was offered as an input: {devices:?}"
        );
        assert_eq!(devices.len(), 2);
    }

    #[test]
    fn devices_are_named_for_people_and_keyed_for_machines() {
        let devices = parse_devices(SOURCES_JSON, "input", "alsa_input.usb-HyperX.analog-stereo").unwrap();
        let hyperx = &devices[0];
        assert_eq!(hyperx.name, "HyperX QuadCast S Analog Stereo");
        assert_eq!(hyperx.id, "alsa_input.usb-HyperX.analog-stereo");
        assert!(hyperx.is_default, "the server's default device was not marked");

        // A device with no description still has to be selectable, so it
        // falls back to the only name there is rather than showing blank.
        assert_eq!(devices[1].name, devices[1].id);
        assert!(!devices[1].is_default);
    }

    #[test]
    fn the_system_default_is_a_destination_rather_than_an_absence() {
        // Choosing "system default" has to actively move the stream. The sound
        // server puts an application back on the device it was last moved to,
        // so treating the default as "do not route" silently means "keep
        // whatever was chosen before" - which is how audio ends up playing
        // into a device nobody is listening to.
        let Ok(output) = default_device(Stream::Output) else {
            return; // no sound server here; nothing to assert against
        };
        assert!(!output.is_empty(), "the default sink resolved to nothing");
        let input = default_device(Stream::Input).expect("a server with a sink should name a source");
        assert!(!input.is_empty(), "the default source resolved to nothing");
    }

    #[test]
    fn our_own_streams_are_identified_the_way_the_sound_server_names_them() {
        // The ALSA plugin publishes no process id, only a node name built from
        // the executable's file name. Matching on anything else silently finds
        // nothing, which looks exactly like a device that cannot be routed to.
        let input = stream_node_name(Stream::Input);
        let output = stream_node_name(Stream::Output);
        assert!(input.starts_with("alsa_capture."), "unexpected: {input}");
        assert!(output.starts_with("alsa_playback."), "unexpected: {output}");
        assert_ne!(input, "alsa_capture.", "the executable name was lost");
    }

    #[test]
    fn preferences_survive_a_restart_and_a_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("nobilis-voice-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("voice.toml");

        let store = VoicePrefsStore::open(path.clone());
        assert_eq!(store.get(), VoicePrefs::default());
        store.update(|p| {
            p.input = Some("alsa_input.usb-HyperX.analog-stereo".into());
            p.mic_muted = true;
        });

        // Muting and then finding yourself live on the next run is the
        // failure this guards against.
        let reopened = VoicePrefsStore::open(path.clone());
        assert_eq!(reopened.get().input.as_deref(), Some("alsa_input.usb-HyperX.analog-stereo"));
        assert!(reopened.get().mic_muted);

        // Garbage on disk must not stop the daemon starting.
        std::fs::write(&path, "this is not toml {{{").unwrap();
        assert_eq!(VoicePrefsStore::open(path).get(), VoicePrefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_frame_is_handed_out_whole_or_not_at_all() {
        let (source, mut sink) = MicSource::new();
        sink(&[0.25f32; 1000]);
        assert!(source.take(1920).is_none(), "half a frame is a click, not a frame");
        sink(&[0.25f32; 920]);
        let frame = source.take(1920).expect("a whole frame once it has arrived");
        assert_eq!(frame.len(), 1920);
        assert!(source.take(1).is_none(), "nothing is left over");
    }

    #[test]
    fn a_stalled_reader_does_not_grow_the_backlog_without_limit() {
        let (source, mut sink) = MicSource::new();
        let chunk = vec![0.5f32; 4800];
        for _ in 0..100 {
            sink(&chunk);
        }
        let buffered = source.buffer.lock().unwrap().len();
        // One second of 48kHz stereo, and not the ten seconds pushed in.
        assert!(buffered <= 48_000 * 2, "backlog grew to {buffered} samples");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn the_share_mix_takes_other_applications_and_never_our_own() {
        let graph = serde_json::json!([
            { "id": 10, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "nobilis_share_1", "media.class": "Audio/Sink" } } },
            { "id": 11, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 10, "port.direction": "in", "audio.channel": "FL" } } },
            { "id": 12, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 10, "port.direction": "in", "audio.channel": "FR" } } },
            // A film playing: both channels, one already linked.
            { "id": 20, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "Firefox", "media.class": "Stream/Output/Audio", "application.process.id": "900" } } },
            { "id": 21, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 20, "port.direction": "out", "audio.channel": "FL" } } },
            { "id": 22, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 20, "port.direction": "out", "audio.channel": "FR" } } },
            { "id": 23, "type": "PipeWire:Interface:Link", "info": { "output-port-id": 21, "input-port-id": 11 } },
            // A mono game.
            { "id": 30, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "game", "media.class": "Stream/Output/Audio" } } },
            { "id": 31, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 30, "port.direction": "out", "audio.channel": "MONO" } } },
            // Our own call audio, by node name and by process: never.
            { "id": 40, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "alsa_playback.nobilis", "media.class": "Stream/Output/Audio" } } },
            { "id": 41, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 40, "port.direction": "out", "audio.channel": "FL" } } },
            { "id": 50, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "other", "media.class": "Stream/Output/Audio", "application.process.id": 4242 } } },
            { "id": 51, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 50, "port.direction": "out", "audio.channel": "FL" } } },
            // A speaker's monitor port: never a source.
            { "id": 60, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "speakers", "media.class": "Audio/Sink" } } },
            { "id": 61, "type": "PipeWire:Interface:Port", "info": { "props": { "node.id": 60, "port.direction": "out", "audio.channel": "FL", "port.monitor": true } } }
        ]);
        let mut plan = links_to_make(&graph, "nobilis_share_1", "alsa_playback.nobilis", 4242);
        plan.sort();
        assert_eq!(plan, vec![(22, 12), (31, 11), (31, 12)]);
    }

    /// Records what the computer is playing. Ignored by default: it needs a
    /// sound server and something playing, so it is run deliberately with
    /// `cargo test -- --ignored system_capture` while a sound plays.
    #[test]
    #[ignore]
    fn system_capture_hears_what_is_played() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let loudest = Arc::new(Mutex::new(0.0f32));
        let peak = loudest.clone();
        let capture = start_system_capture(move |pcm: &[f32]| {
            seen.fetch_add(pcm.len(), Ordering::Relaxed);
            let mut p = peak.lock().unwrap();
            *p = pcm.iter().fold(*p, |m, s| m.max(s.abs()));
        })
        .expect("opening the system recording");
        std::thread::sleep(std::time::Duration::from_secs(6));
        let routed = system_capture_streams().lock().unwrap().clone();
        drop(capture);
        let samples = count.load(Ordering::Relaxed);
        let peak = *loudest.lock().unwrap();
        println!("recorded {samples} samples, peak {peak:.4}, streams {routed:?}");
        assert!(samples > 0, "the recording produced nothing");
        assert!(peak > 0.01, "silence - the recording is not on the monitor");
        assert!(system_capture_streams().lock().unwrap().is_empty(), "closing it should release its stream");
    }

    /// Opens the real default microphone. Ignored by default: it needs
    /// hardware and a running sound server, so it is run deliberately with
    /// `cargo test -- --ignored microphone`.
    #[test]
    #[ignore]
    fn microphone_delivers_audio() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let capture = start_capture(None, move |pcm: &[f32]| {
            seen.fetch_add(pcm.len(), Ordering::Relaxed);
        })
        .expect("opening the default input");

        let mut peak = 0.0f32;
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            peak = peak.max(*capture.level.lock().unwrap());
        }

        let samples = count.load(Ordering::Relaxed);
        let seconds = samples as f64 / 2.0 / TARGET_RATE as f64;
        println!("captured {samples} samples ({seconds:.2}s of 48kHz stereo), peak {peak:.4}");
        assert!(samples > 0, "the input stream produced nothing at all");
        // Three seconds of wall clock should yield roughly three seconds of
        // audio; a badly wrong resample ratio shows up here.
        assert!(seconds > 1.5, "far less audio than elapsed time: {seconds:.2}s");
    }
}
