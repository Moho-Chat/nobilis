//! Matrix media: fetching it, caching it, and putting it back.
//!
//! Two paths for everything, because an encrypted room's attachments are
//! encrypted too - the same picture is either a URL or a URL plus a key, and
//! the difference reaches all the way down to how it is cached.

use super::*;

/// Turns a media event's `content` into an Attachment, carrying across the
/// `info` block Matrix already provides - mimetype, size, intrinsic
/// dimensions and blurhash - rather than reducing all of it to a file path.
/// The dimensions in particular let a frontend reserve layout space before
/// any bytes arrive, which is why Element's timeline doesn't reflow as
/// images load.
pub(super) fn build_attachment(content: &Value, path: Option<String>, thumbnail_path: Option<String>) -> Attachment {
    let info = &content["info"];
    let mimetype = info["mimetype"].as_str().map(str::to_string);
    Attachment {
        kind: match content["msgtype"].as_str().unwrap_or("") {
            "m.image" => "image",
            "m.video" => "video",
            "m.audio" => "audio",
            _ => "file",
        }
        .to_string(),
        filename: content["body"].as_str().map(str::to_string),
        size: info["size"].as_u64(),
        width: info["w"].as_u64().map(|v| v as u32),
        height: info["h"].as_u64().map(|v| v as u32),
        // MSC2448, still under its unstable prefix on most servers.
        blurhash: info["xyz.amorgan.blurhash"].as_str().or_else(|| info["blurhash"].as_str()).map(str::to_string),
        mimetype,
        path,
        thumbnail_path,
        duration_secs: voice_duration_secs(content),
        waveform: voice_waveform(content),
        // MSC3245: an empty object whose presence is the whole statement.
        // Element sends it on anything recorded with its microphone button,
        // and on nothing else - so this says "somebody spoke" exactly where
        // Discord's flag does, and an .ogg deliberately attached stays a file.
        voice: !content["org.matrix.msc3245.voice"].is_null(),
        url: None,
    }
}

/// Fetches the server-generated thumbnail a media event points at, when it
/// has one. Preferring this for previews is what keeps a timeline from
/// pulling full-size originals off the homeserver just to draw something
/// small - the same reason Element requests the thumbnail endpoint.
pub(super) async fn thumbnail_for(content: &Value, homeserver_url: &str, access_token: &str) -> Option<String> {
    let info = &content["info"];
    // Encrypted rooms carry the thumbnail as its own EncryptedFile rather
    // than a plain mxc URI, and it needs the same decrypt path as the body.
    if let Some(file) = info.get("thumbnail_file").filter(|v| v.is_object()) {
        return cached_encrypted_media_path(homeserver_url, access_token, file, "").await;
    }
    if let Some(mxc) = info["thumbnail_url"].as_str() {
        return cached_media_path(homeserver_url, access_token, mxc, "").await;
    }

    // Nothing the sender made, so ask the server to make one. Only for a
    // picture big enough that it is worth a second request: a small image
    // fetched whole is one round trip, and the same image fetched as a
    // thumbnail and then whole is two.
    //
    // Not for an encrypted room - and the `file` branch above has already
    // taken those. The server holds ciphertext there and cannot resize what
    // it cannot read, which is why a client-made thumbnail is the only kind
    // an encrypted room ever has.
    if content["msgtype"].as_str() != Some("m.image") || !worth_thumbnailing(info) {
        return None;
    }
    let mxc = content["url"].as_str()?;
    cached_thumbnail_path(homeserver_url, access_token, mxc).await
}

/// Whether asking the server to shrink this is worth the request.
///
/// A picture already small enough to draw is fetched once either way, and
/// asking for a thumbnail of it costs a round trip to save nothing. The size
/// is the honest measure where the sender gave one; the dimensions are the
/// fallback, since a large photograph compresses well and a small one badly.
fn worth_thumbnailing(info: &Value) -> bool {
    const WORTH_IT: u64 = 512 * 1024;
    if let Some(size) = info["size"].as_u64() {
        return size > WORTH_IT;
    }
    let (w, h) = (info["w"].as_u64().unwrap_or(0), info["h"].as_u64().unwrap_or(0));
    w > THUMBNAIL_W || h > THUMBNAIL_H
}

