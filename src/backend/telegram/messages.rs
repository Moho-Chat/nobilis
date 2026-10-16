//! A Telegram message as moho's: who said it, what it says, what it answers, what came with it.
//!
//! Telegram marks up a message with entities - a kind, an offset and a length, counted in UTF-16 code units - which
//! become the message's `html` (bold, italic, underline, strike, code, a quote, a link, a spoiler) beside its plain
//! `body`. Media is not drawn until Round 4; a message that carries some says what it is in words, after its caption.

use super::media;
use super::peers;
use crate::model::ReplyPreview;
use crate::state::AppState;
use grammers_client::message::Message;
use grammers_client::peer::Peer;
use grammers_client::tl;

/// What the connection knows about the account itself: its name here, which a message it sent is attributed to.
#[derive(Clone)]
pub struct Own {
    pub name: String,
    pub user_id: i64,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A span to mark: where it starts and ends in UTF-16 units, and the tags that open and close it.
struct Span {
    from: usize,
    to: usize,
    open: String,
    close: &'static str,
}

fn span_of(entity: &tl::enums::MessageEntity, text16: &[u16]) -> Option<Span> {
    use tl::enums::MessageEntity as E;
    let (offset, length, open, close): (i32, i32, String, &'static str) = match entity {
        E::Bold(e) => (e.offset, e.length, "<b>".into(), "</b>"),
        E::Italic(e) => (e.offset, e.length, "<i>".into(), "</i>"),
        E::Underline(e) => (e.offset, e.length, "<u>".into(), "</u>"),
        E::Strike(e) => (e.offset, e.length, "<s>".into(), "</s>"),
        E::Code(e) => (e.offset, e.length, "<code>".into(), "</code>"),
        E::Pre(e) => (e.offset, e.length, "<pre>".into(), "</pre>"),
        E::Blockquote(e) => (e.offset, e.length, "<blockquote>".into(), "</blockquote>"),
        E::Spoiler(e) => (e.offset, e.length, "<span class=\"spoiler\">".into(), "</span>"),
        E::TextUrl(e) => (e.offset, e.length, format!("<a href=\"{}\">", escape(&e.url)), "</a>"),
        E::Url(e) => {
            // The link is its own text.
            let (from, to) = (e.offset.max(0) as usize, ((e.offset + e.length).max(0) as usize).min(text16.len()));
            let target = String::from_utf16_lossy(text16.get(from..to)?);
            let target = if target.contains("://") { target } else { format!("https://{target}") };
            (e.offset, e.length, format!("<a href=\"{}\">", escape(&target)), "</a>")
        }
        _ => return None,
    };
    let from = offset.max(0) as usize;
    let to = ((offset + length).max(0) as usize).min(text16.len());
    (from < to).then_some(Span { from, to, open, close })
}

/// The message's text with its entities as html, or `None` when it has none worth drawing.
pub fn entities_html(text: &str, entities: &[tl::enums::MessageEntity]) -> Option<String> {
    let text16: Vec<u16> = text.encode_utf16().collect();
    let mut spans: Vec<Span> = entities.iter().filter_map(|e| span_of(e, &text16)).collect();
    if spans.is_empty() {
        return None;
    }
    // Outer spans open first: by where they start, the longer before the shorter.
    spans.sort_by(|a, b| a.from.cmp(&b.from).then(b.to.cmp(&a.to)));
    let mut out = String::new();
    let mut open: Vec<&Span> = Vec::new();
    let mut next = 0;
    let mut position = 0;
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if !run.is_empty() {
            out.push_str(&escape(run).replace('\n', "<br>"));
            run.clear();
        }
    };
    let mut units = text16.iter().copied().peekable();
    while position <= text16.len() {
        // What ends here closes first, innermost first.
        while let Some(last) = open.last() {
            if last.to <= position {
                flush(&mut run, &mut out);
                out.push_str(last.close);
                open.pop();
            } else {
                break;
            }
        }
        while next < spans.len() && spans[next].from <= position {
            flush(&mut run, &mut out);
            out.push_str(&spans[next].open);
            open.push(&spans[next]);
            next += 1;
        }
        let Some(unit) = units.next() else { break };
        // A character outside the basic plane is two units: take both together.
        let mut pair = vec![unit];
        if (0xD800..0xDC00).contains(&unit) {
            if let Some(low) = units.next() {
                pair.push(low);
                position += 1;
            }
        }
        run.push_str(&String::from_utf16_lossy(&pair));
        position += 1;
    }
    flush(&mut run, &mut out);
    while let Some(last) = open.pop() {
        out.push_str(last.close);
    }
    Some(out)
}

