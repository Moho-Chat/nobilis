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

    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), anonymous_client().get(src).send())
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

/// Buffers with a re-sign running, and the messages queued behind it.
///
/// One sweep per buffer at a time, so a burst of getBacklog calls (opening,
/// scrolling, opening again) does not fan out against a rate-limited
/// endpoint. What arrives while one runs is queued for it rather than dropped:
/// dropping was what left a page scrolled to mid-sweep with dead links for
/// good, because nothing would ask for it again.
pub(super) fn resigning() -> &'static std::sync::Mutex<std::collections::HashMap<String, Waiting>> {
    static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Waiting>>> =
        std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// What is queued behind a buffer's running re-sign.
#[derive(Default)]
pub(super) struct Waiting {
    ids: Vec<String>,
    /// Whether the window named any of them, which is what decides how far
    /// the next round will go to reach them.
    named: bool,
}

/// Messages a sweep already asked about and got nothing new for: a link
/// somebody typed that Discord will not sign again, a message since deleted.
/// Asking again on every page read would cost a request each time and change
/// nothing, so a sweep leaves them alone until the daemon next starts. What
/// the window names is asked for regardless.
fn given_up() -> &'static std::sync::Mutex<std::collections::HashSet<(String, String)>> {
    static GIVEN_UP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<(String, String)>>> =
        std::sync::OnceLock::new();
    GIVEN_UP.get_or_init(Default::default)
}

/// How many history pages one sweep will pull. Each covers ~50 messages, so
/// this reaches a few screenfuls; past that the remaining stale messages are
/// left for the window to name as they come on screen, rather than hammering
/// a rate-limited endpoint on every buffer open.
pub(super) const MAX_RESIGN_FETCHES: usize = 3;

/// How many pages a round goes to for messages the window named. Those are
/// on screen with a dead picture, so they are reached however spread out they
/// are - short only of a runaway, since every request is paced as it is.
pub(super) const MAX_NAMED_FETCHES: usize = 24;

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
    // Somebody pasting a Discord link into another service's room is not
    // something this account can ask Discord about.
    if !buffer_id.starts_with("discord:") {
        return;
    }
    let mut ids = stale_message_ids(messages);
    {
        let tried = given_up().lock().unwrap();
        ids.retain(|id| !tried.contains(&(buffer_id.clone(), id.clone())));
    }
    queue_resign(state, buffer_id, ids, false);
}

/// Re-signs these messages' attachments, batched with whatever else this
/// buffer has waiting.
///
/// The window names messages itself when a picture it is showing would not
/// load - a link that lapsed between the page being read and being drawn, or
/// one that failed for a reason its expiry does not show.
pub fn resign_messages(state: AppState, buffer_id: String, ids: Vec<String>) {
    queue_resign(state, buffer_id, ids, true);
}

fn queue_resign(state: AppState, buffer_id: String, ids: Vec<String>, named: bool) {
    if ids.is_empty() {
        return;
    }
    {
        let mut all = resigning().lock().unwrap();
        if let Some(queued) = all.get_mut(&buffer_id) {
            for id in ids {
                if !queued.ids.contains(&id) {
                    queued.ids.push(id);
                }
            }
            queued.named |= named;
            return;
        }
        all.insert(buffer_id.clone(), Waiting::default());
    }

    tokio::spawn(async move {
        let mut batch = ids;
        let mut named = named;
        loop {
            sort_newest_first(&mut batch);
            let pages = if named { MAX_NAMED_FETCHES } else { MAX_RESIGN_FETCHES };
            if let Err(e) = run_resign(&state, &buffer_id, batch, pages).await {
                tracing::debug!("discord: re-signing {buffer_id}: {e}");
            }
            // Whatever queued up meanwhile is the next batch; the entry goes
            // only once there is nothing left, under the same lock a caller
            // queues under, so nothing can be added to a sweep that has
            // already finished.
            let mut all = resigning().lock().unwrap();
            match all.get_mut(&buffer_id) {
                Some(queued) if !queued.ids.is_empty() => {
                    batch = std::mem::take(&mut queued.ids);
                    named = std::mem::take(&mut queued.named);
                }
                _ => {
                    all.remove(&buffer_id);
                    break;
                }
            }
        }
    });
}

