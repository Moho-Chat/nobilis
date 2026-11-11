//! What a `<message>` says, read out of its XML: the text, who it is from, and everything riding on it - receipts,
//! markers, typing, corrections, retractions, reactions, replies, the ids the sender and the archive gave it.
//!
//! Pure: no state, no network, so each rule is tested on the XML a client actually sends. What to *do* with a message
//! - which conversation it belongs to, whether it is a duplicate, what it changes - is `messages.rs`.

use super::ns;
use super::xml::{child, parse_stamp};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::{Message, MessageType};

/// What a chat marker (XEP-0333) says about a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Marker {
    Markable,
    Received(String),
    Displayed(String),
    Acknowledged(String),
}

/// A typing state (XEP-0085).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatState {
    Active,
    Composing,
    Paused,
    Inactive,
    Gone,
}

/// One id the archive (or a room) gave the message: `by` is whose stanza-id it is.
#[derive(Debug, Clone, PartialEq)]
pub struct StanzaId {
    pub by: String,
    pub id: String,
}

/// A reply (XEP-0461): which message, and who wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub id: String,
    pub to: Option<String>,
}

/// A span of the body that is not the message but a fallback for clients that do not understand something
/// (XEP-0428), in characters.
#[derive(Debug, Clone, PartialEq)]
pub struct Fallback {
    pub for_ns: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub body: Option<String>,
    pub subject: Option<String>,
    pub id: Option<String>,
    pub origin_id: Option<String>,
    pub stanza_ids: Vec<StanzaId>,
    /// When the message was sent, if it says (a delayed message, or one from an archive), unix seconds.
    pub sent_at: Option<i64>,
    pub thread: Option<String>,
    /// The receipt this message asks for (XEP-0184), by its id.
    pub wants_receipt: bool,
    pub receipt_for: Option<String>,
    pub marker: Option<Marker>,
    pub chat_state: Option<ChatState>,
    /// The message this one replaces (XEP-0308).
    pub replaces: Option<String>,
    /// The message this one takes back (XEP-0424).
    pub retracts: Option<String>,
    /// Set when a moderator, not the sender, retracted it (XEP-0425), with the reason.
    pub moderated: Option<Option<String>>,
    /// A reaction set (XEP-0444): the message, and the emoji the sender now has on it.
    pub reactions: Option<(String, Vec<String>)>,
    pub reply: Option<Reply>,
    pub fallbacks: Vec<Fallback>,
    /// Links the sender attached as files (XEP-0066), with their descriptions.
    pub oob: Vec<(String, Option<String>)>,
    /// Files shared with their description (XEP-0447 with XEP-0446): what they are, how big, where from.
    pub files: Vec<FileRef>,
    /// The sender says this is not to be kept (XEP-0334 `no-store` / `no-permanent-store`).
    pub no_store: bool,
    /// Something is encrypted that this client cannot read (XEP-0380 names what).
    pub encrypted: Option<String>,
    /// The message carries a body that is only there for clients that cannot show what it points at.
    pub styling_disabled: bool,
}

/// A file somebody shared: where it is and what it says it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileRef {
    pub url: String,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub size: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub description: Option<String>,
    /// The hash the sender gave: algorithm and base64 digest.
    pub hash: Option<(String, String)>,
    /// How long, in seconds, for audio and video.
    pub duration: Option<f64>,
}

/// The file described by a `<file-sharing/>` (XEP-0447), if it names a source we can fetch.
fn read_sharing(e: &Element) -> Option<FileRef> {
    let file = xml_child(e, "file", ns::FILE_METADATA)?;
    let text = |name: &str| file.children().find(|c| c.name() == name).map(|c| c.text()).filter(|t| !t.is_empty());
    let dims = file.children().find(|c| c.name() == "dimensions");
    let url = xml_child(e, "sources", ns::SFS)
        .and_then(|s| s.children().find(|c| c.is("url-data", ns::URL_DATA)))
        .and_then(|u| u.attr("target"))?
        .to_string();
    let hash = file.children().find(|c| c.is("hash", ns::HASHES)).and_then(|h| Some((h.attr("algo")?.to_string(), h.text())));
    Some(FileRef {
        url,
        name: text("name"),
        media_type: text("media-type"),
        size: text("size").and_then(|s| s.parse().ok()),
        width: dims.and_then(|d| d.attr("width")).and_then(|v| v.parse().ok()),
        height: dims.and_then(|d| d.attr("height")).and_then(|v| v.parse().ok()),
        description: text("desc"),
        hash,
        duration: text("length").and_then(|s| s.parse::<f64>().ok()).map(|ms| ms / 1000.0),
    })
}

