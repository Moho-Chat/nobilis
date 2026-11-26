//! Everything this client puts into a room.
//!
//! Over the account's one socket (chat.rs). A post waits for its own echo,
//! which carries the nonce it was sent with; an edit or a removal waits for
//! the chat's `command_ack`. Either way the call answers with what the chat
//! said, so a refused message is said to be refused rather than left grey.

use super::*;
use serde_json::json;

/// The message a reply answers: who wrote it, its id, and what it said.
#[derive(Clone, Debug)]
pub struct ReplyTo {
    pub from: String,
    pub id: String,
    pub body: String,
}

/// The body that goes out: the reply's quote, as the site's own client
/// writes one, then what was typed.
///
/// A reply with nothing of theirs to quote - "Mention" on a name arms one with
/// no message - is a mention instead: `@Name, ` at the front, the way people
/// address each other in the room, unless the text already opens with them.
pub(super) fn compose(body: &str, reply: Option<&ReplyTo>) -> String {
    let text = body.trim();
    match reply {
        Some(r) => match protocol::reply_quote(&r.from, &r.id, &r.body) {
            quote if !quote.is_empty() => format!("{quote}{text}"),
            _ => addressed(&r.from, text),
        },
        None => text.to_string(),
    }
}

/// `text` opened with a mention of `name`, unless it already names them first.
fn addressed(name: &str, text: &str) -> String {
    let name = name.trim();
    let lower = text.to_lowercase();
    let at = format!("@{}", name.to_lowercase());
    let already = lower.strip_prefix(&at).is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_alphanumeric()));
    if name.is_empty() || already {
        text.to_string()
    } else {
        format!("@{name}, {text}")
    }
}

/// Sends a message to the room a buffer shows, optionally as a reply.
///
/// What the site's own composer understands typed here as well: `/w Name
/// text` (or `/whisper`) whispers - a private room with that person - and
/// `/topic text` (or `/subtitle`) sets the room's subtitle. `@Name` for
/// anybody the chat has shown is sent as the mention the chat reads.
pub async fn send_message(state: &AppState, account_id: &str, buffer_name: &str, body: &str, reply_to: Option<ReplyTo>) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let trimmed = body.trim_start();
    for command in ["/whisper ", "/w ", "/msg "] {
        if let Some(rest) = trimmed.strip_prefix(command) {
            let (target, text) = whisper_target(&chat, rest).ok_or_else(|| anyhow!("whisper to whom? Nobody of that name has been seen in this chat"))?;
            return send_whisper(state, account_id, &target, &text, Some(buffer_name)).await;
        }
    }
    for command in ["/topic ", "/subtitle "] {
        if let Some(rest) = trimmed.strip_prefix(command) {
            return set_subtitle(account_id, buffer_name, rest.trim()).await;
        }
    }
    post(account_id, buffer_name, &compose(body, reply_to.as_ref())).await
}

/// Posts text as it stands to the room a buffer shows, mentions written as
/// the chat reads them, and waits for its echo.
async fn post(account_id: &str, buffer_name: &str, text: &str) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let original = text;
    let text = protocol::names_to_mentions(text, &chat.known_names());
    if text.is_empty() {
        return Ok(());
    }
    if text.len() > 15 * 1024 {
        bail!("that's longer than the chat takes");
    }
    let room = chat.room_of_buffer(buffer_name).ok_or_else(|| anyhow!("\"{buffer_name}\" isn't one of this account's rooms"))?;
    if !chat.joined(&room) {
        bail!("not in this room right now - it is still joining");
    }
    let nonce = protocol::post_nonce();
    let sent = chat.command(&nonce, json!({ "t": "send", "chat_room_uuid": room, "body": text, "client_nonce": nonce })).await;
    // A message that carried mention tags and was refused for a reason the
    // chat has no word for goes again with the names as plain text: said
    // without the ping beats not said at all.
    match sent {
        Err(e) if text != original && e.to_string().starts_with("the chat refused it") => {
            tracing::warn!("sneedchat[{account_id}]: the chat refused a message with mentions ({e:#}); sending it with plain names");
            let nonce = protocol::post_nonce();
            chat.command(&nonce, json!({ "t": "send", "chat_room_uuid": room, "body": original, "client_nonce": nonce })).await
        }
        other => other,
    }
}

