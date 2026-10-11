//! A link to a GIF, drawn as the GIF.
//!
//! Tenor, Giphy and Klipy are where nearly every GIF in a chat comes from, and
//! a link to one is a link to a page: nothing in its text says it is a moving
//! picture, so a line that was just such a link showed as an address.
//!
//! Discord settles it for its own messages - it unfurls the link and sends the
//! picture with the message (see discord::messages::gif_embed). Nothing else
//! does: an IRC line, a Sneedchat post, a Kick message are a bare address. So
//! here, for every service but Discord, the page is found for what it is.
//!
//! What can be done differs by site, and only what can be done honestly is:
//!
//! - **Tenor** says what its pages show in OpenGraph tags, one request for the
//!   page and nothing more.
//! - **Giphy** puts the picture at an address made of the page's own id, so no
//!   request is made at all.
//! - **Klipy** puts its pages behind a challenge that a program cannot pass, and
//!   is left alone rather than got round. Its pictures arrive as such wherever a
//!   Discord account carries them, and its direct links - which end in a file
//!   extension - are drawn like any other.
//!
//! Like the YouTube cards (unfurl.rs) this is the account's own request: it goes
//! the account's way, through Tor where the account is, and is never made
//! directly instead when no route is ready.

use crate::model::Embed;
use crate::state::AppState;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// A link to a GIF's page, and what it is on.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum GifLink {
    Tenor { url: String },
    Giphy { url: String, id: String },
}

impl GifLink {
    fn url(&self) -> &str {
        match self {
            GifLink::Tenor { url } | GifLink::Giphy { url, .. } => url,
        }
    }
}

/// The address a link ends at: up to whitespace or the punctuation that closes
/// a sentence or a bracket round it.
fn address_at(text: &str, start: usize) -> String {
    let rest = &text[start..];
    let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '[' | ']' | '"' | '\'')).unwrap_or(rest.len());
    rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']).to_string()
}

/// Where a page of a given site is, anywhere in `text`: the earliest link of
/// either kind.
pub fn find_link(text: &str) -> Option<GifLink> {
    let mut best: Option<(usize, GifLink)> = None;
    let mut consider = |at: usize, link: GifLink| {
        if best.as_ref().is_none_or(|(pos, _)| at < *pos) {
            best = Some((at, link));
        }
    };

    for scheme_host in ["https://tenor.com/", "https://www.tenor.com/", "http://tenor.com/"] {
        if let Some(at) = text.find(scheme_host) {
            let url = address_at(text, at);
            // `/view/` after an optional locale: `/de/view/`, `/en-GB/view/`.
            let path = &url[scheme_host.len()..];
            let after_locale = match path.split_once('/') {
                Some((first, rest)) if first.len() <= 5 && first != "view" && !rest.is_empty() => rest,
                _ => path,
            };
            if after_locale.starts_with("view/") && after_locale.len() > "view/".len() {
                consider(at, GifLink::Tenor { url });
            }
        }
    }

    for scheme_host in ["https://giphy.com/gifs/", "https://www.giphy.com/gifs/", "https://giphy.com/embed/"] {
        if let Some(at) = text.find(scheme_host) {
            let url = address_at(text, at);
            let tail = url[scheme_host.len()..].split(['?', '#', '/']).next().unwrap_or("");
            // `slug-with-hyphens-AbC123`: the id is what follows the last hyphen.
            let id = tail.rsplit('-').next().unwrap_or("").to_string();
            if id.len() >= 6 && id.chars().all(|c| c.is_ascii_alphanumeric()) {
                consider(at, GifLink::Giphy { url, id });
            }
        }
    }
    best.map(|(_, link)| link)
}

/// What a `<meta>` tag with this `property` says, from a page's HTML.
fn meta_content(html: &str, property: &str) -> Option<String> {
    let needle_double = format!("property=\"{property}\"");
    let needle_single = format!("property='{property}'");
    let mut from = 0;
    while let Some(open) = html[from..].find("<meta") {
        let start = from + open;
        let end = html[start..].find('>').map_or(html.len(), |e| start + e);
        let tag = &html[start..end];
        if tag.contains(&needle_double) || tag.contains(&needle_single) {
            let content = tag.find("content=\"").map(|c| (c + 9, '"')).or_else(|| tag.find("content='").map(|c| (c + 9, '\'')))?;
            let value = &tag[content.0..];
            let value = &value[..value.find(content.1)?];
            return Some(value.replace("&amp;", "&"));
        }
        from = end;
    }
    None
}

