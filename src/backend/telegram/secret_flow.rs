//! Secret chats with Telegram: starting one, being asked for one, and messages both ways (see `secret` for the
//! end-to-end layer itself).
//!
//! # As the person sees it
//!
//! A secret chat is a conversation of its own beside the ordinary one with the same person, named with a lock
//! ("🔒 Ada"), under Direct Messages. Somebody asking for one is offered on the Invitations tile. Messages, replies,
//! formatting, files and pictures go both ways; a message cannot be edited (Telegram's apps cannot either) and
//! deleting one deletes it on both sides. The self-destruct timer is the conversation's, set by either person.
//! Closing the conversation ends the chat on both sides.
//!
//! # Order
//!
//! Everything for one account's secret chats goes through one worker in arrival order, since each message's sequence
//! numbers say what came before it. The saved file is written after every change.

use super::messages::Own;
use super::secret::{self, Action, Arrival, Chat, Decrypted, Media, Saved, Stage};
use super::send::explain;
use super::Handle;
use crate::model::{Attachment, Buffer, ReplyPreview};
use crate::state::AppState;
use grammers_client::tl;
use grammers_session::types::PeerId;
use grammers_session::Session;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

/// What a secret chat's buffer is called in `remote_id`: never a number, so nothing mistakes it for a peer.
pub const PREFIX: &str = "secret:";

pub fn remote_of(chat: i32) -> String {
    format!("{PREFIX}{chat}")
}

pub fn chat_of(buffer: &Buffer) -> Option<i32> {
    buffer.remote_id.as_deref()?.strip_prefix(PREFIX)?.parse().ok()
}

pub fn is_secret(buffer: &Buffer) -> bool {
    chat_of(buffer).is_some()
}

/// One account's chats, behind a lock, and where they are kept.
struct Account {
    path: std::path::PathBuf,
    saved: Saved,
}

type Shared = Arc<Mutex<Account>>;

fn accounts() -> &'static std::sync::Mutex<HashMap<String, Shared>> {
    static A: OnceLock<std::sync::Mutex<HashMap<String, Shared>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

fn account_of(handle: &Handle, account_id: &str) -> Shared {
    let mut all = accounts().lock().unwrap_or_else(|e| e.into_inner());
    all.entry(account_id.to_string())
        .or_insert_with(|| {
            let path = secret::path_for(handle.own.user_id);
            Arc::new(Mutex::new(Account { saved: secret::load(&path), path }))
        })
        .clone()
}

fn keep(account: &Account) {
    if let Err(e) = secret::save(&account.path, &account.saved) {
        tracing::warn!("telegram: keeping the secret chats: {e:#}");
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn handle_of(state: &AppState, account_id: &str) -> Result<Handle, String> {
    state.runtime.telegram_handle(account_id).ok_or_else(|| "that Telegram account is not connected".to_string())
}

fn input_chat(chat: &Chat) -> tl::enums::InputEncryptedChat {
    tl::types::InputEncryptedChat { chat_id: chat.id, access_hash: chat.access_hash }.into()
}

// ---------------------------------------------------------------------------------------------------------------
// The person and the conversation.

/// The other person, as Telegram describes them: their name and picture.
async fn person(handle: &Handle, user_id: i64) -> Option<grammers_client::peer::Peer> {
    let peer_ref = handle.session.peer_ref(PeerId::user_unchecked(user_id)).await.ok().flatten()?;
    handle.client.resolve_peer(peer_ref).await.ok()
}

async fn name_of(handle: &Handle, user_id: i64) -> String {
    match person(handle, user_id).await {
        Some(peer) => super::messages::name_of(&peer),
        None => format!("Telegram user {user_id}"),
    }
}

/// The conversation a secret chat is shown as, made if it is not there yet.
async fn buffer_for(state: &AppState, account_id: &str, handle: &Handle, chat: &mut Chat) -> Buffer {
    let remote = remote_of(chat.id);
    if let Some(found) = state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote.as_str())) {
        return found;
    }
    let who = person(handle, chat.user_id).await;
    let title = who.as_ref().map(super::messages::name_of).unwrap_or_else(|| format!("Telegram user {}", chat.user_id));
    let taken: Vec<String> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == account_id).map(|b| b.name).collect();
    let mut name = format!("🔒 {title}");
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("🔒 {title} ({n})");
        n += 1;
    }
    let buffer = state.runtime.ensure_buffer(state, account_id, &name, "dm");
    state.runtime.set_buffer_remote_id(state, &buffer.id, &remote);
    state.runtime.set_buffer_category(state, &buffer.id, Some(super::peers::DIRECT_MESSAGES), 0);
    state.runtime.set_buffer_name_icon(state, &buffer.id, Some("lock"));
    if let Some(avatar) = who.as_ref().and_then(|p| super::media::avatar_url(handle.own.user_id, p)) {
        state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
    }
    chat.buffer = buffer.id.clone();
    state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}

/// A line from moho itself in the conversation.
fn note(state: &AppState, account_id: &str, buffer: &Buffer, text: &str) {
    state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, "Telegram", text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None, Some(now()), None, None);
}

/// The asks for a secret chat waiting on the Invitations tile, in the shape every service's invitations share.
fn announce_requests(state: &AppState, account_id: &str, saved: &Saved, names: &HashMap<i64, String>) {
    let invites: Vec<serde_json::Value> = saved
        .chats
        .values()
        .filter(|c| matches!(c.stage, Stage::Requested { .. }))
        .map(|c| {
            let name = names.get(&c.user_id).cloned().unwrap_or_else(|| format!("Telegram user {}", c.user_id));
            json!({ "roomId": remote_of(c.id), "name": format!("🔒 {name}"), "inviter": name, "avatarUrl": null, "isDirect": true, "secret": true })
        })
        .collect();
    let ids: std::collections::HashSet<String> = state.runtime.matrix_invites(account_id).iter().filter_map(|i| i["roomId"].as_str().map(str::to_string)).filter(|r| r.starts_with(PREFIX)).collect();
    // Asks that were answered (here or on the other side) are taken down with the rest.
    if state.runtime.merge_matrix_invites(account_id, invites, &ids.into_iter().filter(|r| !saved.chats.values().any(|c| remote_of(c.id) == *r && matches!(c.stage, Stage::Requested { .. }))).collect()) {
        state.events.emit("matrixInvites", json!({ "accountId": account_id, "invites": state.runtime.matrix_invites(account_id) }));
    }
}

