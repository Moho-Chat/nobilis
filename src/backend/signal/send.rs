//! Saying things: what the window's send, react, delete and edit do for a Signal conversation.
//!
//! Each asks the account's thread to do it (see `commands`), waits for the answer, and - only once Signal has taken the
//! message - draws it. A message that did not go is not left in the conversation as though it had.

use super::{commands, compose, ids, ranges, Command, Done, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use presage::store::Thread;
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a send may take before it is called lost. Signal itself retries; the window should not wait forever.
const PATIENCE: Duration = Duration::from_secs(90);

fn target(state: &AppState, buffer: &Buffer) -> Result<(Handle, Thread), String> {
    let handle = state.runtime.signal_handle(&buffer.account_id).ok_or("that Signal account is not connected")?;
    let thread = buffer.remote_id.as_deref().and_then(ids::parse_remote_id).ok_or("that conversation is not a Signal one")?;
    Ok((handle, thread))
}

async fn ask(handle: &Handle, make: impl FnOnce(Done) -> Command) -> Result<u64, String> {
    let (done, answer) = oneshot::channel();
    handle.commands.send(make(done)).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(PATIENCE, answer).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}

/// Sends text, optionally as a reply to the message `reply_to`.
pub async fn text(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    files(state, buffer, body, &[], reply_to, None).await
}

/// The most Signal takes in one attachment, and in one message.
const BIGGEST: u64 = 100 * 1024 * 1024;
const MOST: usize = 32;

fn content_type_of(path: &str) -> &'static str {
    match std::path::Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "aac") => "audio/aac",
        Some("ogg" | "oga" | "opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// The pixel size of a picture, if it can be read without decoding the whole of it.
fn dimensions_of(path: &str, content_type: &str) -> (Option<u32>, Option<u32>) {
    if !content_type.starts_with("image/") {
        return (None, None);
    }
    match image::ImageReader::open(path).and_then(|r| r.with_guessed_format()).map(|r| r.into_dimensions()) {
        Ok(Ok((w, h))) => (Some(w), Some(h)),
        _ => (None, None),
    }
}

/// Sends text and files: `paths` are put on Signal's servers first, then the message carries them.
pub async fn files(state: &AppState, buffer: &Buffer, body: &str, paths: &[(String, bool)], reply_to: Option<&str>, progress: Option<&crate::upload::Progress>) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (plain, styles) = compose::parse(body);
    if plain.trim().is_empty() && paths.is_empty() {
        return Err("there is nothing to send".to_string());
    }
    if paths.len() > MOST {
        return Err(format!("Signal takes at most {MOST} files in a message, and this has {}", paths.len()));
    }
    let mut uploads = Vec::new();
    let mut local = Vec::new();
    let total: u64 = paths.iter().filter_map(|(p, _)| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Preparing, total as usize, "Signal");
    }
    for (path, _) in paths {
        let size = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
        if size > BIGGEST {
            return Err(format!("{path} is {} MB and Signal takes at most {} MB", size / 1_000_000, BIGGEST / 1_000_000));
        }
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading {path}: {e}"))?;
        let content_type = content_type_of(path);
        let (width, height) = dimensions_of(path, content_type);
        let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        local.push(path.clone());
        uploads.push(super::Upload { name, content_type: content_type.to_string(), bytes, width, height });
    }
    let pointers = if uploads.is_empty() {
        Vec::new()
    } else {
        if let Some(p) = progress {
            p.at(crate::upload::Phase::Sending, total as usize, "Signal");
        }
        let (done, answer) = oneshot::channel();
        handle.commands.send(Command::Upload { files: uploads, done }).map_err(|_| "that Signal account is not connected".to_string())?;
        let pointers = match tokio::time::timeout(Duration::from_secs(600), answer).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err("the connection ended before Signal took the files".to_string()),
            Err(_) => return Err("Signal did not take the files in time".to_string()),
        };
        if let Some(p) = progress {
            p.at(crate::upload::Phase::Waiting, total as usize, "Signal");
        }
        pointers
    };
    let quote = reply_to.and_then(|id| commands::quote_of(state, &buffer.id, id));
    let reply = reply_to.zip(quote.as_ref()).map(|(id, q)| crate::model::ReplyPreview {
        id: id.to_string(),
        from: q.author_aci.as_deref().and_then(|a| uuid::Uuid::parse_str(a).ok()).map(|a| name_here(state, buffer, &a, &handle)).unwrap_or_default(),
        body: q.text.clone().unwrap_or_default(),
        thread: false,
        forwarded: false,
        thread_root: None,
        chat: None,
    });
    let (plain_for_wire, ranges_for_wire) = (plain.clone(), styles.clone());
    let timer = presage::store::ContentsStore::expire_timer(&handle.store, &thread).await.ok().flatten().map(|(seconds, _)| seconds).filter(|s| *s > 0);
    let noted_thread = thread.clone();
    let sent_pointers = pointers.clone();
    let at = ask(&handle, |done| Command::Send { thread, body: plain_for_wire, ranges: ranges_for_wire, quote, attachments: sent_pointers, done }).await?;
    let drawn = ranges::render(&plain, &styles, |_| "someone".to_string());
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_else(|| "Me".to_string());
    // The files are on hand already: they go into the cache under the names the attachments will be looked for by.
    let attachments = super::media::attachments_of(&buffer.account_id, &noted_thread, &handle.own, at, &pointers);
    for (attachment, from) in attachments.iter().zip(&local) {
        if let Some(to) = attachment.path.as_deref().and_then(|p| p.strip_prefix("file://")) {
            if let Some(parent) = std::path::Path::new(to).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(from, to);
        }
    }
    state.runtime.record_message_at(
        state, &buffer.account_id, &buffer.name, &buffer.kind, &own, &drawn.body, false, "chat", reply, Some(ids::message_id(at, &handle.own)), false, None, Vec::new(), attachments, Some(handle.own.to_string()), Some((at / 1000) as i64), drawn.html, None,
    );
    // The conversation has a disappearing timer, which the library put on the message: it runs from now.
    if let Some(seconds) = timer {
        let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
        super::expiry::from_sent(&ctx, &buffer.id, &noted_thread, &ids::message_id(at, &handle.own), at, seconds);
    }
    Ok(())
}