/// What a message with media says it carries, in words.
pub fn media_label(media: &tl::enums::MessageMedia) -> Option<String> {
    use tl::enums::MessageMedia as M;
    Some(match media {
        M::Empty | M::Unsupported => return None,
        // A link's card is the link already in the text.
        M::WebPage(_) => return None,
        M::Photo(_) => "[Photo]".to_string(),
        M::Geo(_) | M::GeoLive(_) => "[Location]".to_string(),
        M::Venue(v) => format!("[Place: {}]", v.title),
        M::Contact(c) => format!("[Contact: {} {}]", c.first_name, c.last_name).trim_end().replace(" ]", "]"),
        M::Poll(p) => format!("[Poll: {}]", poll_question(&p.poll)),
        M::Dice(d) => format!("[{} {}]", d.emoticon, d.value),
        M::Document(d) => match &d.document {
            Some(tl::enums::Document::Document(doc)) => document_label(doc),
            _ => "[File]".to_string(),
        },
        _ => "[Attachment]".to_string(),
    })
}

fn poll_question(poll: &tl::enums::Poll) -> String {
    match poll {
        tl::enums::Poll::Poll(p) => match &p.question {
            tl::enums::TextWithEntities::Entities(t) => t.text.clone(),
        },
    }
}

fn document_label(doc: &tl::types::Document) -> String {
    use tl::enums::DocumentAttribute as A;
    let name = doc.attributes.iter().find_map(|a| if let A::Filename(f) = a { Some(f.file_name.clone()) } else { None });
    for attribute in &doc.attributes {
        match attribute {
            A::Sticker(s) => return format!("[Sticker {}]", s.alt).replace("[Sticker ]", "[Sticker]"),
            A::Audio(a) if a.voice => return "[Voice message]".to_string(),
            A::Audio(a) => return format!("[Audio: {}]", a.title.clone().or(name.clone()).unwrap_or_else(|| "track".to_string())),
            A::Video(v) if v.round_message => return "[Video message]".to_string(),
            A::Video(_) => return format!("[Video{}]", name.map(|n| format!(": {n}")).unwrap_or_default()),
            A::Animated => return "[GIF]".to_string(),
            _ => {}
        }
    }
    format!("[File{}]", name.map(|n| format!(": {n}")).unwrap_or_default())
}

