//! OMEMO (XEP-0384), in both namespaces: the older `eu.siacs.conversations.axolotl`, which is the Signal protocol carried in XMPP
//! and the one every client speaks, and OMEMO 2 (`urn:xmpp:omemo:2`, `v2.rs`), a different wire form under the same identity. A
//! device that publishes OMEMO 2 is written to in it; the rest in the older namespace, in the same message when a person has both.
//!
//! A message is encrypted once, under a random key, with AES-128-GCM; that key (and the GCM tag) is then encrypted separately
//! to every device of every person it is for - each of the recipient's devices and each of this account's other devices - with
//! the Signal session each pair of devices has. A device announces itself by being in its owner's device list (PEP) and having a
//! bundle of public keys published, from which anybody can start a session without it being online.
//!
//! This device's keys, its sessions and who is trusted live in one private file per account (`store.rs`).

pub mod curve;
pub mod ratchet;
pub mod store;
pub mod v2;

use super::session::Handle;
use super::xml::Attr;
use super::ns;
use aes_gcm::aead::consts::{U12, U16};
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::AesGcm;
use base64::Engine;
use ratchet::{Bundle, Error as SessionError, Session};
use std::sync::Mutex;
use store::{Seen, Store};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;

pub const NS: &str = "eu.siacs.conversations.axolotl";
pub const DEVICELIST_NODE: &str = "eu.siacs.conversations.axolotl.devicelist";
pub const BUNDLES_NODE_PREFIX: &str = "eu.siacs.conversations.axolotl.bundles:";
/// What a client that cannot read an OMEMO message is shown instead (the words every client uses).
pub const FALLBACK_BODY: &str = "I sent you an OMEMO encrypted message but your client doesn't seem to support that. Find more information on https://conversations.im/omemo";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyElement {
    pub rid: u32,
    pub prekey: bool,
    pub data: Vec<u8>,
}

/// An `<encrypted/>`: who sent it, the keys for each device, and the payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub sid: u32,
    pub keys: Vec<KeyElement>,
    pub iv: Vec<u8>,
    pub payload: Option<Vec<u8>>,
}

pub fn parse_envelope(e: &Element) -> Option<Envelope> {
    let header = e.children().find(|c| c.name() == "header")?;
    let sid = header.attr("sid")?.parse().ok()?;
    let keys = header
        .children()
        .filter(|c| c.name() == "key")
        .filter_map(|k| Some(KeyElement { rid: k.attr("rid")?.parse().ok()?, prekey: matches!(k.attr("prekey"), Some("true" | "1")), data: b64().decode(k.text().trim()).ok()? }))
        .collect();
    let iv = b64().decode(header.children().find(|c| c.name() == "iv")?.text().trim()).ok()?;
    let payload = e.children().find(|c| c.name() == "payload").and_then(|p| b64().decode(p.text().trim()).ok());
    Some(Envelope { sid, keys, iv, payload })
}

pub fn parse_devicelist(list: &Element) -> Vec<u32> {
    list.children().filter(|c| c.name() == "device").filter_map(|d| d.attr("id")?.parse().ok()).collect()
}

/// A device's bundle as a [`Bundle`], choosing one of its one-time pre-keys.
pub fn parse_bundle(b: &Element) -> Option<Bundle> {
    use rand::seq::SliceRandom;
    let signed = b.children().find(|c| c.name() == "signedPreKeyPublic")?;
    let signature = b.children().find(|c| c.name() == "signedPreKeySignature")?;
    let identity = b.children().find(|c| c.name() == "identityKey")?;
    let pre_keys: Vec<(u32, [u8; 32])> = b.children().find(|c| c.name() == "prekeys").map(|p| p.children().filter(|k| k.name() == "preKeyPublic").filter_map(|k| Some((k.attr("preKeyId")?.parse().ok()?, curve::parse_public(&b64().decode(k.text().trim()).ok()?)?))).collect()).unwrap_or_default();
    Some(Bundle {
        identity: curve::parse_public(&b64().decode(identity.text().trim()).ok()?)?,
        signed_pre_key_id: signed.attr("signedPreKeyId")?.parse().ok()?,
        signed_pre_key: curve::parse_public(&b64().decode(signed.text().trim()).ok()?)?,
        signature: b64().decode(signature.text().trim()).ok()?.try_into().ok()?,
        pre_key: pre_keys.choose(&mut rand::thread_rng()).copied(),
    })
}

/// What decrypting a message told us beyond its text.
pub struct Decrypted {
    pub text: Option<String>,
    /// OMEMO 2: everything the message held (a body and whatever else was encrypted with it).
    pub content: Vec<Element>,
    /// The sending device should be sent an empty message: it started a session that this one accepted (the empty message
    /// completes it), or it has sent many without hearing back. The device, and whether it is OMEMO 2.
    pub answer: Option<(u32, bool)>,
    /// A pre-key was used up or a device is new: the bundle should be published again.
    pub republish: bool,
    pub seen: Seen,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("this message has no key for this device")]
    NotForUs,
    #[error("the sender's address is not known")]
    NoRealJid,
    #[error("there is no session with the device that sent this")]
    NoSession,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("the message's key is not the size it should be")]
    BadKey,
    #[error("the message cannot be decrypted")]
    Payload,
    #[error(transparent)]
    V2(#[from] v2::Error),
    #[error("the message says it is from somebody other than who sent it")]
    WrongSender,
}

pub struct Omemo {
    pub store: Mutex<Store>,
}

impl Omemo {
    pub fn open(path: Option<std::path::PathBuf>) -> Omemo {
        Omemo { store: Mutex::new(Store::open(path)) }
    }

