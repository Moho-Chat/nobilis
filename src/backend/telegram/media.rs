//! Pictures, video, files, voice and stickers: what a message carries, and fetching it.
//!
//! Nothing is fetched until it is looked at. A message is recorded with attachments that name files in the daemon's
//! caches (`telegram-media`, `telegram-avatars`) - files that need not exist yet - and when the window asks for one
//! that is not there, the "restore a cached picture" route it already has for every other service asks the daemon,
//! which finds the file's message again (a file reference expires, so the one remembered would not do), downloads it
//! and answers. So opening a channel of fifty posts fetches the pictures on screen, not fifty videos.
//!
//! A cached file is named for what it is: `tg_<account>_<conversation>_<message>_<thumb|full>.<ext>` for a message's
//! media, `tg_<account>_<person>_<photo>.jpg` for a profile picture.

use super::peers;
use super::Handle;
use crate::model::{Attachment, Embed};
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use grammers_client::media::{Document, Downloadable, Media, PhotoSize};
use grammers_client::message::Message;
use grammers_client::tl;
use grammers_session::types::PeerId;
use grammers_session::Session;
use std::path::{Path, PathBuf};

pub const MEDIA: &str = "telegram-media";
pub const AVATARS: &str = "telegram-avatars";

/// How many downloads run at once: enough for a screenful of pictures, few enough not to look like a scraper.
const AT_ONCE: usize = 4;

/// How many whole files (videos, documents) are fetched at once. They take minutes, and pictures must not wait behind
/// them: those have places of their own.
const BIG_AT_ONCE: usize = 2;

fn permits(big: bool) -> &'static tokio::sync::Semaphore {
    static P: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    static BIG: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    if big {
        BIG.get_or_init(|| tokio::sync::Semaphore::new(BIG_AT_ONCE))
    } else {
        P.get_or_init(|| tokio::sync::Semaphore::new(AT_ONCE))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// A small picture to draw in the list.
    Thumb,
    /// The picture at the size the viewer shows, or the file itself.
    Full,
}

impl Variant {
    fn word(self) -> &'static str {
        match self {
            Variant::Thumb => "thumb",
            Variant::Full => "full",
        }
    }
}

/// A conversation's id as a piece of a file name: a group's `-` would be taken for a separator.
fn slug(remote: &str) -> String {
    remote.replace('-', "n")
}

fn unslug(text: &str) -> String {
    match text.strip_prefix('n') {
        Some(rest) => format!("-{rest}"),
        None => text.to_string(),
    }
}

pub fn media_name(account: i64, remote: &str, message: i32, variant: Variant, ext: &str) -> String {
    format!("tg_{account}_{}_{message}_{}.{ext}", slug(remote), variant.word())
}

pub fn avatar_name(account: i64, remote: &str, photo: i64) -> String {
    format!("tg_{account}_{}_{photo}.jpg", slug(remote))
}

/// What a cached media file's name says: the account, the conversation, the message and which version of it.
pub fn parse_media_name(name: &str) -> Option<(i64, String, i32, Variant)> {
    let stem = name.rsplit_once('.')?.0;
    let mut parts = stem.strip_prefix("tg_")?.split('_');
    let account = parts.next()?.parse().ok()?;
    let remote = unslug(parts.next()?);
    let message = parts.next()?.parse().ok()?;
    let variant = match parts.next()? {
        "thumb" => Variant::Thumb,
        "full" => Variant::Full,
        _ => return None,
    };
    parts.next().is_none().then_some((account, remote, message, variant))
}

/// What a cached avatar's name says: the account, the person or chat, and which picture of theirs.
pub fn parse_avatar_name(name: &str) -> Option<(i64, String, i64)> {
    let stem = name.strip_suffix(".jpg")?;
    let mut parts = stem.strip_prefix("tg_")?.split('_');
    let account = parts.next()?.parse().ok()?;
    let remote = unslug(parts.next()?);
    let photo = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((account, remote, photo))
}

fn local(dir: &str, name: &str) -> String {
    format!("file://{}", crate::media_cache::dir(dir).join(name).display())
}

/// The file's extension, from its name or else its type.
pub fn extension_of(name: Option<&str>, mime: Option<&str>) -> String {
    if let Some(ext) = name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric())) {
        return ext;
    }
    match mime.unwrap_or("") {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/m4a" => "m4a",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/x-tgsticker" => "tgs",
        "text/plain" => "txt",
        _ => "bin",
    }
    .to_string()
}