fn xml_child<'a>(e: &'a Element, name: &str, ns: &str) -> Option<&'a Element> {
    e.children().find(|c| c.is(name, ns))
}

/// Reads one message.
pub fn parse(message: &Message) -> Parsed {
    let mut p = Parsed { id: message.id.as_ref().map(|i| i.0.clone()), ..Default::default() };
    p.body = message.get_best_body_cloned(vec!["en"]).map(|(_, b)| b);
    p.subject = message.get_best_subject_cloned(vec!["en"]).map(|(_, s)| s);
    p.thread = message.thread.as_ref().map(|t| t.id.clone());
    for e in &message.payloads {
        read_payload(&mut p, e);
    }
    p
}

fn read_payload(p: &mut Parsed, e: &Element) {
    match (e.name(), e.ns().as_str()) {
        ("origin-id", ns::SID) => p.origin_id = e.attr("id").map(str::to_string),
        ("stanza-id", ns::SID) => {
            if let (Some(by), Some(id)) = (e.attr("by"), e.attr("id")) {
                p.stanza_ids.push(StanzaId { by: by.to_string(), id: id.to_string() });
            }
        }
        ("delay", ns::DELAY) => {
            if p.sent_at.is_none() {
                p.sent_at = e.attr("stamp").and_then(parse_stamp);
            }
        }
        ("x", ns::DELAY_LEGACY) => {
            // The old form, `20261109T12:00:00`, has no zone and means UTC.
            if p.sent_at.is_none() {
                p.sent_at = e.attr("stamp").and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H:%M:%S").ok()).map(|d| d.and_utc().timestamp());
            }
        }
        ("request", ns::RECEIPTS) => p.wants_receipt = true,
        ("received", ns::RECEIPTS) => p.receipt_for = e.attr("id").map(str::to_string),
        ("markable", ns::MARKERS) => p.marker = Some(Marker::Markable),
        ("received", ns::MARKERS) => p.marker = e.attr("id").map(|i| Marker::Received(i.to_string())),
        ("displayed", ns::MARKERS) => p.marker = e.attr("id").map(|i| Marker::Displayed(i.to_string())),
        ("acknowledged", ns::MARKERS) => p.marker = e.attr("id").map(|i| Marker::Acknowledged(i.to_string())),
        ("active", ns::CHATSTATES) => p.chat_state = Some(ChatState::Active),
        ("composing", ns::CHATSTATES) => p.chat_state = Some(ChatState::Composing),
        ("paused", ns::CHATSTATES) => p.chat_state = Some(ChatState::Paused),
        ("inactive", ns::CHATSTATES) => p.chat_state = Some(ChatState::Inactive),
        ("gone", ns::CHATSTATES) => p.chat_state = Some(ChatState::Gone),
        ("replace", ns::CORRECT) => p.replaces = e.attr("id").map(str::to_string),
        // XEP-0424 version 0.4: the retraction names the message itself.
        ("retract", ns::RETRACT) => p.retracts = e.attr("id").map(str::to_string),
        // Version 0.3 wrapped it in an application of the message it retracts (XEP-0422).
        ("apply-to", ns::FASTEN) => {
            if let Some(target) = e.attr("id") {
                if child(e, "retract", ns::RETRACT_OLD).is_some() {
                    p.retracts = Some(target.to_string());
                }
            }
        }
        ("moderated", ns::MODERATED) => {
            p.moderated = Some(e.children().find(|c| c.name() == "reason").map(|r| r.text()));
        }
        ("reactions", ns::REACTIONS) => {
            if let Some(id) = e.attr("id") {
                let mut seen = Vec::new();
                for r in e.children().filter(|c| c.is("reaction", ns::REACTIONS)) {
                    let text = r.text();
                    let text = text.trim();
                    if !text.is_empty() && !seen.iter().any(|s: &String| s == text) {
                        seen.push(text.to_string());
                    }
                }
                p.reactions = Some((id.to_string(), seen));
            }
        }
        ("reply", ns::REPLY) => {
            if let Some(id) = e.attr("id") {
                p.reply = Some(Reply { id: id.to_string(), to: e.attr("to").map(str::to_string) });
            }
        }
        ("fallback", ns::FALLBACK) => {
            let for_ns = e.attr("for").unwrap_or_default().to_string();
            for b in e.children().filter(|c| c.name() == "body") {
                let start = b.attr("start").and_then(|s| s.parse().ok());
                let end = b.attr("end").and_then(|s| s.parse().ok());
                if let (Some(start), Some(end)) = (start, end) {
                    p.fallbacks.push(Fallback { for_ns: for_ns.clone(), start, end });
                }
            }
        }
        ("file-sharing", ns::SFS) => {
            if let Some(f) = read_sharing(e) {
                p.files.push(f);
            }
        }
        ("x", ns::OOB) => {
            if let Some(url) = e.children().find(|c| c.name() == "url").map(|u| u.text()) {
                let desc = e.children().find(|c| c.name() == "desc").map(|d| d.text()).filter(|d| !d.is_empty());
                p.oob.push((url.trim().to_string(), desc));
            }
        }
        ("no-store" | "no-permanent-store", ns::HINTS) => p.no_store = true,
        ("encryption", ns::EME) => p.encrypted = Some(e.attr("name").map(str::to_string).unwrap_or_else(|| e.attr("namespace").unwrap_or("an encryption").to_string())),
        // OMEMO's own element, when nothing named it.
        ("encrypted", "eu.siacs.conversations.axolotl" | "urn:xmpp:omemo:2") if p.encrypted.is_none() => p.encrypted = Some("OMEMO".to_string()),
        _ => {}
    }
}