/// Who a typed whisper is for and what it says: the longest name the chat
/// has shown that the text starts with, since names have spaces in them.
fn whisper_target(chat: &super::chat::Chat, rest: &str) -> Option<(String, String)> {
    let rest = rest.trim_start().trim_start_matches('@');
    let mut names: Vec<String> = chat.known_names().into_iter().map(|(_, n)| n).collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
    names.into_iter().find_map(|name| {
        let head = rest.get(..name.len())?;
        let tail = &rest[name.len()..];
        (head.eq_ignore_ascii_case(&name) && tail.chars().next().is_none_or(|c| c.is_whitespace() || c == ',' || c == ':'))
            .then(|| (name.clone(), tail.trim_start_matches([',', ':']).trim().to_string()))
    })
}

/// Whispers to somebody: in the private room this account already has with
/// them, or a new one, which the chat opens (`open_side_room`) and which
/// appears as a conversation of its own.
pub async fn send_whisper(_state: &AppState, account_id: &str, target: &str, body: &str, _from_buffer: Option<&str>) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let person = chat.person_named(target).ok_or_else(|| anyhow!("nobody called {target} has been seen in this chat"))?;
    let buffer = match chat.side_room_with(&person.uuid) {
        Some((_, buffer)) => buffer,
        None => {
            let key = format!("side:{}", person.uuid);
            chat.command(&key, json!({ "t": "open_side_room", "principal_uuids": [person.uuid] })).await.context("opening a private room")?;
            chat.side_room_with(&person.uuid).map(|(_, b)| b).ok_or_else(|| anyhow!("the chat opened no private room with {}", person.name))?
        }
    };
    if body.trim().is_empty() {
        return Ok(());
    }
    // A moment for the new room's join to land before the post that needs it.
    for _ in 0..20 {
        if chat.room_of_buffer(&buffer).is_some_and(|r| chat.joined(&r)) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    post(account_id, &buffer, body.trim()).await
}

/// This account's side rooms, for choosing where to invite somebody.
pub fn side_rooms(account_id: &str) -> Vec<serde_json::Value> {
    super::chat::chat(account_id)
        .side_rooms()
        .into_iter()
        .map(|(_, name, group)| json!({ "bufferId": crate::model::buffer_id(account_id, &name), "name": name, "group": group }))
        .collect()
}

/// Somebody to invite, by name: everyone the chat has shown this account -
/// in its rooms' member lists, its conversations, and their stored history.
/// The chat has no directory of its own to search; this is all anybody can
/// be found by, here or on the site.
pub fn search_people(state: &AppState, account_id: &str, query: &str) -> Vec<serde_json::Value> {
    let chat = super::chat::chat(account_id);
    let q = query.trim().trim_start_matches('@').to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let me = chat.me.lock().unwrap().as_ref().map(|m| m.uuid.clone()).unwrap_or_default();
    let mut found: Vec<(String, String)> = chat
        .people
        .lock()
        .unwrap()
        .values()
        .filter(|p| p.name.to_lowercase().contains(&q))
        .map(|p| (p.uuid.clone(), p.name.clone()))
        .collect();
    for (uuid, name) in state.store.senders_matching(account_id, &q, 50).unwrap_or_default() {
        if protocol::is_uuid(&uuid) && !found.iter().any(|(u, _)| u == &uuid) {
            found.push((uuid, name));
        }
    }
    found.retain(|(u, _)| u != &me);
    // Those whose name starts with what was typed first, then by name.
    found.sort_by(|(_, a), (_, b)| {
        let (a_starts, b_starts) = (a.to_lowercase().starts_with(&q), b.to_lowercase().starts_with(&q));
        b_starts.cmp(&a_starts).then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    found.truncate(30);
    found.into_iter().map(|(uuid, name)| json!({ "id": uuid, "name": name })).collect()
}

/// Invites somebody (by principal uuid, or a name the chat has shown) to a
/// side room: to the one a conversation shows, or to a new one. Answers the
/// conversation they were invited into.
pub async fn invite(account_id: &str, who: &str, to_buffer: Option<&str>) -> Result<String> {
    let chat = super::chat::chat(account_id);
    let principal = if protocol::is_uuid(who) {
        who.to_lowercase()
    } else {
        chat.person_named(who).map(|p| p.uuid).ok_or_else(|| anyhow!("nobody called {who} has been seen in this chat"))?
    };
    match to_buffer {
        Some(buffer) => {
            let room = chat.room_of_buffer(buffer).ok_or_else(|| anyhow!("\"{buffer}\" isn't one of this account's side rooms"))?;
            if chat.in_room(&room, &principal) {
                return Ok(buffer.to_string());
            }
            chat.command(&format!("invite:{room}"), json!({ "t": "invite", "chat_room_uuid": room, "principal_uuid": principal })).await?;
            Ok(buffer.to_string())
        }
        None => {
            chat.command(&format!("side:{principal}"), json!({ "t": "open_side_room", "principal_uuids": [principal] })).await.context("opening a side room")?;
            chat.side_room_with(&principal).map(|(_, b)| b).ok_or_else(|| anyhow!("the chat opened no side room"))
        }
    }
}

/// Sets the room's subtitle - what the site shows under its name.
pub async fn set_subtitle(account_id: &str, buffer_name: &str, text: &str) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let room = chat.room_of_buffer(buffer_name).ok_or_else(|| anyhow!("\"{buffer_name}\" isn't one of this account's rooms"))?;
    let op = protocol::operation_id();
    chat.command(&op, json!({ "t": "subtitle", "chat_room_uuid": room, "chat_room_subtitle": text, "chat_operation_uuid": op })).await
}

/// Adds or takes back this account's reaction on a message. The chat holds
/// one reaction per person per message: a new one replaces the old, and
/// taking one back sends no reaction at all.
pub async fn react(account_id: &str, msg_id: &str, emoji: &str, add: bool) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let code = super::catalogue::reaction_code(emoji).ok_or_else(|| anyhow!("Kiwi Farms reactions are its own set - pick one from the reaction list"))?;
    let message = msg_id.to_lowercase();
    // Taking back a reaction that is not ours is nothing to do.
    if !add && chat.own_reaction(&message).as_deref() != Some(code.as_str()) {
        return Ok(());
    }
    let reaction = if add { json!(code) } else { serde_json::Value::Null };
    chat.command(&format!("react:{message}"), json!({ "t": "react", "chat_message_uuid": message, "reaction_code": reaction })).await
}

