//! What the chat offers besides messages: its emoticons, its reactions, and
//! what each room lets this account do.
//!
//! Emoticons come from `GET /chat/v1/emoticons` (revised whenever the chat's
//! `welcome` or `emoticons_changed` names a new revision). The reaction set
//! and each room's permissions are not in the chat's API at all: they are in
//! the chat page the forum serves, as the reaction picker's template and as
//! each room tab's `data-chat-permissions`. Pictures are fetched through the
//! account's own client and cached as local files, since the site's gate is
//! in front of them too.

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One picture with every code that types it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emoticon {
    pub title: String,
    pub codes: Vec<String>,
    /// The local picture, `file://...`, once fetched.
    pub path: Option<String>,
    /// Whether the site's own picker offers it; the rest are aliases and
    /// retired codes that still render.
    pub picker: bool,
}

/// One of the reactions a message can be given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub code: String,
    pub title: String,
    pub path: Option<String>,
}

/// What a room lets this account do, from its tab's permission bits: the
/// first three are posting, reacting and whispering (what a guest is told it
/// cannot do), and any later bit is a moderator's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Powers {
    pub post: bool,
    pub react: bool,
    pub whisper: bool,
    pub moderate: bool,
}

impl Powers {
    pub fn from_bits(bits: &str) -> Self {
        let on = |i: usize| bits.as_bytes().get(i) == Some(&b'1');
        Self { post: on(0), react: on(1), whisper: on(2), moderate: bits.bytes().skip(3).any(|b| b == b'1') }
    }
}

#[derive(Default)]
struct Catalogue {
    revision: Option<String>,
    emoticons: Vec<Emoticon>,
    reactions: Vec<Reaction>,
    /// Per room uuid: (permission bits, subtitle).
    rooms: HashMap<String, (String, String)>,
    /// The badges members are grouped by, highest first, as the page orders
    /// them (`data-chat-member-groups`, "staff" today).
    member_groups: Vec<String>,
}

fn catalogue() -> &'static Mutex<Catalogue> {
    static C: OnceLock<Mutex<Catalogue>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The emoticons as `listSneedchatSmilies` answers: label, codes, picture.
/// None until the chat's own list has been read; the client then keeps its
/// bundled table.
pub fn smilies_json() -> Option<Vec<serde_json::Value>> {
    let c = catalogue().lock().unwrap();
    if c.emoticons.is_empty() {
        return None;
    }
    Some(
        c.emoticons
            .iter()
            .filter_map(|e| Some(serde_json::json!({ "label": e.title, "aliases": e.codes, "path": e.path.clone()? })))
            .collect(),
    )
}

/// The picker's Sneedchat section: the emoticons the site's own picker
/// offers, each picked as its first code.
pub fn picker_entries() -> Option<Vec<crate::model::EmojiEntry>> {
    let c = catalogue().lock().unwrap();
    if c.emoticons.is_empty() {
        return None;
    }
    Some(
        c.emoticons
            .iter()
            .filter(|e| e.picker && e.path.is_some())
            .filter_map(|e| Some(crate::model::EmojiEntry { id: e.codes.first()?.clone(), name: e.title.clone(), url: None, animated: false, locked: false }))
            .collect(),
    )
}

/// The reactions a message can be given, for the reaction picker.
pub fn reactions() -> Vec<Reaction> {
    catalogue().lock().unwrap().reactions.clone()
}

/// What a room lets this account do; everything a member can do when the
/// room's tab has not been read.
pub fn powers(room: &str) -> Powers {
    catalogue().lock().unwrap().rooms.get(room).map(|(bits, _)| Powers::from_bits(bits)).unwrap_or(Powers { post: true, react: true, whisper: true, moderate: false })
}

/// A room's subtitle as its tab carries it.
pub fn subtitle(room: &str) -> Option<String> {
    catalogue().lock().unwrap().rooms.get(room).map(|(_, s)| s.clone()).filter(|s| !s.is_empty())
}

/// The token a reaction is held under in the store and drawn from by the
/// window: its code, its title and its picture.
pub fn reaction_token(code: &str) -> String {
    let c = catalogue().lock().unwrap();
    match c.reactions.iter().find(|r| r.code == code) {
        Some(Reaction { title, path: Some(path), .. }) => {
            let title: String = title.chars().filter(|ch| ch.is_alphanumeric() || *ch == ' ').collect();
            format!("[sneedchat:reaction:{code}:{title}:{path}]")
        }
        _ => format!("[sneedchat:reaction:{code}::]"),
    }
}

