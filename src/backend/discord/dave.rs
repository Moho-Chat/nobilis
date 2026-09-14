//! DAVE, the end-to-end encryption a Discord voice connection now has to
//! speak.
//!
//! Discord refuses a voice connection that cannot do this with close code
//! 4017, "E2EE protocol required" - and it means it: a connection declaring
//! `max_dave_protocol_version: 0` is refused just as firmly as one that says
//! nothing at all. That is why the channel's audio works, since songbird
//! negotiates DAVE, while the hand-rolled stream connection beside it did
//! not.
//!
//! The cryptography is `davey`'s, which is the same implementation songbird
//! uses. What is here is the part that is not cryptography: the opcodes, what
//! to do with each, and where the group state lives so that both the
//! websocket loop and the thing sending video can reach it.
//!
//! # How a session comes to exist
//!
//! 1. The identify declares a version. The session description answers with
//!    the version actually in force - zero means the server changed its mind
//!    and there is nothing to do.
//! 2. A key package goes up (binary, op 26). That is this device asking to
//!    join the group.
//! 3. The server sends the external sender (op 25), then proposals (op 27),
//!    to which the answer is a commit and a welcome (op 28).
//! 4. A commit (op 29) or a welcome (op 30) arrives, each naming a
//!    transition; acknowledging it (op 23) is what makes the new epoch real.
//! 5. Once the group is ready, every media frame is encrypted with it before
//!    it is sealed for the wire.
//!
//! Epochs change as people come and go, and during a transition the server
//! asks for passthrough (op 21) so media keeps flowing while the group is
//! rebuilt. Nothing here initiates any of it: DAVE is server-driven, and a
//! client's whole job is to answer correctly and promptly.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::num::NonZeroU16;

/// The opcodes that arrive as binary frames rather than JSON.
pub const OP_EXTERNAL_SENDER: u8 = 25;
pub const OP_KEY_PACKAGE: u8 = 26;
pub const OP_PROPOSALS: u8 = 27;
pub const OP_COMMIT_WELCOME: u8 = 28;
pub const OP_ANNOUNCE_COMMIT_TRANSITION: u8 = 29;
pub const OP_WELCOME: u8 = 30;

/// And the ones that are ordinary JSON.
pub const OP_PREPARE_TRANSITION: u64 = 21;
pub const OP_EXECUTE_TRANSITION: u64 = 22;
pub const OP_TRANSITION_READY: u64 = 23;
pub const OP_PREPARE_EPOCH: u64 = 24;
pub const OP_INVALID_COMMIT_WELCOME: u64 = 31;

/// One binary frame from the server, unwrapped.
///
/// Under voice gateway v8 a server-to-client binary frame is
/// `[sequence: u16][opcode: u8][payload]`; the sequence exists so a resumed
/// connection can say how far it got, and is not otherwise this module's
/// business. Client-to-server frames carry no sequence at all.
///
/// Worth being exact about: songbird speaks v4, where there is no sequence
/// and the opcode is the first byte. Reading a v8 frame that way takes the
/// top half of the sequence number for an opcode and the rest of the frame is
/// nonsense - which would look exactly like Discord sending something
/// unrecognised.
pub struct BinaryFrame<'a> {
    pub opcode: u8,
    pub payload: &'a [u8],
}

pub fn read_binary(data: &[u8]) -> Option<BinaryFrame<'_>> {
    if data.len() < 3 {
        return None;
    }
    Some(BinaryFrame { opcode: data[2], payload: &data[3..] })
}

/// A binary frame to send. No sequence: that direction does not carry one.
pub fn write_binary(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(opcode);
    out.extend_from_slice(payload);
    out
}

/// What a client has to send back after taking in a frame.
///
/// Returned rather than sent from here so that this module holds no socket:
/// the group state is shared between the websocket loop and the sender, and
/// anything that could try to write from two places would need a lock around
/// the socket as well as around the session.
pub enum Reply {
    Nothing,
    Binary(u8, Vec<u8>),
    Json(Value),
}

/// The group this connection belongs to, and the version in force.
pub struct Dave {
    session: davey::DaveSession,
    version: u16,
}

