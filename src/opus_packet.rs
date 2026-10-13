//! Whether bytes are an Opus packet worth handing to libopus.
//!
//! libopus is C, and what it decodes here comes from other people: every
//! voice packet of everybody in a Discord call, and soundboard sounds anyone
//! in a server can upload (#251). It validates packets itself and is fuzzed
//! continuously, so this is not a second decoder - it is the structural part
//! of RFC 6716 (§3.2, requirements R1-R7) checked in Rust first, so that
//! anything malformed, or longer than the buffer it would be decoded into, is
//! refused before C code reads a byte of it.

/// The most one frame may hold (RFC 6716 R2).
const MAX_FRAME_BYTES: usize = 1275;

/// The most audio one packet may carry: 120 ms at 48 kHz (R5).
pub const MAX_PACKET_SAMPLES: usize = 5760;

/// Samples per channel at 48 kHz in one frame of the packet's configuration
/// (the top five bits of its first byte; RFC 6716 Table 2).
fn frame_samples(config: u8) -> usize {
    match config {
        // SILK-only: narrow, medium, wide band; 10, 20, 40, 60 ms.
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        // Hybrid: super-wide and full band; 10, 20 ms.
        12..=15 => [480, 960][(config % 2) as usize],
        // CELT-only: 2.5, 5, 10, 20 ms.
        _ => [120, 240, 480, 960][(config % 4) as usize],
    }
}

/// A frame length as codes 2 and 3 write one: one byte below 252, otherwise
/// two (RFC 6716 §3.2.1). Returns the length and how many bytes it took.
fn frame_length(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()? as usize;
    if first < 252 {
        return Some((first, 1));
    }
    let second = *bytes.get(1)? as usize;
    Some((second * 4 + first, 2))
}