/// How big a thumbnail to ask for.
///
/// Generous for a timeline picture and far short of a modern camera's output
/// - a phone photograph is several thousand pixels across and a few
/// megabytes, and this is tens of kilobytes. `scale` rather than `crop`
/// because the picture in a timeline is the whole picture: cropping would
/// quietly cut the sides off somebody's screenshot.
const THUMBNAIL_W: u64 = 800;
const THUMBNAIL_H: u64 = 600;

/// Fetches a server-made thumbnail, cached beside the originals.
///
/// Its own name in the cache rather than the original's, because they are
/// different files: a thumbnail written under the original's name would be
/// handed out later to anything that asked for the full-size picture.
async fn cached_thumbnail_path(homeserver_url: &str, access_token: &str, mxc_uri: &str) -> Option<String> {
    let rest = mxc_uri.strip_prefix("mxc://")?;
    let (server_name, media_id) = rest.split_once('/')?;
    let dir = media_cache_dir();
    let path = dir.join(format!("{server_name}_{media_id}_thumb{THUMBNAIL_W}x{THUMBNAIL_H}"));
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Some(format!("file://{}", path.display()));
    }

    let url = format!(
        "{}/_matrix/client/v1/media/thumbnail/{}/{}?width={THUMBNAIL_W}&height={THUMBNAIL_H}&method=scale",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(server_name.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(media_id.as_bytes()).collect::<String>(),
    );
    let fetch = tokio::time::timeout(std::time::Duration::from_secs(20), http::get_bytes(&url, access_token)).await;
    // Quiet on every failure. A server that will not thumbnail - an old one,
    // one that cannot read the format, one that has the original on cold
    // storage - is not a problem to report: the full-size picture is fetched
    // anyway and the reader sees the same image.
    let Ok(Ok((status, bytes))) = fetch else { return None };
    if !(200..300).contains(&status) || bytes.is_empty() {
        return None;
    }
    if tokio::fs::create_dir_all(&dir).await.is_err() || tokio::fs::write(&path, &bytes).await.is_err() {
        return None;
    }
    Some(format!("file://{}", path.display()))
}

/// Matrix's own mxc:// media ids carry no file extension - unlike
/// backend/sneedchat/mod.rs's cached attachments, which keep whatever
/// extension the original filename already had. The cached file still gets a
/// real extension so that anything reading it off disk (an image viewer the
/// user opens it in, a frontend sniffing by name) sees the right type - but
/// this is now a detail of how the cache is named, not something the wire
/// contract depends on: the mimetype travels on the attachment itself.
/// Historically this existed because the client classified media by URL
/// pattern - the
/// same class of bug already fixed once for Discord/klipy's own
/// extensionless CDN URLs (see opaqueImageHosts there), just hit again
/// here from a different direction (no extension at all, rather than an
/// extension-less but known host). Only the handful of types actually
/// worth embedding inline are mapped; anything else falls back to no
/// extension, same as before this existed - it just won't auto-embed,
/// same as it wouldn't have anyway.
pub(super) fn extension_for_mimetype(mimetype: &str) -> &'static str {
    match mimetype {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/mpeg" => "mp3",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
        "video/ogg" => "ogv",
        _ => "",
    }
}

/// Resolves an `mxc://` URI to a local `file://` path, downloading and
/// caching it first if needed - a plain QML `Image`/media element has no
/// route to send the `Authorization: Bearer` header Matrix's media
/// endpoint requires, so the raw remote URL would simply never load (same
/// reasoning backend/sneedchat/mod.rs's own avatar caching already
/// documents). Cached permanently per media id for this daemon's
/// lifetime - see MEDIA_CACHE_MAX_BYTES/sweep_media_cache for the size
/// cap that keeps that bounded. `extension` (from extension_for_mimetype,
/// empty string if unknown) is appended to the cache filename - see that
/// function's own doc comment on why this can't just be left off.
pub(crate) async fn cached_media_path(homeserver_url: &str, access_token: &str, mxc_uri: &str, extension: &str) -> Option<String> {
    let rest = mxc_uri.strip_prefix("mxc://")?;
    let (server_name, media_id) = rest.split_once('/')?;
    let dir = media_cache_dir();
    let filename = if extension.is_empty() { format!("{server_name}_{media_id}") } else { format!("{server_name}_{media_id}.{extension}") };
    let path = dir.join(filename);

    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Some(format!("file://{}", path.display()));
    }

    let url = format!(
        "{}/_matrix/client/v1/media/download/{}/{}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(server_name.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(media_id.as_bytes()).collect::<String>(),
    );
    let fetch = tokio::time::timeout(std::time::Duration::from_secs(20), http::get_bytes(&url, access_token)).await;
    let (status, bytes) = match fetch {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::debug!("matrix: media fetch for {mxc_uri} failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::debug!("matrix: media fetch for {mxc_uri} timed out");
            return None;
        }
    };
    if !(200..300).contains(&status) || bytes.is_empty() {
        tracing::debug!("matrix: media fetch for {mxc_uri} returned HTTP {status}");
        return None;
    }
    if tokio::fs::create_dir_all(&dir).await.is_err() || tokio::fs::write(&path, &bytes).await.is_err() {
        return None;
    }
    Some(format!("file://{}", path.display()))
}