/// The asks waiting, for the window's first reading.
pub async fn requests(state: &AppState, account_id: &str) -> Result<serde_json::Value, String> {
    let handle = handle_of(state, account_id)?;
    let shared = account_of(&handle, account_id);
    let account = shared.lock().await;
    let mut names = HashMap::new();
    for c in account.saved.chats.values().filter(|c| matches!(c.stage, Stage::Requested { .. })) {
        names.insert(c.user_id, name_of(&handle, c.user_id).await);
    }
    announce_requests(state, account_id, &account.saved, &names);
    Ok(serde_json::Value::Array(state.runtime.matrix_invites(account_id).into_iter().filter(|i| i["roomId"].as_str().is_some_and(|r| r.starts_with(PREFIX))).collect()))
}

// ---------------------------------------------------------------------------------------------------------------
// The key exchange.

/// Telegram's DH prime and generator, checked once per version and kept.
async fn dh_config(handle: &Handle, saved: &mut Saved) -> Result<(i32, Vec<u8>), String> {
    let got = handle.client.invoke(&tl::functions::messages::GetDhConfig { version: saved.dh_version, random_length: 0 }).await.map_err(|e| explain(&e))?;
    if let tl::enums::messages::DhConfig::Config(c) = got {
        if !secret::good_config(&c.p, c.g) {
            return Err("Telegram's key-exchange numbers do not check out, so no secret chat was made".to_string());
        }
        saved.dh_version = c.version;
        saved.dh_g = c.g;
        saved.dh_p = hex::encode(&c.p);
    }
    let p = hex::decode(&saved.dh_p).unwrap_or_default();
    if p.len() != 256 {
        return Err("Telegram did not send its key-exchange numbers".to_string());
    }
    Ok((saved.dh_g, p))
}

/// Asks somebody for a secret chat. The conversation appears at once, waiting for them to accept on their phone.
pub async fn start(state: &AppState, account_id: &str, user_id: &str) -> Result<String, String> {
    let handle = handle_of(state, account_id)?;
    let user: i64 = user_id.trim().parse().map_err(|_| "a secret chat is with one person, named by their number".to_string())?;
    let peer = handle.session.peer_ref(PeerId::user_unchecked(user)).await.ok().flatten().ok_or_else(|| "Telegram has not told this account about that person yet".to_string())?;
    let shared = account_of(&handle, account_id);
    let mut account = shared.lock().await;
    let (g, p) = dh_config(&handle, &mut account.saved).await?;
    let a = secret::random_exponent();
    let g_a = secret::power(&secret::g_bytes(g), &a, &p);
    let made = handle
        .client
        .invoke(&tl::functions::messages::RequestEncryption { user_id: peer.into(), random_id: rand::random(), g_a })
        .await
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name == "USER_IS_BOT" => "A bot cannot have a secret chat".to_string(),
            _ => explain(&e),
        })?;
    let (id, hash) = match made {
        tl::enums::EncryptedChat::Waiting(w) => (w.id, w.access_hash),
        tl::enums::EncryptedChat::Chat(c) => (c.id, c.access_hash),
        _ => return Err("Telegram did not start the secret chat".to_string()),
    };
    let mut chat = Chat::new(id, hash, user, true, Stage::Waiting { a: hex::encode(&a) });
    let buffer = buffer_for(state, account_id, &handle, &mut chat).await;
    note(state, account_id, &buffer, "Waiting for them to accept this secret chat. It works only on the device that accepts it, and only here on this side.");
    account.saved.chats.insert(id, chat);
    keep(&account);
    Ok(buffer.id)
}

/// Answers somebody's ask: accepting works out the key and opens the conversation; declining ends it.
pub async fn answer(state: &AppState, account_id: &str, remote: &str, accept: bool) -> Result<Option<String>, String> {
    let handle = handle_of(state, account_id)?;
    let id: i32 = remote.strip_prefix(PREFIX).unwrap_or(remote).parse().map_err(|_| "that is not a secret chat".to_string())?;
    let shared = account_of(&handle, account_id);
    let mut account = shared.lock().await;
    let mut chat = account.saved.chats.get(&id).cloned().ok_or_else(|| "that ask is no longer there".to_string())?;
    let Stage::Requested { g_a } = chat.stage.clone() else { return Err("that secret chat was already answered".to_string()) };
    if !accept {
        let _ = handle.client.invoke(&tl::functions::messages::DiscardEncryption { delete_history: true, chat_id: id }).await;
        account.saved.chats.remove(&id);
        keep(&account);
        announce_requests(state, account_id, &account.saved, &HashMap::new());
        return Ok(None);
    }
    let (g, p) = dh_config(&handle, &mut account.saved).await?;
    let g_a = hex::decode(g_a).unwrap_or_default();
    if !secret::good_public(&g_a, &p) {
        let _ = handle.client.invoke(&tl::functions::messages::DiscardEncryption { delete_history: true, chat_id: id }).await;
        account.saved.chats.remove(&id);
        keep(&account);
        return Err("Their key does not check out, so the secret chat was turned down".to_string());
    }
    let b = secret::random_exponent();
    let key = secret::power(&g_a, &b, &p);
    let g_b = secret::power(&secret::g_bytes(g), &b, &p);
    handle
        .client
        .invoke(&tl::functions::messages::AcceptEncryption { peer: input_chat(&chat), g_b, key_fingerprint: secret::fingerprint(&key) })
        .await
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name == "ENCRYPTION_ALREADY_ACCEPTED" => "Another of your devices accepted it already".to_string(),
            grammers_client::InvocationError::Rpc(r) if r.name == "ENCRYPTION_ALREADY_DECLINED" => "They took the ask back".to_string(),
            _ => explain(&e),
        })?;
    chat.use_key(&key, now());
    chat.stage = Stage::Ready;
    let buffer = buffer_for(state, account_id, &handle, &mut chat).await;
    note(state, account_id, &buffer, "Secret chat started. Messages are encrypted end to end and kept only on this device.");
    let layer = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::NotifyLayer(secret::LAYER) });
    send_raw(&handle, &chat, &layer, true).await;
    account.saved.chats.insert(id, chat);
    keep(&account);
    announce_requests(state, account_id, &account.saved, &HashMap::new());
    Ok(Some(buffer.id))
}

