//! What the phone keeps in Signal's storage service: the account's own address book and settings, shared by every
//! device. A linked device reads it rather than being told: the names the phone's contacts have (the old contact sync
//! is answered with an empty list by today's phones), whether a conversation has been accepted or blocked, archived,
//! marked unread or muted, which are pinned, and the account's privacy settings.
//!
//! Read at connect and whenever the phone says it changed (a `FetchLatest` of the storage manifest); only when the
//! manifest's version differs from the one already read, so a reconnect with nothing new asks for one small answer.
//! Kept in the account's file, so it is known before the connection is up.

use super::ids;
use super::store::SignalStore;
use presage::libsignal_service::proto::{account_record, contact_record, manifest_record, storage_record, AccountRecord, ContactRecord, GroupV2Record, StorageRecord};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

/// How the phone has a conversation: the parts of a contact or group record a client acts on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Standing {
    /// What the person has been named by the account: a nickname given in Signal, else the phone's address book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// The person's number, when the phone knows it (international form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    /// A note the account keeps about the person (Signal's "Note" on a contact).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub blocked: bool,
    /// The conversation was accepted (Signal's "whitelisted": the account shares its profile with them).
    #[serde(default)]
    pub accepted: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub marked_unread: bool,
    /// Muted until this time (ms since the epoch); `u64::MAX` is "always".
    #[serde(default)]
    pub muted_until: u64,
    /// Taken off the list on the phone (a contact "removed" there, kept only so a message from them is recognised).
    #[serde(default)]
    pub hidden: bool,
    /// Signal said this person is no longer registered.
    #[serde(default)]
    pub unregistered: bool,
    /// Their stories (or the group's) are hidden on the phone.
    #[serde(default)]
    pub hide_story: bool,
}

impl Standing {
    pub fn muted(&self, now_ms: u64) -> bool {
        self.muted_until > now_ms
    }
}

/// The account's own settings as the storage service has them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountSettings {
    pub read_receipts: bool,
    pub typing_indicators: bool,
    pub link_previews: bool,
    pub sealed_sender_indicators: bool,
    /// The disappearing-message timer a new conversation starts with, in seconds (0 is off).
    pub default_timer: u32,
    /// Whether authors are told their stories were viewed; unset follows `read_receipts`, as on the phone.
    #[serde(default)]
    pub story_view_receipts: Option<bool>,
    /// Who sees the account's number: 1 everybody, 2 nobody (0 not said).
    #[serde(default)]
    pub phone_number_sharing: i32,
    /// Whether the account can be found by its number (false: unlisted).
    #[serde(default)]
    pub findable_by_number: bool,
    #[serde(default)]
    pub stories_disabled: bool,
    #[serde(default)]
    pub keep_muted_chats_archived: bool,
}

impl AccountSettings {
    pub fn tells_story_views(&self) -> bool {
        self.story_view_receipts.unwrap_or(self.read_receipts)
    }
}

/// Everything read, as of one manifest version.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    /// The manifest version this was read from; `0` for never read.
    pub version: u64,
    /// By conversation (`p:<aci>`, `g:<master key hex>`).
    #[serde(default)]
    pub conversations: BTreeMap<String, Standing>,
    /// Pinned conversations, in the phone's order.
    #[serde(default)]
    pub pinned: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_username: Option<String>,
    /// The account's username link: its entropy and server handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_username_link: Option<(Vec<u8>, Uuid)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountSettings>,
    /// Note to Self's archive and unread marks, which the account's own record keeps.
    #[serde(default)]
    pub note_to_self: Standing,
    /// The account's story audiences: My Story and the lists made on the phone (not deleted ones).
    #[serde(default)]
    pub story_lists: Vec<StoryList>,
    /// Whether this copy was read by a moho that reads story lists (an older copy is read again in full once).
    #[serde(default)]
    pub reads_story_lists: bool,
    /// The account's chat folders, in the phone's order (not deleted ones, nor the "all chats" one).
    #[serde(default)]
    pub chat_folders: Vec<ChatFolder>,
    /// The account's notification profiles (not deleted ones).
    #[serde(default)]
    pub notification_profiles: Vec<NotificationProfile>,
    /// A profile turned on by hand: its id and until when (ms; 0 is until turned off). `None` with `profiles_off_at`
    /// set is "turned off by hand" at that time.
    #[serde(default)]
    pub profile_on: Option<(Uuid, u64)>,
    #[serde(default)]
    pub profiles_off_at: u64,
    /// Whether this copy was read by a moho that reads folders and profiles (an older copy is read again once).
    #[serde(default)]
    pub reads_folders: bool,
}

/// A chat folder as the phone keeps it: which chats it holds, by rule and by name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatFolder {
    pub id: Uuid,
    pub name: String,
    pub position: u32,
    pub all_people: bool,
    pub all_groups: bool,
    pub muted_too: bool,
    pub only_unread: bool,
    /// Conversations (`p:<aci>`, `g:<hex>`) named in, and named out.
    pub included: Vec<String>,
    pub excluded: Vec<String>,
}

