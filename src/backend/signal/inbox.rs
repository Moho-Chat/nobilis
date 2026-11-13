//! What the library hands back, as the daemon's conversations and messages.
//!
//! Everything arrives as a `Content` the library has already decrypted and kept in the account's store. This reads what
//! is in it: a message from somebody, a message the account sent from its phone (which Signal tells every linked device
//! about so the conversation looks the same everywhere), an edit, a reaction, a delete, a change of the disappearing
//! timer. What it does not draw yet it says so about, in the log, never silently as though it were nothing.

use super::ids;
use super::people::{self, Ctx};
use super::ranges;
use super::requests::Gate;
use super::store::SignalStore;
use super::Handle;
use crate::model::{Buffer, ReplyPreview};
use crate::state::AppState;
use presage::{
    libsignal_service::{
        content::{Content, ContentBody},
        protocol::ServiceId,
    },
    manager::Registered,
    proto::{receipt_message, sync_message::Sent, typing_message, DataMessage, EditMessage, ReceiptMessage, SyncMessage, TypingMessage},
    store::{ContentsStore, Thread},
    Manager,
};
use std::collections::HashMap;
use uuid::Uuid;

/// `DataMessage.flags` bits.
const END_SESSION: u32 = 1;
const EXPIRATION_TIMER_UPDATE: u32 = 2;
const PROFILE_KEY_UPDATE: u32 = 4;

/// `AttachmentPointer.flags` bits.
const VOICE_MESSAGE: u32 = 1;
const GIF: u32 = 8;

fn ctx<'a>(state: &'a AppState, handle: &'a Handle, manager: &Manager<SignalStore, Registered>) -> Ctx<'a> {
    ctx_for(state, &handle.account_id, manager)
}

/// What this account's own messages are signed with: its profile name if the store has one, else its number masked.
pub async fn announce_self(state: &AppState, config: &crate::accounts::SignalAccountConfig, manager: &Manager<SignalStore, Registered>) {
    let own = Uuid::from(manager.registration_data().service_ids.aci());
    let key = manager.registration_data().profile_key();
    let named = match manager.store().profile(own, key).await {
        Ok(Some(profile)) => profile.name.map(|n| n.to_string()).filter(|n| !n.trim().is_empty()),
        _ => None,
    };
    let label = named.or_else(|| Some(config.masked_number()).filter(|n| !n.is_empty())).unwrap_or_else(|| "Me".to_string());
    state.runtime.set_own_identity(&config.account_id(), &label);
}

/// A person's profile has been fetched: what they are called and what they look like may have changed.
pub async fn profile_arrived(ctx: &Ctx<'_>, who: &Uuid) {
    if ctx.is_own(who) {
        // The account's own name, for what it says from here.
        let key = presage::libsignal_service::protocol::ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(*who));
        if let Ok(Some(k)) = ctx.store.profile_key(&key).await {
            if let Ok(Some(profile)) = ctx.store.profile(*who, k).await {
                if let Some(name) = profile.name.map(|n| n.to_string()).filter(|n| !n.trim().is_empty()) {
                    ctx.state.runtime.set_own_identity(ctx.account_id, &name);
                }
            }
        }
        return;
    }
    rename_known(ctx).await;
    let remote = ids::remote_id(&Thread::Contact(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(*who))));
    if let Some(avatar) = super::media::avatar_of(ctx.account_id, who) {
        if let Some(buffer) = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str())) {
            ctx.state.runtime.set_buffer_avatar(ctx.state, &buffer.id, &avatar);
        }
    }
}

/// Asks for the profile of a person the account cannot name yet, once per connection.
pub async fn want_profile(ctx: &Ctx<'_>, handle: &Handle, who: &Uuid) {
    if ctx.is_own(who) || people::has_name(ctx, who).await {
        return;
    }
    let first = handle.memory.asked_profiles.lock().unwrap_or_else(|e| e.into_inner()).insert(*who);
    if first {
        let _ = handle.commands.send(super::Command::Profile { who: *who });
    }
}

/// Opens (or finds) the conversation with a person, named by their ACI, their username (`name.42`, with or without an
/// `@`), or a number the phone already knows. A number nobody has given the account is not looked up: that needs
/// Signal's contact discovery, which moho does not do (see the plan's Round 12).
pub async fn open_dm(state: &AppState, handle: &Handle, user_id: &str) -> anyhow::Result<String> {
    let wanted = user_id.trim();
    let wanted = wanted.strip_prefix("p:").unwrap_or(wanted);
    let who = match Uuid::parse_str(wanted) {
        Ok(who) => who,
        Err(_) => find_person(state, handle, wanted).await?,
    };
    let ctx = Ctx::new(state, &handle.account_id, handle.store.clone(), handle.own);
    let thread = Thread::Contact(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(who)));
    Ok(people::buffer_for(&ctx, &thread).await.id)
}

/// Who a username or number names.
pub(super) async fn find_person(state: &AppState, handle: &Handle, wanted: &str) -> anyhow::Result<Uuid> {
    let kept = super::storage::current(&handle.account_id, &handle.store);
    let looks_like_number = wanted.starts_with('+') || wanted.chars().all(|c| c.is_ascii_digit() || " -().".contains(c));
    if looks_like_number {
        if let Some(who) = kept.person_with_number(wanted) {
            return Ok(who);
        }
        // The old contact sync carries numbers too.
        let digits: String = wanted.chars().filter(char::is_ascii_digit).collect();
        if let Ok(contacts) = handle.store.contacts().await {
            for contact in contacts.filter_map(Result::ok) {
                if contact.phone_number.as_ref().is_some_and(|n| n.to_string().chars().filter(char::is_ascii_digit).collect::<String>() == digits) {
                    return Ok(contact.uuid);
                }
            }
        }
        anyhow::bail!("Nobody with that number is in this account's contacts. Signal finds a number only through its contact discovery, which moho does not use: ask them for their username, or add them on your phone first");
    }
    let name = wanted.trim_start_matches('@');
    if let Some(who) = kept.person_with_username(name) {
        return Ok(who);
    }
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(super::Command::LookUpUsername { name: name.to_string(), done }).map_err(|_| anyhow::anyhow!("that Signal account is not connected"))?;
    let _ = state;
    match tokio::time::timeout(std::time::Duration::from_secs(30), answer).await {
        Ok(Ok(Ok(Some(who)))) => Ok(who),
        Ok(Ok(Ok(None))) => anyhow::bail!("Nobody on Signal has the username {name}. A username has a number after a dot, like name.42"),
        Ok(Ok(Err(e))) => anyhow::bail!("{e}"),
        Ok(Err(_)) => anyhow::bail!("the connection ended before Signal answered"),
        Err(_) => anyhow::bail!("Signal did not answer in time"),
    }
}

/// Opens a conversation for each group the account is in, so a quiet group is on the list too.
pub async fn open_groups(state: &AppState, account_id: &str, manager: &Manager<SignalStore, Registered>) {
    let ctx = ctx_for(state, account_id, manager);
    let Ok(groups) = manager.store().groups().await else { return };
    let keys: Vec<[u8; 32]> = groups.filter_map(|g| g.ok()).map(|(key, _)| key).collect();
    for key in keys {
        if ctx.store.is_group_left(key).await.unwrap_or(false) {
            continue;
        }
        people::buffer_for(&ctx, &Thread::Group(key)).await;
        super::groups::publish_members(&ctx, &key).await;
    }
}

fn ctx_for<'a>(state: &'a AppState, account_id: &'a str, manager: &Manager<SignalStore, Registered>) -> Ctx<'a> {
    let own = Uuid::from(manager.registration_data().service_ids.aci());
    Ctx::new(state, account_id, manager.store().clone(), own)
}

pub async fn contacts_arrived(state: &AppState, handle: &Handle, manager: &Manager<SignalStore, Registered>) {
    let ctx = ctx(state, handle, manager);
    tracing::info!("signal[{}]: the phone sent its contacts", ctx.account_id);
    // Conversations already open are named again: a contact may have had no name until now.
    rename_known(&ctx).await;
}

/// Gives every open conversation the name it has now (after the storage service was read).
pub(super) async fn rename_all(ctx: &Ctx<'_>) {
    rename_known(ctx).await;
}

