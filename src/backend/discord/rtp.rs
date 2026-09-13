//! Video, as Discord's voice connection carries it.
//!
//! A Go Live stream is a second voice connection of its own - its own
//! endpoint, its own token, its own UDP session and its own SSRCs - so none of
//! this touches the channel connection songbird holds. Songbird has no video
//! path at all (its `Driver` exposes playback and nothing that would let a
//! caller put a packet on the wire), which is why this exists.
//!
//! Two jobs, and they are separable on purpose: turning one encoded frame into
//! RTP packets, and turning one RTP packet into bytes. Both are arithmetic
//! over byte layouts, which is the part of a media stack that is wrong
//! silently - a packet with the wrong marker bit or a fragment header off by
//! one nibble produces a picture that never appears and no error anywhere.

/// The largest payload to put in one packet.
///
/// Discord's own client sends about this. It is the usual 1500-byte path MTU
/// less the IP and UDP headers, the RTP header, and the room the AEAD takes -
/// a 16-byte tag and a 4-byte nonce suffix. Erring small costs a few more
/// packets; erring large costs every packet that crosses a smaller link.
pub const MAX_PAYLOAD: usize = 1100;

/// The RTP payload type Discord uses for VP8.
///
/// VP8 rather than H.264 because Chromium's encoder always has it in
/// software, on every machine this runs on - the H.264 one depends on the
/// platform, and a screen share that works on one desktop and not the next is
/// worse than one that is a little larger on the wire.
pub const PAYLOAD_TYPE_VP8: u8 = 103;

/// One RTP packet, before encryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// Set on the last packet of a frame, which is how the far end knows the
    /// frame is whole and can be handed to a decoder.
    pub marker: bool,
    pub payload: Vec<u8>,
}

/// The twelve-byte RTP header, as Discord expects it.
///
/// Version 2, no padding, no extension, no CSRCs - which is what every packet
/// out of this stack is, so the first two bytes are constant apart from the
/// marker bit.
pub fn header(packet: &Packet) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0] = 0x80;
    out[1] = packet.payload_type | if packet.marker { 0x80 } else { 0 };
    out[2..4].copy_from_slice(&packet.sequence.to_be_bytes());
    out[4..8].copy_from_slice(&packet.timestamp.to_be_bytes());
    out[8..12].copy_from_slice(&packet.ssrc.to_be_bytes());
    out
}

/// The 90kHz clock RTP timestamps video on.
///
/// Not the frame number and not milliseconds: a decoder times playback from
/// this, and a stream timestamped in frames plays at whatever rate the
/// receiver guesses.
pub const VIDEO_CLOCK_HZ: u64 = 90_000;

/// A frame's presentation time, in RTP's clock.
///
/// WebCodecs hands out microseconds. Wrapping is not an error - the field is
/// 32 bits and a long stream is expected to go round - so it is done
/// deliberately rather than left to overflow in a debug build.
pub fn timestamp_from_micros(micros: i64) -> u32 {
    ((micros.max(0) as u128 * VIDEO_CLOCK_HZ as u128) / 1_000_000) as u32
}