/// The name already on a message in this conversation, for a quote's author.
fn name_here(state: &AppState, buffer: &Buffer, author: &uuid::Uuid, handle: &Handle) -> String {
    if *author == handle.own {
        return state.runtime.own_identity(&buffer.account_id).unwrap_or_default();
    }
    state
        .store
        .get_backlog(&buffer.id, i64::MAX, 200)
        .ok()
        .and_then(|rows| rows.into_iter().find(|m| m.sender_id.as_deref() == Some(author.to_string().as_str())).map(|m| m.from))
        .unwrap_or_default()
}

/// Reacts to message `msg_id`, or takes the reaction back.
pub async fn react(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (target_ms, target_author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    let emoji_owned = emoji.to_string();
    ask(&handle, |done| Command::React { thread, target_ms, target_author, emoji: emoji_owned, remove: !add, done }).await?;
    state.runtime.update_reaction(state, &buffer.id, msg_id, emoji, true, add);
    Ok(())
}

/// Takes one of the account's own messages back, for everyone.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (target_ms, author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    if author != handle.own {
        return Err("Only your own messages can be taken back".to_string());
    }
    ask(&handle, |done| Command::Delete { thread, target_ms, done }).await?;
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(())
}

/// Changes one of the account's own messages.
pub async fn edit(state: &AppState, buffer: &Buffer, msg_id: &str, body: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (target_ms, author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    if author != handle.own {
        return Err("Only your own messages can be changed".to_string());
    }
    let (plain, styles) = compose::parse(body);
    if plain.trim().is_empty() {
        return Err("a message cannot be changed to nothing; delete it instead".to_string());
    }
    let (wire_body, wire_ranges) = (plain.clone(), styles);
    ask(&handle, |done| Command::Edit { thread, target_ms, body: wire_body, ranges: wire_ranges, done }).await?;
    let held = state.store.get_message(&buffer.id, msg_id).ok().flatten();
    let (embeds, attachments) = held.map(|m| (m.embeds, m.attachments)).unwrap_or_default();
    state.runtime.update_message(state, &buffer.id, msg_id, &plain, &embeds, &attachments);
    Ok(())
}

/// Says the account is typing. Only when the phone's privacy settings say typing indicators are on: until they are known,
/// nothing is told.
pub fn typing(state: &AppState, buffer: &Buffer, started: bool) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    if handle.memory.synced().typing_indicators != Some(true) {
        return Ok(());
    }
    handle.commands.send(Command::Typing { thread, started }).map_err(|_| "that Signal account is not connected".to_string())
}

