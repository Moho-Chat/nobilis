//! What an account keeps for OMEMO: the keys that are this device, the sessions with everybody else's, who is trusted, and
//! which conversations are encrypted. One private file per account; losing it means this device has to start over (a new
//! device id, new sessions, and everybody asked to trust it again), so it is written whole through a temporary file and
//! replaced, never changed in place.

use super::curve::{self, KeyPair};
use super::ratchet::{Pair, Session};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// How many one-time pre-keys are kept published.
pub const PRE_KEYS: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// Seen, and not yet decided: nothing is encrypted to it.
    Undecided,
    /// Trusted without anybody having compared fingerprints (blind trust before verification).
    Trusted,
    /// The fingerprint was compared with the person.
    Verified,
    Untrusted,
}

impl Trust {
    pub fn encrypts(self) -> bool {
        matches!(self, Trust::Trusted | Trust::Verified)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SignedPreKey {
    pub id: u32,
    pub pair: Pair,
    pub signature: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    pub device_id: u32,
    pub identity: Option<Pair>,
    pub signed_pre_key: Option<SignedPreKey>,
    pub pre_keys: BTreeMap<u32, Pair>,
    pub next_pre_key_id: u32,
    /// Sessions, by `jid/device`.
    pub sessions: BTreeMap<String, Session>,
    /// Trust, by `jid/device`.
    pub trust: BTreeMap<String, Trust>,
    /// The identity key (hex) each `jid/device` has shown.
    pub identities: BTreeMap<String, String>,
    /// The devices each person is known to have.
    pub devices: BTreeMap<String, Vec<u32>>,
    /// Conversations (by address) that are encrypted.
    pub enabled: BTreeSet<String>,
    /// OMEMO 2's own keys and sessions (the identity and the device id are the ones above).
    #[serde(default)]
    pub v2: V2State,
    /// Decisions about keys not seen yet (from a verification link or a trust message), applied when they are: by person, the key
    /// (hex) and whether it is trusted.
    #[serde(default)]
    pub pending_keys: BTreeMap<String, BTreeMap<String, bool>>,
    /// What devices that are not verified yet said in trust messages (XEP-0450): kept until they are, then applied.
    #[serde(default)]
    pub cached_trust: Vec<CachedTrust>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CachedTrust {
    /// The device that said it, `jid/device`.
    pub sender: String,
    pub owner: String,
    /// The key, hex.
    pub key: String,
    pub trust: bool,
}

/// What OMEMO 2 keeps beside the older namespace's state.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct V2State {
    pub signed_pre_key: Option<SignedPreKey>,
    pub pre_keys: BTreeMap<u32, Pair>,
    pub next_pre_key_id: u32,
    /// Sessions, by `jid/device`.
    pub sessions: BTreeMap<String, super::v2::Session>,
    /// The devices each person lists under OMEMO 2.
    pub devices: BTreeMap<String, Vec<u32>>,
}

pub struct Store {
    path: Option<PathBuf>,
    pub state: State,
}

pub fn key(jid: &str, device: u32) -> String {
    format!("{jid}/{device}")
}

impl Store {
    /// Opens the account's file, or makes the device from nothing.
    pub fn open(path: Option<PathBuf>) -> Store {
        let state = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<State>(&t).ok());
        let mut store = Store { path, state: state.unwrap_or_default() };
        if store.state.identity.is_none() {
            store.make_device();
        }
        if store.state.v2.signed_pre_key.is_none() {
            store.new_signed_pre_key_v2();
            store.top_up_v2();
            store.save();
        }
        store
    }

    fn make_device(&mut self) {
        let identity = KeyPair::generate();
        self.state.device_id = rand::thread_rng().gen_range(1..0x7fff_ffff);
        self.state.identity = Some((&identity).into());
        self.state.signed_pre_key = None;
        self.state.pre_keys.clear();
        self.state.next_pre_key_id = 1;
        self.new_signed_pre_key();
        self.top_up();
        self.save();
    }

    pub fn identity(&self) -> KeyPair {
        self.state.identity.as_ref().map(KeyPair::from).expect("a device has an identity")
    }

    /// A new signed pre-key, signed by the identity key.
    pub fn new_signed_pre_key(&mut self) {
        let identity = self.identity();
        let pair = KeyPair::generate();
        let id = self.state.signed_pre_key.as_ref().map(|s| s.id + 1).unwrap_or(1);
        let signature = curve::sign(&identity.private, &pair.public_serialized());
        self.state.signed_pre_key = Some(SignedPreKey { id, pair: (&pair).into(), signature: curve::hex(&signature) });
    }

    /// OMEMO 2's signed pre-key: an Ed25519 signature, by the identity key, over the bare 32 bytes.
    pub fn new_signed_pre_key_v2(&mut self) {
        let identity = self.identity();
        let pair = KeyPair::generate();
        let id = self.state.v2.signed_pre_key.as_ref().map(|s| s.id + 1).unwrap_or(1);
        let signature = super::v2::ed_sign(&identity.private, &pair.public);
        self.state.v2.signed_pre_key = Some(SignedPreKey { id, pair: (&pair).into(), signature: curve::hex(&signature) });
    }

    /// OMEMO 2's one-time pre-keys, made until there are enough. Returns whether it made any.
    pub fn top_up_v2(&mut self) -> bool {
        let mut made = false;
        if self.state.v2.next_pre_key_id == 0 {
            self.state.v2.next_pre_key_id = 1;
        }
        while (self.state.v2.pre_keys.len() as u32) < PRE_KEYS {
            let id = self.state.v2.next_pre_key_id;
            self.state.v2.next_pre_key_id += 1;
            self.state.v2.pre_keys.insert(id, (&KeyPair::generate()).into());
            made = true;
        }
        made
    }

    /// Makes one-time pre-keys until there are enough. Returns whether it made any.
    pub fn top_up(&mut self) -> bool {
        let mut made = false;
        while (self.state.pre_keys.len() as u32) < PRE_KEYS {
            let id = self.state.next_pre_key_id;
            self.state.next_pre_key_id += 1;
            self.state.pre_keys.insert(id, (&KeyPair::generate()).into());
            made = true;
        }
        made
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let Ok(text) = serde_json::to_string(&self.state) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        let written = crate::secure::create_private_file(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(text.as_bytes())?;
            f.sync_all().map_err(Into::into)
        });
        if written.is_ok() {
            let _ = std::fs::rename(&tmp, path);
        } else {
            tracing::warn!("xmpp: the OMEMO keys could not be written");
        }
    }