/// Gives every open conversation the name it has now.
async fn rename_known(ctx: &Ctx<'_>) {
    for buffer in ctx.state.runtime.list_buffers().into_iter().filter(|b| b.account_id == ctx.account_id) {
        let Some(thread) = buffer.remote_id.as_deref().and_then(ids::parse_remote_id) else { continue };
        let title = match &thread {
            Thread::Contact(id) if ids::uuid_of(id) == ctx.own => continue,
            Thread::Contact(id) => people::person(ctx, &ids::uuid_of(id)).await,
            Thread::Group(key) => people::group_title(ctx, key).await,
        };
        if title != buffer.name && !title.starts_with("Signal user") {
            ctx.state.runtime.rename_buffer(ctx.state, ctx.account_id, &buffer.name, &title, &buffer.kind);
        }
    }
}

pub async fn decryption_failed(state: &AppState, handle: &Handle, _manager: &mut Manager<SignalStore, Registered>, _sender: ServiceId) {
    // The library keeps going; the other side is asked for the message again by a retry receipt it sends itself.
    tracing::warn!("signal[{}]: a message could not be decrypted", handle.account_id);
    let _ = state;
}

/// One decrypted message, of whatever kind.
pub async fn content(state: &AppState, handle: &Handle, manager: &Manager<SignalStore, Registered>, content: Content) {
    let ctx = ctx(state, handle, manager);
    let sender = ids::uuid_of(&content.metadata.sender);
    let sent_at = content.metadata.client_timestamp.timestamp_millis().max(0) as u64;
    if matches!(content.body, ContentBody::DataMessage(_) | ContentBody::EditMessage(_)) {
        // Somebody not in the address book is called what they call themselves, which takes their profile.
        want_profile(&ctx, handle, &sender).await;
    }
    announce_changed_identities(&ctx, sent_at).await;
    match &content.body {
        ContentBody::DataMessage(dm) => {
            let Ok(thread) = Thread::try_from(&content) else { return };
            match super::requests::gate(&ctx, &thread, &sender).await {
                Gate::Drop => return,
                Gate::Hold => {
                    // Kept by the library already; the window hears only that somebody is asking. Delivered is said,
                    // as Signal's own clients say it: read is not, until the request is accepted and read.
                    if shows_something(dm) {
                        super::requests::hold(&ctx, &thread, &sender, dm.timestamp.unwrap_or(sent_at), dm.body.as_deref().unwrap_or("")).await;
                    }
                    delivered(handle, &content, dm.timestamp.unwrap_or(sent_at));
                    return;
                }
                Gate::Show => {}
            }
            data_message(&ctx, &thread, sender, sent_at, dm).await;
            delivered(handle, &content, dm.timestamp.unwrap_or(sent_at));
            announce_changed_identities(&ctx, sent_at).await;
        }
        ContentBody::EditMessage(edit) => {
            let Ok(thread) = Thread::try_from(&content) else { return };
            if super::requests::gate(&ctx, &thread, &sender).await != Gate::Show {
                // Held edits are played with the rest when the request is accepted.
                return;
            }
            edited(&ctx, &thread, sender, edit).await;
            delivered(handle, &content, edit.data_message.as_ref().and_then(|d| d.timestamp).unwrap_or(sent_at));
        }
        ContentBody::SynchronizeMessage(sync) => synchronized(&ctx, handle, &content, sync).await,
        ContentBody::TypingMessage(typing) => typing_seen(&ctx, handle, &content, typing).await,
        ContentBody::ReceiptMessage(receipt) => receipt_seen(&ctx, handle, &content, receipt).await,
        ContentBody::NullMessage(_) => {}
        ContentBody::CallMessage(_) => tracing::debug!("signal[{}]: a call message (calls are not handled yet)", ctx.account_id),
        ContentBody::StoryMessage(story) => {
            // Nothing from somebody blocked; a story is otherwise shown whoever posted it, as Signal's clients do for
            // the people the account has accepted (a stranger's do not reach it).
            let blocked = super::storage::current(ctx.account_id, &ctx.store).blocked(&format!("p:{sender}"));
            if !blocked {
                super::stories::arrived(&ctx, sender, sent_at, story).await;
            }
        }
        ContentBody::DecryptionErrorMessage(_) => {}
    }
}

/// Says, in the conversation with them, that somebody's safety number changed. The library trusts the new key (as Signal's
/// own clients do for anyone not marked verified), and what is owed is telling the person.
async fn announce_changed_identities(ctx: &Ctx<'_>, at_ms: u64) {
    for name in ctx.store.take_changed_identities() {
        let Some(who) = ServiceId::parse_from_service_id_string(&name).map(|id| ids::uuid_of(&id)) else { continue };
        if ctx.is_own(&who) {
            continue;
        }
        let thread = Thread::Contact(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(who)));
        let buffer = people::buffer_for(ctx, &thread).await;
        let person = people::person(ctx, &who).await;
        system(ctx, &buffer, &format!("Your safety number with {person} has changed. Check it in their profile if you want to be sure it is them"), at_ms);
    }
}

async fn synchronized(ctx: &Ctx<'_>, handle: &Handle, content: &Content, sync: &SyncMessage) {
    use presage::proto::sync_message::Content as Sync;
    match &sync.content {
        Some(Sync::Sent(sent)) => sent_from_phone(ctx, content, sent).await,
        Some(Sync::Configuration(said)) => {
            // The phone's privacy settings: what decides whether receipts and typing notices are sent at all.
            let synced = handle.memory.synced().with(said);
            handle.memory.set_synced(synced);
            super::network::allow_link_previews(synced.link_previews == Some(true));
            synced.save(&ctx.store);
            tracing::info!("signal[{}]: the phone's settings arrived", ctx.account_id);
        }
        Some(Sync::CallEvent(event)) => call_logged(ctx, content, event).await,
        Some(Sync::MessageRequestResponse(answer)) => {
            // Answered on the phone: the same here, without telling the phone again.
            let thread = match (ids::aci_of(answer.thread_aci.as_deref(), answer.thread_aci_binary.as_deref()), &answer.group_id) {
                (Some(who), _) => Some(Thread::Contact(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(who)))),
                (None, Some(group_id)) => thread_of_group_id(ctx, group_id).await,
                _ => None,
            };
            if let (Some(thread), Some(kind)) = (thread, answer.r#type.and_then(super::requests::Answer::from_response)) {
                super::requests::apply(ctx, &thread, kind).await;
            }
        }
        Some(Sync::Blocked(blocked)) => blocked_on_phone(ctx, blocked).await,
        Some(Sync::DeleteForMe(deletes)) => deleted_on_phone(ctx, deletes).await,
        Some(Sync::Verified(verified)) => {
            // Marked verified (or not) on the phone: kept with the person, for their card.
            if let Some(who) = ids::aci_of(verified.destination_aci.as_deref(), verified.destination_aci_binary.as_deref()) {
                let state = verified.state.unwrap_or(0);
                let key = format!("verified:{who}");
                let _ = ctx.store.set_blob(&key, if state == 1 { b"1" } else { b"0" });
            }
        }
        Some(Sync::FetchLatest(latest)) => {
            use presage::proto::sync_message::fetch_latest::Type;
            if latest.r#type == Some(Type::StorageManifest as i32) {
                let _ = handle.commands.send(super::Command::Storage);
            }
        }
        // The rest is taken up by the library or by later rounds.
        _ => {}
    }
    if !sync.read.is_empty() {
        read_on_phone(ctx, &sync.read).await;
    }
}

/// What the phone's call log says about a call, as a line in the conversation: moho cannot take part in a call, so what it
/// can honestly show is that one happened and how it ended.
pub fn call_words(kind: i32, direction: i32, event: i32) -> Option<String> {
    use presage::proto::sync_message::call_event::{Direction, Event, Type};
    let what = match Type::try_from(kind).unwrap_or(Type::UnknownType) {
        Type::VideoCall => "video call",
        Type::GroupCall | Type::AdHocCall => "group call",
        Type::AudioCall | Type::UnknownType => "voice call",
    };
    let incoming = Direction::try_from(direction).ok() == Some(Direction::Incoming);
    match Event::try_from(event).ok()? {
        Event::Accepted if incoming => Some(format!("Incoming {what}")),
        Event::Accepted => Some(format!("Outgoing {what}")),
        Event::NotAccepted if incoming => Some(format!("Missed {what}")),
        Event::NotAccepted => Some(format!("Outgoing {what}, not answered")),
        Event::Delete | Event::Observed | Event::UnknownEvent => None,
    }
}

