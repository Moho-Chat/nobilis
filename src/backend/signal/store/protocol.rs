//! The part of the store the Signal protocol itself reads and writes while it runs: sessions, identities, the three kinds
//! of pre-key, and sender keys. One instance is for the account's ACI and one for its PNI (the account's phone-number
//! identity); they share the tables and differ in the `identity` column.

use super::{kv_get, SignalStore};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use presage::{
    libsignal_service::{
        pre_keys::{KyberPreKeyStoreExt, PreKeysStore},
        prelude::{SessionStoreExt, Uuid},
        protocol::{
            CiphertextMessageType, DeviceId, Direction, GenericSignedPreKey, IdentityChange, IdentityKey,
            IdentityKeyPair, IdentityKeyStore, KyberPreKeyId, KyberPreKeyRecord, KyberPreKeyStore, PreKeyId,
            PreKeyRecord, PreKeyStore, ProtocolAddress, ProtocolStore, PublicKey, SenderKeyRecord, SenderKeyStore,
            ServiceId, SessionRecord, SessionStore, SignalProtocolError, SignedPreKeyId, SignedPreKeyRecord,
            SignedPreKeyStore,
        },
        push_service::DEFAULT_DEVICE_ID,
    },
    model::identity::OnNewIdentity,
    store::StateStore,
};
use rusqlite::{params, OptionalExtension};
use tracing::warn;

type ProtocolResult<T> = Result<T, SignalProtocolError>;

#[derive(Clone)]
pub struct SignalProtocolStore {
    pub(crate) store: SignalStore,
    pub(crate) identity: IdentityType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityType {
    Aci,
    Pni,
}

impl IdentityType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Aci => "aci",
            Self::Pni => "pni",
        }
    }

    pub(crate) fn identity_key_pair_key(self) -> &'static str {
        match self {
            Self::Aci => "identity_keypair_aci",
            Self::Pni => "identity_keypair_pni",
        }
    }
}

impl ProtocolStore for SignalProtocolStore {}

/// Largest id in a table for this identity, or none if the table holds no key for it.
fn max_id(store: &SignalStore, table: &'static str, identity: IdentityType) -> ProtocolResult<Option<u32>> {
    Ok(store.read(|conn| {
        Ok(conn.query_row(
            &format!("SELECT MAX(id) FROM {table} WHERE identity = ?1"),
            [identity.as_str()],
            |row| row.get::<_, Option<u32>>(0),
        )?)
    })?)
}

fn count(store: &SignalStore, sql: &'static str, identity: IdentityType) -> ProtocolResult<usize> {
    let n: i64 = store.read(|conn| Ok(conn.query_row(sql, [identity.as_str()], |row| row.get(0))?))?;
    Ok(usize::try_from(n).unwrap_or(0))
}

#[async_trait(?Send)]
impl SessionStore for SignalProtocolStore {
    async fn load_session(&self, address: &ProtocolAddress) -> ProtocolResult<Option<SessionRecord>> {
        let device_id: u8 = address.device_id().into();
        let name = address.name();
        let identity = self.identity.as_str();
        let record: Option<Vec<u8>> = self.store.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT record FROM sessions WHERE address = ?1 AND device_id = ?2 AND identity = ?3",
                    params![name, device_id, identity],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        record.map(|bytes| SessionRecord::deserialize(&bytes)).transpose()
    }

