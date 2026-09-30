//! Turning a stream of RTP packets back into frames.
//!
//! The mirror of `rtp::packetise_vp8`, and harder than it, because sending is
//! done to a network and receiving is done to whatever the network did on the
//! way. Packets arrive out of order, arrive twice, or do not arrive. A frame
//! is only a frame once every one of its packets is present, and a decoder
//! handed a frame with a hole in it does not report an error - it produces a
//! picture that is wrong and keeps producing wrong pictures until the next
//! keyframe, which is the failure mode this whole module exists to avoid.
//!
//! So: hold packets in sequence order, hand out only runs that are provably
//! whole, and drop what is too old to wait for any longer.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::rtp::Packet;

/// One reassembled frame, as a decoder wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// RTP's 90kHz clock, straight off the packets it came from.
    pub timestamp: u32,
    /// Whether a decoder could start here. A viewer arriving mid-stream has
    /// to throw everything away until one of these, and a decoder fed an
    /// inter frame first will either refuse it or produce garbage.
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Something before this frame was lost for good. A decoder handed an
    /// inter frame after a loss draws it on top of a picture it never had -
    /// the smearing that spreads across a moving camera until the next
    /// keyframe - so whoever decodes must stop at this and wait for one.
    pub after_gap: bool,
}

/// How long a complete frame waits behind a missing packet for it to be sent
/// again, before the missing packet is given up on.
///
/// A retransmission asked for straight away comes back in one round trip to
/// the relay - tens of milliseconds - and a frame at thirty a second is
/// thirty-three; this is a few frames' worth, which covers a resend without
/// making every loss a visible stall.
pub const RETRANSMIT_WAIT: Duration = Duration::from_millis(120);

/// How many packets to hold before giving up on a gap.
///
/// At 720p30 and 2.5Mbit a frame is a handful of packets, so this is some
/// tens of frames - long enough to outlast ordinary reordering and a
/// retransmit, short enough that a viewer is not watching a picture from two
/// seconds ago because one packet went missing.
pub const DEFAULT_WINDOW: usize = 512;

/// The reorder window for one sender.
///
/// One per SSRC. Two people streaming into the same connection would
/// otherwise share a sequence space they do not share in reality.
pub struct Reassembler {
    packets: BTreeMap<u64, Packet>,
    /// The highest extended sequence number seen, which is what later
    /// sequence numbers are placed relative to.
    highest: Option<u64>,
    /// Everything below this has already been emitted or given up on, and is
    /// dropped on arrival rather than reopening a frame that was completed
    /// without it.
    floor: u64,
    window: usize,
    /// Since when a complete frame has been held behind a hole, if one is.
    waiting_since: Option<Instant>,
    /// Whether any frame has been handed out. Until one has, nothing can have
    /// been lost - the stream simply had not begun - and a receiver is
    /// waiting for a keyframe anyway.
    emitted: bool,
}

impl Reassembler {
    pub fn new(window: usize) -> Self {
        Self { packets: BTreeMap::new(), highest: None, floor: 0, window: window.max(2), waiting_since: None, emitted: false }
    }

    /// Where a 16-bit sequence number sits on a line that does not wrap.
    ///
    /// RTP counts in 16 bits and a stream of any length goes round, so the
    /// numbers themselves cannot be compared or ordered. Each one is placed
    /// next to the highest seen so far by the shortest signed distance
    /// between them, which is right whenever the true gap is under half the
    /// space - 32767 packets, or minutes of video.
    fn extend(&self, sequence: u16) -> u64 {
        let Some(highest) = self.highest else { return sequence as u64 };
        let base = (highest & 0xffff) as u16;
        let delta = sequence.wrapping_sub(base) as i16;
        (highest as i64 + delta as i64).max(0) as u64
    }

    /// Takes one packet and returns whatever frames it completed.
    ///
    /// Usually none: a frame is finished by its last packet, so most packets
    /// complete nothing and the one carrying the marker completes everything
    /// at once. More than one comes back when a late packet fills the hole in
    /// an older frame and the frames after it were already whole.
    #[cfg(test)]
    pub fn push(&mut self, packet: Packet) -> Vec<Frame> {
        self.push_at(packet, Instant::now())
    }

    /// `push`, told the time - which is what decides whether a hole has been
    /// waited on long enough.
    pub fn push_at(&mut self, packet: Packet, now: Instant) -> Vec<Frame> {
        let extended = self.extend(packet.sequence);
        if extended < self.floor {
            return Vec::new();
        }
        self.highest = Some(self.highest.map_or(extended, |h| h.max(extended)));
        // Insert rather than overwrite: a duplicate is the same packet, and
        // replacing it would be harmless but pointless.
        self.packets.entry(extended).or_insert(packet);
        self.evict();
        self.drain(now)
    }