impl ChatFolder {
    /// Whether the folder holds a conversation (the phone's rules; "only unread" is left to the window's own unread
    /// marks, so the folder holds what it would hold read or not).
    pub fn holds(&self, remote: &str, standing: Option<&Standing>, now_ms: u64) -> bool {
        if self.excluded.iter().any(|r| r == remote) {
            return false;
        }
        if self.included.iter().any(|r| r == remote) {
            return true;
        }
        let by_kind = (self.all_people && remote.starts_with("p:")) || (self.all_groups && remote.starts_with("g:"));
        by_kind && (self.muted_too || !standing.is_some_and(|s| s.muted(now_ms)))
    }
}

/// A notification profile: while it is on, only these people (and, if it says so, mentions) are announced.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NotificationProfile {
    pub id: Uuid,
    pub name: String,
    pub emoji: Option<String>,
    pub allow_mentions: bool,
    pub allow_calls: bool,
    /// Conversations (`p:<aci>`, `g:<hex>`) whose messages still notify.
    pub allowed: Vec<String>,
    pub scheduled: bool,
    /// The schedule, as the phone writes it: 24-hour clock numbers (930 is 9:30), and days 1 (Monday) to 7.
    pub start: u32,
    pub end: u32,
    pub days: Vec<i32>,
}

impl NotificationProfile {
    /// Whether the schedule has it on at this local time: `day` 1 (Monday) to 7, `clock` as 24-hour hhmm. A schedule
    /// that runs past midnight is on from its start to midnight on its day and from midnight to its end the day after.
    pub fn scheduled_on(&self, day: i32, clock: u32) -> bool {
        if !self.scheduled {
            return false;
        }
        let day_on = |d: i32| self.days.iter().any(|x| *x == d || (*x == 0 && d == 1));
        if self.start <= self.end {
            day_on(day) && clock >= self.start && clock < self.end
        } else {
            let yesterday = if day == 1 { 7 } else { day - 1 };
            (day_on(day) && clock >= self.start) || (day_on(yesterday) && clock < self.end)
        }
    }
}

impl Kept {
    /// The profile on now, if one is: one turned on by hand (until its end), else one whose schedule has it on, unless
    /// profiles were turned off by hand since the schedule began. `day` and `clock` are local (see `scheduled_on`).
    pub fn active_profile(&self, now_ms: u64, day: i32, clock: u32) -> Option<&NotificationProfile> {
        if let Some((id, until)) = self.profile_on {
            if until == 0 || until > now_ms {
                return self.notification_profiles.iter().find(|p| p.id == id);
            }
        }
        // Turned off by hand within the last day: the schedule that was on then stays off for now.
        if self.profiles_off_at > 0 && now_ms.saturating_sub(self.profiles_off_at) < 24 * 3600 * 1000 {
            return None;
        }
        self.notification_profiles.iter().find(|p| p.scheduled_on(day, clock))
    }
}

/// A recipient as the storage service names one, as moho names its conversation.
fn recipient_remote(r: &presage::libsignal_service::proto::Recipient) -> Option<String> {
    use presage::libsignal_service::proto::recipient::Identifier;
    match r.identifier.as_ref()? {
        Identifier::Contact(c) => {
            let aci = if c.service_id_binary.len() == 16 { Uuid::from_slice(&c.service_id_binary).ok() } else { Uuid::parse_str(&c.service_id).ok() }?;
            Some(format!("p:{aci}"))
        }
        Identifier::GroupMasterKey(key) => Some(format!("g:{}", ids::hex(key))),
        _ => None,
    }
}

/// One of the account's story audiences, as the phone keeps it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoryList {
    pub id: Uuid,
    pub name: String,
    /// For My Story with `excluding`, the people left out of all Signal connections; otherwise the people it goes to.
    pub people: Vec<Uuid>,
    pub excluding: bool,
    pub replies: bool,
}

/// My Story's identifier: the nil UUID, as Signal's clients name it.
pub const MY_STORY: Uuid = Uuid::nil();

impl StoryList {
    pub fn is_my_story(&self) -> bool {
        self.id == MY_STORY
    }

    /// Who a story to this list goes to: for My Story set to "all connections except", every Signal connection of the
    /// account (an accepted or saved person, not blocked, hidden or gone from Signal) but those left out.
    pub fn audience(&self, kept: &Kept, own: &Uuid) -> Vec<Uuid> {
        let mut who: Vec<Uuid> = if self.excluding {
            kept.connections().into_iter().filter(|p| !self.people.contains(p)).collect()
        } else {
            self.people.iter().copied().filter(|p| kept.standing(&format!("p:{p}")).is_none_or(|s| !s.blocked)).collect()
        };
        who.retain(|p| p != own);
        who.sort();
        who.dedup();
        who
    }
}

fn story_list(record: &presage::libsignal_service::proto::StoryDistributionListRecord) -> Option<StoryList> {
    if record.deleted_at_timestamp > 0 {
        return None;
    }
    let id = Uuid::from_slice(&record.identifier).ok()?;
    let mut people: Vec<Uuid> = record.recipient_service_ids_binary.iter().filter(|b| b.len() == 16).filter_map(|b| Uuid::from_slice(b).ok()).collect();
    people.extend(record.recipient_service_ids.iter().filter_map(|s| Uuid::parse_str(s).ok()));
    people.sort();
    people.dedup();
    let name = if id == MY_STORY { "My Story".to_string() } else { record.name.clone() };
    Some(StoryList { id, name, people, excluding: record.is_block_list, replies: record.allows_replies })
}

