//! Pictures: thumbnails made, cached, swept, and links re-signed.
//!
//! Discord's CDN links expire, which for a chat log means a picture that was
//! there yesterday is a broken box today unless something notices and asks
//! for a fresh link. That noticing is here, along with the thumbnail cache
//! that keeps the log from pulling full-size images it will draw at 480px.

use super::*;

/// Downloads a preview of each image attachment and records where it landed.
///
/// Runs in the background rather than inline: a message should appear the
/// moment it arrives, not after its picture has been fetched. The stored
/// message is updated once the copies exist, and the change is broadcast so
/// anything already showing that message picks them up.
pub fn cache_thumbnails(state: AppState, buffer_id: String, msg_id: String, attachments: Vec<Attachment>) {
    if !attachments.iter().any(|a| a.kind == "image" && a.thumbnail_path.is_none()) {
        return;
    }
    tokio::spawn(async move {
        let mut updated = attachments;
        let mut any = false;
        for att in &mut updated {
            if att.kind != "image" || att.thumbnail_path.is_some() {
                continue;
            }
            let Some(url) = att.url.as_deref() else { continue };
            let Some(src) = thumbnail_source(url, att.width.unwrap_or(0), att.height.unwrap_or(0)) else { continue };
            if let Some(path) = fetch_thumbnail(&src, url).await {
                att.thumbnail_path = Some(path);
                any = true;
            }
        }
        if !any {
            return;
        }
        // Not an edit: the message text is untouched, only where its preview
        // can be found locally.
        if let Err(e) = state.store.update_message_attachments(&buffer_id, &msg_id, &updated) {
            tracing::debug!("discord: recording thumbnails: {e}");
            return;
        }
        state.events.emit(
            "messageUpdated",
            json!({ "bufferId": buffer_id, "id": msg_id, "edited": false, "attachments": updated }),
        );
    });
}

pub(super) async fn fetch_thumbnail(src: &str, cache_key: &str) -> Option<String> {
    let dir = thumbnail_cache_dir();
    // Keyed by the *unsigned* part of the URL - the signature changes on every
    // refresh, so including it would cache the same picture repeatedly.
    let stable = cache_key.split('?').next().unwrap_or(cache_key);
    let mut hasher = Sha256::new();
    hasher.update(stable.as_bytes());
    let path = dir.join(format!("{:x}", hasher.finalize()));

    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Some(format!("file://{}", path.display()));
    }
    tokio::fs::create_dir_all(&dir).await.ok()?;

    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), http_client().get(src).send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    tokio::fs::write(&path, &bytes).await.ok()?;
    Some(format!("file://{}", path.display()))
}

/// Buffers with a re-sign already running, so a burst of getBacklog calls
/// (opening, scrolling, opening again) issues one sweep rather than several
/// against a rate-limited endpoint.
pub(super) fn resigning() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// How many history pages one sweep will pull. Each covers ~50 messages, so
/// this reaches a few screenfuls; past that the remaining stale messages are
/// left for the per-message refresh a click triggers, rather than hammering a
/// rate-limited endpoint on every buffer open.
pub(super) const MAX_RESIGN_FETCHES: usize = 3;

/// Re-signs every expired attachment across a page of scrollback, in as few
/// requests as it can manage.
///
/// Discord's own client batches this through `/attachments/refresh-urls`,
/// which refuses user tokens here. But a history read re-signs every
/// attachment in the page it returns, so one fetch of ~50 messages does the
/// same job for a screenful - far better than one request per image, which is
/// what refreshing each on click costs.
///
/// Runs in the background: the buffer opens immediately showing cached
/// previews, and re-signed links arrive as messageUpdated events.
pub fn resign_stale_attachments(state: AppState, buffer_id: String, messages: &[crate::model::Message]) {
    let stale = stale_message_ids(messages);
    if stale.is_empty() {
        return;
    }

    if !resigning().lock().unwrap().insert(buffer_id.clone()) {
        return;
    }

    tokio::spawn(async move {
        if let Err(e) = run_resign(&state, &buffer_id, stale).await {
            tracing::debug!("discord: re-signing {buffer_id}: {e}");
        }
        resigning().lock().unwrap().remove(&buffer_id);
    });
}

