//! Keeping a received picture whole on a network that loses packets.
//!
//! What every WebRTC receiver does, in three layers, because the reassembler
//! on its own can only refuse to hand out a broken frame - it cannot mend
//! one:
//!
//! 1. Ask for a lost packet again (a NACK). The relay keeps the last moments
//!    of every stream and resends on a second SSRC (RTX), so a loss mended
//!    within a round trip is never seen at all.
//! 2. When a loss is not mended in time, stop decoding. An inter frame drawn
//!    over a picture the decoder never had is the smear that spreads across a
//!    moving camera; nothing is better than that.
//! 3. Ask the sender for a keyframe (a PLI) so the picture restarts at once,
//!    rather than whenever the sender next happens to send one - which on a
//!    phone can be many seconds.
//!
//! Both kinds of request are RTCP, sealed the way this connection seals
//! everything else, sent back down the socket the picture arrived on.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::reassemble::{Frame, Reassembler, DEFAULT_WINDOW};
use super::rtp::Packet;
use super::voicecrypto::Sealer;

/// The payload type retransmissions arrive under, beside VP8's 103.
pub const PAYLOAD_TYPE_RTX: u8 = 104;

/// How long to wait before asking for the same packet again. About a round
/// trip to the relay.
const NACK_AGAIN_AFTER: Duration = Duration::from_millis(100);
/// How many times to ask for one packet before leaving it to the keyframe.
const NACK_TRIES: u8 = 2;
/// The fewest milliseconds between two keyframe requests. A keyframe is many
/// times the size of an inter frame, and asking faster than one can arrive
/// only makes the sender produce more of them into the same congestion.
const PLI_EVERY: Duration = Duration::from_millis(500);

/// What happened to one picture, for the log.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub frames: u64,
    pub gaps: u64,
    pub nacked: u64,
    pub resent: u64,
    pub plis: u64,
    pub dropped: u64,
}

/// One sender's picture: reassembly, recovery, and whether the decoder may be
/// given what comes out.
pub struct VideoReceiver {
    window: Reassembler,
    waiting_for_keyframe: bool,
    /// Sequence numbers asked for, when last, and how many times.
    asked: HashMap<u16, (Instant, u8)>,
    last_pli: Option<Instant>,
    pub stats: Stats,
}

impl Default for VideoReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoReceiver {
    pub fn new() -> Self {
        Self {
            window: Reassembler::new(DEFAULT_WINDOW),
            waiting_for_keyframe: true,
            asked: HashMap::new(),
            last_pli: None,
            stats: Stats::default(),
        }
    }

    /// A packet, as first sent or as recovered from a retransmission.
    pub fn push(&mut self, packet: Packet, resent: bool, now: Instant) -> Vec<Frame> {
        if resent {
            self.stats.resent += 1;
        }
        self.asked.remove(&packet.sequence);
        self.window.push_at(packet, now)
    }

    /// The sequence numbers worth asking for now.
    pub fn due_for_asking(&mut self, now: Instant) -> Vec<u16> {
        let missing = self.window.missing(64);
        self.asked.retain(|sequence, _| missing.contains(sequence));
        let mut ask = Vec::new();
        for sequence in missing {
            let entry = self.asked.entry(sequence).or_insert((now - NACK_AGAIN_AFTER, 0));
            if entry.1 < NACK_TRIES && now.duration_since(entry.0) >= NACK_AGAIN_AFTER {
                *entry = (now, entry.1 + 1);
                ask.push(sequence);
            }
        }
        self.stats.nacked += ask.len() as u64;
        ask
    }

    /// Whether a whole, decrypted frame may go to the decoder.
    ///
    /// No after a loss that was not mended, until a keyframe - and no before
    /// the first keyframe, for the same reason.
    pub fn admit(&mut self, frame: &Frame, keyframe: bool) -> bool {
        if frame.after_gap {
            self.stats.gaps += 1;
            if !keyframe {
                self.waiting_for_keyframe = true;
            }
        }
        if self.waiting_for_keyframe {
            if !keyframe {
                self.stats.dropped += 1;
                return false;
            }
            self.waiting_for_keyframe = false;
        }
        self.stats.frames += 1;
        true
    }

    /// Whether a keyframe should be asked for now: while the decoder is
    /// waiting for one, no oftener than `PLI_EVERY`.
    pub fn wants_keyframe(&mut self, now: Instant) -> bool {
        if !self.waiting_for_keyframe || self.last_pli.is_some_and(|last| now.duration_since(last) < PLI_EVERY) {
            return false;
        }
        self.last_pli = Some(now);
        self.stats.plis += 1;
        true
    }