/// Same as cached_media_path, but for E2EE media: `file` is the message's
/// `content.file` object (mxc URI plus the per-file AES key/iv/hash - see
/// protocol::encrypted_media_file) - the downloaded ciphertext is
/// AES-256-CTR decrypted (matrix-sdk-crypto's AttachmentDecryptor, the
/// same primitive send_message's upload_encrypted_media_message uses in
/// reverse) before being written to the same on-disk cache, so the cached
/// file is real plaintext by the time a plain QML Image/MediaPlayer opens
/// it - same reasoning as the unencrypted path needing to route around a
/// plain Image having no way to send a Bearer header, plus here also no
/// way to AES-decrypt on the fly.
pub(crate) async fn cached_encrypted_media_path(homeserver_url: &str, access_token: &str, file: &Value, extension: &str) -> Option<String> {
    use matrix_sdk_crypto::{AttachmentDecryptor, MediaEncryptionInfo};
    use std::io::{Cursor, Read};

    let mxc_uri = file["url"].as_str()?;
    let rest = mxc_uri.strip_prefix("mxc://")?;
    let (server_name, media_id) = rest.split_once('/')?;
    let dir = media_cache_dir();
    let filename = if extension.is_empty() { format!("{server_name}_{media_id}") } else { format!("{server_name}_{media_id}.{extension}") };
    let path = dir.join(filename);

    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Some(format!("file://{}", path.display()));
    }

    // Strip "url" before deserializing - MediaEncryptionInfo's shape is
    // exactly this object minus the mxc URI (see upload_encrypted_media_
    // message, which builds it the same way in reverse), and its custom
    // Deserialize impl expects no extra fields.
    let mut encryption_info = file.clone();
    if let Some(obj) = encryption_info.as_object_mut() {
        obj.remove("url");
    }
    let encryption_info: MediaEncryptionInfo = match serde_json::from_value(encryption_info) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("matrix: invalid encrypted media info for {mxc_uri}: {e}");
            return None;
        }
    };

    let download_url = format!(
        "{}/_matrix/client/v1/media/download/{}/{}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(server_name.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(media_id.as_bytes()).collect::<String>(),
    );
    let fetch = tokio::time::timeout(std::time::Duration::from_secs(20), http::get_bytes(&download_url, access_token)).await;
    let (status, ciphertext) = match fetch {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::debug!("matrix: encrypted media fetch for {mxc_uri} failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::debug!("matrix: encrypted media fetch for {mxc_uri} timed out");
            return None;
        }
    };
    if !(200..300).contains(&status) || ciphertext.is_empty() {
        tracing::debug!("matrix: encrypted media fetch for {mxc_uri} returned HTTP {status}");
        return None;
    }

    let mut cursor = Cursor::new(ciphertext);
    let mut decryptor = match AttachmentDecryptor::new(&mut cursor, encryption_info) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("matrix: building decryptor for {mxc_uri} failed: {e}");
            return None;
        }
    };
    let mut plaintext = Vec::new();
    if let Err(e) = decryptor.read_to_end(&mut plaintext) {
        tracing::debug!("matrix: decrypting media {mxc_uri} failed: {e}");
        return None;
    }

    if tokio::fs::create_dir_all(&dir).await.is_err() || tokio::fs::write(&path, &plaintext).await.is_err() {
        return None;
    }
    Some(format!("file://{}", path.display()))
}