async fn call_logged(ctx: &Ctx<'_>, content: &Content, event: &presage::proto::sync_message::CallEvent) {
    let Some(words) = call_words(event.r#type.unwrap_or(0), event.direction.unwrap_or(0), event.event.unwrap_or(0)) else { return };
    let thread = match &event.conversation_id {
        // A person is named by a service id; a group by its identifier.
        Some(id) if id.len() == 32 => match thread_of_group_id(ctx, id).await {
            Some(thread) => thread,
            None => return,
        },
        _ => match Thread::try_from(content) {
            Ok(thread) => thread,
            Err(_) => return,
        },
    };
    let buffer = people::buffer_for(ctx, &thread).await;
    let at = event.timestamp.unwrap_or_else(|| content.metadata.client_timestamp.timestamp_millis().max(0) as u64);
    let id = format!("call-{}-{}", event.call_id.unwrap_or(at), event.event.unwrap_or(0));
    ctx.state.runtime.record_message_at(ctx.state, ctx.account_id, &buffer.name, &buffer.kind, "Signal", &format!("{words}. Calls are answered in Signal on your phone"), false, "system", None, Some(id), false, None, Vec::new(), Vec::new(), None, Some(seconds(at)), None, None);
}

/// Tells the sender their message arrived, as every Signal client does. It says nothing about it having been read.
fn delivered(handle: &Handle, content: &Content, sent_ms: u64) {
    if !content.metadata.needs_receipt {
        return;
    }
    let to = content.metadata.sender;
    if ids::uuid_of(&to) == handle.own {
        return;
    }
    let _ = handle.commands.send(super::Command::Receipt { to, kind: receipt_message::Type::Delivery, timestamps: vec![sent_ms] });
}

/// The thread a group's identifier names: the one whose master key makes it.
async fn thread_of_group_id(ctx: &Ctx<'_>, group_id: &[u8]) -> Option<Thread> {
    let groups = ctx.store.groups().await.ok()?;
    groups.filter_map(Result::ok).map(|(key, _)| key).find(|key| super::commands::group_identifier(key) == group_id).map(Thread::Group)
}

/// Somebody is typing, or stopped.
async fn typing_seen(ctx: &Ctx<'_>, handle: &Handle, content: &Content, typing: &TypingMessage) {
    let who = ids::uuid_of(&content.metadata.sender);
    if who == handle.own {
        return;
    }
    let thread = match &typing.group_id {
        Some(group_id) => match thread_of_group_id(ctx, group_id).await {
            Some(thread) => thread,
            None => return,
        },
        None => Thread::Contact(content.metadata.sender),
    };
    // A typing notice does not open a conversation that is not there.
    let remote = ids::remote_id(&thread);
    let Some(buffer) = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str())) else { return };
    let started = typing.action.is_none_or(|a| a == typing_message::Action::Started as i32);
    if started {
        let name = people::person(ctx, &who).await;
        ctx.state.events.emit("typing", serde_json::json!({ "accountId": ctx.account_id, "bufferId": buffer.id, "nicks": [name], "expiresInMs": 15_000 }));
    } else if matches!(thread, Thread::Contact(_)) {
        ctx.state.events.emit("typing", serde_json::json!({ "accountId": ctx.account_id, "bufferId": buffer.id, "nicks": [], "expiresInMs": 0 }));
    }
}

/// Somebody read (or viewed) messages of the account's.
async fn receipt_seen(ctx: &Ctx<'_>, handle: &Handle, content: &Content, receipt: &ReceiptMessage) {
    let kind = receipt.r#type.unwrap_or(0);
    // Delivered is not drawn: there is nowhere in a conversation to put it yet.
    if kind != receipt_message::Type::Read as i32 && kind != receipt_message::Type::Viewed as i32 {
        return;
    }
    let reader = ids::uuid_of(&content.metadata.sender);
    let mine = ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(ctx.own));
    for &ts in &receipt.timestamp {
        let Ok(Some(thread)) = ctx.store.thread_for_sender_and_timestamp(&mine, ts).await else { continue };
        let remote = ids::remote_id(&thread);
        let Some(buffer) = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str())) else { continue };
        note_read(ctx, handle, &buffer, reader, ts).await;
    }
}

/// Moves a reader's marker in a conversation to message `ts`, if that is further than it was, and tells the window.
async fn note_read(ctx: &Ctx<'_>, handle: &Handle, buffer: &Buffer, reader: Uuid, ts: u64) {
    let readers = {
        let mut marks = handle.memory.read_marks.lock().unwrap_or_else(|e| e.into_inner());
        let here = marks.entry(buffer.id.clone()).or_default();
        let at = here.entry(reader).or_insert(0);
        if *at >= ts {
            return;
        }
        *at = ts;
        here.clone()
    };
    let mut list = Vec::new();
    for (who, at) in readers {
        list.push(serde_json::json!({ "userId": who.to_string(), "nick": people::person(ctx, &who).await, "messageId": ids::message_id(at, &ctx.own) }));
    }
    ctx.state.events.emit("readReceipts", serde_json::json!({ "accountId": ctx.account_id, "bufferId": buffer.id, "readers": list }));
}

/// Messages read on the phone: if that was everything in the conversation, it is not unread here either.
async fn read_on_phone(ctx: &Ctx<'_>, reads: &[presage::proto::sync_message::Read]) {
    for read in reads {
        let (Some(from), Some(ts)) = (ids::aci_of(read.sender_aci.as_deref(), read.sender_aci_binary.as_deref()), read.timestamp) else { continue };
        let sender = ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(from));
        let Ok(Some(thread)) = ctx.store.thread_for_sender_and_timestamp(&sender, ts).await else { continue };
        let remote = ids::remote_id(&thread);
        let Some(buffer) = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str())) else { continue };
        let newer = ctx
            .state
            .store
            .get_backlog(&buffer.id, i64::MAX, 50)
            .map(|rows| rows.iter().any(|m| !m.is_own && m.kind == "chat" && ids::parse_message_id(&m.id).is_some_and(|(sent, _)| sent > ts)))
            .unwrap_or(true);
        if !newer {
            ctx.state.events.emit("bufferRead", serde_json::json!({ "accountId": ctx.account_id, "bufferId": buffer.id }));
        }
    }
}

/// The phone's list of who is blocked, which replaces what was known.
async fn blocked_on_phone(ctx: &Ctx<'_>, blocked: &presage::proto::sync_message::Blocked) {
    let mut listed: Vec<String> = blocked.acis.iter().filter_map(|a| Uuid::parse_str(a).ok()).map(|u| format!("p:{u}")).collect();
    listed.extend(blocked.acis_binary.iter().filter_map(|b| Uuid::from_slice(b).ok()).map(|u| format!("p:{u}")));
    for group_id in &blocked.group_ids {
        if let Some(thread) = thread_of_group_id(ctx, group_id).await {
            listed.push(ids::remote_id(&thread));
        }
    }
    let mut kept = (*super::storage::current(ctx.account_id, &ctx.store)).clone();
    for (remote, standing) in kept.conversations.iter_mut() {
        standing.blocked = listed.contains(remote);
    }
    for remote in &listed {
        kept.conversations.entry(remote.clone()).or_default().blocked = true;
    }
    super::storage::keep(ctx.account_id, &ctx.store, kept);
    tracing::info!("signal[{}]: the phone's block list arrived ({} blocked)", ctx.account_id, listed.len());
}

/// The thread a conversation identifier names.
async fn thread_of_conversation(ctx: &Ctx<'_>, id: &presage::proto::ConversationIdentifier) -> Option<Thread> {
    use presage::proto::conversation_identifier::Identifier;
    match id.identifier.as_ref()? {
        Identifier::ThreadServiceId(text) => ServiceId::parse_from_service_id_string(text).map(Thread::Contact),
        Identifier::ThreadServiceIdBinary(bytes) => ServiceId::parse_from_service_id_binary(bytes).map(Thread::Contact),
        Identifier::ThreadGroupId(group_id) => thread_of_group_id(ctx, group_id).await,
        Identifier::ThreadE164(_) => None,
    }
}