/// Which messages in a page carry a lapsed attachment link, newest first.
///
/// Newest first because those are the ones most likely to be on screen, so
/// the first fetch re-signs what the reader is actually looking at.
///
/// Attachments with no `url` at all are nobilis's own locally-cached media
/// (Matrix, Sneedchat) and never expire, so a non-Discord buffer produces an
/// empty list here and costs nothing.
pub(super) fn stale_message_ids(messages: &[crate::model::Message]) -> Vec<String> {
    let mut ids: Vec<String> = messages
        .iter()
        .filter(|m| {
            m.attachments
                .iter()
                .any(|a| a.url.as_deref().is_some_and(attachment_expired))
        })
        .map(|m| m.id.clone())
        .collect();
    // Discord ids are snowflakes: lexicographically ordered for equal length,
    // and longer means newer, so sort by length first.
    ids.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| b.cmp(a)));
    ids
}

pub(super) async fn run_resign(state: &AppState, buffer_id: &str, mut stale: Vec<String>) -> Result<()> {
    let buffer = state.runtime.get_buffer(buffer_id).context("no such buffer")?;
    let config = state.accounts.get_discord(&buffer.account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel")?;

    for _ in 0..MAX_RESIGN_FETCHES {
        let Some(anchor) = stale.first().cloned() else { break };
        // `around` centres the page on the message, so one fetch covers what
        // sits either side of it - usually the rest of the same screenful.
        let resp = http_client()
            .get(format!("{API_BASE}/channels/{channel_id}/messages"))
            .query(&[("limit", "50"), ("around", anchor.as_str())])
            .header("Authorization", &config.token)
            .send()
            .await
            .context("fetching a page to re-sign")?;
        if !resp.status().is_success() {
            bail!("Discord refused the request ({})", resp.status());
        }
        let page: Vec<Value> = resp.json().await.context("parsing the re-signed page")?;
        if page.is_empty() {
            break;
        }

        let mut handled = 0usize;
        for message in &page {
            let Some(id) = message["id"].as_str() else { continue };
            if !stale.iter().any(|s| s == id) {
                continue;
            }
            let attachments = merge_cached_thumbnails(state, buffer_id, id, extract_attachments(message));
            if attachments.is_empty() {
                continue;
            }
            if state.store.update_message_attachments(buffer_id, id, &attachments).unwrap_or(false) {
                state.events.emit(
                    "messageUpdated",
                    json!({ "bufferId": buffer_id, "id": id, "edited": false, "attachments": attachments.clone() }),
                );
            }
            // The link is valid again right now, and this message reached a
            // re-sign because its preview was needed - so take one while it can
            // still be fetched, and the next expiry has something to show.
            cache_thumbnails(state.clone(), buffer_id.to_string(), id.to_string(), attachments);
            handled += 1;
        }

        let covered: std::collections::HashSet<&str> = page.iter().filter_map(|m| m["id"].as_str()).collect();
        // Drop everything this page spanned, not just what it re-signed - a
        // message inside the returned range that came back without
        // attachments was deleted or edited, and asking again won't change
        // that. Without this the anchor would not advance and the loop would
        // refetch the same page.
        stale.retain(|id| !covered.contains(id.as_str()));
        if stale.is_empty() {
            break;
        }
        // Nothing in range matched and nothing was dropped: give up rather
        // than spin.
        if handled == 0 && covered.is_empty() {
            break;
        }
    }
    Ok(())
}

/// Cached previews are keyed by the unsigned URL, so they stay valid across a
/// re-sign and are carried over rather than re-fetched.
pub(super) fn merge_cached_thumbnails(
    state: &AppState,
    buffer_id: &str,
    message_id: &str,
    attachments: Vec<Attachment>,
) -> Vec<Attachment> {
    match state.store.get_message(buffer_id, message_id) {
        Ok(Some(old)) => attachments
            .into_iter()
            .enumerate()
            .map(|(i, mut a)| {
                if let Some(prev) = old.attachments.get(i) {
                    a.thumbnail_path = a.thumbnail_path.or_else(|| prev.thumbnail_path.clone());
                }
                a
            })
            .collect(),
        _ => attachments,
    }
}

/// Re-signs a message's attachment links by asking Discord for the message
/// again.
///
/// Discord signs CDN links when it serves them, so a fresh read of the same
/// message carries fresh signatures - which is the way in, because the
/// dedicated `/attachments/refresh-urls` endpoint refuses this backend's
/// user-token requests outright (see message_link's doc comment). The
/// messages endpoint used here is the same one history paging already uses,
/// so it is known to work with this token.
///
/// `around` rather than fetching the message by id directly: the single
/// message endpoint is bot-only, while `around` is what a user client uses.
pub async fn refresh_attachments(state: &AppState, buffer_id: &str, message_id: &str) -> Result<Vec<Attachment>> {
    let buffer = state.runtime.get_buffer(buffer_id).context("no such buffer")?;
    let config = state.accounts.get_discord(&buffer.account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this buffer")?;

    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", "1"), ("around", message_id)])
        .header("Authorization", &config.token)
        .send()
        .await
        .context("re-fetching the message")?;
    if !resp.status().is_success() {
        bail!("Discord refused the request ({})", resp.status());
    }
    let messages: Vec<Value> = resp.json().await.context("parsing the re-fetched message")?;
    let message = messages
        .iter()
        .find(|m| m["id"].as_str() == Some(message_id))
        .context("Discord no longer has that message")?;

    let attachments = extract_attachments(message);
    if attachments.is_empty() {
        bail!("that message no longer has any attachments");
    }
    let attachments = merge_cached_thumbnails(state, buffer_id, message_id, attachments);

    state.store.update_message_attachments(buffer_id, message_id, &attachments)?;
    state.events.emit(
        "messageUpdated",
        json!({ "bufferId": buffer_id, "id": message_id, "edited": false, "attachments": attachments.clone() }),
    );
    // Same reasoning as the batch sweep: capture a preview while this link is
    // freshly signed, so the next expiry is not another round-trip.
    cache_thumbnails(state.clone(), buffer_id.to_string(), message_id.to_string(), attachments.clone());
    Ok(attachments)
}

pub(super) fn guild_icon_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("discord-icons")
}

pub async fn sweep_guild_icon_cache() {
    crate::backend::sneedchat::sweep_cache_dir(&guild_icon_cache_dir(), GUILD_ICON_CACHE_MAX_BYTES, "discord guild icon").await;
}

/// Guild icons are small and there are only as many as the user has servers,
/// so this is a much smaller cap than the message thumbnail cache.
pub(super) const GUILD_ICON_CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Where a guild's icon would already be on disk, if it has been fetched.
///
/// Keyed by the icon hash as well as the guild id, so a server changing its
/// icon fetches the new one rather than showing the old one forever.
pub(super) async fn cached_guild_icon(guild_id: &str, icon_hash: Option<&str>) -> Option<String> {
    let hash = icon_hash?;
    let path = guild_icon_cache_dir().join(format!("{guild_id}-{hash}.png"));
    tokio::fs::try_exists(&path).await.unwrap_or(false).then(|| format!("file://{}", path.display()))
}

/// Fetches a guild's icon in the background and re-registers the rail entry
/// once it lands.
///
/// Background rather than inline: this runs while connecting, and a user with
/// thirty servers should not wait on thirty image fetches before any of their
/// channels appear. A guild with no icon set is not an error - the rail draws
/// initials for it, the same as Discord does.
/// `pending` is carried through rather than defaulted. This rebuilds the
/// whole rail entry once the picture lands, so anything it does not know is
/// silently reset - which is exactly what happened to the membership flag:
/// it was set correctly on connect and wiped moments later by the icon
/// arriving.
#[allow(clippy::too_many_arguments)]
pub(super) fn cache_guild_icon(state: AppState, account_id: String, guild_id: String, name: String, icon_hash: Option<String>, position: i64, pending: bool) {
    let Some(hash) = icon_hash else { return };
    tokio::spawn(async move {
        let dir = guild_icon_cache_dir();
        let path = dir.join(format!("{guild_id}-{hash}.png"));
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            // Animated icons have an a_ prefix and are served as .gif; asking
            // for .png yields a still frame of the same thing, which is what a
            // rail wants anyway.
            let url = format!("https://cdn.discordapp.com/icons/{guild_id}/{hash}.png?size=128");
            let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(20), http_client().get(&url).send()).await else {
                tracing::debug!("discord: guild icon fetch for {guild_id} timed out");
                return;
            };
            if !resp.status().is_success() {
                tracing::debug!("discord: guild icon for {guild_id} returned HTTP {}", resp.status());
                return;
            }
            let Ok(bytes) = resp.bytes().await else { return };
            if tokio::fs::create_dir_all(&dir).await.is_err() || tokio::fs::write(&path, &bytes).await.is_err() {
                return;
            }
        }
        state.runtime.upsert_buffer_group(
            &state,
            crate::model::BufferGroup {
                id: guild_group_id(&account_id, &guild_id),
                account_id,
                service: "discord".to_string(),
                kind: "guild".to_string(),
                name,
                icon_url: Some(format!("file://{}", path.display())),
                position,
                pending,
            },
        );
    });
}