/// A genuine cache (every file here is re-fetchable from the homeserver
/// given its message/state event, and encrypted media additionally needs
/// the room's Megolm session either way) - belongs under XDG_CACHE_HOME,
/// not alongside accounts.toml/the crypto store/scrollback in
/// config_dir()'s `~/.config/nobilis` (see main.rs's own
/// migrate_caches_to_xdg_cache_dir, which moves any pre-existing directory
/// here on first startup after this changed).
pub(super) fn media_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache")).join("nobilis").join("matrix-media")
}

/// Cap on the media cache's total size on disk - same unbounded-growth
/// concern (and same fix) as backend/sneedchat/mod.rs's avatar/attachment
/// caches: every distinct piece of media ever seen would otherwise
/// accumulate its own permanently-cached file forever.
pub const MEDIA_CACHE_MAX_BYTES: u64 = 250 * 1024 * 1024;

/// Evicts the oldest-written files in the media cache until it's back
/// under MEDIA_CACHE_MAX_BYTES - oldest-by-mtime, same simplification
/// backend/sneedchat/mod.rs's own sweep_cache_dir documents (not true LRU,
/// but a reasonable approximation without an extra dependency).
pub async fn sweep_media_cache() {
    let dir = media_cache_dir();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else { return };

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
    if total <= MEDIA_CACHE_MAX_BYTES {
        return;
    }

    files.sort_by_key(|(_, _, mtime)| *mtime);
    let mut to_free = total - MEDIA_CACHE_MAX_BYTES;
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
    tracing::info!("matrix: media cache was over its {}MB cap, evicted {removed} oldest file(s)", MEDIA_CACHE_MAX_BYTES / 1024 / 1024);
}

/// `m.room.message` msgtype + upload Content-Type for a local file, chosen
/// from its extension - a reasonable guess without needing to sniff file
/// contents, same convention backend/sneedchat/mod.rs's own attachment
/// handling uses. Shared by both the plaintext and encrypted upload paths
/// below - only the mime half is unused by the encrypted one (see its own
/// doc comment on why the upload itself always goes out as opaque bytes
/// regardless of the real content type).
pub(super) fn media_msgtype_and_mime(ext: &str) -> (&'static str, &'static str) {
    match ext {
        "png" => ("m.image", "image/png"),
        "jpg" | "jpeg" => ("m.image", "image/jpeg"),
        "gif" => ("m.image", "image/gif"),
        "webp" => ("m.image", "image/webp"),
        "avif" => ("m.image", "image/avif"),
        "mp4" | "webm" => ("m.video", "video/mp4"),
        // Each by its own name. These used to answer "audio/mpeg" together,
        // which is wrong for three of the four and is the type a voice
        // message would have been uploaded under - an .ogg labelled as mp3
        // plays in some clients and not others.
        "mp3" => ("m.audio", "audio/mpeg"),
        "ogg" | "opus" => ("m.audio", "audio/ogg"),
        "wav" => ("m.audio", "audio/wav"),
        "flac" => ("m.audio", "audio/flac"),
        _ => ("m.file", "application/octet-stream"),
    }
}

/// How long a Matrix voice message runs.
///
/// Milliseconds here, where Discord counts seconds - MSC1767's audio block
/// first, since that is where a voice message carries it, falling back to the
/// `info` block that ordinary audio has used since long before any of this.
fn voice_duration_secs(content: &Value) -> Option<f64> {
    let ms = content["org.matrix.msc1767.audio"]["duration"]
        .as_f64()
        .or_else(|| content["info"]["duration"].as_f64())?;
    (ms > 0.0).then_some(ms / 1000.0)
}

