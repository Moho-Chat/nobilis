//! Attachments on Google Chat: downloading uploaded files (`get_attachment_url`), caching them in `googlechat-media`
//! so the window loads them as `file://` URLs, and uploading outbound files via Google's resumable `/uploads` endpoint.
//!
//! Attachment downloads follow redirects manually (up to 10 hops) so cookies are sent ONLY to Google hosts
//! (`*.google.com`) and re-added if a `googleusercontent.com` hop redirects back to `chat.google.com`.

use super::pb;
use super::session::{is_google, GcError, Session};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use prost::Message;
use reqwest::header::HeaderValue;
use serde_json::json;
use std::path::{Path, PathBuf};

pub const MEDIA: &str = "googlechat-media";

const GET_ATTACHMENT_URL: &str = "https://chat.google.com/api/get_attachment_url";
const UPLOAD_URL: &str = "https://chat.google.com/uploads";
const FIFE_IMAGE_SIZE: &str = "w10000-h10000";
const MAX_REDIRECTS: usize = 10;
const MAX_DOWNLOAD_BYTES: usize = 100 * 1024 * 1024;

/// A stable short name for an address (FNV-1a), so the same attachment maps to the same cache file.
fn hash(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn extension_of(name: Option<&str>, content_type: Option<&str>) -> String {
    if let Some(ext) = name
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 6 && e.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        return ext;
    }
    match content_type.unwrap_or("").split(';').next().unwrap_or("").trim() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    }
    .to_string()
}

/// Builds the `get_attachment_url` fetch URL for an `UploadMetadata` annotation.
pub fn attachment_url(meta: &pb::UploadMetadata) -> Option<String> {
    let token = match meta.payload.as_ref()? {
        pb::upload_metadata::Payload::AttachmentToken(t) if !t.is_empty() => t.as_str(),
        _ => return None,
    };
    let content_type = meta.content_type();
    let is_image = content_type.starts_with("image/");
    let mut url = reqwest::Url::parse(GET_ATTACHMENT_URL).ok()?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("attachment_token", token);
        if is_image {
            q.append_pair("url_type", "FIFE_URL");
            q.append_pair("sz", FIFE_IMAGE_SIZE);
            q.append_pair("content_type", content_type);
        } else {
            q.append_pair("url_type", "DOWNLOAD_URL");
        }
    }
    Some(url.to_string())
}

/// Registers an attachment in the `googlechat-media` cache and returns its `file://` URL.
/// The bytes are fetched lazily when the window requests it (`restore_cached`) or eagerly via `ensure_cached`.
pub fn cached_attachment(account_id: &str, meta: &pb::UploadMetadata) -> Option<String> {
    let url = attachment_url(meta)?;
    let name = (!meta.content_name().is_empty()).then_some(meta.content_name());
    let ct = (!meta.content_type().is_empty()).then_some(meta.content_type());
    let file = crate::media_cache::dir(MEDIA).join(format!("{}.{}", hash(&format!("{account_id}{url}")), extension_of(name, ct)));
    crate::media_cache::record(&file, json!({ "url": url, "account": account_id, "auth": true }));
    Some(format!("file://{}", file.display()))
}

/// Registers an external inline media URL (e.g. a GIF's `image_url` from `UrlMetadata`) in the cache, fetched
/// without Google cookies.
pub fn cached_external(account_id: &str, url: &str, content_type: Option<&str>) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    let path_name = url.split(['?', '#']).next();
    let file = crate::media_cache::dir(MEDIA).join(format!("{}.{}", hash(&format!("{account_id}{url}")), extension_of(path_name, content_type)));
    crate::media_cache::record(&file, json!({ "url": url, "account": account_id, "auth": false }));
    Some(format!("file://{}", file.display()))
}

