//! Ephyra Chat's frames - the shapes the chat's socket and API speak.
//!
//! Read from the site's own client (`js/USIPS/EphyraChat/ephyra-chat.js`)
//! and from a socket watched as a guest. Frames are JSON objects keyed by
//! `t`; everything here is read leniently, because a frame this does not
//! understand is one to skip, never one to stop the connection over - which
//! is what the site's own client does too.

use serde_json::Value;

/// The subprotocol the socket is opened with.
pub const SUBPROTOCOL: &str = "ephyra-chat.v1";

/// What the chat calls somebody: a principal, by uuid, with the name and
/// picture shown for them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Principal {
    pub uuid: String,
    pub name: String,
    pub avatar: Option<String>,
    pub badges: Vec<String>,
}

impl Principal {
    /// A principal as a `presence` or `joined` frame lists one.
    pub fn from_value(v: &Value) -> Option<Self> {
        let uuid = v["principal_uuid"].as_str()?.to_string();
        let name = v["display_name"].as_str().filter(|n| !n.is_empty())?.to_string();
        let avatar = v["avatar_url"].as_str().filter(|a| !a.is_empty()).map(str::to_string);
        let badges = v["badges"].as_array().map(|b| b.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Some(Self { uuid, name, avatar, badges })
    }
}

/// A room, as `rooms/heads` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomHead {
    pub uuid: String,
    pub title: String,
    pub slug: String,
    pub head_seq: u64,
    pub subtitle: Option<String>,
}

/// The listed rooms out of `GET /chat/v1/rooms/heads?kind=listed`.
pub fn parse_heads(body: &str) -> Vec<RoomHead> {
    let Ok(v) = serde_json::from_str::<Value>(body) else { return Vec::new() };
    v["rooms"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["state"].as_str().is_none_or(|s| s == "active"))
        .filter_map(|r| {
            let uuid = r["chat_room_uuid"].as_str()?.to_string();
            let title = r["title"].as_str().unwrap_or_default().to_string();
            let slug = r["slug"].as_str().map(str::to_string).filter(|s| !s.is_empty()).unwrap_or_else(|| super::rooms::slug(&title));
            Some(RoomHead {
                uuid,
                title,
                slug,
                head_seq: r["head_seq"].as_u64().unwrap_or(0),
                subtitle: r["chat_room_subtitle"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
            })
        })
        .collect()
}

/// One entry in a room's log - a frame `t: "entry"`, or one element of a
/// `/log` answer's `entries`, which are the same object.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub room: String,
    pub room_seq: u64,
    pub kind: String,
    pub message: Option<String>,
    pub author: Option<String>,
    pub at: Option<i64>,
    pub body: Option<String>,
    pub mentions: Vec<String>,
    pub state_seq: Option<u64>,
    pub html: Option<String>,
    pub client_nonce: Option<String>,
    pub subtitle: Option<String>,
}

impl Entry {
    pub fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            room: v["chat_room_uuid"].as_str().unwrap_or_default().to_string(),
            room_seq: v["room_seq"].as_u64()?,
            kind: v["kind"].as_str().unwrap_or_default().to_string(),
            message: v["chat_message_uuid"].as_str().map(str::to_lowercase),
            author: v["author_principal_uuid"].as_str().or_else(|| v["actor_principal_uuid"].as_str()).map(str::to_string),
            at: v["at"].as_i64(),
            body: v["body_bbcode"].as_str().map(str::to_string),
            mentions: v["mentions"]
                .as_array()
                .map(|m| m.iter().filter_map(|x| x.as_str().or_else(|| x["principal_uuid"].as_str()).map(str::to_string)).collect())
                .unwrap_or_default(),
            // The row carries it too; the frame's own field wins when both do.
            state_seq: v["message_state_seq"].as_u64().or_else(|| v["html"].as_str().and_then(|h| attr(h, "data-message-state-seq")?.parse().ok())),
            html: v["html"].as_str().map(str::to_string),
            client_nonce: v["client_nonce"].as_str().map(str::to_string),
            subtitle: v["chat_room_subtitle"].as_str().map(str::to_string),
        })
    }

    /// Who wrote it, as the rendered row says - the only place the author's
    /// name and picture are carried.
    pub fn row_author(&self) -> Option<Principal> {
        let html = self.html.as_deref()?;
        let uuid = attr(html, "data-author-uuid").or_else(|| self.author.clone())?;
        let name = between(html, "class=\"chat-author\">", "</span>").map(|n| unescape_html(n).trim().to_string()).filter(|n| !n.is_empty())?;
        let avatar = tag_with_class(html, "img", "chat-avatar").and_then(|tag| attr(tag, "src")).map(|s| unescape_html(&s));
        let mut badges = Vec::new();
        let mut rest = html;
        while let Some(at) = rest.find("data-badge=\"") {
            rest = &rest[at + "data-badge=\"".len()..];
            if let Some(end) = rest.find('"') {
                badges.push(rest[..end].to_string());
            }
        }
        Some(Principal { uuid, name, avatar, badges })
    }

    /// Whether the rendered row says this message has been taken down.
    pub fn row_tombstoned(&self) -> bool {
        self.html.as_deref().and_then(|h| attr(h, "data-tombstoned")).is_some_and(|v| v == "true")
    }
}

