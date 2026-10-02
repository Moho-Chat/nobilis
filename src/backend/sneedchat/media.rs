//! Avatars and attachments: fetched through Tor, cached on disk.
//!
//! Nothing here can be handed to the renderer as a URL - the site is only
//! reachable through the daemon's Tor client, and a window has no Tor of its
//! own. So everything is fetched here and passed on as a local path.

use super::*;

/// Where cached avatar images live - a genuine cache (every file here is
/// re-fetchable from the site given its raw_avatar_url, see
/// cached_avatar_path), so XDG_CACHE_HOME rather than the app's own
/// config dir (see main.rs's `parse_args`) it used to sit under - main.rs's
/// migrate_caches_to_xdg_cache_dir moves any pre-existing directory here
/// once at startup.
pub(super) fn avatar_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache")).join("nobilis").join("sneedchat-avatars")
}

/// Cap on the avatar cache's total size on disk - without this, every
/// distinct poster ever seen across every room accumulates its own
/// permanently-cached file forever (see cached_avatar_path's own doc
/// comment: avatars are never re-checked once cached), which over months
/// of use in busy rooms is genuinely unbounded growth, not a one-time cost.
pub(super) const AVATAR_CACHE_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// How often to sweep the cache back under the cap - called from main.rs,
/// once per daemon process regardless of how many Sneedchat accounts are
/// configured (a per-account timer would just repeat the same sweep).
pub const AVATAR_CACHE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Evicts the oldest-written files in `dir` until it's back under
/// `max_bytes`. Oldest-by-mtime rather than true LRU (a cache hit doesn't
/// bump a file's timestamp) - simpler, no extra dependency, and "the files
/// nobody's referenced recently age out first" is a reasonable
/// approximation of LRU for this purpose anyway. Shared by the avatar and
/// attachment caches below - same growth problem (a permanent per-item
/// file that's never re-checked once cached, see cached_avatar_path's and
/// resolve_attachment's own doc comments), same fix.
pub async fn sweep_cache_dir(dir: &std::path::Path, max_bytes: u64, label: &str) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };

    let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        total += meta.len();
        files.push((entry.path(), meta.len(), mtime));
    }

    if total <= max_bytes {
        return;
    }

    files.sort_by_key(|(_, _, mtime)| *mtime);
    let overage = total - max_bytes;
    let mut to_free = overage;
    let mut removed = 0usize;
    for (path, size, _) in files {
        if to_free == 0 {
            break;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            to_free = to_free.saturating_sub(size);
            removed += 1;
        }
    }
    tracing::info!(
        "{label} cache was {}MB over its {}MB cap, evicted {removed} oldest file(s)",
        overage / 1024 / 1024,
        max_bytes / 1024 / 1024
    );
}

/// Removes the caches Sneedchat kept while it was called Sockchat.
///
/// The message store moved its ids to the new name; nothing moved or read the
/// files, which sat at a hundred-odd megabytes unread ever since. A cache, so
/// nothing is lost: anything still wanted is fetched again under the new name.
pub async fn remove_retired_caches() {
    let root = dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis");
    for name in ["sockchat-attachments", "sockchat-avatars"] {
        let dir = root.join(name);
        if tokio::fs::remove_dir_all(&dir).await.is_ok() {
            tracing::info!("removed the retired {name} cache");
        }
    }
}

pub async fn sweep_avatar_cache() {
    sweep_cache_dir(&avatar_cache_dir(), AVATAR_CACHE_MAX_BYTES, "avatar").await;
}

/// Where cached attachment images/video live - same `~/.cache/nobilis`
/// root every other protocol's media/avatar cache uses (see matrix/mod.rs's
/// media_cache_dir), rather than the OS temp dir this used to sit under -
/// one place to look, one sweep loop, one size cap convention for
/// "downloaded content this app can always re-fetch on a cache miss"
/// regardless of which backend produced it. main.rs's
/// migrate_caches_to_xdg_cache_dir moves any pre-existing `/tmp` directory
/// here once at startup.
pub(super) fn attachment_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache")).join("nobilis").join("sneedchat-attachments")
}

/// Bigger than the avatar cap - full images/clips run much larger than
/// small profile pictures.
pub(super) const ATTACHMENT_CACHE_MAX_BYTES: u64 = 250 * 1024 * 1024;