/// What a service message says (somebody joined, the title changed), or `None` for those with nothing to say.
pub fn service_text(who: &str, action: &tl::enums::MessageAction) -> Option<String> {
    use tl::enums::MessageAction as A;
    Some(match action {
        A::ChatCreate(a) => format!("{who} created the group \"{}\"", a.title),
        A::ChatEditTitle(a) => format!("{who} changed the title to \"{}\"", a.title),
        A::ChatEditPhoto(_) => format!("{who} changed the group photo"),
        A::ChatDeletePhoto => format!("{who} removed the group photo"),
        A::ChatAddUser(a) => format!("{who} added {} {}", a.users.len(), if a.users.len() == 1 { "person" } else { "people" }),
        A::ChatDeleteUser(_) => format!("{who} removed somebody from the group"),
        A::ChatJoinedByLink(_) => format!("{who} joined by invite link"),
        A::ChatJoinedByRequest => format!("{who} joined after being approved"),
        A::ChannelCreate(a) => format!("The channel \"{}\" was created", a.title),
        A::PinMessage => format!("{who} pinned a message"),
        A::HistoryClear => "The history was cleared".to_string(),
        A::ScreenshotTaken => format!("{who} took a screenshot"),
        A::ContactSignUp => format!("{who} joined Telegram"),
        A::SetMessagesTtl(a) => if a.period == 0 { "Messages no longer disappear".to_string() } else { format!("Messages now disappear after {} seconds", a.period) },
        A::PhoneCall(a) => format!("{who} {} call", if a.video { "had a video" } else { "had a voice" }),
        A::TopicCreate(a) => format!("{who} created the topic \"{}\"", a.title),
        A::Empty => return None,
        _ => format!("{who} did something Telegram describes in a way moho does not show yet"),
    })
}

/// A person's name, for a line that says who did something.
pub fn name_of(peer: &Peer) -> String {
    peers::title_of(peer)
}

/// Everything about a message that is words: the caption or text, what came with it, and where it was forwarded from.
pub fn body_of(message: &Message) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(header) = message.forward_header() {
        let tl::enums::MessageFwdHeader::Header(h) = header;
        let from = h.from_name.clone().or_else(|| h.post_author.clone()).unwrap_or_else(|| "somebody".to_string());
        parts.push(format!("↪ Forwarded from {from}"));
    }
    let text = message.text();
    if !text.is_empty() {
        parts.push(text.to_string());
    }
    if let tl::enums::Message::Message(m) = &message.raw {
        if let Some(label) = m.media.as_ref().and_then(media_label) {
            parts.push(label);
        }
    }
    parts.join("\n")
}

/// Records a message in its conversation, as it arrived live or as history. `false` when it was already there.
pub fn record(state: &AppState, account_id: &str, own: &Own, message: &Message) -> bool {
    record_named(state, account_id, own, message, None)
}

/// The same, remembering who the sender is for `names` when it is given.
pub fn record_named(state: &AppState, account_id: &str, own: &Own, message: &Message, names: Option<&std::sync::Mutex<std::collections::HashMap<i64, String>>>) -> bool {
    let Some(peer) = message.peer() else { return false };
    record_in(state, account_id, own, message, peer, message.sender(), names)
}