/// Ends a secret chat on both sides, and the conversation with it.
pub async fn end(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let id = chat_of(buffer).ok_or_else(|| "that is not a secret chat".to_string())?;
    let handle = handle_of(state, &buffer.account_id)?;
    let shared = account_of(&handle, &buffer.account_id);
    let mut account = shared.lock().await;
    let _ = handle.client.invoke(&tl::functions::messages::DiscardEncryption { delete_history: true, chat_id: id }).await;
    account.saved.chats.remove(&id);
    keep(&account);
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Sending.

/// Encrypts and sends one layered message; a service message goes as one.
async fn send_raw(handle: &Handle, chat: &Chat, layered: &[u8], service: bool) -> Option<i32> {
    let data = secret::encrypt(&chat.key_bytes(), chat.x_out(), layered);
    let random_id: i64 = rand::random();
    let sent = if service {
        handle.client.invoke(&tl::functions::messages::SendEncryptedService { peer: input_chat(chat), random_id, data }).await
    } else {
        handle.client.invoke(&tl::functions::messages::SendEncrypted { silent: false, peer: input_chat(chat), random_id, data }).await
    };
    match sent {
        Ok(tl::enums::messages::SentEncryptedMessage::Message(m)) => Some(m.date),
        Ok(tl::enums::messages::SentEncryptedMessage::SentEncryptedFile(f)) => Some(f.date),
        Err(e) => {
            tracing::warn!("telegram: sending in a secret chat: {}", explain(&e));
            None
        }
    }
}

/// The chat a buffer is, ready to send in.
async fn ready(state: &AppState, buffer: &Buffer) -> Result<(Handle, Shared, i32), String> {
    let id = chat_of(buffer).ok_or_else(|| "that is not a secret chat".to_string())?;
    let handle = handle_of(state, &buffer.account_id)?;
    let shared = account_of(&handle, &buffer.account_id);
    {
        let account = shared.lock().await;
        match account.saved.chats.get(&id).map(|c| &c.stage) {
            Some(Stage::Ready) => {}
            Some(Stage::Waiting { .. }) => return Err("They have not accepted this secret chat yet".to_string()),
            Some(_) | None => return Err("This secret chat has ended".to_string()),
        }
    }
    Ok((handle, shared, id))
}

/// Says one message, as text with formatting, perhaps answering another.
pub async fn send_text(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    let (text, entities) = super::send::checked(body)?;
    let (handle, shared, id) = ready(state, buffer).await?;
    let mut account = shared.lock().await;
    let chat = account.saved.chats.get_mut(&id).ok_or("This secret chat has ended")?;
    let random_id: i64 = rand::random();
    let reply = reply_to.and_then(|r| r.parse::<i64>().ok());
    let layered = chat.next_out(Decrypted::Message { random_id, ttl: chat.ttl, text: text.clone(), media: None, entities: entities.clone(), reply_to: reply, silent: false });
    let chat = chat.clone();
    keep(&account);
    let date = send_raw(&handle, &chat, &layered, false).await.ok_or("Telegram did not take the message")?;
    drop(account);
    let html = super::messages::entities_html(&text, &entities).filter(|h| !h.is_empty());
    record(state, &buffer.account_id, &handle.own, buffer, true, random_id, &text, html, reply, Vec::new(), date as i64, chat.ttl);
    rekey_if_due(state, &buffer.account_id, &handle, id).await;
    Ok(())
}

/// Sends files, each encrypted with a key of its own and uploaded as such.
pub async fn send_files(state: &AppState, buffer: &Buffer, caption: &str, files: &[(String, bool)], reply_to: Option<&str>) -> Result<(), String> {
    let (handle, shared, id) = ready(state, buffer).await?;
    for (i, (path, _spoiler)) in files.iter().enumerate() {
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("could not read {path}: {e}"))?;
        let (key, iv) = secret::file_key();
        let sealed = secret::encrypt_file(&bytes, &key, &iv);
        // Uploaded from a private file of its own: the library uploads from a path.
        let tmp = std::env::temp_dir().join(format!("moho-secret-{}.bin", rand::random::<u64>()));
        {
            use std::io::Write;
            let mut f = crate::secure::create_private_file(&tmp).map_err(|e| e.to_string())?;
            f.write_all(&sealed).map_err(|e| e.to_string())?;
        }
        let uploaded = handle.client.upload_file(&tmp).await;
        let _ = std::fs::remove_file(&tmp);
        let uploaded = uploaded.map_err(|e| format!("could not upload {path}: {e}"))?;
        let fingerprint = secret::file_fingerprint(&key, &iv);
        let file: tl::enums::InputEncryptedFile = match uploaded.raw {
            tl::enums::InputFile::File(f) => tl::types::InputEncryptedFileUploaded { id: f.id, parts: f.parts, md5_checksum: f.md5_checksum, key_fingerprint: fingerprint }.into(),
            tl::enums::InputFile::Big(f) => tl::types::InputEncryptedFileBigUploaded { id: f.id, parts: f.parts, key_fingerprint: fingerprint }.into(),
            _ => return Err("Telegram would not take that file".to_string()),
        };
        let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let mime = crate::backend::stoat::send::content_type_of(&name).to_string();
        let caption = if i == 0 { caption.trim().to_string() } else { String::new() };
        let media = Media::Document {
            thumb: Vec::new(),
            mime: mime.clone(),
            size: bytes.len() as i64,
            key: key.to_vec(),
            iv: iv.to_vec(),
            attributes: vec![tl::types::DocumentAttributeFilename { file_name: name.clone() }.into()],
            caption: caption.clone(),
        };
        let random_id: i64 = rand::random();
        let reply = if i == 0 { reply_to.and_then(|r| r.parse::<i64>().ok()) } else { None };
        let mut account = shared.lock().await;
        let chat = account.saved.chats.get_mut(&id).ok_or("This secret chat has ended")?;
        let layered = chat.next_out(Decrypted::Message { random_id, ttl: chat.ttl, text: String::new(), media: Some(media), entities: Vec::new(), reply_to: reply, silent: false });
        let data = secret::encrypt(&chat.key_bytes(), chat.x_out(), &layered);
        let chat = chat.clone();
        keep(&account);
        drop(account);
        let sent = handle
            .client
            .invoke(&tl::functions::messages::SendEncryptedFile { silent: false, peer: input_chat(&chat), random_id: rand::random(), data, file })
            .await
            .map_err(|e| explain(&e))?;
        let date = match sent {
            tl::enums::messages::SentEncryptedMessage::Message(m) => m.date,
            tl::enums::messages::SentEncryptedMessage::SentEncryptedFile(f) => f.date,
        };
        // What was sent is kept as it is here, without fetching it back.
        let cached = format!("tsec_{}_{}_{}_full.{}", handle.own.user_id, id, random_id as u64, super::media::extension_of(Some(&name), Some(&mime)));
        let local = crate::media_cache::dir(super::media::MEDIA).join(&cached);
        let _ = std::fs::create_dir_all(crate::media_cache::dir(super::media::MEDIA));
        let _ = std::fs::write(&local, &bytes);
        let attachment = Attachment {
            kind: super::media::kind_of_mime(&mime).into(),
            mimetype: Some(mime),
            filename: Some(name),
            size: Some(bytes.len() as u64),
            path: Some(super::media::local(super::media::MEDIA, &cached)),
            ..Default::default()
        };
        record(state, &buffer.account_id, &handle.own, buffer, true, random_id, &caption, None, reply, vec![attachment], date as i64, chat.ttl);
    }
    rekey_if_due(state, &buffer.account_id, &handle, id).await;
    Ok(())
}

