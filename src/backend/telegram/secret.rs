//! Secret chats: the end-to-end layer, apart from talking to Telegram (that is `secret_flow`).
//!
//! What is here is pure and unit-tested: the key exchange's arithmetic, MTProto 2.0's encryption as secret chats use
//! it, the sequence numbers, the end-to-end TL that travels inside (`decryptedMessageLayer` and what it carries), the
//! file keys, and what is kept about each chat between runs.
//!
//! # What is kept, and where
//!
//! A secret chat lives on one device: the key is made here and never leaves. Each account's chats are one private
//! file beside its session (`telegram/<user_id>.secret`), 0600 like the session: the key, the other side's layer,
//! both sequence counters, the last messages sent (a resend can be asked for), and a re-key in progress. Losing the
//! file loses the chats, as uninstalling Telegram's own app does.
//!
//! # References
//!
//! core.telegram.org/api/end-to-end (key exchange, encryption, layers), /api/end-to-end/seq_no, /api/end-to-end/pfs,
//! and /schema/end-to-end for the constructors.

use anyhow::{Context, Result};
use grammers_client::tl;
use grammers_client::tl::{Cursor, Deserializable, Serializable};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use sha1::Digest as _;
use sha2::Digest as _;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

/// The end-to-end layer moho speaks: high enough for the current message and document constructors (143), and what
/// it tells the other side in `decryptedMessageActionNotifyLayer`.
pub const LAYER: i32 = 143;
/// The lowest layer a peer may have and still be talked to: below it messages are not wrapped in a layer at all.
pub const MIN_LAYER: i32 = 46;
/// How many messages are kept for a resend the other side may ask for.
const KEEP_SENT: usize = 200;
/// Official apps make a new key after this many messages under one, or after a week.
pub const REKEY_AFTER: u32 = 100;
pub const REKEY_AGE: i64 = 7 * 24 * 3600;

// Constructor ids from /schema/end-to-end.
const DECRYPTED_MESSAGE_LAYER: u32 = 0x1be31789;
const DECRYPTED_MESSAGE_73: u32 = 0x91cc4674;
const DECRYPTED_MESSAGE_45: u32 = 0x36b091de;
const DECRYPTED_MESSAGE_17: u32 = 0x204d3878;
const DECRYPTED_MESSAGE_SERVICE: u32 = 0x73164160;
const DECRYPTED_MESSAGE_SERVICE_8: u32 = 0xaa48327d;
const ACTION_SET_TTL: u32 = 0xa1733aec;
const ACTION_READ: u32 = 0x0c4f40be;
const ACTION_DELETE: u32 = 0x65614304;
const ACTION_SCREENSHOT: u32 = 0x8ac1f475;
const ACTION_FLUSH: u32 = 0x6719e45c;
const ACTION_RESEND: u32 = 0x511110b0;
const ACTION_NOTIFY_LAYER: u32 = 0xf3048883;
const ACTION_TYPING: u32 = 0xccb27641;
const ACTION_REQUEST_KEY: u32 = 0xf3c9611b;
const ACTION_ACCEPT_KEY: u32 = 0x6fe1735b;
const ACTION_ABORT_KEY: u32 = 0xdd05ec6b;
const ACTION_COMMIT_KEY: u32 = 0xec2e0b9b;
const ACTION_NOOP: u32 = 0xa82fdd63;
const MEDIA_EMPTY: u32 = 0x089f5c4a;
const MEDIA_PHOTO: u32 = 0xf1fa8d78;
const MEDIA_DOCUMENT: u32 = 0x6abd9782;
const MEDIA_DOCUMENT_45: u32 = 0x7afe8ae2;
const MEDIA_GEO: u32 = 0x35480a59;
const MEDIA_CONTACT: u32 = 0x588a0a97;
const MEDIA_EXTERNAL_DOCUMENT: u32 = 0xfa95b0dd;
const MEDIA_WEBPAGE: u32 = 0xe50511d8;
const MEDIA_VENUE: u32 = 0x8a0df56f;

// ---------------------------------------------------------------------------------------------------------------
// The key exchange.

/// A secret exponent, as a 256-byte big-endian number.
pub fn random_exponent() -> Vec<u8> {
    let mut a = vec![0u8; 256];
    rand::Rng::fill(&mut rand::thread_rng(), &mut a[..]);
    a
}

/// `pow(g, a) mod p`, 256 bytes.
pub fn power(base: &[u8], exponent: &[u8], p: &[u8]) -> Vec<u8> {
    padded(&BigUint::from_bytes_be(base).modpow(&BigUint::from_bytes_be(exponent), &BigUint::from_bytes_be(p)))
}

/// `g` as bytes, for `power`.
pub fn g_bytes(g: i32) -> Vec<u8> {
    BigUint::from(g as u32).to_bytes_be()
}

fn padded(n: &BigUint) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    let mut out = vec![0u8; 256usize.saturating_sub(bytes.len())];
    out.extend(bytes);
    out
}