    #[cfg(test)]
    pub fn is_waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }
}

/// A retransmission's payload: the original sequence number, then the
/// original payload (RFC 4588). None for the padding-only packets a sender
/// uses to probe bandwidth, which carry nothing to recover.
pub fn unwrap_rtx(payload: &[u8]) -> Option<(u16, Vec<u8>)> {
    if payload.len() <= 2 {
        return None;
    }
    Some((u16::from_be_bytes([payload[0], payload[1]]), payload[2..].to_vec()))
}

/// Every retransmission SSRC an `op 12` names, with the SSRC it resends for.
pub fn rtx_pairs(d: &Value) -> Vec<(u32, u32)> {
    let mut pairs = Vec::new();
    for stream in d["streams"].as_array().into_iter().flatten() {
        let media = stream["ssrc"].as_u64().filter(|s| *s != 0);
        let rtx = stream["rtx_ssrc"].as_u64().filter(|s| *s != 0);
        if let (Some(media), Some(rtx)) = (media, rtx) {
            pairs.push((rtx as u32, media as u32));
        }
    }
    if let (Some(media), Some(rtx)) = (
        d["video_ssrc"].as_u64().filter(|s| *s != 0),
        d["rtx_ssrc"].as_u64().filter(|s| *s != 0),
    ) {
        pairs.push((rtx as u32, media as u32));
    }
    pairs
}

/// A generic NACK (RFC 4585 §6.2.1) for `lost`, from `sender` about `media`.
pub fn nack(sender: u32, media: u32, lost: &[u16]) -> Vec<u8> {
    let mut sorted = lost.to_vec();
    sorted.sort_by_key(|s| *s);
    sorted.dedup();
    // Each entry names one packet and a bitmask of the sixteen after it.
    let mut entries: Vec<(u16, u16)> = Vec::new();
    for sequence in sorted {
        if let Some((first, mask)) = entries.last_mut() {
            let offset = sequence.wrapping_sub(*first);
            if (1..=16).contains(&offset) {
                *mask |= 1 << (offset - 1);
                continue;
            }
        }
        entries.push((sequence, 0));
    }
    let mut packet = feedback_header(205, 1, sender, media, entries.len());
    for (first, mask) in entries {
        packet.extend_from_slice(&first.to_be_bytes());
        packet.extend_from_slice(&mask.to_be_bytes());
    }
    packet
}

/// A picture loss indication (RFC 4585 §6.3.1): send a keyframe.
pub fn pli(sender: u32, media: u32) -> Vec<u8> {
    feedback_header(206, 1, sender, media, 0)
}

fn feedback_header(payload_type: u8, format: u8, sender: u32, media: u32, words: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(12 + words * 4);
    packet.push(0x80 | format);
    packet.push(payload_type);
    packet.extend_from_slice(&((2 + words) as u16).to_be_bytes());
    packet.extend_from_slice(&sender.to_be_bytes());
    packet.extend_from_slice(&media.to_be_bytes());
    packet
}

/// How many leading bytes of an RTCP packet are left in the clear and
/// authenticated: its header and the sender's SSRC, as for Discord's own
/// clients - the RTCP equivalent of the RTP fixed header.
pub const RTCP_CLEAR: usize = 8;

/// Seals an RTCP packet for the wire.
pub fn seal_rtcp(sealer: &mut Sealer, packet: &[u8]) -> anyhow::Result<Vec<u8>> {
    sealer.seal(&packet[..RTCP_CLEAR], &packet[RTCP_CLEAR..])
}