    /// Records the identity key a device has shown. Returns what that means: a new device, the same one, or a changed key.
    pub fn see_identity(&mut self, jid: &str, device: u32, identity: &[u8; 32]) -> Seen {
        let k = key(jid, device);
        let hex = curve::hex(identity);
        match self.state.identities.get(&k) {
            Some(known) if *known == hex => Seen::Known,
            Some(_) => {
                self.state.identities.insert(k.clone(), hex);
                self.state.trust.insert(k.clone(), Trust::Untrusted);
                self.state.sessions.remove(&k);
                self.state.v2.sessions.remove(&k);
                self.save();
                Seen::Changed
            }
            None => {
                self.state.identities.insert(k.clone(), hex.clone());
                // Blind trust before verification: a new device is trusted, unless this person has a device somebody
                // checked by hand - then a new one is a question, not an answer.
                let someone_verified = self.state.trust.iter().any(|(other, t)| other.starts_with(&format!("{jid}/")) && *t == Trust::Verified);
                self.state.trust.insert(k.clone(), if someone_verified { Trust::Undecided } else { Trust::Trusted });
                // A decision made before the key was seen (a link scanned, a trust message) is applied now.
                let decided = self.state.pending_keys.get_mut(jid).and_then(|p| p.remove(&hex));
                if self.state.pending_keys.get(jid).is_some_and(|p| p.is_empty()) {
                    self.state.pending_keys.remove(jid);
                }
                match decided {
                    Some(true) => self.set_trust(jid, device, Trust::Verified),
                    Some(false) => self.set_trust(jid, device, Trust::Untrusted),
                    None => {}
                }
                self.save();
                Seen::New
            }
        }
    }

    pub fn trust_of(&self, jid: &str, device: u32) -> Trust {
        self.state.trust.get(&key(jid, device)).copied().unwrap_or(Trust::Undecided)
    }

