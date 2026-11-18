//! Avatars, server icons, pictures and files: named in the cache when a message or a list mentions them, and fetched
//! when the window looks.
//!
//! Fluxer serves files from its media proxy (the address is in the discovery document) to
//! anybody with the address, so nothing here needs the token. A file is named in the cache by its address, and the
//! address is recorded against the name: when the window finds the file missing - not fetched yet, expired, cleared -
//! it asks for it again, and the address is what says where from.

use super::discovery::Instance;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const MEDIA: &str = "fluxer-media";
pub const AVATARS: &str = "fluxer-avatars";

/// The address of an image asset on the media proxy (`media-proxy/routes.mdx`): `kind` is `avatars`, `icons`, `banners`
/// and the like, `owner` the user or guild it belongs to, `hash` the hash the object carries. An `a_` hash is animated.
pub fn asset_url(instance: &Instance, kind: &str, owner: &str, hash: &str) -> Option<String> {
    if instance.media.is_empty() || !safe(owner) || !safe(hash) {
        return None;
    }
    let ext = if hash.starts_with("a_") { "gif" } else { "webp" };
    Some(format!("{}/{kind}/{owner}/{hash}.{ext}?size=128", instance.media))
}

/// A custom emoji's picture.
pub fn emoji_url(instance: &Instance, id: &str, animated: bool) -> Option<String> {
    (!instance.media.is_empty() && safe(id)).then(|| format!("{}/emojis/{id}.{}", instance.media, if animated { "gif?animated=true" } else { "webp" }))
}

/// A sticker's picture.
pub fn sticker_url(instance: &Instance, id: &str, animated: bool) -> Option<String> {
    (!instance.media.is_empty() && safe(id)).then(|| format!("{}/stickers/{id}.webp{}", instance.media, if animated { "?animated=true" } else { "" }))
}

/// Whether a piece of a path is only the letters, digits, underscores and dashes ids and hashes are made of.
fn safe(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

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

/// Where a file of an address is (or will be) in the cache, as the `file://` address the window loads. The address it
/// comes from is recorded, so it can be fetched when it is not there.
pub fn cached(account_id: &str, dir: &str, url: &str, name: Option<&str>, content_type: Option<&str>) -> String {
    let path = crate::media_cache::dir(dir).join(format!("{}.{}", hash(url), extension_of(name, content_type)));
    crate::media_cache::record(&path, json!({ "url": url, "account": account_id }));
    format!("file://{}", path.display())
}

/// Fetches an address into a cache file, whole and renamed into place.
async fn fetch(account_id: &str, url: &str, path: &Path) -> Result<()> {
    // Only ever an address the instance gave: the path is the cache's own and the address was recorded by `cached`.
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("not an address");
    }
    let router = crate::net::route::router();
    let client = router.client_if("fluxer", router.routed(account_id), |b| b.user_agent(super::user_agent()).timeout(std::time::Duration::from_secs(60)));
    let response = client.get(url).send().await.with_context(|| format!("fetching {url}"))?;
    if !response.status().is_success() {
        bail!("{} answered {}", url, response.status());
    }
    let bytes = response.bytes().await.context("reading the file")?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = path.with_extension("part");
    tokio::fs::write(&part, &bytes).await?;
    tokio::fs::rename(&part, path).await.with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Fetches a cached file again, from the address recorded for it.
pub async fn restore_cached(path: &Path) -> Result<PathBuf> {
    let source = crate::media_cache::source(path).ok_or_else(|| anyhow!("nothing says where this file came from"))?;
    let url = source["url"].as_str().ok_or_else(|| anyhow!("no address was recorded for this file"))?;
    fetch(source["account"].as_str().unwrap_or(""), url, path).await?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> Instance {
        Instance { media: "https://media.example".into(), ..Default::default() }
    }

    #[test]
    fn an_asset_is_on_the_media_proxy_by_its_owner_and_hash() {
        assert_eq!(asset_url(&instance(), "avatars", "123", "abc").as_deref(), Some("https://media.example/avatars/123/abc.webp?size=128"));
        assert_eq!(asset_url(&instance(), "avatars", "123", "a_abc").as_deref(), Some("https://media.example/avatars/123/a_abc.gif?size=128"), "an a_ hash is animated");
        assert_eq!(asset_url(&instance(), "avatars", "123", "../x"), None);
        assert_eq!(asset_url(&Instance::default(), "avatars", "1", "x"), None, "an instance with no media proxy has none");
        assert_eq!(emoji_url(&instance(), "77", false).as_deref(), Some("https://media.example/emojis/77.webp"));
        assert_eq!(sticker_url(&instance(), "88", false).as_deref(), Some("https://media.example/stickers/88.webp"));
    }

    #[test]
    fn the_same_address_is_the_same_name_and_a_different_one_is_not() {
        assert_eq!(hash("https://cdn.example/avatars/1"), hash("https://cdn.example/avatars/1"));
        assert_ne!(hash("https://cdn.example/avatars/1"), hash("https://cdn.example/avatars/2"));
        assert_eq!(hash("").len(), 16);
    }

    #[test]
    fn an_extension_comes_from_the_name_or_the_kind_of_file() {
        assert_eq!(extension_of(Some("cat.JPG"), None), "jpg");
        assert_eq!(extension_of(None, Some("image/webp")), "webp");
        assert_eq!(extension_of(Some("noext"), Some("video/mp4")), "mp4");
        assert_eq!(extension_of(None, None), "png");
        assert_eq!(extension_of(Some("a.way-too-long-extension"), None), "png");
    }
}
