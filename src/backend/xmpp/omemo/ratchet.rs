//! The Signal protocol's session: X3DH to start one from a published bundle, then the Double Ratchet to keep it, in the wire
//! form (version 3) that OMEMO's older namespace carries and the other clients read. Written from the protocol's specification;
//! the test that matters is the one against another implementation (`scripts/probe-xmpp-omemo.mjs`).

use super::curve::{self, KeyPair};
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hkdf::Hkdf;
use hmac012::{Hmac, Mac};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const VERSION: u8 = 0x33;
const MAX_SKIPPED: usize = 2000;
const MAC_LENGTH: usize = 8;

#[derive(Clone, PartialEq, prost::Message)]
pub struct SignalMessage {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub ratchet_key: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "2")]
    pub counter: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub previous_counter: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub ciphertext: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PreKeySignalMessage {
    #[prost(uint32, optional, tag = "5")]
    pub registration_id: Option<u32>,
    #[prost(uint32, optional, tag = "1")]
    pub pre_key_id: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub signed_pre_key_id: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub base_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub identity_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub message: Option<Vec<u8>>,
}

/// What a peer publishes so that somebody who has never spoken to them can start a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    pub identity: [u8; 32],
    pub signed_pre_key_id: u32,
    pub signed_pre_key: [u8; 32],
    pub signature: [u8; 64],
    pub pre_key: Option<(u32, [u8; 32])>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("the signature on the signed pre-key is not the identity key's")]
    BadSignature,
    #[error("the message is not in a form this client reads")]
    Malformed,
    #[error("the message's authentication code is wrong")]
    BadMac,
    #[error("the message was already read, or is too old")]
    Duplicate,
    #[error("too many messages were skipped")]
    TooFarAhead,
    #[error("the message cannot be decrypted")]
    Decrypt,
    #[error("the pre-key it names is not one this device holds")]
    UnknownPreKey,
}

