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
    // A group left stays to be read, as on the phone, and nothing more is sent to it.
    if let Thread::Group(key) = &thread {
        if handle.store.has_left(key) {
            return Err("You are no longer in this group".to_string());
        }
    }
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
        // An .m4a is audio in an MP4 container, as Signal's own clients label it; a bare .aac is the raw stream.
        Some("m4a") => "audio/mp4",
        Some("aac") => "audio/aac",
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
    if buffer.remote_id.as_deref() == Some(super::stories::REMOTE) {
        return story_reply(state, buffer, body, paths, reply_to).await;
    }
    let (handle, thread) = target(state, buffer)?;
    // In a group, `@Name` for a member is a mention, which Signal carries as a character and a range over it.
    let (body, mentioned) = match &thread {
        Thread::Group(key) => compose::mentions(body, &members_of(state, buffer, &handle, key).await),
        Thread::Contact(_) => (body.to_string(), Vec::new()),
    };
    let (plain, mut styles) = compose::parse(&body);
    styles.extend(compose::mention_ranges(&plain, &mentioned));
    styles.sort_by_key(|r| (r.start, std::cmp::Reverse(r.length)));
    if plain.trim().is_empty() && paths.is_empty() {
        return Err("there is nothing to send".to_string());
    }
    if paths.len() > MOST {
        return Err(format!("Signal takes at most {MOST} files in a message, and this has {}", paths.len()));
    }
    let mut uploads = Vec::new();
    let mut local = Vec::new();
    // Over Signal's 2000 bytes, the message carries the first part as its text and the whole as a file, as Signal's
    // clients send it.
    let (wire_text, wire_ranges, long) = cut_long(&plain, &styles);
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
    if let Some(whole) = long {
        uploads.push(super::Upload { name: "long-message.txt".to_string(), content_type: super::media::LONG_TEXT.to_string(), bytes: whole.into_bytes(), width: None, height: None });
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
    let quote = reply_to.and_then(|id| commands::quote_of(state, &handle.store, &buffer.id, id));
    let reply = reply_to.zip(quote.as_ref()).map(|(id, q)| crate::model::ReplyPreview {
        id: id.to_string(),
        from: q.author_aci.as_deref().and_then(|a| uuid::Uuid::parse_str(a).ok()).map(|a| name_here(state, buffer, &a, &handle)).unwrap_or_default(),
        body: q.text.clone().unwrap_or_default(),
        thread: false,
        forwarded: false,
        thread_root: None,
        chat: None,
    });
    let (plain_for_wire, ranges_for_wire) = (wire_text, wire_ranges);
    let timer = presage::store::ContentsStore::expire_timer(&handle.store, &thread).await.ok().flatten().map(|(seconds, _)| seconds).filter(|s| *s > 0);
    let noted_thread = thread.clone();
    let sent_pointers = pointers.clone();
    let at = ask(&handle, |done| Command::Send { thread, body: plain_for_wire, ranges: ranges_for_wire, quote, attachments: sent_pointers, done }).await?;
    let names: Vec<(String, String)> = mentioned.iter().map(|id| (id.to_string(), name_here(state, buffer, id, &handle))).collect();
    let drawn = ranges::render(&plain, &styles, |aci| names.iter().find(|(id, _)| id == aci).map(|(_, n)| n.clone()).unwrap_or_else(|| "someone".to_string()));
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_else(|| "Me".to_string());
    // The files are on hand already: they go into the cache under the names the attachments will be looked for by.
    let mut attachments = super::media::attachments_of(&buffer.account_id, &noted_thread, &handle.own, at, &pointers);
    attachments.retain(|a| a.mimetype.as_deref() != Some(super::media::LONG_TEXT));
    for (attachment, from) in attachments.iter().zip(&local) {
        if let Some(to) = attachment.path.as_deref().and_then(|p| p.strip_prefix("file://")) {
            if let Some(parent) = std::path::Path::new(to).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(from, to);
        }
    }
    // The link previews the library built while sending (when the phone allows them), from its saved copy: without this
    // the phone showed the card and moho's own copy did not (seen on the first real account).
    let embeds = match super::media::stored_content(&handle.store, &noted_thread, at).await {
        Some(content) => super::media::data_message_of(&content).map(|dm| super::media::previews_of(&buffer.account_id, &noted_thread, &handle.own, at, dm)).unwrap_or_default(),
        None => Vec::new(),
    };
    state.runtime.record_message_at(
        state, &buffer.account_id, &buffer.name, &buffer.kind, &own, &drawn.body, false, "chat", reply, Some(ids::message_id(at, &handle.own)), false, None, embeds, attachments, Some(handle.own.to_string()), Some((at / 1000) as i64), drawn.html, None,
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

/// Answers a story in words: the reply goes to the person who posted it, in the conversation with them, and is shown
/// there as Signal's clients show it.
async fn story_reply(state: &AppState, buffer: &Buffer, body: &str, paths: &[(String, bool)], reply_to: Option<&str>) -> Result<(), String> {
    let handle = state.runtime.signal_handle(&buffer.account_id).ok_or("that Signal account is not connected")?;
    let story = reply_to.ok_or("Stories are answered one at a time: reply to the story you mean")?;
    if !paths.is_empty() {
        return Err("A story is answered in words or with a reaction, not with a file".to_string());
    }
    let (thread, context) = super::stories::context_of(&handle.store, story).ok_or("that is not a story")?;
    // One's own story is answered in its group, where the members talk about it; to oneself there is nobody to tell.
    if matches!(&thread, Thread::Contact(id) if ids::uuid_of(id) == handle.own) {
        return Err("That is your own story".to_string());
    }
    let (plain, _) = compose::parse(body);
    if plain.trim().is_empty() {
        return Err("there is nothing to send".to_string());
    }
    let words = plain.clone();
    let at = ask(&handle, |done| Command::StoryAnswer { thread: thread.clone(), context, body: Some(words), emoji: None, done }).await?;
    // Drawn in the conversation with them, where Signal's clients put it, saying what it answers.
    let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
    let theirs = super::people::buffer_for(&ctx, &thread).await;
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_else(|| "Me".to_string());
    state.runtime.record_message_at(
        state, &theirs.account_id, &theirs.name, &theirs.kind, &own, &format!("[Reply to a story]\n{plain}"), false, "chat", None, Some(ids::message_id(at, &handle.own)), false, None, Vec::new(), Vec::new(),
        Some(handle.own.to_string()), Some((at / 1000) as i64), None, None,
    );
    Ok(())
}

/// Reacts to message `msg_id`, or takes the reaction back.
pub async fn react(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    if buffer.remote_id.as_deref() == Some(super::stories::REMOTE) {
        let handle = state.runtime.signal_handle(&buffer.account_id).ok_or("that Signal account is not connected")?;
        let (thread, context) = super::stories::context_of(&handle.store, msg_id).ok_or("that is not a story")?;
        ask(&handle, |done| Command::StoryAnswer { thread, context, body: None, emoji: Some((emoji.to_string(), !add)), done }).await?;
        state.runtime.update_reaction(state, &buffer.id, msg_id, emoji, true, add);
        return Ok(());
    }
    let (handle, thread) = target(state, buffer)?;
    let (sent_ms, target_author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    // An edited message is named by its newest revision, as Signal's clients name it.
    let target_ms = super::revisions::latest_ms(&handle.store, msg_id, sent_ms);
    let emoji_owned = emoji.to_string();
    ask(&handle, |done| Command::React { thread, target_ms, target_author, emoji: emoji_owned, remove: !add, done }).await?;
    // Counted as Signal counts it: the account's new reaction replaces its last, and the same one twice is one.
    super::inbox::apply_reaction(state, &handle.store, &buffer.id, msg_id, &handle.own, true, emoji, !add);
    Ok(())
}

/// Takes one of the account's own messages back, for everyone.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (sent_ms, author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    if author != handle.own {
        return Err("Only your own messages can be taken back".to_string());
    }
    let target_ms = super::revisions::latest_ms(&handle.store, msg_id, sent_ms);
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
    let revision_ms = ask(&handle, |done| Command::Edit { thread, target_ms, body: wire_body, ranges: wire_ranges, done }).await?;
    super::revisions::note(&handle.store, &handle.own, revision_ms, msg_id);
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

/// The most a message's own text may be, in bytes; past it the whole text goes as a file.
const MOST_TEXT: usize = 2000;

/// A text that is too long for a message, cut where Signal's clients cut it: the first part (on a character boundary)
/// with the styles that fit in it, and the whole text to send beside it. `None` for the whole when it fits.
pub fn cut_long(plain: &str, styles: &[presage::proto::BodyRange]) -> (String, Vec<presage::proto::BodyRange>, Option<String>) {
    if plain.len() <= MOST_TEXT {
        return (plain.to_string(), styles.to_vec(), None);
    }
    let mut end = MOST_TEXT;
    while !plain.is_char_boundary(end) {
        end -= 1;
    }
    let head = plain[..end].to_string();
    let units = head.encode_utf16().count() as u32;
    let kept = styles.iter().filter(|r| r.start.unwrap_or(0) + r.length.unwrap_or(0) <= units).cloned().collect();
    (head, kept, Some(plain.to_string()))
}

/// The members of a group other than the account, with what they are called, for finding mentions.
async fn members_of(state: &AppState, buffer: &Buffer, handle: &Handle, key: &[u8; 32]) -> Vec<(String, uuid::Uuid)> {
    let Ok(Some(group)) = presage::store::ContentsStore::group(&handle.store, *key).await else { return Vec::new() };
    let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
    let mut out = Vec::new();
    for id in group.members.iter().map(|m| uuid::Uuid::from(m.aci)).filter(|id| *id != handle.own) {
        out.push((super::people::person(&ctx, &id).await, id));
    }
    out
}

async fn wait<T>(answer: oneshot::Receiver<Result<T, String>>, patience: Duration) -> Result<T, String> {
    match tokio::time::timeout(patience, answer).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}

fn uuid_param(params: &serde_json::Value, name: &str) -> Result<uuid::Uuid, String> {
    params.get(name).and_then(|v| v.as_str()).and_then(|s| uuid::Uuid::parse_str(s.trim_start_matches("p:")).ok()).ok_or_else(|| format!("that needs a Signal person's id as \"{name}\""))
}

/// The edit an RPC's `action` and parameters name.
pub fn edit_of(params: &serde_json::Value) -> Result<super::manage::Edit, String> {
    use super::manage::{Edit, Link, Who};
    let text = |name: &str| params.get(name).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let flag = |name: &str| params.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
    let who = |name: &str| Who::parse(&text(name)).ok_or_else(|| format!("\"{name}\" is all or admins"));
    Ok(match text("action").as_str() {
        "rename" => Edit::Rename(text("name")),
        // The file is read by edit_group (and uploaded on the account's thread); an empty "path" takes the picture away.
        "picture" => Edit::Picture(String::new()),
        "describe" => Edit::Describe(text("text")),
        "timer" => Edit::Timer(params.get("seconds").and_then(|v| v.as_u64()).unwrap_or(0) as u32),
        "add" => {
            let ids = params.get("userIds").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let people: Vec<(uuid::Uuid, bool)> = ids.iter().filter_map(|v| v.as_str()).filter_map(|s| uuid::Uuid::parse_str(s.trim_start_matches("p:")).ok()).map(|u| (u, false)).collect();
            if people.is_empty() {
                return Err("nobody to add".to_string());
            }
            Edit::Add(people)
        }
        "remove" => Edit::Remove(uuid_param(params, "userId")?),
        "admin" => Edit::Admin(uuid_param(params, "userId")?, flag("admin")),
        "uninvite" => Edit::Uninvite(uuid_param(params, "userId")?),
        "approve" => Edit::Approve(uuid_param(params, "userId")?, flag("accept")),
        "leave" => Edit::Leave,
        "link" => Edit::Link(match text("mode").as_str() {
            "off" => Link::Off,
            "on" => Link::On,
            "approval" => Link::Approval,
            _ => return Err("the link is off, on or approval".to_string()),
        }),
        "resetLink" => Edit::ResetLink,
        "whoMayEdit" => Edit::DetailsRights(who("who")?),
        "whoMayAdd" => Edit::AddRights(who("who")?),
        "announcementsOnly" => Edit::AnnouncementsOnly(flag("on")),
        other => return Err(format!("a Signal group has no action \"{other}\"")),
    })
}

/// Changes a group (the buffer is the group's conversation).
pub async fn edit_group(state: &AppState, buffer: &Buffer, params: &serde_json::Value) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let Thread::Group(key) = thread else { return Err("that is not a group".to_string()) };
    // People to add may be named by username or a known number as well as by id.
    let mut params = params.clone();
    if let Some(named) = params.get("userIds").and_then(|v| v.as_array()).cloned() {
        let mut ids = Vec::new();
        for name in named.iter().filter_map(|v| v.as_str()) {
            let id = match uuid::Uuid::parse_str(name.trim().trim_start_matches("p:")) {
                Ok(id) => id,
                Err(_) => super::inbox::find_person(state, &handle, name.trim()).await.map_err(|e| e.to_string())?,
            };
            ids.push(serde_json::Value::String(id.to_string()));
        }
        params["userIds"] = serde_json::Value::Array(ids);
    }
    let edit = edit_of(&params)?;
    let picture = match (&edit, params.get("path").and_then(|v| v.as_str()).map(str::trim)) {
        (super::manage::Edit::Picture(_), Some(path)) if !path.is_empty() => Some(group_picture(path)?),
        _ => None,
    };
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::EditGroup { key, edit, picture, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    wait(answer, Duration::from_secs(120)).await
}

/// The most a group's picture may weigh: Signal's clients make theirs a small square, and the CDN's form is for that.
const BIGGEST_GROUP_PICTURE: u64 = 2 * 1024 * 1024;

/// A picture to put up as a group's: read from `path`, and only a picture (JPEG, PNG, WebP or GIF, by its first bytes)
/// small enough to be one.
fn group_picture(path: &str) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(path).map_err(|e| format!("that picture could not be read: {e}"))?.len();
    if size > BIGGEST_GROUP_PICTURE {
        return Err("That picture is too big for a group: pick one under 2 MB".to_string());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("that picture could not be read: {e}"))?;
    if !is_picture(&bytes) {
        return Err("That is not a picture (JPEG, PNG, WebP or GIF)".to_string());
    }
    Ok(bytes)
}

/// Whether bytes begin as a JPEG, PNG, WebP or GIF does.
fn is_picture(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"GIF8")
        || (bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

/// What the group's settings page shows.
pub async fn group_info(state: &AppState, buffer: &Buffer) -> Result<serde_json::Value, String> {
    use presage::libsignal_service::groups_v2::{AccessRequired, Role};
    use presage::store::ContentsStore;
    let (handle, thread) = target(state, buffer)?;
    let key = match &thread {
        Thread::Group(key) => *key,
        // A conversation with one person has only its timer to show.
        Thread::Contact(_) => {
            let timer = handle.store.expire_timer(&thread).await.ok().flatten().map(|(seconds, _)| seconds).unwrap_or(0);
            return Ok(serde_json::json!({ "dm": true, "timer": timer }));
        }
    };
    let group = handle.store.group(key).await.ok().flatten().ok_or("that group is not known to this account")?;
    let own = presage::libsignal_service::protocol::Aci::from(handle.own);
    let admin = group.members.iter().any(|m| m.aci == own && matches!(m.role, Role::Administrator));
    let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
    let words = |a: Option<&AccessRequired>| match a {
        Some(AccessRequired::Administrator) => "admins",
        _ => "all",
    };
    let access = group.access_control.as_ref();
    let link_mode = match access.map(|a| &a.add_from_invite_link) {
        Some(AccessRequired::Any) => "on",
        Some(AccessRequired::Administrator) => "approval",
        _ => "off",
    };
    let mut requesting = Vec::new();
    for r in &group.requesting_members {
        let who = uuid::Uuid::from(r.aci);
        requesting.push(serde_json::json!({ "userId": who.to_string(), "nick": super::people::person(&ctx, &who).await }));
    }
    let mut members = Vec::new();
    for m in &group.members {
        let who = uuid::Uuid::from(m.aci);
        members.push(serde_json::json!({ "userId": who.to_string(), "nick": super::people::person(&ctx, &who).await, "admin": matches!(m.role, Role::Administrator), "me": m.aci == own }));
    }
    let mut pending = Vec::new();
    for p in &group.pending_members {
        pending.push(serde_json::json!({ "userId": p.uuid.to_string(), "nick": super::people::person(&ctx, &p.uuid).await, "mine": p.added_by_aci == own }));
    }
    Ok(serde_json::json!({
        "dm": false,
        "title": group.title,
        "description": group.description.clone().unwrap_or_default(),
        "timer": group.disappearing_messages_timer.as_ref().map(|t| t.duration).unwrap_or(0),
        "isAdmin": admin,
        "memberCount": group.members.len(),
        "linkMode": link_mode,
        // Only admins are shown the link to hand out; it is a key to the group.
        "link": if link_mode != "off" && admin && !group.invite_link_password.is_empty() { Some(super::manage::link_of(&key, &group.invite_link_password)) } else { None },
        "whoMayEdit": words(access.map(|a| &a.attributes)),
        "whoMayAdd": words(access.map(|a| &a.members)),
        "members": members,
        "requesting": requesting,
        "pending": pending,
    }))
}

/// Makes a group with these people in it; answers its conversation.
pub async fn create_group(state: &AppState, account_id: &str, title: &str, members: &[String]) -> Result<String, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    if title.trim().is_empty() {
        return Err("A group needs a name".to_string());
    }
    // Named by id, username or a known number.
    let mut ids = Vec::new();
    for name in members.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        ids.push(match uuid::Uuid::parse_str(name.trim_start_matches("p:")) {
            Ok(id) => id,
            Err(_) => super::inbox::find_person(state, &handle, name).await.map_err(|e| format!("{name}: {e}"))?,
        });
    }
    let members = ids;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::CreateGroup { title: title.to_string(), members, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let key = wait(answer, Duration::from_secs(120)).await?;
    let remote = format!("g:{}", ids::hex(&key));
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote.as_str())).map(|b| b.id).ok_or_else(|| "the group was made but its conversation did not open".to_string())
}

