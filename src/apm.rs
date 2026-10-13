//! What a call's microphone goes through before it is sent.
//!
//! A Matrix call takes its microphone from the window, where Chromium has
//! already run WebRTC's audio processing over it - echo cancellation, noise
//! suppression, a high-pass filter. A Discord call's microphone is a raw
//! capture in this process, and had none of that: anybody on speakers sent
//! the call back into itself (#232). This is the same processing, from a
//! pure-Rust port of it, between the capture and the encoder.
//!
//! The echo canceller needs to hear what the speakers are playing, which is
//! the call's own mix: the voice clock hands each 20ms of it here as it plays
//! it, and each 20ms of microphone after. Both are 48kHz stereo, interleaved,
//! in the 10ms frames WebRTC works in.

use sonora::config::{EchoCanceller, HighPassFilter, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig};

const RATE: usize = 48_000;
const CHANNELS: usize = 2;
/// One 10ms frame, interleaved.
const CHUNK: usize = RATE / 100 * CHANNELS;

pub struct CallProcessor {
    apm: AudioProcessing,
    echo: bool,
    noise: bool,
}

impl CallProcessor {
    pub fn new(echo: bool, noise: bool) -> Self {
        let apm = AudioProcessing::builder()
            .config(config(echo, noise))
            .capture_config(StreamConfig::new(RATE as u32, CHANNELS as u16))
            .render_config(StreamConfig::new(RATE as u32, CHANNELS as u16))
            .build();
        CallProcessor { apm, echo, noise }
    }

    /// Follows the settings, which can change during a call.
    pub fn set(&mut self, echo: bool, noise: bool) {
        if (echo, noise) != (self.echo, self.noise) {
            self.echo = echo;
            self.noise = noise;
            self.apm.apply_config(config(echo, noise));
        }
    }

    /// What the speakers are playing right now - silence included, because
    /// the canceller keeps time by it.
    pub fn render(&mut self, played: &[i16]) {
        if !self.echo {
            return;
        }
        let mut scratch = [0i16; CHUNK];
        for chunk in played.as_chunks::<CHUNK>().0 {
            let _ = self.apm.process_render_i16(chunk, &mut scratch);
        }
    }

    /// The microphone, cleaned in place.
    pub fn capture(&mut self, frame: &mut [f32]) {
        if !self.echo && !self.noise {
            return;
        }
        let mut src = [0i16; CHUNK];
        let mut out = [0i16; CHUNK];
        for chunk in frame.as_chunks_mut::<CHUNK>().0 {
            for (s, f) in src.iter_mut().zip(chunk.iter()) {
                *s = (f.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            }
            if self.apm.process_capture_i16(&src, &mut out).is_ok() {
                for (f, s) in chunk.iter_mut().zip(out.iter()) {
                    *f = *s as f32 / i16::MAX as f32;
                }
            }
        }
    }
}

fn config(echo: bool, noise: bool) -> Config {
    Config {
        // Always on: it takes out the rumble below speech, which nobody
        // wants sent, and the echo canceller expects it.
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: echo.then(EchoCanceller::default),
        noise_suppression: noise.then(NoiseSuppression::default),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone played out of the speakers and heard back by the microphone
    /// comes out of the canceller much quieter than it went in.
    #[test]
    fn an_echo_of_what_was_played_is_taken_out() {
        let mut apm = CallProcessor::new(true, false);
        let tone = |n: usize| -> f32 { (n as f32 * 2.0 * std::f32::consts::PI * 440.0 / RATE as f32).sin() * 0.3 };
        let mut heard_in = 0.0f32;
        let mut heard_out = 0.0f32;
        // Ten seconds, so the canceller has time to converge; measured over
        // the last two.
        for tick in 0..500usize {
            let base = tick * 960;
            let played: Vec<i16> = (0..960).flat_map(|i| {
                let v = (tone(base + i) * i16::MAX as f32) as i16;
                [v, v]
            }).collect();
            apm.render(&played);
            // The room: the same tone, a little later and quieter.
            let mut mic: Vec<f32> = (0..960).flat_map(|i| {
                let v = if base + i >= 480 { tone(base + i - 480) * 0.5 } else { 0.0 };
                [v, v]
            }).collect();
            let before: f32 = mic.iter().map(|s| s * s).sum();
            apm.capture(&mut mic);
            if tick >= 400 {
                heard_in += before;
                heard_out += mic.iter().map(|s| s * s).sum::<f32>();
            }
        }
        assert!(heard_out < heard_in * 0.1, "echo energy went {heard_in} -> {heard_out}");
    }

    #[test]
    fn off_is_untouched() {
        let mut apm = CallProcessor::new(false, false);
        let mut mic = vec![0.25f32; 1920];
        apm.capture(&mut mic);
        assert!(mic.iter().all(|s| *s == 0.25));
    }
}