/// Who reacted to a message with a reaction, by name.
pub async fn reactors(account_id: &str, buffer_name: &str, msg_id: &str, emoji: &str) -> Result<Vec<String>> {
    let chat = super::chat::chat(account_id);
    let code = super::catalogue::reaction_code(emoji).ok_or_else(|| anyhow!("not one of the chat's reactions"))?;
    let room = chat.room_of_buffer(buffer_name).ok_or_else(|| anyhow!("not one of this account's rooms"))?;
    let session = chat.session.lock().unwrap().clone().ok_or_else(|| anyhow!("not connected to the chat right now"))?;
    let host = chat.host.lock().unwrap().clone();
    let v = super::chat::api_get(&session, &host, &format!("rooms/{room}/messages/{}/reactions", msg_id.to_lowercase())).await?;
    Ok(v["reactions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["reaction_code"].as_str() == Some(code.as_str()))
        .flat_map(|r| r["reactors"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p["display_name"].as_str().map(str::to_string))
        .collect())
}

/// What the reaction picker offers for this account: the site's reactions.
pub fn reaction_choices() -> Vec<serde_json::Value> {
    super::catalogue::reactions()
        .into_iter()
        .filter_map(|r| Some(json!({ "id": r.code, "name": r.title, "url": r.path? })))
        .collect()
}

/// What this account may do in the room a buffer shows.
pub fn powers(account_id: &str, buffer_name: &str) -> serde_json::Value {
    let chat = super::chat::chat(account_id);
    // A room not joined yet (the daemon has just started) is answered as any
    // member's, not as one this account may do nothing in: the window keeps
    // what it is told until the connection says to ask again.
    let p = match chat.room_of_buffer(buffer_name) {
        Some(room) => super::catalogue::powers(&room),
        None => super::catalogue::Powers { post: true, react: true, whisper: true, moderate: false },
    };
    json!({ "post": p.post, "react": p.react, "whisper": p.whisper, "moderate": p.moderate, "frozen": chat.frozen() })
}

/// A moderator's action on somebody: `mute` or `ban`, in this room or across
/// the whole chat (`estate`), for `minutes` or for good; or `lift` to undo
/// the sanction they are under.
pub async fn moderate(account_id: &str, buffer_name: &str, who: &str, action: &str, minutes: Option<u64>, site_wide: bool, reason: Option<&str>) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let person = match chat.person_named(who) {
        Some(p) => p.uuid,
        None if protocol::is_uuid(who) => who.to_string(),
        None => bail!("nobody called {who} has been seen in this chat"),
    };
    let op = protocol::operation_id();
    let frame = match action {
        "lift" => {
            let sanction = chat.sanction_of(&person).ok_or_else(|| anyhow!("no mute or ban on them has been seen here to lift"))?;
            json!({ "t": "lift_sanction", "chat_sanction_uuid": sanction, "chat_operation_uuid": op })
        }
        "mute" | "ban" => {
            let room = if site_wide { None } else { Some(chat.room_of_buffer(buffer_name).ok_or_else(|| anyhow!("not one of this account's rooms"))?) };
            let expires = minutes.map(|m| chrono::Utc::now().timestamp() + (m as i64) * 60);
            json!({
                "t": "sanction",
                "principal_uuid": person,
                "chat_room_uuid": room,
                "sanction_kind": action,
                "expires_at": expires,
                "reason": reason.filter(|r| !r.trim().is_empty()),
                "chat_operation_uuid": op,
            })
        }
        other => bail!("not a moderation action: {other}"),
    };
    chat.command(&op, frame).await
}

/// The page of a room's history before the oldest message held.
pub async fn older_history(state: &AppState, account_id: &str, buffer_name: &str) -> Result<()> {
    super::chat::older(state, account_id, buffer_name).await
}

/// Brings back a message that was taken down.
pub async fn restore(account_id: &str, msg_id: &str) -> Result<()> {
    operate(account_id, msg_id, "restore", None).await
}

/// The "+" attachment button's SneedChat backend. Unlike Discord (whose API
/// natively accepts a file alongside a message), Sneedchat's own chat
/// protocol is text-only - there's no upload endpoint on the site itself.
/// This does what regulars already do by hand: upload the file to a
/// separate anonymous image host, then post the resulting URL as a normal
/// chat message wrapped in the [img] BBCode the site itself expects for a
/// picture to actually render there - the same `[img]...[/img]` shape
/// find_attachment_url/normalizeBBCode already know how to unwrap on the
/// receiving end. Previously qu.ax; switched to postimg.cc after qu.ax
/// stopped reliably serving uploaded images back out.
///
/// Wrapped as `[url=<page>][img]<direct>[/img][/url]` rather than a bare
/// `[img]...[/img]` - postimg.cc's own "Thumbnail for forums" BBCode
/// preset (the one shown on its own result page) uses exactly this shape,
/// giving the posted image a click-through back to the postimg.cc page
/// alongside the inline preview, which a bare [img] tag wouldn't.
///
/// Two requests, not one: the upload POST only returns the *page* URL
/// (`https://postimg.cc/<slug>/<hash>`), not a direct image link - the
/// actual `i.postimg.cc/...` link (and the server's own possibly-
/// sanitized version of the filename) only appears in that page's own
/// HTML, so it has to be fetched and scraped same as the reference
/// upload flow this was ported from does.
///
/// Deliberately `Transport::Direct` (plain clearnet), not the account's own
/// Tor transport: postimg.cc is a general image host, not part of Kiwi
/// Farms - nothing it sees is tied to the Sneedchat account or identity at
/// all (it's a plain anonymous upload, no auth). Following qu.ax's own
/// precedent of not routing this over Tor - free upload hosts commonly
/// block Tor exit traffic outright.
///
/// A fresh, throwaway HttpClient rather than the account's own logged-in
/// session: postimg.cc needs no authentication at all for an anonymous
/// upload, so there's nothing to gain from reusing the Sneedchat session's
/// cookies, and building a new client here means this doesn't need any new
/// account-level state threaded through Runtime just for this one feature.
/// Which host to put it on. `None` keeps postimg.cc, which is what this
/// always did and what the site's own regulars use.
///
/// Honouring the caller's choice is the whole point: the uploads setting
/// promised it covered "anything Sneedchat's own uploader refuses", and it
/// reached IRC only - a Sneedchat send called straight into the postimg path
/// below, whose signature had nowhere to put a host. Attaching a video there
/// failed with "postimg.cc only accepts images" and no way to pick something
/// that would take it.
pub async fn send_attachment(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    caption: &str,
    file_path: &str,
    host: Option<crate::upload::Host>,
    progress: Option<&crate::upload::Progress>,
) -> Result<()> {
    // Only used to confirm the account is real before spending any time on
    // the upload - the transport below is deliberately unrelated to it.
    state.accounts.get_sneedchat(account_id).ok_or_else(|| anyhow!("no such account"))?;

    // The chat's own hosting is the default - nothing leaves the site, and it
    // takes video, audio and other files too - but the site's front door
    // refuses uploads for side rooms (whispers, group chats) with a bare 403.
    // There the file goes to a shared host as it did before the chat hosted
    // files: postimg for a picture it takes, catbox for anything else.
    let host = match host {
        None | Some(crate::upload::Host::Native) if super::chat::chat(account_id).is_side_room(buffer_name) => {
            let name = std::path::Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or_default();
            Some(if crate::upload::Host::Postimg.accepts(name) { crate::upload::Host::Postimg } else { crate::upload::Host::Catbox })
        }
        other => other,
    };
    match host {
        None | Some(crate::upload::Host::Native) => {
            return send_native(account_id, buffer_name, caption, file_path, progress).await;
        }
        Some(crate::upload::Host::Postimg) => {}
        Some(host) => {
            return send_via_upload_host(state, account_id, buffer_name, caption, file_path, host, progress).await
        }
    }

    let links = match upload_to_postimg_reporting(file_path, progress).await {
        Ok(links) => {
            if let Some(progress) = progress {
                progress.done(None);
            }
            links
        }
        Err(e) => {
            if let Some(progress) = progress {
                progress.done(Some(&e.to_string()));
            }
            return Err(e);
        }
    };

    let wrapped = format!("[url={}][img]{}[/img][/url]", links.page, links.direct);
    let text = if caption.trim().is_empty() { wrapped } else { format!("{caption}\n{wrapped}") };
    // The caption already carries any mention the caller wanted; an image
    // post is not separately a reply.
    send_message(state, account_id, buffer_name, &text, None).await
}

/// Posting a file the chat hosts itself: stored under the room, then written
/// into the message as the `[file]` tile the chat draws. The tile comes first
/// and the caption after, as the site's own composer lays them out.
async fn send_native(account_id: &str, buffer_name: &str, caption: &str, file_path: &str, progress: Option<&crate::upload::Progress>) -> Result<()> {
    let result = async {
        let chat = super::chat::chat(account_id);
        let room = chat.room_of_buffer(buffer_name).ok_or_else(|| anyhow!("\"{buffer_name}\" isn't one of this account's rooms"))?;
        // Before the upload rather than after it: a file stored for a room that
        // cannot take the message is a file nobody will see.
        if !chat.joined(&room) {
            bail!("not in this room right now - it is still joining");
        }
        let stored = super::hosting::store(account_id, &room, file_path, progress).await?;
        let text = if caption.trim().is_empty() { stored.tag } else { format!("{} {}", stored.tag, caption.trim()) };
        Ok(text)
    }
    .await;
    if let Some(progress) = progress {
        progress.done(result.as_ref().err().map(|e: &anyhow::Error| format!("{e:#}")).as_deref());
    }
    post(account_id, buffer_name, &result?).await
}

/// Posting a file that went to one of the shared upload hosts.
///
/// The link still has to be wrapped in `[img]` for a picture to render
/// rather than sit there as a URL - that is the site's own markup, and what
/// find_attachment_url already unwraps on the way back in. Anything that is
/// not a picture goes as a bare link, because `[img]` around a video would
/// render as a broken image instead of something clickable.
///
/// No `[url=]` wrapper here, unlike the postimg path: these hosts serve the
/// file itself rather than a page about it, so there is nothing to click
/// through to that the image is not already showing.
pub(super) async fn send_via_upload_host(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    caption: &str,
    file_path: &str,
    host: crate::upload::Host,
    progress: Option<&crate::upload::Progress>,
) -> Result<()> {
    // Refused here as well as left out of the menu, because the menu is only
    // one client's: a link from a host the forum does not embed is a picture
    // nobody on the other end sees.
    if !host.offered_to("sneedchat") {
        bail!("{} links don't show as pictures on Sneedchat - choose another image host for it", host.id());
    }
    let link = crate::upload::upload_reporting(host, file_path, None, progress).await?;
    let file_name = std::path::Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let posted = posted_markup(file_name, &link);
    let text = if caption.trim().is_empty() { posted } else { format!("{caption}\n{posted}") };
    send_message(state, account_id, buffer_name, &text, None).await
}

/// How a finished upload is written into a message.
pub(super) fn posted_markup(file_name: &str, link: &str) -> String {
    if crate::upload::is_web_picture(file_name) {
        format!("[img]{link}[/img]")
    } else {
        link.to_string()
    }
}

/// Changes what a message says (`revise`).
pub async fn edit_message(state: &AppState, account_id: &str, _buffer_name: &str, msg_id: &str, body: &str) -> Result<()> {
    let text = body.trim();
    if text.is_empty() {
        bail!("there's nothing to change it to");
    }
    // An edit keeps the reply it was: the quote stays at its head.
    let stored = state.store.get_message(&crate::model::buffer_id(account_id, _buffer_name), msg_id).ok().flatten();
    let reply = stored.and_then(|m| m.reply_to).map(|r| ReplyTo { from: r.from, id: r.id, body: r.body });
    let text = protocol::names_to_mentions(&compose(text, reply.as_ref()), &super::chat::chat(account_id).known_names());
    operate(account_id, msg_id, "revise", Some(&text)).await
}

/// Takes a message down (`tombstone`).
pub async fn delete_message(_state: &AppState, account_id: &str, _buffer_name: &str, msg_id: &str) -> Result<()> {
    operate(account_id, msg_id, "tombstone", None).await
}

/// A change to a message: it names the state the message is in, which the
/// chat checks so that two changes cannot cross.
async fn operate(account_id: &str, msg_id: &str, action: &str, body: Option<&str>) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let known = match chat.known(msg_id) {
        Some(k) => k.state_seq,
        None => message_state(&chat, msg_id).await?,
    };
    let op = protocol::operation_id();
    let mut frame = json!({
        "t": action,
        "chat_message_uuid": msg_id.to_lowercase(),
        "expected_message_state_seq": known,
        "reason": null,
        "notify_author": false,
        "chat_operation_uuid": op,
    });
    if let Some(b) = body {
        frame["body"] = json!(b);
    }
    chat.command(&op, frame).await
}

