//! Changes made here that the account's other devices keep: a conversation muted, archived, pinned or marked unread, the
//! privacy settings, and the story lists - all of which live in the storage service, the account's shared copy.
//!
//! The storage service is the phone's own record of its contacts and settings, so a change is made the careful way:
//! the record is read fresh and changed in its bytes, field by field, so everything this copy of Signal's protobufs does
//! not know (newer fields, a phone's own additions) goes back exactly as it was. Then, as Signal's apps write: the record
//! gets a new id, the manifest a new version naming it in place of the old one, and the old item is dropped. A device
//! that wrote in between makes the server refuse; it is read again and the change made again on what is there now. The
//! other devices are then told to fetch the new manifest.

use presage::libsignal_service::proto::manifest_record::identifier::Type;
use presage::libsignal_service::proto::{storage_record, ManifestRecord, StorageRecord};
use presage::libsignal_service::StorageServiceError;
use prost::Message;
use uuid::Uuid;

// -- the bytes of a protobuf message, changed field by field ----------------------------------------------------------

/// One field of a message as it is on the wire: its number, its wire type, and its bytes (tag included).
#[derive(Debug, Clone)]
struct Field<'a> {
    number: u32,
    wire: u8,
    /// The whole field, tag included, as it was.
    raw: &'a [u8],
    /// The field's value: the varint's bytes, or a length-delimited field's contents.
    value: &'a [u8],
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut out = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *bytes.get(*at)?;
        *at += 1;
        out |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(out);
        }
    }
    None
}

fn write_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// The fields of a message, in order; `None` if the bytes are not a message.
fn fields(bytes: &[u8]) -> Option<Vec<Field<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let tag = read_varint(bytes, &mut at)?;
        let (number, wire) = ((tag >> 3) as u32, (tag & 7) as u8);
        let value_start;
        match wire {
            0 => {
                value_start = at;
                read_varint(bytes, &mut at)?;
            }
            1 => {
                value_start = at;
                at = at.checked_add(8).filter(|&e| e <= bytes.len())?;
            }
            2 => {
                let len = read_varint(bytes, &mut at)? as usize;
                value_start = at;
                at = at.checked_add(len).filter(|&e| e <= bytes.len())?;
            }
            5 => {
                value_start = at;
                at = at.checked_add(4).filter(|&e| e <= bytes.len())?;
            }
            _ => return None,
        }
        if number == 0 {
            return None;
        }
        out.push(Field { number, wire, raw: &bytes[start..at], value: &bytes[value_start..at] });
    }
    Some(out)
}

fn tag(number: u32, wire: u8, out: &mut Vec<u8>) {
    write_varint((u64::from(number) << 3) | u64::from(wire), out);
}

/// The message with field `number` replaced by these values (each written whole: a varint, or length-delimited bytes),
/// in the place the first of the old ones was, every other field kept as it was. No values removes the field.
fn replace(bytes: &[u8], number: u32, values: &[Value]) -> Option<Vec<u8>> {
    let all = fields(bytes)?;
    let mut out = Vec::with_capacity(bytes.len() + 16);
    let mut written = false;
    let write_new = |out: &mut Vec<u8>| {
        for v in values {
            match v {
                Value::Varint(n) => {
                    tag(number, 0, out);
                    write_varint(*n, out);
                }
                Value::Bytes(b) => {
                    tag(number, 2, out);
                    write_varint(b.len() as u64, out);
                    out.extend_from_slice(b);
                }
            }
        }
    };
    for f in &all {
        if f.number == number {
            if !written {
                write_new(&mut out);
                written = true;
            }
            continue;
        }
        out.extend_from_slice(f.raw);
    }
    if !written {
        write_new(&mut out);
    }
    Some(out)
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Varint(u64),
    Bytes(Vec<u8>),
}

/// A proto3 scalar: set when it is not its default, absent when it is (as Signal's apps write them).
fn set_varint(bytes: &[u8], number: u32, value: u64) -> Option<Vec<u8>> {
    replace(bytes, number, &if value == 0 { vec![] } else { vec![Value::Varint(value)] })
}

