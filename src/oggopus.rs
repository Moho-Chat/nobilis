//! Turning captured microphone audio into the file a voice message is.
//!
//! Discord's own client records Ogg/Opus, and so does everything that plays
//! one back. The pieces were nearly all here already - cpal captures the
//! microphone for calls, and libopus is linked for them too - but nothing in
//! this daemon had ever written an Ogg container, because a call puts Opus
//! packets straight onto the wire with no container at all.
//!
//! So this is the missing part, and it is small: Ogg is a framing format for
//! packets, and Opus-in-Ogg is three rules on top of it (RFC 7845) - a header
//! packet, a tags packet, then the audio, with the granule position counted in
//! 48kHz samples regardless of what the encoder was actually fed.
//!
//! Written here rather than taken from a crate because it is this much code
//! and the alternative is a dependency whose only use would be these eighty
//! lines. What makes that a reasonable trade is that the result is checkable:
//! the tests hand the output to ffprobe, which is a decoder nobody here wrote.

use anyhow::{Context, Result};
use opus2::{Application, Channels, Encoder};

/// The rate everything speaks in: Opus is defined at 48kHz, the capture path
/// already resamples to it, and an Ogg granule position is counted in it.
pub const RATE: u32 = 48_000;

/// 20ms of audio per packet.
///
/// What every voice application uses, and what Discord's own recordings carry.
/// Smaller packets cost proportionally more header; larger ones lose more when
/// one goes missing, which matters for a call and not for a file, but there is
/// no reason to differ from what every decoder is best exercised on.
const FRAME_MS: u32 = 20;
const FRAME_SAMPLES: usize = (RATE as usize / 1000) * FRAME_MS as usize;

/// Encodes mono 48kHz samples into an Ogg/Opus file.
///
/// Mono because it is somebody talking into one microphone: stereo would
/// double the size to say the same thing twice.
pub fn encode(samples: &[f32]) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new(RATE, Channels::Mono, Application::Voip)
        .map_err(|e| anyhow::anyhow!("starting the Opus encoder: {e}"))?;

    let mut ogg = Ogg::new(rand_serial());
    // The identification header, RFC 7845 section 5.1. The pre-skip is the
    // encoder's own latency, which a player trims from the front; libopus at
    // 48kHz reports 312 samples and is asked rather than assumed.
    let pre_skip = encoder.get_lookahead().unwrap_or(312) as u16;
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(1); // channels
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&RATE.to_le_bytes()); // the original rate, for information
    head.extend_from_slice(&0u16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family 0: mono or stereo, no table
    ogg.page(&[&head], 0, true, false);

    // The comment header, which is required even when there is nothing to say.
    let vendor = b"moho";
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments
    ogg.page(&[&tags], 0, false, false);

    // The audio itself, one page per packet. A page per packet is more header
    // than a long recording needs, and is what keeps this simple enough to be
    // obviously right; a minute of speech costs about 1.4KB of framing.
    let mut buffer = vec![0u8; 4000];
    let mut granule: u64 = 0;
    let frames: Vec<&[f32]> = samples.chunks(FRAME_SAMPLES).collect();
    for (i, frame) in frames.iter().enumerate() {
        // The last frame is padded rather than dropped: a partial frame is
        // still something somebody said, and Opus only encodes whole ones.
        let mut whole;
        let input: &[f32] = if frame.len() == FRAME_SAMPLES {
            frame
        } else {
            whole = frame.to_vec();
            whole.resize(FRAME_SAMPLES, 0.0);
            &whole
        };
        let n = encoder
            .encode_float(input, &mut buffer)
            .map_err(|e| anyhow::anyhow!("encoding {FRAME_MS}ms of audio: {e}"))?;
        granule += FRAME_SAMPLES as u64;
        ogg.page(&[&buffer[..n]], granule, false, i + 1 == frames.len());
    }
    // A recording of nothing still has to be a valid file: end the stream even
    // when no audio packet was ever written.
    if frames.is_empty() {
        ogg.page(&[&[]], 0, false, true);
    }
    Ok(ogg.done())
}

