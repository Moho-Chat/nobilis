//! Files sent from one device to another, with no server keeping them (Jingle File Transfer, XEP-0234, over In-Band
//! Bytestreams, XEP-0261 with XEP-0047): what Dino, Gajim and Conversations do when there is no HTTP Upload, or when
//! somebody chooses it.
//!
//! Receiving: an offer from a contact (only a contact's: anybody else's is refused) waits on the Invitations tile; accepted,
//! the file arrives in blocks through the stream, is checked against the size and the hash it was offered with, kept with
//! the other received files, and shown in the conversation. Sending: to a device of theirs that says it takes such files, the
//! offer, then - once they accept - the file in blocks, then the end of the session.
//!
//! In-band is the slow way (base64 through the server, a request per block), and the one every client that does file
//! transfer has; the SOCKS5 transports (XEP-0260, XEP-0065) are not built.

use super::session::Handle;
use super::xml::{self, Attr};
use crate::state::AppState;
use base64::Engine;
use serde_json::json;
use sha2::Digest;
use tokio::sync::oneshot;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::stanza_error::DefinedCondition;
use tokio_xmpp::Stanza;

pub const FT: &str = "urn:xmpp:jingle:apps:file-transfer:5";
pub const IBB_T: &str = "urn:xmpp:jingle:transports:ibb:1";
pub const IBB: &str = "http://jabber.org/protocol/ibb";
const JINGLE: &str = "urn:xmpp:jingle:1";
const HASHES: &str = "urn:xmpp:hashes:2";
/// What this client says it does, for capabilities.
pub const FEATURES: &[&str] = &[FT, IBB_T, IBB];
/// The largest file taken or sent this way: in-band, every byte goes through the server as base64.
pub const MOST: u64 = 200 * 1024 * 1024;
const BLOCK: u16 = 4096;

/// What a file says it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileDesc {
    pub name: String,
    pub size: u64,
    pub media_type: String,
    /// The hash it was offered with: algorithm and base64.
    pub hash: Option<(String, String)>,
}

/// One transfer, either way.
#[derive(Debug, Default)]
pub struct Transfer {
    pub peer: Option<Jid>,
    pub outgoing: bool,
    pub file: FileDesc,
    /// The content's name and its creator, echoed in what is said about it.
    pub content: String,
    pub creator: String,
    pub ibb_sid: String,
    pub block: u16,
    /// What has arrived, and the block it came in.
    pub data: Vec<u8>,
    pub seq: Option<u16>,
    pub accepted: bool,
    pub buffer_id: String,
    /// The sender waiting for an answer to its offer.
    pub answer: Option<oneshot::Sender<bool>>,
}

/// Reads a `<file/>` of XEP-0234.
pub fn read_file(file: &Element) -> FileDesc {
    let text = |n: &str| file.children().find(|c| c.name() == n).map(|c| c.text().trim().to_string()).unwrap_or_default();
    FileDesc {
        name: sanitise(&text("name")),
        size: text("size").parse().unwrap_or(0),
        media_type: text("media-type"),
        hash: file.children().find(|c| c.is("hash", HASHES)).and_then(|h| Some((h.attr("algo")?.to_string(), h.text().trim().to_string()))),
    }
}

/// A name a file can be kept under: no path in it, nothing hidden, not empty.
pub fn sanitise(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim().trim_start_matches('.');
    let clean: String = base.chars().filter(|c| !c.is_control()).take(120).collect();
    if clean.is_empty() {
        "file".to_string()
    } else {
        clean
    }
}

fn file_element(f: &FileDesc) -> Element {
    let mut file = Element::builder("file", FT).build();
    file.append_child(Element::builder("name", FT).append(f.name.clone()).build());
    file.append_child(Element::builder("size", FT).append(f.size.to_string()).build());
    if !f.media_type.is_empty() {
        file.append_child(Element::builder("media-type", FT).append(f.media_type.clone()).build());
    }
    if let Some((algo, value)) = &f.hash {
        file.append_child(Element::builder("hash", HASHES).at("algo", algo.as_str()).append(value.clone()).build());
    }
    file
}

