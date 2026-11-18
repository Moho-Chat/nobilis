//! Pictures, video, voice notes and files: named in the cache when a message carries them, and fetched when the window
//! looks.
//!
//! Signal puts an attachment's bytes on its CDN, encrypted, and the message carries a pointer: where, the key, the
//! digest. Nothing is fetched until it is looked at. A message is recorded with attachments that name files in the
//! `signal-media` cache - files that need not exist yet - and when the window asks for one that is not there, the
//! daemon's "restore a cached file" route comes here, finds the pointer again in the account's store (the keys never
//! leave it), downloads, decrypts, checks the digest and writes the file.

use super::ids;
use super::{Command, Handle};
use crate::model::Attachment;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use presage::{
    libsignal_service::content::{Content, ContentBody},
    proto::{sync_message, AttachmentPointer, DataMessage, SyncMessage},
    store::{ContentExt, ContentsStore, Thread},
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

pub const MEDIA: &str = "signal-media";
pub const AVATARS: &str = "signal-avatars";

/// `AttachmentPointer.flags` bits.
const VOICE_MESSAGE: u32 = 1;

/// How long a download may take before it is called lost.
const PATIENCE: Duration = Duration::from_secs(300);

/// A stable short name (FNV-1a), so the same attachment is the same cache entry on every run.
fn hash(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn extension_of(name: Option<&str>, content_type: Option<&str>) -> String {
    if let Some(ext) = name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).filter(|e| !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric())) {
        return ext;
    }
    match content_type.unwrap_or("") {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/heic" => "heic",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        "audio/aac" | "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        _ => "bin",
    }
    .to_string()
}

fn kind_of(content_type: &str) -> &'static str {
    match content_type.split('/').next().unwrap_or("") {
        "image" => "image",
        "video" => "video",
        "audio" => "audio",
        _ => "file",
    }
}

/// Where an attachment is (or will be) in the cache, as the `file://` address the window loads.
fn cache_path(account_id: &str, thread: &str, ts: u64, author: &Uuid, index: usize, name: Option<&str>, content_type: Option<&str>) -> PathBuf {
    let key = format!("{account_id}|{thread}|{ts}|{author}|{index}");
    crate::media_cache::dir(MEDIA).join(format!("{}.{}", hash(&key), extension_of(name, content_type)))
}

/// The attachments of one message, as the window draws them. Each is recorded against the place it can be found again.
pub fn attachments_of(account_id: &str, thread: &Thread, author: &Uuid, ts: u64, pointers: &[AttachmentPointer]) -> Vec<Attachment> {
    let remote = ids::remote_id(thread);
    pointers
        .iter()
        .enumerate()
        .map(|(index, pointer)| {
            let content_type = pointer.content_type.clone().unwrap_or_default();
            let path = cache_path(account_id, &remote, ts, author, index, pointer.file_name.as_deref(), Some(&content_type));
            crate::media_cache::record(&path, json!({ "account": account_id, "thread": remote, "ts": ts, "author": author.to_string(), "index": index }));
            Attachment {
                kind: kind_of(&content_type).to_string(),
                mimetype: Some(content_type).filter(|t| !t.is_empty()),
                filename: pointer.file_name.clone().filter(|n| !n.is_empty()),
                size: pointer.size.map(u64::from),
                width: pointer.width.filter(|w| *w > 0),
                height: pointer.height.filter(|h| *h > 0),
                blurhash: pointer.blur_hash.clone().filter(|b| !b.is_empty()),
                duration_secs: None,
                waveform: None,
                voice: pointer.flags.unwrap_or(0) & VOICE_MESSAGE != 0,
                path: Some(format!("file://{}", path.display())),
                thumbnail_path: None,
                url: None,
            }
        })
        .collect()
}

/// Where a person's picture is (or will be) in the cache.
pub fn avatar_path(account_id: &str, who: &Uuid) -> PathBuf {
    crate::media_cache::dir(AVATARS).join(format!("{}.jpg", hash(&format!("{account_id}|{who}"))))
}

/// Where a group's picture is (or will be) in the cache, by the CDN key the group names it by: a new picture is a new
/// file, so one changed is fetched again rather than an old copy shown.
pub fn group_avatar_path(account_id: &str, master_key: &[u8; 32], cdn_key: &str) -> PathBuf {
    crate::media_cache::dir(AVATARS).join(format!("{}.jpg", hash(&format!("{account_id}|g:{}|{cdn_key}", super::ids::hex(master_key)))))
}

/// The address of a person's picture if it has been fetched.
pub fn avatar_of(account_id: &str, who: &Uuid) -> Option<String> {
    let path = avatar_path(account_id, who);
    path.exists().then(|| format!("file://{}", path.display()))
}

