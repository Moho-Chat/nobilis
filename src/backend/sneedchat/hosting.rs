//! The chat's own file hosting.
//!
//! Ephyra Chat takes files itself: the composer's paperclip posts the raw
//! bytes to the forum's `/chat/files` and gets back an attachment id, which
//! goes into the message as `[file=<id>]label[/file]` and which the chat
//! draws as a tile (a thumbnail, a video poster, a player). Nothing leaves
//! Kiwi Farms - no third-party host, no link to unfurl - so this is where a
//! Sneedchat attachment goes unless the person chose another host.
//!
//! Same-origin and signed in, exactly as the page's own script does it
//! (`startUpload` in the chat's `ephyra-chat.js`): the forum session cookie
//! and the page's CSRF token, with the room and a label in the query.

use super::*;
use serde_json::Value;
use std::sync::Arc;

/// What the chat page says about uploading: where to send a file, and the
/// size the site will take. Read from the page each time - the token in it
/// belongs to the session, and an upload is rare enough that one more page
/// is no cost.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Hosting {
    url: String,
    max_bytes: Option<u64>,
    token: String,
}

/// The upload endpoint, size cap and token off a chat page.
pub(super) fn read_hosting(page: &str) -> Option<Hosting> {
    // The room root carries its own copy of the form token; the page's `data-csrf` is the same value.
    let token = protocol::attr(page, "data-chat-csrf-token").or_else(|| protocol::attr(page, "data-csrf"))?;
    let url = protocol::attr(page, "data-chat-upload-url").filter(|u| !u.trim().is_empty())?;
    let max_bytes = protocol::attr(page, "data-chat-upload-max-bytes").and_then(|n| n.trim().parse().ok()).filter(|n| *n > 0);
    Some(Hosting { url: protocol::unescape_html(&url), max_bytes, token })
}

/// The tag a stored file is written into a message as.
pub(super) fn file_tag(attachment: &str, label: &str) -> String {
    format!("[file={attachment}]{label}[/file]")
}

/// What the chat said to an upload.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Stored { attachment: String },
    Refused { code: String, retry_after: Option<u64> },
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// Reads an upload's answer: a 2xx naming an attachment, or the common error
/// envelope's code. Anything else - a proxy's 413, a gate page - is generic.
pub(super) fn outcome(status: u16, body: &str, retry_after: Option<&str>) -> Outcome {
    let json: Option<Value> = serde_json::from_str(body).ok();
    if (200..300).contains(&status) && status != super::pow::GATE_STATUS {
        return match json.as_ref().and_then(|j| j["attachment_uuid"].as_str()).filter(|u| is_uuid(u)) {
            Some(uuid) => Outcome::Stored { attachment: uuid.to_ascii_lowercase() },
            None => Outcome::Refused { code: "generic".into(), retry_after: None },
        };
    }
    let code = json.as_ref().and_then(|j| j["error"]["code"].as_str()).unwrap_or("generic");
    let known = ["file_too_large", "file_type_refused", "file_quota_exceeded", "file_refused", "uploads_unavailable", "rate_limited"];
    Outcome::Refused {
        code: if known.contains(&code) { code.to_string() } else { "generic".to_string() },
        retry_after: retry_after.and_then(|v| v.trim().parse().ok()),
    }
}

/// A refusal in words.
pub(super) fn refusal(code: &str, retry_after: Option<u64>, status: u16) -> String {
    match code {
        "file_too_large" => "that file is over the chat's size limit".to_string(),
        "file_type_refused" => "the chat does not take that type of file".to_string(),
        "file_quota_exceeded" => "you have used up your upload allowance on the chat".to_string(),
        "file_refused" => "the chat did not accept that file".to_string(),
        "uploads_unavailable" => "the chat's file hosting is unavailable right now".to_string(),
        "rate_limited" => match retry_after {
            Some(s) => format!("uploading too fast - the chat asks you to try again in {s} seconds"),
            None => "uploading too fast - try again in a moment".to_string(),
        },
        _ => format!("the chat could not store that file (HTTP {status})"),
    }
}

/// Whether a character would be written into a `[file]` label as itself.
fn label_char(c: char) -> char {
    match c {
        '[' => '(',
        ']' => ')',
        c if c.is_control() => ' ',
        c => c,
    }
}

/// How long `encodeURIComponent` makes this text: the forum asserts a label
/// in at most 255 bytes of that.
fn encoded_len(text: &str) -> usize {
    text.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) { 1 } else { c.len_utf8() * 3 }).sum()
}

const LABEL_MAX_ENCODED: usize = 255;

