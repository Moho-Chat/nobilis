//! Files in a conversation: pictures and short videos.
//!
//! Steam's chat takes a file through its website, not the connection that
//! carries the messages. The official client does the same three steps, and so
//! does this:
//!
//! 1. Tell `steamcommunity.com/chat/beginfileupload` what is coming - size,
//!    name, type, picture size. It answers with where to put it.
//! 2. `PUT` the file there.
//! 3. Tell `steamcommunity.com/chat/commitfileupload` it arrived, and to whom.
//!    This is what sends the message: Steam writes it into the conversation
//!    itself, and the other side - and our own other sessions, this one
//!    included - hear it the way they hear any message. The spoiler flag is a
//!    field here, so a picture is hidden by Steam and not by anything moho
//!    wraps around it.
//!
//! The website knows an account by cookies, which a refresh token makes: it
//! can be exchanged, over the same connection, for a short-lived access token,
//! and the site accepts that as the login cookie.
//!
//! The types and the size are what the official client checks before it will
//! start, read out of the client: a file Steam will not take is refused here
//! with the same words, rather than after the upload.

use super::link::Link;
use super::proto::{GenerateAccessTokenRequest, GenerateAccessTokenResponse};
use crate::upload::{Phase, Progress};
use rand::Rng;
use serde::Deserialize;
use sha1::{Digest, Sha1};

const SITE: &str = "https://steamcommunity.com";

/// The most a file in chat may be, in bytes: the client's own 30 MB.
pub const MAX_BYTES: u64 = 30 * 1024 * 1024;

/// What the client accepts: its extensions, and the type it tells the site.
pub const TYPES: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mpg", "video/mpeg"),
    ("mpeg", "video/mpeg"),
    ("ogv", "video/ogg"),
];

/// The file's type if Steam takes it, going by its extension as the client does.
pub fn type_of(file_name: &str) -> Option<&'static str> {
    let extension = file_name.rsplit_once('.')?.1.to_ascii_lowercase();
    TYPES.iter().find(|(e, _)| *e == extension).map(|(_, t)| *t)
}

/// Why this file will not go, or nothing if it will.
pub fn refusal(file_name: &str, size: u64) -> Option<String> {
    if size == 0 {
        return Some(format!("{file_name} is empty"));
    }
    if type_of(file_name).is_none() {
        let extension = file_name.rsplit_once('.').map(|(_, e)| e).unwrap_or(file_name);
        let known = TYPES.iter().map(|(e, _)| *e).collect::<Vec<_>>().join(", ");
        return Some(format!("Steam does not take .{extension} files in chat - it takes {known}"));
    }
    if size > MAX_BYTES {
        return Some(format!("{file_name} is over the {} MB Steam allows in chat", MAX_BYTES / 1024 / 1024));
    }
    None
}

/// The picture's size in pixels, or nothing for a file that is not one the
/// reader knows (a video) - which Steam is told as zero, as the client does.
fn dimensions(bytes: &[u8]) -> (usize, usize) {
    imagesize::blob_size(bytes).map(|s| (s.width, s.height)).unwrap_or((0, 0))
}