/// The id a message an addressable reference names has here.
fn addressed(message: &presage::proto::AddressableMessage) -> Option<(u64, Uuid)> {
    use presage::proto::addressable_message::Author;
    let author = match message.author.as_ref()? {
        Author::AuthorServiceId(text) => ServiceId::parse_from_service_id_string(text).map(|id| ids::uuid_of(&id)),
        Author::AuthorServiceIdBinary(bytes) => ServiceId::parse_from_service_id_binary(bytes).map(|id| ids::uuid_of(&id)),
        Author::AuthorE164(_) => None,
    }?;
    Some((message.sent_timestamp?, author))
}

/// Messages and conversations deleted on the phone for the account only ("delete for me"): gone here as well.
async fn deleted_on_phone(ctx: &Ctx<'_>, deletes: &presage::proto::sync_message::DeleteForMe) {
    let mut store = ctx.store.clone();
    for delete in &deletes.message_deletes {
        let Some(thread) = delete.conversation.as_ref() else { continue };
        let Some(thread) = thread_of_conversation(ctx, thread).await else { continue };
        let remote = ids::remote_id(&thread);
        let buffer = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str()));
        for (sent, author) in delete.messages.iter().filter_map(addressed) {
            if let Some(buffer) = &buffer {
                ctx.state.runtime.delete_message(ctx.state, &buffer.id, &ids::message_id(sent, &author));
            }
            let _ = presage::store::ContentsStore::delete_message(&mut store, &thread, sent).await;
        }
    }
    let whole = deletes.conversation_deletes.iter().filter_map(|d| d.conversation.as_ref()).chain(deletes.local_only_conversation_deletes.iter().filter_map(|d| d.conversation.as_ref()));
    for conversation in whole {
        let Some(thread) = thread_of_conversation(ctx, conversation).await else { continue };
        let remote = ids::remote_id(&thread);
        let _ = presage::store::ContentsStore::clear_thread(&mut store, &thread).await;
        if let Some(buffer) = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str())) {
            let _ = ctx.state.store.forget_buffer(&buffer.id);
            ctx.state.runtime.remove_buffer(ctx.state, &buffer.id);
        }
    }
}

/// Whether a message is a line in the conversation (text, a file, a sticker...) rather than housekeeping.
fn shows_something(dm: &DataMessage) -> bool {
    dm.body.is_some() || !dm.attachments.is_empty() || dm.sticker.is_some() || !dm.contact.is_empty() || dm.poll_create.is_some()
}

/// What the account said on another device.
async fn sent_from_phone(ctx: &Ctx<'_>, content: &Content, sent: &Sent) {
    let Ok(thread) = Thread::try_from(content) else { return };
    // Writing in a conversation answers its request: the phone did, so here it is accepted without telling anybody.
    if super::requests::is_held(&ctx.store, &ids::remote_id(&thread)) {
        super::requests::apply(ctx, &thread, super::requests::Answer::Accept).await;
    }
    let at = sent.timestamp.unwrap_or_else(|| content.metadata.client_timestamp.timestamp_millis().max(0) as u64);
    if let Some(story) = &sent.story_message {
        // A story the account posted from the phone.
        super::stories::arrived(ctx, ctx.own, at, story).await;
        return;
    }
    if let Some(dm) = &sent.message {
        data_message(ctx, &thread, ctx.own, at, dm).await;
    } else if let Some(edit) = &sent.edit_message {
        edited(ctx, &thread, ctx.own, edit).await;
    }
}

fn seconds(ms: u64) -> i64 {
    (ms / 1000) as i64
}

/// Writes a line that is the conversation speaking, not a person.
fn system(ctx: &Ctx<'_>, buffer: &Buffer, text: &str, at_ms: u64) {
    ctx.state.runtime.record_message_at(
        ctx.state, ctx.account_id, &buffer.name, &buffer.kind, "Signal", text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None, Some(seconds(at_ms)), None, None,
    );
}

/// A message, a reaction, a delete or a change of timer, from `author`, in `thread`.
pub(super) async fn data_message(ctx: &Ctx<'_>, thread: &Thread, author: Uuid, at: u64, dm: &DataMessage) {
    let at = dm.timestamp.unwrap_or(at);
    let flags = dm.flags.unwrap_or(0);
    if flags & PROFILE_KEY_UPDATE != 0 || flags & END_SESSION != 0 {
        // Housekeeping between devices: the library has taken what it needs from it.
        return;
    }
    let buffer = people::buffer_for(ctx, thread).await;
    // A change to the group, said line by line; the library has already brought the group's state up to date.
    if let Some(changes) = dm.group_v2.as_ref().and_then(super::groups::change_of) {
        for line in super::groups::lines_for(ctx, &changes).await {
            system(ctx, &buffer, &line, at);
        }
        if changes.changes.iter().any(|c| matches!(c, presage::libsignal_service::groups_v2::GroupChange::Title(_))) {
            rename_known(ctx).await;
        }
        if let Thread::Group(key) = thread {
            super::groups::publish_members(ctx, key).await;
        }
    }
    if flags & EXPIRATION_TIMER_UPDATE != 0 {
        let timer = dm.expire_timer.unwrap_or(0);
        let who = people::person(ctx, &author).await;
        let words = if timer == 0 { format!("{who} turned off disappearing messages") } else { format!("{who} set disappearing messages to {}", people::timer_words(timer)) };
        system(ctx, &buffer, &words, at);
        let mut store = ctx.store.clone();
        let _ = presage::store::ContentsStore::update_expire_timer(&mut store, thread, timer, dm.expire_timer_version.unwrap_or(1)).await;
        return;
    }
    if let (Some(reaction), Some(_)) = (&dm.reaction, &dm.story_context) {
        // A reaction to a story: on the story, and said in the conversation, as Signal's clients say it.
        let stories = super::stories::buffer(ctx);
        reacted(ctx, &stories, author, reaction);
        if !reaction.remove.unwrap_or(false) {
            let who = people::person(ctx, &author).await;
            let whose = if reaction.target_author_aci.as_deref() == Some(ctx.own.to_string().as_str()) { "your" } else { "a" };
            system(ctx, &buffer, &format!("{who} reacted {} to {whose} story", reaction.emoji.as_deref().unwrap_or("")), at);
        }
        return;
    }
    if let Some(reaction) = &dm.reaction {
        reacted(ctx, &buffer, author, reaction);
        return;
    }
    if let Some(vote) = &dm.poll_vote {
        super::polls::voted(ctx, &buffer, &author, vote);
        return;
    }
    if let Some(end) = &dm.poll_terminate {
        super::polls::ended(ctx, &buffer, &author, end);
        return;
    }
    if let Some(create) = &dm.poll_create {
        super::polls::created(ctx, &buffer, &author, at, create);
    }
    if let Some(delete) = &dm.delete {
        if let Some(target) = delete.target_sent_timestamp {
            // Only the author can take a message back.
            deleted(ctx, thread, &buffer, &author, target).await;
        }
        return;
    }
    if dm.body.is_none() && dm.attachments.is_empty() && dm.sticker.is_none() && dm.contact.is_empty() && dm.poll_create.is_none() && dm.group_call_update.is_none() && dm.payment.is_none() && dm.gift_badge.is_none() {
        // A poll vote, a pin, a profile key: not a line in the conversation.
        tracing::debug!("signal[{}]: a message with nothing to show", ctx.account_id);
        return;
    }
    if dm.group_call_update.is_some() {
        let who = people::person(ctx, &author).await;
        system(ctx, &buffer, &format!("{who} started a call"), at);
        return;
    }

    let shown = describe(ctx, dm).await;
    let id = ids::message_id(at, &author);
    let from = people::person(ctx, &author).await;
    let reply = match &dm.quote {
        Some(quote) => Some(quoted(ctx, quote).await),
        None => None,
    };
    let own = ctx.is_own(&author);
    let highlight = !own && shown.mentioned_me;
    // The pictures, files and stickers: named in the cache now, fetched when looked at (see `media`).
    let view_once = dm.is_view_once.unwrap_or(false);
    let pointers = if view_once { Vec::new() } else { super::media::pointers_of(dm) };
    let mut attachments = super::media::attachments_of(ctx.account_id, thread, &author, at, &pointers[..super::media::shown_count(dm).min(pointers.len())]);
    // A message longer than Signal's 2000 bytes carries the whole of its text as a file; that is not a file to show.
    let long_text = pointers.iter().find(|p| p.content_type.as_deref() == Some(super::media::LONG_TEXT)).cloned();
    attachments.retain(|a| a.mimetype.as_deref() != Some(super::media::LONG_TEXT));
    let cards = if view_once { Vec::new() } else { super::media::previews_of(ctx.account_id, thread, &author, at, dm) };
    let recorded = ctx.state.runtime.record_message_at(
        ctx.state,
        ctx.account_id,
        &buffer.name,
        &buffer.kind,
        &from,
        &shown.body,
        false,
        "chat",
        reply,
        Some(id),
        highlight,
        super::media::avatar_of(ctx.account_id, &author),
        cards,
        attachments,
        Some(author.to_string()),
        Some(seconds(at)),
        shown.html,
        None,
    );
    if recorded {
        if let (Some(pointer), Some(handle)) = (long_text, ctx.state.runtime.signal_handle(ctx.account_id)) {
            // The whole text, fetched now rather than on look: it is the message, not an attachment to it.
            let (state, buffer_id, id) = (ctx.state.clone(), buffer.id.clone(), ids::message_id(at, &author));
            tokio::task::spawn_local(async move {
                let (done, answer) = tokio::sync::oneshot::channel();
                if handle.commands.send(super::Command::Fetch { pointer: Box::new(pointer), done }).is_err() {
                    return;
                }
                if let Ok(Ok(bytes)) = answer.await {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    state.runtime.update_message_body_only(&state, &buffer_id, &id, &text);
                }
            });
        }
    }
    if !recorded {
        tracing::debug!("signal[{}]: a message was not new", ctx.account_id);
    } else if let Some(seconds) = dm.expire_timer.filter(|s| *s > 0) {
        // It will disappear: from when it was sent if it is the account's own, from when it is looked at otherwise.
        let id = ids::message_id(at, &author);
        if own {
            super::expiry::from_sent(ctx, &buffer.id, thread, &id, at, seconds);
        } else {
            super::expiry::from_received(ctx, &buffer.id, thread, &id, at, seconds);
        }
    }
}