/// The broad shape of a document, which decides how the window draws it.
fn kind_of_mime(mime: &str) -> &'static str {
    if mime.starts_with("image/") && mime != "image/gif" {
        "image"
    } else if mime.starts_with("video/") || mime == "image/gif" {
        "video"
    } else if mime.starts_with("audio/") {
        "audio"
    } else {
        "file"
    }
}

fn dimensions(size: &PhotoSize) -> Option<(i32, i32)> {
    match size {
        PhotoSize::Size(s) => Some((s.width, s.height)),
        PhotoSize::Cached(s) => Some((s.width, s.height)),
        PhotoSize::Progressive(s) => Some((s.width, s.height)),
        _ => None,
    }
}

/// The version of a photo to draw in the list (about 320 wide) and the one to show in the viewer (about 1280).
pub(super) fn pick(sizes: &[PhotoSize], target: i32) -> Option<&PhotoSize> {
    let sized: Vec<(&PhotoSize, i32)> = sizes.iter().filter_map(|s| dimensions(s).map(|(w, h)| (s, w.max(h)))).collect();
    choose(&sized, target).copied()
}

/// The smallest that reaches the target, else the largest there is.
fn choose<T>(items: &[(T, i32)], target: i32) -> Option<&T> {
    let mut by_size: Vec<&(T, i32)> = items.iter().collect();
    by_size.sort_by_key(|(_, longest)| *longest);
    by_size.iter().find(|(_, longest)| *longest >= target).or(by_size.last()).map(|(item, _)| item)
}

/// The width a link card's picture is fetched at: it is drawn larger than a list thumbnail.
const CARD: i32 = 800;

/// The picture of the web page a message links to, if the page has one.
fn page_photo(media: &Media) -> Option<grammers_client::media::Photo> {
    let Media::WebPage(page) = media else { return None };
    let tl::enums::WebPage::Page(page) = &page.raw.webpage else { return None };
    page.photo.clone().map(grammers_client::media::Photo::from_raw)
}

/// The link preview a message carries (what Telegram calls a web page, with or without an Instant View): a card with
/// the site, title, description and picture. Its picture is named in the cache and fetched like any preview.
pub fn embeds_of(account: i64, remote: &str, message: &Message) -> Vec<Embed> {
    let Some(media) = message.media() else { return Vec::new() };
    let Media::WebPage(page) = &media else { return Vec::new() };
    let tl::enums::WebPage::Page(page) = &page.raw.webpage else { return Vec::new() };
    let has_picture = page_photo(&media).is_some();
    if page.title.is_none() && page.description.is_none() && !has_picture {
        return Vec::new();
    }
    let non_empty = |t: &Option<String>| t.clone().filter(|t| !t.trim().is_empty());
    vec![Embed {
        title: non_empty(&page.title),
        description: non_empty(&page.description),
        url: Some(page.url.clone()),
        provider: non_empty(&page.site_name),
        author: non_empty(&page.author),
        image_url: has_picture.then(|| local(MEDIA, &media_name(account, remote, message.id(), Variant::Thumb, "jpg"))),
        // A page Telegram has made an article of opens as one: the card says so, as Telegram's own does.
        kind: page.cached_page.is_some().then(|| "instant".to_string()),
        cta: page.cached_page.is_some().then(|| crate::model::EmbedLink { label: "Instant View".to_string(), url: None }),
        ..Default::default()
    }]
}