/// Downloads an attachment, following redirects manually and sending cookies only to Google hosts.
pub async fn download_attachment(session: &Session, initial_url: &str) -> Result<Vec<u8>> {
    let client = session.no_redirect_client();
    let mut current = initial_url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let parsed = reqwest::Url::parse(&current).with_context(|| format!("invalid attachment URL {current}"))?;
        let mut req = client.get(parsed.clone());
        if is_google(parsed.as_str()) {
            req = req.headers(session.headers(parsed.as_str()));
        }
        let resp = req.send().await.context("downloading Google Chat attachment")?;
        if is_google(resp.url().as_str()) {
            session.absorb(&resp);
        }
        let status = resp.status().as_u16();
        // Follow 301, 302, 307, 308 (deliberately not 303, matching gchatmeow/download.go).
        if matches!(status, 301 | 302 | 307 | 308) {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("redirect missing Location header"))?;
            let next = parsed.join(loc).with_context(|| format!("invalid redirect location {loc}"))?;
            current = next.to_string();
            continue;
        }
        if status >= 400 {
            bail!("Google Chat attachment returned HTTP {status}");
        }
        if resp.content_length().is_some_and(|len| len as usize > MAX_DOWNLOAD_BYTES) {
            bail!("attachment exceeds maximum size ({MAX_DOWNLOAD_BYTES} bytes)");
        }
        let bytes = resp.bytes().await.context("reading attachment body")?;
        if bytes.len() > MAX_DOWNLOAD_BYTES {
            bail!("attachment exceeds maximum size ({MAX_DOWNLOAD_BYTES} bytes)");
        }
        return Ok(bytes.to_vec());
    }
    bail!("attachment download exceeded {MAX_REDIRECTS} redirects")
}

/// Downloads an external URL without any Google cookies, over the account's route.
async fn download_external(account_id: &str, url: &str) -> Result<Vec<u8>> {
    if !url.starts_with("https://") {
        bail!("refusing non-HTTPS external media URL");
    }
    let router = crate::net::route::router();
    let client = router.client_if("googlechat-ext", router.routed(account_id), |b| {
        b.user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))).timeout(std::time::Duration::from_secs(60))
    });
    let resp = client.get(url).send().await.with_context(|| format!("fetching {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} answered {}", resp.status());
    }
    if resp.content_length().is_some_and(|len| len as usize > MAX_DOWNLOAD_BYTES) {
        bail!("external media exceeds maximum size");
    }
    let bytes = resp.bytes().await.context("reading external media")?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        bail!("external media exceeds maximum size");
    }
    Ok(bytes.to_vec())
}

/// Fetches a cached file on demand when the window asks `restoreMedia`.
pub async fn restore_cached(state: &crate::state::AppState, file: &Path) -> Result<PathBuf> {
    let source = crate::media_cache::source(file).ok_or_else(|| anyhow!("nothing says where this file came from"))?;
    let account = source["account"].as_str().unwrap_or("");
    let url = source["url"].as_str().ok_or_else(|| anyhow!("no address was recorded for this file"))?;
    let auth = source["auth"].as_bool().unwrap_or(true);
    let bytes = if auth {
        let handle = state.runtime.googlechat_handle(account).ok_or_else(|| anyhow!("that Google Chat account is not connected"))?;
        download_attachment(&handle.api.session, url).await?
    } else {
        download_external(account, url).await?
    };
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = file.with_extension("part");
    tokio::fs::write(&part, &bytes).await?;
    tokio::fs::rename(&part, file).await.with_context(|| format!("writing {}", file.display()))?;
    Ok(file.to_path_buf())
}