/// What a group link leads to.
pub async fn link_info(state: &AppState, account_id: &str, link: &str) -> Result<serde_json::Value, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::LinkInfo { link: link.to_string(), done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let (title, members, approval) = wait(answer, Duration::from_secs(60)).await?;
    Ok(serde_json::json!({ "title": title, "memberCount": members, "needsApproval": approval }))
}

/// Joins a group through its link: the conversation when in, `None` when an admin has to approve first.
pub async fn join_link(state: &AppState, account_id: &str, link: &str) -> Result<Option<String>, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::JoinByLink { link: link.to_string(), done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let (key, joined) = wait(answer, Duration::from_secs(120)).await?;
    if !joined {
        return Ok(None);
    }
    let remote = format!("g:{}", ids::hex(&key));
    Ok(state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote.as_str())).map(|b| b.id))
}

/// Sets how long messages last in a conversation (0: they do not disappear). A group's is a change to the group.
pub async fn set_timer(state: &AppState, buffer: &Buffer, seconds: u32) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    if let Thread::Group(_) = thread {
        return edit_group(state, buffer, &serde_json::json!({ "action": "timer", "seconds": seconds })).await;
    }
    let at = ask(&handle, |done| Command::SetTimer { thread, seconds, done }).await?;
    // Said here as on the phone: nothing comes back to say it (seen on the first real account, where the phone showed
    // the change and moho did not).
    let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
    super::inbox::system(&ctx, buffer, &timer_line("You", seconds), at);
    Ok(())
}