    /// Sequence numbers that are missing between what has been handed out and
    /// the newest packet - the ones worth asking the sender for again. At most
    /// `limit` of them, oldest first.
    pub fn missing(&self, limit: usize) -> Vec<u16> {
        let Some(highest) = self.highest else { return Vec::new() };
        if !self.emitted {
            return Vec::new();
        }
        (self.floor..highest)
            .filter(|sequence| !self.packets.contains_key(sequence))
            .take(limit)
            .map(|sequence| (sequence & 0xffff) as u16)
            .collect()
    }

    /// Forgets packets too old to be waiting for.
    ///
    /// A gap that is never going to be filled would otherwise hold every
    /// frame behind it forever - the picture stops, and nothing in the log
    /// says why. Giving up moves the floor past the gap, which lets the
    /// frames after it out; they are almost certainly inter frames that the
    /// decoder will refuse, and it will be shown the next keyframe instead.
    fn evict(&mut self) {
        while self.packets.len() > self.window {
            let Some((&oldest, _)) = self.packets.iter().next() else { break };
            self.packets.remove(&oldest);
            self.floor = oldest + 1;
        }
    }

    /// Every complete frame currently in the window, oldest first.
    fn drain(&mut self, now: Instant) -> Vec<Frame> {
        let mut out = Vec::new();
        loop {
            let Some(range) = self.first_complete() else { break };
            let (start, end) = range;
            // A hole before this frame: hold it a moment for the hole to be
            // filled by a resend, then give up on the hole and say so.
            let after_gap = self.emitted && start > self.floor;
            if after_gap {
                let since = *self.waiting_since.get_or_insert(now);
                if now.duration_since(since) < RETRANSMIT_WAIT {
                    break;
                }
            }
            self.waiting_since = None;
            let mut data = Vec::new();
            let mut keyframe = false;
            let mut timestamp = 0;
            for sequence in start..=end {
                let Some(packet) = self.packets.remove(&sequence) else { break };
                timestamp = packet.timestamp;
                let Some(offset) = vp8_payload_start(&packet.payload) else { continue };
                if sequence == start {
                    keyframe = is_keyframe(&packet.payload[offset..]);
                }
                data.extend_from_slice(&packet.payload[offset..]);
            }
            self.floor = end + 1;
            // Packets before a frame that has now been handed out cannot
            // belong to anything a decoder will still accept.
            self.packets.retain(|&sequence, _| sequence > end);
            if !data.is_empty() {
                self.emitted = true;
                out.push(Frame { timestamp, keyframe, data, after_gap });
            }
        }
        out
    }

    /// The first run of packets that is a whole frame: starts with `S` and a
    /// partition index of zero, ends with the marker bit, and is unbroken in
    /// between.
    fn first_complete(&self) -> Option<(u64, u64)> {
        let mut start: Option<u64> = None;
        let mut previous: Option<u64> = None;
        for (&sequence, packet) in &self.packets {
            // A hole: everything gathered so far belongs to a frame that
            // cannot be completed from here, so begin again on the far side.
            if previous.is_some_and(|p| sequence != p + 1) {
                start = None;
            }
            previous = Some(sequence);
            if starts_frame(&packet.payload) {
                start = Some(sequence);
            }
            let Some(first) = start else { continue };
            if packet.marker {
                return Some((first, sequence));
            }
        }
        None
    }
}

/// Where the VP8 bitstream begins, past the payload descriptor.
///
/// The descriptor is one byte here and can be up to six on the wire, because
/// every optional field is announced by a bit in the one before it. Discord's
/// own client sends the longer forms - picture ids and temporal layer
/// indices - so a receiver that assumed the short form would take the first
/// byte of a picture id for the first byte of a frame, on every packet, and
/// decode nothing.
///
/// Returns None for a payload too short to hold the descriptor it claims,
/// which is a corrupt packet rather than a small one.
pub fn vp8_payload_start(payload: &[u8]) -> Option<usize> {
    let first = *payload.first()?;
    let mut at = 1;
    if first & 0x80 != 0 {
        // X: the extension byte, which says which of the rest are present.
        let extension = *payload.get(at)?;
        at += 1;
        if extension & 0x80 != 0 {
            // I: a picture id, one byte or two. The long form announces
            // itself with the top bit of the first.
            let id = *payload.get(at)?;
            at += if id & 0x80 != 0 { 2 } else { 1 };
        }
        if extension & 0x40 != 0 {
            at += 1; // L: TL0PICIDX
        }
        if extension & 0x30 != 0 {
            at += 1; // T and K share one byte, present if either is set.
        }
    }
    (at <= payload.len()).then_some(at)
}

