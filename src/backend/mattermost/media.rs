//! Avatars, team icons, files and custom emoji: named in the cache when a post or a list mentions them, and fetched
//! when the window looks.
//!
//! Mattermost serves none of these to a stranger: every one is asked for with the account's session. So the cache
//! records, beside each name, the path on the server and the account it belongs to, and fetching it again goes through
//! that account's client (and so its route).
//!
//! A picture a link preview names lives on another site; it is asked for through the server's own image address
//! (`/api/v4/image`), which goes through the server's image proxy where it has one turned on, as Mattermost's app does.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const MEDIA: &str = "mattermost-media";
pub const AVATARS: &str = "mattermost-avatars";

/// A stable short name for an address (FNV-1a), so the same file is the same cache entry on every run.
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
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        _ => "png",
    }
    .to_string()
}

/// Where a file on the server is (or will be) in the cache, as the `file://` address the window loads. `path` is the
/// server's own (`/api/v4/files/<id>`), recorded so the file can be fetched when it is not there.
pub fn cached(account_id: &str, dir: &str, path: &str, name: Option<&str>, content_type: Option<&str>) -> String {
    let file = crate::media_cache::dir(dir).join(format!("{}.{}", hash(&format!("{account_id}{path}")), extension_of(name, content_type)));
    crate::media_cache::record(&file, json!({ "path": path, "account": account_id }));
    format!("file://{}", file.display())
}

/// A person's picture.
pub fn avatar(account_id: &str, user: &str, updated: i64) -> String {
    cached(account_id, AVATARS, &format!("/api/v4/users/{user}/image?_={updated}"), None, Some("image/png"))
}

/// A team's icon, where it has one.
pub fn team_icon(account_id: &str, team: &str, updated: i64) -> Option<String> {
    (updated > 0).then(|| cached(account_id, AVATARS, &format!("/api/v4/teams/{team}/image?_={updated}"), None, Some("image/png")))
}

/// A custom emoji's picture.
pub fn emoji(account_id: &str, id: &str) -> String {
    cached(account_id, MEDIA, &format!("/api/v4/emoji/{id}/image"), None, Some("image/png"))
}

/// The server's address for a picture on another site.
pub fn external_path(url: &str) -> Option<String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return None;
    }
    let mut path = url::Url::parse("http://x/api/v4/image").ok()?;
    path.query_pairs_mut().append_pair("url", url);
    Some(format!("{}?{}", path.path(), path.query().unwrap_or("")))
}

/// A picture on another site, through the server's image address.
pub fn external(account_id: &str, url: &str) -> Option<String> {
    Some(cached(account_id, MEDIA, &external_path(url)?, None, None))
}

/// Fetches a cached file again, from the path recorded for it, with its account's session.
pub async fn restore_cached(state: &crate::state::AppState, file: &Path) -> Result<PathBuf> {
    let source = crate::media_cache::source(file).ok_or_else(|| anyhow!("nothing says where this file came from"))?;
    let path = source["path"].as_str().ok_or_else(|| anyhow!("no address was recorded for this file"))?;
    if !path.starts_with("/api/v4/") {
        bail!("not a Mattermost address");
    }
    let account = source["account"].as_str().unwrap_or("");
    let handle = state.runtime.mattermost_handle(account).ok_or_else(|| anyhow!("that Mattermost account is not connected"))?;
    let (bytes, _) = handle.api.bytes(path).await.map_err(|e| anyhow!("{e}"))?;
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = file.with_extension("part");
    tokio::fs::write(&part, &bytes).await?;
    tokio::fs::rename(&part, file).await.with_context(|| format!("writing {}", file.display()))?;
    Ok(file.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_file_of_the_same_account_is_the_same_name() {
        let a = cached("mattermost:h:u", MEDIA, "/api/v4/files/abc", Some("x.JPG"), None);
        assert_eq!(a, cached("mattermost:h:u", MEDIA, "/api/v4/files/abc", Some("x.JPG"), None));
        assert_ne!(a, cached("mattermost:h2:u", MEDIA, "/api/v4/files/abc", Some("x.JPG"), None), "another server's file of the same id is another file");
        assert!(a.starts_with("file://") && a.ends_with(".jpg") && a.contains("/mattermost-media/"), "{a}");
    }

    #[test]
    fn a_picture_elsewhere_goes_through_the_servers_image_address() {
        assert_eq!(external_path("https://site.example/a b.png").as_deref(), Some("/api/v4/image?url=https%3A%2F%2Fsite.example%2Fa+b.png"));
        assert!(external("mattermost:h:u", "javascript:alert(1)").is_none());
    }

    #[test]
    fn a_team_with_no_icon_has_none() {
        assert!(team_icon("mattermost:h:u", "t", 0).is_none());
        assert!(team_icon("mattermost:h:u", "t", 5).is_some());
    }
}