/// A Tenor page's GIF, from the tags it describes itself with.
///
/// The video is preferred to the image: it is a fraction of the size, and the
/// window plays one as it plays any inline video, looped and silent.
fn from_tenor_page(html: &str, url: &str) -> Option<Embed> {
    let video = meta_content(html, "og:video").or_else(|| meta_content(html, "og:video:secure_url"));
    let kind = if video.is_some() { "video" } else { "image" };
    let media = video.or_else(|| meta_content(html, "og:image"))?;
    // Only from Tenor's own media hosts: a page cannot make this say "go and
    // load that" about anywhere else.
    if !is_tenor_media(&media) {
        return None;
    }
    let size = |p: &str| meta_content(html, p).and_then(|v| v.parse::<u32>().ok()).filter(|&n| n > 0);
    Some(Embed {
        kind: Some("gif".to_string()),
        url: Some(url.to_string()),
        media_url: Some(media),
        media_kind: Some(kind.to_string()),
        width: size("og:image:width").or_else(|| size("og:video:width")),
        height: size("og:image:height").or_else(|| size("og:video:height")),
        provider: Some("Tenor".to_string()),
        ..Default::default()
    })
}

fn is_tenor_media(url: &str) -> bool {
    let host = url.strip_prefix("https://").and_then(|rest| rest.split('/').next()).unwrap_or("");
    host == "tenor.com" || host.ends_with(".tenor.com")
}

/// A Giphy GIF, from its id alone.
fn from_giphy_id(url: &str, id: &str) -> Embed {
    Embed {
        kind: Some("gif".to_string()),
        url: Some(url.to_string()),
        media_url: Some(format!("https://media.giphy.com/media/{id}/giphy.mp4")),
        media_kind: Some("video".to_string()),
        provider: Some("Giphy".to_string()),
        ..Default::default()
    }
}

/// Pages already looked at, and ones that said nothing. Bounded crudely: all it
/// saves is a repeat request.
static KNOWN: LazyLock<Mutex<HashMap<String, Option<Embed>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const KNOWN_LIMIT: usize = 512;

/// A backlog full of links is read two at a time, not all at once.
static SLOTS: Semaphore = Semaphore::const_new(2);

const TIMEOUT: Duration = Duration::from_secs(15);

/// Puts the GIF on the message if its text has a link to one. Spawned: the
/// message is already out, and nothing here holds it up.
///
/// Not for Discord, which has already said what its links are.
pub fn gif_later(state: &AppState, account_id: &str, buffer_id: &str, msg_id: &str, body: &str) {
    if account_id.starts_with("discord:") {
        return;
    }
    let Some(link) = find_link(body) else { return };
    let (state, account_id, buffer_id, msg_id) =
        (state.clone(), account_id.to_string(), buffer_id.to_string(), msg_id.to_string());
    tokio::spawn(async move {
        let Some(embed) = describe(&state, &account_id, &link).await else { return };
        crate::unfurl::add_embed(&state, &buffer_id, &msg_id, embed);
    });
}

async fn describe(state: &AppState, account_id: &str, link: &GifLink) -> Option<Embed> {
    // Giphy needs nothing from anybody.
    if let GifLink::Giphy { url, id } = link {
        return Some(from_giphy_id(url, id));
    }
    if let Some(known) = KNOWN.lock().unwrap().get(link.url()) {
        return known.clone();
    }
    let _slot = SLOTS.acquire().await.ok()?;
    if let Some(known) = KNOWN.lock().unwrap().get(link.url()) {
        return known.clone();
    }

    let router = crate::net::route::router();
    let routed = router.tunnel_all()
        || state.accounts.route_level_of(account_id).is_some_and(|level| level != "clearnet");
    if routed && router.ready(|_| {}).await.is_err() {
        // No route. Not remembered: the next message may find one.
        return None;
    }
    let client = router.client_if("unfurl", routed, |builder| builder.timeout(TIMEOUT).user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))));

    let response = client.get(link.url()).send().await.ok()?;
    let status = response.status();
    if !status.is_success() {
        // A page that is gone is gone; a refusal says nothing about the GIF.
        if matches!(status.as_u16(), 400 | 404 | 410) {
            remember(link.url(), None);
        }
        return None;
    }
    // Only the head of the page: the tags are in it, and the rest is not worth
    // reading however long it is.
    let html = response.text().await.ok()?;
    let head = &html[..html.len().min(256 * 1024)];
    let found = match link {
        GifLink::Tenor { url } => from_tenor_page(head, url),
        GifLink::Giphy { .. } => None,
    };
    remember(link.url(), found.clone());
    found
}