/// What is shown of a failure: Steam's own words where it gave any.
#[derive(Deserialize, Default)]
struct Answer {
    success: Option<i64>,
    message: Option<String>,
    hmac: Option<String>,
    timestamp: Option<i64>,
    result: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Begun {
    ugcid: serde_json::Value,
    url_host: String,
    url_path: String,
    #[serde(default)]
    use_https: bool,
    #[serde(default)]
    request_headers: Vec<Header>,
}

#[derive(Deserialize)]
struct Header {
    name: String,
    value: String,
}

/// The cookies the site reads a login from, for `steamid`.
fn cookies(steamid: &str, access_token: &str, session_id: &str) -> String {
    format!("sessionid={session_id}; steamLoginSecure={steamid}%7C%7C{access_token}")
}

/// A short-lived token for the website, made from the refresh token.
pub async fn access_token(link: &Link, steamid: u64, refresh_token: &str) -> Result<String, String> {
    let request = GenerateAccessTokenRequest { refresh_token: Some(refresh_token.to_string()), steamid: Some(steamid), renewal_type: Some(0) };
    let answer: GenerateAccessTokenResponse = link
        .call("Authentication.GenerateAccessTokenForApp#1", &request, true)
        .await
        .map_err(|e| format!("Steam would not give this account a web session: {e}"))?;
    answer.access_token.filter(|t| !t.is_empty()).ok_or_else(|| "Steam gave no web session for this account".to_string())
}

/// The website as this account: a client, the cookie it signs in with, and the
/// session id the forms repeat.
async fn login(link: &Link, key: &str, steamid: u64, refresh_token: &str) -> Result<(reqwest::Client, String, String), String> {
    let router = crate::net::route::router();
    let token = access_token(link, steamid, refresh_token).await?;
    let session_id = {
        let mut rng = rand::thread_rng();
        (0..24).map(|_| format!("{:x}", rng.gen_range(0..16))).collect::<String>()
    };
    let cookie = cookies(&steamid.to_string(), &token, &session_id);
    let http = router.client_if("steam", router.routed(key), |b| {
        b.timeout(std::time::Duration::from_secs(600)).user_agent(concat!("moho/", env!("CARGO_PKG_VERSION")))
    });
    Ok((http, cookie, session_id))
}

/// Asks Steam whether this account may upload a picture, without sending
/// anything: the first step is a question, and nothing exists until the file is
/// put where the answer says. For the live check.
#[cfg(test)]
pub async fn may_upload(link: &Link, key: &str, steamid: u64, refresh_token: &str) -> Result<String, String> {
    let (http, cookie, session_id) = login(link, key, steamid, refresh_token).await?;
    let form = reqwest::multipart::Form::new()
        .text("sessionid", session_id)
        .text("l", "english")
        .text("file_size", "1000")
        .text("file_name", "probe_check.png")
        .text("file_sha", "0".repeat(40))
        .text("file_image_width", "10")
        .text("file_image_height", "10")
        .text("file_type", "image/png");
    let answer = site(&http, &cookie, &format!("{SITE}/chat/beginfileupload/?l=english"), form).await?;
    let host = answer.result.as_ref().and_then(|r| r.get("url_host")).and_then(|h| h.as_str()).unwrap_or("?");
    Ok(format!("yes - Steam would take the file at {host}"))
}

/// Sends one file to a friend, hidden behind a spoiler if asked. Returns
/// where Steam keeps it.
///
/// `key` is the account's id, which decides whether the traffic is routed.
#[allow(clippy::too_many_arguments)]
pub async fn send_file(
    link: &Link,
    key: &str,
    steamid: u64,
    refresh_token: &str,
    friend: &str,
    path: &str,
    spoiler: bool,
    progress: Option<&Progress>,
) -> Result<String, String> {
    let file_name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let size = tokio::fs::metadata(path).await.map_err(|e| format!("reading {file_name}: {e}"))?.len();
    if let Some(why) = refusal(&file_name, size) {
        return Err(why);
    }
    let mime = type_of(&file_name).unwrap_or("application/octet-stream");
    if let Some(p) = progress {
        p.at(Phase::Preparing, size as usize, "steam");
    }
    let router = crate::net::route::router();
    if router.routed(key) {
        router.ready_for_general().await.map_err(|e| format!("reaching Steam by the route chosen: {e:#}"))?;
    }
    let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading {file_name}: {e}"))?;
    let (width, height) = dimensions(&bytes);
    // The official client sends a random value here and Steam does not check
    // it; the file's real hash is as good and means what the field says.
    let sha = Sha1::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
    let upload_name = format!("{}_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default(), file_name);

    let (http, cookie, session_id) = login(link, key, steamid, refresh_token).await?;
    let begin = reqwest::multipart::Form::new()
        .text("sessionid", session_id.clone())
        .text("l", "english")
        .text("file_size", size.to_string())
        .text("file_name", upload_name.clone())
        .text("file_sha", sha.clone())
        .text("file_image_width", width.to_string())
        .text("file_image_height", height.to_string())
        .text("file_type", mime.to_string());
    let answer = site(&http, &cookie, &format!("{SITE}/chat/beginfileupload/?l=english"), begin).await?;
    let (hmac, timestamp) = (answer.hmac.clone().unwrap_or_default(), answer.timestamp.unwrap_or_default());
    let begun: Begun = serde_json::from_value(answer.result.clone().unwrap_or_default()).map_err(|_| "Steam did not say where to put the file".to_string())?;

    if let Some(p) = progress {
        p.at(Phase::Sending, bytes.len(), "steam");
    }
    let url = format!("{}://{}{}", if begun.use_https { "https" } else { "http" }, begun.url_host, begun.url_path);
    let mut put = http.put(&url).body(bytes);
    for h in begun.request_headers.iter().filter(|h| !h.name.eq_ignore_ascii_case("content-length") && !h.name.eq_ignore_ascii_case("host")) {
        put = put.header(h.name.as_str(), h.value.as_str());
    }
    let stored = put.send().await.map_err(|e| format!("sending {file_name} to Steam: {e}"))?;
    if !stored.status().is_success() {
        return Err(format!("Steam's file server refused {file_name} ({})", stored.status()));
    }

    if let Some(p) = progress {
        p.at(Phase::Waiting, size as usize, "steam");
    }
    let ugcid = match &begun.ugcid {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let commit = reqwest::multipart::Form::new()
        .text("sessionid", session_id)
        .text("l", "english")
        .text("file_name", upload_name)
        .text("file_sha", sha)
        .text("success", "1")
        .text("ugcid", ugcid)
        .text("file_type", mime.to_string())
        .text("file_image_width", width.to_string())
        .text("file_image_height", height.to_string())
        .text("timestamp", timestamp.to_string())
        .text("hmac", hmac)
        .text("friend_steamid", friend.to_string())
        .text("spoiler", if spoiler { "1" } else { "0" });
    let done = site(&http, &cookie, &format!("{SITE}/chat/commitfileupload/"), commit).await?;
    let result = done.result.unwrap_or_default();
    // Valve's own nesting: the call succeeded, and inside it there is a second
    // verdict on whether the file did.
    if result.get("success").and_then(|v| v.as_i64()).is_some_and(|v| v != 1) {
        return Err(done.message.unwrap_or_else(|| "Steam did not send the file".to_string()));
    }
    Ok(result.pointer("/details/url").and_then(|v| v.as_str()).unwrap_or_default().to_string())
}

/// An answer's text. The site compresses what it likes whatever it is asked,
/// and a refusal's reason is in the body, so it is unpacked here.
fn plain(raw: &[u8]) -> String {
    use std::io::Read;
    if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = String::new();
        if flate2::read::GzDecoder::new(raw).read_to_string(&mut out).is_ok() {
            return out;
        }
    }
    String::from_utf8_lossy(raw).into_owned()
}

/// One call to the site, read as the client reads it: the body is JSON even
/// when the status is an error, and its `message` is what to show.
async fn site(http: &reqwest::Client, cookie: &str, url: &str, form: reqwest::multipart::Form) -> Result<Answer, String> {
    let response = http
        .post(url)
        .header("Cookie", cookie)
        .header("Referer", format!("{SITE}/chat/"))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("could not reach Steam's website: {e}"))?;
    let status = response.status();
    let raw = response.bytes().await.unwrap_or_default();
    let text = plain(&raw);
    let answer: Answer = serde_json::from_str(&text).unwrap_or_default();
    tracing::debug!("steam web: {url} -> {status}, success {:?}, body {:?}", answer.success, text.chars().take(200).collect::<String>());
    if !status.is_success() || answer.success.is_some_and(|s| s != 1) {
        return Err(answer.message.filter(|m| !m.is_empty()).unwrap_or_else(|| match status.as_u16() {
            401 | 403 => "Steam's website did not accept this sign-in".to_string(),
            _ => format!("Steam's website refused it ({status})"),
        }));
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_types_are_the_ones_the_client_takes() {
        for name in ["a.jpg", "a.JPEG", "a.png", "a.gif", "a.webm", "a.mpg", "a.mp4", "a.mpeg", "a.ogv", "a.webp", "a.avif"] {
            assert!(type_of(name).is_some(), "{name}");
        }
        assert_eq!(type_of("clip.MP4"), Some("video/mp4"));
        assert_eq!(type_of("photo.jpg"), Some("image/jpeg"));
        assert_eq!(type_of("paper.pdf"), None);
        assert_eq!(type_of("noextension"), None);
    }

    #[test]
    fn a_file_steam_will_not_take_is_refused_in_its_own_terms() {
        assert!(refusal("a.png", 100).is_none());
        assert!(refusal("a.png", MAX_BYTES).is_none());
        assert!(refusal("a.png", MAX_BYTES + 1).unwrap().contains("30 MB"));
        assert!(refusal("a.pdf", 10).unwrap().contains(".pdf"));
        assert!(refusal("a.png", 0).unwrap().contains("empty"));
    }

    #[test]
    fn the_login_is_two_cookies() {
        assert_eq!(cookies("7656", "tok", "abc"), "sessionid=abc; steamLoginSecure=7656%7C%7Ctok");
    }

    #[test]
    fn a_picture_has_a_size_and_anything_else_has_none() {
        let png = image::RgbaImage::new(3, 2);
        let mut bytes = Vec::new();
        png.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        assert_eq!(dimensions(&bytes), (3, 2));
        assert_eq!(dimensions(b"not a picture"), (0, 0));
    }
}
