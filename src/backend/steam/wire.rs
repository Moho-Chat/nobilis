//! How a packet is laid out on Steam's WebSocket, and nothing about what is in
//! one.
//!
//! One WebSocket binary message is one packet:
//!
//! ```text
//! u32 LE   message type, with the top bit set when the body is protobuf
//! u32 LE   length of the header
//! header   a `Header` protobuf (jobs, the account, the session, a result)
//! body     the message's own protobuf
//! ```
//!
//! A handful of older messages use a fixed binary header instead of a
//! protobuf one; they are read so that one arriving does not end the
//! connection, but nothing here sends one.
//!
//! One kind of packet holds others: a `Multi`, whose body is a run of
//! `u32 LE length, packet`, gzip-compressed when it says how long it is once
//! unzipped. A server batches what it has to say after a sign-in this way, so
//! the whole friends list and every name in it commonly arrive inside one.

use super::proto::{Header, Multi};
use anyhow::{anyhow, bail, Context, Result};
use prost::Message;
use std::io::Read;

/// Set on the message type of a packet whose body (and header) is protobuf.
pub const PROTO_MASK: u32 = 0x8000_0000;

/// No job: what a packet that neither starts nor answers a request carries.
pub const NO_JOB: u64 = u64::MAX;

/// The message types this client sends or looks for. Numbers from the
/// official client's own table (`enums_clientserver.proto`).
pub mod emsg {
    pub const MULTI: u32 = 1;
    #[cfg(test)]
    pub const SERVICE_METHOD_RESPONSE: u32 = 147;
    pub const SERVICE_METHOD_CALL_FROM_CLIENT: u32 = 151;
    pub const SERVICE_METHOD_SEND_TO_CLIENT: u32 = 152;
    pub const CLIENT_HEART_BEAT: u32 = 703;
    pub const CLIENT_LOG_OFF: u32 = 706;
    pub const CLIENT_CHANGE_STATUS: u32 = 716;
    pub const CLIENT_LOG_ON_RESPONSE: u32 = 751;
    pub const CLIENT_LOGGED_OFF: u32 = 757;
    pub const CLIENT_PERSONA_STATE: u32 = 766;
    pub const CLIENT_FRIENDS_LIST: u32 = 767;
    pub const CLIENT_ACCOUNT_INFO: u32 = 768;
    pub const CLIENT_REQUEST_FRIEND_DATA: u32 = 815;
    pub const CLIENT_LOGON: u32 = 5514;
    pub const SERVICE_METHOD_CALL_FROM_CLIENT_NON_AUTHED: u32 = 9804;
    pub const CLIENT_HELLO: u32 = 9805;
}

/// The most a compressed batch may grow to. A real one is a few hundred
/// kilobytes; this is a stop against a body that claims to be small and is not.
const MAX_UNZIPPED: usize = 64 * 1024 * 1024;

/// One packet, with its header read and its body left as bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    /// The message type, without the protobuf bit.
    pub emsg: u32,
    /// Whether the body is protobuf; false for the old fixed-header packets.
    pub protobuf: bool,
    pub header: Header,
    pub body: Vec<u8>,
}

impl Packet {
    pub fn new(emsg: u32, header: Header, body: Vec<u8>) -> Self {
        Self { emsg, protobuf: true, header, body }
    }

    /// The request this answers, or `NO_JOB`.
    pub fn job_target(&self) -> u64 {
        self.header.jobid_target.unwrap_or(NO_JOB)
    }

    /// How it went. Absent reads as the schema's default, a failure: success
    /// is always written down.
    pub fn eresult(&self) -> i32 {
        self.header.eresult.unwrap_or(super::eresult::FAIL)
    }