/// Sends a service message (a timer, a deletion, typing) in a chat that is ready.
async fn send_service(state: &AppState, buffer: &Buffer, action: Action) -> Result<(), String> {
    let (handle, shared, id) = ready(state, buffer).await?;
    let mut account = shared.lock().await;
    let chat = account.saved.chats.get_mut(&id).ok_or("This secret chat has ended")?;
    if let Action::SetTtl(t) = action {
        chat.ttl = t;
    }
    let layered = chat.next_out(Decrypted::Service { random_id: rand::random(), action });
    let chat = chat.clone();
    keep(&account);
    drop(account);
    send_raw(&handle, &chat, &layered, true).await.map(drop).ok_or_else(|| "Telegram did not take that".to_string())
}

/// Deletes a message on both sides.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let random_id: i64 = msg_id.parse().map_err(|_| "that message cannot be deleted".to_string())?;
    send_service(state, buffer, Action::Delete(vec![random_id])).await?;
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(())
}

/// Sets how long messages last after they are read; 0 is for good.
pub async fn set_timer(state: &AppState, buffer: &Buffer, seconds: i32) -> Result<(), String> {
    if !(0..=7 * 24 * 3600).contains(&seconds) {
        return Err("A timer is up to a week".to_string());
    }
    send_service(state, buffer, Action::SetTtl(seconds)).await?;
    note(state, &buffer.account_id, buffer, &timer_words("You", seconds));
    Ok(())
}

fn timer_words(who: &str, seconds: i32) -> String {
    if seconds == 0 {
        return format!("{who} turned the self-destruct timer off");
    }
    let span = match seconds {
        s if s < 60 => format!("{s} seconds"),
        s if s < 3600 => format!("{} minutes", s / 60),
        s if s < 86400 => format!("{} hours", s / 3600),
        s => format!("{} days", s / 86400),
    };
    format!("{who} set messages to disappear {span} after they are read")
}

pub async fn typing(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, shared, id) = ready(state, buffer).await?;
    let chat = shared.lock().await.saved.chats.get(&id).cloned().ok_or("This secret chat has ended")?;
    handle.client.invoke(&tl::functions::messages::SetEncryptedTyping { peer: input_chat(&chat), typing: true }).await.map(drop).map_err(|e| explain(&e))
}

/// Says the conversation was read, and starts the timer of what was read that has one.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
    let (handle, shared, id) = ready(state, buffer).await?;
    let chat = shared.lock().await.saved.chats.get(&id).cloned().ok_or("This secret chat has ended")?;
    handle.client.invoke(&tl::functions::messages::ReadEncryptedHistory { peer: input_chat(&chat), max_date: now() as i32 }).await.map_err(|e| explain(&e))?;
    start_timers(state, &buffer.id);
    Ok(())
}

/// The safety picture both people compare, and the timer, for the conversation's settings.
pub async fn info(state: &AppState, buffer: &Buffer) -> Result<serde_json::Value, String> {
    let id = chat_of(buffer).ok_or_else(|| "that is not a secret chat".to_string())?;
    let handle = handle_of(state, &buffer.account_id)?;
    let shared = account_of(&handle, &buffer.account_id);
    let account = shared.lock().await;
    let chat = account.saved.chats.get(&id).ok_or("This secret chat has ended")?;
    Ok(json!({
        "ready": chat.stage == Stage::Ready,
        "timer": chat.ttl,
        "fingerprint": chat.visual(),
        "layer": chat.layer,
    }))
}

