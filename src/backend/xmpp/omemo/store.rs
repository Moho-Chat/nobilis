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
                self.save();
                Seen::Changed
            }
            None => {
                self.state.identities.insert(k.clone(), hex);
                // Blind trust before verification: a new device is trusted, unless this person has a device somebody
                // checked by hand - then a new one is a question, not an answer.
                let someone_verified = self.state.trust.iter().any(|(other, t)| other.starts_with(&format!("{jid}/")) && *t == Trust::Verified);
                self.state.trust.insert(k, if someone_verified { Trust::Undecided } else { Trust::Trusted });
                self.save();
                Seen::New
            }
        }
    }

    pub fn trust_of(&self, jid: &str, device: u32) -> Trust {
        self.state.trust.get(&key(jid, device)).copied().unwrap_or(Trust::Undecided)
    }

    pub fn set_trust(&mut self, jid: &str, device: u32, trust: Trust) {
        self.state.trust.insert(key(jid, device), trust);
        self.save();
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
    fn a_fingerprint_reads_in_groups() {
        assert_eq!(Store::fingerprint("aabbccdd11223344"), "aabbccdd 11223344");
    }
}
