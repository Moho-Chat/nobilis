//! Rows to the library's types and back.

use super::{Result, SignalStoreError};
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use presage::{
    libsignal_service::{
        content::Metadata,
        models::Attachment,
        prelude::{phonenumber, Content, Timer, Uuid},
        profile_name::ProfileName,
        protocol::{Aci, Pni, ServiceId},
        zkgroup::GroupMasterKeyBytes,
        Profile,
    },
    model::{contacts::Contact, groups::Group},
    proto::{self, verified, Verified},
    store::{StickerPack, StickerPackManifest},
};
use rusqlite::Row;

/// A uuid as it is kept in a column: its sixteen bytes.
pub(super) fn uuid_from_blob(bytes: &[u8]) -> Result<Uuid> {
    Uuid::from_slice(bytes).map_err(|_| SignalStoreError::InvalidFormat)
}

/// The columns of a contact joined to its verification state, in the order `CONTACT_COLUMNS` names them.
pub(super) const CONTACT_COLUMNS: &str = "c.uuid, c.phone_number, c.name, c.profile_key, c.expire_timer, \
     c.expire_timer_version, c.inbox_position, c.avatar, cv.identity_key, cv.is_verified";

pub(super) const CONTACT_FROM: &str =
    "contacts c LEFT JOIN contacts_verification_state cv ON c.uuid = cv.destination_aci";

pub(super) fn contact_from_row(row: &Row<'_>) -> Result<Contact> {
    let uuid = uuid_from_blob(&row.get::<_, Vec<u8>>(0)?)?;
    let phone_number: Option<String> = row.get(1)?;
    let identity_key: Option<Vec<u8>> = row.get(8)?;
    let is_verified: Option<bool> = row.get(9)?;
    let verified = match identity_key {
        Some(identity_key) => {
            let aci = Aci::from(uuid);
            Verified {
                destination_aci: Some(uuid.to_string()),
                destination_aci_binary: Some(aci.service_id_binary()),
                identity_key: Some(identity_key),
                state: is_verified.map(|v| {
                    match v {
                        true => verified::State::Verified,
                        false => verified::State::Unverified,
                    }
                    .into()
                }),
                null_message: None,
            }
        }
        None => Verified::default(),
    };
    Ok(Contact {
        uuid,
        phone_number: phone_number.map(|p| phonenumber::parse(None, &p)).transpose()?,
        name: row.get(2)?,
        verified,
        profile_key: row.get(3)?,
        expire_timer: row.get::<_, i64>(4)? as u32,
        expire_timer_version: row.get::<_, i64>(5)? as u32,
        inbox_position: row.get::<_, i64>(6)? as u32,
        avatar: row
            .get::<_, Option<Vec<u8>>>(7)?
            .map(|b| Attachment { content_type: "application/octet-stream".to_owned(), reader: Bytes::from(b) }),
    })
}

/// Whether a contact's verification state is "verified", "unverified", or not said.
pub(super) fn verified_flag(verified: &Verified) -> Option<bool> {
    match verified::State::try_from(verified.state.unwrap_or_default()) {
        Err(_) | Ok(verified::State::Default) => None,
        Ok(verified::State::Unverified) => Some(false),
        Ok(verified::State::Verified) => Some(true),
    }
}

pub(super) const GROUP_COLUMNS: &str = "master_key, title, revision, invite_link_password, access_control, avatar, \
     description, members, pending_members, requesting_members, disappearing_messages_timer";

pub(super) fn group_from_row(row: &Row<'_>) -> Result<(GroupMasterKeyBytes, Group)> {
    let master_key: Vec<u8> = row.get(0)?;
    let master_key: GroupMasterKeyBytes =
        master_key.as_slice().try_into().map_err(|_| SignalStoreError::InvalidFormat)?;
    let access_control: Option<String> = row.get(4)?;
    let members: String = row.get(7)?;
    let pending_members: String = row.get(8)?;
    let requesting_members: String = row.get(9)?;
    let group = Group {
        title: row.get(1)?,
        revision: row.get::<_, i64>(2)? as u32,
        invite_link_password: row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
        access_control: access_control.map(|s| serde_json::from_str(&s)).transpose()?,
        avatar: row.get(5)?,
        description: row.get(6)?,
        members: serde_json::from_str(&members)?,
        pending_members: serde_json::from_str(&pending_members)?,
        requesting_members: serde_json::from_str(&requesting_members)?,
        disappearing_messages_timer: row.get::<_, Option<i64>>(10)?.map(|t| Timer { duration: t as u32 }),
    };
    Ok((master_key, group))
}

pub(super) const PROFILE_COLUMNS: &str =
    "p.given_name, p.family_name, p.about, p.about_emoji, p.avatar, p.unrestricted_unidentified_access";

pub(super) fn profile_from_row(row: &Row<'_>) -> Result<Profile> {
    let given_name: Option<String> = row.get(0)?;
    let family_name: Option<String> = row.get(1)?;
    Ok(Profile {
        name: given_name.map(|given_name| ProfileName { given_name, family_name }),
        about: row.get(2)?,
        about_emoji: row.get(3)?,
        avatar: row.get(4)?,
        unrestricted_unidentified_access: row.get(5)?,
    })
}

pub(super) const MESSAGE_COLUMNS: &str = "ts, server_ts, sender_service_id, sender_device_id, destination_service_id, \
     needs_receipt, unidentified_sender, content_body, was_plaintext, pni_verified";

pub(super) fn message_from_row(row: &Row<'_>) -> Result<Content> {
    let ts: i64 = row.get(0)?;
    let server_ts: Option<i64> = row.get(1)?;
    let sender_service_id: String = row.get(2)?;
    let sender_device_id: u8 = row.get(3)?;
    let destination_service_id: String = row.get(4)?;
    let content_body: Vec<u8> = row.get(7)?;
    let pni_verified: Option<String> = row.get(9)?;

    let body: proto::Content = prost::Message::decode(&*content_body).map_err(|_| SignalStoreError::InvalidFormat)?;
    let metadata = Metadata {
        sender: ServiceId::parse_from_service_id_string(&sender_service_id).ok_or(SignalStoreError::InvalidFormat)?,
        destination: ServiceId::parse_from_service_id_string(&destination_service_id)
            .ok_or(SignalStoreError::InvalidFormat)?,
        sender_device: sender_device_id.try_into()?,
        client_timestamp: Utc.timestamp_millis_opt(ts).single().ok_or(SignalStoreError::InvalidFormat)?,
        // A message stored before the server's stamp was kept has none; its own time stands in.
        server_timestamp: Utc
            .timestamp_millis_opt(server_ts.unwrap_or(ts))
            .single()
            .ok_or(SignalStoreError::InvalidFormat)?,
        needs_receipt: row.get(5)?,
        unidentified_sender: row.get(6)?,
        server_guid: None,
        was_plaintext: row.get(8)?,
        pni_verified: pni_verified
            .as_deref()
            .map(|p| Pni::parse_from_service_id_string(p).ok_or(SignalStoreError::InvalidFormat))
            .transpose()?,
    };
    Content::from_proto(body, metadata).map_err(|_| SignalStoreError::InvalidFormat)
}

pub(super) fn sticker_pack_from_row(row: &Row<'_>) -> Result<StickerPack> {
    let manifest: String = row.get(2)?;
    Ok(StickerPack {
        id: row.get(0)?,
        key: row.get(1)?,
        manifest: serde_json::from_str::<StickerPackManifest>(&manifest)?,
    })
}
