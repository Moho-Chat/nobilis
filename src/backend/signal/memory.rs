//! What a connection remembers while it is up, and the few settings it keeps between connections.

use super::store::SignalStore;
use presage::proto::sync_message::Configuration;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use uuid::Uuid;

/// The account's privacy settings as the phone last said them (a `Configuration` sync). A setting the phone has not
/// said is `None`, and is treated as off where acting on it would tell somebody something: nothing is sent as a
/// receipt or a typing notice on a guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Synced {
    pub read_receipts: Option<bool>,
    pub typing_indicators: Option<bool>,
    pub link_previews: Option<bool>,
    pub unidentified_delivery_indicators: Option<bool>,
}

const NAME: &str = "synced-settings";

impl Synced {
    pub fn load(store: &SignalStore) -> Self {
        store.blob(NAME).ok().flatten().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    pub fn save(&self, store: &SignalStore) {
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = store.set_blob(NAME, &bytes);
        }
    }

    /// What the phone said, laid over what was known: a setting it left out keeps its old value.
    pub fn with(self, said: &Configuration) -> Self {
        Self {
            read_receipts: said.read_receipts.or(self.read_receipts),
            typing_indicators: said.typing_indicators.or(self.typing_indicators),
            link_previews: said.link_previews.or(self.link_previews),
            unidentified_delivery_indicators: said.unidentified_delivery_indicators.or(self.unidentified_delivery_indicators),
        }
    }

    pub fn known(&self) -> bool {
        self.read_receipts.is_some() && self.typing_indicators.is_some()
    }
}

#[derive(Default)]
pub struct Memory {
    pub synced: Mutex<Synced>,
    /// Where each person has read up to in each conversation (the time of the newest of this account's messages their
    /// receipt named), by conversation.
    pub read_marks: Mutex<HashMap<String, HashMap<Uuid, u64>>>,
    /// Messages a read receipt has been dealt with for, so reading a conversation twice sends nothing twice.
    pub receipted: Mutex<HashSet<String>>,
    /// People whose profile has been asked for on this connection, so it is asked for once.
    pub asked_profiles: Mutex<HashSet<Uuid>>,
}

impl Memory {
    pub fn synced(&self) -> Synced {
        *self.synced.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_synced(&self, synced: Synced) {
        *self.synced.lock().unwrap_or_else(|e| e.into_inner()) = synced;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::model::identity::OnNewIdentity;

    #[test]
    fn the_phones_settings_are_kept_and_a_setting_it_leaves_out_stays() {
        let store = SignalStore::open_in_memory(OnNewIdentity::Trust).unwrap();
        assert_eq!(Synced::load(&store), Synced::default());
        let first = Synced::default().with(&Configuration { read_receipts: Some(true), typing_indicators: Some(false), ..Default::default() });
        first.save(&store);
        assert_eq!(Synced::load(&store), first);
        assert!(first.known());
        let later = first.with(&Configuration { typing_indicators: Some(true), ..Default::default() });
        assert_eq!((later.read_receipts, later.typing_indicators), (Some(true), Some(true)));
        assert!(!Synced::default().known());
    }
}
