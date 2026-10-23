//! Saying things in a conversation: send, answer, edit, delete, "typing", "read".
//!
//! Each starts from the buffer: its `remote_id` names the peer, and the session turns that into the reference
//! Telegram wants with it (an id with the authority to use it). A message that was sent is recorded here as it is
//! returned, because Telegram does not send an account its own message back as an update.

use super::markup;
use super::messages;
use super::peers;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use crate::upload::{Phase, Progress};
use grammers_client::media::{InputMedia, Uploaded};
use grammers_client::message::InputMessage;
use grammers_client::{tl, InvocationError};
use grammers_session::types::PeerRef;
use grammers_session::Session;

/// What Telegram allows in one message, counted the way it counts (UTF-16 units).
pub const MAX_UNITS: usize = 4096;

/// Telegram's refusal, said so a person can act on it.
pub fn explain(error: &InvocationError) -> String {
    match error {
        InvocationError::Rpc(rpc) => {
            let n = rpc.value.unwrap_or(0);
            match rpc.name.as_str() {
                "FLOOD_WAIT" | "FLOOD_PREMIUM_WAIT" => format!("Telegram says to wait {n} seconds before doing that again"),
                "SLOWMODE_WAIT" => format!("Slow mode is on here: wait {n} seconds between messages"),
                "CHAT_WRITE_FORBIDDEN" | "CHAT_SEND_PLAIN_FORBIDDEN" => peers::NO_SEND_PERMISSION.to_string(),
                "USER_BANNED_IN_CHANNEL" => "You are banned from sending messages in this chat".to_string(),
                "USER_IS_BLOCKED" => "This person has blocked you".to_string(),
                "USER_PRIVACY_RESTRICTED" => "This person's privacy settings do not let you message them".to_string(),
                "PEER_FLOOD" => "Telegram is limiting how many people this account may start a conversation with".to_string(),
                "MESSAGE_TOO_LONG" => format!("That message is too long: Telegram takes at most {MAX_UNITS} characters"),
                "MESSAGE_EMPTY" => "There is nothing to send".to_string(),
                "MESSAGE_NOT_MODIFIED" => "That is already what the message says".to_string(),
                "MESSAGE_EDIT_TIME_EXPIRED" => "Telegram no longer lets this message be edited".to_string(),
                "MESSAGE_ID_INVALID" | "MESSAGE_NOT_FOUND" => "That message is no longer there".to_string(),
                "CHAT_ADMIN_REQUIRED" => "Only an administrator may do that here".to_string(),
                "MEDIA_EMPTY" | "FILE_PART_INVALID" => "Telegram would not take that file".to_string(),
                "FILE_REFERENCE_EXPIRED" => "Telegram needs the message read again before that file can be downloaded".to_string(),
                other => format!("Telegram refused: {}", other.to_lowercase().replace('_', " ")),
            }
        }
        InvocationError::Io(e) => format!("Telegram could not be reached: {e}"),
        other => format!("{other}"),
    }
}