/// Whether a hash the file came with matches what arrived. A hash of an algorithm not known here is not a reason to refuse.
pub fn hash_matches(hash: &Option<(String, String)>, data: &[u8]) -> bool {
    let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(d);
    match hash {
        Some((algo, value)) => match algo.as_str() {
            "sha-256" => b64(&sha2::Sha256::digest(data)) == *value,
            "sha-512" => b64(&sha2::Sha512::digest(data)) == *value,
            "sha-1" => b64(&<sha1_010::Sha1 as Digest>::digest(data)) == *value,
            _ => true,
        },
        None => true,
    }
}

fn reply(id: &str, to: Jid, error: Option<DefinedCondition>) -> Stanza {
    Stanza::Iq(match error {
        None => Iq::Result { from: None, to: Some(to), id: id.to_string(), payload: None },
        Some(c) => Iq::Error { from: None, to: Some(to), id: id.to_string(), error: tokio_xmpp::parsers::stanza_error::StanzaError::new(tokio_xmpp::parsers::stanza_error::ErrorType::Cancel, c, "en", ""), payload: None },
    })
}

fn jingle(action: &str, sid: &str) -> tokio_xmpp::minidom::ElementBuilder {
    Element::builder("jingle", JINGLE).at("action", action).at("sid", sid)
}

/// Whether a `<jingle/>` is about a file (an offer of one, or a transfer under way).
pub fn is_ours(handle: &Handle, j: &Element) -> bool {
    let sid = j.attr("sid").unwrap_or("");
    handle.session().transfers.contains_key(sid) || j.children().any(|c| c.name() == "content" && c.children().any(|d| d.is("description", FT)))
}

/// The offers waiting for an answer, for the Invitations tile.
pub fn offers_json(handle: &Handle) -> Vec<serde_json::Value> {
    handle.session().transfers.iter().filter(|(_, t)| !t.outgoing && !t.accepted).map(|(sid, t)| json!({ "sid": sid, "from": t.peer.as_ref().map(|p| p.to_bare().to_string()).unwrap_or_default(), "name": t.file.name, "size": t.file.size, "mediaType": t.file.media_type })).collect()
}

pub fn announce(state: &AppState, handle: &Handle) {
    state.events.emit("xmppFileOffers", json!({ "accountId": handle.account_id, "offers": offers_json(handle) }));
}

fn terminate(sid: &str, reason: &str) -> Element {
    jingle("session-terminate", sid).append(Element::builder("reason", JINGLE).append(xml::empty(reason, JINGLE)).build()).build()
}