/// The reaction tallies a rendered row shows: `(code, count)` for each
/// `[data-reaction-code][data-count]` chip in its `[data-chat-reactions]`.
pub fn chips(html: &str) -> Vec<(String, i64)> {
    let Some(start) = html.find("data-chat-reactions") else { return Vec::new() };
    let list = &html[start..];
    let list = &list[..list.find("</ul>").unwrap_or(list.len())];
    let mut out = Vec::new();
    let mut rest = list;
    while let Some(at) = rest.find("data-reaction-code=\"") {
        let tag_start = rest[..at].rfind('<').unwrap_or(0);
        let tail = &rest[tag_start..];
        let end = tail.find('>').unwrap_or(tail.len());
        let tag = &tail[..end];
        if let (Some(code), Some(count)) = (attr(tag, "data-reaction-code"), attr(tag, "data-count").and_then(|c| c.parse::<i64>().ok())) {
            if count > 0 && !out.iter().any(|(c, _)| c == &code) {
                out.push((code, count));
            }
        }
        rest = &tail[end.max(1)..];
    }
    out
}

/// A mention as the chat writes it, `[user=<uuid>]Name[/user]`, read back as
/// the `@Name` a person types and reads.
pub fn mentions_to_names(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find("[user=") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let parsed = tail.find(']').and_then(|close| {
            let inner = &tail[close + 1..];
            let end = inner.find("[/user]")?;
            Some((inner[..end].to_string(), close + 1 + end + "[/user]".len()))
        });
        match parsed {
            Some((name, used)) => {
                out.push('@');
                out.push_str(name.trim_start_matches('@'));
                rest = &tail[used..];
            }
            None => {
                out.push_str("[user=");
                rest = &tail["[user=".len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A name as the BBCode body can carry it: no brackets, no line breaks, one
/// space between words.
fn markup_name(name: &str) -> String {
    name.replace(['[', ']'], "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `@Name` written the way the chat reads a mention, for every name it knows:
/// the longest known name wins, so "@Ryo Ma" is not taken for "@Ryo".
pub fn names_to_mentions(body: &str, people: &[(String, String)]) -> String {
    // What the site's own composer writes and the chat's parser reads: a
    // principal's uuid, and a name with no brackets (which would open tags)
    // or line breaks in it. Anybody whose uuid is not one - a placeholder for
    // an author the chat has not described - cannot be mentioned at all.
    // Matched on the name as typed, written as the markup-safe one.
    let mut known: Vec<&(String, String)> = people.iter().filter(|(uuid, name)| is_uuid(uuid) && !markup_name(name).is_empty()).collect();
    known.sort_by_key(|(_, name)| std::cmp::Reverse(name.chars().count()));
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        // Only at a word's start: an address like a@b is not a mention.
        let starts_word = out.chars().last().is_none_or(|c| c.is_whitespace() || "([{,".contains(c));
        let hit = starts_word
            .then(|| {
                known.iter().find(|(_, name)| {
                    after.len() >= name.len()
                        && after.is_char_boundary(name.len())
                        && after[..name.len()].eq_ignore_ascii_case(name)
                        && after[name.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_')
                })
            })
            .flatten();
        match hit {
            Some((uuid, name)) => {
                out.push_str(&format!("[user={}]{}[/user]", uuid.to_lowercase(), markup_name(name)));
                rest = &after[name.len()..];
            }
            None => {
                out.push('@');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A message's reply, which Ephyra writes the way XenForo does: the message
/// opens with a quote of the one it answers, attributed
/// `[quote="Name, chat_message: <uuid>"]`. Split off here so the reply can
/// be shown as a reply and the message as what was actually said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quoted {
    pub name: String,
    pub message: String,
    pub text: String,
}

pub fn split_reply(body: &str) -> (Option<Quoted>, &str) {
    let trimmed = body.trim_start();
    let Some(open_rest) = trimmed.strip_prefix("[quote=\"") else { return (None, body) };
    let Some((attribution, after_open)) = open_rest.split_once("\"]") else { return (None, body) };
    let Some((name, message)) = attribution.split_once(", chat_message: ") else { return (None, body) };
    // The quote's own end, nested quotes counted.
    let mut depth = 1usize;
    let mut i = 0usize;
    let lower = after_open.to_ascii_lowercase();
    while i < lower.len() {
        let rest = &lower[i..];
        if rest.starts_with("[quote") {
            depth += 1;
            i += 6;
        } else if rest.starts_with("[/quote]") {
            depth -= 1;
            if depth == 0 {
                let text = after_open[..i].trim().to_string();
                let after = after_open[i + "[/quote]".len()..].trim_start_matches(['\n', '\r', ' ']);
                return (Some(Quoted { name: name.trim().to_string(), message: message.trim().to_lowercase(), text }), after);
            }
            i += 8;
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    (None, body)
}

/// The quote a reply opens with, as the site's own client writes it.
pub fn reply_quote(name: &str, message: &str, quoted: &str) -> String {
    let shown: String = name.replace(['"', '[', ']', ','], "").split_whitespace().collect::<Vec<_>>().join(" ");
    let (_, own) = split_reply(quoted);
    let own = own.trim();
    if shown.is_empty() || own.is_empty() {
        return String::new();
    }
    format!("[quote=\"{shown}, chat_message: {}\"]{own}[/quote]\n", message.to_lowercase())
}

/// A post's `client_nonce`: a version-7 uuid, which is what the site's own
/// client sends - the server reads the time out of it, and keeps a nonce
/// only for a window, so the time in it has to be now.
pub fn post_nonce() -> String {
    let mut bytes: [u8; 16] = rand::random();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    for (i, byte) in bytes.iter_mut().take(6).enumerate() {
        *byte = (now >> ((5 - i) * 8)) as u8;
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

/// A command's `chat_operation_uuid`: a random (version-4) uuid.
pub fn operation_id() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

fn format_uuid(b: &[u8; 16]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

/// What the chat's error codes mean, said to a person.
pub fn describe_error(code: &str) -> String {
    match code {
        "not_permitted" => "the chat doesn't let this account do that".into(),
        "rate_limited" => "the chat says to slow down".into(),
        "muted" | "principal_muted" => "this account is muted in this room".into(),
        "banned" | "principal_banned" => "this account is banned from this room".into(),
        "room_not_found" => "the chat has no such room".into(),
        "message_not_found" => "that message is gone".into(),
        "message_state_conflict" => "that message changed in the meantime - try again".into(),
        "body_too_long" => "that's longer than the chat takes".into(),
        "body_empty" => "there's nothing to send".into(),
        "frozen" => "the chat is frozen for now".into(),
        "unavailable" | "internal_error" => "the chat is having trouble - try again".into(),
        other => format!("the chat refused it ({other})"),
    }
}

/// The value of attribute `name` in the first place it appears in `html`.
pub fn attr(html: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let mut from = 0;
    while let Some(at) = html[from..].find(&needle) {
        let start = from + at;
        // A whole attribute name, not the tail of a longer one.
        let ok = start == 0 || html.as_bytes()[start - 1].is_ascii_whitespace();
        let value_start = start + needle.len();
        if ok {
            let end = html[value_start..].find('"')?;
            return Some(html[value_start..value_start + end].to_string());
        }
        from = value_start;
    }
    None
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)?;
    Some(&s[start..start + end])
}

/// The first `<tag ...>` whose class list includes `class`.
fn tag_with_class<'a>(html: &'a str, tag: &str, class: &str) -> Option<&'a str> {
    let open = format!("<{tag} ");
    let mut rest = html;
    while let Some(at) = rest.find(&open) {
        let tail = &rest[at..];
        let end = tail.find('>')?;
        let whole = &tail[..end];
        if attr(whole, "class").is_some_and(|c| c.split_whitespace().any(|c| c == class)) {
            return Some(whole);
        }
        rest = &tail[end..];
    }
    None
}

/// Decode the HTML entities in text taken out of a rendered row.
///
/// Numeric forms matter as much as the named ones: the site emits
/// apostrophes as `&#x27;`, so handling only `&#039;` leaves visible
/// mojibake in ordinary names.
pub fn unescape_html(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];

        let named = [("&quot;", "\""), ("&apos;", "'"), ("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&nbsp;", " ")]
            .iter()
            .find_map(|(ent, rep)| tail.strip_prefix(ent).map(|r| ((*rep).to_string(), r)));

        match named.or_else(|| numeric_entity(tail)) {
            Some((rep, r)) => {
                out.push_str(&rep);
                rest = r;
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode `&#NN;` and `&#xHH;`, returning the text and the remaining input.
fn numeric_entity(tail: &str) -> Option<(String, &str)> {
    let body = tail.strip_prefix("&#")?;
    let (digits, radix) = match body.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (body, 10),
    };
    let end = digits.find(';')?;
    if end == 0 || end > 7 {
        return None;
    }
    let code = u32::from_str_radix(&digits[..end], radix).ok()?;
    let ch = char::from_u32(code)?;
    Some((ch.to_string(), &digits[end + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame as the live chat sent it to a guest on 2026-11-19.
    const POSTED: &str = r#"{"t":"entry","chat_room_uuid":"01a1f0f8-2d32-7660-a237-1f38d4009cf2","room_seq":17,"kind":"chat_message_posted","entry_uuid":"01a1f1c2-a5bf-7596-a32e-8347ad4e4227","at":1795057427,"actor_principal_uuid":"019e6198-a12a-72d2-936c-a7c55c2993fb","imported":false,"chat_message_uuid":"01a1f1c2-a5be-7158-80b9-cca987eba9a4","revision":1,"body_bbcode":":gunt:","mentions":[],"author_principal_uuid":"019e6198-a12a-72d2-936c-a7c55c2993fb","edited_at":null,"html":"<li class=\"chat-row\" data-room-seq=\"17\" data-posted-seq=\"17\" data-message-uuid=\"01a1f1c2-a5be-7158-80b9-cca987eba9a4\" data-author-uuid=\"019e6198-a12a-72d2-936c-a7c55c2993fb\" data-message-state-seq=\"17\" data-tombstoned=\"false\"><img class=\"chat-avatar\" alt=\"\" loading=\"lazy\" src=\"https://kiwifarms.net/data/avatars/m/219/219166.jpg?1766045204\"><span class=\"chat-author\">Ryo Ma</span><span class=\"chat-badges\"><span data-badge=\"hidden\"></span><span data-badge=\"primary\"></span></span><time datetime=\"2026-11-19T03:03:47.901601Z\">2026-11-19 03:03:47 UTC</time><div class=\"chat-body\">x</div><ul data-chat-reactions></ul></li>"}"#;

    #[test]
    fn reads_a_posted_message_and_its_author() {
        let v: Value = serde_json::from_str(POSTED).unwrap();
        let e = Entry::from_value(&v).unwrap();
        assert_eq!(e.kind, "chat_message_posted");
        assert_eq!(e.room_seq, 17);
        assert_eq!(e.state_seq, Some(17));
        assert_eq!(e.body.as_deref(), Some(":gunt:"));
        assert_eq!(e.message.as_deref(), Some("01a1f1c2-a5be-7158-80b9-cca987eba9a4"));
        assert!(!e.row_tombstoned());
        let who = e.row_author().unwrap();
        assert_eq!(who.name, "Ryo Ma");
        assert_eq!(who.uuid, "019e6198-a12a-72d2-936c-a7c55c2993fb");
        assert_eq!(who.avatar.as_deref(), Some("https://kiwifarms.net/data/avatars/m/219/219166.jpg?1766045204"));
        assert_eq!(who.badges, vec!["hidden", "primary"]);
    }

    #[test]
    fn reads_the_room_list() {
        let body = r#"{"rooms":[{"chat_room_uuid":"01a1f0f8-2d2c-7264-86b9-1e38d9a538a4","chat_room_kind":"listed","state":"active","head_seq":228,"floor_seq":1,"title":"Beauty Parlor","slug":"beauty-parlor","chat_room_subtitle":null},{"chat_room_uuid":"01a1f0f8-2d2f-729f-90a8-eb841d7073f1","chat_room_kind":"listed","state":"active","head_seq":16,"title":"SPORTS!!","slug":"","chat_room_subtitle":"go"}]}"#;
        let rooms = parse_heads(body);
        assert_eq!(rooms.len(), 2);
        assert_eq!(rooms[0].slug, "beauty-parlor");
        assert_eq!(rooms[0].head_seq, 228);
        // No slug from the site: made from the title the way rooms always were.
        assert_eq!(rooms[1].slug, "sports");
        assert_eq!(rooms[1].subtitle.as_deref(), Some("go"));
        assert!(parse_heads("<html>gate</html>").is_empty());
    }

    #[test]
    fn a_reply_is_the_quote_it_opens_with() {
        let body = "[quote=\"Ryo Ma, chat_message: 01A1F1C2-a5be-7158-80b9-cca987eba9a4\"]what [quote=\"x\"]inner[/quote] up[/quote]\nnot much";
        let (quoted, rest) = split_reply(body);
        let quoted = quoted.unwrap();
        assert_eq!(quoted.name, "Ryo Ma");
        assert_eq!(quoted.message, "01a1f1c2-a5be-7158-80b9-cca987eba9a4");
        assert_eq!(quoted.text, "what [quote=\"x\"]inner[/quote] up");
        assert_eq!(rest, "not much");
        // A quote that names no chat message is part of the message.
        let plain = "[quote=\"Someone\"]hi[/quote] yes";
        assert_eq!(split_reply(plain), (None, plain));
        // Unclosed: left alone.
        assert_eq!(split_reply("[quote=\"A, chat_message: b\"]open").0, None);
    }

    #[test]
    fn a_reply_quotes_only_what_the_message_said_itself() {
        let quoted = "[quote=\"B, chat_message: x\"]old[/quote]\nnew words";
        assert_eq!(
            reply_quote("Ryo, Ma \"x\"", "ABC", quoted),
            "[quote=\"Ryo Ma x, chat_message: abc\"]new words[/quote]\n"
        );
        assert_eq!(reply_quote("", "abc", "x"), "");
    }

    #[test]
    fn ids_are_the_versions_the_chat_expects() {
        let n = post_nonce();
        assert!(is_uuid(&n));
        assert_eq!(&n[14..15], "7");
        let o = operation_id();
        assert!(is_uuid(&o));
        assert_eq!(&o[14..15], "4");
        assert!(!is_uuid("20"));
    }

    #[test]
    fn attributes_are_whole_names() {
        assert_eq!(attr("<li data-room-seq=\"1\" seq=\"2\">", "seq").as_deref(), Some("2"));
        assert_eq!(attr("<li x=\"1\">", "seq"), None);
    }

    #[test]
    fn reaction_chips_are_read_off_the_row() {
        let html = r#"<li><div class="chat-body">x</div><ul data-chat-reactions><li><span data-reaction-code="reaction_2" data-count="3"><img src="a.svg" alt="Agree"></span></li><li><span data-reaction-code="reaction_1" data-count="1"></span></li></ul></li>"#;
        assert_eq!(chips(html), vec![("reaction_2".to_string(), 3), ("reaction_1".to_string(), 1)]);
        assert!(chips("<ul data-chat-reactions></ul>").is_empty());
        assert!(chips("<li>no list</li>").is_empty());
    }

    #[test]
    fn mentions_go_both_ways() {
        assert_eq!(mentions_to_names("hi [user=019e-1]Ryo Ma[/user], ok"), "hi @Ryo Ma, ok");
        assert_eq!(mentions_to_names("[user=broken"), "[user=broken");
        const U1: &str = "019e6198-a12a-72d2-936c-a7c55c2993fb";
        const U2: &str = "019e6198-a12a-72d2-936c-a7c55c2993fc";
        let people = vec![(U1.to_string(), "Ryo".to_string()), (U2.to_string(), "Ryo Ma".to_string())];
        assert_eq!(names_to_mentions("@ryo ma hello @Ryo!", &people), format!("[user={U2}]Ryo Ma[/user] hello [user={U1}]Ryo[/user]!"));
        // Not a mention: inside a word, or nobody of that name.
        assert_eq!(names_to_mentions("mail me@ryo and @Nobody", &people), "mail me@ryo and @Nobody");
        // A name with brackets is written without them, as the site's own composer does,
        // and somebody with no real uuid is not mentionable.
        let odd = vec![(U1.to_string(), "[Cool] Guy".to_string()), ("someone".to_string(), "Ghost".to_string())];
        assert_eq!(names_to_mentions("@[Cool] Guy and @Ghost", &odd), format!("[user={U1}]Cool Guy[/user] and @Ghost"));
        let odd = vec![(U1.to_uppercase(), "Cool  Guy".to_string())];
        assert_eq!(names_to_mentions("@Cool  Guy", &odd), format!("[user={U1}]Cool Guy[/user]"));
        // "@Ryox" is not "@Ryo".
        assert_eq!(names_to_mentions("@Ryox", &people), "@Ryox");
    }

    #[test]
    fn entities_in_names_decode() {
        assert_eq!(unescape_html("Bob&#x27;s &amp; co"), "Bob's & co");
    }
}