    pub fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn device_id(&self) -> u32 {
        self.store().state.device_id
    }

    pub fn devicelist_element(ids: &[u32]) -> Element {
        let mut list = Element::builder("list", NS).build();
        for id in ids {
            list.append_child(Element::builder("device", NS).at("id", id.to_string()).build());
        }
        list
    }

    /// This device's bundle, for others to start a session from.
    pub fn bundle_element(&self) -> Element {
        let s = self.store();
        let identity = s.identity();
        let signed = s.state.signed_pre_key.as_ref().expect("a device has a signed pre-key");
        let signed_pair = curve::KeyPair::from(&signed.pair);
        let signature = curve::unhex(&signed.signature).unwrap_or_default();
        let mut bundle = Element::builder("bundle", NS).build();
        bundle.append_child(Element::builder("signedPreKeyPublic", NS).at("signedPreKeyId", signed.id.to_string()).append(b64().encode(signed_pair.public_serialized())).build());
        bundle.append_child(Element::builder("signedPreKeySignature", NS).append(b64().encode(signature)).build());
        bundle.append_child(Element::builder("identityKey", NS).append(b64().encode(identity.public_serialized())).build());
        let mut pre = Element::builder("prekeys", NS).build();
        for (id, pair) in &s.state.pre_keys {
            pre.append_child(Element::builder("preKeyPublic", NS).at("preKeyId", id.to_string()).append(b64().encode(curve::serialize_public(&pair.public))).build());
        }
        bundle.append_child(pre);
        bundle
    }

