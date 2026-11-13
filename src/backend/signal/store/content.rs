//! What the account has been told: who its contacts are, which groups it is in, the profiles it has fetched, the sticker
//! packs it has installed, and the messages. Keyed the way presage keys them - a message by its timestamp inside a
//! thread, a thread by the other person's id or the group's master key.

use super::data::{
    contact_from_row, group_from_row, message_from_row, profile_from_row, sticker_pack_from_row, uuid_from_blob,
    verified_flag, CONTACT_COLUMNS, CONTACT_FROM, GROUP_COLUMNS, MESSAGE_COLUMNS, PROFILE_COLUMNS,
};
use super::{Result, SignalStore, SignalStoreError};
use presage::{
    libsignal_service::{
        content::Metadata,
        prelude::{phonenumber, Content, ProfileKey, Uuid},
        protocol::ServiceId,
        zkgroup::GroupMasterKeyBytes,
        Profile,
    },
    model::{contacts::Contact, groups::Group},
    store::{ContentsStore, StickerPack, Thread},
    AvatarBytes,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::ops::{Bound, RangeBounds};

type Boxed<T> = Box<dyn Iterator<Item = Result<T>> + Send + Sync>;

/// One end of a range of time: the limit that excludes its own value and the one that includes it, whichever the range
/// has (neither, if it is open on that side).
type Limit = (Option<i64>, Option<i64>);

/// How a stored conversation is keyed: its group's master key, or the other person's id - one of the two.
type ThreadKey = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The id of the row that stands for a conversation, if it has one yet.
fn thread_id(conn: &Connection, thread: &Thread) -> Result<Option<i64>> {
    let found = match thread {
        Thread::Contact(service_id) => conn
            .query_row(
                "SELECT id FROM threads WHERE recipient_id = ?1",
                [service_id.raw_uuid().as_bytes().to_vec()],
                |row| row.get(0),
            )
            .optional()?,
        Thread::Group(master_key) => conn
            .query_row("SELECT id FROM threads WHERE group_master_key = ?1", [master_key.as_slice()], |row| row.get(0))
            .optional()?,
    };
    Ok(found)
}

impl SignalStore {
    /// The people the account has a one-to-one conversation with that still holds a message (the account itself among
    /// them, for Note to Self). Signal keeps no list of conversations on its server; this is what there is of one.
    pub fn people_with_messages(&self) -> Result<Vec<uuid::Uuid>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.recipient_id FROM threads t
                 WHERE t.recipient_id IS NOT NULL
                   AND EXISTS (SELECT 1 FROM thread_messages m WHERE m.thread_id = t.id)",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            Ok(rows.filter_map(|r| r.ok()).filter_map(|bytes| uuid::Uuid::from_slice(&bytes).ok()).collect())
        })
    }
}

/// The id of the row for a conversation, made if it is the first time it is spoken of.
fn ensure_thread_id(conn: &Connection, thread: &Thread) -> Result<i64> {
    Ok(match thread {
        Thread::Contact(service_id) => conn.query_row(
            "INSERT INTO threads (recipient_id, group_master_key) VALUES (?1, NULL)
             ON CONFLICT (recipient_id) DO UPDATE SET recipient_id = excluded.recipient_id RETURNING id",
            [service_id.raw_uuid().as_bytes().to_vec()],
            |row| row.get(0),
        )?,
        Thread::Group(master_key) => conn.query_row(
            "INSERT INTO threads (recipient_id, group_master_key) VALUES (NULL, ?1)
             ON CONFLICT (group_master_key) DO UPDATE SET group_master_key = excluded.group_master_key RETURNING id",
            [master_key.as_slice()],
            |row| row.get(0),
        )?,
    })
}

/// The two ends of a range as the pair of `(strictly after, from)` / `(strictly before, up to)` limits the query uses,
/// each absent when the range is open on that side.
fn bounds(range: &impl RangeBounds<u64>) -> (Limit, Limit) {
    fn split(bound: Bound<&u64>) -> Limit {
        match bound {
            Bound::Excluded(x) => (Some(*x as i64), None),
            Bound::Included(x) => (None, Some(*x as i64)),
            Bound::Unbounded => (None, None),
        }
    }
    (split(range.start_bound()), split(range.end_bound()))
}