/// Splits one encoded VP8 frame across as many packets as it needs.
///
/// The VP8 payload descriptor is one byte here, which is the smallest form the
/// RFC allows: no picture id, no temporal layers, no keyidx. Discord's own
/// client sends more of it; a receiver is required to accept this form, and
/// the fields left out are for features this does not use.
///
/// `S` is set on the first packet of a frame and the marker bit on the last,
/// which between them are how a receiver reassembles without knowing the
/// length in advance. A single-packet frame carries both.
pub fn packetise_vp8(frame: &[u8], ssrc: u32, first_sequence: u16, timestamp: u32) -> Vec<Packet> {
    // An empty frame is not a frame. Sending one would burn a sequence number
    // and give the far end a descriptor with nothing behind it.
    if frame.is_empty() {
        return Vec::new();
    }
    // One byte of every packet is the descriptor, so that much less is
    // available for the frame itself.
    let per_packet = MAX_PAYLOAD - 1;
    let mut packets = Vec::new();
    let mut sequence = first_sequence;
    for (i, chunk) in frame.chunks(per_packet).enumerate() {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        // X=0 R=0 N=0 S=<first> R=0 PID=0. The S bit is bit 4.
        payload.push(if i == 0 { 0x10 } else { 0x00 });
        payload.extend_from_slice(chunk);
        packets.push(Packet {
            payload_type: PAYLOAD_TYPE_VP8,
            sequence,
            timestamp,
            ssrc,
            marker: false,
            payload,
        });
        sequence = sequence.wrapping_add(1);
    }
    // Every packet of a frame carries the same timestamp, and the last one
    // says so - that is the only thing telling the far end the frame is whole.
    if let Some(last) = packets.last_mut() {
        last.marker = true;
    }
    packets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_says_version_two_and_carries_the_marker() {
        let p = Packet {
            payload_type: PAYLOAD_TYPE_VP8,
            sequence: 0x1234,
            timestamp: 0xDEADBEEF,
            ssrc: 0x01020304,
            marker: true,
            payload: vec![],
        };
        let h = header(&p);
        assert_eq!(h[0], 0x80, "version 2, no padding, no extension, no CSRCs");
        assert_eq!(h[1], 0x80 | 103, "marker bit above the payload type");
        assert_eq!(&h[2..4], &[0x12, 0x34]);
        assert_eq!(&h[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&h[8..12], &[0x01, 0x02, 0x03, 0x04]);

        // And without it, which is every packet of a frame but the last.
        let h = header(&Packet { marker: false, ..p });
        assert_eq!(h[1], 103);
    }

    /// A frame small enough for one packet still has to say it both begins
    /// and ends there, or a receiver waits for a continuation that never
    /// comes.
    #[test]
    fn a_small_frame_is_one_packet_that_starts_and_finishes_it() {
        let packets = packetise_vp8(&[1, 2, 3, 4], 7, 100, 900);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].payload[0] & 0x10, 0x10, "S bit set on the first packet");
        assert!(packets[0].marker, "and the marker on the last");
        assert_eq!(&packets[0].payload[1..], &[1, 2, 3, 4]);
        assert_eq!(packets[0].sequence, 100);
        assert_eq!(packets[0].timestamp, 900);
        assert_eq!(packets[0].ssrc, 7);
    }

    /// The case the whole module exists for: a keyframe is several kilobytes
    /// and the path MTU is not.
    #[test]
    fn a_large_frame_is_split_and_reassembles_to_itself() {
        let frame: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let packets = packetise_vp8(&frame, 9, 65_534, 4_000);
        assert!(packets.len() > 1, "a 10 KB frame does not fit in one packet");

        // Only the first says it starts a frame; only the last says it ends
        // one. Getting either wrong is a picture that never appears.
        assert_eq!(packets[0].payload[0] & 0x10, 0x10);
        for p in &packets[1..] {
            assert_eq!(p.payload[0] & 0x10, 0, "only the first packet starts the frame");
        }
        assert!(packets.last().unwrap().marker);
        for p in &packets[..packets.len() - 1] {
            assert!(!p.marker, "only the last packet ends the frame");
        }

        // Every packet fits, descriptor included.
        for p in &packets {
            assert!(p.payload.len() <= MAX_PAYLOAD, "{} is over the MTU", p.payload.len());
        }

        // One timestamp for the whole frame, and sequence numbers that go
        // round rather than panicking at the top.
        assert!(packets.iter().all(|p| p.timestamp == 4_000));
        assert_eq!(packets[0].sequence, 65_534);
        assert_eq!(packets[1].sequence, 65_535);
        assert_eq!(packets[2].sequence, 0, "the sequence wraps rather than overflowing");

        // And the frame comes back out of them unchanged, which is the whole
        // claim.
        let rebuilt: Vec<u8> = packets.iter().flat_map(|p| p.payload[1..].iter().copied()).collect();
        assert_eq!(rebuilt, frame);
    }

    /// A frame exactly the size of one packet's room must not produce an
    /// empty second packet - the off-by-one that `chunks` gets right and
    /// hand-written loops usually do not.
    #[test]
    fn a_frame_that_exactly_fills_a_packet_makes_only_one() {
        let frame = vec![0u8; MAX_PAYLOAD - 1];
        let packets = packetise_vp8(&frame, 1, 0, 0);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].payload.len(), MAX_PAYLOAD);

        let one_more = vec![0u8; MAX_PAYLOAD];
        assert_eq!(packetise_vp8(&one_more, 1, 0, 0).len(), 2);
    }

    #[test]
    fn nothing_is_sent_for_nothing() {
        assert!(packetise_vp8(&[], 1, 0, 0).is_empty());
    }

    /// A decoder times playback from the RTP clock, so the conversion from
    /// what WebCodecs hands out has to be right.
    #[test]
    fn microseconds_become_the_ninety_kilohertz_clock() {
        assert_eq!(timestamp_from_micros(0), 0);
        // One second.
        assert_eq!(timestamp_from_micros(1_000_000), 90_000);
        // One frame at 30fps.
        assert_eq!(timestamp_from_micros(33_333), 2_999);
        // A long stream goes round rather than overflowing. 2^32 ticks is
        // about thirteen hours, which is a stream somebody really does leave
        // running - and the tick either side of the top is where a cast that
        // truncated instead of wrapping would panic in a debug build.
        assert_eq!(timestamp_from_micros(47_721_858_844), u32::MAX);
        assert_eq!(timestamp_from_micros(47_721_858_845), 0);
        assert_eq!(timestamp_from_micros(47_721_858_856), 1);
        // Nothing sensible to do with a negative timestamp but start at zero.
        assert_eq!(timestamp_from_micros(-5), 0);
    }
}