/// The same for a message whose conversation the caller found. A message a person sent in a private chat arrives as
/// a short update that names no one, so the library cannot say who it is from or where it is: the caller asks
/// Telegram, and the other person in a private chat is the sender of everything not sent by the account.
pub fn record_in(state: &AppState, account_id: &str, own: &Own, message: &Message, peer: &Peer, sender: Option<&Peer>, names: Option<&std::sync::Mutex<std::collections::HashMap<i64, String>>>) -> bool {
    let buffer = peers::buffer_for(state, account_id, peer);
    let outgoing = message.outgoing();
    let sender = sender.or_else(|| (!outgoing && matches!(peer, Peer::User(_))).then_some(peer));
    if let (Some(names), Some(Peer::User(user))) = (names, sender) {
        names.lock().unwrap_or_else(|e| e.into_inner()).insert(user.id().bare_id_unchecked(), name_of(&Peer::User(user.clone())));
    }
    let from = if outgoing {
        own.name.clone()
    } else {
        sender.map(name_of).unwrap_or_else(|| peers::title_of(peer))
    };
    let sender_id = if outgoing { Some(own.user_id.to_string()) } else { message.sender_id().map(peers::remote_id) };
    let ts = message.date().as_second();
    let id = message.id().to_string();

    if let Some(action) = message.action() {
        let Some(text) = service_text(&from, action) else { return false };
        return state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, "Telegram", &text, false, "system", None, Some(id), false, None, Vec::new(), Vec::new(), None, Some(ts), None, None);
    }

    let topic_only = matches!(&message.raw, tl::enums::Message::Message(m) if m.reply_to.as_ref().is_some_and(is_topic_root));
    let reply = message.reply_to_message_id().filter(|_| !topic_only).map(|reply_id| {
        let stored = state.store.get_message(&buffer.id, &reply_id.to_string()).ok().flatten();
        ReplyPreview {
            id: reply_id.to_string(),
            from: stored.as_ref().map(|m| m.from.clone()).unwrap_or_default(),
            body: stored.map(|m| m.body).unwrap_or_default(),
            thread: false,
            forwarded: false,
            thread_root: None,
        }
    });
    let body = body_of(message);
    let html = message.fmt_entities().and_then(|e| entities_html(message.text(), e)).filter(|h| !h.is_empty());
    // The first line of a forward is words of our own, so the html (made from the text alone) would lose it: a
    // forwarded message is shown as plain text.
    let html = if message.forward_header().is_some() { None } else { html };
    let attachments = buffer.remote_id.as_deref().map(|remote| media::attachments_of(own.user_id, remote, message)).unwrap_or_default();
    let avatar = if outgoing { None } else { sender.and_then(|p| media::avatar_url(own.user_id, p)) };
    if let Some(remote) = buffer.remote_id.as_deref() {
        media::prefetch_preview(state, account_id, own.user_id, remote, message);
    }
    let again = attachments.clone();
    let cards = buffer.remote_id.as_deref().map(|remote| media::embeds_of(own.user_id, remote, message)).unwrap_or_default();
    let cards_again = cards.clone();
    let recorded = state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, &from, &body, false, "chat", reply, Some(id.clone()), message.mentioned(), avatar, cards, attachments, sender_id, Some(ts), html, None);
    if !recorded && !cards_again.is_empty() {
        if let Ok(Some(stored)) = state.store.get_message(&buffer.id, &id) {
            if stored.embeds != cards_again && state.store.set_message_embeds(&buffer.id, &id, &cards_again).unwrap_or(false) {
                state.events.emit("messageUpdated", serde_json::json!({ "bufferId": buffer.id, "id": id, "body": stored.body, "edited": false, "embeds": cards_again, "attachments": stored.attachments }));
            }
        }
    }
    // Read again, a message already kept without its picture or file gets it: earlier builds kept the text alone.
    if !recorded && !again.is_empty() {
        if let Ok(Some(stored)) = state.store.get_message(&buffer.id, &id) {
            if stored.attachments.is_empty() && state.store.update_message_attachments(&buffer.id, &id, &again).unwrap_or(false) {
                state.events.emit("messageUpdated", serde_json::json!({ "bufferId": buffer.id, "id": id, "body": stored.body, "edited": false, "embeds": stored.embeds, "attachments": again }));
            }
        }
    }
    // A poll that is still open is the conversation's card.
    if let tl::enums::Message::Message(raw) = &message.raw {
        if let Some(tl::enums::MessageMedia::Poll(p)) = &raw.media {
            let (tl::enums::Poll::Poll(poll), tl::enums::PollResults::Results(results)) = (&p.poll, &p.results);
            super::polls::announce(state, &buffer.id, message.id(), poll, results);
        }
    }
    // What people have reacted with, which a message arrives carrying.
    if let tl::enums::Message::Message(m) = &message.raw {
        // How many have answered it, or commented under it if it is a channel's post.
        if let Some(tl::enums::MessageReplies::Replies(r)) = &m.replies {
            if r.replies > 0 {
                state.events.emit("threadCount", serde_json::json!({ "bufferId": buffer.id, "id": id, "count": r.replies }));
            }
        }
        if let Some(reactions) = &m.reactions {
            let list = super::reactions::from_tl(reactions);
            if !list.is_empty() {
                super::reactions::apply(state, &buffer.id, &id, &list);
            }
        }
    }
    recorded
}

