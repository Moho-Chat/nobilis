//! OMEMO 2 (XEP-0384 0.8 and later, `urn:xmpp:omemo:2`): the same idea as the older namespace - a payload encrypted once, its
//! key encrypted to every device with a session per pair of devices - in a different wire form and with different primitives:
//! the identity key is published as Ed25519, the session is X3DH and the Double Ratchet with OMEMO's own labels, messages are
//! AES-256-CBC with a truncated HMAC, and what is encrypted is a whole stanza's worth of elements (XEP-0420, Stanza Content
//! Encryption) rather than a body.
//!
//! The identity key and the device id are the ones the older namespace uses (as python-omemo and the other clients that speak
//! both do), so a device is one device whichever namespace is talking to it, and its fingerprint and trust are the same. The
//! signed pre-key and the one-time pre-keys are this namespace's own.
//!
//! Written against the reference implementation (python-omemo's twomemo); `scripts/probe-xmpp-omemo2.mjs` is the test that
//! matters.

use super::curve::{self, KeyPair};
use super::super::xml::Attr;
use super::ratchet::hexarr;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::Engine;
use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use hkdf::Hkdf;
use hmac012::{Hmac, Mac};
use prost::Message as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use tokio_xmpp::minidom::Element;

pub const NS: &str = "urn:xmpp:omemo:2";
pub const DEVICES_NODE: &str = "urn:xmpp:omemo:2:devices";
pub const BUNDLES_NODE: &str = "urn:xmpp:omemo:2:bundles";
pub const SCE: &str = "urn:xmpp:sce:1";
const CLIENT: &str = "jabber:client";
/// How many message keys may be skipped in one go, and kept, per session (the reference implementation's numbers).
const MAX_SKIP: u32 = 1000;
const MAC_LENGTH: usize = 16;
/// How long a receiving chain may grow before this side answers with a heartbeat (XEP-0384's number).
const STALE: u32 = 53;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("the message is not in the form OMEMO 2 writes")]
    Malformed,
    #[error("the bundle's signed pre-key is not signed by its identity key")]
    BadSignature,
    #[error("the bundle has no one-time pre-key to start a session with")]
    NoPreKey,
    #[error("the message was started from a pre-key this device no longer has")]
    UnknownPreKey,
    #[error("the message's authentication failed")]
    Authentication,
    #[error("this message was already read")]
    Duplicate,
    #[error("too many messages were skipped")]
    TooManySkipped,
}

// --- the wire form (protobuf, proto2 with every field required) ------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
struct OmemoMessage {
    #[prost(uint32, required, tag = "1")]
    n: u32,
    #[prost(uint32, required, tag = "2")]
    pn: u32,
    #[prost(bytes = "vec", required, tag = "3")]
    dh_pub: Vec<u8>,
    #[prost(bytes = "vec", optional, tag = "4")]
    ciphertext: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct OmemoAuthenticatedMessage {
    #[prost(bytes = "vec", required, tag = "1")]
    mac: Vec<u8>,
    #[prost(bytes = "vec", required, tag = "2")]
    message: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct OmemoKeyExchange {
    #[prost(uint32, required, tag = "1")]
    pk_id: u32,
    #[prost(uint32, required, tag = "2")]
    spk_id: u32,
    #[prost(bytes = "vec", required, tag = "3")]
    ik: Vec<u8>,
    #[prost(bytes = "vec", required, tag = "4")]
    ek: Vec<u8>,
    #[prost(message, required, tag = "5")]
    message: OmemoAuthenticatedMessage,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Header {
    dh_pub: [u8; 32],
    pn: u32,
    n: u32,
}

/// How a session was started, carried with every message until the other side answers.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct KeyExchange {
    pub pk_id: u32,
    pub spk_id: u32,
    #[serde(with = "hexarr")]
    pub ik: [u8; 32],
    #[serde(with = "hexarr")]
    pub ek: [u8; 32],
}

fn arr32(v: &[u8]) -> Result<[u8; 32], Error> {
    v.try_into().map_err(|_| Error::Malformed)
}

/// Takes a `<key kex='true'>`'s bytes apart: how the session was started, and the message inside.
pub fn parse_key_exchange(bytes: &[u8]) -> Result<(KeyExchange, Vec<u8>), Error> {
    let k = OmemoKeyExchange::decode(bytes).map_err(|_| Error::Malformed)?;
    Ok((KeyExchange { pk_id: k.pk_id, spk_id: k.spk_id, ik: arr32(&k.ik)?, ek: arr32(&k.ek)? }, k.message.encode_to_vec()))
}

fn key_exchange_bytes(kex: &KeyExchange, message: &[u8]) -> Vec<u8> {
    let message = OmemoAuthenticatedMessage::decode(message).expect("this side wrote it");
    OmemoKeyExchange { pk_id: kex.pk_id, spk_id: kex.spk_id, ik: kex.ik.to_vec(), ek: kex.ek.to_vec(), message }.encode_to_vec()
}

// --- primitives --------------------------------------------------------------------------------------------------------------

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

fn hkdf<const N: usize>(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    Hkdf::<Sha256>::new(Some(salt), ikm).expand(info, &mut out).expect("the output is short enough");
    out
}

/// Encryption key, authentication key and IV from a key (80 bytes of HKDF with a zero salt).
fn split_keys(key: &[u8], info: &[u8]) -> ([u8; 32], [u8; 32], [u8; 16]) {
    let d: [u8; 80] = hkdf(&[0u8; 32], key, info);
    (d[..32].try_into().expect("32"), d[32..64].try_into().expect("32"), d[64..].try_into().expect("16"))
}

fn cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    cbc::Encryptor::<aes::Aes256>::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plain)
}

