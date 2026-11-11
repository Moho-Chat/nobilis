//! Files and pictures: the ones people share, and the pictures that stand for people.
//!
//! A file is named in the cache by its address when a message mentions it, and fetched when the window looks (so nothing
//! is fetched for a message nobody opens: an address in a message is whatever its sender wants it to be, and fetching it is
//! telling them this client is reading). Only `https` addresses are fetched. The connection is the account's own - through
//! its route, and trusting the authorities the account lists - because the server that stores uploads is usually the
//! account's own server, whatever it is signed by.

use crate::model::Attachment;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const MEDIA: &str = "xmpp-media";
pub const AVATARS: &str = "xmpp-avatars";

/// A stable short name for an address (FNV-1a), so the same file is the same cache entry on every run.
pub fn hash(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// What a file's extension is, from its name or its kind.
pub fn extension_of(name: Option<&str>, content_type: Option<&str>) -> String {
    if let Some(ext) = name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).filter(|e| !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric())) {
        return ext;
    }
    match content_type.unwrap_or("") {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/mp4" | "audio/aac" => "m4a",
        "audio/webm" => "webm",
        "application/pdf" => "pdf",
        _ => "bin",
    }
    .to_string()
}

/// The broad kind of a file, from its media type or else its name.
pub fn kind_of(media_type: Option<&str>, name: Option<&str>) -> &'static str {
    let by_type = media_type.map(|t| t.split('/').next().unwrap_or("")).unwrap_or("");
    match by_type {
        "image" => return "image",
        "video" => return "video",
        "audio" => return "audio",
        _ => {}
    }
    match name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "svg") => "image",
        Some("mp4" | "webm" | "mkv" | "mov") => "video",
        Some("mp3" | "ogg" | "opus" | "oga" | "m4a" | "aac" | "wav" | "flac") => "audio",
        _ => "file",
    }
}

/// Where a file of an address is (or will be) in the cache, as the `file://` address the window loads. The address it comes
/// from is recorded so that it can be fetched when it is not there.
pub fn cached(account_id: &str, dir: &str, url: &str, name: Option<&str>, content_type: Option<&str>) -> String {
    let path = crate::media_cache::dir(dir).join(format!("{}.{}", hash(url), extension_of(name, content_type)));
    crate::media_cache::record(&path, json!({ "url": url, "account": account_id }));
    format!("file://{}", path.display())
}

/// The attachment a shared file is in a message.
pub fn attachment(account_id: &str, f: &super::incoming::FileRef) -> Attachment {
    let kind = kind_of(f.media_type.as_deref(), f.name.as_deref());
    Attachment {
        kind: kind.to_string(),
        mimetype: f.media_type.clone(),
        filename: f.name.clone().or_else(|| f.url.split('#').next().and_then(|u| u.rsplit('/').next()).map(|s| s.to_string()).filter(|s| !s.is_empty())),
        size: f.size,
        width: f.width,
        height: f.height,
        blurhash: None,
        duration_secs: f.duration,
        waveform: None,
        voice: false,
        path: Some(cached(account_id, MEDIA, &f.url, f.name.as_deref(), f.media_type.as_deref())),
        thumbnail_path: None,
        url: Some(f.url.clone()),
    }
}

fn clients() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();
    CLIENTS.get_or_init(Default::default)
}

/// The HTTP client for an account: through its route, trusting the authorities it lists beside the system's.
pub fn http_client(account_id: &str, ca_pem: &str) -> reqwest::Client {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let key = format!("{account_id}|{}|{}", crate::net::route::router().routed(account_id), super::transport::sha256_hex(ca_pem.as_bytes()));
    if let Some(c) = clients().lock().unwrap().get(&key) {
        return c.clone();
    }
    let router = crate::net::route::router();
    let mut builder = reqwest::Client::builder().user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))).connect_timeout(std::time::Duration::from_secs(20));
    match router.proxy_if(router.routed(account_id)).and_then(|p| reqwest::Proxy::all(p).ok()) {
        Some(proxy) => builder = builder.proxy(proxy),
        None => builder = builder.no_proxy(),
    }
    for der in CertificateDer::pem_slice_iter(ca_pem.as_bytes()).flatten() {
        if let Ok(cert) = reqwest::Certificate::from_der(der.as_ref()) {
            builder = builder.add_root_certificate(cert);
        }
    }
    let client = builder.build().unwrap_or_else(|_| reqwest::Client::new());
    clients().lock().unwrap().insert(key, client.clone());
    client
}