/// In a group with topics every message "replies" to its topic's first message; that is where it is, not who it
/// answers. A real reply names a message and the topic it is in as well.
fn is_topic_root(header: &tl::enums::MessageReplyHeader) -> bool {
    matches!(header, tl::enums::MessageReplyHeader::Header(h) if h.forum_topic && h.reply_to_top_id.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::types as t;

    fn bold(offset: i32, length: i32) -> tl::enums::MessageEntity {
        tl::enums::MessageEntity::Bold(t::MessageEntityBold { offset, length })
    }
    fn italic(offset: i32, length: i32) -> tl::enums::MessageEntity {
        tl::enums::MessageEntity::Italic(t::MessageEntityItalic { offset, length })
    }

    fn header(forum_topic: bool, msg: i32, top: Option<i32>) -> tl::enums::MessageReplyHeader {
        tl::enums::MessageReplyHeader::Header(t::MessageReplyHeader {
            reply_to_scheduled: false,
            forum_topic,
            quote: false,
            reply_to_ephemeral: false,
            reply_to_msg_id: Some(msg),
            reply_to_peer_id: None,
            reply_from: None,
            reply_media: None,
            reply_to_top_id: top,
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            todo_item_id: None,
            poll_option: None,
        })
    }

    #[test]
    fn a_message_in_a_topic_is_not_a_reply_to_the_topic() {
        assert!(is_topic_root(&header(true, 10, None)));
        assert!(!is_topic_root(&header(true, 55, Some(10))), "answering a message inside the topic");
        assert!(!is_topic_root(&header(false, 10, None)), "an ordinary reply");
    }

    #[test]
    fn entities_become_html_and_nest() {
        assert_eq!(entities_html("hello world", &[bold(0, 5)]).unwrap(), "<b>hello</b> world");
        assert_eq!(entities_html("a bc d", &[bold(2, 2), italic(2, 1)]).unwrap(), "a <b><i>b</i>c</b> d");
        assert_eq!(entities_html("plain", &[]), None);
        // The text is escaped, and a line break is one.
        assert_eq!(entities_html("a < b\nc", &[bold(0, 1)]).unwrap(), "<b>a</b> &lt; b<br>c");
    }

    #[test]
    fn offsets_are_in_utf16_units_so_an_emoji_before_the_text_moves_it_by_two() {
        // "😀 hi": the emoji is two units, the space one, so "hi" starts at 3.
        assert_eq!(entities_html("😀 hi", &[bold(3, 2)]).unwrap(), "😀 <b>hi</b>");
        // A span that runs past the end is cut at it rather than panicking.
        assert_eq!(entities_html("ab", &[bold(1, 50)]).unwrap(), "a<b>b</b>");
    }

    #[test]
    fn a_link_is_its_own_text_or_its_url_and_a_bare_domain_gets_a_scheme() {
        let url = tl::enums::MessageEntity::Url(t::MessageEntityUrl { offset: 4, length: 11 });
        assert_eq!(entities_html("see example.com now", &[url]).unwrap(), "see <a href=\"https://example.com\">example.com</a> now");
        let text_url = tl::enums::MessageEntity::TextUrl(t::MessageEntityTextUrl { offset: 0, length: 4, url: "https://a.example/?q=1&r=2".into() });
        assert_eq!(entities_html("here", &[text_url]).unwrap(), "<a href=\"https://a.example/?q=1&amp;r=2\">here</a>");
    }

    #[test]
    fn service_messages_say_who_did_what() {
        let title = tl::enums::MessageAction::ChatEditTitle(t::MessageActionChatEditTitle { title: "Lunch".into() });
        assert_eq!(service_text("Ann", &title).as_deref(), Some("Ann changed the title to \"Lunch\""));
        assert_eq!(service_text("Ann", &tl::enums::MessageAction::PinMessage).as_deref(), Some("Ann pinned a message"));
        assert_eq!(service_text("Ann", &tl::enums::MessageAction::Empty), None);
    }
}