/// Newest first, which is most likely what is on screen. Discord ids are
/// snowflakes: lexicographically ordered for equal length, and longer is newer.
fn sort_newest_first(ids: &mut [String]) {
    ids.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| b.cmp(a)));
}

/// Which messages in a page carry a lapsed link, newest first - in their
/// files, in a card under them, or in what was typed.
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
                || stored_in_old_shape(m)
                || cdn_links(&m.body).any(attachment_expired)
                || m.embeds.iter().any(|e| embed_links(e).any(|u| is_cdn_link(u) && attachment_expired(u)))
        })
        .map(|m| m.id.clone())
        .collect();
    sort_newest_first(&mut ids);
    ids
}

/// Whether a message was stored before attachments were kept as a list
/// (nobilis 9351109, 21 August 2026), when they were appended to the text as
/// links. Nothing re-signs a link in the text, so these stayed dead.
///
/// A candidate only: somebody pasting a file link into what they wrote looks
/// the same from here. Which links really are the message's own attachments
/// is settled against Discord's copy of it - see `strip_attachment_links`.
///
/// Also a Lottie sticker stored before it could be drawn, as the line
/// "sent a sticker: <name>".
pub(super) fn stored_in_old_shape(m: &crate::model::Message) -> bool {
    m.attachments.is_empty()
        && (m.body.contains("cdn.discordapp.com/attachments/")
            || m.body.contains("media.discordapp.net/attachments/")
            || m.body.lines().any(|l| l.starts_with(STICKER_LINE)))
}

const STICKER_LINE: &str = "sent a sticker: ";

/// A link without its signature, which is what stays the same when Discord
/// signs it again - and on either of its two hosts.
fn unsigned(url: &str) -> String {
    url.split('?').next().unwrap_or(url).replace("media.discordapp.net", "cdn.discordapp.com")
}

/// The two hosts Discord serves files from. The same file is on both.
const CDN_HOSTS: [&str; 2] = ["https://cdn.discordapp.com/", "https://media.discordapp.net/"];

fn is_cdn_link(url: &str) -> bool {
    CDN_HOSTS.iter().any(|host| url.starts_with(host))
}

/// Where each of Discord's file links sits in a piece of text: from the host
/// to whatever ends a link in prose or in markdown.
fn cdn_link_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let Some(start) = CDN_HOSTS.iter().filter_map(|host| rest.find(host)).min() else { break };
        let link = &rest[start..];
        let len = link
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '>' | '<' | '"' | '\'' | '|' | ']'))
            .unwrap_or(link.len());
        out.push(at + start..at + start + len);
        at += start + len;
    }
    out
}

fn cdn_links(text: &str) -> impl Iterator<Item = &str> {
    cdn_link_ranges(text).into_iter().map(move |range| &text[range])
}

/// The addresses a card carries that a window might fetch.
fn embed_links(embed: &Embed) -> impl Iterator<Item = &str> {
    [&embed.media_url, &embed.image_url, &embed.url].into_iter().filter_map(|u| u.as_deref())
}