/// The words of a message and what to say about the rest of it.
struct Shown {
    body: String,
    html: Option<String>,
    mentioned_me: bool,
}

async fn describe(ctx: &Ctx<'_>, dm: &DataMessage) -> Shown {
    // Mentions are resolved before drawing: the drawing is not async.
    let mut names: HashMap<String, String> = HashMap::new();
    for range in &dm.body_ranges {
        use presage::proto::body_range::AssociatedValue as V;
        let aci = match &range.associated_value {
            Some(V::MentionAci(text)) => ids::aci_of(Some(text), None),
            Some(V::MentionAciBinary(bytes)) => ids::aci_of(None, Some(bytes)),
            _ => None,
        };
        if let Some(aci) = aci {
            names.insert(aci.to_string(), people::person(ctx, &aci).await);
        }
    }
    let text = dm.body.clone().unwrap_or_default();
    let drawn = ranges::render(&text, &dm.body_ranges, |aci| names.get(aci).cloned().unwrap_or_else(|| "someone".to_string()));
    let mentioned_me = drawn.mentions.iter().any(|m| m == &ctx.own.to_string());
    let mut lines = Vec::new();
    if dm.story_context.is_some() {
        lines.push("[Reply to a story]".to_string());
    }
    if dm.is_view_once.unwrap_or(false) {
        // Seen once and gone: moho does not keep or show it, so it says where it can be seen.
        lines.push("[View-once media: open it in Signal on your phone]".to_string());
    }
    for card in &dm.contact {
        let name = card.name.as_ref().map(|n| [n.given_name.as_deref(), n.family_name.as_deref()].iter().flatten().copied().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        lines.push(if name.is_empty() { "[Contact]".to_string() } else { format!("[Contact: {name}]") });
    }
    if let Some(poll) = &dm.poll_create {
        lines.push(format!("[Poll: {}]", poll.question.clone().unwrap_or_default()));
    }
    if dm.payment.is_some() {
        lines.push("[Payment: not supported in moho]".to_string());
    }
    if dm.gift_badge.is_some() {
        lines.push("[Gift badge: not supported in moho]".to_string());
    }
    let body = if lines.is_empty() {
        drawn.body
    } else if drawn.body.is_empty() {
        lines.join("\n")
    } else {
        format!("{}\n{}", drawn.body, lines.join("\n"))
    };
    // The html is only the styled text; the labels would be lost from it, so it is dropped when there are labels.
    let html = if lines.is_empty() { drawn.html } else { None };
    Shown { body, html, mentioned_me }
}

/// The quote a reply carries, as the window draws it.
async fn quoted(ctx: &Ctx<'_>, quote: &presage::proto::data_message::Quote) -> ReplyPreview {
    let author = ids::aci_of(quote.author_aci.as_deref(), quote.author_aci_binary.as_deref());
    let from = match &author {
        Some(author) => people::person(ctx, author).await,
        None => String::new(),
    };
    let text = quote.text.clone().unwrap_or_default();
    let body = if !text.trim().is_empty() {
        // The quoted text can carry mentions too; they are drawn as plain words.
        let names: HashMap<String, String> = HashMap::new();
        ranges::render(&text, &quote.body_ranges, |aci| names.get(aci).cloned().unwrap_or_else(|| "someone".to_string())).body
    } else if !quote.attachments.is_empty() {
        let kind = quote.attachments[0].content_type.as_deref().unwrap_or("");
        match kind.split('/').next().unwrap_or("") {
            "image" => "Photo",
            "video" => "Video",
            "audio" => "Audio",
            _ => "Attachment",
        }
        .to_string()
    } else {
        String::new()
    };
    ReplyPreview {
        id: match (quote.id, author) {
            (Some(ts), Some(author)) => ids::message_id(ts, &author),
            _ => String::new(),
        },
        from,
        body,
        thread: false,
        forwarded: false,
        thread_root: None,
        chat: None,
    }
}

/// Somebody reacted to a message, or took it back.
fn reacted(ctx: &Ctx<'_>, buffer: &Buffer, by: Uuid, reaction: &presage::proto::data_message::Reaction) {
    let (Some(emoji), Some(ts)) = (reaction.emoji.as_deref(), reaction.target_sent_timestamp) else { return };
    let Some(target) = ids::aci_of(reaction.target_author_aci.as_deref(), reaction.target_author_aci_binary.as_deref()) else { return };
    let id = ids::message_id(ts, &target);
    let mine = ctx.is_own(&by);
    let removing = reaction.remove.unwrap_or(false);
    // Signal lets a person have one reaction on a message: a new one replaces theirs. Who reacted with what is kept in
    // the account's file, so a change after a restart still takes the old one away.
    let key = format!("reactions:{id}");
    let mut by_person: std::collections::BTreeMap<String, String> = ctx.store.blob(&key).ok().flatten().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let before = by_person.get(&by.to_string()).cloned();
    if removing {
        if before.as_deref() == Some(emoji) {
            by_person.remove(&by.to_string());
        }
        ctx.state.runtime.update_reaction(ctx.state, &buffer.id, &id, emoji, mine, false);
    } else {
        if let Some(old) = before.filter(|old| old != emoji) {
            ctx.state.runtime.update_reaction(ctx.state, &buffer.id, &id, &old, mine, false);
        }
        by_person.insert(by.to_string(), emoji.to_string());
        ctx.state.runtime.update_reaction(ctx.state, &buffer.id, &id, emoji, mine, true);
    }
    if let Ok(bytes) = serde_json::to_vec(&by_person) {
        let _ = ctx.store.set_blob(&key, &bytes);
    }
}

/// A message taken back by its author.
async fn deleted(ctx: &Ctx<'_>, thread: &Thread, buffer: &Buffer, author: &Uuid, target_ms: u64) {
    let id = ids::message_id(target_ms, author);
    ctx.state.runtime.delete_message(ctx.state, &buffer.id, &id);
    let mut store = ctx.store.clone();
    let _ = presage::store::ContentsStore::delete_message(&mut store, thread, target_ms).await;
}

/// A message its author changed.
pub(super) async fn edited(ctx: &Ctx<'_>, thread: &Thread, author: Uuid, edit: &EditMessage) {
    let (Some(target), Some(dm)) = (edit.target_sent_timestamp, edit.data_message.as_ref()) else { return };
    let buffer = people::buffer_for(ctx, thread).await;
    let id = ids::message_id(target, &author);
    let shown = describe(ctx, dm).await;
    let held = ctx.state.store.get_message(&buffer.id, &id).ok().flatten();
    let (embeds, attachments) = held.map(|m| (m.embeds, m.attachments)).unwrap_or_default();
    if !ctx.state.runtime.update_message(ctx.state, &buffer.id, &id, &shown.body, &embeds, &attachments) {
        tracing::debug!("signal[{}]: an edit of a message that is not here", ctx.account_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;
    use presage::{
        libsignal_service::protocol::Aci,
        model::contacts::Contact,
        model::groups::Group,
        store::ContentsStore,
        proto::{body_range, data_message, AttachmentPointer, BodyRange},
    };

    const ME: &str = "00000000-0000-4000-8000-000000000001";
    const ALICE: &str = "11111111-1111-4111-8111-111111111111";
    const BOB: &str = "22222222-2222-4222-8222-222222222222";
    const ACCOUNT: &str = "signal:00000000-0000-4000-8000-000000000001";

    fn uuid(text: &str) -> Uuid {
        Uuid::parse_str(text).unwrap()
    }

    /// A made-up account with a contact called Alice, and a daemon to put what it hears into.
    async fn world(name: &str) -> (AppState, SignalStore) {
        let state = simulated_daemon(&format!("signal-{name}"));
        state.runtime.set_own_identity(ACCOUNT, "Me");
        let mut store = SignalStore::open_in_memory(presage::model::identity::OnNewIdentity::Trust).unwrap();
        for (id, name) in [(ALICE, "Alice"), (BOB, "Bob")] {
            store
                .save_contact(&Contact { uuid: uuid(id), phone_number: None, name: name.into(), verified: Default::default(), profile_key: Vec::new(), expire_timer: 0, expire_timer_version: 2, inbox_position: 0, avatar: None })
                .await
                .unwrap();
        }
        (state, store)
    }

    fn ctx<'a>(state: &'a AppState, store: &SignalStore) -> Ctx<'a> {
        Ctx::new(state, ACCOUNT, store.clone(), uuid(ME))
    }

    fn person(id: &str) -> Thread {
        Thread::Contact(ServiceId::Aci(Aci::from(uuid(id))))
    }

    fn text(body: &str, at: u64) -> DataMessage {
        DataMessage { body: Some(body.into()), timestamp: Some(at), ..Default::default() }
    }

    fn only_buffer(state: &AppState) -> Buffer {
        let mut all = state.runtime.list_buffers();
        assert_eq!(all.len(), 1, "{all:?}");
        all.remove(0)
    }

    fn backlog(state: &AppState, buffer: &Buffer) -> Vec<crate::model::Message> {
        let mut rows = state.store.get_backlog(&buffer.id, i64::MAX, 100).unwrap();
        rows.sort_by_key(|m| m.ts);
        rows
    }

    #[tokio::test]
    async fn a_message_from_a_contact_opens_a_conversation_named_for_them() {
        let (state, store) = world("dm").await;
        let ctx = ctx(&state, &store);
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_000_000, &text("hello", 1_700_000_000_000)).await;
        let buffer = only_buffer(&state);
        assert_eq!((buffer.name.as_str(), buffer.kind.as_str()), ("Alice", "dm"));
        assert_eq!(buffer.remote_id.as_deref(), Some(format!("p:{ALICE}").as_str()));
        assert_eq!(buffer.category.as_deref(), Some("Direct Messages"));
        let rows = backlog(&state, &buffer);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].from.as_str(), rows[0].body.as_str(), rows[0].is_own), ("Alice", "hello", false));
        assert_eq!(rows[0].id, format!("1700000000000-{ALICE}"));
        assert_eq!(rows[0].ts, 1_700_000_000);
    }

    #[tokio::test]
    async fn a_group_message_goes_to_the_group_by_its_title() {
        let (state, store) = world("group").await;
        let writer = store.clone();
        let key = [9u8; 32];
        writer
            .save_group(key, Group { title: "Climbing".into(), avatar: String::new(), disappearing_messages_timer: None, access_control: None, revision: 1, members: Vec::new(), pending_members: Vec::new(), requesting_members: Vec::new(), invite_link_password: Vec::new(), description: None })
            .await
            .unwrap();
        let ctx = ctx(&state, &store);
        data_message(&ctx, &Thread::Group(key), uuid(BOB), 1_700_000_001_000, &text("who is in", 1_700_000_001_000)).await;
        let buffer = only_buffer(&state);
        assert_eq!((buffer.name.as_str(), buffer.kind.as_str(), buffer.category.as_deref()), ("Climbing", "channel", Some("Groups")));
        assert_eq!(backlog(&state, &buffer)[0].from, "Bob");
    }

    #[tokio::test]
    async fn what_the_account_said_on_its_phone_is_its_own() {
        let (state, store) = world("own").await;
        let ctx = ctx(&state, &store);
        data_message(&ctx, &person(BOB), uuid(ME), 1_700_000_002_000, &text("on my way", 1_700_000_002_000)).await;
        let buffer = only_buffer(&state);
        assert_eq!(buffer.name, "Bob");
        let rows = backlog(&state, &buffer);
        assert_eq!((rows[0].from.as_str(), rows[0].is_own), ("Me", true));
    }

    #[tokio::test]
    async fn the_account_talking_to_itself_is_note_to_self() {
        let (state, store) = world("note").await;
        let ctx = ctx(&state, &store);
        data_message(&ctx, &person(ME), uuid(ME), 1_700_000_003_000, &text("remember milk", 1_700_000_003_000)).await;
        assert_eq!(only_buffer(&state).name, "Note to Self");
    }

    #[tokio::test]
    async fn styles_and_mentions_are_drawn_and_a_mention_of_the_account_is_a_highlight() {
        let (state, store) = world("styles").await;
        let ctx = ctx(&state, &store);
        let mut dm = text("hey \u{FFFC} look", 1_700_000_004_000);
        dm.body_ranges = vec![
            BodyRange { start: Some(4), length: Some(1), associated_value: Some(body_range::AssociatedValue::MentionAci(ME.into())) },
            BodyRange { start: Some(6), length: Some(4), associated_value: Some(body_range::AssociatedValue::Style(body_range::Style::Bold as i32)) },
        ];
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_004_000, &dm).await;
        let rows = backlog(&state, &only_buffer(&state));
        assert_eq!(rows[0].body, "hey @Me look");
        assert!(rows[0].is_highlight);
        assert!(rows[0].html.as_deref().is_some_and(|h| h.contains("<b>look</b>") && h.contains("class=\"mention\"")), "{:?}", rows[0].html);
    }

    #[tokio::test]
    async fn a_reply_carries_what_it_answers() {
        let (state, store) = world("quote").await;
        let ctx = ctx(&state, &store);
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_005_000, &text("lunch?", 1_700_000_005_000)).await;
        let mut reply = text("yes", 1_700_000_006_000);
        reply.quote = Some(data_message::Quote { id: Some(1_700_000_005_000), author_aci: Some(ALICE.into()), text: Some("lunch?".into()), ..Default::default() });
        data_message(&ctx, &person(ALICE), uuid(ME), 1_700_000_006_000, &reply).await;
        let rows = backlog(&state, &only_buffer(&state));
        let quote = rows[1].reply_to.as_ref().expect("a reply");
        assert_eq!((quote.id.as_str(), quote.from.as_str(), quote.body.as_str()), (rows[0].id.as_str(), "Alice", "lunch?"));
    }

    #[tokio::test]
    async fn reactions_are_counted_and_taken_back() {
        let (state, store) = world("react").await;
        let ctx = ctx(&state, &store);
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_007_000, &text("big news", 1_700_000_007_000)).await;
        let react = |remove: bool| DataMessage {
            reaction: Some(data_message::Reaction { emoji: Some("👍".into()), remove: Some(remove), target_author_aci: Some(ALICE.into()), target_sent_timestamp: Some(1_700_000_007_000), ..Default::default() }),
            ..Default::default()
        };
        data_message(&ctx, &person(ALICE), uuid(ME), 1_700_000_007_500, &react(false)).await;
        let buffer = only_buffer(&state);
        let reactions = backlog(&state, &buffer)[0].reactions.clone();
        assert_eq!((reactions.len(), reactions[0].emoji.as_str(), reactions[0].count, reactions[0].me), (1, "👍", 1, true));
        data_message(&ctx, &person(ALICE), uuid(ME), 1_700_000_008_000, &react(true)).await;
        assert!(backlog(&state, &buffer)[0].reactions.is_empty());
    }

    #[tokio::test]
    async fn a_new_reaction_from_the_same_person_replaces_theirs() {
        let (state, store) = world("react-again").await;
        let ctx = ctx(&state, &store);
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_017_000, &text("news", 1_700_000_017_000)).await;
        let react = |emoji: &str| DataMessage {
            reaction: Some(data_message::Reaction { emoji: Some(emoji.into()), remove: Some(false), target_author_aci: Some(ALICE.into()), target_sent_timestamp: Some(1_700_000_017_000), ..Default::default() }),
            ..Default::default()
        };
        data_message(&ctx, &person(ALICE), uuid(BOB), 1_700_000_017_100, &react("😂")).await;
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_017_200, &react("😂")).await;
        // Bob changes his mind: his laugh goes, Alice's stays.
        data_message(&ctx, &person(ALICE), uuid(BOB), 1_700_000_017_300, &react("👍")).await;
        let mut got: Vec<(String, i64)> = backlog(&state, &only_buffer(&state))[0].reactions.iter().map(|r| (r.emoji.clone(), r.count)).collect();
        got.sort();
        assert_eq!(got, vec![("👍".to_string(), 1), ("😂".to_string(), 1)]);
    }

    #[tokio::test]
    async fn a_message_is_taken_back_by_its_author_and_changed_by_an_edit() {
        let (state, store) = world("edit").await;
        let ctx = ctx(&state, &store);
        let thread = person(ALICE);
        data_message(&ctx, &thread, uuid(ALICE), 1_700_000_009_000, &text("teh plan", 1_700_000_009_000)).await;
        data_message(&ctx, &thread, uuid(ALICE), 1_700_000_010_000, &text("oops", 1_700_000_010_000)).await;
        let buffer = only_buffer(&state);
        edited(&ctx, &thread, uuid(ALICE), &EditMessage { target_sent_timestamp: Some(1_700_000_009_000), data_message: Some(text("the plan", 1_700_000_011_000)) }).await;
        let rows = backlog(&state, &buffer);
        assert_eq!((rows[0].body.as_str(), rows[0].edited), ("the plan", true));
        let take_back = DataMessage { delete: Some(data_message::Delete { target_sent_timestamp: Some(1_700_000_010_000) }), ..Default::default() };
        data_message(&ctx, &thread, uuid(ALICE), 1_700_000_012_000, &take_back).await;
        let rows = backlog(&state, &buffer);
        assert_eq!(rows.len(), 1);
        // Somebody else cannot take back Alice's message.
        let forged = DataMessage { delete: Some(data_message::Delete { target_sent_timestamp: Some(1_700_000_009_000) }), ..Default::default() };
        data_message(&ctx, &thread, uuid(BOB), 1_700_000_013_000, &forged).await;
        assert_eq!(backlog(&state, &buffer).len(), 1);
    }

    #[tokio::test]
    async fn a_new_timer_is_a_line_in_the_conversation() {
        let (state, store) = world("timer").await;
        let ctx = ctx(&state, &store);
        let change = DataMessage { flags: Some(EXPIRATION_TIMER_UPDATE), expire_timer: Some(86_400), timestamp: Some(1_700_000_014_000), ..Default::default() };
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_014_000, &change).await;
        let rows = backlog(&state, &only_buffer(&state));
        assert_eq!((rows[0].kind.as_str(), rows[0].body.as_str()), ("system", "Alice set disappearing messages to 1 day"));
    }

    #[tokio::test]
    async fn media_and_things_moho_cannot_draw_are_named_not_dropped() {
        let (state, store) = world("media").await;
        let ctx = ctx(&state, &store);
        let mut dm = text("look", 1_700_000_015_000);
        dm.attachments = vec![AttachmentPointer { content_type: Some("image/jpeg".into()), ..Default::default() }];
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_015_000, &dm).await;
        let mut gift = DataMessage { timestamp: Some(1_700_000_016_000), ..Default::default() };
        gift.gift_badge = Some(Default::default());
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_016_000, &gift).await;
        let rows = backlog(&state, &only_buffer(&state));
        assert_eq!(rows[0].body, "look");
        assert_eq!((rows[0].attachments.len(), rows[0].attachments[0].kind.as_str()), (1, "image"));
        assert!(rows[0].attachments[0].path.as_deref().is_some_and(|p| p.contains("/signal-media/")));
        assert!(rows[1].body.contains("not supported in moho"), "{}", rows[1].body);
    }

    fn handle(own: &str) -> (Handle, tokio::sync::mpsc::UnboundedReceiver<super::super::Command>) {
        let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
        let store = SignalStore::open_in_memory(presage::model::identity::OnNewIdentity::Trust).unwrap();
        (Handle { account_id: ACCOUNT.to_string(), own: uuid(own), store, memory: Default::default(), commands }, rx)
    }

    fn content_from(sender: &str, body: ContentBody, needs_receipt: bool) -> Content {
        content_at(sender, body, needs_receipt, 1_700_000_100_000)
    }

    fn content_at(sender: &str, body: ContentBody, needs_receipt: bool, at_ms: i64) -> Content {
        use presage::libsignal_service::{content::Metadata, protocol::Aci};
        let at = chrono::DateTime::from_timestamp_millis(at_ms).unwrap();
        Content {
            metadata: Metadata {
                sender: ServiceId::Aci(Aci::from(uuid(sender))),
                destination: ServiceId::Aci(Aci::from(uuid(ME))),
                sender_device: presage::libsignal_service::protocol::DeviceId::new(1).unwrap(),
                pni_verified: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
            body,
        }
    }

    #[tokio::test]
    async fn a_read_receipt_moves_the_readers_marker_forward_only() {
        let (state, store) = world("receipt").await;
        let ctx = ctx(&state, &store);
        let (handle, _rx) = handle(ME);
        state.runtime.set_signal_handle(ACCOUNT, handle.clone());
        // Two messages the account sent to Alice, kept in the library's store as it keeps what it sends.
        for at in [1_700_000_020_000u64, 1_700_000_030_000] {
            data_message(&ctx, &person(ALICE), uuid(ME), at, &text("x", at)).await;
            let saved = content_at(ME, ContentBody::DataMessage(text("x", at)), false, at as i64);
            store.save_message(&person(ALICE), saved).await.unwrap();
        }
        let mut events = state.events.subscribe();
        let read = |ts: u64| content_from(ALICE, ContentBody::ReceiptMessage(ReceiptMessage { r#type: Some(receipt_message::Type::Read as i32), timestamp: vec![ts] }), false);
        let receipt = |ts: u64| ReceiptMessage { r#type: Some(receipt_message::Type::Read as i32), timestamp: vec![ts] };
        receipt_seen(&ctx, &handle, &read(1_700_000_030_000), &receipt(1_700_000_030_000)).await;
        let event = events.try_recv().expect("a marker");
        assert_eq!(event.name, "readReceipts");
        assert_eq!(event.data["readers"][0]["nick"], "Alice");
        assert_eq!(event.data["readers"][0]["messageId"], format!("1700000030000-{ME}"));
        // An older receipt arriving late does not move it back, and says nothing.
        receipt_seen(&ctx, &handle, &read(1_700_000_020_000), &receipt(1_700_000_020_000)).await;
        assert!(events.try_recv().is_err());
        // A delivery receipt is not a reading.
        let delivered = ReceiptMessage { r#type: Some(receipt_message::Type::Delivery as i32), timestamp: vec![1_700_000_030_000] };
        receipt_seen(&ctx, &handle, &read(1_700_000_030_000), &delivered).await;
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn typing_is_shown_for_an_open_conversation_and_ends() {
        let (state, store) = world("typing").await;
        let ctx = ctx(&state, &store);
        let (handle, _rx) = handle(ME);
        let started = TypingMessage { timestamp: Some(1), action: Some(typing_message::Action::Started as i32), group_id: None };
        // Nobody is typing in a conversation that is not open.
        let mut events = state.events.subscribe();
        typing_seen(&ctx, &handle, &content_from(ALICE, ContentBody::TypingMessage(started.clone()), false), &started).await;
        assert!(events.try_recv().is_err());
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_040_000, &text("hi", 1_700_000_040_000)).await;
        while events.try_recv().is_ok() {}
        typing_seen(&ctx, &handle, &content_from(ALICE, ContentBody::TypingMessage(started.clone()), false), &started).await;
        let event = events.try_recv().expect("typing");
        assert_eq!((event.name, event.data["nicks"][0].as_str()), ("typing", Some("Alice")));
        let stopped = TypingMessage { timestamp: Some(2), action: Some(typing_message::Action::Stopped as i32), group_id: None };
        typing_seen(&ctx, &handle, &content_from(ALICE, ContentBody::TypingMessage(stopped.clone()), false), &stopped).await;
        assert_eq!(events.try_recv().expect("cleared").data["nicks"].as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn a_message_that_asks_for_a_receipt_gets_a_delivery_receipt_and_nothing_more() {
        let (handle, mut rx) = handle(ME);
        let asking = content_from(ALICE, ContentBody::DataMessage(text("hi", 5)), true);
        delivered(&handle, &asking, 5);
        match rx.try_recv().expect("a receipt") {
            super::super::Command::Receipt { kind, timestamps, .. } => assert_eq!((kind, timestamps), (receipt_message::Type::Delivery, vec![5])),
            other => panic!("{other:?}"),
        }
        delivered(&handle, &content_from(ALICE, ContentBody::DataMessage(text("hi", 6)), false), 6);
        // The account's own messages from another device are not receipted to itself.
        delivered(&handle, &content_from(ME, ContentBody::DataMessage(text("hi", 7)), true), 7);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reading_a_conversation_tells_its_senders_only_if_the_phone_says_read_receipts_are_on() {
        use super::super::{send, Command};
        let (state, store) = world("reading").await;
        let ctx = ctx(&state, &store);
        let (handle, mut rx) = handle(ME);
        state.runtime.set_signal_handle(ACCOUNT, handle.clone());
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_050_000, &text("one", 1_700_000_050_000)).await;
        data_message(&ctx, &person(ALICE), uuid(ME), 1_700_000_051_000, &text("two", 1_700_000_051_000)).await;
        let buffer = only_buffer(&state);
        // Unknown settings: the phone is told, Alice is not.
        send::mark_read(&state, &buffer).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Command::ReadSync { reads }) if reads == vec![(uuid(ALICE), 1_700_000_050_000)]));
        assert!(rx.try_recv().is_err());
        // Reading again sends nothing.
        send::mark_read(&state, &buffer).unwrap();
        assert!(rx.try_recv().is_err());
        // With receipts on, a new message from Alice is receipted as read.
        handle.memory.set_synced(crate::backend::signal::memory::Synced { read_receipts: Some(true), ..Default::default() });
        data_message(&ctx, &person(ALICE), uuid(ALICE), 1_700_000_052_000, &text("three", 1_700_000_052_000)).await;
        send::mark_read(&state, &buffer).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Command::Receipt { kind: receipt_message::Type::Read, timestamps, .. }) if timestamps == vec![1_700_000_052_000]));
        assert!(matches!(rx.try_recv(), Ok(Command::ReadSync { .. })));
    }

    #[tokio::test]
    async fn a_disappearing_message_goes_when_its_time_comes_and_not_before_it_was_seen() {
        use super::super::{expiry, send};
        let (state, store) = world("vanish").await;
        let ctx = ctx(&state, &store);
        let (handle, _rx) = handle(ME);
        let handle = Handle { store: store.clone(), ..handle };
        state.runtime.set_signal_handle(ACCOUNT, handle.clone());
        let now_ms = (expiry::now() * 1000) as u64;
        // A message received with a one-minute timer, and one the account sent with a timer that has already run out.
        let mut incoming = text("secret", now_ms);
        incoming.expire_timer = Some(60);
        data_message(&ctx, &person(ALICE), uuid(ALICE), now_ms, &incoming).await;
        let mut mine = text("mine", now_ms - 120_000);
        mine.expire_timer = Some(60);
        data_message(&ctx, &person(ALICE), uuid(ME), now_ms - 120_000, &mine).await;
        let buffer = only_buffer(&state);
        assert_eq!(backlog(&state, &buffer).len(), 2);
        assert_eq!(store.expiry_count().unwrap(), 2);
        // Only the account's own is due: the other has not been looked at.
        assert_eq!(expiry::sweep(&state, &store).await, 1);
        let rows = backlog(&state, &buffer);
        assert_eq!(rows.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), vec!["secret"]);
        // Looking at the conversation starts its clock; a minute later it is gone.
        send::mark_read(&state, &buffer).unwrap();
        assert_eq!(store.expiry_due(expiry::now()).unwrap().len(), 0);
        assert_eq!(store.expiry_due(expiry::now() + 61).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_changed_safety_number_is_told_in_the_conversation_with_that_person() {
        use presage::libsignal_service::protocol::{DeviceId, IdentityKeyPair, IdentityKeyStore, ProtocolAddress};
        use presage::store::Store;
        let (state, store) = world("safety").await;
        let ctx = ctx(&state, &store);
        let mut rng = rand09::rng();
        let address = ProtocolAddress::new(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(uuid(ALICE))).service_id_string(), DeviceId::new(1).unwrap());
        let mut protocol = store.aci_protocol_store();
        protocol.save_identity(&address, IdentityKeyPair::generate(&mut rng).identity_key()).await.unwrap();
        // Meeting somebody is not a change.
        announce_changed_identities(&ctx, 1_700_000_200_000).await;
        assert!(state.runtime.list_buffers().is_empty());
        protocol.save_identity(&address, IdentityKeyPair::generate(&mut rng).identity_key()).await.unwrap();
        announce_changed_identities(&ctx, 1_700_000_201_000).await;
        let rows = backlog(&state, &only_buffer(&state));
        assert_eq!(rows[0].kind, "system");
        assert!(rows[0].body.contains("safety number with Alice has changed"), "{}", rows[0].body);
        // And once told, not told again.
        announce_changed_identities(&ctx, 1_700_000_202_000).await;
        assert_eq!(backlog(&state, &only_buffer(&state)).len(), 1);
    }

    #[test]
    fn the_phones_call_log_becomes_words() {
        use presage::proto::sync_message::call_event::{Direction, Event, Type};
        let say = |t: Type, d: Direction, e: Event| call_words(t as i32, d as i32, e as i32);
        assert_eq!(say(Type::AudioCall, Direction::Incoming, Event::NotAccepted).as_deref(), Some("Missed voice call"));
        assert_eq!(say(Type::VideoCall, Direction::Incoming, Event::Accepted).as_deref(), Some("Incoming video call"));
        assert_eq!(say(Type::AudioCall, Direction::Outgoing, Event::Accepted).as_deref(), Some("Outgoing voice call"));
        assert_eq!(say(Type::VideoCall, Direction::Outgoing, Event::NotAccepted).as_deref(), Some("Outgoing video call, not answered"));
        assert_eq!(say(Type::GroupCall, Direction::Incoming, Event::NotAccepted).as_deref(), Some("Missed group call"));
        assert_eq!(say(Type::AudioCall, Direction::Incoming, Event::Delete), None);
        assert_eq!(say(Type::AudioCall, Direction::Incoming, Event::Observed), None);
    }
}