const NAME: &str = "storage-service";

fn joined(given: &str, family: &str) -> Option<String> {
    let name = format!("{} {}", given.trim(), family.trim()).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn contact_remote(record: &ContactRecord) -> Option<String> {
    let aci = if record.aci_binary.len() == 16 { Uuid::from_slice(&record.aci_binary).ok() } else { Uuid::parse_str(&record.aci).ok() }?;
    Some(format!("p:{aci}"))
}

fn contact_standing(record: &ContactRecord) -> Standing {
    let nickname = record.nickname.as_ref().and_then(|n| joined(&n.given, &n.family));
    let system = joined(&record.system_given_name, &record.system_family_name).or_else(|| text(&record.system_nickname));
    Standing {
        name: nickname.or(system),
        username: text(&record.username),
        number: text(&record.e164),
        note: text(&record.note),
        blocked: record.blocked,
        accepted: record.whitelisted,
        archived: record.archived,
        marked_unread: record.marked_unread,
        muted_until: record.muted_until_timestamp,
        hidden: record.hidden,
        unregistered: record.unregistered_at_timestamp > 0,
        hide_story: record.hide_story,
    }
}

fn group_standing(record: &GroupV2Record) -> Standing {
    Standing {
        blocked: record.blocked,
        accepted: record.whitelisted,
        archived: record.archived,
        marked_unread: record.marked_unread,
        muted_until: record.muted_until_timestamp,
        hide_story: record.hide_story,
        ..Default::default()
    }
}

fn pinned_remote(pin: &account_record::PinnedConversation) -> Option<String> {
    use account_record::pinned_conversation::Identifier;
    match pin.identifier.as_ref()? {
        Identifier::Contact(c) => {
            let aci = if c.service_id_binary.len() == 16 { Uuid::from_slice(&c.service_id_binary).ok() } else { Uuid::parse_str(&c.service_id).ok() }?;
            Some(format!("p:{aci}"))
        }
        Identifier::GroupMasterKey(key) => Some(format!("g:{}", ids::hex(key))),
        _ => None,
    }
}

fn account_settings(record: &AccountRecord) -> AccountSettings {
    AccountSettings {
        read_receipts: record.read_receipts,
        typing_indicators: record.typing_indicators,
        link_previews: record.link_previews,
        sealed_sender_indicators: record.sealed_sender_indicators,
        default_timer: record.universal_expire_timer,
        story_view_receipts: match record.story_view_receipts_enabled {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        },
        phone_number_sharing: record.phone_number_sharing_mode,
        findable_by_number: !record.unlisted_phone_number,
        stories_disabled: record.stories_disabled,
        keep_muted_chats_archived: record.keep_muted_chats_archived,
    }
}

impl Kept {
    /// What the records say, as of `version`. Records of kinds a client does not act on are passed over.
    pub fn from_records(version: u64, records: &[StorageRecord]) -> Self {
        let mut kept = Kept { version, reads_story_lists: true, reads_folders: true, ..Default::default() };
        for record in records {
            match &record.record {
                Some(storage_record::Record::Contact(c)) => {
                    if let Some(remote) = contact_remote(c) {
                        kept.conversations.insert(remote, contact_standing(c));
                    }
                }
                Some(storage_record::Record::GroupV2(g)) if g.master_key.len() == 32 => {
                    kept.conversations.insert(format!("g:{}", ids::hex(&g.master_key)), group_standing(g));
                }
                Some(storage_record::Record::ChatFolder(f)) if f.deleted_at_timestamp_ms == 0 && f.folder_type == 2 => {
                    if let Ok(id) = Uuid::from_slice(&f.identifier) {
                        kept.chat_folders.push(ChatFolder {
                            id,
                            name: f.name.clone(),
                            position: f.position,
                            all_people: f.include_all_individual_chats,
                            all_groups: f.include_all_group_chats,
                            muted_too: f.show_muted_chats,
                            only_unread: f.show_only_unread,
                            included: f.included_recipients.iter().filter_map(recipient_remote).collect(),
                            excluded: f.excluded_recipients.iter().filter_map(recipient_remote).collect(),
                        });
                    }
                }
                Some(storage_record::Record::NotificationProfile(p)) if p.deleted_at_timestamp_ms == 0 => {
                    if let Ok(id) = Uuid::from_slice(&p.id) {
                        kept.notification_profiles.push(NotificationProfile {
                            id,
                            name: p.name.clone(),
                            emoji: p.emoji.clone().filter(|e| !e.is_empty()),
                            allow_mentions: p.allow_all_mentions,
                            allow_calls: p.allow_all_calls,
                            allowed: p.allowed_members.iter().filter_map(recipient_remote).collect(),
                            scheduled: p.schedule_enabled,
                            start: p.schedule_start_time,
                            end: p.schedule_end_time,
                            days: p.schedule_days_enabled.clone(),
                        });
                    }
                }
                Some(storage_record::Record::StoryDistributionList(list)) => {
                    if let Some(list) = story_list(list) {
                        kept.story_lists.push(list);
                    }
                }
                Some(storage_record::Record::Account(a)) => {
                    kept.pinned = a.pinned_conversations.iter().filter_map(pinned_remote).collect();
                    kept.own_username = text(&a.username);
                    kept.own_username_link = a.username_link.as_ref().filter(|l| l.entropy.len() == 32).and_then(|l| Uuid::from_slice(&l.server_id).ok().map(|h| (l.entropy.clone(), h)));
                    kept.account = Some(account_settings(a));
                    {
                        use presage::libsignal_service::proto::account_record::notification_profile_manual_override::Override;
                        match a.notification_profile_manual_override.as_ref().and_then(|o| o.r#override.as_ref()) {
                            Some(Override::Enabled(on)) => kept.profile_on = Uuid::from_slice(&on.id).ok().map(|id| (id, on.end_at_timestamp_ms)),
                            Some(Override::DisabledAtTimestampMs(at)) => kept.profiles_off_at = *at,
                            None => {}
                        }
                    }
                    kept.note_to_self = Standing { archived: a.note_to_self_archived, marked_unread: a.note_to_self_marked_unread, ..Default::default() };
                }
                _ => {}
            }
        }
        kept.chat_folders.sort_by_key(|f| f.position);
        kept
    }

    pub fn load(store: &SignalStore) -> Self {
        store.blob(NAME).ok().flatten().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    pub fn save(&self, store: &SignalStore) {
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = store.set_blob(NAME, &bytes);
        }
    }

    /// Whether anything has been read: until it has, nothing is decided from it (no conversation is held back as a
    /// request on a guess).
    pub fn known(&self) -> bool {
        self.version > 0
    }

    /// The account's Signal connections, as Signal's clients count them for My Story: people it has accepted (shares
    /// its profile with) or saved, not blocked, hidden or gone from Signal.
    pub fn connections(&self) -> Vec<Uuid> {
        self.conversations
            .iter()
            .filter(|(remote, s)| remote.starts_with("p:") && (s.accepted || s.number.is_some()) && !s.blocked && !s.hidden && !s.unregistered)
            .filter_map(|(remote, _)| Uuid::parse_str(&remote[2..]).ok())
            .collect()
    }

    pub fn standing(&self, remote: &str) -> Option<&Standing> {
        self.conversations.get(remote)
    }

    pub fn blocked(&self, remote: &str) -> bool {
        self.standing(remote).is_some_and(|s| s.blocked)
    }

    /// Whether a conversation is a message request: somebody (or a group) the account has not accepted, and has not
    /// itself written to. Note to Self never is; neither is anything while the storage service is not known.
    pub fn is_request(&self, remote: &str, own_remote: &str, account_has_written: bool) -> bool {
        if !self.known() || remote == own_remote || account_has_written {
            return false;
        }
        match self.standing(remote) {
            Some(s) => !s.accepted,
            // Nobody the phone knows: a stranger.
            None => true,
        }
    }

    /// The person the phone knows by this number (digits compared, so spaces and dashes do not matter).
    pub fn person_with_number(&self, number: &str) -> Option<Uuid> {
        let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
        let wanted = digits(number);
        if wanted.len() < 6 {
            return None;
        }
        self.conversations.iter().find(|(remote, s)| remote.starts_with("p:") && s.number.as_deref().is_some_and(|n| digits(n) == wanted)).and_then(|(remote, _)| Uuid::parse_str(&remote[2..]).ok())
    }

    /// The person with this username, if the phone has them.
    pub fn person_with_username(&self, username: &str) -> Option<Uuid> {
        let wanted = username.trim_start_matches('@').to_lowercase();
        self.conversations.iter().find(|(remote, s)| remote.starts_with("p:") && s.username.as_deref().is_some_and(|u| u.to_lowercase() == wanted)).and_then(|(remote, _)| Uuid::parse_str(&remote[2..]).ok())
    }

    /// The pinned position of a conversation, if it is pinned.
    pub fn pin_index(&self, remote: &str) -> Option<usize> {
        self.pinned.iter().position(|p| p == remote)
    }
}

/// The block list a `Blocked` sync carries: every person and group blocked.
pub fn blocked_list(kept: &Kept) -> presage::proto::sync_message::Blocked {
    let mut list = presage::proto::sync_message::Blocked::default();
    for (remote, standing) in kept.conversations.iter().filter(|(_, s)| s.blocked) {
        if let Some(aci) = remote.strip_prefix("p:").and_then(|a| Uuid::parse_str(a).ok()) {
            list.acis.push(aci.to_string());
            list.acis_binary.push(aci.as_bytes().to_vec());
        } else if let Some(key) = remote.strip_prefix("g:").and_then(ids::unhex) {
            if let Ok(key) = <[u8; 32]>::try_from(key.as_slice()) {
                list.group_ids.push(super::commands::group_identifier(&key));
            }
        }
        let _ = standing;
    }
    list
}

/// What has been read for each account, so names and requests are answered without opening the file each time.
fn cache() -> &'static Mutex<HashMap<String, Arc<Kept>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Kept>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// What is known for the account.
pub fn current(account_id: &str, store: &SignalStore) -> Arc<Kept> {
    if let Some(found) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(account_id) {
        return found.clone();
    }
    let kept = Arc::new(Kept::load(store));
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(account_id.to_string(), kept.clone());
    kept
}

/// Replaces what is known, here and in the account's file.
pub fn keep(account_id: &str, store: &SignalStore, kept: Kept) -> Arc<Kept> {
    kept.save(store);
    let kept = Arc::new(kept);
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(account_id.to_string(), kept.clone());
    kept
}

/// Changes one conversation's standing here (an answer given in moho, before the phone's copy says the same).
pub fn amend(account_id: &str, store: &SignalStore, remote: &str, change: impl FnOnce(&mut Standing)) -> Arc<Kept> {
    let mut kept = (*current(account_id, store)).clone();
    change(kept.conversations.entry(remote.to_string()).or_default());
    keep(account_id, store, kept)
}

pub fn forget(account_id: &str) {
    cache().lock().unwrap_or_else(|e| e.into_inner()).remove(account_id);
}

/// How many records are asked for at once.
const READ_CHUNK: usize = 100;

/// Reads the storage service if it changed since `known`. `None` when it has not.
pub async fn fetch(manager: &presage::Manager<SignalStore, presage::manager::Registered>, known: u64) -> Result<Option<Kept>, String> {
    let storage = manager.storage_service().await.map_err(|e| format!("{e}"))?;
    let manifest = if known == 0 {
        Some(storage.manifest().await.map_err(|e| format!("{e}"))?)
    } else {
        storage.manifest_if_changed(known).await.map_err(|e| format!("{e}"))?
    };
    let Some(manifest) = manifest else { return Ok(None) };
    use manifest_record::identifier::Type;
    let wanted: Vec<Vec<u8>> = manifest
        .identifiers
        .iter()
        .filter(|id| matches!(id.r#type(), Type::Contact | Type::Groupv2 | Type::Account | Type::StoryDistributionList | Type::ChatFolder | Type::NotificationProfile))
        .map(|id| id.raw.clone())
        .collect();
    let mut records = Vec::with_capacity(wanted.len());
    for chunk in wanted.chunks(READ_CHUNK) {
        records.extend(storage.read_items(chunk.to_vec(), Some(&manifest.record_ikm)).await.map_err(|e| format!("{e}"))?);
    }
    Ok(Some(Kept::from_records(manifest.version, &records)))
}

/// Reads the storage service if it changed, keeps what it says, and acts on it: names, settings, mutes and the
/// requests it answers. Failures are said in the log and leave what was known.
pub async fn refresh(state: &crate::state::AppState, handle: &super::Handle, manager: &presage::Manager<SignalStore, presage::manager::Registered>) {
    let account_id = handle.account_id.as_str();
    let store = manager.store().clone();
    let before = current(account_id, &store);
    // A copy read before story lists were read is read again in full.
    let known = if before.reads_story_lists && before.reads_folders { before.version } else { 0 };
    let fetched = tokio::time::timeout(std::time::Duration::from_secs(30), fetch(manager, known)).await;
    let kept = match fetched {
        Ok(Ok(Some(kept))) => keep(account_id, &store, kept),
        Ok(Ok(None)) => {
            tracing::debug!("signal[{account_id}]: the storage service has not changed");
            return;
        }
        Ok(Err(e)) => {
            tracing::info!("signal[{account_id}]: the storage service was not read: {e}");
            return;
        }
        Err(_) => {
            tracing::info!("signal[{account_id}]: the storage service did not answer in time");
            return;
        }
    };
    tracing::info!("signal[{account_id}]: the storage service was read (version {}, {} conversations)", kept.version, kept.conversations.len());
    let ctx = super::people::Ctx::new(state, account_id, store, handle.own);
    arrived(&ctx, handle, &kept, &before).await;
}

/// Whether the phone's unread mark on a conversation is something to act on: `Some(true)` while it is marked (and on
/// the first reading), `Some(false)` when the mark was just taken off, `None` when there is nothing to do.
fn marked_unread_now(before: &Kept, now: &Kept, remote: &str) -> Option<bool> {
    let was = before.standing(remote).is_some_and(|s| s.marked_unread);
    let is = now.standing(remote).is_some_and(|s| s.marked_unread);
    match (was, is) {
        (_, true) => Some(true),
        (true, false) => Some(false),
        (false, false) => None,
    }
}

/// Acts on what was read.
/// The local day (1 Monday to 7) and the 24-hour clock as hhmm, as a notification profile's schedule reads them.
fn local_now() -> (i32, u32) {
    use chrono::{Datelike, Timelike};
    let now = chrono::Local::now();
    (now.weekday().number_from_monday() as i32, now.hour() * 100 + now.minute())
}

/// Puts the account's notification profile into effect: while one is on, only the people it allows (and mentions, if
/// it says so) are announced here, as on the phone. Said to the window when what is on changes.
pub fn apply_profile(state: &crate::state::AppState, account_id: &str, kept: &Kept) {
    let (day, clock) = local_now();
    let now = super::commands::now_ms();
    let active = kept.active_profile(now, day, clock);
    state.runtime.set_quiet(account_id, active.map(|p| crate::runtime::Quiet { allowed: p.allowed.clone(), mentions: p.allow_mentions }));
    let said = active.map(|p| p.id);
    let mut last = profile_said().lock().unwrap_or_else(|e| e.into_inner());
    if last.get(account_id) != Some(&said) {
        last.insert(account_id.to_string(), said);
        state.events.emit("signalProfile", serde_json::json!({ "accountId": account_id, "profile": active.map(|p| serde_json::json!({ "id": p.id.to_string(), "name": p.name, "emoji": p.emoji })) }));
    }
}

fn profile_said() -> &'static Mutex<HashMap<String, Option<Uuid>>> {
    static SAID: OnceLock<Mutex<HashMap<String, Option<Uuid>>>> = OnceLock::new();
    SAID.get_or_init(Default::default)
}

/// The account's chat folders as the window draws them (the shape Telegram's folders have): each with the
/// conversations it holds, by remote id. Numbered from 2 in the phone's order.
pub fn folders_for_window(state: &crate::state::AppState, account_id: &str, kept: &Kept) -> serde_json::Value {
    let now = super::commands::now_ms();
    let remotes: Vec<String> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == account_id).filter_map(|b| b.remote_id).filter(|r| r.starts_with("p:") || r.starts_with("g:")).collect();
    serde_json::Value::Array(
        kept.chat_folders
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let members: Vec<&String> = remotes.iter().filter(|r| f.holds(r, kept.standing(r), now)).collect();
                serde_json::json!({ "id": i as i64 + 2, "name": f.name, "emoticon": null, "members": members, "shared": false, "signal": true })
            })
            .collect(),
    )
}