    /// The bytes to put on the wire.
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(self.protobuf, "only protobuf packets are written");
        let header = self.header.encode_to_vec();
        let mut out = Vec::with_capacity(8 + header.len() + self.body.len());
        out.extend_from_slice(&(self.emsg | PROTO_MASK).to_le_bytes());
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.body);
        out
    }

    /// Reads one packet from a WebSocket message.
    pub fn decode(bytes: &[u8]) -> Result<Packet> {
        let kind = u32_at(bytes, 0).context("a packet shorter than its message type")?;
        let protobuf = kind & PROTO_MASK != 0;
        let emsg = kind & !PROTO_MASK;
        if protobuf {
            let header_len = u32_at(bytes, 4).context("a packet with no header length")? as usize;
            let end = 8usize.checked_add(header_len).filter(|end| *end <= bytes.len()).ok_or_else(|| anyhow!("a header longer than its packet"))?;
            let header = Header::decode(&bytes[8..end]).context("reading a packet header")?;
            return Ok(Packet { emsg, protobuf, header, body: bytes[end..].to_vec() });
        }
        // The old layout: a one-byte header size, a two-byte version, the two
        // job ids, a canary, the account and the session - 32 bytes in all
        // after the type. It is read for the jobs and the session, because
        // that is what the rest of this needs from it.
        const LEGACY: usize = 4 + 32;
        if bytes.len() < LEGACY {
            bail!("an old-style packet of {} bytes", bytes.len());
        }
        let target = u64_at(bytes, 7).context("a legacy job id")?;
        let source = u64_at(bytes, 15).context("a legacy job id")?;
        let steamid = u64_at(bytes, 24).context("a legacy steam id")?;
        let session = i32::from_le_bytes(bytes[32..36].try_into().expect("four bytes"));
        let header = Header {
            steamid: Some(steamid),
            client_sessionid: Some(session),
            jobid_source: Some(source),
            jobid_target: Some(target),
            ..Header::default()
        };
        Ok(Packet { emsg, protobuf, header, body: bytes[LEGACY..].to_vec() })
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// The packets inside a `Multi`.
///
/// Nested batches are unpacked too, in order, so a caller sees a flat run.
pub fn unpack_multi(body: &[u8]) -> Result<Vec<Packet>> {
    let multi = Multi::decode(body).context("reading a batch of packets")?;
    let inner = multi.message_body.unwrap_or_default();
    let bytes = match multi.size_unzipped.filter(|n| *n > 0) {
        None => inner,
        Some(expected) => {
            let expected = expected as usize;
            if expected > MAX_UNZIPPED {
                bail!("a batch that claims to unzip to {expected} bytes");
            }
            let mut out = Vec::with_capacity(expected);
            // Reading one byte past the claim is how an overlong stream is
            // told from an exact one without trusting the claim.
            flate2::read::GzDecoder::new(inner.as_slice())
                .take(expected as u64 + 1)
                .read_to_end(&mut out)
                .context("unzipping a batch of packets")?;
            if out.len() != expected {
                bail!("a batch that said {expected} bytes and unzipped to {}", out.len());
            }
            out
        }
    };

    let mut packets = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let len = u32_at(&bytes, at).context("a batch cut off in a length")? as usize;
        at += 4;
        let end = at.checked_add(len).filter(|end| *end <= bytes.len()).ok_or_else(|| anyhow!("a packet longer than its batch"))?;
        let packet = Packet::decode(&bytes[at..end])?;
        if packet.emsg == emsg::MULTI {
            packets.extend(unpack_multi(&packet.body)?);
        } else {
            packets.push(packet);
        }
        at = end;
    }
    Ok(packets)
}