/// The contents of every field `number` that is length-delimited.
fn all_bytes(bytes: &[u8], number: u32) -> Vec<&[u8]> {
    fields(bytes).unwrap_or_default().into_iter().filter(|f| f.number == number && f.wire == 2).map(|f| f.value).collect()
}

fn varint_of(bytes: &[u8], number: u32) -> Option<u64> {
    let f = fields(bytes)?.into_iter().rfind(|f| f.number == number && f.wire == 0)?;
    let mut at = 0;
    read_varint(f.value, &mut at)
}

// -- what can be changed ----------------------------------------------------------------------------------------------

/// Which record a change is to.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A person's conversation (their contact record), or Note to Self when it is the account itself.
    Person(Uuid),
    Group([u8; 32]),
    /// The account's own record: settings, pins, Note to Self.
    Account,
    /// A story list (My Story is the nil id).
    StoryList(Uuid),
}

/// One change.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Muted until this time (ms since the epoch); 0 unmutes, `u64::MAX` is always.
    Mute(u64),
    Archive(bool),
    MarkUnread(bool),
    /// Pinned or not (an account-record change, whatever the conversation).
    Pin(bool),
    /// An account setting (a field of the account record): a yes or no as 1 or 0, a choice, or a number.
    Setting(AccountSetting, u64),
    /// Whether a story list's audience may reply.
    ListReplies(bool),
    /// A person added to (true) or taken off a story list.
    ListPerson(Uuid, bool),
    /// A story list's new name (not My Story's).
    ListRename(String),
    /// A story list taken away (marked deleted, as Signal's apps do; not My Story).
    ListDelete,
    /// My Story's audience: every connection except those listed (true), or only those listed; the list starts empty.
    ListMode(bool),
    /// The account's username and its link (entropy, server handle), or none.
    Username(Option<(String, [u8; 32], Uuid)>),
    /// A new link for the same username.
    UsernameLink([u8; 32], Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSetting {
    ReadReceipts,
    SealedSenderIndicators,
    TypingIndicators,
    LinkPreviews,
    /// Who sees the account's number: 1 everybody, 2 nobody.
    PhoneNumberSharing,
    /// Found by number: written as the account record's "unlisted", the other way round.
    FindableByNumber,
    /// The timer a new conversation starts with, in seconds (0 off).
    DefaultTimer,
    StoriesDisabled,
    /// 1 on, 2 off (the record's optional bool).
    StoryViewReceipts,
    KeepMutedChatsArchived,
}

impl AccountSetting {
    fn field(self) -> u32 {
        match self {
            Self::ReadReceipts => 6,
            Self::SealedSenderIndicators => 7,
            Self::TypingIndicators => 8,
            Self::LinkPreviews => 11,
            Self::PhoneNumberSharing => 12,
            Self::FindableByNumber => 13,
            Self::DefaultTimer => 17,
            Self::KeepMutedChatsArchived => 25,
            Self::StoriesDisabled => 29,
            Self::StoryViewReceipts => 30,
        }
    }

    /// The value written for what the window says: a yes or no, or a number.
    pub fn value(self, on: Option<bool>, number: Option<u64>) -> Result<u64, String> {
        match self {
            Self::PhoneNumberSharing => match number {
                Some(n @ (1 | 2)) => Ok(n),
                _ => Err("phone number sharing is 1 (everybody) or 2 (nobody)".to_string()),
            },
            Self::DefaultTimer => number.filter(|n| *n <= 4 * 7 * 24 * 3600).ok_or_else(|| "the timer is a number of seconds, at most four weeks".to_string()),
            Self::StoryViewReceipts => on.map(|on| if on { 1 } else { 2 }).ok_or_else(|| "on or off".to_string()),
            // Unlisted is the record's word: findable is the other way round.
            Self::FindableByNumber => on.map(|on| u64::from(!on)).ok_or_else(|| "on or off".to_string()),
            _ => on.map(u64::from).ok_or_else(|| "on or off".to_string()),
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "readReceipts" => Self::ReadReceipts,
            "sealedSenderIndicators" => Self::SealedSenderIndicators,
            "typingIndicators" => Self::TypingIndicators,
            "linkPreviews" => Self::LinkPreviews,
            "phoneNumberSharing" => Self::PhoneNumberSharing,
            "findableByNumber" => Self::FindableByNumber,
            "defaultTimer" => Self::DefaultTimer,
            "storiesDisabled" => Self::StoriesDisabled,
            "storyViewReceipts" => Self::StoryViewReceipts,
            "keepMutedChatsArchived" => Self::KeepMutedChatsArchived,
            _ => return None,
        })
    }
}