/// A Jingle request about a file.
pub async fn jingle_iq(state: &AppState, handle: &Handle, from: Jid, id: &str, j: &Element) {
    let action = j.attr("action").unwrap_or("");
    let sid = j.attr("sid").unwrap_or("").to_string();
    let peer = from.to_bare();
    if action == "session-initiate" {
        let known = handle.session().roster.get(&peer).is_some_and(|c| c.subscription != "none") || peer == handle.own();
        if !known {
            tracing::debug!("xmpp[{}]: a file from {from}, who is not a contact: refused", handle.account_id);
            return handle.send(reply(id, from, Some(DefinedCondition::ServiceUnavailable))).await;
        }
        let Some(content) = j.children().find(|c| c.name() == "content") else { return handle.send(reply(id, from, Some(DefinedCondition::BadRequest))).await };
        let Some(file) = content.children().find(|d| d.is("description", FT)).and_then(|d| d.children().find(|f| f.name() == "file")) else { return handle.send(reply(id, from, Some(DefinedCondition::BadRequest))).await };
        handle.send(reply(id, from.clone(), None)).await;
        let transport = content.children().find(|t| t.is("transport", IBB_T));
        let desc = read_file(file);
        let Some(transport) = transport else {
            // Only in-band is done here: an offer over SOCKS5 alone is ended, saying why.
            let _ = handle.request(Some(from), true, terminate(&sid, "unsupported-transports")).await;
            return;
        };
        if desc.size > MOST {
            let _ = handle.request(Some(from), true, terminate(&sid, "decline")).await;
            return;
        }
        let buffer = super::roster::ensure_buffer(state, handle, &peer);
        let t = Transfer {
            peer: Some(from),
            outgoing: false,
            file: desc,
            content: content.attr("name").unwrap_or("a").to_string(),
            creator: content.attr("creator").unwrap_or("initiator").to_string(),
            ibb_sid: transport.attr("sid").unwrap_or("").to_string(),
            block: transport.attr("block-size").and_then(|b| b.parse().ok()).unwrap_or(BLOCK).clamp(16, BLOCK),
            buffer_id: buffer.id,
            ..Default::default()
        };
        tracing::info!("xmpp[{}]: {peer} offers {} ({} bytes)", handle.account_id, t.file.name, t.file.size);
        handle.session().transfers.insert(sid, t);
        announce(state, handle);
        return;
    }
    let matches = handle.session().transfers.get(&sid).is_some_and(|t| t.peer.as_ref().map(|p| p.to_bare()) == Some(peer.clone()));
    if !matches {
        return handle.send(reply(id, from, Some(DefinedCondition::ItemNotFound))).await;
    }
    handle.send(reply(id, from.clone(), None)).await;
    match action {
        "session-accept" => {
            let mut s = handle.session();
            if let Some(t) = s.transfers.get_mut(&sid).filter(|t| t.outgoing) {
                if let Some(b) = j.children().find(|c| c.name() == "content").and_then(|c| c.children().find(|t| t.is("transport", IBB_T))).and_then(|t| t.attr("block-size")).and_then(|b| b.parse::<u16>().ok()) {
                    t.block = t.block.min(b.max(16));
                }
                t.peer = Some(from);
                if let Some(a) = t.answer.take() {
                    let _ = a.send(true);
                }
            }
        }
        "session-terminate" => {
            let gone = handle.session().transfers.remove(&sid);
            if let Some(mut t) = gone {
                if let Some(a) = t.answer.take() {
                    let _ = a.send(false);
                }
                if !t.outgoing && t.accepted && (t.data.len() as u64) < t.file.size {
                    card(state, handle, &t.buffer_id, &format!("{} stopped sending {}", peer, t.file.name));
                }
                if !t.outgoing && !t.accepted {
                    announce(state, handle);
                }
            }
        }
        _ => {}
    }
}

fn card(state: &AppState, handle: &Handle, buffer_id: &str, words: &str) {
    if let Some(b) = state.runtime.get_buffer(buffer_id) {
        state.runtime.record_message_at(state, &handle.account_id, &b.name, "dm", "*", words, false, "system", None, None, false, None, Vec::new(), Vec::new(), None, None, None, None);
    }
}

/// Accepts or declines an offer.
pub async fn answer(state: &AppState, handle: &Handle, sid: &str, accept: bool) -> Result<(), String> {
    let (to, content, creator, file, ibb_sid, block) = {
        let mut s = handle.session();
        let t = s.transfers.get_mut(sid).filter(|t| !t.outgoing && !t.accepted).ok_or("no such offer")?;
        if accept {
            t.accepted = true;
        }
        (t.peer.clone().ok_or("no such offer")?, t.content.clone(), t.creator.clone(), t.file.clone(), t.ibb_sid.clone(), t.block)
    };
    announce(state, handle);
    if !accept {
        handle.session().transfers.remove(sid);
        return handle.request(Some(to), true, terminate(sid, "decline")).await.map(|_| ()).map_err(|e| e.words());
    }
    let accept = jingle("session-accept", sid)
        .at("responder", handle.bound.to_string())
        .append(
            Element::builder("content", JINGLE)
                .at("creator", creator.as_str())
                .at("name", content.as_str())
                .append(Element::builder("description", FT).append(file_element(&file)).build())
                .append(Element::builder("transport", IBB_T).at("sid", ibb_sid.as_str()).at("block-size", block.to_string()).build())
                .build(),
        )
        .build();
    handle.request(Some(to), true, accept).await.map(|_| ()).map_err(|e| e.words())
}