/// Uploads a file via Google Chat's single-shot resumable `/uploads` endpoint and returns `UploadMetadata`
/// for an `UPLOAD_METADATA` annotation.
///
/// Must NOT send `alt` or `key` query params; `group_id` is the bare inner DM or space id.
pub async fn upload_file(session: &Session, group_inner_id: &str, data: &[u8], filename: &str, mime_type: &str) -> Result<pb::UploadMetadata, GcError> {
    let xsrf = session.ensure_xsrf().await?;
    let mut start_url = reqwest::Url::parse(UPLOAD_URL).map_err(|e| GcError::Network(e.to_string()))?;
    start_url.query_pairs_mut().append_pair("group_id", group_inner_id);

    let mut start_headers = session.headers(start_url.as_str());
    start_headers.insert("x-goog-upload-protocol", HeaderValue::from_static("resumable"));
    start_headers.insert("x-goog-upload-command", HeaderValue::from_static("start"));
    if let Ok(v) = HeaderValue::from_str(&data.len().to_string()) {
        start_headers.insert("x-goog-upload-content-length", v);
    }
    if let Ok(v) = HeaderValue::from_str(mime_type) {
        start_headers.insert("x-goog-upload-content-type", v);
    }
    if let Ok(v) = HeaderValue::from_str(filename) {
        start_headers.insert("x-goog-upload-file-name", v);
    }
    if let Ok(v) = HeaderValue::from_str(&xsrf) {
        start_headers.insert("x-framework-xsrf-token", v);
    }

    let start_resp = session
        .client()
        .post(start_url)
        .headers(start_headers)
        .send()
        .await
        .map_err(|e| GcError::Network(crate::backend::mattermost::api::network_words(&e)))?;
    session.absorb(&start_resp);
    let start_status = start_resp.status().as_u16();
    let next_url = start_resp
        .headers()
        .get("x-goog-upload-url")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if start_status != 200 {
        let body = start_resp.text().await.unwrap_or_default();
        return Err(GcError::Status { status: start_status, body });
    }
    let next_url = next_url.ok_or_else(|| GcError::Network("upload start response missing x-goog-upload-url".into()))?;

    let mut finalize_headers = session.headers(&next_url);
    finalize_headers.insert("x-goog-upload-command", HeaderValue::from_static("upload, finalize"));
    finalize_headers.insert("x-goog-upload-protocol", HeaderValue::from_static("resumable"));
    finalize_headers.insert("x-goog-upload-offset", HeaderValue::from_static("0"));
    if is_google(&next_url) {
        if let Ok(v) = HeaderValue::from_str(&xsrf) {
            finalize_headers.insert("x-framework-xsrf-token", v);
        }
    }

    let finalize_resp = session
        .client()
        .put(&next_url)
        .headers(finalize_headers)
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| GcError::Network(crate::backend::mattermost::api::network_words(&e)))?;
    session.absorb(&finalize_resp);
    let fin_status = finalize_resp.status().as_u16();
    let fin_bytes = finalize_resp.bytes().await.map_err(|e| GcError::Network(e.to_string()))?;
    if fin_status != 200 {
        return Err(GcError::Status { status: fin_status, body: String::from_utf8_lossy(&fin_bytes[..fin_bytes.len().min(512)]).into_owned() });
    }

    let stripped: Vec<u8> = fin_bytes.iter().copied().filter(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)).collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&stripped)
        .map_err(|e| GcError::Network(format!("could not decode base64 upload response: {e}")))?;
    pb::UploadMetadata::decode(decoded.as_slice()).map_err(|e| GcError::Network(format!("could not parse upload metadata protobuf: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_url_branches_on_image_mime() {
        let img = pb::UploadMetadata {
            payload: Some(pb::upload_metadata::Payload::AttachmentToken("tok123".into())),
            content_type: Some("image/png".into()),
            ..Default::default()
        };
        let u = attachment_url(&img).unwrap();
        assert!(u.contains("url_type=FIFE_URL"));
        assert!(u.contains("sz=w10000-h10000"));
        assert!(u.contains("attachment_token=tok123"));

        let pdf = pb::UploadMetadata {
            payload: Some(pb::upload_metadata::Payload::AttachmentToken("tok456".into())),
            content_type: Some("application/pdf".into()),
            ..Default::default()
        };
        let u = attachment_url(&pdf).unwrap();
        assert!(u.contains("url_type=DOWNLOAD_URL"));
        assert!(!u.contains("FIFE_URL"));
    }
}
