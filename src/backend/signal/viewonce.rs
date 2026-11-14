//! View-once pictures and videos, which each person may open one time.
//!
//! The message is drawn with a button and nothing else. Pressing it fetches the picture into a file of its own and hands
//! that to the window, which shows it and has the file taken away when it is closed; at the same moment the message
//! forgets the picture, the account's other devices are told it was opened (so the phone shows it as opened too), and the
//! sender is told it was viewed if the phone's read-receipt setting is on. Opened on the phone first, it is forgotten
//! here the same way. The copy that comes from the phone's own sends never carries the picture, and is only named.

use super::people::Ctx;
use super::{ids, Command, Handle};
use crate::model::Component;
use crate::state::AppState;
use presage::libsignal_service::prelude::ProtobufMessage;
use presage::proto::AttachmentPointer;
use std::path::PathBuf;
use uuid::Uuid;

/// The id of the button on a view-once message.
pub const BUTTON: &str = "signal:viewonce";

/// What a view-once message says before it is opened, and after.
pub fn waiting_words(pointer: &AttachmentPointer) -> &'static str {
    if pointer.content_type.as_deref().is_some_and(|c| c.starts_with("video/")) {
        "[View-once video]"
    } else {
        "[View-once photo]"
    }
}
pub const OPENED: &str = "[View-once media: opened]";

fn pointer_key(id: &str) -> String {
    format!("viewonce:{id}")
}

fn buffer_key(id: &str) -> String {
    format!("viewonce-buffer:{id}")
}

/// Where opened pictures are put while they are looked at: under the cache the window may load from.
fn folder() -> PathBuf {
    crate::media_cache::dir(super::media::MEDIA).join("viewonce")
}

fn button() -> Component {
    Component {
        kind: "button".to_string(),
        custom_id: Some(BUTTON.to_string()),
        label: Some("View once".to_string()),
        style: Some("primary".to_string()),
        url: None,
        disabled: false,
        emoji: None,
        options: Vec::new(),
        placeholder: None,
        row: 0,
    }
}

/// Keeps the picture of a view-once message (the message `id` in `buffer_id`) until it is opened, and puts the button on it.
pub fn hold(ctx: &Ctx<'_>, buffer_id: &str, id: &str, pointer: &AttachmentPointer) {
    let _ = ctx.store.set_blob(&pointer_key(id), &pointer.encode_to_vec());
    let _ = ctx.store.set_blob(&buffer_key(id), buffer_id.as_bytes());
    let components = vec![button()];
    if ctx.state.store.set_components(buffer_id, id, &components).is_ok() {
        ctx.state.events.emit("messageComponents", serde_json::json!({ "bufferId": buffer_id, "messageId": id, "components": components }));
    }
}

/// Forgets a view-once picture: its kept pointer, its button, and its words become "opened". Answers whether there was one.
fn forget(state: &AppState, store: &super::store::SignalStore, id: &str) -> bool {
    let Some(buffer_id) = store.blob(&buffer_key(id)).ok().flatten().and_then(|b| String::from_utf8(b).ok()) else { return false };
    let _ = store.set_blob(&pointer_key(id), &[]);
    let _ = store.set_blob(&buffer_key(id), &[]);
    if state.store.clear_components(&buffer_id, id).is_ok() {
        state.events.emit("messageComponents", serde_json::json!({ "bufferId": buffer_id, "messageId": id, "components": [] }));
    }
    state.runtime.update_message_body_only(state, &buffer_id, id, OPENED);
    true
}

/// Opened on another of the account's devices: forgotten here too.
pub fn opened_elsewhere(ctx: &Ctx<'_>, sender: Uuid, sent_ms: u64) {
    forget(ctx.state, &ctx.store, &ids::message_id(sent_ms, &sender));
}

/// Opens a view-once message: the file to show, which the window gives back through `close` when it is done with it.
pub async fn open(state: &AppState, handle: &Handle, msg_id: &str) -> Result<serde_json::Value, String> {
    let (sent_ms, sender) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    let bytes = handle.store.blob(&pointer_key(msg_id)).ok().flatten().filter(|b| !b.is_empty()).ok_or("That was opened already")?;
    let pointer = AttachmentPointer::decode(bytes.as_slice()).map_err(|_| "That picture's details were damaged".to_string())?;
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(Command::Fetch { pointer: Box::new(pointer.clone()), done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let picture = match tokio::time::timeout(std::time::Duration::from_secs(300), answer).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err("the connection ended before Signal gave the picture".to_string()),
        Err(_) => return Err("Signal did not give the picture in time".to_string()),
    };
    let content_type = pointer.content_type.clone().unwrap_or_else(|| "image/jpeg".to_string());
    let extension = match content_type.as_str() {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        _ if content_type.starts_with("video/") => "mp4",
        _ => "jpg",
    };
    let dir = folder();
    std::fs::create_dir_all(&dir).map_err(|e| format!("making a place for the picture: {e}"))?;
    let path = dir.join(format!("{}.{extension}", Uuid::new_v4().simple()));
    std::fs::write(&path, &picture).map_err(|e| format!("keeping the picture to show: {e}"))?;
    // Opened is opened, whether it is looked at for long or not: from here it is gone, as on the phone.
    forget(state, &handle.store, msg_id);
    let _ = handle.commands.send(Command::ViewOnceOpened { sender, sent_ms, receipt: handle.memory.synced().read_receipts == Some(true) });
    Ok(serde_json::json!({
        "path": format!("file://{}", path.display()),
        "kind": if content_type.starts_with("video/") { "video" } else { "image" },
        "mimetype": content_type,
        "width": pointer.width,
        "height": pointer.height,
    }))
}

/// Takes away the file an opened picture was shown from. Only files in the view-once folder are taken.
pub fn close(path: &str) -> Result<(), String> {
    let path = PathBuf::from(path.strip_prefix("file://").unwrap_or(path));
    let dir = folder();
    match (path.canonicalize(), dir.canonicalize()) {
        (Ok(file), Ok(dir)) if file.parent() == Some(dir.as_path()) => std::fs::remove_file(&file).map_err(|e| e.to_string()),
        (Err(_), _) => Ok(()),
        _ => Err("that is not an opened view-once picture".to_string()),
    }
}

/// Clears what was left in the view-once folder by a window that went away while one was open: anything over an hour
/// old, so a picture being looked at in another account's window is not taken from under it.
pub fn sweep() {
    let Ok(entries) = std::fs::read_dir(folder()) else { return };
    for entry in entries.flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 3600);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_words_say_a_photo_or_a_video() {
        let video = AttachmentPointer { content_type: Some("video/mp4".into()), ..Default::default() };
        assert_eq!(waiting_words(&video), "[View-once video]");
        assert_eq!(waiting_words(&AttachmentPointer::default()), "[View-once photo]");
    }

    #[test]
    fn only_a_file_in_the_view_once_folder_is_taken_away() {
        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(close(outside.path().to_str().unwrap()).is_err());
        assert!(outside.path().exists());
        assert!(close("/no/such/file.jpg").is_ok(), "already gone is fine");
    }
}