fn cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], cipher: &[u8]) -> Result<Vec<u8>, Error> {
    cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into()).decrypt_padded_vec_mut::<Pkcs7>(cipher).map_err(|_| Error::Authentication)
}

fn random32() -> [u8; 32] {
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    k
}

fn public_of(private: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::x25519(*private, x25519_dalek::X25519_BASEPOINT_BYTES)
}

/// The Ed25519 form of an identity key: the same scalar times the Edwards base point, its sign as it comes (OMEMO 2 publishes
/// it so; the older namespace's XEdDSA forced the sign, this does not).
pub fn ed_public(private: &[u8; 32]) -> [u8; 32] {
    (ED25519_BASEPOINT_POINT * Scalar::from_bytes_mod_order(*private)).compress().to_bytes()
}

/// The Curve25519 form of an Ed25519 public key, which is what agreements use and what the older namespace publishes - so a
/// device's fingerprint is the same in both.
pub fn ed_to_montgomery(ed: &[u8; 32]) -> Option<[u8; 32]> {
    CompressedEdwardsY(*ed).decompress().map(|p| p.to_montgomery().to_bytes())
}

/// An Ed25519 signature made with a Curve25519 private key (XEdDSA's way of choosing the nonce, without forcing the sign), which
/// any Ed25519 verifier checks against [`ed_public`].
pub fn ed_sign(private: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let a = Scalar::from_bytes_mod_order(*private);
    let public = ed_public(private);
    let mut z = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut z);
    let mut prefix = [0xFFu8; 32];
    prefix[0] = 0xFE;
    let r = Scalar::from_hash(Sha512::new().chain_update(prefix).chain_update(a.to_bytes()).chain_update(message).chain_update(z));
    let big_r = (ED25519_BASEPOINT_POINT * r).compress();
    let h = Scalar::from_hash(Sha512::new().chain_update(big_r.as_bytes()).chain_update(public).chain_update(message));
    let s = r + h * a;
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(big_r.as_bytes());
    out[32..].copy_from_slice(s.as_bytes());
    out
}

pub fn ed_verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Some(a) = CompressedEdwardsY(*public).decompress() else { return false };
    let s: Option<Scalar> = Scalar::from_canonical_bytes(signature[32..].try_into().expect("32")).into();
    let Some(s) = s else { return false };
    let h = Scalar::from_hash(Sha512::new().chain_update(&signature[..32]).chain_update(public).chain_update(message));
    let check: EdwardsPoint = EdwardsPoint::vartime_double_scalar_mul_basepoint(&(-h), &a, &s);
    check.compress().as_bytes() == &signature[..32]
}