/// Packs packets into a gzip-compressed `Multi` body, as a server would. Only
/// the test server and the tests have a use for it.
#[cfg(test)]
pub fn pack_multi(packets: &[Packet], compress: bool) -> Vec<u8> {
    use std::io::Write;
    let mut run = Vec::new();
    for p in packets {
        let encoded = p.encode();
        run.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        run.extend_from_slice(&encoded);
    }
    let multi = if compress {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&run).unwrap();
        Multi { size_unzipped: Some(run.len() as u32), message_body: Some(gz.finish().unwrap()) }
    } else {
        Multi { size_unzipped: None, message_body: Some(run) }
    };
    multi.encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(emsg: u32, body: &[u8]) -> Packet {
        Packet::new(
            emsg,
            Header { steamid: Some(76561197960287930), client_sessionid: Some(7), jobid_source: Some(42), target_job_name: Some("A.B#1".into()), ..Header::default() },
            body.to_vec(),
        )
    }

    #[test]
    fn a_packet_is_its_type_a_header_length_a_header_and_a_body() {
        let p = packet(emsg::CLIENT_HEART_BEAT, &[0x08, 0x01]);
        let bytes = p.encode();
        // The protobuf bit rides on the type.
        assert_eq!(&bytes[0..4], &(703u32 | PROTO_MASK).to_le_bytes());
        let header_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 8 + header_len + 2);
        assert_eq!(&bytes[bytes.len() - 2..], &[0x08, 0x01]);
        assert_eq!(Packet::decode(&bytes).unwrap(), p);
    }

    #[test]
    fn an_answer_names_the_request_it_answers() {
        let mut p = packet(emsg::SERVICE_METHOD_RESPONSE, b"");
        assert_eq!(p.job_target(), NO_JOB);
        p.header.jobid_target = Some(9);
        assert_eq!(Packet::decode(&p.encode()).unwrap().job_target(), 9);
    }

    #[test]
    fn a_result_nobody_wrote_is_a_failure_and_a_written_one_is_believed() {
        let mut p = packet(emsg::SERVICE_METHOD_RESPONSE, b"");
        assert_eq!(p.eresult(), 2);
        p.header.eresult = Some(1);
        assert_eq!(Packet::decode(&p.encode()).unwrap().eresult(), 1);
    }

    #[test]
    fn packets_that_are_cut_short_are_refused_rather_than_guessed_at() {
        assert!(Packet::decode(&[]).is_err());
        assert!(Packet::decode(&[1, 2, 3]).is_err());
        // A protobuf packet whose header claims more than there is.
        let mut bytes = (703u32 | PROTO_MASK).to_le_bytes().to_vec();
        bytes.extend_from_slice(&1000u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 10]);
        assert!(Packet::decode(&bytes).is_err());
        // One that claims the largest header there is must not overflow.
        let mut bytes = (703u32 | PROTO_MASK).to_le_bytes().to_vec();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Packet::decode(&bytes).is_err());
        // And a header that is not a header.
        let mut bytes = (703u32 | PROTO_MASK).to_le_bytes().to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xff, 0xff]);
        assert!(Packet::decode(&bytes).is_err());
    }

    #[test]
    fn an_old_style_packet_is_read_for_its_jobs_and_session() {
        let mut bytes = 5500u32.to_le_bytes().to_vec(); // no protobuf bit
        bytes.push(36); // header size
        bytes.extend_from_slice(&2u16.to_le_bytes()); // version
        bytes.extend_from_slice(&11u64.to_le_bytes()); // target job
        bytes.extend_from_slice(&22u64.to_le_bytes()); // source job
        bytes.push(239); // canary
        bytes.extend_from_slice(&76561197960287930u64.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        bytes.extend_from_slice(b"body");
        let p = Packet::decode(&bytes).unwrap();
        assert!(!p.protobuf);
        assert_eq!(p.emsg, 5500);
        assert_eq!(p.job_target(), 11);
        assert_eq!(p.header.jobid_source, Some(22));
        assert_eq!(p.header.steamid, Some(76561197960287930));
        assert_eq!(p.header.client_sessionid, Some(5));
        assert_eq!(p.body, b"body");
        assert!(Packet::decode(&bytes[..20]).is_err());
    }

    #[test]
    fn a_batch_unpacks_in_order_compressed_or_not() {
        let inside = [packet(emsg::CLIENT_FRIENDS_LIST, b"one"), packet(emsg::CLIENT_PERSONA_STATE, b"two"), packet(emsg::CLIENT_ACCOUNT_INFO, b"three")];
        for compress in [true, false] {
            let got = unpack_multi(&pack_multi(&inside, compress)).unwrap();
            assert_eq!(got, inside, "compressed: {compress}");
        }
        // Nothing in it is nothing out. (A zero length means "not compressed",
        // as it does to every other client, so an empty batch is sent plain.)
        assert!(unpack_multi(&pack_multi(&[], false)).unwrap().is_empty());
    }

    #[test]
    fn a_batch_inside_a_batch_comes_out_flat() {
        let inner = packet(emsg::CLIENT_PERSONA_STATE, b"deep");
        let wrapper = packet(emsg::MULTI, &pack_multi(std::slice::from_ref(&inner), true));
        let got = unpack_multi(&pack_multi(&[packet(emsg::CLIENT_FRIENDS_LIST, b"first"), wrapper], false)).unwrap();
        assert_eq!(got.iter().map(|p| p.body.clone()).collect::<Vec<_>>(), vec![b"first".to_vec(), b"deep".to_vec()]);
    }

    #[test]
    fn a_batch_that_lies_about_itself_is_refused() {
        let good = pack_multi(&[packet(emsg::CLIENT_FRIENDS_LIST, b"x")], true);
        // Says it unzips to a different length than it does.
        let mut multi = Multi::decode(good.as_slice()).unwrap();
        multi.size_unzipped = Some(multi.size_unzipped.unwrap() + 5);
        assert!(unpack_multi(&multi.encode_to_vec()).is_err());
        multi.size_unzipped = Some(3);
        assert!(unpack_multi(&multi.encode_to_vec()).is_err(), "unzips to more than it said");
        // Says it is huge.
        multi.size_unzipped = Some(u32::MAX);
        assert!(unpack_multi(&multi.encode_to_vec()).is_err());
        // Is not gzip at all.
        let junk = Multi { size_unzipped: Some(10), message_body: Some(vec![1, 2, 3, 4]) };
        assert!(unpack_multi(&junk.encode_to_vec()).is_err());
        // Packs a packet that runs past the end.
        let mut run = 100u32.to_le_bytes().to_vec();
        run.extend_from_slice(&[1, 2, 3]);
        let short = Multi { size_unzipped: None, message_body: Some(run) };
        assert!(unpack_multi(&short.encode_to_vec()).is_err());
        // And a length cut off in the middle.
        let cut = Multi { size_unzipped: None, message_body: Some(vec![1, 2]) };
        assert!(unpack_multi(&cut.encode_to_vec()).is_err());
    }
}