/// How a change of timer is said in the conversation.
pub(super) fn timer_line(who: &str, seconds: u32) -> String {
    if seconds == 0 {
        format!("{who} turned off disappearing messages")
    } else {
        format!("{who} set disappearing messages to {}", super::people::timer_words(seconds))
    }
}

/// Blocks or unblocks a person (`p:<aci>` or their id) or a group (`g:<key>`).
pub async fn block(state: &AppState, account_id: &str, id: &str, blocked: bool) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let id = id.trim();
    let thread = ids::parse_remote_id(id).or_else(|| uuid::Uuid::parse_str(id).ok().map(|u| Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(u.into())))).ok_or("that is not a Signal person or group")?;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::Block { thread, blocked, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    wait(answer, Duration::from_secs(60)).await
}

/// Marks a person's safety number as verified (or not), as the account's other devices will see it.
pub async fn verify(state: &AppState, account_id: &str, user_id: &str, verified: bool) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let who = uuid::Uuid::parse_str(user_id.trim().trim_start_matches("p:")).map_err(|_| "that is not a Signal person".to_string())?;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::Verify { who, verified, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    wait(answer, Duration::from_secs(60)).await
}

/// The account's own profile as Signal has it: name parts, about, about emoji, username.
pub async fn own_profile(state: &AppState, account_id: &str) -> Result<serde_json::Value, String> {
    use presage::store::ContentsStore;
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let id = presage::libsignal_service::protocol::ServiceId::Aci(handle.own.into());
    let profile = match handle.store.profile_key(&id).await.ok().flatten() {
        Some(key) => handle.store.profile(handle.own, key).await.ok().flatten(),
        None => None,
    };
    let kept = super::storage::current(account_id, &handle.store);
    let (given, family) = profile.as_ref().and_then(|p| p.name.as_ref()).map(|n| (n.given_name.clone(), n.family_name.clone().unwrap_or_default())).unwrap_or_default();
    Ok(serde_json::json!({
        "given": given,
        "family": family,
        "about": profile.as_ref().and_then(|p| p.about.clone()).unwrap_or_default(),
        "emoji": profile.as_ref().and_then(|p| p.about_emoji.clone()).unwrap_or_default(),
        "username": kept.own_username,
    }))
}