/// The label a file is posted under: its name as one line of plain text that
/// parses as a `[file]` label - no square brackets, no controls or
/// direction overrides, whitespace collapsed - kept to the length the forum
/// takes with its extension intact.
pub(super) fn label_for(file_name: &str) -> String {
    let cleaned: String = file_name
        .chars()
        .filter(|c| !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .map(label_char)
        .collect();
    let text = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "file".to_string();
    }
    if encoded_len(&text) <= LABEL_MAX_ENCODED {
        return text;
    }
    let (stem, ext) = match text.rfind('.') {
        Some(dot) if dot > 0 && text.len() - dot <= 17 => (&text[..dot], &text[dot..]),
        _ => (text.as_str(), ""),
    };
    let mut kept = String::new();
    for c in stem.chars() {
        if encoded_len(&kept) + encoded_len(&c.to_string()) + encoded_len(ext) > LABEL_MAX_ENCODED {
            break;
        }
        kept.push(c);
    }
    format!("{}{ext}", kept.trim())
}

/// The media type to send a file as, by its extension. The chat looks at the
/// bytes; this is only what a browser would have said.
fn content_type_of(file_name: &str) -> &'static str {
    match file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

/// A percent-encoding of one query value, as `encodeURIComponent` writes it.
fn encode_component(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A file stored by the chat, ready to be written into a message.
pub(super) struct Stored {
    pub tag: String,
}

/// Puts a file on the chat's own hosting for a room.
pub(super) async fn store(
    account_id: &str,
    room: &str,
    path: &str,
    progress: Option<&crate::upload::Progress>,
) -> Result<Stored> {
    use crate::upload::Phase;
    let chat = super::chat::chat(account_id);
    // The chat's uploads bit is only a hint: it says "away" while the chat's
    // core is, and is not always updated when it comes back (a reconnect
    // keeps the last word until the next frame). Refusing on it turned a
    // moment's absence into a failed send; the site's own answer to the
    // upload is the one that counts, and says the same thing if it is true.
    if *chat.uploads.lock().unwrap() == Some(false) {
        tracing::info!("sneedchat[{account_id}]: the chat says its uploads are away; trying anyway");
    }
    let session = chat.session.lock().unwrap().clone().ok_or_else(|| anyhow!("not connected to the chat right now"))?;
    let host = chat.host.lock().unwrap().clone();
    let base = format!("https://{host}");

    if let Some(progress) = progress {
        progress.at(Phase::Preparing, 0, "native");
    }
    let bytes = tokio::fs::read(path).await.with_context(|| format!("reading {path}"))?;
    if bytes.is_empty() {
        bail!("that file is empty");
    }
    let file_name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();

    // Where to send it, and the token that goes with it, from the chat's own page.
    let page = session.fetch(&format!("{base}/ephyra-chat/")).await.context("loading the chat page")?;
    let hosting = read_hosting(&page.body).ok_or_else(|| anyhow!("the chat page offers no file hosting - its markup may have changed"))?;
    if let Some(cap) = hosting.max_bytes {
        if bytes.len() as u64 > cap {
            bail!("that file is over the chat's {}MB limit", cap / 1024 / 1024);
        }
    }
    // Same origin as the page, or not at all: the token must not go anywhere else.
    let endpoint = if hosting.url.starts_with('/') && !hosting.url.starts_with("//") {
        format!("{base}{}", hosting.url)
    } else if hosting.url.starts_with(&format!("{base}/")) {
        hosting.url.clone()
    } else {
        bail!("the chat's upload address is on another site - not sending a file there");
    };

    let label = label_for(&file_name);
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    let url = format!("{endpoint}{sep}chat_room_uuid={}&label={}", encode_component(room), encode_component(&label));
    if let Some(progress) = progress {
        progress.at(Phase::Sending, bytes.len(), "native");
    }
    // How far the body has gone, said at most every 150 ms: the window turns
    // it into a percentage and a speed. When the last byte is out the wait is
    // the chat's, and is said to be.
    let counting = progress.cloned().map(|progress| {
        let last = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = std::time::Instant::now();
        Arc::new(move |sent: u64, total: u64| {
            let now = started.elapsed().as_millis() as u64;
            let done = sent >= total;
            if done {
                progress.sent(total, total, "native");
                progress.at(Phase::Waiting, total as usize, "native");
            } else if now.saturating_sub(last.load(std::sync::atomic::Ordering::Relaxed)) >= 150 {
                last.store(now, std::sync::atomic::Ordering::Relaxed);
                progress.sent(sent, total, "native");
            }
        }) as crate::net::webhelper::UploadFn
    });
    let resp = session
        .http
        .request_bytes(
            hyper::Method::POST,
            &url,
            Bytes::from(bytes),
            content_type_of(&file_name),
            &[("X-XF-CSRF-Token", &hosting.token), ("Accept", "application/json"), ("Origin", &base)],
            counting,
        )
        .await
        .context("uploading to the chat")?;
    let retry = resp.headers.get("retry-after").and_then(|v| v.to_str().ok());
    match outcome(resp.status, &resp.body, retry) {
        Outcome::Stored { attachment } => Ok(Stored { tag: file_tag(&attachment, &label) }),
        Outcome::Refused { code, retry_after } => bail!("{}", refusal(&code, retry_after, resp.status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<section class="ephyraChat" data-chat-root data-chat-upload-url="/chat/files" data-chat-upload-max-bytes="26214400" data-chat-csrf-token="1795193575,995b74d7a4cd6b527c448f6996799980" data-chat-room="01a1f0f8-2d19-79de-bdc6-37b3b32de6f0">"#;

    #[test]
    fn the_page_says_where_to_upload() {
        let h = read_hosting(PAGE).unwrap();
        assert_eq!(h, Hosting { url: "/chat/files".into(), max_bytes: Some(26214400), token: "1795193575,995b74d7a4cd6b527c448f6996799980".into() });
    }

    #[test]
    fn a_page_without_uploads_offers_none() {
        assert!(read_hosting(r#"<section data-chat-root data-chat-csrf-token="1,a">"#).is_none());
    }

    #[test]
    fn a_stored_file_is_named_by_the_attachment() {
        let body = r#"{"attachment_uuid":"01A1F9DA-F862-76BA-96AA-CD33035FF4B0","file_kind":"image"}"#;
        assert_eq!(outcome(201, body, None), Outcome::Stored { attachment: "01a1f9da-f862-76ba-96aa-cd33035ff4b0".into() });
        assert_eq!(file_tag("01a1f9da-f862-76ba-96aa-cd33035ff4b0", "a.png"), "[file=01a1f9da-f862-76ba-96aa-cd33035ff4b0]a.png[/file]");
    }

    #[test]
    fn a_2xx_without_an_attachment_is_not_a_success() {
        assert_eq!(outcome(200, "{}", None), Outcome::Refused { code: "generic".into(), retry_after: None });
        assert_eq!(outcome(200, r#"{"attachment_uuid":"nope"}"#, None), Outcome::Refused { code: "generic".into(), retry_after: None });
        // The gate answers in a file's place with a 203.
        assert!(matches!(outcome(203, "<html>", None), Outcome::Refused { .. }));
    }

    #[test]
    fn refusals_carry_the_chats_code() {
        let body = r#"{"error":{"code":"file_quota_exceeded","message":"x"}}"#;
        assert_eq!(outcome(403, body, None), Outcome::Refused { code: "file_quota_exceeded".into(), retry_after: None });
        let body = r#"{"error":{"code":"rate_limited"}}"#;
        assert_eq!(outcome(429, body, Some("7")), Outcome::Refused { code: "rate_limited".into(), retry_after: Some(7) });
        assert_eq!(outcome(413, "too big", None), Outcome::Refused { code: "generic".into(), retry_after: None });
        assert_eq!(outcome(400, r#"{"error":{"code":"something_new"}}"#, None), Outcome::Refused { code: "generic".into(), retry_after: None });
        assert!(refusal("rate_limited", Some(7), 429).contains("7 seconds"));
    }

    #[test]
    fn a_label_is_one_line_of_plain_text() {
        assert_eq!(label_for("photo.png"), "photo.png");
        assert_eq!(label_for("a [b].png"), "a (b).png");
        assert_eq!(label_for("two\nlines\t here.png"), "two lines here.png");
        assert_eq!(label_for("cat\u{202e}gpj.exe"), "catgpj.exe");
        assert_eq!(label_for("   "), "file");
    }

    #[test]
    fn a_long_label_is_cut_and_keeps_its_extension() {
        let long = format!("{}.png", "a".repeat(400));
        let label = label_for(&long);
        assert!(encoded_len(&label) <= LABEL_MAX_ENCODED);
        assert!(label.ends_with(".png"));
        // Multibyte text counts three per byte once encoded.
        let wide = format!("{}.mp4", "é".repeat(200));
        let label = label_for(&wide);
        assert!(encoded_len(&label) <= LABEL_MAX_ENCODED);
        assert!(label.ends_with(".mp4"));
    }

    #[test]
    fn query_values_are_encoded_like_a_browser_does() {
        assert_eq!(encode_component("a b&c.png"), "a%20b%26c.png");
        assert_eq!(encode_component("é"), "%C3%A9");
        assert_eq!(encode_component("it's (ok)"), "it's%20(ok)");
    }
}