    /// Encrypts `plaintext` for the given devices (each must have a session). The payload is encrypted once.
    pub fn encrypt(&self, targets: &[(String, u32)], plaintext: &[u8]) -> Result<Element, Error> {
        use rand::RngCore;
        let mut key = [0u8; 16];
        let mut iv = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut key);
        rand::thread_rng().fill_bytes(&mut iv);
        let cipher = AesGcm::<aes::Aes128, U12>::new(&key.into());
        let mut payload = plaintext.to_vec();
        let tag = cipher.encrypt_in_place_detached(&iv.into(), b"", &mut payload).map_err(|_| Error::Payload)?;
        let mut blob = key.to_vec();
        blob.extend_from_slice(&tag);
        let mut store = self.store();
        let mut header = Element::builder("header", NS).at("sid", store.state.device_id.to_string()).build();
        for (jid, device) in targets {
            let Some(session) = store.state.sessions.get_mut(&store::key(jid, *device)) else { continue };
            let prekey = session.pending.is_some();
            let wrapped = session.encrypt(&blob);
            let mut k = Element::builder("key", NS).at("rid", device.to_string());
            if prekey {
                k = k.at("prekey", "true");
            }
            header.append_child(k.append(b64().encode(wrapped)).build());
        }
        store.save();
        header.append_child(Element::builder("iv", NS).append(b64().encode(iv)).build());
        Ok(Element::builder("encrypted", NS).append(header).append(Element::builder("payload", NS).append(b64().encode(payload)).build()).build())
    }

    /// Reads a message from `from` (a bare address).
    pub fn decrypt(&self, from: &str, env: &Envelope) -> Result<Decrypted, Error> {
        let mut store = self.store();
        let ours = store.state.device_id;
        let key = env.keys.iter().find(|k| k.rid == ours).ok_or(Error::NotForUs)?;
        let session_key = store::key(from, env.sid);
        let mut republish = false;
        let mut seen = Seen::Known;
        let answer = key.prekey.then_some((env.sid, false));
        let blob = if key.prekey {
            let message = ratchet::parse_pre_key(&key.data)?;
            let identity = curve::parse_public(message.identity_key.as_deref().unwrap_or_default()).ok_or(SessionError::Malformed)?;
            seen = store.see_identity(from, env.sid, &identity);
            let signed = store.state.signed_pre_key.clone().ok_or(SessionError::UnknownPreKey)?;
            if message.signed_pre_key_id != Some(signed.id) {
                return Err(SessionError::UnknownPreKey.into());
            }
            let pre_key_id = message.pre_key_id;
            let one_time = match pre_key_id {
                Some(id) => Some(curve::KeyPair::from(store.state.pre_keys.get(&id).ok_or(SessionError::UnknownPreKey)?)),
                None => None,
            };
            let own_identity = store.identity();
            let mut session = Session::accept(&own_identity, &curve::KeyPair::from(&signed.pair), one_time.as_ref(), ours, &message)?;
            let plain = session.decrypt(message.message.as_deref().ok_or(SessionError::Malformed)?)?;
            store.state.sessions.insert(session_key, session);
            if let Some(id) = pre_key_id {
                store.state.pre_keys.remove(&id);
                republish = store.top_up();
            }
            plain
        } else {
            let session = store.state.sessions.get_mut(&session_key).ok_or(Error::NoSession)?;
            session.decrypt(&key.data)?
        };
        store.save();
        if blob.len() < 32 {
            return Err(Error::BadKey);
        }
        let Some(payload) = &env.payload else { return Ok(Decrypted { text: None, content: Vec::new(), answer, republish, seen }) };
        let (k, tag) = blob.split_at(16);
        let mut data = payload.clone();
        let ok = match env.iv.len() {
            12 => AesGcm::<aes::Aes128, U12>::new(k.into()).decrypt_in_place_detached(env.iv.as_slice().into(), b"", &mut data, tag[..16].into()).is_ok(),
            16 => AesGcm::<aes::Aes128, U16>::new(k.into()).decrypt_in_place_detached(env.iv.as_slice().into(), b"", &mut data, tag[..16].into()).is_ok(),
            _ => false,
        };
        if !ok {
            return Err(Error::Payload);
        }
        Ok(Decrypted { text: Some(String::from_utf8_lossy(&data).to_string()), content: Vec::new(), answer, republish, seen })
    }

    /// This device's OMEMO 2 bundle.
    pub fn bundle_element_v2(&self) -> Element {
        let s = self.store();
        let identity = s.identity();
        let signed = s.state.v2.signed_pre_key.as_ref().expect("a device has an OMEMO 2 signed pre-key");
        let signature = curve::unhex(&signed.signature).unwrap_or_default();
        v2::bundle_element(&v2::ed_public(&identity.private), signed.id, &signed.pair.public, &signature, s.state.v2.pre_keys.iter().map(|(id, p)| (*id, p.public)))
    }

    /// Encrypts a message's words to the given devices: an OMEMO 2 element for those that speak it, an element in the older
    /// namespace for the rest. `own` is this account's bare address, which an OMEMO 2 message says it is from.
    pub fn encrypt_all(&self, targets: &[Target], text: &str, own: &str) -> Result<Vec<Element>, Error> {
        let mut out = Vec::new();
        if targets.iter().any(|t| t.v2) {
            let sce = v2::sce_envelope(vec![v2::body_element(text)], own);
            let (cipher, material) = v2::encrypt_payload(&sce);
            let mut store = self.store();
            let mut keys = Vec::new();
            for t in targets.iter().filter(|t| t.v2) {
                if let Some(session) = store.state.v2.sessions.get_mut(&store::key(&t.jid, t.device)) {
                    let (data, kex) = session.encrypt(&material);
                    keys.push(v2::Key { jid: t.jid.clone(), rid: t.device, kex, data });
                }
            }
            store.save();
            out.push(v2::envelope_element(store.state.device_id, &keys, Some(&cipher)));
        }
        let old: Vec<(String, u32)> = targets.iter().filter(|t| !t.v2).map(|t| (t.jid.clone(), t.device)).collect();
        if !old.is_empty() {
            out.push(self.encrypt(&old, text.as_bytes())?);
        }
        Ok(out)
    }

    /// An empty message to one device (key transport): no payload, only a key, which completes a session, moves a stale one on,
    /// or starts one. `None` without a session with the device.
    pub fn empty_message(&self, jid: &str, device: u32, v2: bool) -> Option<Element> {
        use rand::RngCore;
        let mut store = self.store();
        let ours = store.state.device_id;
        let k = store::key(jid, device);
        let element = if v2 {
            let (data, kex) = store.state.v2.sessions.get_mut(&k)?.empty();
            v2::envelope_element(ours, &[v2::Key { jid: jid.to_string(), rid: device, kex, data }], None)
        } else {
            let session = store.state.sessions.get_mut(&k)?;
            let prekey = session.pending.is_some();
            let mut blob = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut blob);
            let mut key = Element::builder("key", NS).at("rid", device.to_string());
            if prekey {
                key = key.at("prekey", "true");
            }
            let key = key.append(b64().encode(session.encrypt(&blob))).build();
            let mut iv = [0u8; 12];
            rand::thread_rng().fill_bytes(&mut iv);
            let header = Element::builder("header", NS).at("sid", ours.to_string()).append(key).append(Element::builder("iv", NS).append(b64().encode(iv)).build()).build();
            Element::builder("encrypted", NS).append(header).build()
        };
        store.save();
        Some(element)
    }

    /// Forgets every session with a person's devices, in both namespaces (a broken session is replaced by hand, never on its own:
    /// XEP-0384). Returns how many there were.
    pub fn reset_sessions(&self, jid: &str) -> usize {
        let mut store = self.store();
        let prefix = format!("{jid}/");
        let before = store.state.sessions.len() + store.state.v2.sessions.len();
        store.state.sessions.retain(|k, _| !k.starts_with(&prefix));
        store.state.v2.sessions.retain(|k, _| !k.starts_with(&prefix));
        let gone = before - store.state.sessions.len() - store.state.v2.sessions.len();
        store.save();
        gone
    }

    /// Reads an OMEMO 2 message from `from` (a bare address) to `own` (this account's).
    pub fn decrypt_v2(&self, from: &str, own: &str, env: &v2::Envelope) -> Result<Decrypted, Error> {
        let mut store = self.store();
        let ours = store.state.device_id;
        let key = env.keys.iter().find(|k| k.rid == ours && (k.jid.is_empty() || k.jid.eq_ignore_ascii_case(own))).ok_or(Error::NotForUs)?;
        let session_key = store::key(from, env.sid);
        let mut republish = false;
        let mut seen = Seen::Known;
        let mut answer = None;
        let material = if key.kex {
            let (kex, inner) = v2::parse_key_exchange(&key.data)?;
            // The same exchange again (the sender has had no answer yet): the session it started reads it.
            match store.state.v2.sessions.get_mut(&session_key).filter(|s| s.started_by == Some(kex.ek)) {
                Some(session) => session.decrypt(&inner)?,
                None => {
                    let identity = v2::ed_to_montgomery(&kex.ik).ok_or(v2::Error::Malformed)?;
                    seen = store.see_identity(from, env.sid, &identity);
                    let signed = store.state.v2.signed_pre_key.clone().filter(|s| s.id == kex.spk_id).ok_or(v2::Error::UnknownPreKey)?;
                    let pk = curve::KeyPair::from(store.state.v2.pre_keys.get(&kex.pk_id).ok_or(v2::Error::UnknownPreKey)?);
                    let (session, plain) = v2::Session::accept(&store.identity(), &curve::KeyPair::from(&signed.pair), &pk, &kex, &inner)?;
                    store.state.v2.sessions.insert(session_key.clone(), session);
                    store.state.v2.pre_keys.remove(&kex.pk_id);
                    answer = Some((env.sid, true));
                    republish = store.top_up_v2();
                    plain
                }
            }
        } else {
            let session = store.state.v2.sessions.get_mut(&session_key).ok_or(Error::NoSession)?;
            session.decrypt(&key.data)?
        };
        if store.state.v2.sessions.get(&session_key).is_some_and(|s| s.heartbeat_due) {
            answer = Some((env.sid, true));
        }
        store.save();
        drop(store);
        let Some(payload) = &env.payload else { return Ok(Decrypted { text: None, content: Vec::new(), answer, republish, seen }) };
        let opened = v2::open_sce(&v2::decrypt_payload(payload, &material)?)?;
        if let Some(said) = &opened.from {
            let said = said.split('/').next().unwrap_or_default();
            if !said.eq_ignore_ascii_case(from) {
                return Err(Error::WrongSender);
            }
        }
        Ok(Decrypted { text: v2::body_of(&opened.content), content: opened.content, answer, republish, seen })
    }
}