// ---------------------------------------------------------------------------------------------------------------
// Self-destruct timers.

/// Messages with a timer waiting for the conversation to be read: (buffer, message id, seconds).
fn timed() -> &'static std::sync::Mutex<Vec<(String, String, i32)>> {
    static T: OnceLock<std::sync::Mutex<Vec<(String, String, i32)>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn start_timers(state: &AppState, buffer_id: &str) {
    let mine: Vec<(String, String, i32)> = {
        let mut all = timed().lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest) = all.drain(..).partition(|(b, _, _)| b == buffer_id);
        *all = rest;
        mine
    };
    for (buffer, id, seconds) in mine {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(seconds.max(1) as u64)).await;
            state.runtime.delete_message(&state, &buffer, &id);
        });
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Recording.

#[allow(clippy::too_many_arguments)]
fn record(state: &AppState, account_id: &str, own: &Own, buffer: &Buffer, mine: bool, random_id: i64, text: &str, html: Option<String>, reply_to: Option<i64>, attachments: Vec<Attachment>, ts: i64, ttl: i32) {
    let id = random_id.to_string();
    let reply = reply_to.and_then(|r| state.store.get_message(&buffer.id, &r.to_string()).ok().flatten()).map(|m| ReplyPreview { id: m.id.clone(), from: m.from.clone(), body: m.body.chars().take(200).collect(), ..Default::default() });
    let from = if mine { own.name.clone() } else { buffer.name.trim_start_matches("🔒 ").to_string() };
    let sender = if mine { Some(own.user_id.to_string()) } else { None };
    state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, &from, text, false, "chat", reply, Some(id.clone()), false, None, Vec::new(), attachments, sender, Some(ts), html, None);
    if ttl > 0 {
        timed().lock().unwrap_or_else(|e| e.into_inner()).push((buffer.id.clone(), id, ttl));
    }
}

/// What a received message carries, as attachments; a file is fetched and decrypted when it is first looked at.
fn attachments_of(saved: &mut Saved, account: i64, chat: i32, random_id: i64, media: &Media, file: Option<&tl::types::EncryptedFile>) -> (Vec<Attachment>, String) {
    let base = format!("tsec_{account}_{chat}_{}", random_id as u64);
    let thumb_of = |thumb: &[u8]| -> Option<String> {
        if thumb.is_empty() {
            return None;
        }
        let name = format!("{base}_thumb.jpg");
        let dir = crate::media_cache::dir(super::media::MEDIA);
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(&name), thumb).ok()?;
        Some(super::media::local(super::media::MEDIA, &name))
    };
    let mut remember = |name: &str, key: &[u8], iv: &[u8], size: i64| {
        if let Some(f) = file {
            saved.files.insert(name.to_string(), secret::SavedFile { id: f.id, access_hash: f.access_hash, dc_id: f.dc_id, size, key: hex::encode(key), iv: hex::encode(iv) });
        }
    };
    match media {
        Media::Photo { thumb, w, h, size, key, iv, caption } => {
            let name = format!("{base}_full.jpg");
            remember(&name, key, iv, *size);
            (
                vec![Attachment {
                    kind: "image".into(),
                    mimetype: Some("image/jpeg".into()),
                    filename: Some("photo.jpg".into()),
                    size: Some(*size as u64),
                    width: Some(*w as u32),
                    height: Some(*h as u32),
                    path: Some(super::media::local(super::media::MEDIA, &name)),
                    thumbnail_path: thumb_of(thumb),
                    ..Default::default()
                }],
                caption.clone(),
            )
        }
        Media::Document { thumb, mime, size, key, iv, attributes, caption } => {
            let file_name = attributes.iter().find_map(|a| match a {
                tl::enums::DocumentAttribute::Filename(f) => Some(f.file_name.clone()),
                _ => None,
            });
            let ext = super::media::extension_of(file_name.as_deref(), Some(mime));
            let name = format!("{base}_full.{ext}");
            remember(&name, key, iv, *size);
            let (w, h) = attributes
                .iter()
                .find_map(|a| match a {
                    tl::enums::DocumentAttribute::ImageSize(s) => Some((s.w as u32, s.h as u32)),
                    tl::enums::DocumentAttribute::Video(v) => Some((v.w as u32, v.h as u32)),
                    _ => None,
                })
                .unzip();
            let voice = attributes.iter().any(|a| matches!(a, tl::enums::DocumentAttribute::Audio(a) if a.voice));
            (
                vec![Attachment {
                    kind: if voice { "audio".into() } else { super::media::kind_of_mime(mime).into() },
                    mimetype: Some(mime.clone()),
                    filename: Some(file_name.unwrap_or_else(|| format!("file.{ext}"))),
                    size: Some(*size as u64),
                    width: w,
                    height: h,
                    path: Some(super::media::local(super::media::MEDIA, &name)),
                    thumbnail_path: thumb_of(thumb),
                    ..Default::default()
                }],
                caption.clone(),
            )
        }
        Media::External { attributes, .. } => {
            let emoji = attributes.iter().find_map(|a| match a {
                tl::enums::DocumentAttribute::Sticker(s) => Some(s.alt.clone()),
                _ => None,
            });
            (Vec::new(), format!("[Sticker{}]", emoji.map(|e| format!(" {e}")).unwrap_or_default()))
        }
        Media::Geo { lat, long } => (Vec::new(), format!("📍 https://www.openstreetmap.org/?mlat={lat}&mlon={long}")),
        Media::Venue { lat, long, title, address } => (Vec::new(), format!("📍 {title}, {address} https://www.openstreetmap.org/?mlat={lat}&mlon={long}")),
        Media::Contact { phone, first, last } => (Vec::new(), format!("👤 {first} {last} {phone}").trim().to_string()),
        Media::WebPage { url } => (Vec::new(), url.clone()),
        Media::Unknown => (Vec::new(), "[Something this version of moho cannot show]".to_string()),
    }
}