/// Whether this packet begins a frame.
///
/// `S` alone is not enough: it marks the start of a *partition*, and VP8
/// frames can be split into several. Only the one with partition index zero
/// starts the frame itself.
pub fn starts_frame(payload: &[u8]) -> bool {
    payload.first().is_some_and(|b| b & 0x10 != 0 && b & 0x07 == 0)
}

/// Whether a frame is a keyframe, from VP8's own header.
///
/// The first three bytes of a frame are uncompressed, and the lowest bit of
/// the first is the frame type - zero for a key frame, which is the opposite
/// way round from how it reads.
pub fn is_keyframe(frame: &[u8]) -> bool {
    frame.first().is_some_and(|b| b & 1 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u16, marker: bool, payload: Vec<u8>) -> Packet {
        Packet { payload_type: super::super::rtp::PAYLOAD_TYPE_VP8, sequence, timestamp: 900, ssrc: 1, marker, payload }
    }

    /// A descriptor with S set and partition zero, then a VP8 header whose
    /// frame type says keyframe, then some body.
    fn first_packet(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x10, 0x00, 0x00, 0x00];
        out.extend_from_slice(body);
        out
    }

    fn later_packet(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x00];
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn one_packet_is_one_frame() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        let frames = r.push(packet(10, true, first_packet(&[1, 2, 3])));
        assert_eq!(frames.len(), 1);
        assert!(frames[0].keyframe);
        assert_eq!(&frames[0].data[3..], &[1, 2, 3]);
    }

    #[test]
    fn a_frame_is_only_whole_once_every_packet_is_there() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        assert!(r.push(packet(10, false, first_packet(&[1]))).is_empty());
        assert!(r.push(packet(11, false, later_packet(&[2]))).is_empty());
        let frames = r.push(packet(12, true, later_packet(&[3])));
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0].data[3..], &[1, 2, 3]);
    }

    #[test]
    fn packets_that_arrive_backwards_still_make_a_frame() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        assert!(r.push(packet(12, true, later_packet(&[3]))).is_empty());
        assert!(r.push(packet(11, false, later_packet(&[2]))).is_empty());
        let frames = r.push(packet(10, false, first_packet(&[1])));
        assert_eq!(frames.len(), 1, "the frame completes when its first packet finally lands");
        assert_eq!(&frames[0].data[3..], &[1, 2, 3]);
    }

    #[test]
    fn a_duplicate_changes_nothing() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        r.push(packet(10, false, first_packet(&[1])));
        r.push(packet(10, false, first_packet(&[1])));
        let frames = r.push(packet(11, true, later_packet(&[2])));
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0].data[3..], &[1, 2]);
    }

    #[test]
    fn a_frame_with_a_hole_in_it_is_never_handed_out() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        r.push(packet(10, false, first_packet(&[1])));
        // 11 never arrives.
        let frames = r.push(packet(12, true, later_packet(&[3])));
        assert!(frames.is_empty(), "a decoder must not be given a frame missing its middle");
    }

    #[test]
    fn a_frame_behind_a_hole_waits_for_the_resend_and_takes_it() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        let t = Instant::now();
        r.push_at(packet(10, true, first_packet(&[1])), t);
        // 11 is lost; 12 is a whole frame, held rather than handed out.
        assert!(r.push_at(packet(12, true, first_packet(&[3])), t).is_empty());
        assert_eq!(r.missing(8), vec![11]);
        // The resend arrives in time: both come out, in order, whole.
        let frames = r.push_at(packet(11, true, first_packet(&[2])), t + Duration::from_millis(40));
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| !f.after_gap));
    }

    #[test]
    fn a_hole_that_is_not_filled_in_time_is_given_up_and_marked() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        let t = Instant::now();
        r.push_at(packet(10, true, first_packet(&[1])), t);
        assert!(r.push_at(packet(12, true, first_packet(&[3])), t).is_empty());
        let frames = r.push_at(packet(13, true, first_packet(&[4])), t + RETRANSMIT_WAIT);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].after_gap, "the first frame past the hole says so");
        assert!(!frames[1].after_gap);
        assert!(r.missing(8).is_empty(), "nothing is still asked for once given up");
    }

    #[test]
    fn the_frame_after_a_lost_one_still_arrives() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        let t = Instant::now();
        r.push_at(packet(9, true, first_packet(&[0])), t);
        r.push_at(packet(10, false, first_packet(&[1])), t);
        r.push_at(packet(12, true, later_packet(&[3])), t);
        assert!(r.push_at(packet(13, true, first_packet(&[9])), t).is_empty(), "held for a resend first");
        let frames = r.push_at(packet(14, true, first_packet(&[10])), t + RETRANSMIT_WAIT);
        assert_eq!(frames.len(), 2, "losing one frame must not stop the stream");
        assert_eq!(&frames[0].data[3..], &[9]);
        assert!(frames[0].after_gap, "and it says a frame was lost before it");
        assert!(!frames[1].after_gap);
    }

    #[test]
    fn sequence_numbers_wrap_without_losing_the_picture() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        assert!(r.push(packet(65_534, false, first_packet(&[1]))).is_empty());
        assert!(r.push(packet(65_535, false, later_packet(&[2]))).is_empty());
        let frames = r.push(packet(0, true, later_packet(&[3])));
        assert_eq!(frames.len(), 1, "65535 is followed by 0, not by a gap of 65535");
        assert_eq!(&frames[0].data[3..], &[1, 2, 3]);
    }

    #[test]
    fn a_gap_that_is_never_filled_does_not_stop_everything_behind_it() {
        let mut r = Reassembler::new(4);
        r.push(packet(10, false, first_packet(&[1])));
        let mut seen = 0;
        // Far more than the window, so the hole at 11 ages out.
        for sequence in 12..40 {
            seen += r.push(packet(sequence, true, first_packet(&[sequence as u8]))).len();
        }
        assert!(seen > 0, "the window has to give up on a hole or the picture stops forever");
    }

    #[test]
    fn an_inter_frame_is_not_mistaken_for_a_keyframe() {
        let mut r = Reassembler::new(DEFAULT_WINDOW);
        let mut payload = vec![0x10, 0x01, 0x00, 0x00];
        payload.extend_from_slice(&[7]);
        let frames = r.push(packet(1, true, payload));
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].keyframe, "the low bit of VP8's first byte set means an inter frame");
    }

    #[test]
    fn the_long_payload_descriptor_is_skipped_whole() {
        // X set, with a two-byte picture id, TL0PICIDX and a TID byte: the
        // shape Discord's own client sends.
        let payload = vec![0x90, 0xf0, 0x80, 0x0a, 0x1f, 0x40, 0xde, 0xad];
        assert_eq!(vp8_payload_start(&payload), Some(6), "1 + X + 2 id + L + T");
        assert_eq!(&payload[6..], &[0xde, 0xad]);
    }

    #[test]
    fn the_short_descriptor_is_one_byte() {
        assert_eq!(vp8_payload_start(&[0x10, 0x01]), Some(1));
    }

    #[test]
    fn a_descriptor_that_runs_off_the_end_is_refused() {
        assert_eq!(vp8_payload_start(&[0x80]), None, "X set with nothing after it");
        assert_eq!(vp8_payload_start(&[]), None);
    }

    /// The one test that proves the two halves of this stack agree.
    ///
    /// Everything else here checks the receiver against a shape written by
    /// hand, which only proves it matches what the test author believed. This
    /// sends a frame through the real sender and the real header writer and
    /// requires the exact bytes back - so if either side's idea of a
    /// descriptor, a marker or a sequence number drifts, this is what says so.
    #[test]
    fn a_frame_sent_by_the_sender_comes_back_out_of_the_receiver() {
        use super::super::rtp;
        // Long enough to need several packets, and starting with a byte whose
        // low bit is clear so it reads as a keyframe.
        let mut frame = vec![0x00, 0x11, 0x22];
        frame.extend((0..rtp::MAX_PAYLOAD * 2 + 7).map(|i| (i % 251) as u8));

        let packets = rtp::packetise_vp8(&frame, 77, 1_000, 90_000);
        assert!(packets.len() > 2, "the point of this test is a frame that had to be split");

        let mut r = Reassembler::new(DEFAULT_WINDOW);
        let mut out = Vec::new();
        for packet in packets {
            // Through the wire format and back, so the header writer and the
            // header parser are both in the loop rather than bypassed.
            let bytes = rtp::header(&packet);
            let parsed = rtp::parse_header(&bytes).expect("the sender's own header");
            out.extend(r.push(Packet {
                payload_type: parsed.payload_type,
                sequence: parsed.sequence,
                timestamp: parsed.timestamp,
                ssrc: parsed.ssrc,
                marker: parsed.marker,
                payload: packet.payload,
            }));
        }
        assert_eq!(out.len(), 1, "one frame in, one frame out");
        assert_eq!(out[0].data, frame, "the bytes have to survive the round trip exactly");
        assert_eq!(out[0].timestamp, 90_000);
        assert!(out[0].keyframe);
    }

    #[test]
    fn only_partition_zero_starts_a_frame() {
        assert!(starts_frame(&[0x10]));
        assert!(!starts_frame(&[0x11]), "S set on partition 1 is a partition, not a frame");
        assert!(!starts_frame(&[0x00]));
    }
}