impl Dave {
    /// Starts a session, if the server asked for one.
    ///
    /// `version` is what the session description said, not what the identify
    /// asked for - the server decides, and zero means it decided against.
    pub fn new(version: u16, user_id: &str, channel_id: &str) -> Result<Option<Dave>> {
        let Some(version) = NonZeroU16::new(version) else { return Ok(None) };
        let user = user_id.parse::<u64>().context("that is not a user id")?;
        // The *stream's* channel, not the conversation's: a Go Live session
        // is its own group, and naming the wrong channel builds a group
        // nobody else is in.
        let channel = channel_id.parse::<u64>().context("that is not a channel id")?;
        let session = davey::DaveSession::new(version, user, channel, None)
            .map_err(|e| anyhow::anyhow!("could not start DAVE: {e:?}"))?;
        Ok(Some(Dave { session, version: version.get() }))
    }

    /// This device asking to join the group.
    pub fn key_package(&mut self) -> Result<Vec<u8>> {
        self.session.create_key_package().map_err(|e| anyhow::anyhow!("could not make a key package: {e:?}"))
    }

    pub fn ready(&self) -> bool {
        self.session.is_ready()
    }

    /// Encrypts one video frame for the group.
    ///
    /// Before the transport sealing rather than instead of it: DAVE hides the
    /// picture from the server, and the packet encryption hides it from
    /// everybody between here and the server. They are different promises to
    /// different parties and both are wanted.
    pub fn encrypt_video(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        Ok(self
            .session
            .encrypt(davey::MediaType::VIDEO, davey::Codec::VP8, frame)
            .map_err(|e| anyhow::anyhow!("could not encrypt a frame: {e:?}"))?
            .into_owned())
    }

    /// Takes in one binary frame and says what to send back.
    /// `recognised` is the set of users this client expects in the group, or
    /// `None` when it does not know. None rather than an empty slice: an
    /// empty set is "nobody belongs here", which rejects every proposal and
    /// leaves a group that never forms.
    pub fn take_binary(&mut self, frame: &BinaryFrame<'_>, recognised: Option<&[u64]>) -> Reply {
        match frame.opcode {
            OP_EXTERNAL_SENDER => {
                if let Err(e) = self.session.set_external_sender(frame.payload) {
                    tracing::warn!("dave: the external sender was refused: {e:?}");
                }
                Reply::Nothing
            }
            OP_PROPOSALS => {
                // The first byte says whether these are being added or taken
                // away, and the rest are the proposals themselves.
                let Some((kind, proposals)) = frame.payload.split_first() else { return Reply::Nothing };
                let operation = match kind {
                    0 => davey::ProposalsOperationType::APPEND,
                    1 => davey::ProposalsOperationType::REVOKE,
                    other => {
                        tracing::warn!("dave: proposals with an operation of {other}, which is neither");
                        return Reply::Nothing;
                    }
                };
                match self.session.process_proposals(operation, proposals, recognised) {
                    Ok(Some(cw)) => {
                        // A commit with a welcome after it, both in one
                        // frame - the welcome is optional and its absence is
                        // simply a shorter frame.
                        let mut payload = cw.commit;
                        if let Some(welcome) = cw.welcome {
                            payload.extend_from_slice(&welcome);
                        }
                        Reply::Binary(OP_COMMIT_WELCOME, payload)
                    }
                    Ok(None) => Reply::Nothing,
                    Err(e) => {
                        tracing::warn!("dave: proposals could not be processed: {e:?}");
                        Reply::Nothing
                    }
                }
            }
            OP_ANNOUNCE_COMMIT_TRANSITION => {
                let Some((transition, commit)) = split_transition(frame.payload) else { return Reply::Nothing };
                match self.session.process_commit(commit) {
                    Ok(()) => self.acknowledge(transition),
                    Err(e) => {
                        tracing::warn!("dave: a commit would not apply: {e:?}");
                        // Said rather than ignored: the server rebuilds the
                        // group for a client that admits it is lost, and
                        // stays silent for one that pretends otherwise.
                        Reply::Json(json!({ "op": OP_INVALID_COMMIT_WELCOME, "d": { "transition_id": transition } }))
                    }
                }
            }
            OP_WELCOME => {
                let Some((transition, welcome)) = split_transition(frame.payload) else { return Reply::Nothing };
                match self.session.process_welcome(welcome) {
                    Ok(()) => self.acknowledge(transition),
                    Err(e) => {
                        tracing::warn!("dave: a welcome would not apply: {e:?}");
                        Reply::Json(json!({ "op": OP_INVALID_COMMIT_WELCOME, "d": { "transition_id": transition } }))
                    }
                }
            }
            other => {
                tracing::debug!("dave: binary opcode {other}, which this does not answer");
                Reply::Nothing
            }
        }
    }