fn remember(url: &str, embed: Option<Embed>) {
    let mut known = KNOWN.lock().unwrap();
    if known.len() >= KNOWN_LIMIT {
        known.clear();
    }
    known.insert(url.to_string(), embed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_tenor_page_and_where_it_ends() {
        let text = "look at this https://tenor.com/view/rickroll-roll-rick-never-gonna-give-you-up-gif-22954713, so good";
        assert_eq!(
            find_link(text),
            Some(GifLink::Tenor { url: "https://tenor.com/view/rickroll-roll-rick-never-gonna-give-you-up-gif-22954713".to_string() })
        );
    }

    #[test]
    fn a_tenor_page_may_carry_a_locale() {
        assert!(matches!(find_link("https://tenor.com/de/view/x-gif-1"), Some(GifLink::Tenor { .. })));
        assert!(matches!(find_link("https://tenor.com/en-GB/view/x-gif-1"), Some(GifLink::Tenor { .. })));
    }

    #[test]
    fn the_rest_of_tenor_is_not_a_gif() {
        assert_eq!(find_link("https://tenor.com/"), None);
        assert_eq!(find_link("https://tenor.com/search/cats-gifs"), None);
        assert_eq!(find_link("https://tenor.com/view/"), None);
    }

    #[test]
    fn a_giphy_page_is_known_by_its_id_alone() {
        let link = find_link("https://giphy.com/gifs/cat-vibing-music-Z7wFi5b2VEAr6").unwrap();
        assert_eq!(link, GifLink::Giphy { url: "https://giphy.com/gifs/cat-vibing-music-Z7wFi5b2VEAr6".to_string(), id: "Z7wFi5b2VEAr6".to_string() });
        let embed = from_giphy_id("https://giphy.com/gifs/x-Z7wFi5b2VEAr6", "Z7wFi5b2VEAr6");
        assert_eq!(embed.media_url.as_deref(), Some("https://media.giphy.com/media/Z7wFi5b2VEAr6/giphy.mp4"));
        assert_eq!(embed.kind.as_deref(), Some("gif"));
        // A bare id, with no slug.
        assert!(matches!(find_link("https://giphy.com/gifs/Z7wFi5b2VEAr6"), Some(GifLink::Giphy { .. })));
        assert_eq!(find_link("https://giphy.com/gifs/"), None);
    }

    #[test]
    fn the_earliest_link_wins() {
        let text = "https://giphy.com/gifs/a-Z7wFi5b2VEAr6 and https://tenor.com/view/b-gif-2";
        assert!(matches!(find_link(text), Some(GifLink::Giphy { .. })));
        let text = "https://tenor.com/view/b-gif-2 and https://giphy.com/gifs/a-Z7wFi5b2VEAr6";
        assert!(matches!(find_link(text), Some(GifLink::Tenor { .. })));
    }

    /// The tags a Tenor page describes itself with, as it sends them.
    const PAGE: &str = r#"<html><head>
<meta class="dynamic" property="og:image" content="https://media1.tenor.com/m/x8v1oNUOmg4AAAAd/rickroll-roll.gif">
<meta class="dynamic" property="og:image:width" content="640">
<meta class="dynamic" property="og:image:height" content="480">
<meta class="dynamic" property="og:video" content="https://media.tenor.com/x8v1oNUOmg4AAAPo/rickroll-roll.mp4?a=1&amp;b=2">
</head></html>"#;

    #[test]
    fn a_tenor_page_gives_its_video_and_its_size() {
        let e = from_tenor_page(PAGE, "https://tenor.com/view/x-gif-1").unwrap();
        assert_eq!(e.media_url.as_deref(), Some("https://media.tenor.com/x8v1oNUOmg4AAAPo/rickroll-roll.mp4?a=1&b=2"));
        assert_eq!((e.width, e.height), (Some(640), Some(480)));
        assert_eq!(e.provider.as_deref(), Some("Tenor"));
        assert_eq!(e.media_kind.as_deref(), Some("video"));
    }

    #[test]
    fn with_no_video_the_picture_stands_in() {
        let page = r#"<meta property="og:image" content="https://media1.tenor.com/m/a/b.gif">"#;
        let e = from_tenor_page(page, "https://tenor.com/view/x-gif-1").unwrap();
        assert_eq!(e.media_url.as_deref(), Some("https://media1.tenor.com/m/a/b.gif"));
        assert_eq!(e.media_kind.as_deref(), Some("image"));
    }

    #[test]
    fn a_page_cannot_point_somewhere_that_is_not_tenor() {
        let page = r#"<meta property="og:video" content="https://evil.example/x.mp4">"#;
        assert!(from_tenor_page(page, "https://tenor.com/view/x-gif-1").is_none());
        let page = r#"<meta property="og:video" content="https://tenor.com.evil.example/x.mp4">"#;
        assert!(from_tenor_page(page, "https://tenor.com/view/x-gif-1").is_none());
        assert!(from_tenor_page("<html></html>", "https://tenor.com/view/x-gif-1").is_none());
    }

    #[test]
    fn a_link_says_what_it_is_for() {
        let tenor = GifLink::Tenor { url: "https://tenor.com/view/x-gif-1".into() };
        let giphy = GifLink::Giphy { url: "https://giphy.com/gifs/x-abc".into(), id: "abc".into() };
        assert_eq!(tenor.url(), "https://tenor.com/view/x-gif-1");
        assert_eq!(giphy.url(), "https://giphy.com/gifs/x-abc");
    }

    #[test]
    fn a_locale_is_skipped_only_when_it_is_one() {
        let url = |locale: &str| format!("https://tenor.com/{locale}/view/cat-gif-9");
        assert!(find_link(&url("de")).is_some());
        assert!(find_link(&url("en-GB")).is_some(), "five characters is the longest");
        assert!(find_link(&url("en-GBxx")).is_none(), "seven is not a locale");
        assert!(find_link("https://tenor.com/view/cat-gif-9").is_some(), "and none is fine");
        assert!(find_link("https://tenor.com/de/").is_none());
    }

    #[test]
    fn a_tenor_page_is_read_from_its_own_tags() {
        let html = r#"<head><meta property="og:video" content="https://media.tenor.com/abc/cat.mp4?a=1&amp;b=2">
            <meta property='og:video:width' content='480'><meta property="og:video:height" content="270"></head>"#;
        let e = from_tenor_page(html, "https://tenor.com/view/cat-gif-9").unwrap();
        assert_eq!(e.kind.as_deref(), Some("gif"));
        assert_eq!(e.url.as_deref(), Some("https://tenor.com/view/cat-gif-9"));
        assert_eq!(e.media_url.as_deref(), Some("https://media.tenor.com/abc/cat.mp4?a=1&b=2"));
        assert_eq!(e.media_kind.as_deref(), Some("video"));
        assert_eq!((e.width, e.height), (Some(480), Some(270)));
        assert_eq!(e.provider.as_deref(), Some("Tenor"));
    }

    #[test]
    fn a_tenor_page_with_only_a_picture_gives_the_picture() {
        let html = r#"<meta property="og:image" content="https://media.tenor.com/abc/cat.gif"><meta property="og:image:width" content="1"><meta property="og:image:height" content="0">"#;
        let e = from_tenor_page(html, "https://tenor.com/view/cat-gif-9").unwrap();
        assert_eq!(e.media_kind.as_deref(), Some("image"));
        // A size of one is a size; a size of nothing is not one.
        assert_eq!((e.width, e.height), (Some(1), None));
    }

    #[test]
    fn a_page_cannot_point_the_window_at_somewhere_else() {
        let from = |media: &str| from_tenor_page(&format!(r#"<meta property="og:video" content="{media}">"#), "https://tenor.com/view/x-gif-1");
        assert!(from("https://media.tenor.com/a.mp4").is_some());
        assert!(from("https://tenor.com/a.mp4").is_some());
        assert!(from("https://evil.example/a.mp4").is_none());
        assert!(from("https://notenor.com/a.mp4").is_none(), "a host that only ends the same way");
        assert!(from("http://media.tenor.com/a.mp4").is_none(), "and only over https");
        assert!(from_tenor_page("<html>nothing</html>", "https://tenor.com/view/x-gif-1").is_none());
    }

    #[test]
    fn a_giphy_gif_is_made_of_its_id() {
        let e = from_giphy_id("https://giphy.com/gifs/dog-Z7wFi5b2VEAr6", "Z7wFi5b2VEAr6");
        assert_eq!(e.kind.as_deref(), Some("gif"));
        assert_eq!(e.url.as_deref(), Some("https://giphy.com/gifs/dog-Z7wFi5b2VEAr6"));
        assert_eq!(e.media_url.as_deref(), Some("https://media.giphy.com/media/Z7wFi5b2VEAr6/giphy.mp4"));
        assert_eq!(e.media_kind.as_deref(), Some("video"));
        assert_eq!(e.provider.as_deref(), Some("Giphy"));
    }

    #[test]
    fn what_was_looked_at_is_remembered_until_the_table_is_full() {
        remember("https://tenor.com/view/remember-a", None);
        remember("https://tenor.com/view/remember-b", Some(Embed::default()));
        let known = KNOWN.lock().unwrap();
        assert!(known.contains_key("https://tenor.com/view/remember-a"), "a page that said nothing is remembered too");
        assert!(known.get("https://tenor.com/view/remember-b").is_some_and(|e| e.is_some()));
    }
}