/// Fetches and decrypts a received file the first time it is looked at (the media cache asks).
pub async fn restore(state: &AppState, name: &str, path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let base = name.strip_prefix("tsec_").ok_or_else(|| anyhow::anyhow!("not a secret chat's file"))?;
    let account = base.split('_').next().unwrap_or("");
    let account_id = format!("telegram:{account}");
    let handle = state.runtime.telegram_handle(&account_id).ok_or_else(|| anyhow::anyhow!("that Telegram account is not connected"))?;
    let shared = account_of(&handle, &account_id);
    let file = shared.lock().await.saved.files.get(name).cloned().ok_or_else(|| anyhow::anyhow!("moho does not know that file"))?;
    let location = Location(tl::types::InputEncryptedFileLocation { id: file.id, access_hash: file.access_hash }.into(), file.dc_id, file.size as usize);
    let mut download = handle.client.iter_download(&location);
    let mut bytes = Vec::with_capacity(file.size as usize + 16);
    while let Some(chunk) = download.next().await.map_err(|e| anyhow::anyhow!(explain(&e)))? {
        bytes.extend(chunk);
    }
    let plain = secret::decrypt_file(bytes, &hex::decode(&file.key)?, &hex::decode(&file.iv)?, file.size as usize)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, plain)?;
    Ok(path.to_path_buf())
}

/// An encrypted file, as something the library can download.
struct Location(tl::enums::InputFileLocation, i32, usize);

impl grammers_client::media::Downloadable for Location {
    fn to_raw_input_location(&self) -> Option<tl::enums::InputFileLocation> {
        Some(self.0.clone())
    }
    fn size(&self) -> Option<usize> {
        Some(self.2)
    }
    fn dc_id(&self) -> Option<i32> {
        Some(self.1)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// What Telegram sends: updates, in order.

type Job = (Handle, tl::enums::Update);

fn workers() -> &'static std::sync::Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<Job>>> {
    static W: OnceLock<std::sync::Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<Job>>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// Takes an update about secret chats, to be handled after the ones before it.
pub fn update(state: &AppState, account_id: &str, handle: &Handle, update: &tl::enums::Update) {
    use tl::enums::Update as U;
    if !matches!(update, U::Encryption(_) | U::NewEncryptedMessage(_) | U::EncryptedChatTyping(_)) {
        return;
    }
    let mut all = workers().lock().unwrap_or_else(|e| e.into_inner());
    let sender = all.entry(account_id.to_string()).or_insert_with(|| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (state, account_id) = (state.clone(), account_id.to_string());
        tokio::spawn(async move {
            while let Some((handle, update)) = rx.recv().await {
                handle_one(&state, &account_id, &handle, update).await;
            }
        });
        tx
    });
    let _ = sender.send((handle.clone(), update.clone()));
}

async fn handle_one(state: &AppState, account_id: &str, handle: &Handle, update: tl::enums::Update) {
    use tl::enums::Update as U;
    match update {
        U::Encryption(u) => chat_changed(state, account_id, handle, u.chat).await,
        U::NewEncryptedMessage(u) => {
            arrived(state, account_id, handle, u.message).await;
            // Telegram may let go of what has been had.
            if u.qts > 0 {
                let _ = handle.client.invoke(&tl::functions::messages::ReceivedQueue { max_qts: u.qts }).await;
            }
        }
        U::EncryptedChatTyping(u) => {
            let shared = account_of(handle, account_id);
            let user = shared.lock().await.saved.chats.get(&u.chat_id).map(|c| c.user_id);
            if let (Some(user), Some(buffer)) = (user, state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote_of(u.chat_id).as_str()))) {
                let name = name_of(handle, user).await;
                super::updates::say_typing(state, account_id, &buffer, name);
            }
        }
        _ => {}
    }
}

/// A secret chat asked for, accepted, or ended.
async fn chat_changed(state: &AppState, account_id: &str, handle: &Handle, chat: tl::enums::EncryptedChat) {
    let shared = account_of(handle, account_id);
    let mut account = shared.lock().await;
    match chat {
        tl::enums::EncryptedChat::Requested(r) => {
            if account.saved.chats.contains_key(&r.id) {
                return;
            }
            account.saved.chats.insert(r.id, Chat::new(r.id, r.access_hash, r.admin_id, false, Stage::Requested { g_a: hex::encode(&r.g_a) }));
            keep(&account);
            let mut names = HashMap::new();
            names.insert(r.admin_id, name_of(handle, r.admin_id).await);
            announce_requests(state, account_id, &account.saved, &names);
        }
        tl::enums::EncryptedChat::Chat(c) => {
            let Some(mut chat) = account.saved.chats.get(&c.id).cloned() else { return };
            let Stage::Waiting { a } = chat.stage.clone() else { return };
            let p = hex::decode(&account.saved.dh_p).unwrap_or_default();
            let buffer = buffer_for(state, account_id, handle, &mut chat).await;
            if !secret::good_public(&c.g_a_or_b, &p) {
                let _ = handle.client.invoke(&tl::functions::messages::DiscardEncryption { delete_history: true, chat_id: c.id }).await;
                chat.stage = Stage::Ended;
                note(state, account_id, &buffer, "Their key did not check out, so the secret chat was ended.");
            } else {
                let key = secret::power(&c.g_a_or_b, &hex::decode(a).unwrap_or_default(), &p);
                if secret::fingerprint(&key) != c.key_fingerprint {
                    let _ = handle.client.invoke(&tl::functions::messages::DiscardEncryption { delete_history: true, chat_id: c.id }).await;
                    chat.stage = Stage::Ended;
                    note(state, account_id, &buffer, "The keys did not agree, so the secret chat was ended. Somebody may have been in the middle.");
                } else {
                    chat.use_key(&key, now());
                    chat.access_hash = c.access_hash;
                    chat.stage = Stage::Ready;
                    note(state, account_id, &buffer, "They accepted. Messages are encrypted end to end and kept only on this device.");
                    let layer = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::NotifyLayer(secret::LAYER) });
                    send_raw(handle, &chat, &layer, true).await;
                }
            }
            account.saved.chats.insert(c.id, chat);
            keep(&account);
        }
        tl::enums::EncryptedChat::Discarded(d) => {
            let Some(mut chat) = account.saved.chats.remove(&d.id) else { return };
            keep(&account);
            if matches!(chat.stage, Stage::Requested { .. }) {
                announce_requests(state, account_id, &account.saved, &HashMap::new());
                return;
            }
            let buffer = buffer_for(state, account_id, handle, &mut chat).await;
            note(state, account_id, &buffer, "The secret chat was ended.");
        }
        _ => {}
    }
}