/// The reaction code inside whatever the window hands back: a stored token,
/// or a pick from the picker (`[emote:<code>:<name>]`).
pub fn reaction_code(token: &str) -> Option<String> {
    let inner = token.strip_prefix("[sneedchat:reaction:").or_else(|| token.strip_prefix("[emote:"))?;
    let code = inner.split(':').next()?;
    (!code.is_empty() && code.len() <= 64 && code.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))).then(|| code.to_string())
}

/// A reaction as the page lists it (code, title, picture address), and a
/// room tab (uuid, permission bits, subtitle).
type PageReaction = (String, String, String);
type PageRoom = (String, String, String);

/// Reads the reaction picker and the room tabs out of a chat page.
pub(super) fn parse_page(html: &str) -> (Vec<PageReaction>, Vec<PageRoom>) {
    let mut reactions = Vec::new();
    if let Some(start) = html.find("data-chat-template=\"reaction-picker\"") {
        let rest = &html[start..];
        let end = rest.find("</template>").unwrap_or(rest.len());
        let mut part = &rest[..end];
        while let Some(at) = part.find("data-reaction-code=\"") {
            let tag_start = part[..at].rfind('<').unwrap_or(0);
            let tail = &part[tag_start..];
            let close = tail.find("</button>").unwrap_or(tail.len());
            let button = &tail[..close];
            if let (Some(code), Some(title)) = (protocol::attr(button, "data-reaction-code"), protocol::attr(button, "title")) {
                let img = button.find("<img").map(|i| &button[i..]).and_then(|t| protocol::attr(t, "src")).unwrap_or_default();
                if !reactions.iter().any(|(c, _, _): &(String, String, String)| c == &code) {
                    reactions.push((code, protocol::unescape_html(&title), protocol::unescape_html(&img)));
                }
            }
            part = &tail[close.max(1).min(tail.len())..];
        }
    }
    let mut rooms = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("class=\"ephyraChat-tab\"") {
        let tail = &rest[at..];
        let end = tail.find('>').unwrap_or(tail.len());
        let tag = &tail[..end];
        if let Some(uuid) = protocol::attr(tag, "data-chat-room-uuid") {
            let bits = protocol::attr(tag, "data-chat-permissions").unwrap_or_default();
            let subtitle = protocol::attr(tag, "data-chat-room-subtitle").map(|s| protocol::unescape_html(&s)).unwrap_or_default();
            rooms.push((uuid, bits, subtitle));
        }
        rest = &tail[end.max(1)..];
    }
    (reactions, rooms)
}

/// The badges members are grouped by, highest first.
pub fn member_groups() -> Vec<String> {
    let c = catalogue().lock().unwrap();
    if c.member_groups.is_empty() {
        vec!["staff".to_string()]
    } else {
        c.member_groups.clone()
    }
}

/// The group a member is listed under: their highest badge in the page's
/// order, or none for everyone else.
pub fn member_group(badges: &[String]) -> Option<String> {
    member_groups().into_iter().find(|g| badges.iter().any(|b| b == g))
}

/// Takes in a chat page: the room tabs at once, the reaction pictures fetched.
pub(super) async fn take_page(http: &http::HttpClient, host: &str, html: &str) {
    if let Some(groups) = protocol::attr(html, "data-chat-member-groups") {
        let groups: Vec<String> = groups.split_whitespace().map(str::to_string).collect();
        if !groups.is_empty() {
            catalogue().lock().unwrap().member_groups = groups;
        }
    }
    let (reactions, rooms) = parse_page(html);
    if !rooms.is_empty() {
        let mut c = catalogue().lock().unwrap();
        for (uuid, bits, subtitle) in rooms {
            c.rooms.insert(uuid, (bits, subtitle));
        }
    }
    if reactions.is_empty() {
        return;
    }
    let mut out = Vec::new();
    for (code, title, url) in reactions {
        let path = if url.is_empty() { None } else { cached_avatar_path(http, host, &format!("reaction-{code}"), &super::chat::mirrored(&url, host)).await };
        out.push(Reaction { code, title, path });
    }
    catalogue().lock().unwrap().reactions = out;
}