/// The waveform, in the one shape the rest of this program reads.
///
/// Matrix sends an array of integers from 0 to 1024; Discord sends bytes,
/// base64'd. Converted here rather than carried as a second format, because
/// the alternative is every client learning both - and a waveform is a
/// picture of a sound, not a protocol detail worth preserving. The scale is
/// 1024 to 255, which loses two bits nobody can see in a 3px-wide bar.
fn voice_waveform(content: &Value) -> Option<String> {
    use base64::Engine;
    let samples = content["org.matrix.msc1767.audio"]["waveform"].as_array()?;
    if samples.is_empty() {
        return None;
    }
    let bytes: Vec<u8> = samples
        .iter()
        .filter_map(|v| v.as_f64())
        .map(|v| (v.clamp(0.0, 1024.0) / 1024.0 * 255.0).round() as u8)
        .collect();
    (!bytes.is_empty()).then(|| base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Marks an uploaded audio file as somebody speaking.
///
/// The three things that make a client draw a waveform instead of a file row,
/// applied to the content an upload already produced: the MSC3245 marker, the
/// MSC1767 audio block with the length and the picture of the sound, and the
/// length again in `info` where clients older than any of this look.
///
/// The waveform is scaled back up on the way out. This program carries one
/// shape internally - Discord's bytes - so converting here keeps every caller
/// from having to know that Matrix counts to 1024.
///
/// Its own function so the shape can be tested without a homeserver, and
/// tested against the parser above: what this writes, `build_attachment` has
/// to read back as a voice message.
pub(super) fn mark_as_voice(content: &mut Value, duration_secs: f64, waveform: &[u8]) {
    let ms = (duration_secs * 1000.0).round() as u64;
    let samples: Vec<u64> = waveform.iter().map(|b| (*b as f64 / 255.0 * 1024.0).round() as u64).collect();
    content["info"]["duration"] = serde_json::json!(ms);
    content["org.matrix.msc1767.audio"] = serde_json::json!({ "duration": ms, "waveform": samples });
    content["org.matrix.msc3245.voice"] = serde_json::json!({});
}

pub(super) fn file_extension(path: &str) -> (String, String) {
    let filename = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    (filename, ext)
}

/// upload_encrypted_media_message for the E2EE counterpart.
/// Refuses a file the server has already said it will not take.
///
/// Asked of what the server published at connect rather than of the server
/// now: the limit is a property of the deployment, and one round trip per
/// attachment to re-read a number that does not change would be a round trip
/// spent to say nothing.
///
/// A server that declined to state a limit refuses nothing here - the spec
/// allows silence, and a client that blocked every upload because it could
/// not read a number would be worse than one that never asked.
pub(super) async fn check_upload_size(state: &AppState, account_id: &str, path: &str) -> Result<()> {
    let Some(facts) = state.runtime.matrix_server_facts(account_id) else { return Ok(()) };
    let Ok(meta) = tokio::fs::metadata(path).await else { return Ok(()) };
    if facts.accepts_upload(meta.len()) {
        return Ok(());
    }
    let limit = facts.max_upload_size.unwrap_or(0);
    anyhow::bail!(
        "That file is {} and this server accepts at most {}",
        human_size(meta.len()),
        human_size(limit)
    )
}

/// A byte count as somebody would say it.
///
/// Powers of 1024 with the shorter names, which is what a file manager shows
/// and therefore what the number somebody is comparing against looks like.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub(super) async fn upload_media_message(base: &str, access_token: &str, path: &str, body: &str) -> Result<Value> {
    let bytes = tokio::fs::read(path).await.context("reading attachment")?;
    let (filename, ext) = file_extension(path);
    let (msgtype, mime) = media_msgtype_and_mime(&ext);

    let upload_url = format!("{base}/_matrix/media/v3/upload?filename={}", url::form_urlencoded::byte_serialize(filename.as_bytes()).collect::<String>());
    let resp = http_client_post_bytes(&upload_url, access_token, mime, bytes).await.context("uploading media")?;
    let content_uri = resp["content_uri"].as_str().context("upload response missing content_uri")?;

    // "info.mimetype" matters on the *receive* side too, not just here -
    // this same content comes back through /sync as our own echo (and to
    // every other member), and extension_for_mimetype (see
    // handle_timeline_event's media branch) needs it to give the cached
    // file a real extension; without it the file lands in the local cache
    // with no extension at all and the client's URL-pattern-based
    // embed detector silently never recognizes it as media (see this
    // project's own earlier "media isn't embedding" fix for the exact same
    // failure mode with media authored by *other* clients).
    Ok(serde_json::json!({ "msgtype": msgtype, "body": if body.is_empty() { filename } else { body.to_string() }, "url": content_uri, "info": { "mimetype": mime } }))
}

/// Same as upload_media_message, but for an E2EE room: the file itself is
/// AES-256-CTR encrypted with a fresh one-time key (matrix-sdk-crypto's
/// AttachmentEncryptor - the same primitive/format `matrix-sdk` itself
/// uses, not hand-rolled here) *before* upload, so the homeserver only
/// ever stores ciphertext. The resulting per-file key/iv/hash go into
/// `content.file` (an `EncryptedFile`, spec-shaped, in place of the plain
/// `content.url` the unencrypted path uses) rather than a bare mxc URI -
/// that whole content object, key included, still passes through the
/// caller's normal share_and_encrypt_content step afterward like any other
/// message, so the per-file key itself is *also* only ever visible to
/// actual room members via Megolm, not just whoever can reach the media
/// repo.
///
/// The upload itself always goes out as opaque `application/octet-stream`
/// bytes with no `?filename=` query param - the ciphertext reveals nothing
/// about the real content either way, but the real filename/mimetype
/// (carried in this same event's `body`/`info` instead) has no reason to
/// ever reach the server in the clear.
pub(super) async fn upload_encrypted_media_message(base: &str, access_token: &str, path: &str, body: &str) -> Result<Value> {
    use matrix_sdk_crypto::AttachmentEncryptor;
    use std::io::{Cursor, Read};

    let bytes = tokio::fs::read(path).await.context("reading attachment")?;
    let (filename, ext) = file_extension(path);
    let (msgtype, mime) = media_msgtype_and_mime(&ext);

    let mut cursor = Cursor::new(bytes);
    let mut encryptor = AttachmentEncryptor::new(&mut cursor);
    let mut ciphertext = Vec::new();
    encryptor.read_to_end(&mut ciphertext).context("encrypting attachment")?;
    let media_info = encryptor.finish();

    let upload_url = format!("{base}/_matrix/media/v3/upload");
    let resp = http_client_post_bytes(&upload_url, access_token, "application/octet-stream", ciphertext).await.context("uploading encrypted media")?;
    let content_uri = resp["content_uri"].as_str().context("upload response missing content_uri")?;

    let mut file = serde_json::to_value(&media_info).context("serializing encryption info")?;
    file["url"] = serde_json::json!(content_uri);

    // See upload_media_message's own comment on why "info.mimetype" (the
    // real type - unrelated to the octet-stream Content-Type the upload
    // itself went out as) matters for the receive-side cache extension.
    Ok(serde_json::json!({ "msgtype": msgtype, "body": if body.is_empty() { filename } else { body.to_string() }, "file": file, "info": { "mimetype": mime } }))
}

pub(super) async fn http_client_post_bytes(url: &str, access_token: &str, content_type: &str, bytes: Vec<u8>) -> Result<Value> {
    let resp = http::http_client()
        .post(url)
        .bearer_auth(access_token)
        .header("Content-Type", content_type)
        .body(bytes)
        .send()
        .await
        .context("upload request failed")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("invalid JSON response")?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {body}");
    }
    Ok(body)
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    /// A picture already small enough to draw is fetched once either way, so
    /// asking the server to shrink it costs a round trip to save nothing.
    #[test]
    fn only_a_big_picture_is_worth_a_second_request() {
        let big = serde_json::json!({ "size": 4 * 1024 * 1024, "w": 4000, "h": 3000 });
        assert!(worth_thumbnailing(&big));

        let small = serde_json::json!({ "size": 20 * 1024, "w": 200, "h": 120 });
        assert!(!worth_thumbnailing(&small));

        // The size is believed where the sender gave one, even when the
        // dimensions are large: a big photograph that compressed well is
        // still one fetch.
        let compressed = serde_json::json!({ "size": 60 * 1024, "w": 4000, "h": 3000 });
        assert!(!worth_thumbnailing(&compressed));

        // With no size at all, the dimensions are the only measure there is.
        assert!(worth_thumbnailing(&serde_json::json!({ "w": 4000, "h": 3000 })));
        assert!(!worth_thumbnailing(&serde_json::json!({ "w": 100, "h": 100 })));
        // And a sender who said nothing gets the old behaviour: one fetch.
        assert!(!worth_thumbnailing(&serde_json::json!({})));
    }

    /// The number somebody is comparing against is the one their file
    /// manager shows, so it is written the way a file manager writes it.
    #[test]
    fn a_size_is_written_the_way_a_person_reads_one() {
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(50 * 1024 * 1024), "50.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn carries_the_matrix_info_block_onto_the_attachment() {
        // Matrix already describes media properly; the point of build_attachment
        // is to stop throwing that description away.
        let content = serde_json::json!({
            "msgtype": "m.image",
            "body": "holiday.jpg",
            "url": "mxc://example.org/abc",
            "info": {
                "mimetype": "image/jpeg",
                "size": 148213,
                "w": 1920,
                "h": 1080,
                "xyz.amorgan.blurhash": "LEHV6nWB2yk8"
            }
        });
        let a = build_attachment(&content, Some("file:///cache/abc.jpg".into()), None);
        assert_eq!(a.kind, "image");
        assert_eq!(a.mimetype.as_deref(), Some("image/jpeg"));
        // `body` is the filename in Matrix, which is exactly what it is here.
        assert_eq!(a.filename.as_deref(), Some("holiday.jpg"));
        assert_eq!(a.size, Some(148213));
        assert_eq!((a.width, a.height), (Some(1920), Some(1080)));
        assert_eq!(a.blurhash.as_deref(), Some("LEHV6nWB2yk8"));
        assert_eq!(a.path.as_deref(), Some("file:///cache/abc.jpg"));
    }

    #[test]
    fn maps_every_media_msgtype_to_a_renderer_kind() {
        for (msgtype, want) in [
            ("m.image", "image"),
            ("m.video", "video"),
            ("m.audio", "audio"),
            ("m.file", "file"),
        ] {
            let content = serde_json::json!({ "msgtype": msgtype, "body": "x", "info": {} });
            assert_eq!(build_attachment(&content, None, None).kind, want, "for {msgtype}");
        }
    }

    #[test]
    fn an_unfetched_attachment_still_describes_itself() {
        // A failed or pending download must not lose the metadata - a frontend
        // can still show a placeholder of the right size and offer a retry.
        let content = serde_json::json!({
            "msgtype": "m.image", "body": "big.png",
            "info": { "mimetype": "image/png", "w": 640, "h": 480 }
        });
        let a = build_attachment(&content, None, None);
        assert!(a.path.is_none());
        assert_eq!((a.width, a.height), (Some(640), Some(480)));
        assert_eq!(a.filename.as_deref(), Some("big.png"));
    }

    /// Unlike the two live probes above, this needs no network/homeserver
    /// at all - it isolates exactly the part of the encrypted-media path
    /// that's actually new/risky (upload_encrypted_media_message's encrypt-
    /// then-serialize-then-stash-url dance, and cached_encrypted_media_
    /// path's exact mirror of it on the way back down) from the
    /// unencrypted-path-reusing upload/download HTTP calls around it,
    /// which the media probe above already exercises for real. A real
    /// AES-256-CTR encrypt/decrypt round trip, through the exact same
    /// serde_json::Value shape both of those functions build/consume.
    #[test]
    fn matrix_encrypted_media_json_round_trip() {
        use matrix_sdk_crypto::{AttachmentDecryptor, AttachmentEncryptor, MediaEncryptionInfo};
        use std::io::{Cursor, Read};

        let plaintext = b"nobilis encrypted attachment round-trip probe".to_vec();

        // --- upload_encrypted_media_message's half ---
        let mut src = Cursor::new(plaintext.clone());
        let mut encryptor = AttachmentEncryptor::new(&mut src);
        let mut ciphertext = Vec::new();
        encryptor.read_to_end(&mut ciphertext).expect("encrypting");
        let media_info = encryptor.finish();

        let mut file = serde_json::to_value(&media_info).expect("serializing encryption info");
        file["url"] = serde_json::json!("mxc://example.org/fake-media-id");
        // A real server response would also be missing "hashes"/"key"
        // ordering guarantees etc - round-tripping through an actual
        // serde_json::Value (not the original struct) is the point, since
        // that's genuinely what flows over the wire as this message's
        // content.file.
        let file_over_the_wire: Value = serde_json::from_str(&file.to_string()).expect("re-parsing as if received fresh over the wire");

        // --- cached_encrypted_media_path's half ---
        let mut encryption_info = file_over_the_wire.clone();
        encryption_info.as_object_mut().expect("file is an object").remove("url");
        let encryption_info: MediaEncryptionInfo = serde_json::from_value(encryption_info).expect("deserializing encryption info back out");

        let mut ciphertext_cursor = Cursor::new(ciphertext);
        let mut decryptor = AttachmentDecryptor::new(&mut ciphertext_cursor, encryption_info).expect("building decryptor");
        let mut decrypted = Vec::new();
        decryptor.read_to_end(&mut decrypted).expect("decrypting");

        assert_eq!(decrypted, plaintext, "decrypted bytes don't match the original plaintext");
    }
}