/// A message's current state, asked of the chat - for one this connection
/// has not seen.
async fn message_state(chat: &super::chat::Chat, msg_id: &str) -> Result<u64> {
    let session = chat.session.lock().unwrap().clone().ok_or_else(|| anyhow!("not connected to the chat right now"))?;
    let host = chat.host.lock().unwrap().clone();
    let v = super::chat::api_get(&session, &host, &format!("messages/{}", msg_id.to_lowercase())).await?;
    v["message_state_seq"]
        .as_u64()
        .or_else(|| v["message"]["message_state_seq"].as_u64())
        .ok_or_else(|| anyhow!("the chat didn't say what state that message is in"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture has to be wrapped for the site to render it; a video must
    /// not be, because [img] around an .mp4 draws a broken image instead of
    /// something you can click.
    #[test]
    fn only_pictures_are_wrapped_for_display() {
        assert_eq!(posted_markup("cat.png", "https://files.catbox.moe/a.png"), "[img]https://files.catbox.moe/a.png[/img]");
        assert_eq!(posted_markup("cat.JPEG", "https://x/a.jpeg"), "[img]https://x/a.jpeg[/img]");
        assert_eq!(posted_markup("clip.mp4", "https://files.catbox.moe/b.mp4"), "https://files.catbox.moe/b.mp4");
        assert_eq!(posted_markup("notes.pdf", "https://x/c.pdf"), "https://x/c.pdf");
        // No extension at all is not a picture.
        assert_eq!(posted_markup("README", "https://x/d"), "https://x/d");
    }

    #[test]
    fn a_reply_opens_with_the_quote_the_site_writes() {
        let reply = ReplyTo { from: "Ryo Ma".into(), id: "01A1".into(), body: "what's up".into() };
        assert_eq!(compose("not much", Some(&reply)), "[quote=\"Ryo Ma, chat_message: 01a1\"]what's up[/quote]\nnot much");
        assert_eq!(compose("  plain  ", None), "plain");
        // Nothing of their own to quote ("Mention" on a name): they are named at the front instead.
        let empty = ReplyTo { from: "Ryo".into(), id: "x".into(), body: "".into() };
        assert_eq!(compose("hi", Some(&empty)), "@Ryo, hi");
        let mention = ReplyTo { from: "Ryo Ma".into(), id: "".into(), body: "".into() };
        assert_eq!(compose(" hello ", Some(&mention)), "@Ryo Ma, hello");
        // Already addressed by hand: not named twice. A longer name is not them.
        assert_eq!(compose("@ryo ma: hello", Some(&mention)), "@ryo ma: hello");
        assert_eq!(compose("@Ryo Max hi", Some(&mention)), "@Ryo Ma, @Ryo Max hi");
        let nobody = ReplyTo { from: " ".into(), id: "".into(), body: "".into() };
        assert_eq!(compose("hi", Some(&nobody)), "hi");
    }
}