/// The account's connection and Telegram's reference to the conversation a buffer is.
pub async fn peer_for(state: &AppState, buffer: &Buffer) -> Result<(Handle, PeerRef), String> {
    let handle = state.runtime.telegram_handle(&buffer.account_id).ok_or_else(|| "that Telegram account is not connected".to_string())?;
    let id = buffer.remote_id.as_deref().and_then(peers::peer_id_of).ok_or_else(|| "this is not a Telegram conversation".to_string())?;
    let peer = handle.session.peer_ref(id).await.ok().flatten().ok_or_else(|| "Telegram has not told this account about that conversation yet".to_string())?;
    Ok((handle, peer))
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Text from the message box, marks taken out and entities in place, if Telegram would take it.
pub fn checked(body: &str) -> Result<(String, Vec<tl::enums::MessageEntity>), String> {
    let (text, entities) = markup::parse(body);
    if text.trim().is_empty() {
        return Err("There is nothing to send".to_string());
    }
    if units(&text) > MAX_UNITS {
        return Err(format!("That message is too long: Telegram takes at most {MAX_UNITS} characters, and this has {}", units(&text)));
    }
    Ok((text, entities))
}

/// The message box's text as a message.
pub fn input_for(body: &str) -> Result<InputMessage, String> {
    let (text, entities) = checked(body)?;
    Ok(InputMessage::new().text(text).fmt_entities(entities))
}

/// Sends a message, as an answer to another if `reply_to` names one.
pub async fn send(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    if let Some(topic) = super::topics::topic_of_buffer(buffer) {
        return send_in_topic(state, buffer, body, reply_to, topic).await;
    }
    let input = input_for(body)?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let input = input.reply_to(reply_to.and_then(|id| id.parse::<i32>().ok()));
    let sent = handle.client.send_message(peer, input).await.map_err(|e| explain(&e))?;
    messages::record(state, &buffer.account_id, &handle.own, &sent);
    Ok(())
}

/// Sends a message into a forum topic. An answer to a message in a topic has to name the topic as well, which the
/// library's own send cannot say, so the request is made directly and the message it made is read back by its id.
async fn send_in_topic(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>, topic: i32) -> Result<(), String> {
    let (text, entities) = checked(body)?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let reply = reply_to.and_then(|id| id.parse::<i32>().ok()).unwrap_or(topic);
    let random_id: i64 = rand::random();
    let updates = handle
        .client
        .invoke(&tl::functions::messages::SendMessage {
            no_webpage: false,
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: peer.into(),
            reply_to: Some(
                tl::types::InputReplyToMessage { reply_to_msg_id: reply, top_msg_id: Some(topic), reply_to_peer_id: None, quote_text: None, quote_entities: None, quote_offset: None, monoforum_peer_id: None, todo_item_id: None, poll_option: None }
                    .into(),
            ),
            message: text,
            random_id,
            reply_markup: None,
            entities: Some(entities).filter(|e| !e.is_empty()),
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_stars: None,
            suggested_post: None,
            rich_message: None,
        })
        .await
        .map_err(|e| explain(&e))?;
    if let Some(id) = sent_id(&updates, random_id) {
        if let Ok(found) = handle.client.get_messages_by_id(peer, &[id]).await {
            if let Some(Some(sent)) = found.into_iter().next() {
                messages::record(state, &buffer.account_id, &handle.own, &sent);
            }
        }
    }
    Ok(())
}

/// The id Telegram gave a message that was sent with `random_id`, from what it answered.
fn sent_id(updates: &tl::enums::Updates, random_id: i64) -> Option<i32> {
    use tl::enums::Updates as U;
    let list: &[tl::enums::Update] = match updates {
        U::Updates(u) => &u.updates,
        U::Combined(u) => &u.updates,
        U::UpdateShort(u) => std::slice::from_ref(&u.update),
        U::UpdateShortSentMessage(u) => return Some(u.id),
        _ => return None,
    };
    list.iter().find_map(|u| match u {
        tl::enums::Update::MessageId(m) if m.random_id == random_id => Some(m.id),
        _ => None,
    })
}

/// Changes what one of the account's own messages says.
pub async fn edit(state: &AppState, buffer: &Buffer, msg_id: &str, body: &str) -> Result<(), String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot be edited".to_string())?;
    let input = input_for(body)?;
    let (handle, peer) = peer_for(state, buffer).await?;
    handle.client.edit_message(peer, id, input).await.map_err(|e| explain(&e))?;
    let (text, _) = markup::parse(body);
    state.runtime.update_message(state, &buffer.id, msg_id, &text, &[], &[]);
    Ok(())
}

/// Removes a message: for everyone where Telegram allows it, or only from this account's own view.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str, for_everyone: bool) -> Result<(), String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot be deleted".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    if for_everyone || peer.id.kind() == grammers_session::types::PeerKind::Channel {
        handle.client.delete_messages(peer, &[id]).await.map_err(|e| explain(&e))?;
    } else {
        handle.client.invoke(&tl::functions::messages::DeleteMessages { revoke: false, id: vec![id] }).await.map_err(|e| explain(&e))?;
    }
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(())
}

/// Says the account is typing. Telegram lets it lapse after a few seconds, so there is no "stopped".
pub async fn typing(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    handle.client.action(peer).oneshot(tl::enums::SendMessageAction::SendMessageTypingAction).await.map_err(|e| explain(&e))
}