pub async fn sweep_attachment_cache() {
    sweep_cache_dir(&attachment_cache_dir(), ATTACHMENT_CACHE_MAX_BYTES, "attachment").await;
}

/// The file extension an image's own leading bytes call for, or None if the
/// format isn't recognised.
///
/// Content, not the URL: the site hands out avatar links ending in .jpg and
/// its CDN answers them with WebP, so the name a link implies is not evidence
/// of what arrived.
pub(super) fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        // RIFF is a container; the form is named four bytes into the payload.
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        // Likewise ISO-BMFF: AVIF, HEIC and MP4 share the ftyp box and are
        // told apart by the brand that follows it.
        [_, _, _, _, b'f', b't', b'y', b'p', b, r, a, n, ..] => match [*b, *r, *a, *n] {
            [b'a', b'v', b'i', b'f'] | [b'a', b'v', b'i', b's'] => Some("avif"),
            [b'h', b'e', b'i', b'c'] | [b'h', b'e', b'i', b'x'] => Some("heic"),
            _ => Some("mp4"),
        },
        _ => None,
    }
}

/// Every extension sniff_image_ext can produce, plus the ones older caches
/// were written with - what a lookup has to consider, since the name a
/// cached avatar ended up under depends on what its bytes turned out to be.
pub(super) const CACHED_AVATAR_EXTS: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "avif", "heic", "mp4"];