impl ContentsStore for SignalStore {
    type ContentsStoreError = SignalStoreError;
    type ContactsIter = Boxed<Contact>;
    type GroupsIter = Boxed<(GroupMasterKeyBytes, Group)>;
    type MessagesIter = Boxed<Content>;
    type StickerPacksIter = Boxed<StickerPack>;

    async fn clear_profiles(&mut self) -> Result<()> {
        self.write(|tx| {
            for table in ["profile_avatars", "profiles", "profile_keys"] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            Ok(())
        })
    }

    async fn clear_contents(&mut self) -> Result<()> {
        self.write(|tx| {
            for table in [
                "thread_messages",
                "threads",
                "contacts_verification_state",
                "contacts",
                "group_avatars",
                "groups",
                "sticker_packs",
            ] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            Ok(())
        })
    }

    async fn clear_messages(&mut self) -> Result<()> {
        self.write(|tx| {
            tx.execute("DELETE FROM thread_messages", [])?;
            tx.execute("DELETE FROM threads", [])?;
            Ok(())
        })
    }

    async fn clear_thread(&mut self, thread: &Thread) -> Result<()> {
        self.write(|tx| {
            if let Some(id) = thread_id(tx, thread)? {
                tx.execute("DELETE FROM thread_messages WHERE thread_id = ?1", [id])?;
            }
            Ok(())
        })
    }