/// Tells the window the account's folders (after the storage service was read, or conversations came and went).
pub fn say_folders(state: &crate::state::AppState, account_id: &str, kept: &Kept) {
    state.events.emit("signalFolders", serde_json::json!({ "accountId": account_id, "folders": folders_for_window(state, account_id, kept) }));
}

async fn arrived(ctx: &super::people::Ctx<'_>, handle: &super::Handle, kept: &Kept, before: &Kept) {
    apply_profile(ctx.state, ctx.account_id, kept);
    say_folders(ctx.state, ctx.account_id, kept);
    // The account's settings, where the phone keeps them for every device: more current than a configuration sync.
    if let Some(account) = kept.account {
        let synced = super::memory::Synced {
            read_receipts: Some(account.read_receipts),
            typing_indicators: Some(account.typing_indicators),
            link_previews: Some(account.link_previews),
            unidentified_delivery_indicators: Some(account.sealed_sender_indicators),
        };
        handle.memory.set_synced(synced);
        synced.save(&ctx.store);
        super::network::allow_link_previews(account.link_previews);
    }
    super::inbox::rename_all(ctx).await;
    // Mutes set on the phone hold here; a conversation the phone does not mute keeps whatever was chosen in moho.
    let now = now_ms();
    for buffer in ctx.state.runtime.list_buffers().into_iter().filter(|b| b.account_id == ctx.account_id) {
        let Some(remote) = buffer.remote_id.as_deref() else { continue };
        // Marked unread on the phone: counted as one waiting here, as Signal's clients draw it; and read there (the mark
        // taken off) is read here too - it stayed at one before (seen on the first real account).
        if marked_unread_now(before, kept, remote) == Some(true) && buffer.unread_seed == 0 {
            ctx.state.runtime.set_buffer_unread(ctx.state, &buffer.id, 1, 0);
        } else if marked_unread_now(before, kept, remote) == Some(false) {
            ctx.state.runtime.set_buffer_unread(ctx.state, &buffer.id, 0, 0);
        }
        let muted = kept.standing(remote).is_some_and(|s| s.muted(now));
        if muted {
            ctx.state.runtime.set_notify_level(ctx.state, &buffer.id, crate::notify::NotifyLevel::Mute, crate::notify::NotifyLevel::Mute);
        } else {
            ctx.state.runtime.release_notify_level(ctx.state, &buffer.id);
        }
    }
    // Requests the phone has answered since they were held.
    for (remote, _) in super::requests::load(&ctx.store) {
        let Some(thread) = ids::parse_remote_id(&remote) else { continue };
        match kept.standing(&remote) {
            Some(s) if s.blocked => super::requests::apply(ctx, &thread, super::requests::Answer::Block).await,
            Some(s) if s.accepted => super::requests::apply(ctx, &thread, super::requests::Answer::Accept).await,
            _ => {}
        }
    }
}