/// An existing cache entry for this user, if one is there and is what its
/// name claims.
///
/// The extension check is what lets a cache written before content sniffing
/// heal itself: an entry whose bytes disagree with its name is treated as
/// absent, so it is re-fetched and rewritten under the right one. Only the
/// first few bytes are read, so this stays cheap enough to run per message.
pub(super) async fn cached_avatar_file(dir: &std::path::Path, user_id: &str) -> Option<std::path::PathBuf> {
    for ext in CACHED_AVATAR_EXTS {
        let path = dir.join(format!("{user_id}.{ext}"));
        let Ok(mut file) = tokio::fs::File::open(&path).await else { continue };
        let mut head = [0u8; 16];
        use tokio::io::AsyncReadExt;
        let Ok(n) = file.read(&mut head).await else { continue };
        match sniff_image_ext(&head[..n]) {
            // "jpeg" and "jpg" are the same thing under two names.
            Some(actual) if actual == ext || (actual == "jpg" && ext == "jpeg") => return Some(path),
            // Unrecognised bytes: nothing better to go on, so keep it.
            None => return Some(path),
            Some(_) => {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    None
}

/// Beside each cached avatar, the URL it was fetched from - what every
/// message's `avatar_url` is checked against.
fn avatar_source_file(dir: &std::path::Path, user_id: &str) -> std::path::PathBuf {
    dir.join(format!("{user_id}.src"))
}

/// The local URL for a cached avatar, versioned by when its bytes last
/// changed.
///
/// The file keeps one name per user, so scrollback already pointing at it
/// shows a new picture once there is one - but a window that has drawn the
/// old one keeps its copy for the same URL. The fragment changes when the
/// picture does, which is what makes the window ask again; the media handler
/// never sees it.
async fn versioned(path: &std::path::Path) -> String {
    let stamp = tokio::fs::metadata(path)
        .await
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("file://{}#{stamp}", path.display())
}

/// Resolves a wire `avatar_url` (a relative path or an absolute URL)
/// against `host`, fetches it through the same Tor-routed client used for
/// everything else, and caches it to a local file - the frontend's plain
/// `Image` element has no route through Tor (or to a `.onion` host at
/// all) on its own, so handing back the original remote URL would simply
/// never load. A `file://` path sidesteps that, the same trick already
/// used for Discord's QR login code.
///
/// Every message carries its author's current `avatar_url`, and that is the
/// check: it is compared with the URL the cached picture came from, and only
/// a different one is fetched. A new avatar is a new URL, so it is picked up
/// on that person's next message, and nothing is fetched while it stays the
/// same - no timer, no rescan.
pub(super) async fn cached_avatar_path(http: &http::HttpClient, host: &str, user_id: &str, raw_avatar_url: &str) -> Option<String> {
    if raw_avatar_url.is_empty() {
        return None;
    }
    let url = if raw_avatar_url.starts_with('/') { format!("https://{host}{raw_avatar_url}") } else { raw_avatar_url.to_string() };

    let dir = avatar_cache_dir();
    let source = avatar_source_file(&dir, user_id);
    let cached = cached_avatar_file(&dir, user_id).await;
    if let Some(path) = &cached {
        if tokio::fs::read_to_string(&source).await.ok().as_deref() == Some(url.as_str()) {
            return Some(versioned(path).await);
        }
    }

    // A different URL, or a picture cached before the URL was kept beside it:
    // fetch it. If that fails, the picture already here is still the best
    // there is.
    let fallback = || async {
        match &cached {
            Some(path) => Some(versioned(path).await),
            None => None,
        }
    };

    // Bounded rather than left open-ended: this runs inline in the room's
    // own read loop (see handle_frame), so a slow/stuck fetch (a fresh Tor
    // circuit needed for a host this session hasn't hit before, a route
    // that just hangs) would otherwise stall that entire room's message
    // processing until it resolved.
    let Ok(fetch) = tokio::time::timeout(std::time::Duration::from_secs(15), http.get_bytes(&url)).await else {
        tracing::debug!("sneedchat: avatar fetch for user {user_id} timed out");
        return fallback().await;
    };
    let bytes = match fetch {
        Ok((status, bytes)) if (200..300).contains(&status) && !bytes.is_empty() => bytes,
        Ok((status, _)) => {
            tracing::debug!("sneedchat: avatar fetch for user {user_id} returned HTTP {status}");
            return fallback().await;
        }
        Err(e) => {
            tracing::debug!("sneedchat: fetching avatar for user {user_id}: {e}");
            return fallback().await;
        }
    };

    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::debug!("sneedchat: creating avatar cache dir: {e}");
        return fallback().await;
    }
    // Name the file after what it actually is. The site's avatar URLs end in
    // .jpg while the CDN transparently serves WebP, so trusting the URL wrote
    // WebP into a .jpg - which anything that dispatches on extension then
    // refuses to load.
    let url_ext = url
        .split(['?', '#'])
        .next()
        .unwrap_or(&url)
        .rsplit('.')
        .next()
        .filter(|e| e.len() <= 4 && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("jpg");
    let ext = sniff_image_ext(&bytes).unwrap_or(url_ext);
    let path = dir.join(format!("{user_id}.{ext}"));
    // Unchanged bytes are not rewritten, so the version a window holds only
    // moves when the picture does.
    if tokio::fs::read(&path).await.ok().as_deref() != Some(&bytes[..]) {
        // A picture that changed format replaces the old file rather than
        // sitting beside it, where the lookup could find the old one first.
        for other in CACHED_AVATAR_EXTS.iter().filter(|e| **e != ext) {
            let _ = tokio::fs::remove_file(dir.join(format!("{user_id}.{other}"))).await;
        }
        if let Err(e) = tokio::fs::write(&path, &bytes).await {
            tracing::debug!("sneedchat: caching avatar for user {user_id}: {e}");
            return fallback().await;
        }
    }
    let _ = tokio::fs::write(&source, &url).await;
    Some(versioned(&path).await)
}

/// Recognized image/video file extensions for attachment links - shares
/// the client's own media-detection list, since this exists for the same
/// reason: deciding whether a link is worth embedding.
pub(super) const ATTACHMENT_EXTS: &[&str] =
    &["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif", "mp4", "webm", "mov", "mkv"];

/// Finds the first Kiwi Farms/XenForo attachment URL in `text` - either
/// host, clearnet or onion (see DEFAULT_ONION) - shaped like
/// `/attachments/<name>-<ext>.<id>/`. Returns the exact matched substring
/// (so callers can `body.replace()` it verbatim) plus the id and
/// extension, both embedded in the URL's own slug: XenForo builds it by
/// replacing the original filename's "." with "-" (so
/// "sam-consent-accident.webp" becomes the slug
/// "sam-consent-accident-webp") and appending ".<attachment id>/". No
/// network round-trip is needed just to classify the link as media - only
/// to actually fetch it (see resolve_attachment below).
pub(super) fn find_attachment_url(text: &str) -> Option<(&str, &str, &str)> {
    for host in [KIWIFARMS_CLEARNET_HOST, DEFAULT_ONION] {
        let marker_start = format!("https://{host}/attachments/");
        let Some(start) = text.find(&marker_start) else { continue };
        let seg_start = start + marker_start.len();
        let rest = &text[seg_start..];
        let seg_len = rest.find(|c: char| c.is_whitespace() || matches!(c, '<' | '[' | ']')).unwrap_or(rest.len());
        let end = seg_start + seg_len;
        let segment = text[seg_start..end].trim_end_matches('/');

        let Some(dot) = segment.rfind('.') else { continue };
        let (name_part, id_part) = (&segment[..dot], &segment[dot + 1..]);
        if id_part.is_empty() || !id_part.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(dash) = name_part.rfind('-') else { continue };
        let ext = &name_part[dash + 1..];
        if !ATTACHMENT_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        return Some((&text[start..end], id_part, ext));
    }
    None
}

pub const KIWIFARMS_CLEARNET_HOST: &str = "kiwifarms.st";

/// Writes the forum's short attachment form out in full.
///
/// People type only the path - `/attachments/seal-png.9609686/` - and the
/// site's own chat page fills in its domain and shows the picture. Nothing
/// else knows the domain, so here it is filled in with `host`: the address
/// this account reaches the forum at, kiwifarms.st or the onion address as
/// it connects over the open internet or through Tor. After this the short
/// form is an ordinary attachment link and embeds like one.
///
/// Only a path standing on its own, at the start of the message or after a
/// space or a tag, and only one shaped like an attachment (`<name>-<ext>.<id>`
/// with a media extension) - so `https://example.com/attachments/...`, or a
/// path somebody is merely talking about, is left as it is.
pub(super) fn expand_attachment_paths<'a>(body: &'a str, host: &str) -> std::borrow::Cow<'a, str> {
    const MARKER: &str = "/attachments/";
    if !body.contains(MARKER) {
        return std::borrow::Cow::Borrowed(body);
    }
    let mut out = String::with_capacity(body.len() + 80);
    let mut rest = body;
    let mut changed = false;
    let mut consumed = 0usize;
    while let Some(at) = rest.find(MARKER) {
        let absolute = consumed + at;
        let before = body[..absolute].chars().next_back();
        let standalone = matches!(before, None | Some(' ' | '\n' | '\t' | ']' | '(' | '>'));
        let tail = &body[absolute..];
        let len = tail.find(|c: char| c.is_whitespace() || matches!(c, '<' | '[' | ']' | ')')).unwrap_or(tail.len());
        let path = &tail[..len];
        out.push_str(&rest[..at]);
        let full = format!("https://{host}{path}");
        if standalone && find_attachment_url(&full).is_some_and(|(matched, ..)| matched == full) {
            out.push_str(&full);
            changed = true;
        } else {
            out.push_str(path);
        }
        consumed = absolute + len;
        rest = &body[consumed..];
    }
    out.push_str(rest);
    if changed { std::borrow::Cow::Owned(out) } else { std::borrow::Cow::Borrowed(body) }
}

/// Fetched from the address this account reaches the forum at (`host`),
/// whichever address the link was written with: through Tor that is the
/// onion address - the clearnet edge mishandles Tor exit traffic, and a
/// hidden service involves no exit at all - and on the open internet it is
/// kiwifarms.st, since an onion address means nothing there.
///
/// Spawned as its own task rather than run inline in handle_frame's
/// per-room read loop: the message is recorded and shown immediately with
/// its original link, and the body only gets patched in place (via
/// Runtime::update_message_body_only, which - unlike a real edit - leaves
/// the "(edited)" label alone) once the local copy is ready, re-rendering
/// the same message with a working embed a moment later instead of
/// stalling the whole room's message processing on one slow Tor fetch.
pub(super) fn spawn_attachment_resolve(state: AppState, http: http::HttpClient, host: &str, buffer_id: String, msg_id: String, body: String) {
    let Some((matched, id, ext)) = find_attachment_url(&body) else { return };
    let (matched, id, ext) = (matched.to_string(), id.to_string(), ext.to_ascii_lowercase());
    let fetch_url = match matched.find("/attachments/") {
        Some(at) => format!("https://{host}{}", &matched[at..]),
        None => return,
    };

    tokio::spawn(async move {
        let dir = attachment_cache_dir();
        let path = dir.join(format!("{id}.{ext}"));

        let local_url = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            Some(format!("file://{}", path.display()))
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(30), http.get_bytes(&fetch_url)).await {
                Ok(Ok((status, bytes))) if (200..300).contains(&status) && !bytes.is_empty() => {
                    if tokio::fs::create_dir_all(&dir).await.is_ok() && tokio::fs::write(&path, &bytes).await.is_ok() {
                        Some(format!("file://{}", path.display()))
                    } else {
                        tracing::debug!("sneedchat: caching attachment {id} failed");
                        None
                    }
                }
                Ok(Ok((status, _))) => {
                    tracing::debug!("sneedchat: attachment {id} fetch returned HTTP {status}");
                    None
                }
                Ok(Err(e)) => {
                    tracing::debug!("sneedchat: fetching attachment {id}: {e}");
                    None
                }
                Err(_) => {
                    tracing::debug!("sneedchat: attachment {id} fetch timed out");
                    None
                }
            }
        };

        if let Some(local_url) = local_url {
            let new_body = body.replace(&matched, &local_url);
            state.runtime.update_message_body_only(&state, &buffer_id, &msg_id, &new_body);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_real_reported_url() {
        let body = "look at this https://kiwifarms.st/attachments/sam-consent-accident-webp.5648955/ what a guy";
        let (matched, id, ext) = find_attachment_url(body).expect("should match");
        assert_eq!(matched, "https://kiwifarms.st/attachments/sam-consent-accident-webp.5648955/");
        assert_eq!(id, "5648955");
        assert_eq!(ext, "webp");
    }

    /// The format the site's own uploader now hands back for a photograph.
    /// Recognised by the sniffer and cacheable long before it was on the list
    /// of extensions worth embedding, so an avif attachment was fetched and
    /// then shown as a link.
    #[test]
    fn an_avif_attachment_is_a_picture_like_any_other() {
        let (matched, id, ext) =
            find_attachment_url("https://kiwifarms.st/attachments/a-photo-avif.123/").expect("should match");
        assert_eq!(matched, "https://kiwifarms.st/attachments/a-photo-avif.123/");
        assert_eq!(id, "123");
        assert_eq!(ext, "avif");
    }

    #[test]
    fn matches_without_a_trailing_slash() {
        let (matched, id, ext) = find_attachment_url("https://kiwifarms.st/attachments/foo-bar-png.42").expect("should match");
        assert_eq!(matched, "https://kiwifarms.st/attachments/foo-bar-png.42");
        assert_eq!(id, "42");
        assert_eq!(ext, "png");
    }

    #[test]
    fn a_bare_attachment_path_is_given_the_host_the_account_connects_to() {
        let clear = "https://kiwifarms.st/attachments/seal-png.9609686/";
        assert_eq!(expand_attachment_paths("/attachments/seal-png.9609686/", KIWIFARMS_CLEARNET_HOST), clear);
        assert_eq!(expand_attachment_paths("look /attachments/seal-png.9609686/ lol", KIWIFARMS_CLEARNET_HOST), format!("look {clear} lol"));
        assert_eq!(expand_attachment_paths("[img]/attachments/seal-png.9609686/[/img]", KIWIFARMS_CLEARNET_HOST), format!("[img]{clear}[/img]"));
        // Through Tor, the onion address instead.
        let onion = format!("https://{DEFAULT_ONION}/attachments/seal-png.9609686/");
        assert_eq!(expand_attachment_paths("/attachments/seal-png.9609686/", DEFAULT_ONION), onion);
        // Without the trailing slash, as people type it too.
        assert_eq!(
            expand_attachment_paths("/attachments/seal-png.9609686", KIWIFARMS_CLEARNET_HOST),
            "https://kiwifarms.st/attachments/seal-png.9609686"
        );
        // And the result is an attachment link like any other.
        assert!(find_attachment_url(&expand_attachment_paths("/attachments/seal-png.9609686/", DEFAULT_ONION)).is_some());
    }

    #[test]
    fn only_a_standalone_attachment_shaped_path_is_expanded() {
        for left_alone in [
            "https://kiwifarms.st/attachments/seal-png.9609686/",
            "https://example.com/attachments/seal-png.9609686/",
            "see site/attachments/seal-png.9609686/",
            "/attachments/report-txt.7/",
            "/attachments/seal-png.notanid/",
            "the /attachments/ folder",
            "nothing here",
        ] {
            assert_eq!(expand_attachment_paths(left_alone, KIWIFARMS_CLEARNET_HOST), left_alone, "{left_alone}");
        }
    }

    #[test]
    fn matches_the_onion_host_too() {
        let url = format!("https://{}/attachments/clip-mp4.99/", super::DEFAULT_ONION);
        let (matched, id, ext) = find_attachment_url(&url).expect("should match");
        assert_eq!(matched, url);
        assert_eq!(id, "99");
        assert_eq!(ext, "mp4");
    }

    #[test]
    fn stops_at_a_bracket_or_whitespace() {
        let (matched, ..) = find_attachment_url("[url=https://kiwifarms.st/attachments/x-webp.1/]click[/url]").expect("should match");
        assert_eq!(matched, "https://kiwifarms.st/attachments/x-webp.1/");
    }

    #[test]
    fn rejects_a_non_media_extension() {
        assert!(find_attachment_url("https://kiwifarms.st/attachments/report-txt.7/").is_none());
    }

    #[test]
    fn rejects_a_non_numeric_id() {
        assert!(find_attachment_url("https://kiwifarms.st/attachments/foo-webp.abc/").is_none());
    }

    #[test]
    fn rejects_an_unrelated_host() {
        assert!(find_attachment_url("https://example.com/attachments/foo-webp.5/").is_none());
    }

    #[test]
    fn rejects_a_path_with_no_extension_suffix() {
        // No "-<ext>" segment before the numeric id at all.
        assert!(find_attachment_url("https://kiwifarms.st/attachments/justaname.5/").is_none());
    }

    #[test]
    fn names_an_image_by_its_own_bytes() {
        assert_eq!(sniff_image_ext(b"\x89PNG\r\n\x1a\n....."), Some("png"));
        assert_eq!(sniff_image_ext(b"\xff\xd8\xff\xe0 jfif"), Some("jpg"));
        assert_eq!(sniff_image_ext(b"GIF89a......"), Some("gif"));
        assert_eq!(sniff_image_ext(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("webp"));
        assert_eq!(sniff_image_ext(b"\x00\x00\x00\x20ftypavif...."), Some("avif"));
        assert_eq!(sniff_image_ext(b"\x00\x00\x00\x20ftypisom...."), Some("mp4"));
    }

    #[test]
    fn a_riff_that_is_not_a_webp_is_not_claimed() {
        // RIFF also fronts WAV and AVI; only the WEBP form is an image here.
        assert_eq!(sniff_image_ext(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_image_ext(b"nothing recognisable"), None);
        assert_eq!(sniff_image_ext(b""), None);
    }

    /// The regression this whole change exists for: the site serves WebP from
    /// a .jpg URL, so entries cached before sniffing carry the wrong name.
    #[tokio::test]
    async fn a_mislabelled_cache_entry_is_discarded_so_it_gets_refetched() {
        let dir = std::env::temp_dir().join(format!("nobilis-avatar-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("42.jpg");
        std::fs::write(&stale, b"RIFF\x00\x00\x00\x00WEBPVP8 payload").unwrap();

        assert!(cached_avatar_file(&dir, "42").await.is_none(), "kept a WebP named .jpg");
        assert!(!stale.exists(), "left the mislabelled file behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_correctly_named_entry_is_reused() {
        let dir = std::env::temp_dir().join(format!("nobilis-avatar-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("7.webp");
        std::fs::write(&good, b"RIFF\x00\x00\x00\x00WEBPVP8 payload").unwrap();

        assert_eq!(cached_avatar_file(&dir, "7").await.as_deref(), Some(good.as_path()));
        // A real JPEG under .jpg must also survive - this must not re-fetch
        // every avatar on every message.
        let jpg = dir.join("8.jpg");
        std::fs::write(&jpg, b"\xff\xd8\xff\xe0 jfif payload").unwrap();
        assert_eq!(cached_avatar_file(&dir, "8").await.as_deref(), Some(jpg.as_path()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