/// Whether the other side's `g_a` or `g_b` is safe to use: `1 < g_x < p - 1`, and away from both ends by 2^(2048-64)
/// as the spec recommends.
pub fn good_public(g_x: &[u8], p: &[u8]) -> bool {
    let x = BigUint::from_bytes_be(g_x);
    let p = BigUint::from_bytes_be(p);
    let margin = BigUint::from(1u8) << (2048 - 64);
    x > margin && p > &margin + &margin && x < &p - &margin
}

/// Whether Telegram's DH prime and generator are what the spec demands (a 2048-bit safe prime, `g` a quadratic
/// residue): the same check grammers makes for the cloud password.
pub fn good_config(p: &[u8], g: i32) -> bool {
    p.len() == 256 && grammers_crypto::two_factor_auth::check_p_and_g(p, &g)
}

/// A key's fingerprint: the last 64 bits of its SHA1, as Telegram reads them (little-endian).
pub fn fingerprint(key: &[u8]) -> i64 {
    let digest = sha1::Sha1::digest(key);
    i64::from_le_bytes(digest[12..20].try_into().unwrap_or_default())
}

/// What both people compare to see that nobody is in the middle: the first 128 bits of SHA1 of the first key, then
/// the first 160 bits of SHA256 of the key in use when the chat reached layer 46 (for moho, the same key).
pub fn visualization(first_key: &[u8], layer46_key: &[u8]) -> Vec<u8> {
    let mut out = sha1::Sha1::digest(first_key)[..16].to_vec();
    out.extend_from_slice(&sha2::Sha256::digest(layer46_key)[..20]);
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Encryption: MTProto 2.0 as secret chats use it.

fn aes_key_iv(key: &[u8], msg_key: &[u8], x: usize) -> ([u8; 32], [u8; 32]) {
    let a = sha2::Sha256::new().chain_update(msg_key).chain_update(&key[x..x + 36]).finalize();
    let b = sha2::Sha256::new().chain_update(&key[40 + x..40 + x + 36]).chain_update(msg_key).finalize();
    let mut aes_key = [0u8; 32];
    aes_key[..8].copy_from_slice(&a[..8]);
    aes_key[8..24].copy_from_slice(&b[8..24]);
    aes_key[24..].copy_from_slice(&a[24..32]);
    let mut aes_iv = [0u8; 32];
    aes_iv[..8].copy_from_slice(&b[..8]);
    aes_iv[8..24].copy_from_slice(&a[8..24]);
    aes_iv[24..].copy_from_slice(&b[24..32]);
    (aes_key, aes_iv)
}

/// Encrypts a message for the other side. `x` is 0 for what the chat's originator sends and 8 for the other way.
/// The result is what `messages.sendEncrypted` carries: key fingerprint, msg_key, ciphertext.
pub fn encrypt(key: &[u8], x: usize, payload: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(payload.len() + 1100);
    plain.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    plain.extend_from_slice(payload);
    // 12 to 1024 bytes of padding, to a multiple of 16.
    let mut pad = 12 + rand::Rng::gen_range(&mut rand::thread_rng(), 0..62) * 16;
    pad += (16 - (plain.len() + pad) % 16) % 16;
    let mut noise = vec![0u8; pad];
    rand::Rng::fill(&mut rand::thread_rng(), &mut noise[..]);
    plain.extend(noise);
    let large = sha2::Sha256::new().chain_update(&key[88 + x..88 + x + 32]).chain_update(&plain).finalize();
    let msg_key = &large[8..24];
    let (aes_key, aes_iv) = aes_key_iv(key, msg_key, x);
    grammers_crypto::aes::ige_encrypt(&mut plain, &aes_key, &aes_iv);
    let mut out = Vec::with_capacity(24 + plain.len());
    out.extend_from_slice(&fingerprint(key).to_le_bytes());
    out.extend_from_slice(msg_key);
    out.extend(plain);
    out
}

/// The key fingerprint a message was encrypted under: its first eight bytes.
pub fn fingerprint_of(data: &[u8]) -> Option<i64> {
    Some(i64::from_le_bytes(data.get(..8)?.try_into().ok()?))
}

/// Decrypts what the other side sent, checking its msg_key. `x` is the sender's (8 when the originator receives).
pub fn decrypt(key: &[u8], x: usize, data: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(data.len() >= 24 + 16 && (data.len() - 24).is_multiple_of(16), "the message is the wrong length");
    anyhow::ensure!(fingerprint_of(data) == Some(fingerprint(key)), "the message is under another key");
    let msg_key = &data[8..24];
    let (aes_key, aes_iv) = aes_key_iv(key, msg_key, x);
    let mut plain = data[24..].to_vec();
    grammers_crypto::aes::ige_decrypt(&mut plain, &aes_key, &aes_iv);
    let large = sha2::Sha256::new().chain_update(&key[88 + x..88 + x + 32]).chain_update(&plain).finalize();
    anyhow::ensure!(&large[8..24] == msg_key, "the message does not check out (msg_key differs)");
    let len = u32::from_le_bytes(plain[..4].try_into()?) as usize;
    anyhow::ensure!(len + 4 <= plain.len() && plain.len() - 4 - len <= 1024 + 15, "the message's length does not check out");
    Ok(plain[4..4 + len].to_vec())
}

// ---------------------------------------------------------------------------------------------------------------
// Files: each has a key and iv of its own, AES-256-IGE, and a fingerprint from MD5.

/// A file's own key and iv.
pub fn file_key() -> ([u8; 32], [u8; 32]) {
    let mut k = [0u8; 32];
    let mut iv = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut k[..]);
    rand::Rng::fill(&mut rand::thread_rng(), &mut iv[..]);
    (k, iv)
}

