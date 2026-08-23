//! Sound devices, and getting a microphone into a voice connection.
//!
//! This lives in the daemon for the same reason the voice connection does:
//! the mixer and the encoder are Songbird's, in this process, and moving raw
//! audio across the RPC socket to be encoded somewhere else would add latency
//! and buffering to solve a problem that does not exist if they sit together.
//! A frontend asks to join, leave, mute, or use a different device.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What Discord's voice protocol expects, and therefore what everything is
/// converted to before it reaches Songbird.
pub const TARGET_RATE: u32 = 48_000;
pub const TARGET_CHANNELS: u16 = 2;

#[derive(Serialize, Clone, Debug)]
pub struct AudioDevice {
    pub name: String,
    /// What the driver says this is - microphone, speaker, headset and so on.
    /// Carried so a frontend can show something better than a bare string.
    #[serde(rename = "deviceType")]
    pub device_type: String,
    /// Whether this is the host's current default. Named rather than assumed,
    /// since "default" is a moving target the user changes outside this app.
    #[serde(rename = "isDefault")]
    pub is_default: bool,
    pub kind: String,
}

/// The devices this machine offers.
///
/// Failure here is reported rather than fatal: a machine with no sound card,
/// or a session with no audio server running, is a perfectly ordinary thing
/// for a chat client to encounter, and everything except voice still works.
/// The devices worth offering.
///
/// On a PipeWire or PulseAudio system - which is to say almost any current
/// Linux desktop - ALSA presents its whole plugin chain as devices: rate
/// converters, channel mixers, a null sink, over a hundred entries on this
/// machine alone. cpal reports them all, classifies every one of them
/// "Unknown", and gives real hardware the same treatment, so there is nothing
/// in the data to filter on and filtering by name would be guesswork.
///
/// So this offers the default and lets the sound server route it. That is not
/// a workaround but the native arrangement: the app appears in pavucontrol as
/// a stream, and its input is changed there, per application, while it runs.
/// A genuine per-device picker needs PipeWire's own enumeration rather than
/// ALSA's, which is worth doing when there is a settings page to hang it on.
pub fn list_devices() -> Result<Vec<AudioDevice>> {
    let host = cpal::default_host();
    let mut out = Vec::new();

    if let Some(device) = host.default_input_device() {
        out.push(AudioDevice {
            name: device.to_string(),
            device_type: "input".to_string(),
            is_default: true,
            kind: "input".to_string(),
        });
    }
    if let Some(device) = host.default_output_device() {
        out.push(AudioDevice {
            name: device.to_string(),
            device_type: "output".to_string(),
            is_default: true,
            kind: "output".to_string(),
        });
    }
    Ok(out)
}

fn input_device(preferred: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if let Some(want) = preferred {
        for device in host.input_devices().context("listing input devices")? {
            if device.to_string() == want {
                return Ok(device);
            }
        }
        // Deliberately an error rather than a silent fallback: someone who
        // chose a device would rather be told it is gone than be recorded on
        // a different one without knowing.
        return Err(anyhow!("input device {want:?} is not available"));
    }
    host.default_input_device().context("no default input device")
}

/// A running microphone capture.
///
/// Dropping it stops the stream. The captured audio is pushed into `sink`,
/// already converted to what Songbird wants.
pub struct Capture {
    _stream: cpal::Stream,
    pub level: Arc<Mutex<f32>>,
    pub active: Arc<AtomicBool>,
}

/// Starts capturing from `device_name`, handing each converted chunk to `sink`.
///
/// The conversion is deliberately simple: nearest-neighbour resampling and
/// channel duplication. It is enough to carry speech correctly and keeps this
/// dependency-free; anything better belongs behind a resampler crate if the
/// difference ever proves audible.
pub fn start_capture<F>(device_name: Option<&str>, mut sink: F) -> Result<Capture>
where
    F: FnMut(&[i16]) + Send + 'static,
{
    let device = input_device(device_name)?;
    let config = device.default_input_config().context("querying the input device")?;
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
            out.push((l.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            out.push((r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
        sink(&out);
    };

    let err = |e| tracing::warn!("audio: capture stream error: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config.clone().into(),
            move |data: &[f32], _: &_| convert(data),
            err,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config.clone().into(),
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
    Ok(Capture { _stream: stream, level, active })
}


#[cfg(test)]
mod tests {
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
        assert!(devices.iter().filter(|d| d.kind == "input").count() <= 1);
    }

    #[test]
    fn a_missing_device_is_an_error_rather_than_a_substitution() {
        // Being recorded on a different microphone than the one chosen is
        // worse than being told the choice is unavailable.
        let err = input_device(Some("no such microphone")).unwrap_err();
        assert!(err.to_string().contains("not available"), "unexpected: {err}");
    }

    /// Opens the real default microphone. Ignored by default: it needs
    /// hardware and a running sound server, so it is run deliberately with
    /// `cargo test -- --ignored microphone`.
    #[test]
    #[ignore]
    fn microphone_delivers_audio() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let capture = start_capture(None, move |pcm: &[i16]| {
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