mod hexarr {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(v: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::curve::hex(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[u8; N], D::Error> {
        let text = String::deserialize(d)?;
        let bytes = super::curve::unhex(&text).ok_or_else(|| serde::de::Error::custom("not hex"))?;
        bytes.try_into().map_err(|_| serde::de::Error::custom("wrong length"))
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Pair {
    #[serde(with = "hexarr")]
    pub private: [u8; 32],
    #[serde(with = "hexarr")]
    pub public: [u8; 32],
}

impl From<&KeyPair> for Pair {
    fn from(k: &KeyPair) -> Self {
        Self { private: k.private, public: k.public }
    }
}

impl From<&Pair> for KeyPair {
    fn from(p: &Pair) -> Self {
        KeyPair { private: p.private, public: p.public }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct Keys {
    #[serde(with = "hexarr")]
    cipher: [u8; 32],
    #[serde(with = "hexarr")]
    mac: [u8; 32],
    #[serde(with = "hexarr")]
    iv: [u8; 16],
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct SenderChain {
    ratchet: Pair,
    #[serde(with = "hexarr")]
    chain_key: [u8; 32],
    index: u32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct ReceiverChain {
    #[serde(with = "hexarr")]
    ratchet: [u8; 32],
    #[serde(with = "hexarr")]
    chain_key: [u8; 32],
    index: u32,
    skipped: Vec<(u32, Keys)>,
}

/// What a session that has only sent says about how it was started, until the other side answers.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Pending {
    pub pre_key_id: Option<u32>,
    pub signed_pre_key_id: u32,
    #[serde(with = "hexarr")]
    pub base_key: [u8; 32],
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Session {
    #[serde(with = "hexarr")]
    pub their_identity: [u8; 32],
    #[serde(with = "hexarr")]
    pub our_identity: [u8; 32],
    pub our_registration_id: u32,
    #[serde(with = "hexarr")]
    root_key: [u8; 32],
    sender: SenderChain,
    receivers: Vec<ReceiverChain>,
    previous_counter: u32,
    pub pending: Option<Pending>,
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

fn hkdf<const N: usize>(ikm: &[u8], salt: Option<&[u8]>, info: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    Hkdf::<Sha256>::new(salt, ikm).expand(info, &mut out).expect("the output is short enough");
    out
}

/// A new root key and chain key from an agreement, with the old root key as salt.
fn create_chain(root_key: &[u8; 32], their_ratchet: &[u8; 32], our_ratchet: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let shared = curve::agree(our_ratchet, their_ratchet);
    let derived: [u8; 64] = hkdf(&shared, Some(root_key), b"WhisperRatchet");
    (derived[..32].try_into().expect("32"), derived[32..].try_into().expect("32"))
}

fn message_keys(chain_key: &[u8; 32]) -> Keys {
    let input = hmac(chain_key, &[&[0x01]]);
    let derived: [u8; 80] = hkdf(&input, None, b"WhisperMessageKeys");
    Keys { cipher: derived[..32].try_into().expect("32"), mac: derived[32..64].try_into().expect("32"), iv: derived[64..].try_into().expect("16") }
}

fn next_chain_key(chain_key: &[u8; 32]) -> [u8; 32] {
    hmac(chain_key, &[&[0x02]])
}

fn identity_serialized(k: &[u8; 32]) -> Vec<u8> {
    curve::serialize_public(k)
}

fn mac_of(keys: &Keys, sender: &[u8; 32], receiver: &[u8; 32], body: &[u8]) -> [u8; MAC_LENGTH] {
    let full = hmac(&keys.mac, &[&identity_serialized(sender), &identity_serialized(receiver), body]);
    full[..MAC_LENGTH].try_into().expect("8")
}

fn encrypt_cbc(keys: &Keys, plaintext: &[u8]) -> Vec<u8> {
    cbc::Encryptor::<aes::Aes256>::new(&keys.cipher.into(), &keys.iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

fn decrypt_cbc(keys: &Keys, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
    cbc::Decryptor::<aes::Aes256>::new(&keys.cipher.into(), &keys.iv.into()).decrypt_padded_vec_mut::<Pkcs7>(ciphertext).map_err(|_| Error::Decrypt)
}

impl Session {
    /// Starts a session from somebody's bundle (they have not spoken to us). Everything we send until they answer is a pre-key message.
    pub fn initiate(our_identity: &KeyPair, our_registration_id: u32, bundle: &Bundle) -> Result<Session, Error> {
        if !curve::verify(&bundle.identity, &curve::serialize_public(&bundle.signed_pre_key), &bundle.signature) {
            return Err(Error::BadSignature);
        }
        let base = KeyPair::generate();
        let mut secrets = vec![0xFFu8; 32];
        secrets.extend_from_slice(&curve::agree(&our_identity.private, &bundle.signed_pre_key));
        secrets.extend_from_slice(&curve::agree(&base.private, &bundle.identity));
        secrets.extend_from_slice(&curve::agree(&base.private, &bundle.signed_pre_key));
        if let Some((_, one_time)) = &bundle.pre_key {
            secrets.extend_from_slice(&curve::agree(&base.private, one_time));
        }
        let derived: [u8; 64] = hkdf(&secrets, None, b"WhisperText");
        let (root, chain): ([u8; 32], [u8; 32]) = (derived[..32].try_into().expect("32"), derived[32..].try_into().expect("32"));
        let sending = KeyPair::generate();
        let (root, send_chain) = create_chain(&root, &bundle.signed_pre_key, &sending.private);
        Ok(Session {
            their_identity: bundle.identity,
            our_identity: our_identity.public,
            our_registration_id,
            root_key: root,
            sender: SenderChain { ratchet: (&sending).into(), chain_key: send_chain, index: 0 },
            receivers: vec![ReceiverChain { ratchet: bundle.signed_pre_key, chain_key: chain, index: 0, skipped: Vec::new() }],
            previous_counter: 0,
            pending: Some(Pending { pre_key_id: bundle.pre_key.map(|p| p.0), signed_pre_key_id: bundle.signed_pre_key_id, base_key: base.public }),
        })
    }

    /// Starts a session from the first message somebody sent us, which names the pre-keys of ours they used.
    pub fn accept(our_identity: &KeyPair, our_signed: &KeyPair, our_one_time: Option<&KeyPair>, our_registration_id: u32, message: &PreKeySignalMessage) -> Result<Session, Error> {
        let their_identity = curve::parse_public(message.identity_key.as_deref().ok_or(Error::Malformed)?).ok_or(Error::Malformed)?;
        let base = curve::parse_public(message.base_key.as_deref().ok_or(Error::Malformed)?).ok_or(Error::Malformed)?;
        let mut secrets = vec![0xFFu8; 32];
        secrets.extend_from_slice(&curve::agree(&our_signed.private, &their_identity));
        secrets.extend_from_slice(&curve::agree(&our_identity.private, &base));
        secrets.extend_from_slice(&curve::agree(&our_signed.private, &base));
        if let Some(one_time) = our_one_time {
            secrets.extend_from_slice(&curve::agree(&one_time.private, &base));
        }
        let derived: [u8; 64] = hkdf(&secrets, None, b"WhisperText");
        Ok(Session {
            their_identity,
            our_identity: our_identity.public,
            our_registration_id,
            root_key: derived[..32].try_into().expect("32"),
            sender: SenderChain { ratchet: our_signed.into(), chain_key: derived[32..].try_into().expect("32"), index: 0 },
            receivers: Vec::new(),
            previous_counter: 0,
            pending: None,
        })
    }

    /// Encrypts. The result is the bytes that go in a `<key/>`.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let keys = message_keys(&self.sender.chain_key);
        let counter = self.sender.index;
        self.sender.chain_key = next_chain_key(&self.sender.chain_key);
        self.sender.index += 1;
        let proto = SignalMessage { ratchet_key: Some(curve::serialize_public(&self.sender.ratchet.public)), counter: Some(counter), previous_counter: Some(self.previous_counter), ciphertext: Some(encrypt_cbc(&keys, plaintext)) };
        let mut body = vec![VERSION];
        body.extend(proto.encode_to_vec());
        let mac = mac_of(&keys, &self.our_identity, &self.their_identity, &body);
        let mut signal = body;
        signal.extend_from_slice(&mac);
        match &self.pending {
            None => signal,
            Some(p) => {
                let wrapped = PreKeySignalMessage { registration_id: Some(self.our_registration_id), pre_key_id: p.pre_key_id, signed_pre_key_id: Some(p.signed_pre_key_id), base_key: Some(curve::serialize_public(&p.base_key)), identity_key: Some(identity_serialized(&self.our_identity)), message: Some(signal) };
                let mut out = vec![VERSION];
                out.extend(wrapped.encode_to_vec());
                out
            }
        }
    }

    /// Decrypts a `SignalMessage` (version byte, protobuf, MAC).
    pub fn decrypt(&mut self, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        if bytes.len() <= MAC_LENGTH + 1 || bytes[0] >> 4 != 3 {
            return Err(Error::Malformed);
        }
        let (body, mac) = bytes.split_at(bytes.len() - MAC_LENGTH);
        let message = SignalMessage::decode(&body[1..]).map_err(|_| Error::Malformed)?;
        let their_ratchet = curve::parse_public(message.ratchet_key.as_deref().ok_or(Error::Malformed)?).ok_or(Error::Malformed)?;
        let counter = message.counter.ok_or(Error::Malformed)?;
        let ciphertext = message.ciphertext.as_deref().ok_or(Error::Malformed)?;
        // Work on a copy: a message that does not check out must leave the session as it was.
        let mut next = self.clone();
        let at = match next.receivers.iter().position(|c| c.ratchet == their_ratchet) {
            Some(at) => at,
            None => {
                // They ratcheted: a new receiving chain from what they sent, then a new sending chain of our own.
                let (root, recv) = create_chain(&next.root_key, &their_ratchet, &next.sender.ratchet.private);
                let ours = KeyPair::generate();
                let (root, send) = create_chain(&root, &their_ratchet, &ours.private);
                next.receivers.push(ReceiverChain { ratchet: their_ratchet, chain_key: recv, index: 0, skipped: Vec::new() });
                if next.receivers.len() > 5 {
                    next.receivers.remove(0);
                }
                next.previous_counter = next.sender.index.saturating_sub(1);
                next.sender = SenderChain { ratchet: (&ours).into(), chain_key: send, index: 0 };
                next.root_key = root;
                next.receivers.len() - 1
            }
        };
        let (their_identity, our_identity) = (next.their_identity, next.our_identity);
        let chain = &mut next.receivers[at];
        let keys = if counter < chain.index {
            let found = chain.skipped.iter().position(|(c, _)| *c == counter).ok_or(Error::Duplicate)?;
            chain.skipped.remove(found).1
        } else {
            if (counter - chain.index) as usize > MAX_SKIPPED {
                return Err(Error::TooFarAhead);
            }
            while chain.index < counter {
                chain.skipped.push((chain.index, message_keys(&chain.chain_key)));
                chain.chain_key = next_chain_key(&chain.chain_key);
                chain.index += 1;
            }
            let keys = message_keys(&chain.chain_key);
            chain.chain_key = next_chain_key(&chain.chain_key);
            chain.index += 1;
            keys
        };
        while chain.skipped.len() > MAX_SKIPPED {
            chain.skipped.remove(0);
        }
        let wanted = mac_of(&keys, &their_identity, &our_identity, body);
        if !constant_time_eq(&wanted, mac) {
            return Err(Error::BadMac);
        }
        let plain = decrypt_cbc(&keys, ciphertext)?;
        // They answered: the session no longer needs to say how it began.
        next.pending = None;
        *self = next;
        Ok(plain)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The pieces of a message that starts a session, if `bytes` is one.
pub fn parse_pre_key(bytes: &[u8]) -> Result<PreKeySignalMessage, Error> {
    if bytes.len() < 2 || bytes[0] >> 4 != 3 {
        return Err(Error::Malformed);
    }
    PreKeySignalMessage::decode(&bytes[1..]).map_err(|_| Error::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Peer {
        identity: KeyPair,
        signed: KeyPair,
        one_time: KeyPair,
        registration: u32,
    }

    fn peer(registration: u32) -> Peer {
        Peer { identity: KeyPair::generate(), signed: KeyPair::generate(), one_time: KeyPair::generate(), registration }
    }

    fn bundle(p: &Peer) -> Bundle {
        Bundle { identity: p.identity.public, signed_pre_key_id: 1, signed_pre_key: p.signed.public, signature: curve::sign(&p.identity.private, &p.signed.public_serialized()), pre_key: Some((7, p.one_time.public)) }
    }

    /// Alice writes first and Bob starts his side from that message, as happens in life.
    fn pair() -> (Session, Session, Vec<u8>) {
        let (alice, bob) = (peer(1), peer(2));
        let mut a = Session::initiate(&alice.identity, alice.registration, &bundle(&bob)).unwrap();
        let first = a.encrypt(b"hello bob");
        let wrapped = parse_pre_key(&first).unwrap();
        let mut b = Session::accept(&bob.identity, &bob.signed, Some(&bob.one_time), bob.registration, &wrapped).unwrap();
        let plain = b.decrypt(wrapped.message.as_deref().unwrap()).unwrap();
        assert_eq!(plain, b"hello bob");
        (a, b, first)
    }

    #[test]
    fn a_session_starts_from_a_bundle_and_the_first_message_names_what_it_used() {
        let (a, _b, first) = pair();
        let w = parse_pre_key(&first).unwrap();
        assert_eq!((w.pre_key_id, w.signed_pre_key_id, w.registration_id), (Some(7), Some(1), Some(1)));
        assert!(a.pending.is_some(), "until Bob answers, Alice must keep saying how it began");
    }

    #[test]
    fn they_talk_in_both_directions_through_many_ratchet_steps() {
        let (mut a, mut b, _) = pair();
        for round in 0..6u8 {
            let to_a = b.encrypt(&[round; 5]);
            assert_eq!(a.decrypt(&to_a).unwrap(), vec![round; 5]);
            assert!(a.pending.is_none(), "an answer ends the pre-key phase");
            let to_b = a.encrypt(&[round, 9, 9]);
            assert_eq!(b.decrypt(&to_b).unwrap(), vec![round, 9, 9]);
        }
    }

    #[test]
    fn messages_that_arrive_out_of_order_are_still_read_once() {
        let (mut a, mut b, _) = pair();
        let reply = b.encrypt(b"first");
        a.decrypt(&reply).unwrap();
        let (m1, m2, m3) = (a.encrypt(b"one"), a.encrypt(b"two"), a.encrypt(b"three"));
        assert_eq!(b.decrypt(&m3).unwrap(), b"three");
        assert_eq!(b.decrypt(&m1).unwrap(), b"one");
        assert_eq!(b.decrypt(&m2).unwrap(), b"two");
        assert_eq!(b.decrypt(&m2), Err(Error::Duplicate), "a message is read once");
    }

    #[test]
    fn a_message_that_was_changed_is_refused_and_leaves_the_session_as_it_was() {
        let (mut a, mut b, _) = pair();
        let reply = b.encrypt(b"x");
        a.decrypt(&reply).unwrap();
        let good = a.encrypt(b"genuine");
        let mut bad = good.clone();
        let at = bad.len() / 2;
        bad[at] ^= 0x01;
        assert!(b.decrypt(&bad).is_err());
        assert_eq!(b.decrypt(&good).unwrap(), b"genuine", "the refused one did not use up the keys");
        let mut wrong_mac = a.encrypt(b"again");
        let last = wrong_mac.len() - 1;
        wrong_mac[last] ^= 1;
        assert_eq!(b.decrypt(&wrong_mac), Err(Error::BadMac));
    }

    #[test]
    fn a_bundle_whose_signature_is_not_the_identitys_is_refused() {
        let (alice, bob) = (peer(1), peer(2));
        let mut b = bundle(&bob);
        b.signature = curve::sign(&alice.identity.private, &bob.signed.public_serialized());
        assert_eq!(Session::initiate(&alice.identity, 1, &b).err(), Some(Error::BadSignature));
    }

    #[test]
    fn a_session_survives_being_written_down_and_read_back() {
        let (mut a, mut b, _) = pair();
        let reply = b.encrypt(b"r");
        a.decrypt(&reply).unwrap();
        let saved = serde_json::to_string(&a).unwrap();
        let mut a2: Session = serde_json::from_str(&saved).unwrap();
        assert_eq!(a2, a);
        let m = a2.encrypt(b"after the restart");
        assert_eq!(b.decrypt(&m).unwrap(), b"after the restart");
    }

    #[test]
    fn without_a_one_time_pre_key_the_session_still_starts() {
        let (alice, bob) = (peer(1), peer(2));
        let mut bundle = bundle(&bob);
        bundle.pre_key = None;
        let mut a = Session::initiate(&alice.identity, 1, &bundle).unwrap();
        let first = a.encrypt(b"hi");
        let w = parse_pre_key(&first).unwrap();
        assert_eq!(w.pre_key_id, None);
        let mut b = Session::accept(&bob.identity, &bob.signed, None, 2, &w).unwrap();
        assert_eq!(b.decrypt(w.message.as_deref().unwrap()).unwrap(), b"hi");
    }
}
