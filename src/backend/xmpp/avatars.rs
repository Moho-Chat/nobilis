//! Pictures of people: the avatar a contact has published (XEP-0084, in PEP, announced to everyone who is told of their
//! changes), or, from clients and servers that predate it, the one in their vCard (XEP-0054) whose hash rides on their
//! presence (XEP-0153).
//!
//! A picture is named by its SHA-1, which is also its identity: the same picture is the same file for everyone who has it,
//! and what arrives is checked against the hash that announced it before it is kept.

use super::session::Handle;
use super::xml::Attr;
use super::{ns, roster};
use crate::state::AppState;
use base64::Engine;
use sha1::{Digest, Sha1};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;

/// The most a picture may be: the specification suggests keeping them small, and one this large is not a picture.
const MOST: usize = 4 * 1024 * 1024;

pub fn sha1_hex(bytes: &[u8]) -> String {
    Sha1::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// What file extension a picture of this media type gets.
fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

/// The media type of a picture from its first bytes: what it says it is is not trusted.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

fn path_for(hash: &str, media_type: &str) -> std::path::PathBuf {
    crate::media_cache::dir(super::media::AVATARS).join(format!("{hash}.{}", extension(media_type)))
}

/// A picture already kept, by its hash.
fn kept(hash: &str) -> Option<String> {
    ["png", "jpg", "gif", "webp"].iter().map(|e| crate::media_cache::dir(super::media::AVATARS).join(format!("{hash}.{e}"))).find(|p| p.exists()).map(|p| format!("file://{}", p.display()))
}

/// The id of the picture a metadata notification says is current, if any. An `info` with a `url` is a picture kept
/// elsewhere, which is not fetched here.
pub fn current(metadata: &Element) -> Option<String> {
    metadata.children().find(|c| c.name() == "info" && c.attr("url").is_none()).and_then(|i| i.attr("id")).map(str::to_string)
}

/// Somebody's picture changed (or was said to be): a PEP notification of the metadata node.
pub async fn metadata_event(state: &AppState, handle: &Handle, from: &BareJid, items: &Element) {
    let item = items.children().find(|c| c.name() == "item");
    let metadata = item.and_then(|i| i.children().find(|c| c.is("metadata", ns::AVATAR_METADATA)));
    match metadata.and_then(current) {
        Some(hash) => ensure(state, handle, from, &hash, ns::AVATAR_DATA).await,
        None => forget(state, handle, from),
    }
}

/// Somebody has no picture any more.
fn forget(state: &AppState, handle: &Handle, who: &BareJid) {
    handle.session().avatars.remove(who);
    roster::publish_presence(state, handle, who);
}

/// Makes sure the picture with this hash is the one shown for this person, fetching it if it is not kept.
pub async fn ensure(state: &AppState, handle: &Handle, who: &BareJid, hash: &str, node: &str) {
    if !hash.chars().all(|c| c.is_ascii_hexdigit()) || hash.len() != 40 {
        return;
    }
    let url = match kept(hash) {
        Some(url) => url,
        None => {
            let bytes = match node {
                ns::AVATAR_DATA => fetch_pep(handle, who, hash).await,
                _ => fetch_vcard(handle, who).await,
            };
            let Some(bytes) = bytes else {
                tracing::debug!("xmpp[{}]: no picture could be read for {who}", handle.account_id);
                return;
            };
            if sha1_hex(&bytes) != hash {
                tracing::debug!("xmpp[{}]: the picture {who} has does not match its hash", handle.account_id);
                return;
            }
            let Some(media_type) = sniff(&bytes) else {
                tracing::debug!("xmpp[{}]: the picture of {who} is not a kind moho shows", handle.account_id);
                return;
            };
            let path = path_for(hash, media_type);
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::write(&path, &bytes).is_err() {
                return;
            }
            format!("file://{}", path.display())
        }
    };
    handle.session().avatars.insert(who.clone(), url.clone());
    if let Some(b) = roster::find_buffer(state, handle, who) {
        state.runtime.set_buffer_avatar(state, &b.id, &url);
    }
    roster::publish_presence(state, handle, who);
}

async fn fetch_pep(handle: &Handle, who: &BareJid, hash: &str) -> Option<Vec<u8>> {
    let item = Element::builder("item", ns::PUBSUB).at("id", hash).build();
    let items = Element::builder("items", ns::PUBSUB).at("node", ns::AVATAR_DATA).append(item).build();
    let q = Element::builder("pubsub", ns::PUBSUB).append(items).build();
    let answer = match handle.request(Some(Jid::from(who.clone())), false, q).await {
        Ok(Some(a)) => a,
        other => {
            tracing::debug!("xmpp[{}]: asking {who} for their picture: {other:?}", handle.account_id);
            return None;
        }
    };
    let data = answer.children().find(|c| c.name() == "items")?.children().find(|c| c.name() == "item")?.children().find(|c| c.is("data", ns::AVATAR_DATA))?.text();
    let decoded = decode(&data);
    if decoded.is_none() {
        tracing::debug!("xmpp[{}]: the picture of {who} did not decode ({} characters)", handle.account_id, data.len());
    }
    decoded
}

/// The picture in somebody's vCard (the old way).
async fn fetch_vcard(handle: &Handle, who: &BareJid) -> Option<Vec<u8>> {
    let answer = handle.request(Some(Jid::from(who.clone())), false, super::xml::empty("vCard", ns::VCARD)).await.ok()??;
    let photo = answer.children().find(|c| c.name() == "PHOTO")?;
    decode(&photo.children().find(|c| c.name() == "BINVAL")?.text())
}

fn decode(base64_text: &str) -> Option<Vec<u8>> {
    let clean: String = base64_text.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(clean).ok()?;
    (!bytes.is_empty() && bytes.len() <= MOST).then_some(bytes)
}

/// A presence that carries the hash of the sender's picture (XEP-0153): fetched when PEP has not already told us.
pub async fn presence_hash(state: &AppState, handle: &Handle, from: &BareJid, x: &Element) {
    let Some(photo) = x.children().find(|c| c.name() == "photo").map(|p| p.text()).filter(|p| !p.is_empty()) else { return };
    if handle.session().avatars.contains_key(from) && handle.session().avatar_source.get(from).is_some_and(|s| s == "pep") {
        return;
    }
    handle.session().avatar_source.insert(from.clone(), "vcard".into());
    ensure(state, handle, from, &photo, ns::VCARD).await;
}

/// Sets the account's own picture (XEP-0084): the data and then the metadata that points at it. An empty path takes the picture off.
pub async fn set_own(handle: &Handle, path: &str) -> Result<(), String> {
    let publish = |node: &str, id: &str, payload: Element| {
        let item = Element::builder("item", ns::PUBSUB).at("id", id).append(payload).build();
        let publish = Element::builder("publish", ns::PUBSUB).at("node", node).append(item).build();
        Element::builder("pubsub", ns::PUBSUB).append(publish).build()
    };
    if path.is_empty() {
        let meta = publish(ns::AVATAR_METADATA, "current", super::xml::empty("metadata", ns::AVATAR_METADATA));
        return handle.request(None, true, meta).await.map(|_| ()).map_err(|e| e.words());
    }
    let bytes = std::fs::read(path.trim_start_matches("file://")).map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() > MOST {
        return Err("That picture is too large: use one under 4 MB".to_string());
    }
    let media_type = sniff(&bytes).ok_or("That is not a picture moho can use (PNG, JPEG, GIF or WebP)")?;
    let hash = sha1_hex(&bytes);
    let data = Element::builder("data", ns::AVATAR_DATA).append(base64::engine::general_purpose::STANDARD.encode(&bytes)).build();
    handle.request(None, true, publish(ns::AVATAR_DATA, &hash, data)).await.map_err(|e| e.words())?;
    let (w, h) = imagesize::blob_size(&bytes).map(|d| (d.width, d.height)).unwrap_or((0, 0));
    let mut info = Element::builder("info", ns::AVATAR_METADATA).at("id", hash.as_str()).at("type", media_type).at("bytes", bytes.len().to_string());
    if w > 0 {
        info = info.at("width", w.to_string()).at("height", h.to_string());
    }
    let metadata = Element::builder("metadata", ns::AVATAR_METADATA).append(info.build()).build();
    handle.request(None, true, publish(ns::AVATAR_METADATA, &hash, metadata)).await.map(|_| ()).map_err(|e| e.words())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn the_hash_of_a_picture_is_the_hex_sha1_every_client_uses() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn what_a_file_says_it_is_is_not_taken_on_trust() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 0, 0]), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'><script/></svg>"), None, "an SVG can carry script and is not accepted");
        assert_eq!(sniff(b"MZ\x90\0"), None);
    }

    #[test]
    fn the_current_picture_is_the_one_kept_in_pep_and_a_url_is_somewhere_else() {
        let m = Element::from_str("<metadata xmlns='urn:xmpp:avatar:metadata'><info id='aaaa' url='https://x/y.png' type='image/png' bytes='3'/><info id='bbbb' type='image/png' bytes='3'/></metadata>").unwrap();
        assert_eq!(current(&m).as_deref(), Some("bbbb"));
        assert_eq!(current(&Element::from_str("<metadata xmlns='urn:xmpp:avatar:metadata'/>").unwrap()), None, "an empty one is no picture");
    }

    #[test]
    fn picture_data_is_base64_with_line_breaks_and_a_size_limit() {
        assert_eq!(decode("YWJj\n ZGVm").unwrap(), b"abcdef");
        assert!(decode("").is_none());
        assert!(decode("!!!").is_none());
    }
}