/// Where cached Discord thumbnails live. Every CDN link Discord serves is
/// signed and lapses roughly a day later, so a message read back out of
/// scrollback after that has a URL that no longer loads. A small local copy
/// taken while the link still works is what lets an old message still show
/// its picture rather than a dead box.
pub(super) fn thumbnail_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("discord-thumbnails")
}

/// Deliberately smaller than the attachment caches: these are previews, not
/// the originals, and the full-size image is always one refresh away.
pub(super) const THUMBNAIL_CACHE_MAX_BYTES: u64 = 100 * 1024 * 1024;

pub async fn sweep_thumbnail_cache() {
    crate::backend::sneedchat::sweep_cache_dir(&thumbnail_cache_dir(), THUMBNAIL_CACHE_MAX_BYTES, "discord thumbnail").await;
}

/// Throw away the previews taken before animation was asked for.
///
/// Every one of them is a still, and nothing would otherwise replace it: a
/// message keeps the path it was given, and a preview is only taken for an
/// attachment that has none. Deleting the files is what retires them - a path
/// to a file that is gone is dropped when the message is read, which puts the
/// picture back to its original until a fresh preview is taken.
///
/// Once, marked by a file in the directory it clears. A preview costs a
/// round-trip, so throwing away good ones on every start would be a poor trade
/// for a one-time correction.
pub async fn retire_still_thumbnails() {
    let dir = thumbnail_cache_dir();
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return;
    }
    let marker = dir.join(".animated");
    if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
        return;
    }

    let mut cleared = 0usize;
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path == marker {
                continue;
            }
            if tokio::fs::remove_file(&path).await.is_ok() {
                cleared += 1;
            }
        }
    }
    // The marker goes down even if nothing was there to clear, so an empty
    // cache is not re-examined on every start.
    if let Err(e) = tokio::fs::write(&marker, b"").await {
        tracing::debug!("discord: marking thumbnails retired: {e}");
        return;
    }
    if cleared > 0 {
        tracing::info!("discord: retired {cleared} still thumbnail(s); they will be taken again animated");
    }
}