/// `digest = md5(key + iv)`, `fingerprint = substr(digest, 0, 4) XOR substr(digest, 4, 4)`.
pub fn file_fingerprint(key: &[u8], iv: &[u8]) -> i32 {
    let mut both = key.to_vec();
    both.extend_from_slice(iv);
    let d = md5::compute(both).0;
    let a = i32::from_le_bytes(d[..4].try_into().unwrap_or_default());
    let b = i32::from_le_bytes(d[4..8].try_into().unwrap_or_default());
    a ^ b
}

/// A file encrypted for sending, padded with random bytes to a multiple of 16.
pub fn encrypt_file(bytes: &[u8], key: &[u8; 32], iv: &[u8; 32]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let pad = (16 - out.len() % 16) % 16;
    let mut noise = vec![0u8; pad];
    rand::Rng::fill(&mut rand::thread_rng(), &mut noise[..]);
    out.extend(noise);
    grammers_crypto::aes::ige_encrypt(&mut out, key, iv);
    out
}

/// A file decrypted, cut to its real size.
pub fn decrypt_file(mut bytes: Vec<u8>, key: &[u8], iv: &[u8], size: usize) -> Result<Vec<u8>> {
    let key: [u8; 32] = key.try_into().context("a file key is 32 bytes")?;
    let iv: [u8; 32] = iv.try_into().context("a file iv is 32 bytes")?;
    anyhow::ensure!(bytes.len().is_multiple_of(16), "the file is the wrong length");
    grammers_crypto::aes::ige_decrypt(&mut bytes, &key, &iv);
    bytes.truncate(size.min(bytes.len()));
    Ok(bytes)
}

// ---------------------------------------------------------------------------------------------------------------
// Sequence numbers.

/// The wire sequence numbers for the next message sent: `(in_seq_no, out_seq_no)`. Each is twice a counter plus a
/// parity bit that says who started the chat.
pub fn seq_numbers(originator: bool, in_raw: i32, out_raw: i32) -> (i32, i32) {
    let x = i32::from(originator);
    (2 * in_raw + (1 - x), 2 * out_raw + x)
}

/// Where a message the other side sent falls, by its `out_seq_no`.
#[derive(Debug, PartialEq, Eq)]
pub enum Arrival {
    /// The one expected next.
    Next,
    /// One already had.
    Repeat,
    /// One after a gap: the raw counters missing are `from..=to`.
    Early { from: i32, to: i32 },
    /// The parity is wrong: the chat cannot be trusted.
    Broken,
}

/// Classifies an incoming `out_seq_no` against how many have arrived in order (`in_raw`).
pub fn arrival(originator: bool, in_raw: i32, their_out: i32) -> Arrival {
    // Their parity is the other one's: the originator's own messages are odd.
    let parity = i32::from(!originator);
    if their_out.rem_euclid(2) != parity {
        return Arrival::Broken;
    }
    let raw = (their_out - parity) / 2;
    match raw.cmp(&in_raw) {
        std::cmp::Ordering::Equal => Arrival::Next,
        std::cmp::Ordering::Less => Arrival::Repeat,
        std::cmp::Ordering::Greater => Arrival::Early { from: in_raw, to: raw - 1 },
    }
}

/// The wire numbers a resend asks for, given raw counters of the other side's messages.
pub fn resend_range(originator: bool, from: i32, to: i32) -> (i32, i32) {
    let parity = i32::from(!originator);
    (2 * from + parity, 2 * to + parity)
}

/// The raw counters of this side's messages that a resend from the other side names.
pub fn resend_wanted(originator: bool, start: i32, end: i32) -> std::ops::RangeInclusive<i32> {
    let parity = i32::from(originator);
    ((start - parity).max(0) / 2)..=((end - parity).max(0) / 2)
}

// ---------------------------------------------------------------------------------------------------------------
// The end-to-end TL.

/// What a decrypted message carries.
#[derive(Debug, Clone, PartialEq)]
pub enum Media {
    Photo { thumb: Vec<u8>, w: i32, h: i32, size: i64, key: Vec<u8>, iv: Vec<u8>, caption: String },
    Document { thumb: Vec<u8>, mime: String, size: i64, key: Vec<u8>, iv: Vec<u8>, attributes: Vec<tl::enums::DocumentAttribute>, caption: String },
    /// A sticker (or other document) from Telegram's servers, sent by reference.
    External { mime: String, attributes: Vec<tl::enums::DocumentAttribute> },
    Geo { lat: f64, long: f64 },
    Contact { phone: String, first: String, last: String },
    WebPage { url: String },
    Venue { lat: f64, long: f64, title: String, address: String },
    /// Something moho cannot read: the message is shown with a note.
    Unknown,
}