// --- bundles -----------------------------------------------------------------------------------------------------------------

/// A device's published keys.
#[derive(Clone, Debug, PartialEq)]
pub struct Bundle {
    /// Ed25519.
    pub ik: [u8; 32],
    pub spk_id: u32,
    pub spk: [u8; 32],
    pub spks: [u8; 64],
    pub pre_keys: Vec<(u32, [u8; 32])>,
}

pub fn parse_bundle(b: &Element) -> Option<Bundle> {
    let text = |name: &str| b.children().find(|c| c.name() == name).and_then(|c| b64().decode(c.text().trim()).ok());
    let spk_el = b.children().find(|c| c.name() == "spk")?;
    let pre_keys = b
        .children()
        .find(|c| c.name() == "prekeys")
        .map(|p| p.children().filter(|k| k.name() == "pk").filter_map(|k| Some((k.attr("id")?.parse().ok()?, b64().decode(k.text().trim()).ok()?.try_into().ok()?))).collect())
        .unwrap_or_default();
    Some(Bundle {
        ik: text("ik")?.try_into().ok()?,
        spk_id: spk_el.attr("id")?.parse().ok()?,
        spk: b64().decode(spk_el.text().trim()).ok()?.try_into().ok()?,
        spks: text("spks")?.try_into().ok()?,
        pre_keys,
    })
}

pub fn bundle_element(ik: &[u8; 32], spk_id: u32, spk: &[u8; 32], spks: &[u8], pre_keys: impl Iterator<Item = (u32, [u8; 32])>) -> Element {
    let mut pk = Element::builder("prekeys", NS).build();
    for (id, key) in pre_keys {
        pk.append_child(Element::builder("pk", NS).at("id", id.to_string()).append(b64().encode(key)).build());
    }
    Element::builder("bundle", NS)
        .append(Element::builder("spk", NS).at("id", spk_id.to_string()).append(b64().encode(spk)).build())
        .append(Element::builder("spks", NS).append(b64().encode(spks)).build())
        .append(Element::builder("ik", NS).append(b64().encode(ik)).build())
        .append(pk)
        .build()
}

pub fn devices_element(ids: &[u32]) -> Element {
    let mut list = Element::builder("devices", NS).build();
    for id in ids {
        list.append_child(Element::builder("device", NS).at("id", id.to_string()).build());
    }
    list
}

pub fn parse_devices(list: &Element) -> Vec<u32> {
    list.children().filter(|c| c.name() == "device").filter_map(|d| d.attr("id")?.parse().ok()).collect()
}

// --- the session -------------------------------------------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct Chain {
    #[serde(with = "hexarr")]
    key: [u8; 32],
    n: u32,
}