/// The width to ask Discord's media proxy for. Big enough to look right in a
/// message list at any sane window size, small enough that caching one per
/// image is cheap.
pub(super) const THUMBNAIL_WIDTH: u32 = 480;

/// Rewrites a CDN link into a resized one through Discord's media proxy,
/// which is the same trick its own clients use for previews. Returns None for
/// anything that isn't a Discord-hosted image, so nothing else gets proxied.
pub(super) fn thumbnail_source(url: &str, width: u32, height: u32) -> Option<String> {
    if !url.starts_with("https://cdn.discordapp.com/") && !url.starts_with("https://media.discordapp.net/") {
        return None;
    }
    let proxied = url.replacen("https://cdn.discordapp.com/", "https://media.discordapp.net/", 1);
    // Preserve the aspect ratio: asking for a square would letterbox it.
    let (w, h) = if width == 0 || height == 0 {
        (THUMBNAIL_WIDTH, THUMBNAIL_WIDTH)
    } else if width >= height {
        (THUMBNAIL_WIDTH, (height * THUMBNAIL_WIDTH / width).max(1))
    } else {
        ((width * THUMBNAIL_WIDTH / height).max(1), THUMBNAIL_WIDTH)
    };
    // animated=true, or the proxy hands back a still. Resizing flattens
    // animation by default, which is why an animated webp sat motionless
    // inline and only moved once the expanded view loaded the original -
    // that view asks for the file itself and so never lost the animation.
    //
    // Costs nothing on a picture that does not move: measured against this
    // proxy, a static png came back byte-for-byte identical with and without
    // it, while an animated webp went from 10KB to 800KB - which is the
    // animation, and still a good deal less than the 1.3MB original the
    // expanded view pulls.
    let sep = if proxied.contains('?') { '&' } else { '?' };
    Some(format!("{proxied}{sep}width={w}&height={h}&animated=true"))
}