/// Says everything in the conversation has been read, so its unread count goes down on the account's other apps.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    // A topic is read on its own: to the newest message of it that is here.
    if let Some(topic) = super::topics::topic_of_buffer(buffer) {
        // Or to the newest Telegram said it has, which may be a message this client never kept.
        let newest = state.store.newest_message_id(&buffer.id).ok().flatten().and_then(|id| id.parse::<i32>().ok()).unwrap_or(0).max(super::topics::top_of(&buffer.id).unwrap_or(0));
        if newest > 0 {
            handle.client.invoke(&tl::functions::messages::ReadDiscussion { peer: peer.into(), msg_id: topic, read_max_id: newest }).await.map_err(|e| explain(&e))?;
        }
        state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
        return Ok(());
    }
    handle.client.mark_as_read(peer).await.map_err(|e| explain(&e))
}


/// A reader that says how much of it has been read, so an upload can show a ring that fills.
struct Counting<'a, R> {
    inner: R,
    done: u64,
    said: u64,
    base: u64,
    total: u64,
    report: &'a (dyn Fn(u64, u64) + Send + Sync),
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Counting<'_, R> {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            self.done += (buf.filled().len() - before) as u64;
            if self.done - self.said >= 128 * 1024 {
                self.said = self.done;
                (self.report)(self.base + self.done, self.total);
            }
        }
        polled
    }
}

/// What a file is, for choosing how Telegram is told about it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sort {
    Photo,
    Video,
    Other,
}

pub fn sort_of(name: &str) -> Sort {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("jpg" | "jpeg" | "png" | "webp" | "bmp") => Sort::Photo,
        Some("mp4" | "mov" | "webm" | "mkv" | "m4v") => Sort::Video,
        _ => Sort::Other,
    }
}

async fn upload(client: &grammers_client::Client, path: &str, base: u64, total: u64, report: &(dyn Fn(u64, u64) + Send + Sync)) -> Result<(Uploaded, String), String> {
    let file = tokio::fs::File::open(path).await.map_err(|e| format!("could not read {path}: {e}"))?;
    let size = file.metadata().await.map_err(|e| e.to_string())?.len();
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".to_string());
    let mut reader = Counting { inner: file, done: 0, said: 0, base, total, report };
    let up = client.upload_stream(&mut reader, size as usize, name.clone()).await.map_err(|e| format!("Telegram would not take {name}: {e}"))?;
    Ok((up, name))
}

/// A picture or a video as media. Anything else is sent as a plain file (see `send_files`).
fn media_for(up: Uploaded, sort: Sort, spoiler: bool) -> Option<tl::enums::InputMedia> {
    match sort {
        Sort::Photo => Some(tl::types::InputMediaUploadedPhoto { spoiler, file: up.raw, stickers: None, ttl_seconds: None, live_photo: false, video: None }.into()),
        Sort::Video => Some(grammers_client::media::InputVideo::new(up).supports_streaming(true).spoiler(spoiler).into()),
        Sort::Other => None,
    }
}

