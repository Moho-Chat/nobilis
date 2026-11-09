//! Where a linked Signal account keeps what it must not lose: its identity and pre-keys, one ratchet session per device
//! of everyone it talks to, the sender keys of the groups, and the contacts, groups, profiles and messages it has been
//! told about. It implements presage's `Store`, so the library can run on it.
//!
//! Presage ships a SQLite store of its own. It cannot be used here: it needs `libsqlite3-sys` 0.36 built with SQLCipher
//! and this daemon's rusqlite brings 0.38, and cargo links one copy of a native library. This is the same store, written
//! over rusqlite - the schema is presage's (see `schema`), the statements are the same, and the behaviour is the
//! same except where a comment says it is not.
//!
//! ## What the store is, and is not
//!
//! - **One file per account**, in the daemon's data directory, private to its owner. The keys in it *are* the account's
//!   device: whoever can read the file can read the account's messages from now on, which is why it is created with the
//!   mode set at creation and the directory closed to others (see `crate::secure`). It is not encrypted otherwise,
//!   like the tokens in `accounts.toml`.
//! - **Not the shared scrollback.** Signal messages are kept here and nowhere else, so that a disappearing message can
//!   be removed in one place and nothing readable is left beside other services' history.
//! - **Durable before acknowledged.** `synchronous = FULL`: a ratchet that advanced and was not written would make the
//!   next message from that person undecryptable, so a write that returned has reached the disk.
//! - **One connection behind a mutex.** SQLite work here is short and local; no `await` is held across the lock, so a
//!   `std` mutex is enough and writes cannot interleave.

mod content;
mod data;
mod error;
mod protocol;
mod schema;
#[cfg(test)]
mod tests;

pub use error::SignalStoreError;
pub use presage::model::identity::OnNewIdentity;
pub use protocol::SignalProtocolStore;

use anyhow::Context;
use protocol::IdentityType;
use presage::{
    libsignal_service::{
        libsignal_account_keys::AccountEntropyPool,
        prelude::MasterKey,
        protocol::{IdentityKeyPair, SenderCertificate},
    },
    manager::RegistrationData,
    store::{StateStore, Store},
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

type Result<T> = std::result::Result<T, SignalStoreError>;

const REGISTRATION: &str = "registration";
const SENDER_CERTIFICATE: &str = "sender_certificate";
const MASTER_KEY: &str = "master_key";
const ACCOUNT_ENTROPY_POOL: &str = "account_entropy_pool";

/// Where Signal accounts' files live.
pub fn folder() -> PathBuf {
    crate::default_data_dir().join("signal")
}

/// An account's store.
pub fn path_for(aci: &str) -> PathBuf {
    let safe: String = aci.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    folder().join(format!("{safe}.db"))
}

/// A link that has not got as far as having an account yet keeps its store here until it has (the name is not known
/// until the phone has answered), and is renamed afterwards.
pub fn path_for_link(link_id: &str) -> PathBuf {
    let safe: String = link_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    folder().join(format!("link-{safe}.db"))
}

/// Gives a link's store the name of the account it turned out to be. Every handle on the store must be dropped first;
/// what the new name replaces (an earlier link of the same account) is gone with the keys it held, which the account
/// no longer has anyway.
pub fn relocate(from: &Path, to: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        let mut stale = to.as_os_str().to_owned();
        stale.push(suffix);
        let _ = std::fs::remove_file(stale);
    }
    std::fs::rename(from, to)?;
    for suffix in ["-wal", "-shm"] {
        let mut old = from.as_os_str().to_owned();
        old.push(suffix);
        let _ = std::fs::remove_file(old);
    }
    crate::secure::restrict_file_to_owner(to)?;
    Ok(())
}

/// Deletes an account's store and the files SQLite keeps beside it.
pub fn delete_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
}

#[derive(Clone)]
pub struct SignalStore {
    db: Arc<Mutex<Connection>>,
    pub(crate) trust_new_identities: OnNewIdentity,
}