/// The `ex=` query parameter is the link's expiry, as a hex unix timestamp.
/// Reading it lets a stale link be recognised before it is requested, rather
/// than after a failed load.
pub(super) fn attachment_expired(url: &str) -> bool {
    let Some(ex) = url.split(['?', '&']).find_map(|p| p.strip_prefix("ex=")) else { return false };
    let Ok(expiry) = u64::from_str_radix(ex, 16) else { return false };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now >= expiry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attachment, Message};

    fn msg(id: &str, urls: &[&str]) -> Message {
        Message {
            id: id.into(),
            buffer_id: "b".into(),
            html: None,
            components: Vec::new(),
            sender_color: None,
            badges: Vec::new(),
            from: "x".into(),
            body: String::new(),
            ts: 0,
            is_action: false,
            is_highlight: false,
            kind: "chat".into(),
            reply_to: None,
            edited: false,
            reactions: Vec::new(),
            is_own: false,
            avatar_url: None,
            embeds: Vec::new(),
            attachments: urls
                .iter()
                .map(|u| Attachment {
                    kind: "image".into(),
                    url: Some((*u).to_string()),
                    ..Default::default()
                })
                .collect(),
            sender_id: None,
        }
    }

    const PAST: &str = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=386d4380&is=1&hm=2";

    const FUTURE: &str = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=f4143f80&is=1&hm=2";

    #[test]
    fn only_messages_with_a_lapsed_link_need_re_signing() {
        let page = vec![msg("100", &[FUTURE]), msg("101", &[PAST]), msg("102", &[])];
        assert_eq!(stale_message_ids(&page), vec!["101"]);
    }

    #[test]
    fn a_message_is_stale_if_any_of_its_attachments_is() {
        let page = vec![msg("100", &[FUTURE, PAST])];
        assert_eq!(stale_message_ids(&page), vec!["100"]);
    }

    #[test]
    fn stale_ids_come_back_newest_first() {
        // Snowflakes: longer is newer, and equal lengths sort lexically.
        let page = vec![msg("100", &[PAST]), msg("1000", &[PAST]), msg("300", &[PAST])];
        assert_eq!(stale_message_ids(&page), vec!["1000", "300", "100"]);
    }

    #[test]
    fn locally_cached_media_never_looks_stale() {
        // Matrix and Sneedchat attachments carry a path, not a url, so a
        // non-Discord buffer costs nothing here.
        let mut m = msg("100", &[]);
        m.attachments = vec![Attachment {
            kind: "image".into(),
            path: Some("file:///cache/x.png".into()),
            ..Default::default()
        }];
        assert!(stale_message_ids(&[m]).is_empty());
    }

    #[test]
    fn reads_the_expiry_out_of_a_signed_cdn_link() {
        // ex= is a hex unix timestamp. Year 2000 is long gone; year 2100 is not.
        let past = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=386d4380&is=1&hm=2";
        let future = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=f4143f80&is=1&hm=2";
        assert!(attachment_expired(past));
        assert!(!attachment_expired(future));
        // An unsigned link has no expiry to read, so it is never "expired".
        assert!(!attachment_expired("https://example.com/a.png"));
    }

    #[test]
    fn builds_a_proxied_thumbnail_preserving_aspect_ratio() {
        let src = thumbnail_source("https://cdn.discordapp.com/attachments/1/2/a.png?ex=1", 1000, 500)
            .expect("should proxy a Discord link");
        // Resizing goes through the media proxy, not the raw CDN host.
        assert!(src.starts_with("https://media.discordapp.net/"), "{src}");
        assert!(src.contains("width=480"), "{src}");
        assert!(src.contains("height=240"), "{src}");
        // The existing query string is kept, not replaced.
        assert!(src.contains("ex=1"), "{src}");
        // Without this the proxy flattens the animation while resizing, so
        // anything that moves sat still inline and only moved when expanded.
        assert!(src.contains("animated=true"), "{src}");
    }

    #[test]
    fn taller_than_wide_is_bounded_by_height() {
        let src = thumbnail_source("https://media.discordapp.net/attachments/1/2/a.png", 500, 1000).unwrap();
        assert!(src.contains("width=240"), "{src}");
        assert!(src.contains("height=480"), "{src}");
    }

    #[test]
    fn only_discord_hosted_images_are_proxied() {
        assert!(thumbnail_source("https://example.com/a.png", 10, 10).is_none());
    }
}
