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
use serde_json::Value;

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
    /// What an avatar upload is known by afterwards.
    sha: Option<String>,
}

#[derive(Deserialize)]
struct Begun {
    ugcid: serde_json::Value,
    url_host: String,
    url_path: String,
    /// A number on the wire (`1`), where the client reads it as a truth value.
    #[serde(default, deserialize_with = "truthy")]
    use_https: bool,
    #[serde(default)]
    request_headers: Vec<Header>,
}

fn truthy<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        serde_json::Value::String(s) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    })
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
pub(super) async fn login(link: &Link, key: &str, steamid: u64, refresh_token: &str) -> Result<(reqwest::Client, String, String), String> {
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
        .text("file_name", format!("{}_probe_check.png", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default()))
        .text("file_sha", (0..40).map(|_| format!("{:x}", rand::thread_rng().gen_range(0..16))).collect::<String>())
        .text("file_image_width", "10")
        .text("file_image_height", "10")
        .text("file_type", "image/png");
    let answer = site(&http, &cookie, &format!("{SITE}/chat/beginfileupload/?l=english"), form).await?;
    let shape = answer.result.as_ref().map(|r| match r {
        serde_json::Value::Object(o) => o.iter().map(|(k, v)| format!("{k}:{}", match v { serde_json::Value::String(_) => "string", serde_json::Value::Number(_) => "number", serde_json::Value::Bool(_) => "bool", serde_json::Value::Array(_) => "array", serde_json::Value::Object(_) => "object", serde_json::Value::Null => "null" })).collect::<Vec<_>>().join(", "),
        _ => "not an object".to_string(),
    });
    Ok(format!("answer keys: {shape:?}; top-level hmac {} timestamp {}", answer.hmac.is_some(), answer.timestamp.is_some()))
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
    // Random, as the official client sends it. Not the file's own hash, for
    // all that the field is called one: Steam refuses a second upload that
    // names a hash it has already been given, so the same picture sent twice
    // would be turned away the second time.
    let sha = (0..40).map(|_| format!("{:x}", rand::thread_rng().gen_range(0..16))).collect::<String>();
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
    let begun: Begun = serde_json::from_value(answer.result.clone().unwrap_or_default()).ok()
        .filter(|b: &Begun| !b.url_host.is_empty())
        .ok_or_else(|| "Steam would not take the file just now - it did not say where to put it".to_string())?;

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

/// What a group's picture may be, as the client's own picker says.
pub const AVATAR_TYPES: &[&str] = &["jpg", "jpeg", "png", "gif"];

/// Sends a picture for a chat group to the site, which keeps it and answers with what it is
/// known by; the group is then told to use that (`SetChatRoomGroupAvatar`).
pub async fn upload_avatar(link: &Link, key: &str, steamid: u64, refresh_token: &str, path: &str) -> Result<Vec<u8>, String> {
    let file_name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("avatar").to_string();
    let extension = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let mime = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        _ => return Err(format!("a group's picture is one of {}", AVATAR_TYPES.join(", "))),
    };
    let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading {file_name}: {e}"))?;
    if bytes.is_empty() {
        return Err(format!("{file_name} is empty"));
    }
    let (http, cookie, session_id) = login(link, key, steamid, refresh_token).await?;
    let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name).mime_str(mime).map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new().text("sessionid", session_id).part("avatar", part);
    let answer = site(&http, &cookie, &format!("{SITE}/chat/avatarfileupload/"), form).await?;
    let sha = answer.sha.filter(|s| s.len() == 40).ok_or("Steam took the picture and did not say what it is called")?;
    (0..sha.len()).step_by(2).map(|i| u8::from_str_radix(&sha[i..i + 2], 16).map_err(|_| "Steam named the picture in a way that is not understood".to_string())).collect()
}

/// An answer's text. The site compresses what it likes whatever it is asked,
/// and a refusal's reason is in the body, so it is unpacked here.
pub(super) fn plain(raw: &[u8]) -> String {
    use std::io::Read;
    if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = String::new();
        if flate2::read::GzDecoder::new(raw).read_to_string(&mut out).is_ok() {
            return out;
        }
    }
    String::from_utf8_lossy(raw).into_owned()
}

