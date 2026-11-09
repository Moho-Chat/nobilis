//! The tables, as the last version of presage's own SQLite store leaves them after its migrations (that store's history
//! of `ALTER TABLE`s is not repeated: a database made here starts at the end of it), with three differences, each for a
//! reason of its own:
//!
//! - `thread_messages.server_ts` is written (presage's store has the column and never fills it), so a message keeps the
//!   time the server stamped on it.
//! - `contacts_verification_state` is keyed by the contact's own id, held the same way in both tables, so the join that
//!   reads it back finds the row. (Presage stores the ACI there as text against a blob key.)
//! - nothing is `SQLCipher`: the file is private to its owner (see `SignalStore::open`) like the other credentials.
//!
//! `user_version` records which of these has been applied; a new step is added as a new constant and a new arm in
//! `migrate`, never an edit of an old one.

pub(super) const KNOWN_VERSION: i64 = 1;

pub(super) const V1: &str = "
CREATE TABLE kv (key TEXT PRIMARY KEY, value BLOB NOT NULL);

-- protocol: one set of tables, 'identity' says whether a row belongs to the account's ACI or its PNI
CREATE TABLE sessions (
  address TEXT NOT NULL,
  device_id INTEGER NOT NULL,
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  record BLOB NOT NULL,
  PRIMARY KEY (address, device_id, identity)
);

CREATE TABLE identities (
  address TEXT NOT NULL,
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  record BLOB NOT NULL,
  PRIMARY KEY (address, identity)
);

CREATE TABLE pre_keys (
  id INTEGER NOT NULL,
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  record BLOB NOT NULL,
  PRIMARY KEY (id, identity)
);

CREATE TABLE signed_pre_keys (
  id INTEGER NOT NULL,
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  record BLOB NOT NULL,
  PRIMARY KEY (id, identity)
);

CREATE TABLE kyber_pre_keys (
  id INTEGER NOT NULL,
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  record BLOB NOT NULL,
  is_last_resort INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id, identity)
);

CREATE TABLE base_keys_seen (
  kyber_pre_key_id INTEGER NOT NULL,
  signed_pre_key_id INTEGER NOT NULL,
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  base_key BLOB NOT NULL,
  PRIMARY KEY (identity, kyber_pre_key_id, signed_pre_key_id, base_key),
  FOREIGN KEY (kyber_pre_key_id, identity) REFERENCES kyber_pre_keys(id, identity) ON DELETE CASCADE,
  FOREIGN KEY (signed_pre_key_id, identity) REFERENCES signed_pre_keys(id, identity) ON DELETE CASCADE
);

CREATE TABLE sender_keys (
  address TEXT NOT NULL,
  device_id INTEGER NOT NULL,
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  distribution_id BLOB NOT NULL,
  record BLOB NOT NULL,
  PRIMARY KEY (address, device_id, identity, distribution_id)
);

-- content
CREATE TABLE contacts (
  uuid BLOB NOT NULL PRIMARY KEY,
  phone_number TEXT,
  name TEXT NOT NULL,
  profile_key BLOB NOT NULL,
  expire_timer INTEGER NOT NULL,
  expire_timer_version INTEGER NOT NULL DEFAULT 2,
  inbox_position INTEGER NOT NULL,
  avatar BLOB
);

CREATE TABLE contacts_verification_state (
  destination_aci BLOB NOT NULL PRIMARY KEY,
  identity_key BLOB NOT NULL,
  is_verified INTEGER,
  FOREIGN KEY (destination_aci) REFERENCES contacts (uuid) ON UPDATE CASCADE ON DELETE CASCADE
);

CREATE TABLE profile_keys (uuid BLOB NOT NULL PRIMARY KEY, key BLOB NOT NULL);

CREATE TABLE profiles (
  uuid BLOB NOT NULL PRIMARY KEY,
  given_name TEXT,
  family_name TEXT,
  about TEXT,
  about_emoji TEXT,
  avatar TEXT,
  unrestricted_unidentified_access INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (uuid) REFERENCES profile_keys (uuid) ON DELETE CASCADE
);

CREATE TABLE profile_avatars (
  uuid BLOB NOT NULL PRIMARY KEY,
  bytes BLOB NOT NULL,
  FOREIGN KEY (uuid) REFERENCES profile_keys (uuid) ON UPDATE CASCADE ON DELETE CASCADE
);

CREATE TABLE groups (
  master_key BLOB NOT NULL PRIMARY KEY,
  title TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 0,
  invite_link_password BLOB,
  access_control TEXT,
  avatar TEXT NOT NULL,
  description TEXT,
  members TEXT NOT NULL,
  pending_members TEXT NOT NULL,
  requesting_members TEXT NOT NULL,
  disappearing_messages_timer INTEGER
);

CREATE TABLE left_groups (master_key BLOB NOT NULL PRIMARY KEY);

CREATE TABLE group_avatars (
  group_master_key BLOB PRIMARY KEY,
  bytes BLOB NOT NULL,
  FOREIGN KEY (group_master_key) REFERENCES groups (master_key) ON DELETE CASCADE
);

CREATE TABLE threads (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  group_master_key BLOB UNIQUE,
  recipient_id BLOB UNIQUE
);

CREATE TABLE thread_messages (
  ts INTEGER NOT NULL,
  thread_id INTEGER NOT NULL,
  sender_service_id TEXT NOT NULL,
  sender_device_id INTEGER NOT NULL,
  destination_service_id TEXT NOT NULL,
  needs_receipt INTEGER NOT NULL,
  unidentified_sender INTEGER NOT NULL,
  content_body BLOB NOT NULL,
  was_plaintext INTEGER NOT NULL,
  server_ts INTEGER,
  pni_verified TEXT,
  PRIMARY KEY (ts, thread_id),
  FOREIGN KEY (thread_id) REFERENCES threads (id) ON UPDATE CASCADE
);

CREATE TABLE sticker_packs (
  id BLOB PRIMARY KEY NOT NULL,
  key BLOB NOT NULL,
  manifest TEXT NOT NULL
);
";