/// The largest file fetched: a message's address is somebody else's choice, and a cache is not for a film.
const MOST: u64 = 200 * 1024 * 1024;

/// Fetches an address into a cache file, whole and renamed into place.
async fn fetch(account_id: &str, ca_pem: &str, url: &str, path: &Path) -> Result<()> {
    // An encrypted file (`aesgcm://`): fetched over https and decrypted with the key in the link's fragment.
    let encrypted = super::omemo::parse_aesgcm(url);
    let fetch_url = encrypted.as_ref().map(|e| e.0.as_str()).unwrap_or(url);
    if !fetch_url.starts_with("https://") {
        bail!("only https addresses are fetched");
    }
    let client = http_client(account_id, ca_pem);
    let response = client.get(fetch_url).send().await.with_context(|| format!("fetching {url}"))?;
    if !response.status().is_success() {
        bail!("{} answered {}", url, response.status());
    }
    if response.content_length().is_some_and(|n| n > MOST) {
        bail!("that file is larger than {} MB", MOST / 1024 / 1024);
    }
    let mut bytes = response.bytes().await.context("reading the file")?.to_vec();
    if bytes.len() as u64 > MOST {
        bail!("that file is larger than {} MB", MOST / 1024 / 1024);
    }
    if let Some((_, iv, key)) = &encrypted {
        bytes = super::omemo::decrypt_file(&bytes, iv, key).map_err(|_| anyhow!("that file could not be decrypted: it was changed, or the link's key is wrong"))?;
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = path.with_extension("part");
    tokio::fs::write(&part, &bytes).await?;
    tokio::fs::rename(&part, path).await.with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Fetches a cached file again, from the address recorded for it.
pub async fn restore_cached(state: &crate::state::AppState, path: &Path) -> Result<PathBuf> {
    let source = crate::media_cache::source(path).ok_or_else(|| anyhow!("nothing says where this file came from"))?;
    let url = source["url"].as_str().ok_or_else(|| anyhow!("no address was recorded for this file"))?;
    let account = source["account"].as_str().unwrap_or("");
    let ca = state.accounts.get_xmpp(account).map(|c| c.ca_pem).unwrap_or_default();
    fetch(account, &ca, url, path).await?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_address_is_the_same_name_and_a_different_one_is_not() {
        assert_eq!(hash("https://a.example/1"), hash("https://a.example/1"));
        assert_ne!(hash("https://a.example/1"), hash("https://a.example/2"));
    }

    #[test]
    fn a_files_kind_comes_from_its_media_type_and_else_its_name() {
        assert_eq!(kind_of(Some("image/png"), None), "image");
        assert_eq!(kind_of(Some("audio/ogg"), Some("x.bin")), "audio");
        assert_eq!(kind_of(None, Some("film.MP4")), "video");
        assert_eq!(kind_of(Some("application/octet-stream"), Some("a.zip")), "file");
        assert_eq!(kind_of(None, None), "file");
    }

    #[test]
    fn an_extension_comes_from_the_name_or_the_kind_of_file() {
        assert_eq!(extension_of(Some("cat.JPG"), None), "jpg");
        assert_eq!(extension_of(None, Some("image/webp")), "webp");
        assert_eq!(extension_of(Some("noext"), Some("audio/ogg")), "ogg");
        assert_eq!(extension_of(Some("a.way-too-long-extension"), None), "bin");
    }
}