impl std::fmt::Debug for SignalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nothing in here belongs in a log line.
        f.debug_struct("SignalStore").finish_non_exhaustive()
    }
}

impl SignalStore {
    /// Opens the store at `path`, making it (and its folder) if there is none. The file is created private; one that
    /// already exists has its mode made private again.
    pub fn open(path: &Path, trust_new_identities: OnNewIdentity) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            crate::secure::restrict_to_owner(dir)?;
        }
        if path.exists() {
            crate::secure::restrict_file_to_owner(path)?;
        } else {
            // An empty file is an empty database. Created with its mode set, so the keys that follow are never in a
            // file anybody else could open.
            crate::secure::create_private_file(path).with_context(|| "creating the Signal store")?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn, trust_new_identities)
    }

    /// A store that lives in memory and is gone when it is dropped. For tests.
    pub fn open_in_memory(trust_new_identities: OnNewIdentity) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, trust_new_identities)
    }

    fn init(conn: Connection, trust_new_identities: OnNewIdentity) -> Result<Self> {
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { db: Arc::new(Mutex::new(conn)), trust_new_identities };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.lock();
        let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > schema::KNOWN_VERSION {
            // Never open what a newer moho wrote: reading it with older rules and writing back would damage it.
            return Err(SignalStoreError::NewerSchema { found, known: schema::KNOWN_VERSION });
        }
        if found < 1 {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(schema::V1)?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// A small named value the client keeps for itself (settings the phone synced, say), apart from the library's data.
    pub fn blob(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.read(|conn| kv_get(conn, &format!("moho:{name}")))
    }

    pub fn set_blob(&self, name: &str, value: &[u8]) -> Result<()> {
        self.write(|tx| kv_set(tx, &format!("moho:{name}"), value))
    }

    /// Moves everything the write-ahead log holds into the main file, so the file alone is the store and can be renamed.
    pub fn checkpoint(&self) -> Result<()> {
        self.read(|conn| {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            Ok(())
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while the lock was held leaves SQLite itself consistent (it rolls the open transaction back), so the
        // connection is still good to use.
        self.db.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs a read.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.lock())
    }

    /// Runs a write in a transaction that takes the write lock at once (a deferred one that read before it wrote could
    /// fail with `SQLITE_BUSY` where no timeout helps).
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

fn kv_get(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| row.get(0)).optional()?)
}

fn kv_set(conn: &Connection, key: &str, value: &[u8]) -> Result<()> {
    conn.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key, value])?;
    Ok(())
}

fn kv_delete(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
    Ok(())
}

/// Every table that holds the account's protocol state: gone when the device is unlinked, since none of it is any use
/// to a device that no longer exists.
const PROTOCOL_TABLES: [&str; 7] =
    ["sessions", "identities", "pre_keys", "signed_pre_keys", "kyber_pre_keys", "base_keys_seen", "sender_keys"];

/// Everything that is about people and what was said, each table after the ones that point at it.
const CONTENT_TABLES: [&str; 11] = [
    "thread_messages",
    "threads",
    "contacts_verification_state",
    "contacts",
    "profile_avatars",
    "profiles",
    "profile_keys",
    "group_avatars",
    "left_groups",
    "groups",
    "sticker_packs",
];

impl Store for SignalStore {
    type Error = SignalStoreError;
    type AciStore = SignalProtocolStore;
    type PniStore = SignalProtocolStore;

    /// Empties the whole store: for resetting a client that is about to be linked again.
    async fn clear(&mut self) -> Result<()> {
        self.write(|tx| {
            for table in PROTOCOL_TABLES.iter().chain(CONTENT_TABLES.iter()) {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            tx.execute("DELETE FROM kv", [])?;
            Ok(())
        })
    }

    fn aci_protocol_store(&self) -> Self::AciStore {
        SignalProtocolStore { store: self.clone(), identity: IdentityType::Aci }
    }

    fn pni_protocol_store(&self) -> Self::PniStore {
        SignalProtocolStore { store: self.clone(), identity: IdentityType::Pni }
    }
}

impl StateStore for SignalStore {
    type StateStoreError = SignalStoreError;