    /// Sets how far a device is trusted. A device that is now verified has what it said in trust messages applied.
    pub fn set_trust(&mut self, jid: &str, device: u32, trust: Trust) {
        self.state.trust.insert(key(jid, device), trust);
        if trust == Trust::Verified {
            self.release_cached(&key(jid, device));
        }
        self.save();
    }

    /// Trusts (verifies) or distrusts a person's key: the devices that showed it, or, if none has yet, the device that shows it
    /// when one does. Returns the devices it changed.
    pub fn decide_key(&mut self, owner: &str, key_bytes: &[u8; 32], trust: bool) -> Vec<u32> {
        let hex = curve::hex(key_bytes);
        let prefix = format!("{owner}/");
        let devices: Vec<u32> = self.state.identities.iter().filter(|(k, v)| k.starts_with(&prefix) && **v == hex).filter_map(|(k, _)| k[prefix.len()..].parse().ok()).collect();
        if devices.is_empty() {
            self.state.pending_keys.entry(owner.to_string()).or_default().insert(hex, trust);
            self.save();
            return devices;
        }
        for d in &devices {
            self.set_trust(owner, *d, if trust { Trust::Verified } else { Trust::Untrusted });
        }
        devices
    }

    /// A trust message from `sender_jid`/`sender_device`: applied if that device is verified, kept until it is otherwise. A
    /// contact's device speaks only for its own account's keys; this account's devices speak for anyone's. Returns how many
    /// decisions were applied now.
    pub fn receive_trust(&mut self, sender_jid: &str, sender_device: u32, own_jid: &str, decisions: &super::atm::Decisions) -> usize {
        let sender = key(sender_jid, sender_device);
        let verified = self.trust_of(sender_jid, sender_device) == Trust::Verified;
        let mut applied = 0;
        for o in decisions.owners.iter().filter(|o| sender_jid == own_jid || o.jid == sender_jid) {
            for (k, trust) in o.trust.iter().map(|k| (k, true)).chain(o.distrust.iter().map(|k| (k, false))) {
                if verified {
                    self.decide_key(&o.jid, k, trust);
                    applied += 1;
                } else {
                    let c = CachedTrust { sender: sender.clone(), owner: o.jid.clone(), key: curve::hex(k), trust };
                    if !self.state.cached_trust.contains(&c) {
                        self.state.cached_trust.push(c);
                    }
                }
            }
        }
        self.save();
        applied
    }

    /// Applies what a device that is now verified said before it was.
    fn release_cached(&mut self, sender: &str) {
        let (mine, rest): (Vec<CachedTrust>, Vec<CachedTrust>) = std::mem::take(&mut self.state.cached_trust).into_iter().partition(|c| c.sender == sender);
        self.state.cached_trust = rest;
        for c in mine {
            if let Some(k) = curve::unhex(&c.key).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                self.decide_key(&c.owner, &k, c.trust);
            }
        }
    }

    /// The verified devices, as `(jid, device, key)`.
    pub fn verified(&self) -> Vec<(String, u32, [u8; 32])> {
        self.state
            .trust
            .iter()
            .filter(|(_, t)| **t == Trust::Verified)
            .filter_map(|(k, _)| {
                let (jid, device) = k.rsplit_once('/')?;
                let identity = curve::unhex(self.state.identities.get(k)?)?.try_into().ok()?;
                Some((jid.to_string(), device.parse().ok()?, identity))
            })
            .collect()
    }

    /// The readable form of a fingerprint: the identity key in lower-case hex, in groups of eight.
    pub fn fingerprint(identity_hex: &str) -> String {
        identity_hex.as_bytes().chunks(8).map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Debug, PartialEq)]
pub enum Seen {
    New,
    Known,
    Changed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_is_made_once_with_its_keys_and_its_pre_keys() {
        let s = Store::open(None);
        assert!(s.state.device_id > 0 && s.state.identity.is_some());
        assert_eq!(s.state.pre_keys.len() as u32, PRE_KEYS);
        let signed = s.state.signed_pre_key.as_ref().unwrap();
        let sig: [u8; 64] = curve::unhex(&signed.signature).unwrap().try_into().unwrap();
        assert!(curve::verify(&s.identity().public, &KeyPair::from(&signed.pair).public_serialized(), &sig));
    }