/// Changes the account's own profile.
pub async fn set_profile(state: &AppState, account_id: &str, params: &serde_json::Value) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let text = |name: &str| params.get(name).and_then(|v| v.as_str()).unwrap_or("").to_string();
    if text("given").trim().is_empty() {
        return Err("Signal needs at least a first name".to_string());
    }
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::SetProfile { given: text("given"), family: text("family"), about: text("about"), emoji: text("emoji"), done }).map_err(|_| "that Signal account is not connected".to_string())?;
    wait(answer, Duration::from_secs(60)).await
}

/// The account's settings as the phone keeps them, and who it blocks: for the account card. Changed on the phone.
pub async fn settings(state: &AppState, account_id: &str) -> Result<serde_json::Value, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let synced = handle.memory.synced();
    let kept = super::storage::current(account_id, &handle.store);
    let ctx = super::people::Ctx::new(state, account_id, handle.store.clone(), handle.own);
    let mut blocked = Vec::new();
    for (remote, _) in kept.conversations.iter().filter(|(_, s)| s.blocked) {
        let name = match ids::parse_remote_id(remote) {
            Some(Thread::Contact(id)) => super::people::person(&ctx, &ids::uuid_of(&id)).await,
            Some(Thread::Group(key)) => super::people::group_title(&ctx, &key).await,
            None => continue,
        };
        blocked.push(serde_json::json!({ "id": remote, "name": name }));
    }
    Ok(serde_json::json!({
        "readReceipts": synced.read_receipts,
        "typingIndicators": synced.typing_indicators,
        "linkPreviews": synced.link_previews,
        "sealedSenderIndicators": synced.unidentified_delivery_indicators,
        "defaultTimer": kept.account.map(|a| a.default_timer),
        "blocked": blocked,
    }))
}