/// Reads the emoticon list when the chat names a revision this has not read.
pub(super) async fn take_emoticons(state: &AppState, session: &Session, host: &str, revision: Option<&str>) {
    {
        let c = catalogue().lock().unwrap();
        if revision.is_some() && c.revision.as_deref() == revision && !c.emoticons.is_empty() {
            return;
        }
    }
    let list = match super::chat::api_get(session, host, "emoticons").await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("sneedchat: reading the emoticons: {e:#}");
            return;
        }
    };
    let mut grouped: Vec<(String, Emoticon)> = Vec::new();
    for e in list["emoticons"].as_array().into_iter().flatten() {
        let (Some(code), Some(url)) = (e["code"].as_str(), e["image_url"].as_str()) else { continue };
        let title = e["title"].as_str().unwrap_or(code).to_string();
        let picker = e["picker"].as_bool().unwrap_or(false);
        match grouped.iter_mut().find(|(u, _)| u == url) {
            Some((_, found)) => {
                found.codes.push(code.to_string());
                found.picker |= picker;
            }
            None => grouped.push((url.to_string(), Emoticon { title, codes: vec![code.to_string()], path: None, picker })),
        }
    }
    // The pictures, a few at a time: there are a few hundred, each a request.
    let http = &session.http;
    let fetched = futures::stream::iter(grouped)
        .map(|(url, mut e)| async move {
            let key = format!("emoticon-{:016x}", fnv(&url));
            e.path = cached_avatar_path(http, host, &key, &super::chat::mirrored(&url, host)).await;
            e
        })
        .buffered(6)
        .collect::<Vec<_>>()
        .await;
    {
        let mut c = catalogue().lock().unwrap();
        c.emoticons = fetched;
        c.revision = list["emoticon_revision"].as_str().map(str::to_string).or_else(|| revision.map(str::to_string));
    }
    state.events.emit("sneedchatSmilies", serde_json::json!({}));
}

/// A short stable name for a picture's address.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

use futures::StreamExt as _;

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<a class="ephyraChat-tab" href="/ephyra-chat/general.1/"
        data-chat-room-uuid="01a1f0f8-2d19-79de-bdc6-37b3b32de6f0"
        data-chat-permissions="111000000"
        data-chat-room-subtitle="test &amp; more"
        aria-current="false">General</a>
        <template data-chat-template="reaction-picker"><div class="ephyraChat-reactionPicker"><button type="button" class="ephyraChat-action" data-chat-action="react" data-reaction-code="reaction_1" title="Like"><img src="https://kiwifarms.st/styles/custom/emoji/Like.svg" alt="Like" width="24" height="24" /></button><button type="button" class="ephyraChat-action" data-chat-action="react" data-reaction-code="reaction_14" title="Dislike"><img src="https://kiwifarms.st/styles/custom/emoji/Dislike.svg?20251115" alt="Dislike" /></button></div></template>"#;

    #[test]
    fn reads_reactions_and_room_tabs_off_the_page() {
        let (reactions, rooms) = parse_page(PAGE);
        assert_eq!(reactions.len(), 2);
        assert_eq!(reactions[0], ("reaction_1".into(), "Like".into(), "https://kiwifarms.st/styles/custom/emoji/Like.svg".into()));
        assert_eq!(reactions[1].0, "reaction_14");
        assert_eq!(rooms, vec![("01a1f0f8-2d19-79de-bdc6-37b3b32de6f0".into(), "111000000".into(), "test & more".into())]);
    }

    #[test]
    fn members_group_by_their_highest_badge() {
        assert_eq!(member_group(&["hidden".into(), "staff".into(), "primary".into()]).as_deref(), Some("staff"));
        assert_eq!(member_group(&["green".into(), "primary".into()]), None);
    }

    #[test]
    fn permission_bits() {
        assert_eq!(Powers::from_bits("111000000"), Powers { post: true, react: true, whisper: true, moderate: false });
        assert_eq!(Powers::from_bits("000000000"), Powers::default());
        assert!(Powers::from_bits("111010000").moderate);
    }

    #[test]
    fn reaction_codes_come_back_from_either_form() {
        assert_eq!(reaction_code("[emote:reaction_2:Agree]").as_deref(), Some("reaction_2"));
        assert_eq!(reaction_code("[sneedchat:reaction:reaction_2:Agree:file:///x/reaction-reaction_2.png#1]").as_deref(), Some("reaction_2"));
        assert_eq!(reaction_code("👍"), None);
        assert_eq!(reaction_code("[emote:../x:y]"), None);
    }
}