/// A message in a secret chat: decrypted, put in order, and acted on.
async fn arrived(state: &AppState, account_id: &str, handle: &Handle, message: tl::enums::EncryptedMessage) {
    let (chat_id, data, date, file) = match message {
        tl::enums::EncryptedMessage::Message(m) => (m.chat_id, m.bytes, m.date, match m.file {
            tl::enums::EncryptedFile::File(f) => Some(f),
            _ => None,
        }),
        tl::enums::EncryptedMessage::Service(m) => (m.chat_id, m.bytes, m.date, None),
    };
    let shared = account_of(handle, account_id);
    let mut account = shared.lock().await;
    let Some(mut chat) = account.saved.chats.get(&chat_id).cloned() else { return };
    let buffer = buffer_for(state, account_id, handle, &mut chat).await;
    // Under the key in use, the one before it, or a new one this side was waiting to switch to.
    let fp = secret::fingerprint_of(&data);
    let plain = {
        let new_key = chat.rekey.as_ref().and_then(|r| r.key.clone()).and_then(|k| hex::decode(k).ok());
        if new_key.as_ref().is_some_and(|k| Some(secret::fingerprint(k)) == fp) {
            let key = new_key.unwrap_or_default();
            chat.use_key(&key, now());
            chat.rekey = None;
            secret::decrypt(&key, chat.x_in(), &data)
        } else if !chat.old_key.is_empty() && Some(secret::fingerprint(&hex::decode(&chat.old_key).unwrap_or_default())) == fp {
            secret::decrypt(&hex::decode(&chat.old_key).unwrap_or_default(), chat.x_in(), &data)
        } else {
            secret::decrypt(&chat.key_bytes(), chat.x_in(), &data)
        }
    };
    let layered = match plain.and_then(|p| secret::decode(&p)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("telegram[{account_id}]: a secret message could not be read: {e:#}");
            note(state, account_id, &buffer, "A message arrived that could not be decrypted.");
            account.saved.chats.insert(chat_id, chat);
            keep(&account);
            return;
        }
    };
    // Layer 8: only ever the layer notice, with no sequence numbers.
    if layered.out_seq < 0 {
        if let Decrypted::Service { action: Action::NotifyLayer(l), .. } = layered.message {
            chat.layer = l;
        }
        account.saved.chats.insert(chat_id, chat);
        keep(&account);
        return;
    }
    chat.layer = layered.layer.max(chat.layer);
    let mut queue = Vec::new();
    match secret::arrival(chat.originator, chat.in_raw, layered.out_seq) {
        Arrival::Repeat => {}
        Arrival::Broken => {
            let _ = handle.client.invoke(&tl::functions::messages::DiscardEncryption { delete_history: false, chat_id }).await;
            chat.stage = Stage::Ended;
            note(state, account_id, &buffer, "The secret chat's messages arrived out of all order, so it was ended to be safe.");
        }
        Arrival::Early { from, to } => {
            // A resend is acted on even when it arrives early.
            if let Decrypted::Service { action: Action::Resend { start, end }, .. } = &layered.message {
                resend(handle, &chat, *start, *end).await;
            }
            let raw = to + 1;
            if chat.early.is_empty() {
                let (start, end) = secret::resend_range(chat.originator, from, to);
                let ask = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::Resend { start, end } });
                send_raw(handle, &chat, &ask, true).await;
            }
            chat.early.insert(raw, hex::encode(secret::encode(&layered)));
        }
        Arrival::Next => {
            queue.push(layered);
            chat.in_raw += 1;
            chat.uses += 1;
            // What arrived early and now follows on.
            while let Some(next) = chat.early.remove(&chat.in_raw) {
                if let Ok(l) = hex::decode(next).map_err(anyhow::Error::from).and_then(|b| secret::decode(&b)) {
                    queue.push(l);
                }
                chat.in_raw += 1;
                chat.uses += 1;
            }
        }
    }
    for layered in queue {
        act(state, account_id, handle, &mut account.saved, &mut chat, &buffer, layered.message, date as i64, file.as_ref()).await;
    }
    account.saved.chats.insert(chat_id, chat);
    keep(&account);
    drop(account);
    rekey_if_due(state, account_id, handle, chat_id).await;
}