    async fn store_session(&mut self, address: &ProtocolAddress, record: &SessionRecord) -> ProtocolResult<()> {
        let device_id: u8 = address.device_id().into();
        let name = address.name();
        let identity = self.identity.as_str();
        let record = record.serialize()?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO sessions (address, device_id, identity, record) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (address, device_id, identity) DO UPDATE SET record = excluded.record",
                params![name, device_id, identity, record],
            )?;
            Ok(())
        })?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl SessionStoreExt for SignalProtocolStore {
    /// Every device of a recipient that has a session here, except the main one.
    async fn get_sub_device_sessions(&self, name: &ServiceId) -> ProtocolResult<Vec<DeviceId>> {
        let address = name.service_id_string();
        let main: u8 = (*DEFAULT_DEVICE_ID).into();
        let identity = self.identity.as_str();
        let ids: Vec<u32> = self.store.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT device_id FROM sessions WHERE address = ?1 AND device_id != ?2 AND identity = ?3")?;
            let rows = stmt.query_map(params![address, main, identity], |row| row.get::<_, u32>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })?;
        Ok(ids.into_iter().filter_map(|id| id.try_into().ok()).collect())
    }

    async fn delete_session(&self, address: &ProtocolAddress) -> ProtocolResult<()> {
        let device_id: u8 = address.device_id().into();
        let name = address.name();
        let identity = self.identity.as_str();
        self.store.write(|tx| {
            tx.execute(
                "DELETE FROM sessions WHERE address = ?1 AND device_id = ?2 AND identity = ?3",
                params![name, device_id, identity],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Every session with a recipient, all devices; how many there were.
    async fn delete_all_sessions(&self, name: &ServiceId) -> ProtocolResult<usize> {
        let address = name.service_id_string();
        let identity = self.identity.as_str();
        Ok(self
            .store
            .write(|tx| Ok(tx.execute("DELETE FROM sessions WHERE address = ?1 AND identity = ?2", params![address, identity])?))?)
    }
}

#[async_trait(?Send)]
impl PreKeyStore for SignalProtocolStore {
    async fn get_pre_key(&self, prekey_id: PreKeyId) -> ProtocolResult<PreKeyRecord> {
        let id: u32 = prekey_id.into();
        let identity = self.identity.as_str();
        let record: Option<Vec<u8>> = self.store.read(|conn| {
            Ok(conn
                .query_row("SELECT record FROM pre_keys WHERE id = ?1 AND identity = ?2", params![id, identity], |row| {
                    row.get(0)
                })
                .optional()?)
        })?;
        PreKeyRecord::deserialize(&record.ok_or(SignalProtocolError::InvalidPreKeyId)?)
    }

    async fn save_pre_key(&mut self, prekey_id: PreKeyId, record: &PreKeyRecord) -> ProtocolResult<()> {
        let id: u32 = prekey_id.into();
        let identity = self.identity.as_str();
        let record = record.serialize()?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO pre_keys (id, identity, record) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id, identity) DO UPDATE SET record = excluded.record",
                params![id, identity, record],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    async fn remove_pre_key(&mut self, prekey_id: PreKeyId) -> ProtocolResult<()> {
        let id: u32 = prekey_id.into();
        let identity = self.identity.as_str();
        self.store.write(|tx| {
            tx.execute("DELETE FROM pre_keys WHERE id = ?1 AND identity = ?2", params![id, identity])?;
            Ok(())
        })?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl PreKeysStore for SignalProtocolStore {
    async fn next_pre_key_id(&self) -> ProtocolResult<u32> {
        Ok(max_id(&self.store, "pre_keys", self.identity)?.map(|id| id + 1).unwrap_or(1))
    }

    async fn next_signed_pre_key_id(&self) -> ProtocolResult<u32> {
        Ok(max_id(&self.store, "signed_pre_keys", self.identity)?.map(|id| id + 1).unwrap_or(1))
    }

    async fn next_pq_pre_key_id(&self) -> ProtocolResult<u32> {
        Ok(max_id(&self.store, "kyber_pre_keys", self.identity)?.map(|id| id + 1).unwrap_or(1))
    }

    async fn signed_pre_keys_count(&self) -> ProtocolResult<usize> {
        count(&self.store, "SELECT COUNT(id) FROM signed_pre_keys WHERE identity = ?1", self.identity)
    }

    /// How many Kyber pre-keys there are, of the kind asked for. (Presage's own store counts both kinds whichever is
    /// asked for.)
    async fn kyber_pre_keys_count(&self, last_resort: bool) -> ProtocolResult<usize> {
        if last_resort {
            count(
                &self.store,
                "SELECT COUNT(id) FROM kyber_pre_keys WHERE identity = ?1 AND is_last_resort != 0",
                self.identity,
            )
        } else {
            count(
                &self.store,
                "SELECT COUNT(id) FROM kyber_pre_keys WHERE identity = ?1 AND is_last_resort = 0",
                self.identity,
            )
        }
    }

    async fn signed_prekey_id(&self) -> ProtocolResult<Option<SignedPreKeyId>> {
        Ok(max_id(&self.store, "signed_pre_keys", self.identity)?.map(SignedPreKeyId::from))
    }

    async fn last_resort_kyber_prekey_id(&self) -> ProtocolResult<Option<KyberPreKeyId>> {
        let identity = self.identity.as_str();
        let id: Option<u32> = self.store.read(|conn| {
            Ok(conn.query_row(
                "SELECT MAX(id) FROM kyber_pre_keys WHERE identity = ?1 AND is_last_resort != 0",
                [identity],
                |row| row.get(0),
            )?)
        })?;
        Ok(id.map(KyberPreKeyId::from))
    }
}

#[async_trait(?Send)]
impl SignedPreKeyStore for SignalProtocolStore {
    async fn get_signed_pre_key(&self, signed_prekey_id: SignedPreKeyId) -> ProtocolResult<SignedPreKeyRecord> {
        let id: u32 = signed_prekey_id.into();
        let identity = self.identity.as_str();
        let record: Option<Vec<u8>> = self.store.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT record FROM signed_pre_keys WHERE id = ?1 AND identity = ?2",
                    params![id, identity],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        SignedPreKeyRecord::deserialize(&record.ok_or(SignalProtocolError::InvalidSignedPreKeyId)?)
    }

    async fn save_signed_pre_key(
        &mut self,
        signed_prekey_id: SignedPreKeyId,
        record: &SignedPreKeyRecord,
    ) -> ProtocolResult<()> {
        let id: u32 = signed_prekey_id.into();
        let identity = self.identity.as_str();
        let record = record.serialize()?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO signed_pre_keys (id, identity, record) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id, identity) DO UPDATE SET record = excluded.record",
                params![id, identity, record],
            )?;
            Ok(())
        })?;
        Ok(())
    }
}

/// What marking a Kyber key as used came to.
enum Used {
    Fine,
    /// A last-resort key met a base key it had already met: the same handshake replayed.
    ReusedBaseKey,
}

#[async_trait(?Send)]
impl KyberPreKeyStore for SignalProtocolStore {
    async fn get_kyber_pre_key(&self, kyber_prekey_id: KyberPreKeyId) -> ProtocolResult<KyberPreKeyRecord> {
        let id: u32 = kyber_prekey_id.into();
        let identity = self.identity.as_str();
        let record: Option<Vec<u8>> = self.store.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT record FROM kyber_pre_keys WHERE id = ?1 AND identity = ?2",
                    params![id, identity],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        KyberPreKeyRecord::deserialize(&record.ok_or(SignalProtocolError::InvalidKyberPreKeyId)?)
    }

    async fn save_kyber_pre_key(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
        record: &KyberPreKeyRecord,
    ) -> ProtocolResult<()> {
        let id: u32 = kyber_prekey_id.into();
        let identity = self.identity.as_str();
        let record = record.serialize()?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO kyber_pre_keys (id, identity, record) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id, identity) DO UPDATE SET record = excluded.record",
                params![id, identity, record],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// A one-time Kyber key is deleted the first time it is used. A last-resort key is kept, and the handshake that used
    /// it is remembered instead (its signed pre-key id and the sender's base key), so the same handshake cannot be
    /// played to it twice.
    async fn mark_kyber_pre_key_used(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
        ec_prekey_id: SignedPreKeyId,
        base_key: &PublicKey,
    ) -> ProtocolResult<()> {
        let kyber_id: u32 = kyber_prekey_id.into();
        let ec_id: u32 = ec_prekey_id.into();
        let base_key = base_key.serialize();
        let identity = self.identity.as_str();
        let used = self.store.write(|tx| {
            let last_resort: Option<i64> = tx
                .query_row(
                    "SELECT is_last_resort FROM kyber_pre_keys WHERE id = ?1 AND identity = ?2",
                    params![kyber_id, identity],
                    |row| row.get(0),
                )
                .optional()?;
            match last_resort {
                None => Err(SignalProtocolError::InvalidKyberPreKeyId.into()),
                Some(0) => {
                    tx.execute(
                        "DELETE FROM kyber_pre_keys WHERE id = ?1 AND identity = ?2 AND is_last_resort = 0",
                        params![kyber_id, identity],
                    )?;
                    Ok(Used::Fine)
                }
                Some(_) => {
                    let inserted = tx.execute(
                        "INSERT INTO base_keys_seen (kyber_pre_key_id, signed_pre_key_id, identity, base_key)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![kyber_id, ec_id, identity, &*base_key],
                    );
                    match inserted {
                        Ok(_) => Ok(Used::Fine),
                        Err(rusqlite::Error::SqliteFailure(failure, _))
                            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                                || failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
                        {
                            Ok(Used::ReusedBaseKey)
                        }
                        Err(other) => Err(other.into()),
                    }
                }
            }
        })?;
        match used {
            Used::Fine => Ok(()),
            Used::ReusedBaseKey => Err(SignalProtocolError::InvalidMessage(
                CiphertextMessageType::PreKey,
                "reused base key".to_owned(),
            )),
        }
    }
}

#[async_trait(?Send)]
impl KyberPreKeyStoreExt for SignalProtocolStore {
    async fn store_last_resort_kyber_pre_key(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
        record: &KyberPreKeyRecord,
    ) -> ProtocolResult<()> {
        let id: u32 = kyber_prekey_id.into();
        let identity = self.identity.as_str();
        let record = record.serialize()?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO kyber_pre_keys (id, identity, is_last_resort, record) VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT (id, identity) DO UPDATE SET is_last_resort = 1, record = excluded.record",
                params![id, identity, record],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    async fn load_last_resort_kyber_pre_keys(&self) -> ProtocolResult<Vec<KyberPreKeyRecord>> {
        let identity = self.identity.as_str();
        let records: Vec<Vec<u8>> = self.store.read(|conn| {
            let mut stmt =
                conn.prepare("SELECT record FROM kyber_pre_keys WHERE identity = ?1 AND is_last_resort != 0")?;
            let rows = stmt.query_map([identity], |row| row.get::<_, Vec<u8>>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })?;
        records.iter().map(|bytes| KyberPreKeyRecord::deserialize(bytes)).collect()
    }

    async fn remove_kyber_pre_key(&mut self, kyber_prekey_id: KyberPreKeyId) -> ProtocolResult<()> {
        let id: u32 = kyber_prekey_id.into();
        let identity = self.identity.as_str();
        self.store.write(|tx| {
            tx.execute("DELETE FROM kyber_pre_keys WHERE id = ?1 AND identity = ?2", params![id, identity])?;
            Ok(())
        })?;
        Ok(())
    }

    /// Nothing in the library calls this yet, and presage's own store stops the program if it ever does. Keeping the
    /// keys (the safe direction: a key kept too long is only a key) is what this does until staleness is tracked.
    async fn mark_all_one_time_kyber_pre_keys_stale_if_necessary(
        &mut self,
        _stale_time: DateTime<Utc>,
    ) -> ProtocolResult<()> {
        Ok(())
    }

    /// See above.
    async fn delete_all_stale_one_time_kyber_pre_keys(
        &mut self,
        _threshold: DateTime<Utc>,
        _min_count: usize,
    ) -> ProtocolResult<()> {
        Ok(())
    }
}

#[async_trait(?Send)]
impl IdentityKeyStore for SignalProtocolStore {
    async fn get_identity_key_pair(&self) -> ProtocolResult<IdentityKeyPair> {
        let key = self.identity.identity_key_pair_key();
        let bytes = self.store.read(|conn| kv_get(conn, key))?.ok_or_else(|| {
            SignalProtocolError::InvalidState("identity key pair", "this account has no identity key yet".into())
        })?;
        IdentityKeyPair::try_from(&*bytes)
    }

    /// The registration id the account was linked with. It does not change run over run.
    async fn get_local_registration_id(&self) -> ProtocolResult<u32> {
        let data = self.store.load_registration_data().await?.ok_or_else(|| {
            SignalProtocolError::InvalidState("failed to load registration ID", "no registration data".into())
        })?;
        Ok(data.registration_id)
    }

    async fn save_identity(&mut self, address: &ProtocolAddress, identity: &IdentityKey) -> ProtocolResult<IdentityChange> {
        let name = address.name();
        let bytes = identity.serialize();
        let kind = self.identity.as_str();
        let replaced = self.store.write(|tx| {
            let existing: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT record FROM identities WHERE address = ?1 AND identity = ?2",
                    params![name, kind],
                    |row| row.get(0),
                )
                .optional()?;
            tx.execute(
                "INSERT INTO identities (address, identity, record) VALUES (?1, ?2, ?3)
                 ON CONFLICT (address, identity) DO UPDATE SET record = excluded.record",
                params![name, kind, &*bytes],
            )?;
            // Replaced means a *different* key was there; the same key saved again is no change.
            Ok(existing.is_some_and(|old| old[..] != *bytes))
        })?;
        if replaced && self.identity.as_str() == "aci" {
            self.store.note_changed_identity(name);
        }
        Ok(if replaced { IdentityChange::ReplacedExisting } else { IdentityChange::NewOrUnchanged })
    }

    async fn is_trusted_identity(
        &self,
        address: &ProtocolAddress,
        identity: &IdentityKey,
        _direction: Direction,
    ) -> ProtocolResult<bool> {
        match self.get_identity(address).await? {
            Some(known) if &known == identity => Ok(true),
            // A key that is not the one on record: whether to go on is a setting.
            Some(_) => Ok(match self.store.trust_new_identities {
                OnNewIdentity::Trust => true,
                OnNewIdentity::Reject => false,
            }),
            // Never seen: trusted on first use, as every Signal client does.
            None => {
                warn!(%address, "trusting a new identity");
                Ok(true)
            }
        }
    }

    async fn get_identity(&self, address: &ProtocolAddress) -> ProtocolResult<Option<IdentityKey>> {
        let name = address.name();
        let kind = self.identity.as_str();
        let record: Option<Vec<u8>> = self.store.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT record FROM identities WHERE address = ?1 AND identity = ?2",
                    params![name, kind],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        record.map(|bytes| IdentityKey::decode(&bytes)).transpose()
    }
}

#[async_trait(?Send)]
impl SenderKeyStore for SignalProtocolStore {
    async fn store_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        record: &SenderKeyRecord,
    ) -> ProtocolResult<()> {
        let name = sender.name();
        let device_id: u8 = sender.device_id().into();
        let identity = self.identity.as_str();
        let record = record.serialize()?;
        let distribution_id = distribution_id.as_bytes().to_vec();
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO sender_keys (address, device_id, identity, distribution_id, record)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (address, device_id, identity, distribution_id) DO UPDATE SET record = excluded.record",
                params![name, device_id, identity, distribution_id, record],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    async fn load_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
    ) -> ProtocolResult<Option<SenderKeyRecord>> {
        let name = sender.name();
        let device_id: u8 = sender.device_id().into();
        let identity = self.identity.as_str();
        let distribution_id = distribution_id.as_bytes().to_vec();
        let record: Option<Vec<u8>> = self.store.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT record FROM sender_keys
                     WHERE address = ?1 AND device_id = ?2 AND identity = ?3 AND distribution_id = ?4",
                    params![name, device_id, identity, distribution_id],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        record.map(|bytes| SenderKeyRecord::deserialize(&bytes)).transpose()
    }
}