/// What a service message does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SetTtl(i32),
    Read(Vec<i64>),
    Delete(Vec<i64>),
    Screenshot(Vec<i64>),
    Flush,
    Resend { start: i32, end: i32 },
    NotifyLayer(i32),
    Typing,
    RequestKey { exchange: i64, g_a: Vec<u8> },
    AcceptKey { exchange: i64, g_b: Vec<u8>, fingerprint: i64 },
    AbortKey(i64),
    CommitKey { exchange: i64, fingerprint: i64 },
    Noop,
    Unknown,
}

/// One message, ordinary or service.
#[derive(Debug, Clone, PartialEq)]
pub enum Decrypted {
    Message { random_id: i64, ttl: i32, text: String, media: Option<Media>, entities: Vec<tl::enums::MessageEntity>, reply_to: Option<i64>, silent: bool },
    Service { random_id: i64, action: Action },
}

/// A message with the layer and sequence numbers around it.
#[derive(Debug, Clone, PartialEq)]
pub struct Layered {
    pub layer: i32,
    pub in_seq: i32,
    pub out_seq: i32,
    pub message: Decrypted,
}

fn put<T: Serializable>(out: &mut Vec<u8>, v: T) {
    v.serialize(out);
}

fn write_media(out: &mut Vec<u8>, media: &Media) {
    match media {
        Media::Photo { thumb, w, h, size, key, iv, caption } => {
            put(out, MEDIA_PHOTO);
            put(out, thumb.clone());
            put(out, if thumb.is_empty() { 0 } else { 90 });
            put(out, if thumb.is_empty() { 0 } else { 90 });
            put(out, *w);
            put(out, *h);
            put(out, *size as i32);
            put(out, key.clone());
            put(out, iv.clone());
            put(out, caption.clone());
        }
        Media::Document { thumb, mime, size, key, iv, attributes, caption } => {
            put(out, MEDIA_DOCUMENT);
            put(out, thumb.clone());
            put(out, if thumb.is_empty() { 0 } else { 90 });
            put(out, if thumb.is_empty() { 0 } else { 90 });
            put(out, mime.clone());
            put(out, *size);
            put(out, key.clone());
            put(out, iv.clone());
            put(out, attributes.clone());
            put(out, caption.clone());
        }
        Media::Geo { lat, long } => {
            put(out, MEDIA_GEO);
            put(out, *lat);
            put(out, *long);
        }
        // Only what moho sends is written; the rest is only ever read.
        _ => put(out, MEDIA_EMPTY),
    }
}

fn write_action(out: &mut Vec<u8>, action: &Action) {
    match action {
        Action::SetTtl(t) => {
            put(out, ACTION_SET_TTL);
            put(out, *t);
        }
        Action::Read(ids) => {
            put(out, ACTION_READ);
            put(out, ids.clone());
        }
        Action::Delete(ids) => {
            put(out, ACTION_DELETE);
            put(out, ids.clone());
        }
        Action::Screenshot(ids) => {
            put(out, ACTION_SCREENSHOT);
            put(out, ids.clone());
        }
        Action::Flush => put(out, ACTION_FLUSH),
        Action::Resend { start, end } => {
            put(out, ACTION_RESEND);
            put(out, *start);
            put(out, *end);
        }
        Action::NotifyLayer(l) => {
            put(out, ACTION_NOTIFY_LAYER);
            put(out, *l);
        }
        Action::Typing => {
            put(out, ACTION_TYPING);
            tl::enums::SendMessageAction::SendMessageTypingAction.serialize(out);
        }
        Action::RequestKey { exchange, g_a } => {
            put(out, ACTION_REQUEST_KEY);
            put(out, *exchange);
            put(out, g_a.clone());
        }
        Action::AcceptKey { exchange, g_b, fingerprint } => {
            put(out, ACTION_ACCEPT_KEY);
            put(out, *exchange);
            put(out, g_b.clone());
            put(out, *fingerprint);
        }
        Action::AbortKey(e) => {
            put(out, ACTION_ABORT_KEY);
            put(out, *e);
        }
        Action::CommitKey { exchange, fingerprint } => {
            put(out, ACTION_COMMIT_KEY);
            put(out, *exchange);
            put(out, *fingerprint);
        }
        Action::Noop | Action::Unknown => put(out, ACTION_NOOP),
    }
}