impl Chain {
    /// The next message key; the chain moves on.
    fn step(&mut self) -> [u8; 32] {
        let message_key = hmac(&self.key, &[&[0x01]]);
        self.key = hmac(&self.key, &[&[0x02]]);
        self.n += 1;
        message_key
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct Skipped {
    #[serde(with = "hexarr")]
    dh_pub: [u8; 32],
    n: u32,
    #[serde(with = "hexarr")]
    key: [u8; 32],
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Session {
    #[serde(with = "hexarr")]
    root: [u8; 32],
    #[serde(with = "hexarr")]
    own_ratchet: [u8; 32],
    #[serde(with = "hexarr")]
    other_ratchet: [u8; 32],
    send: Chain,
    recv: Option<Chain>,
    prev_send: u32,
    skipped: Vec<Skipped>,
    /// Both identity keys (Ed25519), the initiator's first.
    #[serde(with = "hexarr")]
    ad: [u8; 64],
    /// Set while this side started the session and the other has not answered: every message carries it.
    pub pending: Option<KeyExchange>,
    /// The ephemeral key of the exchange the other side started this session with, to know it again when it is repeated.
    #[serde(default, with = "opt_hex")]
    pub started_by: Option<[u8; 32]>,
    /// The other device's identity key (Ed25519).
    #[serde(with = "hexarr")]
    pub their_identity: [u8; 32],
    /// More than 53 messages were read on the other side's current ratchet key without this side sending anything: an empty
    /// message (a heartbeat) moves the ratchet on, as the reference implementation does.
    #[serde(skip)]
    pub heartbeat_due: bool,
}

mod opt_hex {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_some(&super::curve::hex(b)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let v: Option<String> = Option::deserialize(d)?;
        Ok(v.and_then(|t| super::curve::unhex(&t)).and_then(|b| b.try_into().ok()))
    }
}

/// The shared secret X3DH makes from four agreements.
fn x3dh_secret(dh: [[u8; 32]; 4]) -> [u8; 32] {
    let mut ikm = vec![0xFFu8; 32];
    for d in dh {
        ikm.extend_from_slice(&d);
    }
    hkdf(&[0u8; 32], &ikm, b"OMEMO X3DH")
}

impl Session {
    /// A new root key and a chain from an agreement.
    fn root_step(&mut self) -> Chain {
        let dh = x25519_dalek::x25519(self.own_ratchet, self.other_ratchet);
        let d: [u8; 64] = hkdf(&self.root, &dh, b"OMEMO Root Chain");
        self.root = d[..32].try_into().expect("32");
        Chain { key: d[32..].try_into().expect("32"), n: 0 }
    }

    /// Starts a session from a device's bundle. `identity` is this device's identity key pair.
    pub fn initiate(identity: &KeyPair, bundle: &Bundle) -> Result<Session, Error> {
        if !ed_verify(&bundle.ik, &bundle.spk, &bundle.spks) {
            return Err(Error::BadSignature);
        }
        use rand::seq::SliceRandom;
        let (pk_id, pk) = *bundle.pre_keys.choose(&mut rand::thread_rng()).ok_or(Error::NoPreKey)?;
        let their_ik = ed_to_montgomery(&bundle.ik).ok_or(Error::Malformed)?;
        let ek = random32();
        let secret = x3dh_secret([
            x25519_dalek::x25519(identity.private, bundle.spk),
            x25519_dalek::x25519(ek, their_ik),
            x25519_dalek::x25519(ek, bundle.spk),
            x25519_dalek::x25519(ek, pk),
        ]);
        let own_ik = ed_public(&identity.private);
        let mut ad = [0u8; 64];
        ad[..32].copy_from_slice(&own_ik);
        ad[32..].copy_from_slice(&bundle.ik);
        let mut s = Session {
            root: secret,
            own_ratchet: random32(),
            other_ratchet: bundle.spk,
            send: Chain { key: [0; 32], n: 0 },
            recv: None,
            prev_send: 0,
            skipped: Vec::new(),
            ad,
            pending: Some(KeyExchange { pk_id, spk_id: bundle.spk_id, ik: own_ik, ek: public_of(&ek) }),
            started_by: None,
            their_identity: bundle.ik,
            heartbeat_due: false,
        };
        s.send = s.root_step();
        Ok(s)
    }

    /// Accepts a session somebody started with this device, reading the message that came with it. `spk` and `pk` are the
    /// signed and one-time pre-keys the exchange names.
    pub fn accept(identity: &KeyPair, spk: &KeyPair, pk: &KeyPair, kex: &KeyExchange, message: &[u8]) -> Result<(Session, Vec<u8>), Error> {
        let their_ik = ed_to_montgomery(&kex.ik).ok_or(Error::Malformed)?;
        let secret = x3dh_secret([
            x25519_dalek::x25519(spk.private, their_ik),
            x25519_dalek::x25519(identity.private, kex.ek),
            x25519_dalek::x25519(spk.private, kex.ek),
            x25519_dalek::x25519(pk.private, kex.ek),
        ]);
        let (header, _) = open_header(message)?;
        let mut ad = [0u8; 64];
        ad[..32].copy_from_slice(&kex.ik);
        ad[32..].copy_from_slice(&ed_public(&identity.private));
        let mut s = Session {
            root: secret,
            own_ratchet: spk.private,
            other_ratchet: header.dh_pub,
            send: Chain { key: [0; 32], n: 0 },
            recv: None,
            prev_send: 0,
            skipped: Vec::new(),
            ad,
            pending: None,
            started_by: Some(kex.ek),
            their_identity: kex.ik,
            heartbeat_due: false,
        };
        s.recv = Some(s.root_step());
        s.own_ratchet = random32();
        s.send = s.root_step();
        let plain = s.decrypt(message)?;
        Ok((s, plain))
    }

    /// Encrypts (key material, in practice) to the other device: an `OMEMOAuthenticatedMessage`, wrapped in the key exchange
    /// while the other side has not answered. Returns the bytes and whether they are a key exchange.
    pub fn encrypt(&mut self, plain: &[u8]) -> (Vec<u8>, bool) {
        let header = Header { dh_pub: public_of(&self.own_ratchet), pn: self.prev_send, n: self.send.n };
        let mk = self.send.step();
        let (ek, ak, iv) = split_keys(&mk, b"OMEMO Message Key Material");
        let message = OmemoMessage { n: header.n, pn: header.pn, dh_pub: header.dh_pub.to_vec(), ciphertext: Some(cbc_encrypt(&ek, &iv, plain)) }.encode_to_vec();
        let mac = hmac(&ak, &[&self.ad, &message])[..MAC_LENGTH].to_vec();
        let out = OmemoAuthenticatedMessage { mac, message }.encode_to_vec();
        match &self.pending {
            Some(kex) => (key_exchange_bytes(kex, &out), true),
            None => (out, false),
        }
    }

    /// An empty message's key material: 32 zero bytes, encrypted with the session itself (it carries no payload; it moves the
    /// session on, or completes it).
    pub fn empty(&mut self) -> (Vec<u8>, bool) {
        self.encrypt(&[0u8; 32])
    }

    /// Decrypts an `OMEMOAuthenticatedMessage`. Nothing changes unless it was genuine.
    pub fn decrypt(&mut self, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        let (header, auth) = open_header(bytes)?;
        let mut next = self.clone();
        let (mk, found) = match next.skipped.iter().position(|k| k.dh_pub == header.dh_pub && k.n == header.n) {
            Some(i) => (next.skipped[i].key, Some(i)),
            None => (next.next_key(&header)?, None),
        };
        let (ek, ak, iv) = split_keys(&mk, b"OMEMO Message Key Material");
        let mac = hmac(&ak, &[&next.ad, &auth.message]);
        if mac[..MAC_LENGTH] != auth.mac[..] {
            return Err(Error::Authentication);
        }
        let message = OmemoMessage::decode(auth.message.as_slice()).map_err(|_| Error::Malformed)?;
        let plain = cbc_decrypt(&ek, &iv, message.ciphertext.as_deref().unwrap_or_default())?;
        if let Some(i) = found {
            next.skipped.remove(i);
        }
        let excess = next.skipped.len().saturating_sub(MAX_SKIP as usize);
        next.skipped.drain(..excess);
        // An answer: the other side has the session, so the exchange need not be sent again.
        next.pending = None;
        next.heartbeat_due = next.recv.as_ref().is_some_and(|r| r.n > STALE);
        *self = next;
        Ok(plain)
    }

    /// The message key for a header: a step of the Diffie-Hellman ratchet when the other side has a new ratchet key, the keys
    /// of messages that were skipped kept, then the receiving chain moved on to it.
    fn next_key(&mut self, header: &Header) -> Result<[u8; 32], Error> {
        if header.dh_pub != self.other_ratchet || self.recv.is_none() {
            if let Some(recv) = self.recv.as_mut() {
                let missing = header.pn.saturating_sub(recv.n);
                if missing <= MAX_SKIP {
                    for _ in 0..missing {
                        let n = recv.n;
                        let key = recv.step();
                        self.skipped.push(Skipped { dh_pub: self.other_ratchet, n, key });
                    }
                }
            }
            self.other_ratchet = header.dh_pub;
            self.recv = Some(self.root_step());
            self.own_ratchet = random32();
            self.prev_send = self.send.n;
            self.send = self.root_step();
        }
        let recv = self.recv.as_mut().expect("made above");
        if header.n.saturating_sub(recv.n) > MAX_SKIP {
            return Err(Error::TooManySkipped);
        }
        while recv.n < header.n {
            let n = recv.n;
            let key = recv.step();
            self.skipped.push(Skipped { dh_pub: header.dh_pub, n, key });
        }
        if header.n < recv.n {
            return Err(Error::Duplicate);
        }
        Ok(recv.step())
    }
}

/// The header of an `OMEMOAuthenticatedMessage`, and the message.
fn open_header(bytes: &[u8]) -> Result<(Header, OmemoAuthenticatedMessage), Error> {
    let auth = OmemoAuthenticatedMessage::decode(bytes).map_err(|_| Error::Malformed)?;
    let m = OmemoMessage::decode(auth.message.as_slice()).map_err(|_| Error::Malformed)?;
    Ok((Header { dh_pub: arr32(&m.dh_pub)?, pn: m.pn, n: m.n }, auth))
}

// --- the payload -------------------------------------------------------------------------------------------------------------

/// Encrypts a payload under a fresh key. Returns the ciphertext and the key material each device is sent (the key and the
/// authentication tag, 48 bytes).
pub fn encrypt_payload(plain: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let key = random32();
    let (ek, ak, iv) = split_keys(&key, b"OMEMO Payload");
    let cipher = cbc_encrypt(&ek, &iv, plain);
    let tag = hmac(&ak, &[&cipher]);
    let mut material = key.to_vec();
    material.extend_from_slice(&tag[..MAC_LENGTH]);
    (cipher, material)
}

pub fn decrypt_payload(cipher: &[u8], material: &[u8]) -> Result<Vec<u8>, Error> {
    if material.len() < 32 + MAC_LENGTH {
        return Err(Error::Malformed);
    }
    let (key, tag) = material.split_at(32);
    let (ek, ak, iv) = split_keys(key, b"OMEMO Payload");
    if hmac(&ak, &[cipher])[..MAC_LENGTH] != tag[..MAC_LENGTH] {
        return Err(Error::Authentication);
    }
    cbc_decrypt(&ek, &iv, cipher)
}

// --- the element -------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub jid: String,
    pub rid: u32,
    pub kex: bool,
    pub data: Vec<u8>,
}

/// An `<encrypted xmlns='urn:xmpp:omemo:2'/>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub sid: u32,
    pub keys: Vec<Key>,
    pub payload: Option<Vec<u8>>,
}

pub fn parse_envelope(e: &Element) -> Option<Envelope> {
    let header = e.children().find(|c| c.name() == "header")?;
    let sid = header.attr("sid")?.parse().ok()?;
    let mut keys = Vec::new();
    for group in header.children().filter(|c| c.name() == "keys") {
        let jid = group.attr("jid").unwrap_or_default().to_string();
        for k in group.children().filter(|c| c.name() == "key") {
            let (Some(rid), Ok(data)) = (k.attr("rid").and_then(|r| r.parse().ok()), b64().decode(k.text().trim())) else { continue };
            keys.push(Key { jid: jid.clone(), rid, kex: matches!(k.attr("kex"), Some("true" | "1")), data });
        }
    }
    let payload = e.children().find(|c| c.name() == "payload").and_then(|p| b64().decode(p.text().trim()).ok());
    Some(Envelope { sid, keys, payload })
}

pub fn envelope_element(sid: u32, keys: &[Key], payload: Option<&[u8]>) -> Element {
    let mut header = Element::builder("header", NS).at("sid", sid.to_string()).build();
    let mut jids: Vec<&str> = keys.iter().map(|k| k.jid.as_str()).collect();
    jids.dedup();
    let mut seen = std::collections::BTreeSet::new();
    for jid in jids {
        if !seen.insert(jid) {
            continue;
        }
        let mut group = Element::builder("keys", NS).at("jid", jid).build();
        for k in keys.iter().filter(|k| k.jid == jid) {
            let mut b = Element::builder("key", NS).at("rid", k.rid.to_string());
            if k.kex {
                b = b.at("kex", "true");
            }
            group.append_child(b.append(b64().encode(&k.data)).build());
        }
        header.append_child(group);
    }
    let mut e = Element::builder("encrypted", NS).append(header).build();
    if let Some(p) = payload {
        e.append_child(Element::builder("payload", NS).append(b64().encode(p)).build());
    }
    e
}

// --- Stanza Content Encryption (XEP-0420) ------------------------------------------------------------------------------------

/// What is encrypted: the elements (a body, in `jabber:client`, and whatever else belongs inside), random padding so the length
/// says little, and who it is from - which the receiver checks against who sent it.
pub fn sce_envelope(content: Vec<Element>, from: &str) -> Vec<u8> {
    let mut c = Element::builder("content", SCE).build();
    for e in content {
        c.append_child(e);
    }
    let pad_len = (rand::random::<u8>() % 32) as usize + 1;
    let pad: String = (0..pad_len).map(|_| char::from(b'a' + rand::random::<u8>() % 26)).collect();
    let envelope = Element::builder("envelope", SCE)
        .append(c)
        .append(Element::builder("rpad", SCE).append(pad).build())
        .append(Element::builder("from", SCE).at("jid", from).build())
        .build();
    String::from(&envelope).into_bytes()
}

pub fn body_element(text: &str) -> Element {
    Element::builder("body", CLIENT).append(text).build()
}

/// What an envelope held: its elements, and who it says it is from.
pub struct Opened {
    pub content: Vec<Element>,
    pub from: Option<String>,
}

pub fn open_sce(bytes: &[u8]) -> Result<Opened, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Malformed)?;
    let e: Element = text.parse().map_err(|_| Error::Malformed)?;
    if !e.is("envelope", SCE) {
        return Err(Error::Malformed);
    }
    let content = e.children().find(|c| c.is("content", SCE)).map(|c| c.children().cloned().collect()).unwrap_or_default();
    let from = e.children().find(|c| c.is("from", SCE)).and_then(|f| f.attr("jid")).map(str::to_string);
    Ok(Opened { content, from })
}