/// Signal's apps keep at most four conversations pinned.
pub const MOST_PINS: usize = 4;

/// The `StorageRecord` field a record of this target is in.
fn record_field(target: &Target, own: &Uuid) -> u32 {
    match target {
        Target::Person(who) if who == own => 4,
        Target::Person(_) => 1,
        Target::Group(_) => 3,
        Target::Account => 4,
        Target::StoryList(_) => 5,
    }
}

/// A pinned conversation as the account record writes one.
fn pin_bytes(target: &Target) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match target {
        Target::Person(who) => {
            let mut contact = Vec::new();
            tag(1, 2, &mut contact);
            let text = who.to_string();
            write_varint(text.len() as u64, &mut contact);
            contact.extend_from_slice(text.as_bytes());
            tag(3, 2, &mut contact);
            write_varint(16, &mut contact);
            contact.extend_from_slice(who.as_bytes());
            tag(1, 2, &mut out);
            write_varint(contact.len() as u64, &mut out);
            out.extend_from_slice(&contact);
        }
        Target::Group(key) => {
            tag(4, 2, &mut out);
            write_varint(32, &mut out);
            out.extend_from_slice(key);
        }
        _ => return None,
    }
    Some(out)
}

/// Whether a pinned-conversation entry names this target.
fn pin_names(entry: &[u8], target: &Target) -> bool {
    match target {
        Target::Person(who) => all_bytes(entry, 1).into_iter().any(|contact| {
            all_bytes(contact, 3).into_iter().any(|b| b == who.as_bytes()) || all_bytes(contact, 1).into_iter().any(|s| s == who.to_string().as_bytes())
        }),
        Target::Group(key) => all_bytes(entry, 4).into_iter().any(|b| b == key),
        _ => false,
    }
}