/// A message as the end-to-end TL, wrapped in its layer.
pub fn encode(layered: &Layered) -> Vec<u8> {
    let mut out = Vec::new();
    put(&mut out, DECRYPTED_MESSAGE_LAYER);
    // Random bytes, at least 15 of them as the spec once asked: they change every ciphertext.
    let mut noise = vec![0u8; 15];
    rand::Rng::fill(&mut rand::thread_rng(), &mut noise[..]);
    put(&mut out, noise);
    put(&mut out, layered.layer);
    put(&mut out, layered.in_seq);
    put(&mut out, layered.out_seq);
    match &layered.message {
        Decrypted::Message { random_id, ttl, text, media, entities, reply_to, silent } => {
            put(&mut out, DECRYPTED_MESSAGE_73);
            let flags: u32 = (u32::from(*silent) << 5) | (u32::from(media.is_some()) << 9) | (u32::from(!entities.is_empty()) << 7) | (u32::from(reply_to.is_some()) << 3);
            put(&mut out, flags);
            put(&mut out, *random_id);
            put(&mut out, *ttl);
            put(&mut out, text.clone());
            if let Some(media) = media {
                write_media(&mut out, media);
            }
            if !entities.is_empty() {
                put(&mut out, entities.clone());
            }
            if let Some(r) = reply_to {
                put(&mut out, *r);
            }
        }
        Decrypted::Service { random_id, action } => {
            put(&mut out, DECRYPTED_MESSAGE_SERVICE);
            put(&mut out, *random_id);
            write_action(&mut out, action);
        }
    }
    out
}

fn get<T: Deserializable>(c: &mut Cursor) -> Result<T> {
    T::deserialize(c).map_err(|e| anyhow::anyhow!("{e}"))
}

fn read_media(c: &mut Cursor) -> Result<Media> {
    let id: u32 = get(c)?;
    Ok(match id {
        MEDIA_EMPTY => return Err(anyhow::anyhow!("empty")),
        MEDIA_PHOTO => {
            let thumb = get(c)?;
            let _tw: i32 = get(c)?;
            let _th: i32 = get(c)?;
            let w = get(c)?;
            let h = get(c)?;
            let size: i32 = get(c)?;
            Media::Photo { thumb, w, h, size: size as i64, key: get(c)?, iv: get(c)?, caption: get(c)? }
        }
        MEDIA_DOCUMENT | MEDIA_DOCUMENT_45 => {
            let thumb = get(c)?;
            let _tw: i32 = get(c)?;
            let _th: i32 = get(c)?;
            let mime = get(c)?;
            let size: i64 = if id == MEDIA_DOCUMENT { get(c)? } else { get::<i32>(c)? as i64 };
            Media::Document { thumb, mime, size, key: get(c)?, iv: get(c)?, attributes: get(c)?, caption: get(c)? }
        }
        MEDIA_EXTERNAL_DOCUMENT => {
            let _id: i64 = get(c)?;
            let _hash: i64 = get(c)?;
            let _date: i32 = get(c)?;
            let mime = get(c)?;
            let _size: i32 = get(c)?;
            let _thumb: tl::enums::PhotoSize = get(c)?;
            let _dc: i32 = get(c)?;
            Media::External { mime, attributes: get(c)? }
        }
        MEDIA_GEO => Media::Geo { lat: get(c)?, long: get(c)? },
        MEDIA_CONTACT => {
            let phone = get(c)?;
            let first = get(c)?;
            let last = get(c)?;
            let _user: i32 = get(c)?;
            Media::Contact { phone, first, last }
        }
        MEDIA_WEBPAGE => Media::WebPage { url: get(c)? },
        MEDIA_VENUE => {
            let lat = get(c)?;
            let long = get(c)?;
            let title = get(c)?;
            let address = get(c)?;
            let _provider: String = get(c)?;
            let _venue: String = get(c)?;
            Media::Venue { lat, long, title, address }
        }
        _ => Media::Unknown,
    })
}

fn read_action(c: &mut Cursor) -> Result<Action> {
    let id: u32 = get(c)?;
    Ok(match id {
        ACTION_SET_TTL => Action::SetTtl(get(c)?),
        ACTION_READ => Action::Read(get(c)?),
        ACTION_DELETE => Action::Delete(get(c)?),
        ACTION_SCREENSHOT => Action::Screenshot(get(c)?),
        ACTION_FLUSH => Action::Flush,
        ACTION_RESEND => Action::Resend { start: get(c)?, end: get(c)? },
        ACTION_NOTIFY_LAYER => Action::NotifyLayer(get(c)?),
        ACTION_TYPING => {
            let _: tl::enums::SendMessageAction = get(c).unwrap_or(tl::enums::SendMessageAction::SendMessageTypingAction);
            Action::Typing
        }
        ACTION_REQUEST_KEY => Action::RequestKey { exchange: get(c)?, g_a: get(c)? },
        ACTION_ACCEPT_KEY => Action::AcceptKey { exchange: get(c)?, g_b: get(c)?, fingerprint: get(c)? },
        ACTION_ABORT_KEY => Action::AbortKey(get(c)?),
        ACTION_COMMIT_KEY => Action::CommitKey { exchange: get(c)?, fingerprint: get(c)? },
        ACTION_NOOP => Action::Noop,
        _ => Action::Unknown,
    })
}