// --- encrypted files (XEP-0454, the older `aesgcm://` form) -------------------------------------------------------------

/// Encrypts a file for upload: AES-256-GCM under a fresh key, the tag appended. Returns the bytes to upload, and the key and IV
/// that go in the link's fragment (and nowhere else).
/// An encrypted file: the bytes to upload, the key and the IV.
pub type EncryptedFile = (Vec<u8>, [u8; 32], [u8; 12]);

pub fn encrypt_file(plain: &[u8]) -> Result<EncryptedFile, Error> {
    use rand::RngCore;
    let (mut key, mut iv) = ([0u8; 32], [0u8; 12]);
    rand::thread_rng().fill_bytes(&mut key);
    rand::thread_rng().fill_bytes(&mut iv);
    let mut data = plain.to_vec();
    let tag = AesGcm::<aes::Aes256, U12>::new(&key.into()).encrypt_in_place_detached(&iv.into(), b"", &mut data).map_err(|_| Error::Payload)?;
    data.extend_from_slice(&tag);
    Ok((data, key, iv))
}

/// The link an encrypted file is shared by: the upload's address with `aesgcm` as its scheme and the IV and key as its fragment.
pub fn aesgcm_url(https_url: &str, key: &[u8; 32], iv: &[u8; 12]) -> String {
    format!("aesgcm://{}#{}{}", https_url.trim_start_matches("https://"), curve::hex(iv), curve::hex(key))
}

/// Takes an `aesgcm://` link apart: the https address to fetch from, the IV and the key. Both IV sizes are accepted (12 bytes, and
/// the 16 some older clients used).
pub fn parse_aesgcm(url: &str) -> Option<(String, Vec<u8>, [u8; 32])> {
    let rest = url.strip_prefix("aesgcm://")?;
    let (path, fragment) = rest.split_once('#')?;
    let bytes = curve::unhex(fragment)?;
    let iv_len = match bytes.len() {
        44 => 12,
        48 => 16,
        _ => return None,
    };
    let key: [u8; 32] = bytes[iv_len..].try_into().ok()?;
    if path.is_empty() || path.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some((format!("https://{path}"), bytes[..iv_len].to_vec(), key))
}