/// What the message carries, as attachments naming files in the cache. Nothing is downloaded here.
pub fn attachments_of(account: i64, remote: &str, message: &Message) -> Vec<Attachment> {
    let id = message.id();
    let Some(media) = message.media() else { return Vec::new() };
    let thumb = |ext: &str| Some(local(MEDIA, &media_name(account, remote, id, Variant::Thumb, ext)));
    let full = |ext: &str| Some(local(MEDIA, &media_name(account, remote, id, Variant::Full, ext)));
    match media {
        Media::Photo(photo) => {
            let sizes = photo.thumbs();
            let view = pick(&sizes, 1280).and_then(dimensions);
            vec![Attachment {
                kind: "image".into(),
                mimetype: Some("image/jpeg".into()),
                filename: Some(if photo.is_spoiler() { "SPOILER_photo.jpg".into() } else { "photo.jpg".into() }),
                size: photo.size().map(|s| s as u64),
                width: view.map(|(w, _)| w as u32),
                height: view.map(|(_, h)| h as u32),
                path: full("jpg"),
                thumbnail_path: thumb("jpg"),
                ..Default::default()
            }]
        }
        Media::Sticker(sticker) => {
            let document = sticker.document.clone();
            let mime = document.mime_type().unwrap_or("image/webp").to_string();
            let still = mime == "image/webp";
            let (w, h) = document.resolution().unwrap_or((512, 512));
            vec![Attachment {
                kind: "image".into(),
                mimetype: Some(if still { mime.clone() } else { "image/webp".into() }),
                filename: Some(format!("sticker{}", if sticker.emoji().is_empty() { String::new() } else { format!(" {}", sticker.emoji()) })),
                size: document.size().map(|s| s as u64),
                width: Some(w as u32),
                height: Some(h as u32),
                // An animated or video sticker is drawn as the still picture Telegram sends with it.
                path: if still { full(&extension_of(document.name(), Some(&mime))) } else { thumb("jpg") },
                thumbnail_path: thumb("jpg"),
                ..Default::default()
            }]
        }
        Media::Document(document) => {
            let mime = document.mime_type().unwrap_or("application/octet-stream").to_string();
            let name = document.name().map(str::to_string);
            let ext = extension_of(name.as_deref(), Some(&mime));
            let voice = is_voice(&document);
            let mut kind = kind_of_mime(&mime);
            if document.is_animated() && kind == "image" {
                kind = "video";
            }
            let (w, h) = document.resolution().map(|(w, h)| (Some(w as u32), Some(h as u32))).unwrap_or((None, None));
            vec![Attachment {
                kind: kind.into(),
                mimetype: Some(mime),
                filename: Some(match (&name, document.is_spoiler()) {
                    (Some(n), true) => format!("SPOILER_{n}"),
                    (Some(n), false) => n.clone(),
                    (None, _) => format!("file.{ext}"),
                }),
                size: document.size().map(|s| s as u64),
                width: w,
                height: h,
                duration_secs: document.duration(),
                voice,
                path: full(&ext),
                thumbnail_path: (!document.thumbs().is_empty() && matches!(kind, "video" | "file" | "image")).then(|| local(MEDIA, &media_name(account, remote, id, Variant::Thumb, "jpg"))),
                ..Default::default()
            }]
        }
        _ => Vec::new(),
    }
}

/// Whether a recording was spoken into the app rather than attached as a file.
fn is_voice(document: &Document) -> bool {
    match document.raw.document.as_ref() {
        Some(tl::enums::Document::Document(d)) => d.attributes.iter().any(|a| matches!(a, tl::enums::DocumentAttribute::Audio(audio) if audio.voice)),
        _ => false,
    }
}