/// Reads a decrypted message. One that is not in a layer (the obsolete layer 8) is read with layer 8 and no sequence
/// numbers, which only a layer notice ever needs.
pub fn decode(bytes: &[u8]) -> Result<Layered> {
    let c = &mut Cursor::from_slice(bytes);
    let first: u32 = get(c)?;
    let (layer, in_seq, out_seq, id) = if first == DECRYPTED_MESSAGE_LAYER {
        let _noise: Vec<u8> = get(c)?;
        let layer = get(c)?;
        let in_seq = get(c)?;
        let out_seq = get(c)?;
        (layer, in_seq, out_seq, get::<u32>(c)?)
    } else {
        (8, -1, -1, first)
    };
    let message = match id {
        DECRYPTED_MESSAGE_SERVICE => Decrypted::Service { random_id: get(c)?, action: read_action(c)? },
        DECRYPTED_MESSAGE_SERVICE_8 => {
            let random_id = get(c)?;
            let _noise: Vec<u8> = get(c)?;
            Decrypted::Service { random_id, action: read_action(c)? }
        }
        DECRYPTED_MESSAGE_73 | DECRYPTED_MESSAGE_45 => {
            let flags: u32 = get(c)?;
            let random_id = get(c)?;
            let ttl = get(c)?;
            let text = get(c)?;
            let media = if flags & (1 << 9) != 0 { read_media(c).ok() } else { None };
            let entities = if flags & (1 << 7) != 0 { get(c).unwrap_or_default() } else { Vec::new() };
            if flags & (1 << 11) != 0 {
                let _via: String = get(c)?;
            }
            let reply_to = if flags & (1 << 3) != 0 { Some(get(c)?) } else { None };
            Decrypted::Message { random_id, ttl, text, media, entities, reply_to, silent: flags & (1 << 5) != 0 }
        }
        DECRYPTED_MESSAGE_17 => {
            let random_id = get(c)?;
            let ttl = get(c)?;
            let text = get(c)?;
            let media = read_media(c).ok();
            Decrypted::Message { random_id, ttl, text, media, entities: Vec::new(), reply_to: None, silent: false }
        }
        other => anyhow::bail!("a secret message of a kind moho does not know ({other:08x})"),
    };
    Ok(Layered { layer, in_seq, out_seq, message })
}

// ---------------------------------------------------------------------------------------------------------------
// What is kept about each chat.

/// Where a chat stands.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Stage {
    /// This side asked; the other has not answered. `a` is this side's exponent.
    Waiting { a: String },
    /// The other side asked; nobody here has answered. `g_a` is theirs.
    Requested { g_a: String },
    /// Keys agreed: messages go both ways.
    Ready,
    /// Ended, by either side.
    Ended,
}

/// A new key being agreed while the old one is still in use (perfect forward secrecy).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rekey {
    pub exchange: i64,
    /// Whether this side started it (and so commits it).
    pub mine: bool,
    /// This side's exponent, hex.
    pub exponent: String,
    /// The new key once worked out, hex.
    pub key: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chat {
    pub id: i32,
    pub access_hash: i64,
    /// The person at the other end.
    pub user_id: i64,
    /// Whether this side started the chat (decides `x` and the parities).
    pub originator: bool,
    pub stage: Stage,
    /// The key, hex (256 bytes).
    #[serde(default)]
    pub key: String,
    /// The first key, hex, for the picture both people compare.
    #[serde(default)]
    pub first_key: String,
    /// The key before the last re-key, hex, still read until the other side has moved over.
    #[serde(default)]
    pub old_key: String,
    #[serde(default)]
    pub rekey: Option<Rekey>,
    /// The other side's layer.
    #[serde(default = "min_layer")]
    pub layer: i32,
    /// How many messages each way have been counted in order.
    #[serde(default)]
    pub in_raw: i32,
    #[serde(default)]
    pub out_raw: i32,
    /// Messages under the current key, and since when.
    #[serde(default)]
    pub uses: u32,
    #[serde(default)]
    pub key_since: i64,
    /// How long a message lasts once read, in seconds (0: for good).
    #[serde(default)]
    pub ttl: i32,
    /// What was sent, by raw counter, for a resend (hex of the layer bytes).
    #[serde(default)]
    pub sent: VecDeque<(i32, String)>,
    /// What arrived early, by its raw counter, waiting for what came before it.
    #[serde(default)]
    pub early: BTreeMap<i32, String>,
    /// The buffer it is shown as.
    #[serde(default)]
    pub buffer: String,
}

fn min_layer() -> i32 {
    MIN_LAYER
}

impl Chat {
    pub fn new(id: i32, access_hash: i64, user_id: i64, originator: bool, stage: Stage) -> Self {
        Chat { id, access_hash, user_id, originator, stage, key: String::new(), first_key: String::new(), old_key: String::new(), rekey: None, layer: MIN_LAYER, in_raw: 0, out_raw: 0, uses: 0, key_since: 0, ttl: 0, sent: VecDeque::new(), early: BTreeMap::new(), buffer: String::new() }
    }

    pub fn key_bytes(&self) -> Vec<u8> {
        hex::decode(&self.key).unwrap_or_default()
    }