/// The time now in ms, for asking whether something is muted.
pub fn now_ms() -> u64 {
    super::commands::now_ms()
}

/// Whether a contact record names a person who is verified, for the person card. Unused fields of the record stay
/// the library's.
pub fn identity_verified(record: &ContactRecord) -> bool {
    record.identity_state() == contact_record::IdentityState::Verified
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phones_unread_mark_counts_while_it_is_there_and_is_taken_off_when_it_goes() {
        let marked = |on: bool| Kept { conversations: [("g:aa".to_string(), Standing { marked_unread: on, ..Default::default() })].into_iter().collect(), ..Default::default() };
        let none = Kept::default();
        assert_eq!(marked_unread_now(&none, &marked(true), "g:aa"), Some(true));
        assert_eq!(marked_unread_now(&marked(true), &marked(true), "g:aa"), Some(true));
        // Read on the phone: the mark comes off (seen on the first real account, where moho stayed at one).
        assert_eq!(marked_unread_now(&marked(true), &marked(false), "g:aa"), Some(false));
        assert_eq!(marked_unread_now(&marked(false), &marked(false), "g:aa"), None);
        assert_eq!(marked_unread_now(&none, &none, "g:aa"), None);
    }
    use presage::libsignal_service::proto::{account_record::PinnedConversation, contact_record::Name};
    use presage::model::identity::OnNewIdentity;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";

    fn contact(aci: &str, change: impl FnOnce(&mut ContactRecord)) -> StorageRecord {
        let mut c = ContactRecord { aci: aci.into(), ..Default::default() };
        change(&mut c);
        StorageRecord { record: Some(storage_record::Record::Contact(c)) }
    }

    #[test]
    fn my_story_goes_to_connections_but_those_left_out_and_a_list_to_its_people() {
        use presage::libsignal_service::proto::StoryDistributionListRecord;
        let my_story = StorageRecord {
            record: Some(storage_record::Record::StoryDistributionList(StoryDistributionListRecord {
                identifier: MY_STORY.as_bytes().to_vec(),
                is_block_list: true,
                allows_replies: true,
                recipient_service_ids: vec![B.to_string()],
                ..Default::default()
            })),
        };
        let gone = StorageRecord {
            record: Some(storage_record::Record::StoryDistributionList(StoryDistributionListRecord {
                identifier: Uuid::from_u128(5).as_bytes().to_vec(),
                name: "Old".into(),
                deleted_at_timestamp: 1,
                ..Default::default()
            })),
        };
        let close = StorageRecord {
            record: Some(storage_record::Record::StoryDistributionList(StoryDistributionListRecord {
                identifier: Uuid::from_u128(6).as_bytes().to_vec(),
                name: "Close friends".into(),
                recipient_service_ids_binary: vec![Uuid::parse_str(A).unwrap().as_bytes().to_vec()],
                ..Default::default()
            })),
        };
        let kept = Kept::from_records(4, &[contact(A, |c| c.whitelisted = true), contact(B, |c| c.whitelisted = true), my_story, gone, close]);
        assert!(kept.reads_story_lists);
        assert_eq!(kept.story_lists.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["My Story", "Close friends"]);
        let own = Uuid::from_u128(1);
        assert_eq!(kept.story_lists[0].audience(&kept, &own), vec![Uuid::parse_str(A).unwrap()], "everybody but B");
        assert_eq!(kept.story_lists[1].audience(&kept, &own), vec![Uuid::parse_str(A).unwrap()]);
    }

    #[test]
    fn a_folder_holds_by_rule_and_by_name_and_a_profile_runs_on_its_schedule() {
        let folder = ChatFolder { all_groups: true, included: vec!["p:1".into()], excluded: vec!["g:aa".into()], ..Default::default() };
        let muted = Standing { muted_until: u64::MAX, ..Default::default() };
        assert!(folder.holds("p:1", None, 0));
        assert!(!folder.holds("p:2", None, 0), "people are not in it by rule");
        assert!(folder.holds("g:bb", None, 0));
        assert!(!folder.holds("g:aa", None, 0), "named out");
        assert!(!folder.holds("g:bb", Some(&muted), 0), "muted chats only when it says so");
        let work = NotificationProfile { scheduled: true, start: 900, end: 1700, days: vec![1, 2, 3, 4, 5], ..Default::default() };
        assert!(work.scheduled_on(1, 930) && !work.scheduled_on(6, 930) && !work.scheduled_on(1, 1700));
        let night = NotificationProfile { scheduled: true, start: 2200, end: 700, days: vec![5], ..Default::default() };
        assert!(night.scheduled_on(5, 2300) && night.scheduled_on(6, 600) && !night.scheduled_on(6, 2300));
        let mut kept = Kept { notification_profiles: vec![work.clone()], ..Default::default() };
        assert!(kept.active_profile(10, 1, 1000).is_some());
        kept.profiles_off_at = 5;
        assert!(kept.active_profile(10, 1, 1000).is_none(), "turned off by hand");
    }

    #[test]
    fn story_views_follow_read_receipts_unless_set() {
        let mut a = AccountSettings { read_receipts: true, ..Default::default() };
        assert!(a.tells_story_views());
        a.story_view_receipts = Some(false);
        assert!(!a.tells_story_views());
        let record = AccountRecord { read_receipts: false, story_view_receipts_enabled: 1, ..Default::default() };
        assert!(account_settings(&record).tells_story_views());
    }

    #[test]
    fn a_nickname_comes_before_the_address_book_and_the_flags_are_read() {
        let records = vec![
            contact(A, |c| {
                c.system_given_name = "Sam".into();
                c.system_family_name = "Jones".into();
                c.nickname = Some(Name { given: "Sammy".into(), family: String::new() });
                c.whitelisted = true;
                c.muted_until_timestamp = u64::MAX;
                c.username = "sam.01".into();
                c.e164 = "+15550100123".into();
            }),
            contact(B, |c| {
                c.system_nickname = "Bee".into();
                c.blocked = true;
                c.archived = true;
            }),
        ];
        let kept = Kept::from_records(7, &records);
        let a = kept.standing(&format!("p:{A}")).unwrap();
        assert_eq!((a.name.as_deref(), a.username.as_deref(), a.accepted, a.muted(1)), (Some("Sammy"), Some("sam.01"), true, true));
        let b = kept.standing(&format!("p:{B}")).unwrap();
        assert_eq!((b.name.as_deref(), b.blocked, b.archived, b.accepted), (Some("Bee"), true, true, false));
        assert!(kept.blocked(&format!("p:{B}")));
        assert_eq!(kept.person_with_number("+1 555-010-0123"), Some(Uuid::parse_str(A).unwrap()));
        assert_eq!(kept.person_with_number("0123"), None, "too short to mean anybody");
        assert_eq!(kept.person_with_username("@Sam.01"), Some(Uuid::parse_str(A).unwrap()));
    }

    #[test]
    fn the_binary_aci_is_read_when_there_is_one() {
        let uuid = Uuid::parse_str(A).unwrap();
        let kept = Kept::from_records(1, &[contact("", |c| c.aci_binary = uuid.as_bytes().to_vec())]);
        assert!(kept.standing(&format!("p:{A}")).is_some());
    }

    #[test]
    fn groups_pins_and_the_accounts_settings() {
        let key = [7u8; 32];
        let group = StorageRecord { record: Some(storage_record::Record::GroupV2(GroupV2Record { master_key: key.to_vec(), whitelisted: true, marked_unread: true, ..Default::default() })) };
        let pins = vec![
            PinnedConversation { identifier: Some(account_record::pinned_conversation::Identifier::GroupMasterKey(key.to_vec())) },
            PinnedConversation {
                identifier: Some(account_record::pinned_conversation::Identifier::Contact(account_record::pinned_conversation::Contact { service_id: A.into(), ..Default::default() })),
            },
        ];
        let account = StorageRecord {
            record: Some(storage_record::Record::Account(AccountRecord { pinned_conversations: pins, read_receipts: true, link_previews: false, universal_expire_timer: 3600, username: "me.42".into(), ..Default::default() })),
        };
        let kept = Kept::from_records(3, &[group, account]);
        let g = format!("g:{}", ids::hex(&key));
        assert!(kept.standing(&g).unwrap().marked_unread);
        assert_eq!(kept.pin_index(&g), Some(0));
        assert_eq!(kept.pin_index(&format!("p:{A}")), Some(1));
        assert_eq!(kept.own_username.as_deref(), Some("me.42"));
        let settings = kept.account.unwrap();
        assert_eq!((settings.read_receipts, settings.link_previews, settings.default_timer), (true, false, 3600));
    }

    #[test]
    fn a_request_is_a_stranger_or_an_unaccepted_one_and_never_on_a_guess() {
        let own = format!("p:{B}");
        let unread = Kept::default();
        assert!(!unread.is_request(&format!("p:{A}"), &own, false), "nothing is held back before the storage service is read");
        let kept = Kept::from_records(2, &[contact(A, |c| c.whitelisted = false)]);
        assert!(kept.is_request(&format!("p:{A}"), &own, false));
        assert!(!kept.is_request(&format!("p:{A}"), &own, true), "the account wrote to them");
        assert!(kept.is_request("p:33333333-3333-4333-8333-333333333333", &own, false), "a stranger");
        assert!(!kept.is_request(&own, &own, false), "Note to Self");
        let accepted = Kept::from_records(2, &[contact(A, |c| c.whitelisted = true)]);
        assert!(!accepted.is_request(&format!("p:{A}"), &own, false));
    }

    #[test]
    fn the_block_list_names_every_blocked_person_and_group() {
        let key = [4u8; 32];
        let group = StorageRecord { record: Some(storage_record::Record::GroupV2(GroupV2Record { master_key: key.to_vec(), blocked: true, ..Default::default() })) };
        let kept = Kept::from_records(1, &[contact(A, |c| c.blocked = true), contact(B, |_| {}), group]);
        let list = blocked_list(&kept);
        assert_eq!(list.acis, vec![A.to_string()]);
        assert_eq!(list.acis_binary.len(), 1);
        assert_eq!(list.group_ids, vec![super::super::commands::group_identifier(&key)]);
    }

    #[test]
    fn what_was_read_survives_a_restart_and_an_answer_here_is_kept() {
        let store = SignalStore::open_in_memory(OnNewIdentity::Trust).unwrap();
        let kept = Kept::from_records(9, &[contact(A, |c| c.whitelisted = false)]);
        keep("signal:test-storage", &store, kept.clone());
        forget("signal:test-storage");
        assert_eq!(*current("signal:test-storage", &store), kept);
        amend("signal:test-storage", &store, &format!("p:{A}"), |s| s.accepted = true);
        forget("signal:test-storage");
        assert!(current("signal:test-storage", &store).standing(&format!("p:{A}")).unwrap().accepted);
    }
}