    #[test]
    fn a_new_device_is_trusted_until_somebody_has_been_checked_by_hand_and_a_changed_key_is_not() {
        let mut s = Store::open(None);
        let (k1, k2, k3) = (KeyPair::generate().public, KeyPair::generate().public, KeyPair::generate().public);
        assert_eq!(s.see_identity("bob@example.org", 1, &k1), Seen::New);
        assert_eq!(s.trust_of("bob@example.org", 1), Trust::Trusted);
        assert_eq!(s.see_identity("bob@example.org", 1, &k1), Seen::Known);
        s.set_trust("bob@example.org", 1, Trust::Verified);
        assert_eq!(s.see_identity("bob@example.org", 2, &k2), Seen::New);
        assert_eq!(s.trust_of("bob@example.org", 2), Trust::Undecided, "once one device was verified, a new one is a question");
        assert_eq!(s.see_identity("bob@example.org", 1, &k3), Seen::Changed);
        assert_eq!(s.trust_of("bob@example.org", 1), Trust::Untrusted);
        assert!(!Trust::Undecided.encrypts() && !Trust::Untrusted.encrypts() && Trust::Verified.encrypts());
    }

    #[test]
    fn the_file_keeps_everything_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("moho-omemo-{}", std::process::id()));
        let path = dir.join("a.omemo");
        let s = Store::open(Some(path.clone()));
        let id = s.state.device_id;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let again = Store::open(Some(path));
        assert_eq!((again.state.device_id, again.identity().public), (id, s.identity().public));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn omemo_2_has_its_own_signed_pre_key_under_the_same_identity() {
        let s = Store::open(None);
        let signed = s.state.v2.signed_pre_key.as_ref().unwrap();
        let sig: [u8; 64] = curve::unhex(&signed.signature).unwrap().try_into().unwrap();
        assert!(super::super::v2::ed_verify(&super::super::v2::ed_public(&s.identity().private), &signed.pair.public, &sig));
        assert_eq!(s.state.v2.pre_keys.len() as u32, PRE_KEYS);
    }

    #[test]
    fn trust_messages_wait_for_their_sender_to_be_verified_and_contacts_speak_only_for_themselves() {
        use super::super::atm::Decisions;
        let mut s = Store::open(None);
        let (own2, bob1, bob2, carol1) = (KeyPair::generate().public, KeyPair::generate().public, KeyPair::generate().public, KeyPair::generate().public);
        s.see_identity("me@example.org", 2, &own2);
        s.see_identity("bob@example.org", 1, &bob1);
        s.see_identity("carol@example.org", 1, &carol1);
        // My other device says: trust Bob's second device (not seen yet) and Carol's.
        let mut d = Decisions::default();
        d.owner("bob@example.org").trust.push(bob2);
        d.owner("carol@example.org").distrust.push(carol1);
        assert_eq!(s.receive_trust("me@example.org", 2, "me@example.org", &d), 0, "not verified yet: kept");
        assert_eq!(s.trust_of("carol@example.org", 1), Trust::Trusted);
        s.set_trust("me@example.org", 2, Trust::Verified);
        assert_eq!(s.trust_of("carol@example.org", 1), Trust::Untrusted, "applied once the sender is verified");
        assert_eq!(s.see_identity("bob@example.org", 2, &bob2), Seen::New);
        assert_eq!(s.trust_of("bob@example.org", 2), Trust::Verified, "a decision about a key waits for the key");
        // Bob's verified device may speak for Bob, not for Carol.
        s.set_trust("bob@example.org", 1, Trust::Verified);
        let mut c = Decisions::default();
        c.owner("carol@example.org").trust.push(carol1);
        assert_eq!(s.receive_trust("bob@example.org", 1, "me@example.org", &c), 0);
        assert_eq!(s.trust_of("carol@example.org", 1), Trust::Untrusted);
        assert!(s.verified().iter().any(|(j, d, k)| j == "bob@example.org" && *d == 2 && *k == bob2));
    }

    #[test]
    fn a_fingerprint_reads_in_groups() {
        assert_eq!(Store::fingerprint("aabbccdd11223344"), "aabbccdd 11223344");
    }
}