/// The inner record (contact, group, account or story list), changed. `conversation` is the target the change is for
/// (for an account-record change about a conversation: Note to Self, or a pin).
fn change_inner(inner: &[u8], change: &Change, conversation: &Target, own: &Uuid) -> Result<Vec<u8>, String> {
    let note_to_self = matches!(conversation, Target::Person(who) if who == own);
    let broken = || "that record could not be read".to_string();
    let out = match change {
        Change::Mute(until) => match conversation {
            Target::Person(_) if note_to_self => return Err("Note to Self is not muted on Signal".to_string()),
            Target::Person(_) => set_varint(inner, 13, *until),
            Target::Group(_) => set_varint(inner, 6, *until),
            _ => None,
        },
        Change::Archive(on) => match conversation {
            Target::Person(_) if note_to_self => set_varint(inner, 5, u64::from(*on)),
            Target::Person(_) => set_varint(inner, 11, u64::from(*on)),
            Target::Group(_) => set_varint(inner, 4, u64::from(*on)),
            _ => None,
        },
        Change::MarkUnread(on) => match conversation {
            Target::Person(_) if note_to_self => set_varint(inner, 10, u64::from(*on)),
            Target::Person(_) => set_varint(inner, 12, u64::from(*on)),
            Target::Group(_) => set_varint(inner, 5, u64::from(*on)),
            _ => None,
        },
        Change::Pin(on) => {
            let mut pins: Vec<Vec<u8>> = all_bytes(inner, 14).into_iter().map(<[u8]>::to_vec).collect();
            let was = pins.iter().any(|p| pin_names(p, conversation));
            if *on && !was {
                if pins.len() >= MOST_PINS {
                    return Err(format!("Signal keeps at most {MOST_PINS} conversations pinned: unpin one first"));
                }
                pins.push(pin_bytes(conversation).ok_or("only a conversation can be pinned")?);
            } else if !on {
                pins.retain(|p| !pin_names(p, conversation));
            }
            replace(inner, 14, &pins.into_iter().map(Value::Bytes).collect::<Vec<_>>())
        }
        Change::Setting(setting, value) => set_varint(inner, setting.field(), *value),
        Change::ListReplies(on) => set_varint(inner, 5, u64::from(*on)),
        Change::ListRename(name) => {
            let name = name.trim();
            if name.is_empty() {
                return Err("A story list needs a name".to_string());
            }
            replace(inner, 2, &[Value::Bytes(name.as_bytes().to_vec())])
        }
        Change::ListDelete => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(1);
            let step = set_varint(inner, 4, now).ok_or_else(broken)?;
            let step = replace(&step, 3, &[]).ok_or_else(broken)?;
            replace(&step, 7, &[])
        }
        Change::ListMode(excluding) => {
            let step = set_varint(inner, 6, u64::from(*excluding)).ok_or_else(broken)?;
            let step = replace(&step, 3, &[]).ok_or_else(broken)?;
            replace(&step, 7, &[])
        }
        Change::Username(None) => {
            let step = replace(inner, 33, &[]).ok_or_else(broken)?;
            replace(&step, 35, &[])
        }
        Change::Username(Some((name, entropy, handle))) => {
            let step = replace(inner, 33, &[Value::Bytes(name.as_bytes().to_vec())]).ok_or_else(broken)?;
            change_inner(&step, &Change::UsernameLink(*entropy, *handle), conversation, own).ok()
        }
        Change::UsernameLink(entropy, handle) => {
            // The link's colour (and anything else in it) stays; its entropy and handle are the new ones.
            let link = all_bytes(inner, 35).into_iter().next().unwrap_or_default().to_vec();
            let link = replace(&link, 1, &[Value::Bytes(entropy.to_vec())]).ok_or_else(broken)?;
            let link = replace(&link, 2, &[Value::Bytes(handle.as_bytes().to_vec())]).ok_or_else(broken)?;
            replace(inner, 35, &[Value::Bytes(link)])
        }
        Change::ListPerson(who, add) => {
            // The list names people by text and by bytes; both are kept in step, everything else left alone.
            let mut binary: Vec<Vec<u8>> = all_bytes(inner, 7).into_iter().map(<[u8]>::to_vec).collect();
            let mut text: Vec<Vec<u8>> = all_bytes(inner, 3).into_iter().map(<[u8]>::to_vec).collect();
            binary.retain(|b| b != who.as_bytes());
            text.retain(|t| t != who.to_string().as_bytes());
            if *add {
                binary.push(who.as_bytes().to_vec());
                if !text.is_empty() {
                    text.push(who.to_string().into_bytes());
                }
            }
            let step = replace(inner, 7, &binary.into_iter().map(Value::Bytes).collect::<Vec<_>>()).ok_or_else(broken)?;
            replace(&step, 3, &text.into_iter().map(Value::Bytes).collect::<Vec<_>>())
        }
    };
    out.ok_or_else(broken)
}

/// Which record a change is made in: a conversation's own, or the account's (Note to Self and pins live there).
fn record_target(conversation: &Target, change: &Change, own: &Uuid) -> Target {
    match (conversation, change) {
        (_, Change::Pin(_)) | (_, Change::Setting(..)) | (_, Change::Username(_)) | (_, Change::UsernameLink(..)) => Target::Account,
        (Target::Person(who), _) if who == own => Target::Account,
        (t, _) => t.clone(),
    }
}

