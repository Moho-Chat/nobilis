//! Recording somebody speaking, so it can be sent as a voice message.
//!
//! Held here rather than in a backend because none of it is Discord's: a
//! recording is a microphone, some samples and a length, and the only part
//! that knows which service it is going to is the call that sends it.
//!
//! Process-wide, because there is one microphone. Two recordings at once is
//! not a thing to support - it is a thing to refuse, clearly, rather than to
//! half-handle by recording the same audio into two buffers.
//!
//! The capture path is the one calls already use, so a voice message is
//! recorded through the same device choice, the same resampling and the same
//! level meter as a call, and needs no second answer to "which microphone".

use crate::audio::{self, Capture, TARGET_CHANNELS};
use crate::oggopus;
use anyhow::{bail, Result};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// How long a recording may run before it stops itself.
///
/// Discord's own limit, and a backstop besides: a recording is held in memory
/// until it is sent, and a forgotten one should not grow there all afternoon.
/// Five minutes of 48kHz mono is about 57MB before encoding.
const MAX_SECONDS: f64 = 5.0 * 60.0;

struct Recording {
    /// Dropping this stops the microphone, so it is held for the duration.
    _capture: Capture,
    samples: Arc<Mutex<Vec<f32>>>,
    /// Where it will be sent, so stopping needs no second answer.
    buffer_id: String,
    started: Instant,
    level: Arc<Mutex<f32>>,
}

fn slot() -> &'static Mutex<Option<Recording>> {
    static SLOT: OnceLock<Mutex<Option<Recording>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Opens the microphone and starts collecting.
///
/// The captured audio is folded to mono as it arrives rather than at the end:
/// it halves what is held, and a voice message is mono either way.
pub fn start(buffer_id: &str, device_id: Option<&str>) -> Result<()> {
    let mut held = slot().lock().unwrap();
    if let Some(existing) = held.as_ref() {
        bail!("already recording a message for {}", existing.buffer_id);
    }
    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_samples = samples.clone();
    let capture = audio::start_capture(device_id, move |pcm: &[f32]| {
        let mono = oggopus::to_mono(pcm, TARGET_CHANNELS);
        let mut held = sink_samples.lock().unwrap();
        // The cap is enforced where the samples arrive rather than by a timer:
        // it is the memory that matters, and this is the only place that knows
        // how much there is.
        let room = (MAX_SECONDS * oggopus::RATE as f64) as usize;
        if held.len() < room {
            let take = (room - held.len()).min(mono.len());
            held.extend_from_slice(&mono[..take]);
        }
    })?;
    let level = capture.level.clone();
    *held = Some(Recording {
        _capture: capture,
        samples,
        buffer_id: buffer_id.to_string(),
        started: Instant::now(),
        level,
    });
    Ok(())
}

/// What a recording in progress looks like from outside.
pub struct Progress {
    pub buffer_id: String,
    pub seconds: f64,
    /// Peak level since the last look, so a window can show that the
    /// microphone is actually hearing something.
    pub level: f32,
}

pub fn progress() -> Option<Progress> {
    let held = slot().lock().unwrap();
    held.as_ref().map(|recording| Progress {
        buffer_id: recording.buffer_id.clone(),
        seconds: recording.started.elapsed().as_secs_f64(),
        level: *recording.level.lock().unwrap(),
    })
}

/// A finished recording, encoded and ready to be sent.
pub struct Finished {
    pub buffer_id: String,
    /// The file itself, on disk, because that is what the upload paths take.
    pub path: std::path::PathBuf,
    pub duration_secs: f64,
    /// Discord's own encoding: one amplitude byte per bucket, base64'd.
    pub waveform: String,
    pub samples: usize,
}

/// Stops the microphone and turns what was said into a file.
///
/// The length comes from the samples rather than from the clock: the two
/// disagree by whatever the device buffered, and the samples are what a player
/// will actually find in the file.
pub fn finish() -> Result<Finished> {
    let recording = slot().lock().unwrap().take().ok_or_else(|| anyhow::anyhow!("nothing is being recorded"))?;
    let samples = std::mem::take(&mut *recording.samples.lock().unwrap());
    if samples.is_empty() {
        bail!("the microphone produced no audio");
    }
    let duration_secs = oggopus::duration_secs(&samples);
    let waveform = base64_of(&oggopus::waveform(&samples, 256));
    let bytes = oggopus::encode(&samples)?;
    let path = oggopus::write_temp(&bytes)?;
    Ok(Finished { buffer_id: recording.buffer_id, path, duration_secs, waveform, samples: samples.len() })
}

/// Throws the recording away and closes the microphone.
pub fn cancel() -> bool {
    slot().lock().unwrap().take().is_some()
}

/// Base64, of the flavour every service means when it says base64.
fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing may be finished that was never started - and saying so is the
    /// difference between an error a window can show and a panic.
    #[test]
    fn finishing_nothing_is_an_error_rather_than_a_panic() {
        // Whatever else has run, this test's precondition is an empty slot.
        cancel();
        assert!(finish().is_err());
        assert!(!cancel(), "cancelling nothing should say so");
    }

    /// The waveform travels as base64 and has to survive the trip.
    #[test]
    fn the_waveform_encodes_the_way_the_service_reads_it() {
        assert_eq!(base64_of(&[0, 128, 255]), "AID/");
        assert_eq!(base64_of(&[]), "");
    }
}