    /// `x` for what this side sends, and for what it receives.
    pub fn x_out(&self) -> usize {
        if self.originator { 0 } else { 8 }
    }
    pub fn x_in(&self) -> usize {
        if self.originator { 8 } else { 0 }
    }

    /// Takes a key into use: the old one is kept to read what is still on its way.
    pub fn use_key(&mut self, key: &[u8], now: i64) {
        if !self.key.is_empty() {
            self.old_key = std::mem::take(&mut self.key);
        }
        if self.first_key.is_empty() {
            self.first_key = hex::encode(key);
        }
        self.key = hex::encode(key);
        self.uses = 0;
        self.key_since = now;
    }

    /// Wraps the next message in its layer and numbers it, remembering it for a resend.
    pub fn next_out(&mut self, message: Decrypted) -> Vec<u8> {
        let (in_seq, out_seq) = seq_numbers(self.originator, self.in_raw, self.out_raw);
        let bytes = encode(&Layered { layer: LAYER, in_seq, out_seq, message });
        self.sent.push_back((self.out_raw, hex::encode(&bytes)));
        while self.sent.len() > KEEP_SENT {
            self.sent.pop_front();
        }
        self.out_raw += 1;
        self.uses += 1;
        bytes
    }

    /// Whether it is time this side started a new key.
    pub fn wants_rekey(&self, now: i64) -> bool {
        self.stage == Stage::Ready && self.rekey.is_none() && self.layer >= 20 && self.uses > 0 && (self.uses >= REKEY_AFTER || now - self.key_since >= REKEY_AGE)
    }

    /// The safety picture, hex.
    pub fn visual(&self) -> String {
        let first = hex::decode(&self.first_key).unwrap_or_default();
        let key = hex::decode(&self.key).unwrap_or_default();
        if first.is_empty() {
            return String::new();
        }
        hex::encode(visualization(&first, if self.layer >= 46 { &first } else { &key }))
    }
}

/// An account's secret chats, as one private file, and Telegram's DH config.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Saved {
    #[serde(default)]
    pub chats: BTreeMap<i32, Chat>,
    #[serde(default)]
    pub dh_version: i32,
    #[serde(default)]
    pub dh_g: i32,
    #[serde(default)]
    pub dh_p: String,
    /// Received files, by the name the media cache knows them by: where to fetch and how to decrypt.
    #[serde(default)]
    pub files: BTreeMap<String, SavedFile>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedFile {
    pub id: i64,
    pub access_hash: i64,
    pub dc_id: i32,
    pub size: i64,
    pub key: String,
    pub iv: String,
}

pub fn path_for(user_id: i64) -> PathBuf {
    crate::default_data_dir().join("telegram").join(format!("{user_id}.secret"))
}