/// Keeps a person's picture, whole and renamed into place.
pub async fn keep_avatar(account_id: &str, who: &Uuid, bytes: &[u8]) -> Result<()> {
    let path = avatar_path(account_id, who);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = path.with_extension("part");
    tokio::fs::write(&part, bytes).await?;
    tokio::fs::rename(&part, &path).await.with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The data message a stored content carries, whichever way it came: from somebody, from the phone, or as an edit.
pub fn data_message_of(content: &Content) -> Option<&DataMessage> {
    match &content.body {
        ContentBody::DataMessage(dm) => Some(dm),
        ContentBody::EditMessage(edit) => edit.data_message.as_ref(),
        ContentBody::SynchronizeMessage(SyncMessage { content: Some(sync_message::Content::Sent(sent)), .. }) => sent.message.as_ref().or_else(|| sent.edit_message.as_ref().and_then(|e| e.data_message.as_ref())),
        _ => None,
    }
}

/// Every pointer a message carries, in a fixed order: its attachments, a sticker's picture, then the pictures of its link
/// previews. A cached file is found again by its place in this list.
/// The content type of the file that carries a long message's whole text.
pub const LONG_TEXT: &str = "text/x-signal-plain";

pub fn pointers_of(dm: &DataMessage) -> Vec<AttachmentPointer> {
    let mut all: Vec<AttachmentPointer> = dm.attachments.clone();
    if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.clone()) {
        all.push(data);
    }
    all.extend(dm.preview.iter().filter_map(|p| p.image.clone()));
    all
}

/// How many of a message's pointers are things to show as attachments (the rest are pictures on link cards).
pub fn shown_count(dm: &DataMessage) -> usize {
    dm.attachments.len() + usize::from(dm.sticker.as_ref().is_some_and(|s| s.data.is_some()))
}

/// The cards for the links a message carries: what Signal's sender fetched about the page, with its picture named in the
/// cache like any other attachment.
pub fn previews_of(account_id: &str, thread: &Thread, author: &Uuid, ts: u64, dm: &DataMessage) -> Vec<crate::model::Embed> {
    let remote = ids::remote_id(thread);
    let mut index = shown_count(dm);
    dm.preview
        .iter()
        .filter(|p| p.url.as_deref().is_some_and(|u| !u.is_empty()))
        .map(|p| {
            let image = p.image.as_ref().map(|pointer| {
                let content_type = pointer.content_type.clone().unwrap_or_default();
                let path = cache_path(account_id, &remote, ts, author, index, None, Some(&content_type));
                crate::media_cache::record(&path, json!({ "account": account_id, "thread": remote, "ts": ts, "author": author.to_string(), "index": index }));
                index += 1;
                format!("file://{}", path.display())
            });
            crate::model::Embed {
                title: p.title.clone().filter(|t| !t.is_empty()),
                description: p.description.clone().filter(|t| !t.is_empty()),
                url: p.url.clone(),
                image_url: image,
                ..Default::default()
            }
        })
        .collect()
}

/// Finds the stored message sent at `ts` in `thread`, whichever time the library filed it under.
pub async fn stored_content(store: &super::store::SignalStore, thread: &Thread, ts: u64) -> Option<Content> {
    if let Ok(Some(found)) = store.message(thread, ts).await {
        return Some(found);
    }
    let later = store.messages(thread, ts..).await.ok()?;
    later.filter_map(Result::ok).take(500).find(|c| c.timestamp() == ts)
}

/// The pointer for attachment `index` of the message.
pub async fn pointer_for(store: &super::store::SignalStore, thread: &Thread, ts: u64, index: usize) -> Option<AttachmentPointer> {
    let content = stored_content(store, thread, ts).await?;
    pointers_of(data_message_of(&content)?).into_iter().nth(index)
}

/// Fetches a cached file again.
pub async fn restore_cached(state: &AppState, path: &Path) -> Result<PathBuf> {
    let source = crate::media_cache::source(path).ok_or_else(|| anyhow!("nothing says where this file came from"))?;
    let account = source["account"].as_str().context("no account was recorded for this file")?;
    let thread = source["thread"].as_str().and_then(ids::parse_remote_id).context("no conversation was recorded for this file")?;
    let ts = source["ts"].as_u64().context("no time was recorded for this file")?;
    let index = source["index"].as_u64().unwrap_or(0) as usize;
    let author = source["author"].as_str().and_then(|a| Uuid::parse_str(a).ok());
    let handle = state.runtime.signal_handle(account).ok_or_else(|| anyhow!("that Signal account is not connected"))?;
    let bytes = fetch(&handle, thread, ts, index, author).await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = path.with_extension("part");
    tokio::fs::write(&part, &bytes).await?;
    tokio::fs::rename(&part, path).await.with_context(|| format!("writing {}", path.display()))?;
    Ok(path.to_path_buf())
}