/// Votes for option `choice` of the conversation's open poll.
pub async fn vote(state: &AppState, buffer: &Buffer, choices: &[u32]) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let poll_id = state.runtime.live_card(&buffer.id, "poll").and_then(|c| c["id"].as_str().map(String::from)).ok_or("there is no poll open here")?;
    let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
    let (vote, poll) = super::polls::own_vote(&ctx, &poll_id, choices)?;
    ask(&handle, |done| Command::Vote { thread, vote, done }).await?;
    super::polls::keep_own(&ctx, buffer, &poll_id, &poll);
    Ok(())
}

/// The most answers a poll takes, as Signal's clients allow.
pub const MOST_POLL_ANSWERS: usize = 10;

/// What a poll asks, checked: a question and from two to ten answers, none empty.
pub fn poll_of(question: &str, answers: &[String], multiple: bool) -> Result<presage::proto::data_message::PollCreate, String> {
    let question = question.trim();
    if question.is_empty() {
        return Err("A poll needs a question".to_string());
    }
    let options: Vec<String> = answers.iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
    if options.len() < 2 {
        return Err("A poll needs at least two answers".to_string());
    }
    if options.len() > MOST_POLL_ANSWERS {
        return Err(format!("A Signal poll takes at most {MOST_POLL_ANSWERS} answers"));
    }
    Ok(presage::proto::data_message::PollCreate { question: Some(question.to_string()), options, allow_multiple: Some(multiple) })
}