    /// And one JSON frame.
    pub fn take_json(&mut self, op: u64, d: &Value) -> Reply {
        match op {
            OP_PREPARE_TRANSITION => {
                let transition = d["transition_id"].as_u64().unwrap_or(0);
                // Media keeps flowing while the group is rebuilt. Without
                // this, everybody's picture stops for the length of a
                // transition every time somebody joins or leaves.
                self.session.set_passthrough_mode(true, Some(120));
                self.acknowledge(transition)
            }
            OP_EXECUTE_TRANSITION => {
                self.session.set_passthrough_mode(false, None);
                Reply::Nothing
            }
            OP_PREPARE_EPOCH => {
                // Epoch 1 is the group starting over from nothing, which is
                // the one case where what is held is not merely out of date
                // but wrong.
                if d["epoch"].as_u64() == Some(1) {
                    if let Err(e) = self.session.reset() {
                        tracing::warn!("dave: could not start the group again: {e:?}");
                    }
                }
                Reply::Nothing
            }
            _ => Reply::Nothing,
        }
    }

    /// Saying a transition has been applied here.
    ///
    /// Transition zero is the group's first epoch and is not acknowledged -
    /// there is no transition to be ready for, and answering one the server
    /// never announced is how a connection ends up in an epoch by itself.
    fn acknowledge(&self, transition: u64) -> Reply {
        if transition == 0 {
            return Reply::Nothing;
        }
        Reply::Json(json!({
            "op": OP_TRANSITION_READY,
            "d": { "transition_id": transition, "protocol_version": self.version }
        }))
    }
}

/// A transition id and whatever followed it.
fn split_transition(payload: &[u8]) -> Option<(u64, &[u8])> {
    if payload.len() < 2 {
        return None;
    }
    Some((u16::from_be_bytes([payload[0], payload[1]]) as u64, &payload[2..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under v8 a server-to-client binary frame carries a sequence number
    /// ahead of the opcode. songbird speaks v4, where it does not - and
    /// reading a v8 frame the v4 way takes the top half of the sequence for
    /// an opcode and treats the rest as nonsense, which looks exactly like
    /// Discord sending something unrecognised.
    #[test]
    fn a_binary_frame_is_read_past_its_sequence_number() {
        let frame = [0x00, 0x07, OP_EXTERNAL_SENDER, 0xDE, 0xAD];
        let read = read_binary(&frame).expect("a frame");
        assert_eq!(read.opcode, OP_EXTERNAL_SENDER);
        assert_eq!(read.payload, &[0xDE, 0xAD]);

        // A frame with a sequence and an opcode and nothing else is still a
        // frame - an external sender of zero length is a thing Discord can
        // legitimately send.
        let bare = read_binary(&[0x00, 0x01, OP_WELCOME]).expect("a bare frame");
        assert_eq!(bare.opcode, OP_WELCOME);
        assert!(bare.payload.is_empty());

        // Anything shorter is not a frame at all.
        assert!(read_binary(&[0x00, 0x01]).is_none());
        assert!(read_binary(&[]).is_none());
    }

    /// The other direction has no sequence number.
    #[test]
    fn what_goes_out_carries_only_its_opcode() {
        assert_eq!(write_binary(OP_KEY_PACKAGE, &[1, 2, 3]), vec![OP_KEY_PACKAGE, 1, 2, 3]);
        assert_eq!(write_binary(OP_COMMIT_WELCOME, &[]), vec![OP_COMMIT_WELCOME]);
    }

    #[test]
    fn a_transition_id_is_read_off_the_front() {
        assert_eq!(split_transition(&[0x00, 0x05, 0xAA]), Some((5, &[0xAA][..])));
        assert_eq!(split_transition(&[0x01, 0x00]), Some((256, &[][..])));
        assert_eq!(split_transition(&[0x01]), None);
    }

    /// A session is only started when the server says a version is in force.
    /// Zero means it decided against, and building a group anyway would
    /// encrypt everything for an audience of one.
    #[test]
    fn no_version_means_no_session() {
        assert!(Dave::new(0, "1339667204756475924", "1540954302632300564").unwrap().is_none());
        assert!(Dave::new(1, "1339667204756475924", "1540954302632300564").unwrap().is_some());
        // And an id that is not a number is refused rather than silently
        // becoming zero, which would put this device in somebody else's
        // group.
        assert!(Dave::new(1, "not-a-user", "1540954302632300564").is_err());
    }
}
