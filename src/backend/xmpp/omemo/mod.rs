//! OMEMO (XEP-0384), in the form every client speaks: the older namespace `eu.siacs.conversations.axolotl`, which is the Signal
//! protocol carried in XMPP. (The newer one, `urn:xmpp:omemo:2`, is a different protocol with a different wire form and is not
//! built.)
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
    /// A pre-key was used up or a device is new: the bundle should be published again.
    pub republish: bool,
    pub seen: Seen,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("this message has no key for this device")]
    NotForUs,
    #[error("there is no session with the device that sent this")]
    NoSession,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("the message's key is not the size it should be")]
    BadKey,
    #[error("the message cannot be decrypted")]
    Payload,
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
        let Some(payload) = &env.payload else { return Ok(Decrypted { text: None, republish, seen }) };
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
        Ok(Decrypted { text: Some(String::from_utf8_lossy(&data).to_string()), republish, seen })
    }
}

// --- talking to the server ---------------------------------------------------------------------------------------------

fn publish_options() -> Element {
    let mut x = Element::builder("x", ns::DATA).at("type", "submit").build();
    for (var, value) in [("FORM_TYPE", "http://jabber.org/protocol/pubsub#publish-options"), ("pubsub#persist_items", "true"), ("pubsub#access_model", "open")] {
        let mut f = Element::builder("field", ns::DATA).at("var", var).build();
        f.append_child(Element::builder("value", ns::DATA).append(value).build());
        x.append_child(f);
    }
    Element::builder("publish-options", ns::PUBSUB).append(x).build()
}

async fn publish(handle: &Handle, node: &str, item: Element) -> Result<(), String> {
    let item = Element::builder("item", ns::PUBSUB).at("id", "current").append(item).build();
    let publish = Element::builder("publish", ns::PUBSUB).at("node", node).append(item).build();
    let q = Element::builder("pubsub", ns::PUBSUB).append(publish).append(publish_options()).build();
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())
}

async fn items(handle: &Handle, to: Option<Jid>, node: &str) -> Option<Element> {
    let q = Element::builder("pubsub", ns::PUBSUB).append(Element::builder("items", ns::PUBSUB).at("node", node).build()).build();
    let answer = handle.request(to, false, q).await.ok()??;
    answer.children().find(|c| c.name() == "items").and_then(|i| i.children().find(|c| c.name() == "item").and_then(|item| item.children().next().cloned()))
}

/// Makes this device known: in its owner's device list, with its bundle published. Done after signing in and when keys were used up.
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
}

/// The devices a person has: what is known, or what their device list says now.
pub async fn devices_of(handle: &Handle, omemo: &Omemo, jid: &BareJid, refresh: bool) -> Vec<u32> {
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

/// Starts sessions with the devices of `jid` that have none, and returns the devices a message to them can be encrypted to.
pub async fn usable_devices(handle: &Handle, omemo: &Omemo, jid: &BareJid, skip_own_device: bool) -> Vec<(String, u32)> {
    let ours = omemo.device_id();
    let mut out = Vec::new();
    for device in devices_of(handle, omemo, jid, true).await {
        if skip_own_device && device == ours {
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
            out.push((jid.to_string(), device));
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
    fn a_key_sent_without_a_payload_makes_the_session_and_says_nothing() {
        let (alice, bob) = two();
        introduce(&alice, &bob, "alice@example.org", "bob@example.org");
        let sent = alice.encrypt(&[("bob@example.org".into(), bob.device_id())], b"x").unwrap();
        let mut env = parse_envelope(&sent).unwrap();
        env.payload = None;
        assert_eq!(bob.decrypt("alice@example.org", &env).unwrap().text, None);
    }
}