/// The body with the parts that are only fallback for a feature we understand taken out. `understood` are the
/// namespaces of those features (`urn:xmpp:reply:0` when we draw replies ourselves).
pub fn strip_fallbacks(body: &str, fallbacks: &[Fallback], understood: &[&str]) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut cut = vec![false; chars.len()];
    for f in fallbacks.iter().filter(|f| understood.contains(&f.for_ns.as_str())) {
        for flag in cut.iter_mut().take(f.end.min(chars.len())).skip(f.start) {
            *flag = true;
        }
    }
    chars.iter().zip(&cut).filter(|(_, c)| !**c).map(|(ch, _)| *ch).collect::<String>().trim_start_matches('\n').to_string()
}

/// Whether this message type is one that belongs in a conversation at all.
pub fn is_conversational(t: &MessageType) -> bool {
    matches!(t, MessageType::Chat | MessageType::Normal | MessageType::Groupchat)
}

/// The id a message is known by here: the one its sender chose for it (origin-id), else the one on the stanza.
pub fn local_id(p: &Parsed) -> Option<String> {
    p.origin_id.clone().or_else(|| p.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn msg(xml: &str) -> Message {
        Message::try_from(Element::from_str(xml).unwrap()).unwrap()
    }

    const OPEN: &str = "<message xmlns='jabber:client' from='bob@example.org/phone' to='alice@example.org' type='chat' id='m1'>";

    #[test]
    fn a_plain_message_has_a_body_and_the_ids_the_sender_and_the_archive_gave() {
        let p = parse(&msg(&format!("{OPEN}<body>hello</body><origin-id xmlns='urn:xmpp:sid:0' id='o-1'/><stanza-id xmlns='urn:xmpp:sid:0' by='alice@example.org' id='A1'/><stanza-id xmlns='urn:xmpp:sid:0' by='room@muc.example.org' id='R9'/></message>")));
        assert_eq!((p.body.as_deref(), p.id.as_deref(), p.origin_id.as_deref()), (Some("hello"), Some("m1"), Some("o-1")));
        assert_eq!(p.stanza_ids, vec![StanzaId { by: "alice@example.org".into(), id: "A1".into() }, StanzaId { by: "room@muc.example.org".into(), id: "R9".into() }]);
        assert_eq!(local_id(&p).as_deref(), Some("o-1"));
    }

    #[test]
    fn a_message_with_no_origin_id_is_known_by_its_stanza_id() {
        let p = parse(&msg(&format!("{OPEN}<body>x</body></message>")));
        assert_eq!(local_id(&p).as_deref(), Some("m1"));
    }

    #[test]
    fn a_delayed_message_says_when_it_was_sent_in_either_spelling() {
        let new = parse(&msg(&format!("{OPEN}<body>x</body><delay xmlns='urn:xmpp:delay' stamp='2026-11-09T12:00:00Z'/></message>")));
        assert_eq!(new.sent_at, Some(1794225600));
        let old = parse(&msg(&format!("{OPEN}<body>x</body><x xmlns='jabber:x:delay' stamp='20261109T12:00:00'/></message>")));
        assert_eq!(old.sent_at, Some(1794225600));
    }

    #[test]
    fn receipts_markers_and_typing_are_read_whether_or_not_there_is_a_body() {
        let p = parse(&msg(&format!("{OPEN}<body>x</body><request xmlns='urn:xmpp:receipts'/><markable xmlns='urn:xmpp:chat-markers:0'/></message>")));
        assert!(p.wants_receipt);
        assert_eq!(p.marker, Some(Marker::Markable));
        let got = parse(&msg(&format!("{OPEN}<received xmlns='urn:xmpp:receipts' id='m0'/></message>")));
        assert_eq!((got.receipt_for.as_deref(), got.body), (Some("m0"), None));
        let shown = parse(&msg(&format!("{OPEN}<displayed xmlns='urn:xmpp:chat-markers:0' id='m0'/></message>")));
        assert_eq!(shown.marker, Some(Marker::Displayed("m0".into())));
        let typing = parse(&msg(&format!("{OPEN}<composing xmlns='http://jabber.org/protocol/chatstates'/></message>")));
        assert_eq!(typing.chat_state, Some(ChatState::Composing));
        let gone = parse(&msg(&format!("{OPEN}<gone xmlns='http://jabber.org/protocol/chatstates'/></message>")));
        assert_eq!(gone.chat_state, Some(ChatState::Gone));
    }

    #[test]
    fn a_correction_and_a_retraction_name_the_message_they_change() {
        let c = parse(&msg(&format!("{OPEN}<body>fixed</body><replace xmlns='urn:xmpp:message-correct:0' id='m0'/></message>")));
        assert_eq!(c.replaces.as_deref(), Some("m0"));
        let r = parse(&msg(&format!("{OPEN}<retract xmlns='urn:xmpp:message-retract:1' id='m0'/></message>")));
        assert_eq!(r.retracts.as_deref(), Some("m0"));
        let old = parse(&msg(&format!("{OPEN}<apply-to xmlns='urn:xmpp:fasten:0' id='m0'><retract xmlns='urn:xmpp:message-retract:0'/></apply-to></message>")));
        assert_eq!(old.retracts.as_deref(), Some("m0"));
        let mod_ = parse(&msg(&format!("{OPEN}<retract xmlns='urn:xmpp:message-retract:1' id='m0'><moderated xmlns='urn:xmpp:message-moderate:1'><reason>spam</reason></moderated></retract></message>")));
        // The moderation is inside the retract element in the current form, and beside it in older ones.
        assert_eq!(mod_.retracts.as_deref(), Some("m0"));
    }

    #[test]
    fn reactions_are_the_whole_set_the_sender_has_now_without_repeats() {
        let p = parse(&msg(&format!("{OPEN}<reactions xmlns='urn:xmpp:reactions:0' id='m0'><reaction>👍</reaction><reaction>🎉</reaction><reaction>👍</reaction></reactions></message>")));
        assert_eq!(p.reactions, Some(("m0".into(), vec!["👍".into(), "🎉".into()])));
        let none = parse(&msg(&format!("{OPEN}<reactions xmlns='urn:xmpp:reactions:0' id='m0'/></message>")));
        assert_eq!(none.reactions, Some(("m0".into(), vec![])));
    }

    #[test]
    fn a_reply_names_its_target_and_its_fallback_is_taken_out_of_the_body() {
        let body = "> bob\n> earlier\nthe answer";
        let p = parse(&msg(&format!("{OPEN}<body>{body}</body><reply xmlns='urn:xmpp:reply:0' id='m0' to='alice@example.org/x'/><fallback xmlns='urn:xmpp:fallback:0' for='urn:xmpp:reply:0'><body start='0' end='16'/></fallback></message>")));
        assert_eq!(p.reply, Some(Reply { id: "m0".into(), to: Some("alice@example.org/x".into()) }));
        assert_eq!(strip_fallbacks(p.body.as_deref().unwrap(), &p.fallbacks, &[ns::REPLY]), "the answer");
        // A fallback for something we do not draw stays.
        assert_eq!(strip_fallbacks(p.body.as_deref().unwrap(), &p.fallbacks, &[]), body);
    }

    #[test]
    fn fallback_spans_count_characters_not_bytes() {
        let f = [Fallback { for_ns: ns::REPLY.into(), start: 0, end: 3 }];
        assert_eq!(strip_fallbacks("é👍!rest", &f, &[ns::REPLY]), "rest");
    }

    #[test]
    fn links_sent_as_files_and_hints_are_read() {
        let p = parse(&msg(&format!("{OPEN}<body>https://e.org/a.png</body><x xmlns='jabber:x:oob'><url>https://e.org/a.png</url><desc>a picture</desc></x><no-store xmlns='urn:xmpp:hints'/></message>")));
        assert_eq!(p.oob, vec![("https://e.org/a.png".to_string(), Some("a picture".to_string()))]);
        assert!(p.no_store);
    }

    #[test]
    fn an_encrypted_message_we_cannot_read_says_what_it_is() {
        let p = parse(&msg(&format!("{OPEN}<body>I sent you an OMEMO encrypted message but your client doesn't seem to support that.</body><encryption xmlns='urn:xmpp:eme:0' namespace='eu.siacs.conversations.axolotl' name='OMEMO'/></message>")));
        assert_eq!(p.encrypted.as_deref(), Some("OMEMO"));
        let bare = parse(&msg(&format!("{OPEN}<encrypted xmlns='eu.siacs.conversations.axolotl'><header sid='1'/></encrypted></message>")));
        assert_eq!(bare.encrypted.as_deref(), Some("OMEMO"));
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_shared_file_is_read_with_what_the_sender_says_about_it() {
        let xml = "<message xmlns='jabber:client' from='bob@example.org/x' type='chat'><body>https://up.example.org/a/cat.png</body><file-sharing xmlns='urn:xmpp:sfs:0' disposition='inline'><file xmlns='urn:xmpp:file:metadata:0'><media-type>image/png</media-type><name>cat.png</name><size>1234</size><dimensions width='640' height='480'/><desc>a cat</desc><hash xmlns='urn:xmpp:hashes:2' algo='sha-256'>AAEC</hash></file><sources><url-data xmlns='http://jabber.org/protocol/url-data' target='https://up.example.org/a/cat.png'/></sources></file-sharing></message>";
        let p = parse(&Message::try_from(Element::from_str(xml).unwrap()).unwrap());
        assert_eq!(p.files.len(), 1);
        let f = &p.files[0];
        assert_eq!((f.url.as_str(), f.name.as_deref(), f.media_type.as_deref(), f.size, f.width, f.height), ("https://up.example.org/a/cat.png", Some("cat.png"), Some("image/png"), Some(1234), Some(640), Some(480)));
        assert_eq!((f.description.as_deref(), f.hash.clone()), (Some("a cat"), Some(("sha-256".to_string(), "AAEC".to_string()))));
    }

    #[test]
    fn a_file_with_no_source_we_can_read_is_not_a_file() {
        let xml = "<message xmlns='jabber:client' type='chat'><file-sharing xmlns='urn:xmpp:sfs:0'><file xmlns='urn:xmpp:file:metadata:0'><name>x</name></file><sources><jingle-ibb xmlns='urn:xmpp:jingle:transports:ibb:1' sid='s'/></sources></file-sharing></message>";
        assert!(parse(&Message::try_from(Element::from_str(xml).unwrap()).unwrap()).files.is_empty());
    }
}