/// Asks a poll in the conversation, drawn here on the poll card at once as it is on the other devices.
pub async fn start_poll(state: &AppState, buffer: &Buffer, question: &str, answers: &[String], multiple: bool) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let create = poll_of(question, answers, multiple)?;
    let sent = create.clone();
    let at = ask(&handle, |done| Command::Poll { thread, create: sent, done }).await?;
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_else(|| "Me".to_string());
    let line = format!("[Poll: {}]", create.question.clone().unwrap_or_default());
    state.runtime.record_message_at(
        state, &buffer.account_id, &buffer.name, &buffer.kind, &own, &line, false, "chat", None, Some(ids::message_id(at, &handle.own)), false, None, Vec::new(), Vec::new(), Some(handle.own.to_string()), Some((at / 1000) as i64), None, None,
    );
    let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
    super::polls::created(&ctx, buffer, &handle.own, at, &create);
    Ok(())
}

/// Ends a poll the account asked: nobody can vote in it after, here or on the other devices.
pub async fn end_poll(state: &AppState, buffer: &Buffer, poll_id: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (target_ms, author) = ids::parse_message_id(poll_id).ok_or("that is not a Signal poll")?;
    if author != handle.own {
        return Err("Only whoever asked a poll can end it".to_string());
    }
    ask(&handle, |done| Command::EndPoll { thread, target_ms, done }).await?;
    let ctx = super::people::Ctx::new(state, &buffer.account_id, handle.store.clone(), handle.own);
    super::polls::ended(&ctx, buffer, &handle.own, &presage::proto::data_message::PollTerminate { target_sent_timestamp: Some(target_ms) });
    Ok(())
}

/// The most a GIF picked from the picker may weigh: they are short loops, and a bigger one is not one.
const BIGGEST_GIF: usize = 25 * 1024 * 1024;