/// An In-Band Bytestreams request (open, a block, close) for a transfer this side accepted.
pub async fn ibb_iq(state: &AppState, handle: &Handle, from: Jid, id: &str, payload: &Element) {
    let ibb_sid = payload.attr("sid").unwrap_or("");
    let found = handle.session().transfers.iter().find(|(_, t)| !t.outgoing && t.accepted && t.ibb_sid == ibb_sid && t.peer.as_ref() == Some(&from)).map(|(s, _)| s.clone());
    let Some(sid) = found else { return handle.send(reply(id, from, Some(DefinedCondition::ItemNotFound))).await };
    match payload.name() {
        "open" => handle.send(reply(id, from, None)).await,
        "data" => {
            let seq: u16 = payload.attr("seq").and_then(|s| s.parse().ok()).unwrap_or(0);
            let bytes = base64::engine::general_purpose::STANDARD.decode(payload.text().trim()).unwrap_or_default();
            let ok = {
                let mut s = handle.session();
                match s.transfers.get_mut(&sid) {
                    Some(t) => {
                        let expected = t.seq.map(|p| p.wrapping_add(1)).unwrap_or(0);
                        let fits = (t.data.len() + bytes.len()) as u64 <= t.file.size.max(1) && (t.data.len() + bytes.len()) as u64 <= MOST;
                        if seq == expected && fits && !bytes.is_empty() {
                            t.seq = Some(seq);
                            t.data.extend_from_slice(&bytes);
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                }
            };
            if ok {
                handle.send(reply(id, from, None)).await;
            } else {
                // A block out of order, or more than was offered: the stream is ended (XEP-0047 says so for both).
                handle.send(reply(id, from.clone(), Some(DefinedCondition::UnexpectedRequest))).await;
                if let Some(t) = handle.session().transfers.remove(&sid) {
                    card(state, handle, &t.buffer_id, &format!("{} did not arrive whole", t.file.name));
                }
                let _ = handle.request(Some(from), true, terminate(&sid, "failed-transport")).await;
            }
        }
        "close" => {
            handle.send(reply(id, from.clone(), None)).await;
            let Some(t) = handle.session().transfers.remove(&sid) else { return };
            let (state, handle) = (state.clone(), handle.clone());
            tokio::spawn(async move { finish(&state, &handle, &sid, from, t).await });
        }
        _ => handle.send(reply(id, from, Some(DefinedCondition::BadRequest))).await,
    }
}

/// A file that has all arrived: checked, kept, shown, and the session ended.
async fn finish(state: &AppState, handle: &Handle, sid: &str, from: Jid, t: Transfer) {
    let whole = t.data.len() as u64 == t.file.size && hash_matches(&t.file.hash, &t.data);
    if !whole {
        card(state, handle, &t.buffer_id, &format!("{} did not arrive whole (or not as it was described), and was not kept", t.file.name));
        let _ = handle.request(Some(from), true, terminate(sid, "media-error")).await;
        return;
    }
    let dir = crate::media_cache::dir("xmpp-files");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}-{}", &super::transport::sha256_hex(&t.data)[..12], t.file.name));
    if let Err(e) = std::fs::write(&path, &t.data) {
        card(state, handle, &t.buffer_id, &format!("{} could not be kept: {e}", t.file.name));
        return;
    }
    let media_type = (!t.file.media_type.is_empty()).then(|| t.file.media_type.clone());
    let attachment = crate::model::Attachment {
        kind: super::media::kind_of(media_type.as_deref(), Some(&t.file.name)).to_string(),
        mimetype: media_type,
        filename: Some(t.file.name.clone()),
        size: Some(t.file.size),
        path: Some(format!("file://{}", path.display())),
        ..Default::default()
    };
    if let Some(b) = state.runtime.get_buffer(&t.buffer_id) {
        let who = handle.session().shown_name(&from.to_bare());
        state.runtime.record_message_at(state, &handle.account_id, &b.name, "dm", &who, "", false, "chat", None, Some(format!("jingle-{sid}")), false, None, Vec::new(), vec![attachment], Some(from.to_bare().to_string()), None, None, None);
    }
    tracing::info!("xmpp[{}]: received {} ({} bytes) directly", handle.account_id, t.file.name, t.file.size);
    let _ = handle.request(Some(from), true, terminate(sid, "success")).await;
}

/// The device of a person to send a file to directly: one that is online and says it takes Jingle file transfers in-band.
pub async fn device_for(handle: &Handle, peer: &BareJid) -> Option<Jid> {
    let resources: Vec<String> = handle.session().presence.get(peer).map(|m| m.keys().cloned().collect()).unwrap_or_default();
    for r in resources {
        let Ok(full) = format!("{peer}/{r}").parse::<Jid>() else { continue };
        let Ok(Some(info)) = handle.request(Some(full.clone()), false, xml::empty("query", super::ns::DISCO_INFO)).await else { continue };
        let has = |f: &str| info.children().any(|c| c.name() == "feature" && c.attr("var") == Some(f));
        if has(FT) && has(IBB_T) {
            return Some(full);
        }
    }
    None
}

/// Sends a file directly, in the background: the other person has to accept it, which can take minutes, and nothing waits on
/// that. What goes wrong is said in the conversation.
pub fn send_later(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, path: &str) {
    let (state, handle, buffer, path) = (state.clone(), handle.clone(), buffer.clone(), path.to_string());
    tokio::spawn(async move {
        if let Err(e) = send(&state, &handle, &buffer, &path).await {
            card(&state, &handle, &buffer.id, &format!("{} was not sent: {e}", sanitise(&path)));
        }
    });
}

/// Sends a file directly to a person's device. Returns when it has all gone, or says why it did not.
pub async fn send(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, path: &str) -> Result<(), String> {
    let peer: BareJid = buffer.remote_id.as_deref().and_then(|r| r.parse().ok()).ok_or("this conversation is not with a person")?;
    let to = device_for(handle, &peer).await.ok_or_else(|| format!("No device of {peer} that is online takes files sent directly"))?;
    let data = tokio::fs::read(path).await.map_err(|e| format!("{path}: {e}"))?;
    if data.len() as u64 > MOST {
        return Err(format!("Files sent directly are at most {} MB", MOST / 1024 / 1024));
    }
    let name = sanitise(path);
    let file = FileDesc {
        name: name.clone(),
        size: data.len() as u64,
        media_type: super::upload::content_type_of(&name).to_string(),
        hash: Some(("sha-256".to_string(), base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&data)))),
    };
    let (sid, ibb_sid) = (xml::new_id(), xml::new_id());
    let (tx, rx) = oneshot::channel();
    handle.session().transfers.insert(sid.clone(), Transfer { peer: Some(to.clone()), outgoing: true, file: file.clone(), content: "file".into(), creator: "initiator".into(), ibb_sid: ibb_sid.clone(), block: BLOCK, accepted: false, buffer_id: buffer.id.clone(), answer: Some(tx), ..Default::default() });
    let offer = jingle("session-initiate", &sid)
        .at("initiator", handle.bound.to_string())
        .append(
            Element::builder("content", JINGLE)
                .at("creator", "initiator")
                .at("name", "file")
                .at("senders", "initiator")
                .append(Element::builder("description", FT).append(file_element(&file)).build())
                .append(Element::builder("transport", IBB_T).at("sid", ibb_sid.as_str()).at("block-size", BLOCK.to_string()).build())
                .build(),
        )
        .build();
    let forget = |h: &Handle| {
        h.session().transfers.remove(&sid);
    };
    if let Err(e) = handle.request(Some(to.clone()), true, offer).await {
        forget(handle);
        return Err(e.words());
    }
    // They have to say yes: a person, deciding.
    let accepted = matches!(tokio::time::timeout(std::time::Duration::from_secs(300), rx).await, Ok(Ok(true)));
    if !accepted {
        forget(handle);
        return Err(format!("{peer} did not take the file"));
    }
    let block = handle.session().transfers.get(&sid).map(|t| t.block).unwrap_or(BLOCK) as usize;
    let to = handle.session().transfers.get(&sid).and_then(|t| t.peer.clone()).unwrap_or(to);
    let open = Element::builder("open", IBB).at("sid", ibb_sid.as_str()).at("block-size", block.to_string()).at("stanza", "iq").build();
    let result = async {
        handle.request(Some(to.clone()), true, open).await.map_err(|e| e.words())?;
        for (seq, chunk) in data.chunks(block).enumerate() {
            let piece = Element::builder("data", IBB).at("sid", ibb_sid.as_str()).at("seq", ((seq % 65536) as u16).to_string()).append(base64::engine::general_purpose::STANDARD.encode(chunk)).build();
            handle.request(Some(to.clone()), true, piece).await.map_err(|e| e.words())?;
        }
        handle.request(Some(to.clone()), true, Element::builder("close", IBB).at("sid", ibb_sid.as_str()).build()).await.map_err(|e| e.words())?;
        Ok::<(), String>(())
    }
    .await;
    let ended = handle.session().transfers.remove(&sid).is_none();
    if !ended {
        let _ = handle.request(Some(to.clone()), true, terminate(&sid, if result.is_ok() { "success" } else { "failed-transport" })).await;
    }
    result?;
    let attachment = crate::model::Attachment {
        kind: super::media::kind_of(Some(&file.media_type), Some(&name)).to_string(),
        mimetype: Some(file.media_type.clone()),
        filename: Some(name),
        size: Some(file.size),
        path: Some(format!("file://{path}")),
        ..Default::default()
    };
    let own = handle.own().to_string();
    state.runtime.record_message_at(state, &buffer.account_id, &buffer.name, "dm", &own, "", false, "chat", None, Some(format!("jingle-{sid}")), false, None, Vec::new(), vec![attachment], Some(own.clone()), None, None, None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_file_offer_is_read_and_its_name_cannot_go_anywhere() {
        let f = Element::from_str("<file xmlns='urn:xmpp:jingle:apps:file-transfer:5'><media-type>text/plain</media-type><name>../../.bashrc</name><size>6144</size><hash xmlns='urn:xmpp:hashes:2' algo='sha-256'>abc=</hash></file>").unwrap();
        let d = read_file(&f);
        assert_eq!((d.name.as_str(), d.size, d.media_type.as_str()), ("bashrc", 6144, "text/plain"));
        assert_eq!(d.hash, Some(("sha-256".into(), "abc=".into())));
        assert_eq!(sanitise(""), "file");
        assert_eq!(sanitise("C:\\\\Users\\\\x\\\\photo.jpg"), "photo.jpg");
    }

    #[test]
    fn what_arrived_is_checked_against_its_hash() {
        let data = b"hello";
        let good = Some(("sha-256".to_string(), base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(data))));
        assert!(hash_matches(&good, data));
        assert!(!hash_matches(&good, b"hellp"));
        assert!(hash_matches(&None, data));
        assert!(hash_matches(&Some(("blake3".into(), "x".into())), data), "an algorithm not known here is not a refusal");
    }
}