/// The body among an envelope's elements.
pub fn body_of(content: &[Element]) -> Option<String> {
    content.iter().find(|c| c.name() == "body" && (c.ns() == CLIENT || c.ns().is_empty())).map(|b| b.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Device {
        identity: KeyPair,
        spk: KeyPair,
        spks: [u8; 64],
        pk: KeyPair,
    }

    fn device() -> Device {
        let identity = KeyPair::generate();
        let spk = KeyPair::generate();
        let spks = ed_sign(&identity.private, &spk.public);
        Device { identity, spk, spks, pk: KeyPair::generate() }
    }

    fn bundle(d: &Device) -> Bundle {
        Bundle { ik: ed_public(&d.identity.private), spk_id: 1, spk: d.spk.public, spks: d.spks, pre_keys: vec![(7, d.pk.public)] }
    }

    #[test]
    fn signatures_are_ed25519_over_the_identity_key_published() {
        let k = KeyPair::generate();
        let sig = ed_sign(&k.private, b"label");
        assert!(ed_verify(&ed_public(&k.private), b"label", &sig));
        assert!(!ed_verify(&ed_public(&k.private), b"other", &sig));
        // The Curve25519 form is the one the older namespace publishes.
        assert_eq!(ed_to_montgomery(&ed_public(&k.private)), Some(k.public));
    }

    #[test]
    fn a_session_runs_both_ways_out_of_order() {
        let (alice, bob) = (device(), device());
        let mut a = Session::initiate(&alice.identity, &bundle(&bob)).unwrap();
        let (first, kex) = a.encrypt(b"one");
        assert!(kex);
        let (kx, inner) = parse_key_exchange(&first).unwrap();
        assert_eq!((kx.pk_id, kx.spk_id), (7, 1));
        let (mut b, plain) = Session::accept(&bob.identity, &bob.spk, &bob.pk, &kx, &inner).unwrap();
        assert_eq!(plain, b"one");
        assert_eq!(b.started_by, Some(kx.ek));
        // Until Bob answers, Alice keeps sending the exchange.
        let (second, kex2) = a.encrypt(b"two");
        assert!(kex2);
        let (_, inner2) = parse_key_exchange(&second).unwrap();
        assert_eq!(b.decrypt(&inner2).unwrap(), b"two");
        assert_eq!(b.decrypt(&inner2), Err(Error::Duplicate));
        let (r1, k1) = b.encrypt(b"back 1");
        let (r2, _) = b.encrypt(b"back 2");
        assert!(!k1);
        assert_eq!(a.decrypt(&r2).unwrap(), b"back 2");
        assert_eq!(a.decrypt(&r1).unwrap(), b"back 1");
        assert!(a.pending.is_none());
        let (third, kex3) = a.encrypt(b"three");
        assert!(!kex3);
        assert_eq!(b.decrypt(&third).unwrap(), b"three");
        // A forged message changes nothing.
        let mut forged = third.clone();
        let last = forged.len() - 1;
        forged[last] ^= 1;
        let before = b.clone();
        assert!(b.decrypt(&forged).is_err());
        assert_eq!(b, before);
    }

    #[test]
    fn many_messages_unanswered_ask_for_a_heartbeat_and_one_answer_resets_the_count() {
        let (alice, bob) = (device(), device());
        let mut a = Session::initiate(&alice.identity, &bundle(&bob)).unwrap();
        let (first, _) = a.encrypt(b"0");
        let (kx, inner) = parse_key_exchange(&first).unwrap();
        let (mut b, _) = Session::accept(&bob.identity, &bob.spk, &bob.pk, &kx, &inner).unwrap();
        let (answer, kex) = b.empty();
        assert!(!kex);
        assert_eq!(a.decrypt(&answer).unwrap(), [0u8; 32]);
        for i in 1..=STALE {
            b.decrypt(&a.encrypt(format!("{i}").as_bytes()).0).unwrap();
            assert!(!b.heartbeat_due, "{i}");
        }
        b.decrypt(&a.encrypt(b"one too many").0).unwrap();
        assert!(b.heartbeat_due);
        let (beat, _) = b.empty();
        a.decrypt(&beat).unwrap();
        b.decrypt(&a.encrypt(b"after").0).unwrap();
        assert!(!b.heartbeat_due, "the heartbeat moved the ratchet on: a new chain");
    }

    #[test]
    fn a_bundle_with_a_bad_signature_starts_nothing() {
        let (alice, bob) = (device(), device());
        let mut bad = bundle(&bob);
        bad.spks[0] ^= 1;
        assert_eq!(Session::initiate(&alice.identity, &bad).unwrap_err(), Error::BadSignature);
    }

    #[test]
    fn payload_and_envelope() {
        let (cipher, material) = encrypt_payload(b"<x/>");
        assert_eq!(material.len(), 48);
        assert_eq!(decrypt_payload(&cipher, &material).unwrap(), b"<x/>");
        let bytes = sce_envelope(vec![body_element("hi <there> & you")], "a@b.c");
        let opened = open_sce(&bytes).unwrap();
        assert_eq!(body_of(&opened.content).as_deref(), Some("hi <there> & you"));
        assert_eq!(opened.from.as_deref(), Some("a@b.c"));
        let keys = vec![Key { jid: "a@b.c".into(), rid: 3, kex: true, data: vec![1, 2] }, Key { jid: "d@e.f".into(), rid: 4, kex: false, data: vec![3] }];
        let e = envelope_element(9, &keys, Some(&cipher));
        let back = parse_envelope(&e).unwrap();
        assert_eq!((back.sid, back.keys, back.payload), (9, keys, Some(cipher)));
    }

    /// The wire form is the reference implementation's: proto2, every field written even when zero.
    #[test]
    fn wire_form_matches_the_reference() {
        let m = OmemoMessage { n: 0, pn: 0, dh_pub: b"aa".to_vec(), ciphertext: Some(b"x".to_vec()) }.encode_to_vec();
        assert_eq!(curve::hex(&m), "080010001a026161220178");
        let k = OmemoKeyExchange { pk_id: 0, spk_id: 0, ik: b"i".to_vec(), ek: b"e".to_vec(), message: OmemoAuthenticatedMessage { mac: b"m".to_vec(), message: b"z".to_vec() } }.encode_to_vec();
        assert_eq!(curve::hex(&k), "080010001a01692201652a060a016d12017a");
    }
}
