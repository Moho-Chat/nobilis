//! What the library hands back, as the daemon's conversations and messages.
//!
//! Everything arrives as a `Content` the library has already decrypted and kept in the account's store. This reads what
//! is in it: a message from somebody, a message the account sent from its phone (which Signal tells every linked device
//! about so the conversation looks the same everywhere), an edit, a reaction, a delete, a change of the disappearing
//! timer. What it does not draw yet it says so about, in the log, never silently as though it were nothing.

use super::ids;
use super::people::{self, Ctx};
use super::ranges;
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
    proto::{sync_message::Sent, DataMessage, EditMessage, SyncMessage},
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

/// Opens (or finds) the conversation with a person, named by their ACI.
pub async fn open_dm(state: &AppState, handle: &Handle, user_id: &str) -> anyhow::Result<String> {
    let who = Uuid::parse_str(user_id.trim()).map_err(|_| anyhow::anyhow!("Signal needs the person's id, not their name"))?;
    let ctx = Ctx::new(state, &handle.account_id, handle.store.clone(), handle.own);
    let thread = Thread::Contact(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(who)));
    Ok(people::buffer_for(&ctx, &thread).await.id)
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
    match &content.body {
        ContentBody::DataMessage(dm) => {
            let Ok(thread) = Thread::try_from(&content) else { return };
            data_message(&ctx, &thread, sender, sent_at, dm).await;
        }
        ContentBody::EditMessage(edit) => {
            let Ok(thread) = Thread::try_from(&content) else { return };
            edited(&ctx, &thread, sender, edit).await;
        }
        ContentBody::SynchronizeMessage(sync) => synchronized(&ctx, &content, sync).await,
        ContentBody::TypingMessage(_) | ContentBody::ReceiptMessage(_) | ContentBody::NullMessage(_) => {}
        ContentBody::CallMessage(_) => tracing::debug!("signal[{}]: a call message (calls are not handled yet)", ctx.account_id),
        ContentBody::StoryMessage(_) => tracing::debug!("signal[{}]: a story (stories are not handled yet)", ctx.account_id),
        ContentBody::DecryptionErrorMessage(_) => {}
    }
}

async fn synchronized(ctx: &Ctx<'_>, content: &Content, sync: &SyncMessage) {
    use presage::proto::sync_message::Content as Sync;
    // Contacts, blocked lists, configuration and the rest are taken up by the library or by later rounds.
    if let Some(Sync::Sent(sent)) = &sync.content {
        sent_from_phone(ctx, content, sent).await;
    }
}