/// Whether a record (decoded only to tell which it is) is the target's.
fn is_target(record: &StorageRecord, target: &Target) -> bool {
    match (&record.record, target) {
        (Some(storage_record::Record::Contact(c)), Target::Person(who)) => {
            Uuid::from_slice(&c.aci_binary).ok().or_else(|| Uuid::parse_str(&c.aci).ok()) == Some(*who)
        }
        (Some(storage_record::Record::GroupV2(g)), Target::Group(key)) => g.master_key == key,
        (Some(storage_record::Record::Account(_)), Target::Account) => true,
        (Some(storage_record::Record::StoryDistributionList(l)), Target::StoryList(id)) => Uuid::from_slice(&l.identifier).ok() == Some(*id),
        _ => false,
    }
}

fn type_of(target: &Target) -> Type {
    match target {
        Target::Person(_) => Type::Contact,
        Target::Group(_) => Type::Groupv2,
        Target::Account => Type::Account,
        Target::StoryList(_) => Type::StoryDistributionList,
    }
}

/// The whole record's bytes with its inner record changed.
fn change_record(plaintext: &[u8], change: &Change, conversation: &Target, own: &Uuid, field: u32) -> Result<Vec<u8>, String> {
    let inner = all_bytes(plaintext, field).into_iter().next().ok_or("that record is not the kind expected")?;
    let changed = change_inner(inner, change, conversation, own)?;
    replace(plaintext, field, &[Value::Bytes(changed)]).ok_or_else(|| "that record could not be written".to_string())
}