/// Downloads to a file, whole and renamed into place so a half-written one is never taken for a picture.
pub(super) async fn download_to<D: Downloadable>(client: &grammers_client::Client, what: &D, path: &Path, big: bool) -> Result<()> {
    let _permit = permits(big).acquire().await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Its own name for the half-written file: two fetches of one picture (a preview being fetched as the message was
    // recorded, and the window asking for it) must not write into the same one.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part = path.with_extension(format!("{}.part", SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    if let Err(e) = client.download_media(what, &part).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(anyhow!("{}", super::send::explain(&e)));
    }
    tokio::fs::rename(&part, path).await.with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Fetches a message's media again and writes the version the name asks for.
async fn fetch(handle: &Handle, remote: &str, message: i32, variant: Variant, path: &Path) -> Result<()> {
    let id = peers::peer_id_of(remote).ok_or_else(|| anyhow!("not a Telegram conversation"))?;
    let peer = handle.session.peer_ref(id).await.ok().flatten().ok_or_else(|| anyhow!("Telegram has not told this account about that conversation yet"))?;
    let found = handle.client.get_messages_by_id(peer, &[message]).await.map_err(|e| anyhow!("{}", super::send::explain(&e)))?;
    let Some(Some(message)) = found.into_iter().next() else { bail!("that message is no longer there") };
    match message.media().ok_or_else(|| anyhow!("that message has no media"))? {
        Media::Photo(photo) => {
            let sizes = photo.thumbs();
            let wanted = match variant {
                Variant::Thumb => pick(&sizes, 320),
                Variant::Full => pick(&sizes, 1280),
            };
            match wanted {
                Some(size) => download_to(&handle.client, size, path, false).await,
                None => download_to(&handle.client, &photo, path, false).await,
            }
        }
        Media::Sticker(sticker) => match variant {
            Variant::Full => download_to(&handle.client, &sticker.document, path, false).await,
            Variant::Thumb => download_thumb(&handle.client, &sticker.document, path).await,
        },
        Media::Document(document) => match variant {
            Variant::Full => download_to(&handle.client, &document, path, true).await,
            Variant::Thumb => download_thumb(&handle.client, &document, path).await,
        },
        media @ Media::WebPage(_) => {
            let photo = page_photo(&media).ok_or_else(|| anyhow!("that link has no picture"))?;
            match pick(&photo.thumbs(), CARD) {
                Some(size) => download_to(&handle.client, size, path, false).await,
                None => download_to(&handle.client, &photo, path, false).await,
            }
        }
        _ => bail!("that kind of media cannot be fetched"),
    }
}

pub(super) async fn download_thumb(client: &grammers_client::Client, document: &Document, path: &Path) -> Result<()> {
    let sizes = document.thumbs();
    let size = pick(&sizes, 320).or(sizes.first()).ok_or_else(|| anyhow!("Telegram sent no preview of that"))?;
    // A tiny inline preview carries its own bytes; the rest are downloaded.
    if let Some(bytes) = Downloadable::to_data(size) {
        tokio::fs::write(path, bytes).await?;
        return Ok(());
    }
    download_to(client, size, path, false).await
}

/// The previews being fetched now, so a message read twice at once fetches its preview once.
fn fetching() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static F: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = std::sync::OnceLock::new();
    F.get_or_init(Default::default)
}

/// Fetches the small preview of a message's picture or video in the background, as the message is recorded.
///
/// Previews are a few kilobytes and are what the list draws, so they are not left to be asked for one by one when the
/// window finds them missing (which asks Telegram for the message again each time). The message is in hand here, so
/// nothing more is asked. Whole files are still fetched only when looked at.
pub fn prefetch_preview(state: &AppState, account_id: &str, account: i64, remote: &str, message: &Message) {
    let Some(media) = message.media() else { return };
    let path = crate::media_cache::dir(MEDIA).join(media_name(account, remote, message.id(), Variant::Thumb, "jpg"));
    if path.exists() || !matches!(media, Media::Photo(_) | Media::Document(_) | Media::Sticker(_) | Media::WebPage(_)) {
        return;
    }
    if !fetching().lock().unwrap_or_else(|e| e.into_inner()).insert(path.clone()) {
        return;
    }
    let Some(handle) = state.runtime.telegram_handle(account_id) else {
        fetching().lock().unwrap_or_else(|e| e.into_inner()).remove(&path);
        return;
    };
    tokio::spawn(async move {
        let done = match &media {
            Media::Photo(photo) => match pick(&photo.thumbs(), 320) {
                Some(size) => download_to(&handle.client, size, &path, false).await,
                None => download_to(&handle.client, photo, &path, false).await,
            },
            Media::Document(document) => download_thumb(&handle.client, document, &path).await,
            Media::Sticker(sticker) => download_thumb(&handle.client, &sticker.document, &path).await,
            Media::WebPage(_) => match page_photo(&media) {
                Some(photo) => match pick(&photo.thumbs(), CARD) {
                    Some(size) => download_to(&handle.client, size, &path, false).await,
                    None => download_to(&handle.client, &photo, &path, false).await,
                },
                None => Ok(()),
            },
            _ => Ok(()),
        };
        if let Err(e) = done {
            tracing::debug!("telegram: preview {}: {e:#}", path.display());
        }
        fetching().lock().unwrap_or_else(|e| e.into_inner()).remove(&path);
    });
}