/// The names somebody has had, newest first, each with when Steam says they
/// took it - in Steam's own words for the moment, which are not a date a
/// program can rely on ("12 Oct, 2025 @ 6:03pm") and are shown as given.
pub async fn aliases(link: &Link, key: &str, steamid: u64, refresh_token: &str, friend: u64) -> Result<Vec<(String, String)>, String> {
    let (http, cookie, _) = login(link, key, steamid, refresh_token).await?;
    let response = http
        .get(format!("{SITE}/profiles/{friend}/ajaxaliases?l=english"))
        .header("Cookie", cookie)
        .header("Referer", format!("{SITE}/profiles/{friend}"))
        .send()
        .await
        .map_err(|e| format!("could not reach Steam's website: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Steam's website refused it ({})", response.status()));
    }
    let raw = response.bytes().await.map_err(|e| e.to_string())?;
    Ok(read_aliases(&plain(&raw)))
}

/// Reads the answer: a list of `{newname, timechanged}`.
pub fn read_aliases(text: &str) -> Vec<(String, String)> {
    let Ok(Value::Array(list)) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    list.iter()
        .filter_map(|entry| {
            let name = entry["newname"].as_str()?.trim();
            let when = entry["timechanged"].as_str().unwrap_or_default().trim();
            (!name.is_empty()).then(|| (name.chars().take(64).collect(), when.chars().take(40).collect()))
        })
        .collect()
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
    // Never the body: it carries the signed address the file is sent to.
    tracing::debug!("steam web: {} -> {status}, success {:?}", url.split('?').next().unwrap_or(url), answer.success);
    if !status.is_success() || answer.success.is_some_and(|s| s != 1) {
        return Err(answer.message.filter(|m| !m.is_empty()).unwrap_or_else(|| match (status.as_u16(), answer.success) {
            // The site answers in Steam's own result codes, which say why far better than the status.
            (_, Some(code)) if code > 1 => format!("Steam refused it: {}", super::eresult::describe(code as i32)),
            (401 | 403, _) => "Steam's website did not accept this sign-in".to_string(),
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
    fn steams_answer_to_the_first_step_is_read_as_it_is_sent() {
        // As Steam sends it: the id a string, and "use https" a number.
        let said = serde_json::json!({
            "storage_system": 1, "ugcid": "13438947538884666122", "timestamp": 1791849742,
            "url_host": "files.example.test", "url_path": "/a/b", "use_https": 1,
            "request_headers": [{ "name": "x-goog-meta", "value": "v" }], "success": 1
        });
        let begun: Begun = serde_json::from_value(said).expect("what Steam sends");
        assert!(begun.use_https);
        assert_eq!((begun.url_host.as_str(), begun.url_path.as_str(), begun.request_headers.len()), ("files.example.test", "/a/b", 1));
        let no: Result<Begun, _> = serde_json::from_value(serde_json::json!({ "url_host": null, "url_path": null, "ugcid": null, "request_headers": [], "success": 10 }));
        assert!(no.is_err(), "a refusal has no place to put anything");
        let plain: Begun = serde_json::from_value(serde_json::json!({ "ugcid": 5, "url_host": "h", "url_path": "/p", "use_https": false })).unwrap();
        assert!(!plain.use_https);
    }

    #[test]
    fn a_name_history_is_read_as_names_and_the_moments_they_were_taken() {
        let said = r#"[{"newname":"Gabe","timechanged":"12 Oct, 2025 @ 6:03pm"},{"newname":"  ","timechanged":"x"},{"newname":"Old Name","timechanged":"3 Mar, 2019 @ 1:00am"},{"newname":"No Time"}]"#;
        assert_eq!(
            read_aliases(said),
            vec![("Gabe".to_string(), "12 Oct, 2025 @ 6:03pm".to_string()), ("Old Name".to_string(), "3 Mar, 2019 @ 1:00am".to_string()), ("No Time".to_string(), String::new())]
        );
        assert!(read_aliases("not json").is_empty());
        assert!(read_aliases("{}").is_empty());
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