/// Decrypts what was fetched from an `aesgcm://` link.
pub fn decrypt_file(fetched: &[u8], iv: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, Error> {
    if fetched.len() < 16 {
        return Err(Error::Payload);
    }
    let (body, tag) = fetched.split_at(fetched.len() - 16);
    let mut data = body.to_vec();
    let ok = match iv.len() {
        12 => AesGcm::<aes::Aes256, U12>::new(key.into()).decrypt_in_place_detached(iv.into(), b"", &mut data, tag.into()).is_ok(),
        16 => AesGcm::<aes::Aes256, U16>::new(key.into()).decrypt_in_place_detached(iv.into(), b"", &mut data, tag.into()).is_ok(),
        _ => false,
    };
    if ok { Ok(data) } else { Err(Error::Payload) }
}

// --- talking to the server ---------------------------------------------------------------------------------------------

fn data_form(form_type: &str, fields: &[(&str, &str)]) -> Element {
    let mut x = Element::builder("x", ns::DATA).at("type", "submit").build();
    for (var, value) in std::iter::once(&("FORM_TYPE", form_type)).chain(fields) {
        let mut f = Element::builder("field", ns::DATA).at("var", *var).build();
        f.append_child(Element::builder("value", ns::DATA).append(*value).build());
        x.append_child(f);
    }
    x
}

/// Publishes an item with the options the node needs. A node made earlier with other options refuses (precondition-not-met, a
/// conflict): it is configured and the item published again. A server that does not take an option it can live without
/// (`max_items` = `max`) gets the publish without it.
async fn publish_item(handle: &Handle, node: &str, id: &str, item: Element, options: &[(&str, &str)]) -> Result<(), String> {
    let request = |fields: &[(&str, &str)]| {
        let item = Element::builder("item", ns::PUBSUB).at("id", id).append(item.clone()).build();
        let publish = Element::builder("publish", ns::PUBSUB).at("node", node).append(item).build();
        let form = data_form("http://jabber.org/protocol/pubsub#publish-options", fields);
        Element::builder("pubsub", ns::PUBSUB).append(publish).append(Element::builder("publish-options", ns::PUBSUB).append(form).build()).build()
    };
    let all: Vec<(&str, &str)> = [("pubsub#persist_items", "true"), ("pubsub#access_model", "open")].into_iter().chain(options.iter().copied()).collect();
    let Err(e) = handle.request(None, true, request(&all)).await else { return Ok(()) };
    if e.condition == "conflict" {
        let configure = Element::builder("configure", super::nodes::OWNER).at("node", node).append(data_form("http://jabber.org/protocol/pubsub#node_config", &all)).build();
        let _ = handle.request(None, true, Element::builder("pubsub", super::nodes::OWNER).append(configure).build()).await;
        if handle.request(None, true, request(&all)).await.is_ok() {
            return Ok(());
        }
    }
    let without: Vec<(&str, &str)> = all.into_iter().filter(|(k, _)| *k != "pubsub#max_items").collect();
    handle.request(None, true, request(&without)).await.map(|_| ()).map_err(|e| e.words())
}

async fn publish(handle: &Handle, node: &str, item: Element) -> Result<(), String> {
    publish_item(handle, node, "current", item, &[]).await
}

async fn items(handle: &Handle, to: Option<Jid>, node: &str) -> Option<Element> {
    item(handle, to, node, None).await
}

/// The payload of an item of a node: the one with `id`, or the first.
async fn item(handle: &Handle, to: Option<Jid>, node: &str, id: Option<&str>) -> Option<Element> {
    let mut items = Element::builder("items", ns::PUBSUB).at("node", node).build();
    if let Some(id) = id {
        items.append_child(Element::builder("item", ns::PUBSUB).at("id", id).build());
    }
    let q = Element::builder("pubsub", ns::PUBSUB).append(items).build();
    let answer = handle.request(to, false, q).await.ok()??;
    answer.children().find(|c| c.name() == "items").and_then(|i| i.children().find(|c| c.name() == "item" && id.is_none_or(|id| c.attr("id") == Some(id))).and_then(|item| item.children().next().cloned()))
}

/// Makes this device known under both namespaces: in its owner's device lists, with its bundles published. Done after signing
/// in and when keys were used up.
pub async fn publish_device(handle: &Handle, omemo: &Omemo) {
    let ours = omemo.device_id();
    let mut ids = match items(handle, None, DEVICELIST_NODE).await {
        Some(list) => parse_devicelist(&list),
        None => Vec::new(),
    };
    if !ids.contains(&ours) {
        ids.push(ours);
        if let Err(e) = publish(handle, DEVICELIST_NODE, Omemo::devicelist_element(&ids)).await {
            tracing::warn!("xmpp[{}]: OMEMO device list: {e}", handle.account_id);
            return;
        }
    }
    omemo.store().state.devices.insert(handle.own().to_string(), ids);
    if let Err(e) = publish(handle, &format!("{BUNDLES_NODE_PREFIX}{ours}"), omemo.bundle_element()).await {
        tracing::warn!("xmpp[{}]: OMEMO bundle: {e}", handle.account_id);
    }
    // OMEMO 2: the bundle first, so a device in the list always has one.
    if let Err(e) = publish_item(handle, v2::BUNDLES_NODE, &ours.to_string(), omemo.bundle_element_v2(), &[("pubsub#max_items", "max")]).await {
        tracing::warn!("xmpp[{}]: OMEMO 2 bundle: {e}", handle.account_id);
        return;
    }
    let mut ids2 = match items(handle, None, v2::DEVICES_NODE).await {
        Some(list) => v2::parse_devices(&list),
        None => Vec::new(),
    };
    if !ids2.contains(&ours) {
        ids2.push(ours);
        if let Err(e) = publish(handle, v2::DEVICES_NODE, v2::devices_element(&ids2)).await {
            tracing::warn!("xmpp[{}]: OMEMO 2 device list: {e}", handle.account_id);
            return;
        }
    }
    omemo.store().state.v2.devices.insert(handle.own().to_string(), ids2);
}

/// The devices a person has under the older namespace: what is known, or what their device list says now.
async fn devices_old(handle: &Handle, omemo: &Omemo, jid: &BareJid, refresh: bool) -> Vec<u32> {
    if !refresh {
        if let Some(known) = omemo.store().state.devices.get(jid.as_str()) {
            return known.clone();
        }
    }
    let list = match items(handle, Some(Jid::from(jid.clone())), DEVICELIST_NODE).await {
        Some(l) => parse_devicelist(&l),
        None => Vec::new(),
    };
    omemo.store().state.devices.insert(jid.to_string(), list.clone());
    list
}

/// The devices a person lists under OMEMO 2.
async fn devices_new(handle: &Handle, omemo: &Omemo, jid: &BareJid, refresh: bool) -> Vec<u32> {
    if !refresh {
        if let Some(known) = omemo.store().state.v2.devices.get(jid.as_str()) {
            return known.clone();
        }
    }
    let list = match items(handle, Some(Jid::from(jid.clone())), v2::DEVICES_NODE).await {
        Some(l) => v2::parse_devices(&l),
        None => Vec::new(),
    };
    omemo.store().state.v2.devices.insert(jid.to_string(), list.clone());
    list
}

/// Every device a person has, under either namespace.
pub async fn devices_of(handle: &Handle, omemo: &Omemo, jid: &BareJid, refresh: bool) -> Vec<u32> {
    let mut all = devices_new(handle, omemo, jid, refresh).await;
    for d in devices_old(handle, omemo, jid, refresh).await {
        if !all.contains(&d) {
            all.push(d);
        }
    }
    all
}

/// Whether a device of a person speaks OMEMO 2 (it is in their OMEMO 2 device list).
pub fn speaks_v2(omemo: &Omemo, jid: &str, device: u32) -> bool {
    omemo.store().state.v2.devices.get(jid).is_some_and(|l| l.contains(&device))
}

/// A device a message can be encrypted to, and in which namespace.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub jid: String,
    pub device: u32,
    pub v2: bool,
}