/// How long the recording runs, in seconds, as the service wants it.
pub fn duration_secs(samples: &[f32]) -> f64 {
    samples.len() as f64 / RATE as f64
}

/// The picture of the sound that travels with the message.
///
/// Discord takes up to 256 bytes, one amplitude per bucket, and draws them as
/// bars. Peak rather than mean within a bucket: a mean over a fifth of a
/// second of speech is close to silence, and produces the flat line that makes
/// some clients' waveforms look broken.
pub fn waveform(samples: &[f32], buckets: usize) -> Vec<u8> {
    if samples.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let buckets = buckets.min(samples.len());
    let step = samples.len() as f64 / buckets as f64;
    (0..buckets)
        .map(|i| {
            let from = (i as f64 * step) as usize;
            let to = (((i + 1) as f64 * step) as usize).max(from + 1).min(samples.len());
            let peak = samples[from..to].iter().fold(0.0f32, |m, s| m.max(s.abs()));
            (peak.clamp(0.0, 1.0) * 255.0).round() as u8
        })
        .collect()
}

/// A stream serial number, which only has to differ between streams in one
/// file. There is always exactly one stream here, so anything does - but a
/// fixed number would make two files concatenated by accident look like one
/// stream with a broken sequence, which is a confusing thing to debug.
fn rand_serial() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1) | 1
}

/// The Ogg framing itself.
struct Ogg {
    out: Vec<u8>,
    serial: u32,
    sequence: u32,
}

impl Ogg {
    fn new(serial: u32) -> Self {
        Self { out: Vec::new(), serial, sequence: 0 }
    }

    /// One page carrying whole packets.
    ///
    /// Every packet here fits in a page - an Opus frame at any sane bitrate is
    /// far under the 255 * 255 bytes a page can hold - so this deliberately
    /// does not implement continuation across pages. A packet that would not
    /// fit cannot be produced by the encoder above, and pretending to handle a
    /// case that cannot arise would be untested code in a file format parser.
    fn page(&mut self, packets: &[&[u8]], granule: u64, first: bool, last: bool) {
        let mut segments: Vec<u8> = Vec::new();
        let mut body: Vec<u8> = Vec::new();
        for packet in packets {
            let mut left = packet.len();
            while left >= 255 {
                segments.push(255);
                left -= 255;
            }
            segments.push(left as u8);
            body.extend_from_slice(packet);
        }

        let mut header = Vec::with_capacity(27 + segments.len());
        header.extend_from_slice(b"OggS");
        header.push(0); // stream structure version
        header.push(if first { 0x02 } else if last { 0x04 } else { 0x00 });
        header.extend_from_slice(&granule.to_le_bytes());
        header.extend_from_slice(&self.serial.to_le_bytes());
        header.extend_from_slice(&self.sequence.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes()); // checksum, filled in below
        header.push(segments.len() as u8);
        header.extend_from_slice(&segments);

        let mut page = header;
        page.extend_from_slice(&body);
        let crc = crc32(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());

        self.out.extend_from_slice(&page);
        self.sequence += 1;
    }

    fn done(self) -> Vec<u8> {
        self.out
    }
}