    async fn save_message(&self, thread: &Thread, Content { metadata, body }: Content) -> Result<()> {
        let Metadata {
            sender,
            destination,
            sender_device,
            client_timestamp,
            server_timestamp,
            needs_receipt,
            unidentified_sender,
            server_guid: _,
            was_plaintext,
            pni_verified,
        } = metadata;
        let sender_device: u8 = sender_device.into();
        let sender = sender.service_id_string();
        let destination = destination.service_id_string();
        let proto_bytes = prost::Message::encode_to_vec(&body.into_proto());
        let ts = client_timestamp.timestamp_millis();
        let server_ts = server_timestamp.timestamp_millis();
        let pni_verified = pni_verified.map(|p| p.service_id_string());
        self.write(|tx| {
            let thread_id = ensure_thread_id(tx, thread)?;
            tx.execute(
                "INSERT OR REPLACE INTO thread_messages (
                    ts, thread_id, sender_service_id, sender_device_id, destination_service_id, needs_receipt,
                    unidentified_sender, content_body, was_plaintext, server_ts, pni_verified
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    ts,
                    thread_id,
                    sender,
                    sender_device,
                    destination,
                    needs_receipt,
                    unidentified_sender,
                    proto_bytes,
                    was_plaintext,
                    server_ts,
                    pni_verified
                ],
            )?;
            Ok(())
        })
    }

    async fn delete_message(&mut self, thread: &Thread, timestamp: u64) -> Result<bool> {
        let ts: i64 = timestamp.try_into().map_err(|_| SignalStoreError::InvalidFormat)?;
        self.write(|tx| {
            let Some(id) = thread_id(tx, thread)? else { return Ok(false) };
            Ok(tx.execute("DELETE FROM thread_messages WHERE ts = ?1 AND thread_id = ?2", params![ts, id])? > 0)
        })
    }

    async fn message(&self, thread: &Thread, timestamp: u64) -> Result<Option<Content>> {
        let ts: i64 = timestamp.try_into().map_err(|_| SignalStoreError::InvalidFormat)?;
        self.read(|conn| {
            let Some(id) = thread_id(conn, thread)? else { return Ok(None) };
            conn.query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM thread_messages WHERE ts = ?1 AND thread_id = ?2"),
                params![ts, id],
                |row| Ok(message_from_row(row)),
            )
            .optional()?
            .transpose()
        })
    }

    /// The messages of a conversation in a range of time, newest first.
    async fn messages(&self, thread: &Thread, range: impl RangeBounds<u64>) -> Result<Self::MessagesIter> {
        let ((after, from), (before, up_to)) = bounds(&range);
        let messages = self.read(|conn| {
            let Some(id) = thread_id(conn, thread)? else { return Ok(Vec::new()) };
            let mut stmt = conn.prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM thread_messages
                 WHERE thread_id = ?1
                   AND (?2 IS NULL OR ts > ?2) AND (?3 IS NULL OR ts >= ?3)
                   AND (?4 IS NULL OR ts < ?4) AND (?5 IS NULL OR ts <= ?5)
                 ORDER BY ts DESC"
            ))?;
            let rows = stmt.query_map(params![id, after, from, before, up_to], |row| Ok(message_from_row(row)))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        Ok(Box::new(messages.into_iter()))
    }

    async fn thread_for_sender_and_timestamp(&self, sender: &ServiceId, timestamp: u64) -> Result<Option<Thread>> {
        let ts: i64 = timestamp.try_into().map_err(|_| SignalStoreError::InvalidFormat)?;
        let sender = sender.service_id_string();
        let found: Option<ThreadKey> = self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT t.group_master_key, t.recipient_id FROM threads t
                     JOIN thread_messages m ON t.id = m.thread_id
                     WHERE m.sender_service_id = ?1 AND m.ts = ?2",
                    params![sender, ts],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?)
        })?;
        match found {
            None => Ok(None),
            Some((Some(master_key), None)) => Ok(master_key.try_into().ok().map(Thread::Group)),
            Some((None, Some(recipient))) => Ok(Some(Thread::Contact(ServiceId::Aci(uuid_from_blob(&recipient)?.into())))),
            Some(_) => Err(SignalStoreError::InvalidFormat),
        }
    }

    async fn clear_contacts(&mut self) -> Result<()> {
        self.write(|tx| {
            tx.execute("DELETE FROM contacts_verification_state", [])?;
            tx.execute("DELETE FROM contacts", [])?;
            Ok(())
        })
    }

    async fn save_contact(&mut self, contact: &Contact) -> Result<()> {
        let uuid = contact.uuid.as_bytes().to_vec();
        let phone_number = contact.phone_number.as_ref().map(|p| p.format().mode(phonenumber::Mode::E164).to_string());
        let avatar = contact.avatar.as_ref().map(|a| a.reader.to_vec());
        let verification = contact
            .verified
            .identity_key
            .as_ref()
            .filter(|_| contact.verified.destination_aci.is_some() || contact.verified.destination_aci_binary.is_some())
            .map(|key| (key.clone(), verified_flag(&contact.verified)));
        self.write(|tx| {
            // An upsert and not `INSERT OR REPLACE`: a replace deletes the old row first, and the verification kept
            // under it would go with it.
            tx.execute(
                "INSERT INTO contacts
                    (uuid, phone_number, name, profile_key, expire_timer, expire_timer_version, inbox_position, avatar)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (uuid) DO UPDATE SET
                    phone_number = excluded.phone_number, name = excluded.name, profile_key = excluded.profile_key,
                    expire_timer = excluded.expire_timer, expire_timer_version = excluded.expire_timer_version,
                    inbox_position = excluded.inbox_position, avatar = excluded.avatar",
                params![
                    uuid,
                    phone_number,
                    contact.name,
                    contact.profile_key,
                    contact.expire_timer,
                    contact.expire_timer_version,
                    contact.inbox_position,
                    avatar
                ],
            )?;
            if let Some((identity_key, is_verified)) = verification {
                tx.execute(
                    "INSERT OR REPLACE INTO contacts_verification_state (destination_aci, identity_key, is_verified)
                     VALUES (?1, ?2, ?3)",
                    params![uuid, identity_key, is_verified],
                )?;
            }
            Ok(())
        })
    }

    async fn contacts(&self) -> Result<Self::ContactsIter> {
        let contacts = self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!("SELECT {CONTACT_COLUMNS} FROM {CONTACT_FROM} ORDER BY c.inbox_position"))?;
            let rows = stmt.query_map([], |row| Ok(contact_from_row(row)))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        Ok(Box::new(contacts.into_iter()))
    }

    async fn contact_by_id(&self, id: &ServiceId) -> Result<Option<Contact>> {
        let uuid = id.raw_uuid().as_bytes().to_vec();
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {CONTACT_COLUMNS} FROM {CONTACT_FROM} WHERE c.uuid = ?1"),
                [uuid],
                |row| Ok(contact_from_row(row)),
            )
            .optional()?
            .transpose()
        })
    }

    async fn clear_groups(&mut self) -> Result<()> {
        self.write(|tx| {
            for table in ["group_avatars", "left_groups", "groups"] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            Ok(())
        })
    }

    /// Saves a group, and with it forgets any mark that the account had left it.
    async fn save_group(&self, master_key: GroupMasterKeyBytes, group: impl Into<Group>) -> Result<()> {
        let group: Group = group.into();
        let access_control = group.access_control.as_ref().map(serde_json::to_string).transpose()?;
        let members = serde_json::to_string(&group.members)?;
        let pending = serde_json::to_string(&group.pending_members)?;
        let requesting = serde_json::to_string(&group.requesting_members)?;
        let timer = group.disappearing_messages_timer.map(|t| t.duration as i64);
        self.write(|tx| {
            // An upsert, so the avatar kept for the group (which goes with the group row) survives a refresh of it.
            tx.execute(
                "INSERT INTO groups (master_key, title, revision, invite_link_password, access_control, avatar,
                    description, members, pending_members, requesting_members, disappearing_messages_timer)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (master_key) DO UPDATE SET
                    title = excluded.title, revision = excluded.revision,
                    invite_link_password = excluded.invite_link_password, access_control = excluded.access_control,
                    avatar = excluded.avatar, description = excluded.description, members = excluded.members,
                    pending_members = excluded.pending_members, requesting_members = excluded.requesting_members,
                    disappearing_messages_timer = excluded.disappearing_messages_timer",
                params![
                    master_key.as_slice(),
                    group.title,
                    group.revision,
                    group.invite_link_password,
                    access_control,
                    group.avatar,
                    group.description,
                    members,
                    pending,
                    requesting,
                    timer
                ],
            )?;
            tx.execute("DELETE FROM left_groups WHERE master_key = ?1", [master_key.as_slice()])?;
            Ok(())
        })
    }

    async fn groups(&self) -> Result<Self::GroupsIter> {
        let groups = self.read(|conn| {
            let mut stmt = conn.prepare(&format!("SELECT {GROUP_COLUMNS} FROM groups"))?;
            let rows = stmt.query_map([], |row| Ok(group_from_row(row)))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        Ok(Box::new(groups.into_iter()))
    }

    async fn group(&self, master_key: GroupMasterKeyBytes) -> Result<Option<Group>> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {GROUP_COLUMNS} FROM groups WHERE master_key = ?1"),
                [master_key.as_slice()],
                |row| Ok(group_from_row(row)),
            )
            .optional()?
            .transpose()
        })
        .map(|found| found.map(|(_key, group)| group))
    }

    async fn mark_group_left(&self, master_key: GroupMasterKeyBytes) -> Result<()> {
        self.write(|tx| {
            tx.execute("INSERT OR IGNORE INTO left_groups (master_key) VALUES (?1)", [master_key.as_slice()])?;
            Ok(())
        })
    }

    async fn is_group_left(&self, master_key: GroupMasterKeyBytes) -> Result<bool> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM left_groups WHERE master_key = ?1)",
                [master_key.as_slice()],
                |row| row.get(0),
            )?)
        })
    }

    async fn save_group_avatar(&self, master_key: GroupMasterKeyBytes, avatar: &AvatarBytes) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO group_avatars (group_master_key, bytes) VALUES (?1, ?2)",
                params![master_key.as_slice(), avatar],
            )?;
            Ok(())
        })
    }

    async fn group_avatar(&self, master_key: GroupMasterKeyBytes) -> Result<Option<AvatarBytes>> {
        self.read(|conn| {
            Ok(conn
                .query_row("SELECT bytes FROM group_avatars WHERE group_master_key = ?1", [master_key.as_slice()], |row| {
                    row.get(0)
                })
                .optional()?)
        })
    }

    /// Records the key a contact's profile is encrypted with. A key that is not the one on record means the profile
    /// kept for them was made with a key that is gone, and it is dropped.
    async fn upsert_profile_key(&mut self, uuid: &Uuid, key: ProfileKey) -> Result<bool> {
        let uuid = uuid.as_bytes().to_vec();
        let key = key.bytes.to_vec();
        self.write(|tx| {
            let existing: Option<Vec<u8>> = tx
                .query_row("SELECT key FROM profile_keys WHERE uuid = ?1", [&uuid], |row| row.get(0))
                .optional()?;
            let changed = existing.as_ref() != Some(&key);
            if existing.is_some() && changed {
                tx.execute("DELETE FROM profiles WHERE uuid = ?1", [&uuid])?;
                tx.execute("DELETE FROM profile_avatars WHERE uuid = ?1", [&uuid])?;
            }
            tx.execute(
                "INSERT INTO profile_keys (uuid, key) VALUES (?1, ?2)
                 ON CONFLICT (uuid) DO UPDATE SET key = excluded.key",
                params![uuid, key],
            )?;
            Ok(changed)
        })
    }

    async fn profile_key(&self, service_id: &ServiceId) -> Result<Option<ProfileKey>> {
        let uuid = service_id.raw_uuid().as_bytes().to_vec();
        let bytes: Option<Vec<u8>> = self.read(|conn| {
            Ok(conn.query_row("SELECT key FROM profile_keys WHERE uuid = ?1", [uuid], |row| row.get(0)).optional()?)
        })?;
        Ok(bytes.and_then(|bytes| bytes.try_into().ok().map(ProfileKey::create)))
    }

    async fn save_profile(&mut self, uuid: Uuid, key: ProfileKey, profile: Profile) -> Result<()> {
        self.upsert_profile_key(&uuid, key).await?;
        let Profile { name, about, about_emoji, avatar, unrestricted_unidentified_access } = profile;
        let (given_name, family_name) = name.map(|n| (n.given_name, n.family_name)).unzip();
        let family_name = family_name.flatten();
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO profiles
                    (uuid, given_name, family_name, about, about_emoji, avatar, unrestricted_unidentified_access)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    uuid.as_bytes().to_vec(),
                    given_name,
                    family_name,
                    about,
                    about_emoji,
                    avatar,
                    unrestricted_unidentified_access
                ],
            )?;
            Ok(())
        })
    }

    async fn profile(&self, uuid: Uuid, key: ProfileKey) -> Result<Option<Profile>> {
        self.read(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {PROFILE_COLUMNS} FROM profile_keys pk JOIN profiles p ON p.uuid = pk.uuid
                     WHERE pk.uuid = ?1 AND pk.key = ?2"
                ),
                params![uuid.as_bytes().to_vec(), key.bytes.to_vec()],
                |row| Ok(profile_from_row(row)),
            )
            .optional()?
            .transpose()
        })
    }

    async fn save_profile_avatar(&mut self, uuid: Uuid, _key: ProfileKey, profile: &AvatarBytes) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO profile_avatars (uuid, bytes) VALUES (?1, ?2)",
                params![uuid.as_bytes().to_vec(), profile],
            )?;
            Ok(())
        })
    }

    async fn profile_avatar(&self, uuid: Uuid, _key: ProfileKey) -> Result<Option<AvatarBytes>> {
        self.read(|conn| {
            Ok(conn
                .query_row("SELECT bytes FROM profile_avatars WHERE uuid = ?1", [uuid.as_bytes().to_vec()], |row| {
                    row.get(0)
                })
                .optional()?)
        })
    }

    async fn add_sticker_pack(&mut self, pack: &StickerPack) -> Result<()> {
        let manifest = serde_json::to_string(&pack.manifest)?;
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO sticker_packs (id, key, manifest) VALUES (?1, ?2, ?3)",
                params![pack.id, pack.key, manifest],
            )?;
            Ok(())
        })
    }

    async fn sticker_pack(&self, id: &[u8]) -> Result<Option<StickerPack>> {
        self.read(|conn| {
            conn.query_row("SELECT id, key, manifest FROM sticker_packs WHERE id = ?1", [id], |row| {
                Ok(sticker_pack_from_row(row))
            })
            .optional()?
            .transpose()
        })
    }

    async fn remove_sticker_pack(&mut self, id: &[u8]) -> Result<bool> {
        self.write(|tx| Ok(tx.execute("DELETE FROM sticker_packs WHERE id = ?1", [id])? > 0))
    }

    async fn sticker_packs(&self) -> Result<Self::StickerPacksIter> {
        let packs = self.read(|conn| {
            let mut stmt = conn.prepare("SELECT id, key, manifest FROM sticker_packs")?;
            let rows = stmt.query_map([], |row| Ok(sticker_pack_from_row(row)))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        Ok(Box::new(packs.into_iter()))
    }
}