/// Fetches a cached Telegram file again, after it expired, was cleared, or was named before it was ever fetched.
pub async fn restore_cached(state: &AppState, path: &Path) -> Result<PathBuf> {
    let name = path.file_name().and_then(|n| n.to_str()).context("not a cached file")?;
    let parent = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("");
    if parent == AVATARS {
        let (account, peer, photo) = parse_avatar_name(name).context("not a cached Telegram picture")?;
        let handle = state.runtime.telegram_handle(&format!("telegram:{account}")).context("that Telegram account is not connected")?;
        fetch_avatar(&handle, &peer, photo, path).await?;
        return Ok(path.to_path_buf());
    }
    // A GIF the picker listed is fetched from what the picker remembered of it.
    if name.starts_with("tgif_") {
        return super::gifs::restore(state, name, path).await?.context("not a cached Telegram GIF");
    }
    let (account, remote, message, variant) = parse_media_name(name).context("not a cached Telegram file")?;
    // A preview already on its way is waited for, not fetched a second time.
    for _ in 0..120 {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        if !fetching().lock().unwrap_or_else(|e| e.into_inner()).contains(path) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let handle = state.runtime.telegram_handle(&format!("telegram:{account}")).context("that Telegram account is not connected")?;
    fetch(&handle, &remote, message, variant, path).await?;
    Ok(path.to_path_buf())
}

/// The file an avatar is cached as, for a person or chat with a profile photo; `None` for one without.
pub fn avatar_url(account: i64, peer: &grammers_client::peer::Peer) -> Option<String> {
    use grammers_client::peer::Peer;
    let photo_id = match peer {
        Peer::User(u) => u.photo().map(|p| p.photo_id),
        Peer::Group(g) => g.photo().map(|p| p.photo_id),
        Peer::Channel(c) => c.photo().map(|p| p.photo_id),
        Peer::Community(_) => None,
    }?;
    Some(local(AVATARS, &avatar_name(account, &peers::remote_id(peer.id()), photo_id)))
}

async fn fetch_avatar(handle: &Handle, remote: &str, photo_id: i64, path: &Path) -> Result<()> {
    let id: PeerId = peers::peer_id_of(remote).ok_or_else(|| anyhow!("not a Telegram conversation"))?;
    let peer = handle.session.peer_ref(id).await.ok().flatten().ok_or_else(|| anyhow!("Telegram has not told this account about that picture's owner yet"))?;
    let what = grammers_client::media::ChatPhoto { raw: tl::types::InputPeerPhotoFileLocation { big: false, peer: peer.into(), photo_id }.into() };
    download_to(&handle.client, &what, path, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_files_name_says_what_it_is_and_is_read_back() {
        let name = media_name(1234567890, "-1001234567890", 21910, Variant::Full, "mp4");
        assert_eq!(name, "tg_1234567890_n1001234567890_21910_full.mp4");
        assert_eq!(parse_media_name(&name), Some((1234567890, "-1001234567890".to_string(), 21910, Variant::Full)));
        let person = media_name(1, "6576171495", 7, Variant::Thumb, "jpg");
        assert_eq!(parse_media_name(&person), Some((1, "6576171495".to_string(), 7, Variant::Thumb)));
        assert_eq!(parse_media_name("not_ours.jpg"), None);
        assert_eq!(parse_media_name("tg_1_n5_2_other.jpg"), None);
        let avatar = avatar_name(1, "-100123", 99);
        assert_eq!(parse_avatar_name(&avatar), Some((1, "-100123".to_string(), 99)));
        assert_eq!(parse_avatar_name("tg_1_n5_2_full.jpg"), None, "a media file is not an avatar");
    }

    #[test]
    fn a_files_extension_comes_from_its_name_else_its_type() {
        assert_eq!(extension_of(Some("Report.PDF"), None), "pdf");
        assert_eq!(extension_of(None, Some("video/mp4")), "mp4");
        assert_eq!(extension_of(Some("noextension"), Some("audio/ogg")), "ogg");
        assert_eq!(extension_of(Some("weird.name with spaces"), None), "bin");
        assert_eq!(kind_of_mime("image/webp"), "image");
        assert_eq!(kind_of_mime("image/gif"), "video");
        assert_eq!(kind_of_mime("application/pdf"), "file");
    }

    #[test]
    fn the_size_to_draw_is_the_smallest_that_reaches_the_target_else_the_largest() {
        let sizes = vec![("s", 90), ("y", 1280), ("m", 320), ("x", 800)];
        assert_eq!(choose(&sizes, 320), Some(&"m"));
        assert_eq!(choose(&sizes, 1280), Some(&"y"));
        assert_eq!(choose(&sizes, 5000), Some(&"y"));
        assert_eq!(choose(&sizes, 100), Some(&"m"));
        assert_eq!(choose::<&str>(&[], 320), None);
    }
}