/// What the account said on another device.
async fn sent_from_phone(ctx: &Ctx<'_>, content: &Content, sent: &Sent) {
    let Ok(thread) = Thread::try_from(content) else { return };
    let at = sent.timestamp.unwrap_or_else(|| content.metadata.client_timestamp.timestamp_millis().max(0) as u64);
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
async fn data_message(ctx: &Ctx<'_>, thread: &Thread, author: Uuid, at: u64, dm: &DataMessage) {
    let at = dm.timestamp.unwrap_or(at);
    let flags = dm.flags.unwrap_or(0);
    if flags & PROFILE_KEY_UPDATE != 0 || flags & END_SESSION != 0 {
        // Housekeeping between devices: the library has taken what it needs from it.
        return;
    }
    let buffer = people::buffer_for(ctx, thread).await;
    if flags & EXPIRATION_TIMER_UPDATE != 0 {
        let timer = dm.expire_timer.unwrap_or(0);
        let who = people::person(ctx, &author).await;
        let words = if timer == 0 { format!("{who} turned off disappearing messages") } else { format!("{who} set disappearing messages to {}", people::timer_words(timer)) };
        system(ctx, &buffer, &words, at);
        let mut store = ctx.store.clone();
        let _ = presage::store::ContentsStore::update_expire_timer(&mut store, thread, timer, dm.expire_timer_version.unwrap_or(1)).await;
        return;
    }
    if let Some(reaction) = &dm.reaction {
        reacted(ctx, &buffer, author, reaction);
        return;
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
        None,
        Vec::new(),
        Vec::new(),
        Some(author.to_string()),
        Some(seconds(at)),
        shown.html,
        None,
    );
    if !recorded {
        tracing::debug!("signal[{}]: a message was not new", ctx.account_id);
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
        lines.push("[View-once media: open it in Signal on your phone]".to_string());
    } else {
        lines.extend(dm.attachments.iter().map(attachment_label));
    }
    if let Some(sticker) = &dm.sticker {
        let emoji = sticker.emoji.clone().unwrap_or_default();
        lines.push(if emoji.is_empty() { "[Sticker]".to_string() } else { format!("[Sticker {emoji}]") });
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

/// What an attachment is called in the line that stands for it until it can be drawn.
pub fn attachment_label(pointer: &presage::proto::AttachmentPointer) -> String {
    let kind = pointer.content_type.as_deref().unwrap_or("");
    let flags = pointer.flags.unwrap_or(0);
    let name = pointer.file_name.as_deref().filter(|n| !n.is_empty());
    match kind.split('/').next().unwrap_or("") {
        "image" if flags & GIF != 0 || kind == "image/gif" => "[GIF]".to_string(),
        "image" => "[Photo]".to_string(),
        "video" if flags & GIF != 0 => "[GIF]".to_string(),
        "video" => "[Video]".to_string(),
        "audio" if flags & VOICE_MESSAGE != 0 => "[Voice message]".to_string(),
        "audio" => "[Audio]".to_string(),
        _ => match name {
            Some(name) => format!("[File: {name}]"),
            None => "[File]".to_string(),
        },
    }
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
    ctx.state.runtime.update_reaction(ctx.state, &buffer.id, &id, emoji, ctx.is_own(&by), !reaction.remove.unwrap_or(false));
}

/// A message taken back by its author.
async fn deleted(ctx: &Ctx<'_>, thread: &Thread, buffer: &Buffer, author: &Uuid, target_ms: u64) {
    let id = ids::message_id(target_ms, author);
    ctx.state.runtime.delete_message(ctx.state, &buffer.id, &id);
    let mut store = ctx.store.clone();
    let _ = presage::store::ContentsStore::delete_message(&mut store, thread, target_ms).await;
}

/// A message its author changed.
async fn edited(ctx: &Ctx<'_>, thread: &Thread, author: Uuid, edit: &EditMessage) {
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
        assert_eq!(rows[0].body, "look\n[Photo]");
        assert!(rows[1].body.contains("not supported in moho"), "{}", rows[1].body);
    }

    fn pointer(kind: &str, flags: u32, name: Option<&str>) -> AttachmentPointer {
        AttachmentPointer { content_type: Some(kind.to_string()), flags: Some(flags), file_name: name.map(String::from), ..Default::default() }
    }

    #[test]
    fn an_attachment_is_named_for_what_it_is() {
        assert_eq!(attachment_label(&pointer("image/jpeg", 0, None)), "[Photo]");
        assert_eq!(attachment_label(&pointer("image/gif", 0, None)), "[GIF]");
        assert_eq!(attachment_label(&pointer("video/mp4", GIF, None)), "[GIF]");
        assert_eq!(attachment_label(&pointer("video/mp4", 0, None)), "[Video]");
        assert_eq!(attachment_label(&pointer("audio/aac", VOICE_MESSAGE, None)), "[Voice message]");
        assert_eq!(attachment_label(&pointer("audio/mpeg", 0, None)), "[Audio]");
        assert_eq!(attachment_label(&pointer("application/pdf", 0, Some("a.pdf"))), "[File: a.pdf]");
        assert_eq!(attachment_label(&pointer("application/zip", 0, None)), "[File]");
    }
}