/// Makes the change in the storage service: read, changed, written, retried once over a device that wrote first.
pub async fn write(manager: &presage::Manager<super::store::SignalStore, presage::manager::Registered>, own: Uuid, device: u32, conversation: &Target, change: &Change) -> Result<(), String> {
    let storage = manager.storage_service().await.map_err(|e| format!("the storage service could not be reached: {e}"))?;
    let record_at = record_target(conversation, change, &own);
    let field = record_field(&record_at, &own);
    let mut manifest = storage.manifest().await.map_err(|e| format!("the storage service could not be read: {e}"))?;
    for attempt in 0..2 {
        let ikm = Some(manifest.record_ikm.as_slice()).filter(|k| !k.is_empty());
        let kind = type_of(&record_at);
        let candidates: Vec<Vec<u8>> = manifest.identifiers.iter().filter(|i| i.r#type() == kind).map(|i| i.raw.clone()).collect();
        let mut found = None;
        for chunk in candidates.chunks(100) {
            for (raw_id, plaintext) in storage.read_raw_items(chunk.to_vec(), ikm).await.map_err(|e| format!("the storage service could not be read: {e}"))? {
                let Ok(record) = StorageRecord::decode(plaintext.as_slice()) else { continue };
                if is_target(&record, &record_at) {
                    found = Some((raw_id, plaintext));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let (old_id, plaintext) = found.ok_or("That conversation is not in your account's shared settings yet: change it on the phone once")?;
        let changed = change_record(&plaintext, change, conversation, &own, field)?;
        if changed == plaintext {
            return Ok(());
        }
        let new_id: [u8; 16] = rand::random();
        let mut next = ManifestRecord { version: manifest.version + 1, source_device: device, ..manifest.clone() };
        for identifier in &mut next.identifiers {
            if identifier.raw == old_id {
                identifier.raw = new_id.to_vec();
            }
        }
        let item = storage.encrypt_raw_item(new_id.to_vec(), &changed, ikm);
        match storage.write(&next, vec![item], vec![old_id]).await {
            Ok(()) => return Ok(()),
            Err(StorageServiceError::Conflict(current)) if attempt == 0 => {
                tracing::info!("signal: another device wrote the shared settings first; reading them again");
                manifest = storage.open_manifest(&current).map_err(|e| format!("the storage service could not be read: {e}"))?;
            }
            Err(e) => return Err(format!("the change could not be written: {e}")),
        }
    }
    Err("another device kept changing the shared settings; try again".to_string())
}

/// Makes a new story list with these people (replies allowed), as a new record in a new manifest version. Answers its id.
pub async fn create_list(manager: &presage::Manager<super::store::SignalStore, presage::manager::Registered>, device: u32, name: &str, people: &[Uuid]) -> Result<Uuid, String> {
    use presage::libsignal_service::proto::{manifest_record::Identifier, StoryDistributionListRecord};
    let name = name.trim();
    if name.is_empty() {
        return Err("A story list needs a name".to_string());
    }
    let id = Uuid::new_v4();
    let record = StorageRecord {
        record: Some(storage_record::Record::StoryDistributionList(StoryDistributionListRecord {
            identifier: id.as_bytes().to_vec(),
            name: name.to_string(),
            recipient_service_ids_binary: people.iter().map(|p| p.as_bytes().to_vec()).collect(),
            allows_replies: true,
            ..Default::default()
        })),
    }
    .encode_to_vec();
    let storage = manager.storage_service().await.map_err(|e| format!("the storage service could not be reached: {e}"))?;
    let mut manifest = storage.manifest().await.map_err(|e| format!("the storage service could not be read: {e}"))?;
    for attempt in 0..2 {
        let ikm = Some(manifest.record_ikm.as_slice()).filter(|k| !k.is_empty());
        let raw: [u8; 16] = rand::random();
        let mut next = ManifestRecord { version: manifest.version + 1, source_device: device, ..manifest.clone() };
        next.identifiers.push(Identifier { raw: raw.to_vec(), r#type: Type::StoryDistributionList as i32 });
        let item = storage.encrypt_raw_item(raw.to_vec(), &record, ikm);
        match storage.write(&next, vec![item], vec![]).await {
            Ok(()) => return Ok(id),
            Err(StorageServiceError::Conflict(current)) if attempt == 0 => {
                manifest = storage.open_manifest(&current).map_err(|e| format!("the storage service could not be read: {e}"))?;
            }
            Err(e) => return Err(format!("the list could not be made: {e}")),
        }
    }
    Err("another device kept changing the shared settings; try again".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::proto::{account_record, AccountRecord, ContactRecord, GroupV2Record, StoryDistributionListRecord};

    fn wrap(record: storage_record::Record) -> Vec<u8> {
        StorageRecord { record: Some(record) }.encode_to_vec()
    }

    /// A field this copy of the protobufs does not know, put on the end of an inner record.
    fn with_unknown(mut inner: Vec<u8>) -> Vec<u8> {
        tag(999, 2, &mut inner);
        write_varint(3, &mut inner);
        inner.extend_from_slice(b"new");
        inner
    }

    #[test]
    fn a_field_is_changed_and_everything_else_kept_byte_for_byte() {
        let who = Uuid::from_u128(5);
        let own = Uuid::from_u128(1);
        let inner = with_unknown(ContactRecord { aci_binary: who.as_bytes().to_vec(), whitelisted: true, archived: false, ..Default::default() }.encode_to_vec());
        let mut plain = Vec::new();
        tag(1, 2, &mut plain);
        write_varint(inner.len() as u64, &mut plain);
        plain.extend_from_slice(&inner);
        let changed = change_record(&plain, &Change::Archive(true), &Target::Person(who), &own, 1).unwrap();
        let record = StorageRecord::decode(changed.as_slice()).unwrap();
        let Some(storage_record::Record::Contact(c)) = record.record else { panic!() };
        assert!(c.archived && c.whitelisted);
        assert_eq!(c.aci_binary, who.as_bytes());
        // The field nobody here knows is still there, as it was.
        let inner_now = all_bytes(&changed, 1)[0];
        assert_eq!(all_bytes(inner_now, 999), vec![&b"new"[..]]);
        // And back: false is written as absent, as Signal's apps write it.
        let back = change_record(&changed, &Change::Archive(false), &Target::Person(who), &own, 1).unwrap();
        assert!(fields(all_bytes(&back, 1)[0]).unwrap().iter().all(|f| f.number != 11));
    }

    #[test]
    fn a_mute_goes_on_the_group_and_note_to_self_lives_in_the_account() {
        let own = Uuid::from_u128(1);
        let key = [7u8; 32];
        let group = wrap(storage_record::Record::GroupV2(GroupV2Record { master_key: key.to_vec(), ..Default::default() }));
        let muted = change_record(&group, &Change::Mute(1_900_000_000_000), &Target::Group(key), &own, 3).unwrap();
        let Some(storage_record::Record::GroupV2(g)) = StorageRecord::decode(muted.as_slice()).unwrap().record else { panic!() };
        assert_eq!(g.muted_until_timestamp, 1_900_000_000_000);
        assert_eq!(record_target(&Target::Person(own), &Change::Archive(true), &own), Target::Account);
        let account = wrap(storage_record::Record::Account(AccountRecord { read_receipts: true, ..Default::default() }));
        let archived = change_record(&account, &Change::Archive(true), &Target::Person(own), &own, 4).unwrap();
        let Some(storage_record::Record::Account(a)) = StorageRecord::decode(archived.as_slice()).unwrap().record else { panic!() };
        assert!(a.note_to_self_archived && a.read_receipts);
        assert!(change_inner(&[], &Change::Mute(1), &Target::Person(own), &own).is_err(), "Note to Self has no mute");
    }

    #[test]
    fn pins_are_added_kept_to_four_and_taken_off() {
        let own = Uuid::from_u128(1);
        let (a, b) = (Uuid::from_u128(2), Uuid::from_u128(3));
        let account = wrap(storage_record::Record::Account(AccountRecord::default()));
        let one = change_record(&account, &Change::Pin(true), &Target::Person(a), &own, 4).unwrap();
        let two = change_record(&one, &Change::Pin(true), &Target::Group([9; 32]), &own, 4).unwrap();
        let Some(storage_record::Record::Account(acc)) = StorageRecord::decode(two.as_slice()).unwrap().record else { panic!() };
        assert_eq!(acc.pinned_conversations.len(), 2);
        assert!(matches!(&acc.pinned_conversations[0].identifier, Some(account_record::pinned_conversation::Identifier::Contact(c)) if c.service_id_binary == a.as_bytes()));
        // Pinning again is no change; unpinning takes only that one.
        assert_eq!(change_record(&two, &Change::Pin(true), &Target::Person(a), &own, 4).unwrap(), two);
        let less = change_record(&two, &Change::Pin(false), &Target::Person(a), &own, 4).unwrap();
        let Some(storage_record::Record::Account(acc)) = StorageRecord::decode(less.as_slice()).unwrap().record else { panic!() };
        assert_eq!(acc.pinned_conversations.len(), 1);
        let mut full = account.clone();
        for n in 10..14 {
            full = change_record(&full, &Change::Pin(true), &Target::Person(Uuid::from_u128(n)), &own, 4).unwrap();
        }
        assert!(change_record(&full, &Change::Pin(true), &Target::Person(b), &own, 4).unwrap_err().contains("at most 4"));
    }

    #[test]
    fn a_setting_and_a_story_list_are_changed_in_place() {
        let own = Uuid::from_u128(1);
        let account = wrap(storage_record::Record::Account(AccountRecord { typing_indicators: true, ..Default::default() }));
        let changed = change_record(&account, &Change::Setting(AccountSetting::ReadReceipts, 1), &Target::Account, &own, 4).unwrap();
        let Some(storage_record::Record::Account(a)) = StorageRecord::decode(changed.as_slice()).unwrap().record else { panic!() };
        assert!(a.read_receipts && a.typing_indicators);
        let timer = change_record(&changed, &Change::Setting(AccountSetting::DefaultTimer, 3600), &Target::Account, &own, 4).unwrap();
        let unlisted = change_record(&timer, &Change::Setting(AccountSetting::FindableByNumber, AccountSetting::FindableByNumber.value(Some(false), None).unwrap()), &Target::Account, &own, 4).unwrap();
        let Some(storage_record::Record::Account(a)) = StorageRecord::decode(unlisted.as_slice()).unwrap().record else { panic!() };
        assert_eq!((a.universal_expire_timer, a.unlisted_phone_number, a.read_receipts), (3600, true, true));
        assert!(AccountSetting::PhoneNumberSharing.value(None, Some(3)).is_err());
        let (x, y) = (Uuid::from_u128(4), Uuid::from_u128(5));
        let list = wrap(storage_record::Record::StoryDistributionList(StoryDistributionListRecord {
            identifier: Uuid::nil().as_bytes().to_vec(),
            recipient_service_ids_binary: vec![x.as_bytes().to_vec()],
            ..Default::default()
        }));
        let added = change_record(&list, &Change::ListPerson(y, true), &Target::StoryList(Uuid::nil()), &own, 5).unwrap();
        let taken = change_record(&added, &Change::ListPerson(x, false), &Target::StoryList(Uuid::nil()), &own, 5).unwrap();
        let replies = change_record(&taken, &Change::ListReplies(true), &Target::StoryList(Uuid::nil()), &own, 5).unwrap();
        let Some(storage_record::Record::StoryDistributionList(l)) = StorageRecord::decode(replies.as_slice()).unwrap().record else { panic!() };
        assert_eq!(l.recipient_service_ids_binary, vec![y.as_bytes().to_vec()]);
        assert!(l.allows_replies);
        assert_eq!(l.identifier, Uuid::nil().as_bytes());
        // My Story to "only those listed": the list starts empty; renamed and deleted for a custom one.
        let only = change_record(&replies, &Change::ListMode(false), &Target::StoryList(Uuid::nil()), &own, 5).unwrap();
        let Some(storage_record::Record::StoryDistributionList(l)) = StorageRecord::decode(only.as_slice()).unwrap().record else { panic!() };
        assert!(!l.is_block_list && l.recipient_service_ids_binary.is_empty() && l.allows_replies);
        let renamed = change_record(&only, &Change::ListRename(" Close friends ".into()), &Target::StoryList(Uuid::nil()), &own, 5).unwrap();
        let gone = change_record(&renamed, &Change::ListDelete, &Target::StoryList(Uuid::nil()), &own, 5).unwrap();
        let Some(storage_record::Record::StoryDistributionList(l)) = StorageRecord::decode(gone.as_slice()).unwrap().record else { panic!() };
        assert_eq!(l.name, "Close friends");
        assert!(l.deleted_at_timestamp > 0);
    }

    #[test]
    fn a_username_and_its_link_go_in_the_account_record_and_the_colour_stays() {
        use presage::libsignal_service::proto::account_record::UsernameLink;
        let own = Uuid::from_u128(1);
        let account = wrap(storage_record::Record::Account(AccountRecord { username_link: Some(UsernameLink { color: 5, ..Default::default() }), ..Default::default() }));
        let handle = Uuid::from_u128(9);
        let set = change_record(&account, &Change::Username(Some(("moho.42".into(), [3; 32], handle))), &Target::Account, &own, 4).unwrap();
        let Some(storage_record::Record::Account(a)) = StorageRecord::decode(set.as_slice()).unwrap().record else { panic!() };
        let link = a.username_link.unwrap();
        assert_eq!((a.username.as_str(), link.entropy, link.server_id, link.color), ("moho.42", vec![3; 32], handle.as_bytes().to_vec(), 5));
        let gone = change_record(&set, &Change::Username(None), &Target::Account, &own, 4).unwrap();
        let Some(storage_record::Record::Account(a)) = StorageRecord::decode(gone.as_slice()).unwrap().record else { panic!() };
        assert!(a.username.is_empty() && a.username_link.is_none());
    }

    #[test]
    fn bytes_that_are_not_a_message_are_refused_not_mangled() {
        assert!(fields(&[0xff]).is_none());
        assert!(fields(&[0x0a, 0x05, 1]).is_none(), "a length past the end");
        assert_eq!(varint_of(&{
            let mut b = Vec::new();
            tag(6, 0, &mut b);
            write_varint(300, &mut b);
            b
        }, 6), Some(300));
    }
}