/// The packet's audio, in samples per channel at 48 kHz, if it is a
/// well-formed Opus packet no longer than `max_samples`; None otherwise.
///
/// An empty packet is not one: an empty slice is how a caller asks the
/// decoder to conceal a lost frame, and that is the caller's own business.
pub fn plausible(packet: &[u8], max_samples: usize) -> Option<usize> {
    let toc = *packet.first()?;
    let per_frame = frame_samples(toc >> 3);
    let rest = &packet[1..];
    let frames = match toc & 0b11 {
        // One frame.
        0 => {
            if rest.len() > MAX_FRAME_BYTES {
                return None;
            }
            1
        }
        // Two frames of equal size.
        1 => {
            if !rest.len().is_multiple_of(2) || rest.len() / 2 > MAX_FRAME_BYTES {
                return None;
            }
            2
        }
        // Two frames, the first's length given.
        2 => {
            let (first, used) = frame_length(rest)?;
            let remaining = rest.len().checked_sub(used)?;
            let second = remaining.checked_sub(first)?;
            if first > MAX_FRAME_BYTES || second > MAX_FRAME_BYTES {
                return None;
            }
            2
        }
        // Any number of frames, with an optional padding run.
        _ => {
            let header = *rest.first()?;
            let vbr = header & 0x80 != 0;
            let padded = header & 0x40 != 0;
            let count = (header & 0x3f) as usize;
            if count == 0 || count * per_frame > MAX_PACKET_SAMPLES {
                return None;
            }
            let mut at = 1;
            let mut padding = 0usize;
            if padded {
                loop {
                    let b = *rest.get(at)? as usize;
                    at += 1;
                    if b == 255 {
                        padding += 254;
                    } else {
                        padding += b;
                        break;
                    }
                }
            }
            if vbr {
                let mut lengths = 0usize;
                for _ in 0..count - 1 {
                    let (len, used) = frame_length(rest.get(at..)?)?;
                    if len > MAX_FRAME_BYTES {
                        return None;
                    }
                    at += used;
                    lengths += len;
                }
                let data = rest.len().checked_sub(at)?.checked_sub(padding)?;
                let last = data.checked_sub(lengths)?;
                if last > MAX_FRAME_BYTES {
                    return None;
                }
            } else {
                let data = rest.len().checked_sub(at)?.checked_sub(padding)?;
                if !data.is_multiple_of(count) || data / count > MAX_FRAME_BYTES {
                    return None;
                }
            }
            count
        }
    };
    let samples = frames * per_frame;
    (samples <= max_samples.min(MAX_PACKET_SAMPLES)).then_some(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 20 ms CELT full-band frame, config 31: TOC 0b11111_s_cc.
    const CELT_20MS: u8 = 31 << 3;

    #[test]
    fn ordinary_voice_packets_pass() {
        // Code 0: one 20 ms frame of 60 bytes, what Discord voice is.
        let mut p = vec![CELT_20MS];
        p.extend(std::iter::repeat_n(0u8, 60));
        assert_eq!(plausible(&p, 2880), Some(960));
        // Code 1: two equal frames, 40 ms.
        let mut p = vec![CELT_20MS | 1];
        p.extend(std::iter::repeat_n(0u8, 80));
        assert_eq!(plausible(&p, 2880), Some(1920));
        // Code 2: two frames, the first 10 bytes long.
        let mut p = vec![CELT_20MS | 2, 10];
        p.extend(std::iter::repeat_n(0u8, 30));
        assert_eq!(plausible(&p, 2880), Some(1920));
        // Code 3, CBR, three 2.5 ms frames of 20 bytes.
        let mut p = vec![(16 << 3) | 3, 3];
        p.extend(std::iter::repeat_n(0u8, 60));
        assert_eq!(plausible(&p, 2880), Some(360));
    }

    #[test]
    fn malformed_packets_are_refused() {
        assert_eq!(plausible(&[], 5760), None, "empty is the caller's concealment, not a packet");
        // Code 0 with a frame over 1275 bytes.
        let mut p = vec![CELT_20MS];
        p.extend(std::iter::repeat_n(0u8, 1276));
        assert_eq!(plausible(&p, 5760), None);
        // Code 1 with an odd length.
        assert_eq!(plausible(&[CELT_20MS | 1, 0, 0, 0], 5760), None);
        // Code 2 whose first frame claims more than the packet holds.
        assert_eq!(plausible(&[CELT_20MS | 2, 200, 0, 0], 5760), None);
        // Code 2 with its two-byte length cut off.
        assert_eq!(plausible(&[CELT_20MS | 2, 253], 5760), None);
        // Code 3 with no frames.
        assert_eq!(plausible(&[CELT_20MS | 3, 0], 5760), None);
        // Code 3, CBR, data that does not divide among the frames.
        assert_eq!(plausible(&[CELT_20MS | 3, 2, 0, 0, 0], 5760), None);
        // Code 3 whose padding runs past the end.
        assert_eq!(plausible(&[CELT_20MS | 3, 0x40 | 1, 255], 5760), None);
        // Code 3, VBR, lengths summing past the data.
        assert_eq!(plausible(&[CELT_20MS | 3, 0x80 | 2, 50, 0, 0], 5760), None);
    }

    #[test]
    fn too_long_is_refused() {
        // Code 3 with 7 frames of 20 ms: 140 ms, over Opus's own 120.
        let mut p = vec![CELT_20MS | 3, 7];
        p.extend(std::iter::repeat_n(0u8, 70));
        assert_eq!(plausible(&p, 5760), None);
        // Three 20 ms frames: legal Opus, but over a 60 ms voice buffer.
        let mut p = vec![CELT_20MS | 3, 3];
        p.extend(std::iter::repeat_n(0u8, 30));
        assert_eq!(plausible(&p, 5760), Some(2880));
        assert_eq!(plausible(&p, 2880), Some(2880));
        assert_eq!(plausible(&p, 1920), None);
        // A 60 ms SILK frame, config 3.
        assert_eq!(plausible(&[3 << 3, 0, 0], 2880), Some(2880));
    }

    /// Whatever arrives, the check itself never panics.
    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 64) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8 ^ i as u8).collect();
            let _ = plausible(&bytes, 5760);
        }
    }
}

#[cfg(test)]
mod real_packets {
    use super::plausible;

    /// What libopus itself produces must never be refused: a false refusal is
    /// a gap in somebody's voice. Every frame size, voice and music modes,
    /// low and high bitrates, mono speech-like noise and stereo.
    #[test]
    fn everything_the_encoder_makes_passes() {
        let mut seed = 12345u32;
        for &app in &[opus2::Application::Voip, opus2::Application::Audio, opus2::Application::LowDelay] {
            for &bitrate in &[8_000, 32_000, 64_000, 128_000, 510_000] {
                // 2.5, 5, 10, 20, 40, 60 ms per channel at 48 kHz.
                for &frame in &[120usize, 240, 480, 960, 1920, 2880] {
                    let mut enc = opus2::Encoder::new(48_000, opus2::Channels::Stereo, app).unwrap();
                    enc.set_bitrate(opus2::Bitrate::Bits(bitrate)).unwrap();
                    let pcm: Vec<i16> = (0..frame * 2)
                        .map(|i| {
                            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                            (((i as f32 * 0.05).sin() * 8000.0) as i32 + (seed >> 20) as i32 - 2048) as i16
                        })
                        .collect();
                    let mut out = vec![0u8; 4000];
                    let n = enc.encode(&pcm, &mut out).unwrap();
                    assert_eq!(
                        plausible(&out[..n], 2880),
                        Some(frame),
                        "{app:?} at {bitrate} b/s, {frame} samples: refused a packet libopus made"
                    );
                }
            }
        }
    }
}
