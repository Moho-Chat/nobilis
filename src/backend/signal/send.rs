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
    let names: Vec<(String, String)> = mentioned.iter().map(|id| (id.to_string(), name_here(state, buffer, id, &handle))).collect();
    let drawn = ranges::render(&plain, &styles, |aci| names.iter().find(|(id, _)| id == aci).map(|(_, n)| n.clone()).unwrap_or_else(|| "someone".to_string()));
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
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::EditGroup { key, edit, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    wait(answer, Duration::from_secs(120)).await
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
    ask(&handle, |done| Command::SetTimer { thread, seconds, done }).await.map(|_| ())
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