/// Sends files, with the caption on the first: one message each, or an album where there are several pictures and
/// videos. `progress` names the ring the window draws.
pub async fn send_files(state: &AppState, buffer: &Buffer, caption: &str, files: &[(String, bool)], reply_to: Option<&str>, progress: Option<&Progress>) -> Result<(), String> {
    if files.is_empty() {
        return send(state, buffer, caption, reply_to).await;
    }
    let (text, entities) = if caption.trim().is_empty() { (String::new(), Vec::new()) } else { checked(caption)? };
    // Telegram's limit on a caption is much smaller than on a message.
    if units(&text) > 1024 {
        return Err(format!("A caption can be at most 1024 characters, and this has {}", units(&text)));
    }
    let (handle, peer) = peer_for(state, buffer).await?;
    // In a topic, a message with nothing to answer answers the topic's first message, which is how it is sent there.
    let reply = reply_to.and_then(|id| id.parse::<i32>().ok()).or_else(|| super::topics::topic_of_buffer(buffer));
    let total: u64 = files.iter().filter_map(|(p, _)| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    if let Some(p) = progress {
        p.at(Phase::Preparing, total as usize, "Telegram");
    }
    let report = |sent: u64, all: u64| {
        if let Some(p) = progress {
            p.sent(sent, all, "Telegram");
        }
    };
    let mut uploaded: Vec<(Uploaded, Sort, bool)> = Vec::new();
    let mut base = 0u64;
    for (path, spoiler) in files {
        let (up, name) = upload(&handle.client, path, base, total, &report).await?;
        base += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        uploaded.push((up, sort_of(&name), *spoiler));
    }
    if let Some(p) = progress {
        p.at(Phase::Waiting, total as usize, "Telegram");
    }
    let album = uploaded.len() >= 2 && uploaded.len() <= 10 && uploaded.iter().all(|(_, sort, _)| *sort != Sort::Other);
    if album {
        let medias: Vec<InputMedia> = uploaded
            .into_iter()
            .enumerate()
            .map(|(i, (up, sort, spoiler))| {
                let mut m = InputMedia::new().media(media_for(up, sort, spoiler).expect("an album is only pictures and videos"));
                if i == 0 {
                    m = m.caption(text.clone()).fmt_entities(entities.clone()).reply_to(reply);
                }
                m
            })
            .collect();
        for sent in handle.client.send_album(peer, medias).await.map_err(|e| explain(&e))?.into_iter().flatten() {
            messages::record(state, &buffer.account_id, &handle.own, &sent);
        }
        return Ok(());
    }
    for (i, (up, sort, spoiler)) in uploaded.into_iter().enumerate() {
        let mut input = match media_for(up.clone(), sort, spoiler) {
            Some(media) => InputMessage::new().media(media),
            None => InputMessage::new().file(up),
        }
        .reply_to(reply);
        if i == 0 {
            input = input.text(text.clone()).fmt_entities(entities.clone());
        }
        let sent = handle.client.send_message(peer, input).await.map_err(|e| explain(&e))?;
        messages::record(state, &buffer.account_id, &handle.own, &sent);
    }
    Ok(())
}

/// Sends a recording as a voice message (an Ogg file of Opus, which is what the recorder writes).
pub async fn send_voice(state: &AppState, buffer: &Buffer, path: &std::path::Path, duration_secs: f64) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let none = |_: u64, _: u64| {};
    let (up, _) = upload(&handle.client, &path.to_string_lossy(), 0, 0, &none).await?;
    let voice = grammers_client::media::InputAudio::new(up).voice(true).duration(std::time::Duration::from_secs_f64(duration_secs.max(0.0))).mime_type("audio/ogg");
    let sent = handle.client.send_message(peer, InputMessage::new().media(voice)).await.map_err(|e| explain(&e))?;
    messages::record(state, &buffer.account_id, &handle.own, &sent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::RpcError;

    fn rpc(code: i32, name: &str, value: Option<u32>) -> InvocationError {
        InvocationError::Rpc(RpcError { code, name: name.to_string(), value, caused_by: None })
    }

    #[test]
    fn telegrams_refusals_are_said_in_words_with_their_numbers() {
        assert_eq!(explain(&rpc(420, "FLOOD_WAIT", Some(30))), "Telegram says to wait 30 seconds before doing that again");
        assert_eq!(explain(&rpc(420, "SLOWMODE_WAIT", Some(10))), "Slow mode is on here: wait 10 seconds between messages");
        assert_eq!(explain(&rpc(403, "CHAT_WRITE_FORBIDDEN", None)), peers::NO_SEND_PERMISSION);
        assert!(explain(&rpc(400, "MESSAGE_TOO_LONG", None)).contains("4096"));
        assert_eq!(explain(&rpc(400, "SOMETHING_NEW_HAPPENED", None)), "Telegram refused: something new happened");
    }

    #[test]
    fn a_file_is_sent_as_a_picture_a_video_or_a_file_by_its_extension() {
        assert_eq!(sort_of("Holiday.JPG"), Sort::Photo);
        assert_eq!(sort_of("clip.mp4"), Sort::Video);
        assert_eq!(sort_of("report.pdf"), Sort::Other);
        assert_eq!(sort_of("noextension"), Sort::Other);
        // A GIF is sent as a file so Telegram keeps it moving.
        assert_eq!(sort_of("cat.gif"), Sort::Other);
    }

    #[test]
    fn what_cannot_be_sent_is_stopped_before_anything_is_sent() {
        assert!(checked("   ").unwrap_err().contains("nothing to send"));
        let long = "x".repeat(MAX_UNITS + 1);
        assert!(checked(&long).unwrap_err().contains("too long"));
        assert!(checked(&"x".repeat(MAX_UNITS)).is_ok());
        // An emoji is two units, and Telegram counts units.
        assert!(checked(&"😀".repeat(MAX_UNITS / 2 + 1)).unwrap_err().contains("too long"));
        // Formatting marks do not count against the limit: they are taken out.
        let (text, entities) = checked("**hello**").unwrap();
        assert_eq!((text.as_str(), entities.len()), ("hello", 1));
    }
}