/// Ogg's own CRC32, which is not the common one.
///
/// The polynomial is the same 0x04C11DB7, but with no input or output
/// reflection and a zero initial and final value - so a stock crc32 gives a
/// number every decoder rejects. This is the one detail of the format most
/// worth writing down, because a wrong checksum fails as "not an Ogg file"
/// rather than as a checksum problem.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// Reads a whole recording out of the capture format into what `encode` wants.
///
/// The capture path hands over interleaved stereo at 48kHz because that is
/// what a call needs; a voice message is mono, so the two channels are averaged
/// rather than one being thrown away - dropping a channel loses half the sound
/// on a microphone that happens to be wired to the other one.
pub fn to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels as usize)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Writes the encoded recording somewhere it can be uploaded from.
pub fn write_temp(bytes: &[u8]) -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("moho-voice");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("voice-message-{}.ogg", rand_serial()));
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second of a 440Hz tone, which is something a decoder can check the
    /// length of and a person can listen to if this ever needs debugging.
    fn tone(seconds: f64) -> Vec<f32> {
        let n = (RATE as f64 * seconds) as usize;
        (0..n).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5).collect()
    }

    #[test]
    fn the_file_begins_like_an_ogg_opus_file() {
        let bytes = encode(&tone(0.2)).expect("encoding");
        assert_eq!(&bytes[0..4], b"OggS", "not an Ogg stream");
        // The identification header is the first packet of the first page.
        assert_eq!(&bytes[28..36], b"OpusHead");
        assert!(bytes.windows(8).any(|w| w == b"OpusTags"), "the comment header is required");
    }

    /// The checksum is the detail most likely to be wrong and least likely to
    /// say so: a decoder reports a bad one as "not an Ogg file".
    #[test]
    fn the_checksum_is_oggs_own() {
        // Known answers, computed from the polynomial by a second
        // implementation written in Python rather than taken from this one -
        // a self-consistent checksum test proves only that the code agrees
        // with itself, which is exactly the failure being guarded against.
        assert_eq!(crc32(b"OggS"), 0x5fb0_a94f);
        assert_eq!(crc32(b"a"), 0xa864_db20);
        assert_eq!(crc32(&[]), 0);
    }

    /// The test that matters for a container written by hand: hand it to a
    /// decoder nobody here wrote and see whether it agrees about what it is
    /// and how long it lasts.
    ///
    /// Ignored by default because it shells out to ffprobe, which is not
    /// something a test suite should require:
    ///   cargo test --release -- --ignored ffprobe
    #[test]
    #[ignore]
    fn ffprobe_agrees_that_this_is_two_seconds_of_opus() {
        let bytes = encode(&tone(2.0)).expect("encoding");
        let path = write_temp(&bytes).expect("writing");
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=format_name,duration:stream=codec_name,channels,sample_rate", "-of", "default=nw=1", "-i"])
            .arg(&path)
            .output()
            .expect("running ffprobe");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let errors = String::from_utf8_lossy(&out.stderr).to_string();
        println!("{text}");
        assert!(out.status.success(), "ffprobe refused the file: {errors}");
        assert!(errors.is_empty(), "ffprobe complained: {errors}");
        assert!(text.contains("codec_name=opus"), "not opus: {text}");
        assert!(text.contains("format_name=ogg"), "not ogg: {text}");
        assert!(text.contains("channels=1"), "not mono: {text}");
        assert!(text.contains("sample_rate=48000"), "wrong rate: {text}");
        let duration: f64 = text
            .lines()
            .find_map(|l| l.strip_prefix("duration="))
            .and_then(|v| v.parse().ok())
            .expect("a duration");
        assert!((duration - 2.0).abs() < 0.05, "ffprobe reads {duration}s, not 2s");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_recording_of_nothing_is_still_a_file() {
        let bytes = encode(&[]).expect("encoding silence");
        assert_eq!(&bytes[0..4], b"OggS");
        assert_eq!(duration_secs(&[]), 0.0);
    }

    #[test]
    fn the_length_is_what_was_recorded() {
        assert!((duration_secs(&tone(2.5)) - 2.5).abs() < 0.001);
    }

    /// Peak per bucket, not mean: speech averaged over a fifth of a second is
    /// nearly silence, which is what makes a waveform look broken.
    #[test]
    fn the_waveform_follows_the_sound() {
        let mut samples = vec![0.0f32; 4800];
        samples[2400] = 1.0;
        let bars = waveform(&samples, 4);
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[0], 0, "silence should read as silence");
        assert_eq!(bars[2], 255, "a peak should read as a peak");
    }

    #[test]
    fn the_waveform_of_nothing_is_nothing() {
        assert!(waveform(&[], 256).is_empty());
        assert!(waveform(&[0.5], 0).is_empty());
    }

    #[test]
    fn two_channels_become_one_without_losing_a_side() {
        // Everything on the right channel: averaging keeps it, picking the
        // first channel would produce silence.
        let stereo = vec![0.0, 1.0, 0.0, 1.0];
        assert_eq!(to_mono(&stereo, 2), vec![0.5, 0.5]);
        assert_eq!(to_mono(&[0.25, 0.75], 1), vec![0.25, 0.75]);
    }
}