pub fn load(path: &Path) -> Saved {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(path: &Path, saved: &Saved) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("secret.tmp");
    {
        let mut file = crate::secure::create_private_file(&tmp)?;
        file.write_all(&serde_json::to_vec(saved)?)?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Vec<u8> {
        (0..256u32).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn what_one_side_encrypts_the_other_decrypts_and_nothing_else_does() {
        let key = key();
        let payload = b"hello, secretly".to_vec();
        // The originator sends with x = 0; the other side reads with the sender's x.
        let data = encrypt(&key, 0, &payload);
        assert_eq!((data.len() - 24) % 16, 0);
        assert_eq!(fingerprint_of(&data), Some(fingerprint(&key)));
        assert_eq!(decrypt(&key, 0, &data).unwrap(), payload);
        assert!(decrypt(&key, 8, &data).is_err(), "read with the other direction's x it does not check out");
        let mut tampered = data.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(decrypt(&key, 0, &tampered).is_err());
        let mut other = key.clone();
        other[0] ^= 1;
        assert!(decrypt(&other, 0, &data).is_err(), "another key is refused by its fingerprint");
        assert_ne!(encrypt(&key, 0, &payload), data, "padding is random each time");
    }

    #[test]
    fn both_sides_reach_the_same_key() {
        // A small prime stands in for Telegram's 2048-bit one: the arithmetic is the same.
        let p = BigUint::from(2_147_483_647u32).to_bytes_be();
        let g = g_bytes(7);
        let (a, b) = (vec![0x12, 0x34, 0x56], vec![0x65, 0x43, 0x21]);
        let g_a = power(&g, &a, &p);
        let g_b = power(&g, &b, &p);
        assert_eq!(power(&g_b, &a, &p), power(&g_a, &b, &p));
        assert_eq!(power(&g, &a, &p).len(), 256, "padded to 256 bytes");
    }

    #[test]
    fn a_public_value_near_either_end_is_refused() {
        let p = {
            let mut p = vec![0xff; 256];
            p[255] = 0xc3;
            p
        };
        assert!(!good_public(&[2], &p));
        let mut middle = vec![0x80; 256];
        middle[0] = 0x7f;
        assert!(good_public(&middle, &p));
        assert!(!good_public(&p, &p));
    }

    #[test]
    fn sequence_numbers_carry_who_started_the_chat() {
        // The originator's own are odd, what it has had from the other side even.
        assert_eq!(seq_numbers(true, 0, 0), (0, 1));
        assert_eq!(seq_numbers(true, 2, 3), (4, 7));
        assert_eq!(seq_numbers(false, 0, 0), (1, 0));
        // What the originator sent, as the other side counts it.
        assert_eq!(arrival(false, 0, 1), Arrival::Next);
        assert_eq!(arrival(false, 1, 1), Arrival::Repeat);
        assert_eq!(arrival(false, 1, 7), Arrival::Early { from: 1, to: 2 });
        assert_eq!(arrival(false, 0, 2), Arrival::Broken);
        assert_eq!(arrival(true, 0, 0), Arrival::Next);
        assert_eq!(resend_range(false, 1, 2), (3, 5));
        assert_eq!(resend_wanted(true, 3, 5), 1..=2);
    }

    #[test]
    fn a_message_and_a_service_message_come_back_as_they_went() {
        let bold = tl::enums::MessageEntity::Bold(tl::types::MessageEntityBold { offset: 0, length: 2 });
        let message = Layered {
            layer: LAYER,
            in_seq: 4,
            out_seq: 7,
            message: Decrypted::Message { random_id: 99, ttl: 5, text: "hi there".into(), media: None, entities: vec![bold], reply_to: Some(42), silent: false },
        };
        assert_eq!(decode(&encode(&message)).unwrap(), message);
        let doc = Layered {
            layer: LAYER,
            in_seq: 0,
            out_seq: 1,
            message: Decrypted::Message {
                random_id: 7,
                ttl: 0,
                text: String::new(),
                media: Some(Media::Document {
                    thumb: vec![],
                    mime: "text/plain".into(),
                    size: 3,
                    key: vec![1; 32],
                    iv: vec![2; 32],
                    attributes: vec![tl::enums::DocumentAttribute::Filename(tl::types::DocumentAttributeFilename { file_name: "a.txt".into() })],
                    caption: "a file".into(),
                }),
                entities: vec![],
                reply_to: None,
                silent: true,
            },
        };
        assert_eq!(decode(&encode(&doc)).unwrap(), doc);
        for action in [
            Action::SetTtl(30),
            Action::Read(vec![1, 2]),
            Action::Delete(vec![3]),
            Action::Resend { start: 1, end: 5 },
            Action::NotifyLayer(LAYER),
            Action::Typing,
            Action::RequestKey { exchange: 5, g_a: vec![9; 256] },
            Action::AcceptKey { exchange: 5, g_b: vec![8; 256], fingerprint: -3 },
            Action::AbortKey(5),
            Action::CommitKey { exchange: 5, fingerprint: 11 },
            Action::Noop,
        ] {
            let service = Layered { layer: LAYER, in_seq: 0, out_seq: 1, message: Decrypted::Service { random_id: 1, action: action.clone() } };
            assert_eq!(decode(&encode(&service)).unwrap(), service, "{action:?}");
        }
    }

    #[test]
    fn a_file_goes_and_comes_back_whole() {
        let (k, iv) = file_key();
        let bytes = b"seventeen bytes!!".to_vec();
        let sealed = encrypt_file(&bytes, &k, &iv);
        assert_eq!(sealed.len(), 32);
        assert_eq!(decrypt_file(sealed, &k, &iv, bytes.len()).unwrap(), bytes);
        assert_eq!(file_fingerprint(&k, &iv), file_fingerprint(&k, &iv));
    }

    #[test]
    fn a_chat_numbers_what_it_sends_and_asks_for_a_new_key_in_time() {
        let mut chat = Chat::new(1, 2, 3, true, Stage::Ready);
        chat.layer = LAYER;
        chat.use_key(&key(), 1000);
        let first = decode(&chat.next_out(Decrypted::Service { random_id: 1, action: Action::Noop })).unwrap();
        let second = decode(&chat.next_out(Decrypted::Service { random_id: 2, action: Action::Noop })).unwrap();
        assert_eq!((first.out_seq, second.out_seq), (1, 3));
        assert_eq!(chat.sent.len(), 2);
        assert!(!chat.wants_rekey(1001));
        chat.uses = REKEY_AFTER;
        assert!(chat.wants_rekey(1001));
        chat.uses = 1;
        assert!(chat.wants_rekey(1000 + REKEY_AGE));
        chat.use_key(&[5u8; 256], 2000);
        assert!(!chat.old_key.is_empty() && chat.uses == 0);
        assert_eq!(chat.first_key, hex::encode(key()), "the picture stays the first key's");
    }

    #[test]
    fn the_saved_file_is_private_and_comes_back() {
        let dir = std::env::temp_dir().join(format!("moho-secret-{}", std::process::id()));
        let path = dir.join("1.secret");
        let mut saved = Saved::default();
        saved.chats.insert(5, Chat::new(5, 6, 7, false, Stage::Requested { g_a: "00".into() }));
        save(&path, &saved).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(load(&path).chats[&5].stage, Stage::Requested { g_a: "00".into() });
        let _ = std::fs::remove_dir_all(dir);
    }
}