#[cfg(test)]
mod voice_tests {
    use super::{build_attachment, media_msgtype_and_mime};
    use serde_json::json;

    /// A voice message as Element sends one: the marker, the length in
    /// milliseconds, and a waveform of integers up to 1024.
    fn spoken() -> serde_json::Value {
        json!({
            "msgtype": "m.audio",
            "body": "Voice message.ogg",
            "url": "mxc://example.org/abc",
            "info": { "mimetype": "audio/ogg", "size": 23294, "duration": 6615 },
            "org.matrix.msc1767.audio": { "duration": 6615, "waveform": [0, 256, 512, 1024] },
            "org.matrix.msc3245.voice": {}
        })
    }

    #[test]
    fn element_voice_message_is_read_as_one() {
        let att = build_attachment(&spoken(), None, None);
        assert!(att.voice, "the MSC3245 marker was not read");
        assert_eq!(att.kind, "audio");
        // Milliseconds there, seconds here.
        assert_eq!(att.duration_secs, Some(6.615));
        // 0, 256, 512, 1024 scaled from 0-1024 into bytes: 0, 64, 128, 255.
        assert_eq!(att.waveform.as_deref(), Some("AECA/w=="));
    }

    /// The same distinction Discord's flag draws: a sound file somebody
    /// attached is a file they sent, not something they said.
    #[test]
    fn an_ordinary_audio_attachment_is_not_a_voice_message() {
        let mut ordinary = spoken();
        ordinary.as_object_mut().unwrap().remove("org.matrix.msc3245.voice");
        ordinary.as_object_mut().unwrap().remove("org.matrix.msc1767.audio");
        let att = build_attachment(&ordinary, None, None);
        assert!(!att.voice);
        assert_eq!(att.kind, "audio");
        // The length still comes through, because `info.duration` is older
        // than any of this and an audio player can use it.
        assert_eq!(att.duration_secs, Some(6.615));
        assert_eq!(att.waveform, None);
    }