async fn fetch(handle: &Handle, thread: Thread, ts: u64, index: usize, author: Option<Uuid>) -> Result<Vec<u8>> {
    // A story's picture is kept by moho, the library keeping no stories; a file from transferred history too.
    let mut plaintext_hash = None;
    let pointer = match pointer_for(&handle.store, &thread, ts, index).await {
        Some(pointer) => Some(pointer),
        None => match author.and_then(|a| super::stories::pointer(&handle.store, ts, &a)) {
            Some(pointer) => Some(pointer),
            None => author.and_then(|a| super::history::file(&handle.store, &thread, ts, &a, index)).map(|(pointer, hash)| {
                plaintext_hash = hash;
                pointer
            }),
        },
    };
    let pointer = pointer.ok_or_else(|| anyhow!("the message that carried this file is no longer kept"))?;
    let (done, answer) = oneshot::channel();
    handle.commands.send(Command::Fetch { pointer: Box::new(pointer), plaintext_hash, done }).map_err(|_| anyhow!("that Signal account is not connected"))?;
    match tokio::time::timeout(PATIENCE, answer).await {
        Ok(Ok(Ok(bytes))) => Ok(bytes),
        Ok(Ok(Err(e))) => bail!("{e}"),
        Ok(Err(_)) => bail!("the connection ended before the file arrived"),
        Err(_) => bail!("Signal did not send the file in time"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(kind: &str, flags: u32, name: Option<&str>) -> AttachmentPointer {
        AttachmentPointer { content_type: Some(kind.to_string()), flags: Some(flags), file_name: name.map(String::from), size: Some(10), width: Some(3), height: Some(4), blur_hash: Some("LKO2".into()), ..Default::default() }
    }

    #[test]
    fn an_attachment_is_drawn_as_what_it_is_with_a_place_to_be_fetched_to() {
        let author = Uuid::nil();
        let thread = Thread::Group([1u8; 32]);
        let list = attachments_of("signal:x", &thread, &author, 5, &[pointer("image/jpeg", 0, None), pointer("audio/aac", VOICE_MESSAGE, None), pointer("application/pdf", 0, Some("a.pdf"))]);
        assert_eq!(list.iter().map(|a| a.kind.as_str()).collect::<Vec<_>>(), vec!["image", "audio", "file"]);
        assert!(list[1].voice && !list[0].voice);
        assert_eq!((list[0].width, list[0].height, list[0].size, list[0].blurhash.as_deref()), (Some(3), Some(4), Some(10), Some("LKO2")));
        assert_eq!(list[2].filename.as_deref(), Some("a.pdf"));
        let paths: Vec<_> = list.iter().map(|a| a.path.clone().unwrap()).collect();
        assert!(paths[0].ends_with(".jpg") && paths[2].ends_with(".pdf"), "{paths:?}");
        assert!(paths[0].contains("/signal-media/"), "{paths:?}");
        assert_eq!(paths.iter().collect::<std::collections::HashSet<_>>().len(), 3, "each has a name of its own");
        // The same attachment is the same file on the next run.
        assert_eq!(attachments_of("signal:x", &thread, &author, 5, &[pointer("image/jpeg", 0, None)])[0].path, list[0].path);
    }

    #[test]
    fn a_link_card_has_its_words_and_a_picture_fetched_like_any_other() {
        let author = Uuid::nil();
        let thread = Thread::Group([2u8; 32]);
        let dm = DataMessage {
            attachments: vec![pointer("image/png", 0, None)],
            preview: vec![
                presage::proto::Preview { url: Some("https://example.org/a".into()), title: Some("A page".into()), description: Some("About it".into()), image: Some(pointer("image/jpeg", 0, None)), date: None },
                presage::proto::Preview { url: Some("https://example.org/b".into()), title: Some(String::new()), description: None, image: None, date: None },
                presage::proto::Preview { url: None, title: Some("No link".into()), description: None, image: None, date: None },
            ],
            ..Default::default()
        };
        // The attachment is first, so the first card's picture is the second pointer.
        assert_eq!(shown_count(&dm), 1);
        assert_eq!(pointers_of(&dm).len(), 2);
        let cards = previews_of("signal:x", &thread, &author, 9, &dm);
        assert_eq!(cards.len(), 2, "a preview with no address is not a card");
        assert_eq!((cards[0].title.as_deref(), cards[0].description.as_deref(), cards[0].url.as_deref()), (Some("A page"), Some("About it"), Some("https://example.org/a")));
        assert!(cards[0].image_url.as_deref().is_some_and(|p| p.contains("/signal-media/") && p.ends_with(".jpg")));
        assert_eq!((cards[1].title.as_deref(), cards[1].image_url.as_deref()), (None, None));
    }

    #[test]
    fn a_stickers_picture_is_counted_after_the_attachments() {
        let dm = DataMessage {
            attachments: vec![pointer("image/png", 0, None)],
            sticker: Some(presage::proto::data_message::Sticker { data: Some(pointer("image/webp", 0, None)), ..Default::default() }),
            ..Default::default()
        };
        let all = pointers_of(&dm);
        assert_eq!(all.iter().map(|p| p.content_type.as_deref().unwrap()).collect::<Vec<_>>(), vec!["image/png", "image/webp"]);
    }

    #[test]
    fn an_extension_comes_from_the_name_or_the_kind_of_file() {
        assert_eq!(extension_of(Some("cat.JPG"), None), "jpg");
        assert_eq!(extension_of(None, Some("image/webp")), "webp");
        assert_eq!(extension_of(Some("noext"), Some("video/mp4")), "mp4");
        assert_eq!(extension_of(None, None), "bin");
        assert_eq!(extension_of(Some("a.way-too-long-extension"), Some("image/png")), "png");
    }
}