/// Whether a datagram is RTCP rather than RTP: its second byte, read as RTP,
/// is a marker bit and a payload type from 72 (sender report, 200) to 78
/// (payload-specific feedback, 206).
pub fn is_rtcp(datagram: &[u8]) -> bool {
    datagram.len() >= 2 && (200..=206).contains(&datagram[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::discord::voicecrypto::{open_at, Mode};

    fn vp8(sequence: u16, start: bool, marker: bool, key: bool) -> Packet {
        // Descriptor: S bit for a first packet; then a VP8 byte whose low bit
        // clear means keyframe.
        let mut payload = vec![if start { 0x10 } else { 0x00 }];
        payload.push(if key { 0x00 } else { 0x01 });
        payload.extend_from_slice(&[0x9d, 0x01, 0x2a]);
        Packet { payload_type: 103, sequence, timestamp: sequence as u32 * 3000, ssrc: 7, marker, payload }
    }

    #[test]
    fn a_nack_packs_nearby_losses_into_one_entry() {
        let packet = nack(1, 2, &[100, 101, 103, 117, 200]);
        assert_eq!(&packet[..4], &[0x81, 205, 0, 5], "six words in all, written as one fewer");
        assert_eq!(&packet[4..12], &[0, 0, 0, 1, 0, 0, 0, 2]);
        assert_eq!(&packet[12..16], &[0, 100, 0, 0b101], "101 is bit 0, 103 bit 2; 117 is too far");
        assert_eq!(&packet[16..20], &[0, 117, 0, 0]);
        assert_eq!(&packet[20..24], &[0, 200, 0, 0]);
    }

    #[test]
    fn a_pli_is_twelve_bytes() {
        assert_eq!(pli(1, 2), vec![0x81, 206, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2]);
    }

    #[test]
    fn rtcp_is_sealed_with_its_first_eight_bytes_in_the_clear() {
        let key = [7u8; 32];
        let mut sealer = Sealer::new(Mode::Aes256Gcm, &key).unwrap();
        let packet = nack(1, 2, &[5]);
        let sealed = seal_rtcp(&mut sealer, &packet).unwrap();
        assert!(is_rtcp(&sealed));
        assert_eq!(&sealed[..8], &packet[..8]);
        assert_eq!(open_at(Mode::Aes256Gcm, &key, &sealed, 8, 8).unwrap(), &packet[8..]);
    }

    #[test]
    fn a_retransmission_gives_back_its_original_sequence() {
        assert_eq!(unwrap_rtx(&[0x01, 0x02, 9, 9]), Some((0x0102, vec![9, 9])));
        assert_eq!(unwrap_rtx(&[0x01, 0x02]), None, "padding only");
    }

    #[test]
    fn retransmission_ssrcs_are_read_from_the_streams() {
        let d = serde_json::json!({ "video_ssrc": 264, "streams": [{ "ssrc": 264, "rtx_ssrc": 265 }] });
        assert!(rtx_pairs(&d).contains(&(265, 264)));
    }

    #[test]
    fn a_loss_is_asked_for_twice_then_left() {
        let mut rx = VideoReceiver::new();
        let t = Instant::now();
        rx.push(vp8(1, true, true, true), false, t);
        rx.push(vp8(3, true, true, false), false, t);
        assert_eq!(rx.due_for_asking(t), vec![2]);
        assert!(rx.due_for_asking(t).is_empty(), "not again at once");
        assert_eq!(rx.due_for_asking(t + NACK_AGAIN_AFTER), vec![2]);
        assert!(rx.due_for_asking(t + NACK_AGAIN_AFTER * 5).is_empty(), "twice is enough");
    }

    #[test]
    fn a_resend_that_arrives_mends_the_picture_without_a_keyframe() {
        let mut rx = VideoReceiver::new();
        let t = Instant::now();
        let first = rx.push(vp8(1, true, true, true), false, t);
        assert!(rx.admit(&first[0], true));
        assert!(rx.push(vp8(3, true, true, false), false, t).is_empty());
        let frames = rx.push(vp8(2, true, true, false), true, t);
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            assert!(!frame.after_gap);
            assert!(rx.admit(frame, false));
        }
        assert!(!rx.wants_keyframe(t));
    }

    #[test]
    fn an_unmended_loss_stops_the_picture_and_asks_for_a_keyframe() {
        let mut rx = VideoReceiver::new();
        let t = Instant::now();
        let first = rx.push(vp8(1, true, true, true), false, t);
        assert!(rx.admit(&first[0], true));
        rx.push(vp8(3, true, true, false), false, t);
        let frames = rx.push(vp8(4, true, true, false), false, t + Duration::from_millis(200));
        assert!(frames[0].after_gap);
        assert!(!rx.admit(&frames[0], false), "the frame after the loss is not decoded");
        assert!(!rx.admit(&frames[1], false), "nor anything until a keyframe");
        assert!(rx.wants_keyframe(t));
        assert!(!rx.wants_keyframe(t + Duration::from_millis(10)), "and not asked again at once");
        let key = rx.push(vp8(5, true, true, true), false, t + Duration::from_millis(300));
        assert!(rx.admit(&key[0], true));
        assert!(!rx.is_waiting_for_keyframe());
    }
}