    /// The round trip, without a homeserver: what the send path writes has to
    /// come back through the parser as the thing it was meant to be. The
    /// parser is already known to read Element's real shape, so agreeing with
    /// it is the strongest offline statement available about what will land
    /// in a room.
    #[test]
    fn what_is_sent_is_what_is_read_back() {
        use super::mark_as_voice;

        // What an upload leaves behind, before it is marked.
        let mut content = json!({
            "msgtype": "m.audio",
            "body": "Voice message",
            "url": "mxc://example.org/xyz",
            "info": { "mimetype": "audio/ogg" }
        });
        // A recording as this daemon produces one: seconds, and bytes.
        let waveform: Vec<u8> = vec![0, 64, 128, 255];
        mark_as_voice(&mut content, 2.5, &waveform);

        assert!(content["org.matrix.msc3245.voice"].is_object(), "no marker: {content}");
        assert_eq!(content["org.matrix.msc1767.audio"]["duration"], 2500);
        assert_eq!(content["info"]["duration"], 2500);
        // Bytes on the way out, integers up to 1024 on the wire.
        assert_eq!(content["org.matrix.msc1767.audio"]["waveform"], json!([0, 257, 514, 1024]));

        let att = build_attachment(&content, None, None);
        assert!(att.voice);
        assert_eq!(att.duration_secs, Some(2.5));
        // And back to bytes, within the rounding two conversions cost.
        use base64::Engine;
        let back = base64::engine::general_purpose::STANDARD
            .decode(att.waveform.expect("a waveform")) 
            .expect("valid base64");
        assert_eq!(back.len(), waveform.len());
        for (sent, read) in waveform.iter().zip(back.iter()) {
            assert!((*sent as i16 - *read as i16).abs() <= 1, "{sent} came back as {read}");
        }
    }

    /// A voice message recorded here is an Ogg, and saying it is an mp3 is
    /// how it ends up unplayable in somebody else's client.
    #[test]
    fn an_ogg_is_uploaded_as_an_ogg() {
        assert_eq!(media_msgtype_and_mime("ogg"), ("m.audio", "audio/ogg"));
        assert_eq!(media_msgtype_and_mime("mp3"), ("m.audio", "audio/mpeg"));
        assert_eq!(media_msgtype_and_mime("wav"), ("m.audio", "audio/wav"));
    }
}