/// The conversation was read here: the senders of what was new are told it was read (if read receipts are on), and the
/// phone is told so it does not keep the conversation unread.
pub fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    use presage::libsignal_service::protocol::{Aci, ServiceId};
    let (handle, _) = target(state, buffer)?;
    // Looking at a conversation is what starts the clock on what was waiting to disappear in it.
    super::expiry::started(&handle.store, &buffer.id);
    let rows = state.store.get_backlog(&buffer.id, i64::MAX, 100).map_err(|e| e.to_string())?;
    let mut by_sender: std::collections::HashMap<uuid::Uuid, Vec<u64>> = std::collections::HashMap::new();
    {
        let mut done = handle.memory.receipted.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows.iter().filter(|m| !m.is_own && m.kind == "chat") {
            let Some((ts, author)) = ids::parse_message_id(&row.id) else { continue };
            if done.insert(row.id.clone()) {
                by_sender.entry(author).or_default().push(ts);
            }
        }
    }
    if by_sender.is_empty() {
        return Ok(());
    }
    let synced = handle.memory.synced();
    // Read receipts are a promise to the other person about what is shown to them; only the phone's own setting decides.
    if synced.read_receipts == Some(true) {
        for (sender, timestamps) in &by_sender {
            let _ = handle.commands.send(Command::Receipt { to: ServiceId::Aci(Aci::from(*sender)), kind: presage::proto::receipt_message::Type::Read, timestamps: timestamps.clone() });
        }
    }
    let reads: Vec<(uuid::Uuid, u64)> = by_sender.into_iter().flat_map(|(sender, ts)| ts.into_iter().map(move |t| (sender, t))).collect();
    let _ = handle.commands.send(Command::ReadSync { reads });
    Ok(())
}


/// The account's devices, newest link last, as the phone lists them.
pub async fn devices(state: &AppState, account_id: &str) -> Result<Vec<super::DeviceRow>, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::Devices { done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let mut rows = match tokio::time::timeout(Duration::from_secs(60), answer).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err("the connection ended before Signal answered".to_string()),
        Err(_) => return Err("Signal did not answer in time".to_string()),
    };
    rows.sort_by_key(|d| d.created_at);
    Ok(rows)
}

/// Changes the name the phone's list shows for this device.
pub async fn rename_device(state: &AppState, account_id: &str, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a device needs a name".to_string());
    }
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::Rename { name: name.to_string(), done }).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(Duration::from_secs(60), answer).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err("the connection ended before Signal answered".to_string()),
        Err(_) => return Err("Signal did not answer in time".to_string()),
    }
    let owned = name.to_string();
    let _ = state.accounts.mutate_signal(account_id, |a| a.device_name = owned);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_sent_as_what_its_name_says_it_is() {
        assert_eq!(content_type_of("/tmp/Cat.JPG"), "image/jpeg");
        assert_eq!(content_type_of("a.webm"), "video/webm");
        assert_eq!(content_type_of("voice.opus"), "audio/ogg");
        assert_eq!(content_type_of("noext"), "application/octet-stream");
        assert_eq!(content_type_of("archive.zip"), "application/octet-stream");
    }

    #[test]
    fn only_a_picture_has_a_size_to_read() {
        assert_eq!(dimensions_of("/nonexistent.png", "image/png"), (None, None));
        assert_eq!(dimensions_of("/nonexistent.mp4", "video/mp4"), (None, None));
    }
}