    async fn load_registration_data(&self) -> Result<Option<RegistrationData>> {
        self.read(|conn| kv_get(conn, REGISTRATION))?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()
            .map_err(From::from)
    }

    async fn save_registration_data(&mut self, state: &RegistrationData) -> Result<()> {
        let value = serde_json::to_vec(state)?;
        self.write(|tx| kv_set(tx, REGISTRATION, &value))
    }

    async fn is_registered(&self) -> bool {
        self.load_registration_data().await.ok().flatten().is_some()
    }

    /// Forgets that this is a linked device and every key it had, and keeps what people said and who they are.
    async fn clear_registration(&mut self) -> Result<()> {
        self.write(|tx| {
            for key in [
                REGISTRATION,
                IdentityType::Aci.identity_key_pair_key(),
                IdentityType::Pni.identity_key_pair_key(),
                SENDER_CERTIFICATE,
                MASTER_KEY,
                ACCOUNT_ENTROPY_POOL,
            ] {
                kv_delete(tx, key)?;
            }
            for table in PROTOCOL_TABLES {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            Ok(())
        })
    }

    async fn set_aci_identity_key_pair(&self, key_pair: IdentityKeyPair) -> Result<()> {
        let value = key_pair.serialize();
        self.write(|tx| kv_set(tx, IdentityType::Aci.identity_key_pair_key(), &value))
    }

    async fn set_pni_identity_key_pair(&self, key_pair: IdentityKeyPair) -> Result<()> {
        let value = key_pair.serialize();
        self.write(|tx| kv_set(tx, IdentityType::Pni.identity_key_pair_key(), &value))
    }

    async fn sender_certificate(&self) -> Result<Option<SenderCertificate>> {
        self.read(|conn| kv_get(conn, SENDER_CERTIFICATE))?
            .map(|bytes| SenderCertificate::deserialize(&bytes))
            .transpose()
            .map_err(From::from)
    }

    async fn save_sender_certificate(&self, certificate: &SenderCertificate) -> Result<()> {
        let value = certificate.serialized()?.to_vec();
        self.write(|tx| kv_set(tx, SENDER_CERTIFICATE, &value))
    }

    async fn fetch_master_key(&self) -> Result<Option<MasterKey>> {
        self.read(|conn| kv_get(conn, MASTER_KEY))?
            .map(|bytes| MasterKey::from_slice(&bytes))
            .transpose()
            .map_err(|_| SignalStoreError::InvalidFormat)
    }

    async fn store_master_key(&self, master_key: Option<&MasterKey>) -> Result<()> {
        self.write(|tx| match master_key {
            Some(key) => kv_set(tx, MASTER_KEY, &key.inner[..]),
            None => kv_delete(tx, MASTER_KEY),
        })
    }

    async fn fetch_account_entropy_pool(&self) -> Result<Option<AccountEntropyPool>> {
        self.read(|conn| kv_get(conn, ACCOUNT_ENTROPY_POOL))?
            .map(|bytes| {
                let text = std::str::from_utf8(&bytes).map_err(|_| SignalStoreError::InvalidFormat)?;
                AccountEntropyPool::from_str(text).map_err(|_| SignalStoreError::InvalidFormat)
            })
            .transpose()
    }

    async fn store_account_entropy_pool(&self, aep: Option<&AccountEntropyPool>) -> Result<()> {
        self.write(|tx| match aep {
            Some(pool) => kv_set(tx, ACCOUNT_ENTROPY_POOL, pool.to_string().as_bytes()),
            None => kv_delete(tx, ACCOUNT_ENTROPY_POOL),
        })
    }
}