/// Sends again what the other side says it missed, with the same numbers.
async fn resend(handle: &Handle, chat: &Chat, start: i32, end: i32) {
    for raw in secret::resend_wanted(chat.originator, start, end) {
        if let Some((_, bytes)) = chat.sent.iter().find(|(r, _)| *r == raw) {
            send_raw(handle, chat, &hex::decode(bytes).unwrap_or_default(), true).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn act(state: &AppState, account_id: &str, handle: &Handle, saved: &mut Saved, chat: &mut Chat, buffer: &Buffer, message: Decrypted, date: i64, file: Option<&tl::types::EncryptedFile>) {
    let who = buffer.name.trim_start_matches("🔒 ").to_string();
    match message {
        Decrypted::Message { random_id, ttl, text, media, entities, reply_to, .. } => {
            let (attachments, extra) = media.as_ref().map(|m| attachments_of(saved, handle.own.user_id, chat.id, random_id, m, file)).unwrap_or_default();
            let body = [text.as_str(), extra.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
            let html = if extra.is_empty() { super::messages::entities_html(&text, &entities).filter(|h| !h.is_empty()) } else { None };
            record(state, account_id, &handle.own, buffer, false, random_id, &body, html, reply_to, attachments, date, ttl);
        }
        Decrypted::Service { action, .. } => match action {
            Action::SetTtl(t) => {
                chat.ttl = t;
                note(state, account_id, buffer, &timer_words(&who, t));
            }
            Action::Delete(ids) => {
                for id in ids {
                    state.runtime.delete_message(state, &buffer.id, &id.to_string());
                }
            }
            Action::Screenshot(_) => note(state, account_id, buffer, &format!("{who} took a screenshot")),
            Action::Flush => note(state, account_id, buffer, &format!("{who} cleared the history on their side")),
            Action::Resend { start, end } => resend(handle, chat, start, end).await,
            Action::NotifyLayer(l) => chat.layer = l,
            Action::Typing => super::updates::say_typing(state, account_id, buffer, who),
            Action::RequestKey { exchange, g_a } => {
                // Two asking at once: the smaller exchange gives way, silently.
                if let Some(mine) = chat.rekey.as_ref().filter(|r| r.mine) {
                    if mine.exchange > exchange {
                        return;
                    }
                }
                let p = hex::decode(&saved.dh_p).unwrap_or_default();
                if !secret::good_public(&g_a, &p) {
                    let abort = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::AbortKey(exchange) });
                    send_raw(handle, chat, &abort, true).await;
                    return;
                }
                let b = secret::random_exponent();
                let key = secret::power(&g_a, &b, &p);
                let g_b = secret::power(&secret::g_bytes(saved.dh_g), &b, &p);
                chat.rekey = Some(secret::Rekey { exchange, mine: false, exponent: hex::encode(&b), key: Some(hex::encode(&key)) });
                let accept = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::AcceptKey { exchange, g_b, fingerprint: secret::fingerprint(&key) } });
                send_raw(handle, chat, &accept, true).await;
            }
            Action::AcceptKey { exchange, g_b, fingerprint } => {
                let Some(rekey) = chat.rekey.clone().filter(|r| r.mine && r.exchange == exchange) else { return };
                let p = hex::decode(&saved.dh_p).unwrap_or_default();
                let key = secret::power(&g_b, &hex::decode(&rekey.exponent).unwrap_or_default(), &p);
                if !secret::good_public(&g_b, &p) || secret::fingerprint(&key) != fingerprint {
                    let abort = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::AbortKey(exchange) });
                    send_raw(handle, chat, &abort, true).await;
                    chat.rekey = None;
                    return;
                }
                let commit = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::CommitKey { exchange, fingerprint } });
                send_raw(handle, chat, &commit, true).await;
                chat.use_key(&key, now());
                chat.rekey = None;
            }
            Action::CommitKey { exchange, fingerprint } => {
                let Some(key) = chat.rekey.clone().filter(|r| !r.mine && r.exchange == exchange).and_then(|r| r.key).and_then(|k| hex::decode(k).ok()) else { return };
                if secret::fingerprint(&key) != fingerprint {
                    chat.rekey = None;
                    return;
                }
                chat.use_key(&key, now());
                chat.rekey = None;
                // Nothing else to say: a no-op under the new key tells the other side it was taken.
                let noop = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::Noop });
                send_raw(handle, chat, &noop, true).await;
            }
            Action::AbortKey(exchange) => {
                if chat.rekey.as_ref().is_some_and(|r| r.exchange == exchange) {
                    chat.rekey = None;
                }
            }
            Action::Read(_) | Action::Noop | Action::Unknown => {}
        },
    }
}

/// Starts a new key when the current one has been used enough (perfect forward secrecy).
async fn rekey_if_due(_state: &AppState, account_id: &str, handle: &Handle, id: i32) {
    let shared = account_of(handle, account_id);
    let mut account = shared.lock().await;
    let p = hex::decode(&account.saved.dh_p).unwrap_or_default();
    let g = account.saved.dh_g;
    let Some(chat) = account.saved.chats.get_mut(&id) else { return };
    if !chat.wants_rekey(now()) || p.len() != 256 {
        return;
    }
    let a = secret::random_exponent();
    let exchange: i64 = rand::random();
    let g_a = secret::power(&secret::g_bytes(g), &a, &p);
    chat.rekey = Some(secret::Rekey { exchange, mine: true, exponent: hex::encode(&a), key: None });
    let ask = chat.next_out(Decrypted::Service { random_id: rand::random(), action: Action::RequestKey { exchange, g_a } });
    let chat = chat.clone();
    keep(&account);
    drop(account);
    send_raw(handle, &chat, &ask, true).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_chats_conversation_is_told_apart_from_a_peer() {
        assert_eq!(remote_of(42), "secret:42");
        assert!(super::super::peers::peer_id_of(&remote_of(42)).is_none(), "never read as a person or a group");
    }

    #[test]
    fn the_timer_is_said_in_words() {
        assert_eq!(timer_words("You", 0), "You turned the self-destruct timer off");
        assert_eq!(timer_words("Ada", 30), "Ada set messages to disappear 30 seconds after they are read");
        assert_eq!(timer_words("Ada", 3600), "Ada set messages to disappear 1 hours after they are read");
        assert_eq!(timer_words("Ada", 604800), "Ada set messages to disappear 7 days after they are read");
    }
}