/// Sends a GIF from the window's picker the way Signal's clients do: the video itself, as an attachment marked GIF so
/// it loops without sound, rather than a link to it. Fetched through the account's route.
pub async fn gif(state: &AppState, buffer: &Buffer, url: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let parsed = url::Url::parse(url).map_err(|_| "that is not a GIF's address".to_string())?;
    if parsed.scheme() != "https" {
        return Err("that is not a GIF's address".to_string());
    }
    let router = crate::net::route::router();
    let routed = router.routed(&buffer.account_id);
    if routed {
        router.ready(|_| {}).await.map_err(|e| format!("the route is not ready: {e:#}"))?;
    }
    let http = router.client_if("signal-gifs", routed, |b| b.timeout(Duration::from_secs(60)).user_agent(super::network::USER_AGENT));
    let response = http.get(parsed).send().await.map_err(|e| format!("the GIF could not be fetched: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("the GIF could not be fetched ({})", response.status()));
    }
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").split(';').next().unwrap_or("").trim().to_string();
    let bytes = response.bytes().await.map_err(|e| format!("the GIF could not be fetched: {e}"))?;
    if bytes.len() > BIGGEST_GIF {
        return Err("that GIF is too big to send".to_string());
    }
    let content_type = match content_type.as_str() {
        "video/mp4" | "image/gif" | "image/webp" | "video/webm" => content_type,
        _ if url.ends_with(".mp4") => "video/mp4".to_string(),
        _ => "image/gif".to_string(),
    };
    let name = if content_type == "video/mp4" { "gif.mp4" } else { "gif.gif" }.to_string();
    let (done, answer) = oneshot::channel();
    let upload = super::Upload { name, content_type, bytes: bytes.to_vec(), width: None, height: None };
    handle.commands.send(Command::Upload { files: vec![upload], done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let mut pointers = wait(answer, Duration::from_secs(300)).await?;
    for pointer in &mut pointers {
        pointer.flags = Some(pointer.flags.unwrap_or(0) | 8);
    }
    let at = ask(&handle, |done| Command::Send { thread: thread.clone(), body: String::new(), ranges: Vec::new(), quote: None, attachments: pointers.clone(), done }).await?;
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_else(|| "Me".to_string());
    let attachments = super::media::attachments_of(&buffer.account_id, &thread, &handle.own, at, &pointers);
    // The file is on hand: it goes into the cache where the attachment will be looked for, so it is not fetched back.
    if let Some(to) = attachments.first().and_then(|a| a.path.as_deref()).and_then(|p| p.strip_prefix("file://")) {
        if let Some(parent) = std::path::Path::new(to).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(to, &bytes);
    }
    state.runtime.record_message_at(
        state, &buffer.account_id, &buffer.name, &buffer.kind, &own, "", false, "chat", None, Some(ids::message_id(at, &handle.own)), false, None, Vec::new(), attachments,
        Some(handle.own.to_string()), Some((at / 1000) as i64), None, None,
    );
    Ok(())
}

/// The stickers of the account's installed packs (installed on the phone; the library fetches them), each written to
/// the cache once so the picker can draw it.
pub async fn stickers(state: &AppState, buffer: &Buffer) -> Result<serde_json::Value, String> {
    use presage::store::ContentsStore;
    let handle = state.runtime.signal_handle(&buffer.account_id).ok_or("that Signal account is not connected")?;
    let packs = handle.store.sticker_packs().await.map_err(|e| e.to_string())?;
    let dir = crate::media_cache::dir(super::media::MEDIA).join("stickers");
    let _ = std::fs::create_dir_all(&dir);
    let mut out = Vec::new();
    for pack in packs.filter_map(Result::ok) {
        for sticker in &pack.manifest.stickers {
            let Some(bytes) = &sticker.bytes else { continue };
            let path = dir.join(format!("{}-{}.webp", ids::hex(&pack.id), sticker.id));
            if !path.exists() {
                let _ = std::fs::write(&path, bytes);
            }
            let emoji = sticker.emoji.clone().unwrap_or_default();
            out.push(serde_json::json!({
                "id": format!("{}:{}", ids::hex(&pack.id), sticker.id),
                "name": if emoji.is_empty() { "sticker".to_string() } else { emoji.clone() },
                "pack": pack.manifest.title,
                "body": emoji,
                "url": format!("file://{}", path.display()),
            }));
        }
    }
    Ok(serde_json::Value::Array(out))
}

/// Sends sticker `id` (`<pack id hex>:<number>`) from the picker.
pub async fn sticker(state: &AppState, buffer: &Buffer, id: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (pack, number) = id.split_once(':').ok_or("that is not a Signal sticker")?;
    let pack_id = ids::unhex(pack).ok_or("that is not a Signal sticker")?;
    let sticker_id: u32 = number.parse().map_err(|_| "that is not a Signal sticker".to_string())?;
    let at = ask(&handle, |done| Command::SendSticker { thread, pack_id, sticker_id, done }).await?;
    // Drawn from the picture the picker already wrote, as the account's own message.
    let path = crate::media_cache::dir(super::media::MEDIA).join("stickers").join(format!("{pack}-{sticker_id}.webp"));
    let attachment = crate::model::Attachment {
        kind: "image".to_string(),
        mimetype: Some("image/webp".to_string()),
        filename: None,
        size: std::fs::metadata(&path).ok().map(|m| m.len()),
        width: None,
        height: None,
        blurhash: None,
        duration_secs: None,
        waveform: None,
        voice: false,
        path: Some(format!("file://{}", path.display())),
        thumbnail_path: None,
        url: None,
    };
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_else(|| "Me".to_string());
    state.runtime.record_message_at(
        state, &buffer.account_id, &buffer.name, &buffer.kind, &own, "", false, "chat", None, Some(ids::message_id(at, &handle.own)), false, None, Vec::new(), vec![attachment],
        Some(handle.own.to_string()), Some((at / 1000) as i64), None, None,
    );
    Ok(())
}

/// The message requests waiting on an account.
pub async fn requests(state: &AppState, account_id: &str) -> Result<serde_json::Value, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    Ok(super::requests::listing_for(state, &handle).await)
}

/// Accepts, deletes or blocks a message request (`id` is the conversation, `p:<aci>` or `g:<key>`).
pub async fn answer_request(state: &AppState, account_id: &str, id: &str, answer: &str) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let thread = super::ids::parse_remote_id(id).ok_or("that is not a Signal conversation")?;
    let answer = super::requests::Answer::parse(answer).ok_or("the answer is accept, delete or block")?;
    let (done, reply) = oneshot::channel();
    handle.commands.send(Command::AnswerRequest { thread, answer, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(Duration::from_secs(60), reply).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_picture_is_only_a_picture() {
        assert!(is_picture(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]));
        assert!(is_picture(b"\x89PNG\r\n\x1a\n...."));
        assert!(is_picture(b"GIF89a"));
        assert!(is_picture(b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!is_picture(b"#!/bin/sh\nrm -rf"));
        assert!(!is_picture(b"RIFF\0\0\0\0WAVE"));
        assert!(!is_picture(&[]));
    }

    #[test]
    fn a_poll_asks_a_question_with_two_to_ten_answers() {
        let answers = |n: usize| (1..=n).map(|i| format!(" answer {i} ")).collect::<Vec<_>>();
        let poll = poll_of(" Lunch? ", &answers(3), true).unwrap();
        assert_eq!((poll.question.as_deref(), poll.options.len(), poll.options[0].as_str(), poll.allow_multiple), (Some("Lunch?"), 3, "answer 1", Some(true)));
        assert!(poll_of("", &answers(2), false).is_err());
        assert!(poll_of("Lunch?", &answers(1), false).is_err());
        assert!(poll_of("Lunch?", &[" ".to_string(), "Soup".to_string()], false).is_err(), "an empty answer is not one");
        assert!(poll_of("Lunch?", &answers(10), false).is_ok());
        assert!(poll_of("Lunch?", &answers(11), false).is_err());
    }

    #[test]
    fn a_change_of_timer_is_said_as_signal_says_it() {
        assert_eq!(timer_line("You", 300), "You set disappearing messages to 5 minutes");
        assert_eq!(timer_line("Alice", 0), "Alice turned off disappearing messages");
    }

    #[test]
    fn a_file_is_sent_as_what_its_name_says_it_is() {
        assert_eq!(content_type_of("/tmp/Cat.JPG"), "image/jpeg");
        assert_eq!(content_type_of("a.webm"), "video/webm");
        assert_eq!(content_type_of("voice.opus"), "audio/ogg");
        assert_eq!(content_type_of("song.M4A"), "audio/mp4");
        assert_eq!(content_type_of("raw.aac"), "audio/aac");
        assert_eq!(content_type_of("noext"), "application/octet-stream");
        assert_eq!(content_type_of("archive.zip"), "application/octet-stream");
    }

    #[test]
    fn only_a_picture_has_a_size_to_read() {
        assert_eq!(dimensions_of("/nonexistent.png", "image/png"), (None, None));
        assert_eq!(dimensions_of("/nonexistent.mp4", "video/mp4"), (None, None));
    }

    #[test]
    fn a_long_message_is_cut_on_a_character_and_sent_whole_beside() {
        let short = "hello";
        assert_eq!(super::cut_long(short, &[]).2, None);
        let long = format!("{}é{}", "a".repeat(1999), "b".repeat(50));
        let range = |start, length| presage::proto::BodyRange { start: Some(start), length: Some(length), associated_value: None };
        let (head, kept, whole) = super::cut_long(&long, &[range(0, 5), range(1990, 30)]);
        assert_eq!(head.len(), 1999, "the two-byte letter would straddle the limit, so the cut is before it");
        assert_eq!(kept.len(), 1, "a style running past the cut is left off the short part");
        assert_eq!(whole.as_deref(), Some(long.as_str()));
    }
}