/// Starts sessions with the devices of `jid` that have none, and returns the devices a message to them can be encrypted to: in
/// OMEMO 2 where the device publishes it, in the older namespace otherwise.
pub async fn usable_devices(handle: &Handle, omemo: &Omemo, jid: &BareJid, skip_own_device: bool) -> Vec<Target> {
    let ours = omemo.device_id();
    let mut out = Vec::new();
    let own_list = *jid == handle.own();
    for device in devices_new(handle, omemo, jid, true).await {
        if own_list && skip_own_device && device == ours {
            continue;
        }
        let key = store::key(jid.as_str(), device);
        if !omemo.store().state.v2.sessions.contains_key(&key) {
            let Some(bundle) = item(handle, Some(Jid::from(jid.clone())), v2::BUNDLES_NODE, Some(&device.to_string())).await.as_ref().and_then(v2::parse_bundle) else { continue };
            let Some(identity) = v2::ed_to_montgomery(&bundle.ik) else { continue };
            let mut store = omemo.store();
            let seen = store.see_identity(jid.as_str(), device, &identity);
            if seen == Seen::Changed || !store.trust_of(jid.as_str(), device).encrypts() {
                continue;
            }
            match v2::Session::initiate(&store.identity(), &bundle) {
                Ok(session) => {
                    store.state.v2.sessions.insert(key.clone(), session);
                    store.save();
                }
                Err(e) => {
                    tracing::debug!("xmpp: no OMEMO 2 session with {jid}/{device}: {e}");
                    continue;
                }
            }
        }
        if omemo.store().trust_of(jid.as_str(), device).encrypts() {
            out.push(Target { jid: jid.to_string(), device, v2: true });
        }
    }
    for device in devices_old(handle, omemo, jid, true).await {
        if (own_list && skip_own_device && device == ours) || out.iter().any(|t| t.device == device) {
            continue;
        }
        let key = store::key(jid.as_str(), device);
        if !omemo.store().state.sessions.contains_key(&key) {
            let Some(bundle) = items(handle, Some(Jid::from(jid.clone())), &format!("{BUNDLES_NODE_PREFIX}{device}")).await.as_ref().and_then(parse_bundle) else { continue };
            let mut store = omemo.store();
            let seen = store.see_identity(jid.as_str(), device, &bundle.identity);
            if seen == Seen::Changed || !store.trust_of(jid.as_str(), device).encrypts() {
                continue;
            }
            let identity = store.identity();
            let registration = store.state.device_id;
            match Session::initiate(&identity, registration, &bundle) {
                Ok(session) => {
                    store.state.sessions.insert(key.clone(), session);
                    store.save();
                }
                Err(e) => {
                    tracing::debug!("xmpp: no session with {jid}/{device}: {e}");
                    continue;
                }
            }
        }
        if omemo.store().trust_of(jid.as_str(), device).encrypts() {
            out.push(Target { jid: jid.to_string(), device, v2: false });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use store::Trust;

    fn two() -> (Omemo, Omemo) {
        (Omemo::open(None), Omemo::open(None))
    }

    /// Bob's bundle as Alice would fetch it, and a session started from it.
    fn introduce(alice: &Omemo, bob: &Omemo, alice_jid: &str, bob_jid: &str) {
        let bundle = parse_bundle(&bob.bundle_element()).expect("a bundle");
        let mut a = alice.store();
        a.see_identity(bob_jid, bob.device_id(), &bundle.identity);
        let (identity, registration) = (a.identity(), a.state.device_id);
        let session = Session::initiate(&identity, registration, &bundle).unwrap();
        a.state.sessions.insert(store::key(bob_jid, bob.device_id()), session);
        let _ = alice_jid;
    }

    #[test]
    fn a_message_goes_from_one_device_to_another_through_the_xml_that_carries_it() {
        let (alice, bob) = two();
        introduce(&alice, &bob, "alice@example.org", "bob@example.org");
        let sent = alice.encrypt(&[("bob@example.org".into(), bob.device_id())], "a secret".as_bytes()).unwrap();
        // As it travels: serialised and read again.
        let env = parse_envelope(&Element::from_str(&String::from(&sent)).unwrap()).unwrap();
        assert!(env.keys[0].prekey, "the first message to a device says how the session began");
        let got = bob.decrypt("alice@example.org", &env).unwrap();
        assert_eq!(got.text.as_deref(), Some("a secret"));
        assert_eq!(got.seen, Seen::New);
        assert!(got.republish, "the pre-key that was used is replaced");
        assert_eq!(bob.store().trust_of("alice@example.org", alice.device_id()), Trust::Trusted);
    }

    #[test]
    fn an_answer_is_an_ordinary_message_and_the_conversation_goes_on() {
        let (alice, bob) = two();
        introduce(&alice, &bob, "alice@example.org", "bob@example.org");
        let first = alice.encrypt(&[("bob@example.org".into(), bob.device_id())], b"one").unwrap();
        bob.decrypt("alice@example.org", &parse_envelope(&first).unwrap()).unwrap();
        let reply = bob.encrypt(&[("alice@example.org".into(), alice.device_id())], b"two").unwrap();
        let env = parse_envelope(&reply).unwrap();
        assert!(!env.keys[0].prekey);
        assert_eq!(alice.decrypt("bob@example.org", &env).unwrap().text.as_deref(), Some("two"));
        let third = alice.encrypt(&[("bob@example.org".into(), bob.device_id())], b"three").unwrap();
        assert!(!parse_envelope(&third).unwrap().keys[0].prekey, "once he answered, no more pre-key messages");
        assert_eq!(bob.decrypt("alice@example.org", &parse_envelope(&third).unwrap()).unwrap().text.as_deref(), Some("three"));
    }

    #[test]
    fn a_message_for_other_devices_only_is_not_for_this_one() {
        let (alice, bob) = two();
        let carol = Omemo::open(None);
        introduce(&alice, &carol, "alice@example.org", "carol@example.org");
        let sent = alice.encrypt(&[("carol@example.org".into(), carol.device_id())], b"x").unwrap();
        assert!(matches!(bob.decrypt("alice@example.org", &parse_envelope(&sent).unwrap()), Err(Error::NotForUs)));
    }

    #[test]
    fn one_message_is_encrypted_once_and_its_key_to_every_device() {
        let (alice, bob) = two();
        let bob2 = Omemo::open(None);
        introduce(&alice, &bob, "alice@example.org", "bob@example.org");
        introduce(&alice, &bob2, "alice@example.org", "bob@example.org");
        let sent = alice.encrypt(&[("bob@example.org".into(), bob.device_id()), ("bob@example.org".into(), bob2.device_id())], b"both").unwrap();
        let env = parse_envelope(&sent).unwrap();
        assert_eq!(env.keys.len(), 2);
        assert_eq!(bob.decrypt("alice@example.org", &env).unwrap().text.as_deref(), Some("both"));
        assert_eq!(bob2.decrypt("alice@example.org", &env).unwrap().text.as_deref(), Some("both"));
    }

    #[test]
    fn a_payload_that_was_changed_in_transit_is_not_read() {
        let (alice, bob) = two();
        introduce(&alice, &bob, "alice@example.org", "bob@example.org");
        let sent = alice.encrypt(&[("bob@example.org".into(), bob.device_id())], b"intact").unwrap();
        let mut env = parse_envelope(&sent).unwrap();
        env.payload.as_mut().unwrap()[0] ^= 1;
        assert!(matches!(bob.decrypt("alice@example.org", &env), Err(Error::Payload)));
    }

    #[test]
    fn a_device_list_and_a_bundle_are_read_from_what_other_clients_publish() {
        assert_eq!(parse_devicelist(&Element::from_str("<list xmlns='eu.siacs.conversations.axolotl'><device id='12'/><device id='x'/><device id='345'/></list>").unwrap()), vec![12, 345]);
        let (_, bob) = two();
        let bundle = parse_bundle(&Element::from_str(&String::from(&bob.bundle_element())).unwrap()).unwrap();
        assert_eq!(bundle.identity, bob.store().identity().public);
        assert!(bundle.pre_key.is_some());
    }

    #[test]
    fn an_encrypted_file_is_shared_by_a_link_that_carries_its_key_and_is_read_back_from_what_was_fetched() {
        let plain = b"the contents of a file".to_vec();
        let (uploaded, key, iv) = encrypt_file(&plain).unwrap();
        assert_ne!(&uploaded[..plain.len()], &plain[..], "what is uploaded is not the file");
        let url = aesgcm_url("https://up.example.org/f/abc/cat.png", &key, &iv);
        let (fetch, iv2, key2) = parse_aesgcm(&url).unwrap();
        assert_eq!(fetch, "https://up.example.org/f/abc/cat.png");
        assert_eq!(decrypt_file(&uploaded, &iv2, &key2).unwrap(), plain);
        let mut changed = uploaded.clone();
        changed[0] ^= 1;
        assert!(decrypt_file(&changed, &iv2, &key2).is_err(), "a changed file is not read");
        assert!(parse_aesgcm("aesgcm://host/x#abcd").is_none() && parse_aesgcm("https://host/x#00").is_none());
        // The 16-byte IV some older clients used.
        let mut legacy = vec![0u8; 16];
        legacy.extend_from_slice(&key);
        assert_eq!(parse_aesgcm(&format!("aesgcm://h/x#{}", curve::hex(&legacy))).map(|p| p.1.len()), Some(16));
    }

    #[test]
    fn a_key_sent_without_a_payload_makes_the_session_and_says_nothing() {
        let (alice, bob) = two();
        introduce(&alice, &bob, "alice@example.org", "bob@example.org");
        let sent = alice.encrypt(&[("bob@example.org".into(), bob.device_id())], b"x").unwrap();
        let mut env = parse_envelope(&sent).unwrap();
        env.payload = None;
        assert_eq!(bob.decrypt("alice@example.org", &env).unwrap().text, None);
    }

    fn session_v2(from: &Omemo, to: &Omemo, to_jid: &str) {
        let bundle = v2::parse_bundle(&Element::from_str(&String::from(&to.bundle_element_v2())).unwrap()).unwrap();
        let identity = from.store().identity();
        let session = v2::Session::initiate(&identity, &bundle).unwrap();
        from.store().state.v2.sessions.insert(store::key(to_jid, to.device_id()), session);
    }

    #[test]
    fn omemo_2_goes_from_one_device_to_another_and_its_fingerprint_is_the_older_namespaces() {
        let (alice, bob) = two();
        session_v2(&alice, &bob, "bob@example.org");
        let to_bob = vec![Target { jid: "bob@example.org".into(), device: bob.device_id(), v2: true }];
        let sent = alice.encrypt_all(&to_bob, "a <secret> & more", "alice@example.org").unwrap();
        assert_eq!(sent.len(), 1, "nothing in the older namespace when every device speaks OMEMO 2");
        let env = v2::parse_envelope(&Element::from_str(&String::from(&sent[0])).unwrap()).unwrap();
        assert!(env.keys[0].kex);
        let done = bob.decrypt_v2("alice@example.org", "bob@example.org", &env).unwrap();
        assert_eq!(done.text.as_deref(), Some("a <secret> & more"));
        assert_eq!(done.seen, Seen::New);
        let shown = bob.store().state.identities.get(&store::key("alice@example.org", alice.device_id())).cloned();
        assert_eq!(shown, Some(curve::hex(&alice.store().identity().public)), "one device, one fingerprint, whichever namespace");
        // Alice has had no answer: the exchange comes again, and the session it started reads it.
        let again = alice.encrypt_all(&to_bob, "again", "alice@example.org").unwrap();
        let env = v2::parse_envelope(&again[0]).unwrap();
        assert!(env.keys[0].kex);
        assert_eq!(bob.decrypt_v2("alice@example.org", "bob@example.org", &env).unwrap().text.as_deref(), Some("again"));
        // Bob answers; from then on Alice's messages are ordinary ones.
        let to_alice = vec![Target { jid: "alice@example.org".into(), device: alice.device_id(), v2: true }];
        let reply = bob.encrypt_all(&to_alice, "hello", "bob@example.org").unwrap();
        let env = v2::parse_envelope(&reply[0]).unwrap();
        assert!(!env.keys[0].kex);
        assert_eq!(alice.decrypt_v2("bob@example.org", "alice@example.org", &env).unwrap().text.as_deref(), Some("hello"));
        let third = alice.encrypt_all(&to_bob, "three", "alice@example.org").unwrap();
        let env = v2::parse_envelope(&third[0]).unwrap();
        assert!(!env.keys[0].kex);
        assert_eq!(bob.decrypt_v2("alice@example.org", "bob@example.org", &env).unwrap().text.as_deref(), Some("three"));
    }

    #[test]
    fn an_omemo_2_message_that_says_it_is_from_somebody_else_is_not_read() {
        let (alice, bob) = two();
        session_v2(&alice, &bob, "bob@example.org");
        let to_bob = vec![Target { jid: "bob@example.org".into(), device: bob.device_id(), v2: true }];
        let sent = alice.encrypt_all(&to_bob, "it is me, carol", "carol@example.org").unwrap();
        let env = v2::parse_envelope(&sent[0]).unwrap();
        assert!(matches!(bob.decrypt_v2("alice@example.org", "bob@example.org", &env), Err(Error::WrongSender)));
    }

    #[test]
    fn devices_that_speak_only_the_older_namespace_get_it_in_the_same_message() {
        let (alice, bob) = two();
        let carol = Omemo::open(None);
        session_v2(&alice, &bob, "bob@example.org");
        let carol_bundle = parse_bundle(&carol.bundle_element()).unwrap();
        let identity = alice.store().identity();
        let registration = alice.device_id();
        let session = Session::initiate(&identity, registration, &carol_bundle).unwrap();
        alice.store().state.sessions.insert(store::key("carol@example.org", carol.device_id()), session);
        let targets = vec![
            Target { jid: "bob@example.org".into(), device: bob.device_id(), v2: true },
            Target { jid: "carol@example.org".into(), device: carol.device_id(), v2: false },
        ];
        let sent = alice.encrypt_all(&targets, "to you both", "alice@example.org").unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(bob.decrypt_v2("alice@example.org", "bob@example.org", &v2::parse_envelope(&sent[0]).unwrap()).unwrap().text.as_deref(), Some("to you both"));
        assert_eq!(carol.decrypt("alice@example.org", &parse_envelope(&sent[1]).unwrap()).unwrap().text.as_deref(), Some("to you both"));
    }

    #[test]
    fn an_accepted_session_is_answered_with_an_empty_message_which_completes_it() {
        let (alice, bob) = two();
        session_v2(&alice, &bob, "bob@example.org");
        let to_bob = vec![Target { jid: "bob@example.org".into(), device: bob.device_id(), v2: true }];
        let sent = alice.encrypt_all(&to_bob, "hi", "alice@example.org").unwrap();
        let done = bob.decrypt_v2("alice@example.org", "bob@example.org", &v2::parse_envelope(&sent[0]).unwrap()).unwrap();
        assert_eq!(done.answer, Some((alice.device_id(), true)));
        let empty = bob.empty_message("alice@example.org", alice.device_id(), true).unwrap();
        let env = v2::parse_envelope(&empty).unwrap();
        assert!(env.payload.is_none() && !env.keys[0].kex);
        let read = alice.decrypt_v2("bob@example.org", "alice@example.org", &env).unwrap();
        assert_eq!((read.text, read.answer), (None, None), "an empty message is not answered");
        let next = alice.encrypt_all(&to_bob, "now an ordinary message", "alice@example.org").unwrap();
        assert!(!v2::parse_envelope(&next[0]).unwrap().keys[0].kex);
        // The older namespace the same way.
        let carol = Omemo::open(None);
        let carol_bundle = parse_bundle(&carol.bundle_element()).unwrap();
        let identity = alice.store().identity();
        let session = Session::initiate(&identity, alice.device_id(), &carol_bundle).unwrap();
        alice.store().state.sessions.insert(store::key("carol@example.org", carol.device_id()), session);
        let first = alice.empty_message("carol@example.org", carol.device_id(), false).unwrap();
        let got = carol.decrypt("alice@example.org", &parse_envelope(&first).unwrap()).unwrap();
        assert_eq!((got.text, got.answer), (None, Some((alice.device_id(), false))));
        assert_eq!(alice.reset_sessions("carol@example.org"), 1);
        assert_eq!(alice.reset_sessions("bob@example.org"), 1);
    }
}