/// A link without its signature, host kept: what a stored link is matched to
/// its freshly signed self by.
fn bare(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Every signed file link anywhere in a message as Discord just sent it, by
/// what it is a link to.
///
/// Anywhere, rather than field by field: Discord signs a link wherever it
/// appears - an attachment, a card's picture or video, a forward's files, a
/// link typed into the text - and a list of the places would be one short the
/// next time it adds one.
pub(super) fn fresh_links(message: &Value) -> std::collections::HashMap<String, String> {
    fn walk(value: &Value, out: &mut std::collections::HashMap<String, String>) {
        match value {
            Value::String(text) => {
                for link in cdn_links(text) {
                    if link.contains("ex=") && !attachment_expired(link) {
                        out.insert(bare(link).to_string(), link.to_string());
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            Value::Object(map) => map.values().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = std::collections::HashMap::new();
    walk(message, &mut out);
    out
}

/// The text with each lapsed link in it swapped for its fresh one, or nothing
/// if there was none to swap. Everything that is not such a link is left
/// exactly as stored.
pub(super) fn resign_text(text: &str, fresh: &std::collections::HashMap<String, String>) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut changed = false;
    for range in cdn_link_ranges(text) {
        let link = &text[range.clone()];
        out.push_str(&text[at..range.start]);
        match fresh.get(bare(link)) {
            Some(new) if new != link => {
                out.push_str(new);
                changed = true;
            }
            _ => out.push_str(link),
        }
        at = range.end;
    }
    out.push_str(&text[at..]);
    changed.then_some(out)
}

/// The same for the cards under a message. Says whether any changed.
fn resign_embeds(embeds: &mut [Embed], fresh: &std::collections::HashMap<String, String>) -> bool {
    let mut changed = false;
    for embed in embeds {
        for field in [&mut embed.media_url, &mut embed.image_url, &mut embed.url] {
            let Some(old) = field.as_deref().filter(|u| is_cdn_link(u)) else { continue };
            if let Some(new) = fresh.get(bare(old)).filter(|new| new.as_str() != old) {
                *field = Some(new.clone());
                changed = true;
            }
        }
    }
    changed
}

/// Puts the fresh links from Discord's copy of a message into the stored one,
/// and tells the windows. Says whether anything changed, and what the
/// message's files now are.
///
/// Its files are replaced whole, since the list is rebuilt from the message
/// the same way it was when it arrived. Its text and cards are not rebuilt -
/// names resolved into the text and cards described after the fact would be
/// lost - so there only the links change.
fn store_fresh(state: &AppState, buffer_id: &str, id: &str, message: &Value) -> (bool, Vec<Attachment>) {
    let Some(stored) = state.store.get_message(buffer_id, id).ok().flatten() else { return (false, Vec::new()) };
    let attachments = merge_cached_thumbnails(state, buffer_id, id, all_attachments(message));
    let fresh = fresh_links(message);
    let mut update = serde_json::Map::new();
    let mut body = stored.body.clone();

    if !attachments.is_empty() {
        // Stored in the old shape: the links come out of the text as the
        // list goes in, or the picture would show twice - once as itself
        // and once as a dead link under it.
        if stored_in_old_shape(&stored) {
            body = strip_attachment_links(&stored.body, &attachments);
            if state.store.repair_message_media(buffer_id, id, &body, &attachments).unwrap_or(false) {
                update.insert("body".into(), json!(body));
                update.insert("attachments".into(), json!(attachments));
            }
        } else if state.store.update_message_attachments(buffer_id, id, &attachments).unwrap_or(false) {
            update.insert("attachments".into(), json!(attachments));
        }
    }
    if let Some(signed) = resign_text(&body, &fresh) {
        if state.store.update_message_body_silent(buffer_id, id, &signed).unwrap_or(false) {
            update.insert("body".into(), json!(signed));
        }
    }
    let mut embeds = stored.embeds.clone();
    if resign_embeds(&mut embeds, &fresh) && state.store.set_message_embeds(buffer_id, id, &embeds).unwrap_or(false) {
        update.insert("embeds".into(), json!(embeds));
    }

    if update.is_empty() {
        return (false, attachments);
    }
    update.insert("bufferId".into(), json!(buffer_id));
    update.insert("id".into(), json!(id));
    update.insert("edited".into(), json!(false));
    state.events.emit("messageUpdated", Value::Object(update));
    // The links are valid again right now, and this message reached a re-sign
    // because its preview was needed - so take one while it can still be
    // fetched, and the next expiry has something to show.
    cache_thumbnails(state.clone(), buffer_id.to_string(), id.to_string(), attachments.clone());
    (true, attachments)
}

/// The text with the links that are really these attachments taken out, and
/// whatever else it said left exactly as stored.
pub(super) fn strip_attachment_links(body: &str, attachments: &[Attachment]) -> String {
    let own: Vec<String> = attachments.iter().filter_map(|a| a.url.as_deref()).map(unsigned).collect();
    // The names of the Lottie stickers it carries, whose old stand-in line
    // goes now that they can be drawn.
    let stickers: Vec<String> = attachments
        .iter()
        .filter(|a| a.kind == "lottie")
        .filter_map(|a| a.filename.as_deref())
        .map(|f| f.trim_end_matches(".json").to_string())
        .collect();
    let kept: Vec<String> = body
        .lines()
        .filter(|line| !line.strip_prefix(STICKER_LINE).is_some_and(|name| stickers.iter().any(|s| s == name)))
        .filter_map(|line| {
            let left = line
                .split(' ')
                .filter(|word| !(word.starts_with("http") && own.contains(&unsigned(word))))
                .collect::<Vec<_>>()
                .join(" ");
            // A line that was only the link goes with it; a blank line the
            // sender wrote stays.
            (line.trim().is_empty() || !left.trim().is_empty()).then_some(left)
        })
        .collect();
    kept.join("\n").trim().to_string()
}

pub(super) async fn run_resign(state: &AppState, buffer_id: &str, mut stale: Vec<String>, pages: usize) -> Result<()> {
    let buffer = state.runtime.get_buffer(buffer_id).context("no such buffer")?;
    let config = state.accounts.get_discord(&buffer.account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel")?;

    for _ in 0..pages {
        let Some(anchor) = stale.first().cloned() else { break };
        // `around` centres the page on the message, so one fetch covers what
        // sits either side of it - usually the rest of the same screenful.
        let resp = http_client_for(&config.token)
            .get(format!("{API_BASE}/channels/{channel_id}/messages"))
            .query(&[("limit", "50"), ("around", anchor.as_str())])
            .header("Authorization", &config.token)
            .send_paced()
            .await
            .context("fetching a page to re-sign")?;
        if !resp.status().is_success() {
            bail!("Discord refused the request ({})", resp.status());
        }
        let page: Vec<Value> = resp.json().await.context("parsing the re-signed page")?;
        if page.is_empty() {
            break;
        }

        let mut unchanged: Vec<String> = Vec::new();
        for message in &page {
            let Some(id) = message["id"].as_str() else { continue };
            if !stale.iter().any(|s| s == id) {
                continue;
            }
            if !store_fresh(state, buffer_id, id, message).0 {
                unchanged.push(id.to_string());
            }
        }

        let covered: std::collections::HashSet<&str> = page.iter().filter_map(|m| m["id"].as_str()).collect();
        // What came back with nothing new has nothing new to give, and an
        // anchor that did not come back at all was deleted. Neither is worth
        // a sweep asking about again.
        {
            let mut tried = given_up().lock().unwrap();
            if !covered.contains(anchor.as_str()) {
                unchanged.push(anchor.clone());
            }
            tried.extend(unchanged.into_iter().map(|id| (buffer_id.to_string(), id)));
        }
        // Drop everything this page spanned, not just what it re-signed, and
        // the anchor whether or not it came back - without that the loop
        // would fetch the same page again.
        stale.retain(|id| id != &anchor && !covered.contains(id.as_str()));
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

    let resp = http_client_for(&config.token)
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", "1"), ("around", message_id)])
        .header("Authorization", &config.token)
        .send_paced()
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

    let (changed, attachments) = store_fresh(state, buffer_id, message_id, message);
    if !changed && attachments.is_empty() {
        bail!("Discord has no fresh link for that message");
    }
    Ok(attachments)
}

pub(super) fn guild_icon_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("discord-icons")
}

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
            let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(20), http_client_for(&account_id).get(&url).send()).await else {
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
///
/// "Expired" includes the last ten minutes before it lapses: a page read now
/// is looked at for a while, and a link that dies while it is on screen is the
/// picture that goes blank for no visible reason.
pub(super) fn attachment_expired(url: &str) -> bool {
    expires_within(url, EXPIRY_MARGIN_SECS)
}

const EXPIRY_MARGIN_SECS: u64 = 600;

fn expires_within(url: &str, margin: u64) -> bool {
    let Some(ex) = url.split(['?', '&']).find_map(|p| p.strip_prefix("ex=")) else { return false };
    let Ok(expiry) = u64::from_str_radix(ex, 16) else { return false };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now + margin >= expiry
}

/// Fetches a cached thumbnail again, after it expired or was cleared.
///
/// The file is named by a hash of its picture's URL, so only the message says
/// which picture it was. Its link may have expired as well - Discord's do, in
/// a day - in which case the message is re-fetched for a freshly signed one
/// first, as a lapsed link already is.
pub async fn restore_thumbnail(state: &AppState, path: &std::path::Path, buffer_id: &str, message_id: &str) -> Result<std::path::PathBuf> {
    let wanted = format!("file://{}", path.display());
    let find = |attachments: &[Attachment]| {
        attachments
            .iter()
            .find(|a| a.thumbnail_path.as_deref() == Some(wanted.as_str()))
            .and_then(|a| Some((a.url.clone()?, a.width.unwrap_or(0), a.height.unwrap_or(0))))
    };
    let message = state.store.get_message(buffer_id, message_id)?.context("that message is not stored here")?;
    let (url, width, height) = find(&message.attachments).context("that message has no such picture")?;
    let src = thumbnail_source(&url, width, height).context("that picture is not on Discord's CDN")?;
    if fetch_thumbnail(&src, &url).await.is_some() {
        return Ok(path.to_path_buf());
    }
    let fresh = refresh_attachments(state, buffer_id, message_id).await?;
    let (url, width, height) = find(&fresh).context("that message no longer has the picture")?;
    let src = thumbnail_source(&url, width, height).context("that picture is not on Discord's CDN")?;
    fetch_thumbnail(&src, &url).await.context("Discord did not send the picture")?;
    Ok(path.to_path_buf())
}

/// Fetches a cached server icon again. Its name is the server and the icon's
/// hash, which is the whole of its CDN address.
pub async fn restore_guild_icon(path: &std::path::Path) -> Result<std::path::PathBuf> {
    let name = path.file_stem().and_then(|n| n.to_str()).context("not a cached icon")?;
    let (guild_id, hash) = name.split_once('-').context("not a cached icon")?;
    if !guild_id.bytes().all(|b| b.is_ascii_digit()) || !hash.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        bail!("not a cached icon");
    }
    let url = format!("https://cdn.discordapp.com/icons/{guild_id}/{hash}.png?size=128");
    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), anonymous_client().get(&url).send())
        .await
        .context("Discord took too long")??;
    if !resp.status().is_success() {
        bail!("Discord answered {} for that icon", resp.status());
    }
    let bytes = resp.bytes().await?;
    tokio::fs::create_dir_all(guild_icon_cache_dir()).await?;
    tokio::fs::write(path, &bytes).await?;
    Ok(path.to_path_buf())
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
            moderated: None,
            delivery: None,
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
            edited_ts: None,
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

    /// A link a few minutes from lapsing is re-signed now, not after it has
    /// gone blank on screen.
    #[test]
    fn a_link_about_to_lapse_counts_as_lapsed() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let link = |at: u64| format!("https://cdn.discordapp.com/attachments/1/2/a.png?ex={at:x}&is=1&hm=2");
        assert!(attachment_expired(&link(now + 120)));
        assert!(!attachment_expired(&link(now + 3600)));
    }

    /// Only the links that are the message's own attachments come out; a
    /// link somebody wrote stays, and so does everything else they wrote.
    #[test]
    fn old_attachment_links_come_out_of_the_text() {
        let own = Attachment {
            url: Some("https://cdn.discordapp.com/attachments/1/2/shot.png?ex=aa&is=bb&hm=cc".into()),
            ..Default::default()
        };
        let body = "look at this\nhttps://cdn.discordapp.com/attachments/1/2/shot.png?ex=11&is=22&hm=33\nand https://cdn.discordapp.com/attachments/9/9/other.png?ex=1";
        assert_eq!(
            strip_attachment_links(body, std::slice::from_ref(&own)),
            "look at this\nand https://cdn.discordapp.com/attachments/9/9/other.png?ex=1"
        );
        // The media host is the same file.
        assert_eq!(strip_attachment_links("https://media.discordapp.net/attachments/1/2/shot.png", &[own]), "");
    }

    #[test]
    fn a_sticker_stand_in_goes_once_the_sticker_can_be_drawn() {
        let sticker = Attachment { kind: "lottie".into(), filename: Some("wave.json".into()), ..Default::default() };
        assert_eq!(strip_attachment_links("sent a sticker: wave", std::slice::from_ref(&sticker)), "");
        // Somebody else's sticker named in passing stays.
        assert_eq!(strip_attachment_links("sent a sticker: dance", &[sticker]), "sent a sticker: dance");
    }

    #[test]
    fn a_message_with_a_link_and_no_list_is_a_candidate() {
        let mut m = msg("5", &[]);
        m.body = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=1".into();
        assert_eq!(stale_message_ids(&[m]), ["5"]);
        let mut plain = msg("6", &[]);
        plain.body = "no links here".into();
        assert!(stale_message_ids(&[plain]).is_empty());
    }

    #[test]
    fn newest_messages_are_re_signed_first() {
        let mut ids = vec!["99".to_string(), "1000".to_string(), "100".to_string()];
        sort_newest_first(&mut ids);
        assert_eq!(ids, ["1000", "100", "99"]);
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

    /// A forward's files are inside what it forwards, and a sticker is beside
    /// the attachments: a re-sign that read only `attachments` found neither.
    #[test]
    fn a_forward_and_a_sticker_are_among_a_messages_files() {
        let forward = json!({
            "attachments": [],
            "message_snapshots": [{ "message": { "attachments": [
                { "url": FUTURE, "content_type": "image/jpeg", "filename": "a.jpg" },
                { "url": FUTURE, "content_type": "image/jpeg", "filename": "b.jpg" }
            ] } }]
        });
        let files = all_attachments(&forward);
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].filename.as_deref(), Some("b.jpg"));
    }

    #[test]
    fn fresh_links_are_found_wherever_discord_put_them() {
        let message = json!({
            "content": format!("see {FUTURE} there"),
            "embeds": [{ "video": { "proxy_url": "https://media.discordapp.net/attachments/7/8/v.mp4?ex=f4143f80&is=1&hm=9" } }],
            "message_snapshots": [{ "message": { "attachments": [{ "url": "https://cdn.discordapp.com/attachments/3/4/f.png?ex=f4143f80&is=1&hm=5" }] } }]
        });
        let fresh = fresh_links(&message);
        assert_eq!(fresh.get("https://cdn.discordapp.com/attachments/1/2/a.png").map(String::as_str), Some(FUTURE));
        assert!(fresh.contains_key("https://media.discordapp.net/attachments/7/8/v.mp4"));
        assert!(fresh.contains_key("https://cdn.discordapp.com/attachments/3/4/f.png"));
        // A link that came back already lapsed is no improvement on the stored one.
        assert!(fresh_links(&json!({ "content": PAST })).is_empty());
    }

    #[test]
    fn a_typed_link_is_signed_again_where_it_stands() {
        let fresh = fresh_links(&json!({ "content": FUTURE }));
        let body = format!("before [shot]({PAST}) and <{PAST}> after\nhttps://example.com/x.png?ex=1");
        let signed = resign_text(&body, &fresh).expect("the lapsed links are swapped");
        assert_eq!(signed, format!("before [shot]({FUTURE}) and <{FUTURE}> after\nhttps://example.com/x.png?ex=1"));
        // Nothing to swap is said as nothing, so nothing is written.
        assert!(resign_text("no links", &fresh).is_none());
        assert!(resign_text(FUTURE, &fresh).is_none());
        assert!(resign_text("https://cdn.discordapp.com/attachments/9/9/gone.png?ex=386d4380", &fresh).is_none());
    }

    #[test]
    fn a_lapsed_link_in_the_text_or_on_a_card_needs_re_signing() {
        let mut typed = msg("7", &[]);
        typed.body = format!("look {PAST}");
        // Not the old shape's catch-all: this one also has a file of its own.
        typed.attachments = msg("x", &[FUTURE]).attachments;
        let mut card = msg("8", &[]);
        card.embeds = vec![Embed { media_url: Some(PAST.into()), ..Default::default() }];
        let mut fine = msg("9", &[FUTURE]);
        fine.body = format!("look {FUTURE}");
        fine.embeds = vec![Embed { media_url: Some("https://images-ext-1.discordapp.net/x.mp4".into()), ..Default::default() }];
        assert_eq!(stale_message_ids(&[typed, card, fine]), ["8", "7"]);
    }

    #[test]
    fn a_cards_links_are_signed_again() {
        let fresh = fresh_links(&json!({ "embeds": [{ "video": { "proxy_url": FUTURE } }] }));
        let mut embeds = vec![Embed { media_url: Some(PAST.into()), url: Some("https://example.com".into()), ..Default::default() }];
        assert!(resign_embeds(&mut embeds, &fresh));
        assert_eq!(embeds[0].media_url.as_deref(), Some(FUTURE));
        assert_eq!(embeds[0].url.as_deref(), Some("https://example.com"));
        assert!(!resign_embeds(&mut embeds, &fresh));
    }
}
