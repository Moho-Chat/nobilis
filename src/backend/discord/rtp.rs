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

/// Opus, as every Discord voice connection negotiates it.
pub const PAYLOAD_TYPE_OPUS: u8 = 120;

/// Opus runs at 48kHz and Discord sends it in 20ms frames: 960 samples per
/// channel, which is also how far the RTP timestamp moves per packet.
pub const OPUS_FRAME_SAMPLES: usize = 960;

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

/// An incoming packet's header, and where its payload begins.
///
/// The sending side writes a fixed twelve bytes, because everything it sends
/// is the simplest shape there is. Nothing incoming is promised to be: a
/// sender may carry contributing sources, and Discord's own client sends a
/// header extension on every packet. So the length has to be read out of the
/// header rather than assumed, and it matters twice over - it is where the
/// payload starts, and under the `_rtpsize` modes it is also exactly the span
/// the AEAD authenticates. Get it wrong and every packet fails to open, with
/// nothing to say why beyond "the packet did not open".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parsed {
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub marker: bool,
    /// Bytes before the payload: the fixed header, the contributing sources,
    /// and the extension if there is one.
    pub header_len: usize,
    /// The same without the extension - twelve bytes plus any contributing
    /// sources. Kept separately because where the extension falls relative to
    /// the encryption is a question the cipher has to answer, not this.
    pub fixed_len: usize,
}

/// Reads an RTP header, or refuses.
///
/// Refusing rather than guessing: a datagram that is not RTP at all arrives on
/// this socket routinely - the discovery answer, and whatever else the server
/// decides to send - and treating one of those as a packet would put noise
/// into the decoder.
pub fn parse_header(bytes: &[u8]) -> Option<Parsed> {
    if bytes.len() < 12 {
        return None;
    }
    // Version 2 in the top two bits. Anything else is not a packet this
    // understands, whatever else it may be.
    if bytes[0] >> 6 != 2 {
        return None;
    }
    let csrc_count = (bytes[0] & 0x0f) as usize;
    let extended = bytes[0] & 0x10 != 0;
    let fixed_len = 12 + csrc_count * 4;
    let mut header_len = fixed_len;
    if extended {
        // The extension is a four-byte header - two bytes of profile, two of
        // length - followed by that many 32-bit words.
        let words = u16::from_be_bytes([*bytes.get(header_len + 2)?, *bytes.get(header_len + 3)?]) as usize;
        header_len += 4 + words * 4;
    }
    if bytes.len() < header_len {
        return None;
    }
    Some(Parsed {
        payload_type: bytes[1] & 0x7f,
        sequence: u16::from_be_bytes([bytes[2], bytes[3]]),
        timestamp: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        ssrc: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        marker: bytes[1] & 0x80 != 0,
        header_len,
        fixed_len,
    })
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

/// How many packets a frame will take.
///
/// Answered without building them, because a sender has to reserve its
/// sequence numbers before it takes the first one - two frames handed in at
/// once would otherwise interleave, which the far end reads as loss and
/// answers by asking for a keyframe over and over.
pub fn packet_count_vp8(frame: &[u8]) -> usize {
    if frame.is_empty() {
        return 0;
    }
    frame.len().div_ceil(MAX_PAYLOAD - 1)
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
    fn a_header_written_here_reads_back_the_same() {
        let packet = Packet {
            payload_type: PAYLOAD_TYPE_VP8,
            sequence: 4_242,
            timestamp: 90_900,
            ssrc: 0xdead_beef,
            marker: true,
            payload: vec![1, 2, 3],
        };
        let bytes = header(&packet);
        let parsed = parse_header(&bytes).expect("our own header must parse");
        assert_eq!(parsed.payload_type, PAYLOAD_TYPE_VP8);
        assert_eq!(parsed.sequence, 4_242);
        assert_eq!(parsed.timestamp, 90_900);
        assert_eq!(parsed.ssrc, 0xdead_beef);
        assert!(parsed.marker);
        assert_eq!(parsed.header_len, 12);
    }

    #[test]
    fn the_marker_bit_is_not_read_as_part_of_the_payload_type() {
        let mut bytes = [0u8; 12];
        bytes[0] = 0x80;
        bytes[1] = 0x80 | PAYLOAD_TYPE_VP8;
        let parsed = parse_header(&bytes).unwrap();
        assert_eq!(parsed.payload_type, PAYLOAD_TYPE_VP8);
        assert!(parsed.marker);
    }

    #[test]
    fn contributing_sources_move_the_payload_along() {
        let mut bytes = vec![0u8; 12 + 8];
        bytes[0] = 0x82; // version 2, two CSRCs
        assert_eq!(parse_header(&bytes).unwrap().header_len, 20);
    }

    #[test]
    fn an_extension_is_counted_in_words_and_skipped_whole() {
        // Version 2, X set, no CSRCs, then a four-byte extension header
        // announcing three words of extension.
        let mut bytes = vec![0u8; 12 + 4 + 12];
        bytes[0] = 0x90;
        bytes[14] = 0x00;
        bytes[15] = 0x03;
        let parsed = parse_header(&bytes).expect("Discord sends an extension on every packet");
        assert_eq!(parsed.header_len, 28, "12 fixed + 4 extension header + 3 words");
    }

    #[test]
    fn something_that_is_not_a_packet_is_refused_rather_than_guessed_at() {
        assert!(parse_header(&[]).is_none());
        assert!(parse_header(&[0u8; 8]).is_none(), "too short to be a header");
        assert!(parse_header(&[0u8; 12]).is_none(), "version 0 is not RTP");
        // An extension claiming more words than the datagram holds.
        let mut bytes = vec![0u8; 16];
        bytes[0] = 0x90;
        bytes[15] = 0xff;
        assert!(parse_header(&bytes).is_none());
    }

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

    /// The count has to agree with what packetising actually produces, or a
    /// sender reserves the wrong range and every frame after the first is
    /// numbered over the top of the one before.
    #[test]
    fn the_count_agrees_with_the_packets() {
        for len in [1, 100, MAX_PAYLOAD - 2, MAX_PAYLOAD - 1, MAX_PAYLOAD, MAX_PAYLOAD * 3 + 7, 10_000] {
            let frame = vec![7u8; len];
            assert_eq!(
                packet_count_vp8(&frame),
                packetise_vp8(&frame, 1, 0, 0).len(),
                "disagreed for a {len}-byte frame"
            );
        }
        assert_eq!(packet_count_vp8(&[]), 0);
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
