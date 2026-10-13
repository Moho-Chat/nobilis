//! What a YouTube link in a message is: its title and its channel.
//!
//! Discord describes a posted video itself, and its embed arrives with the
//! message. Nothing else does - an IRC line, a Sneedchat post, a Kick chat
//! message, a Matrix event in an encrypted room are a bare URL - so the
//! frontend had a thumbnail and nothing to say what it was a thumbnail of.
//! YouTube answers that for anyone, with no key, through oEmbed: one small
//! JSON request per video.
//!
//! Here rather than in a window because every frontend would otherwise make
//! the same request, and because it is the account's request: it goes the
//! account's way. A channel on Tor has its videos described through Tor, and
//! with no route ready the card is simply not made - never fetched directly
//! instead.
//!
//! Stored on the message as an ordinary embed, so scrollback keeps it and a
//! window opened tomorrow does not ask again.

use crate::model::Embed;
use crate::state::AppState;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Where each form of link keeps the video id.
const MARKERS: [&str; 9] = [
    "youtube.com/shorts/",
    "youtube.com/embed/",
    "youtube-nocookie.com/embed/",
    "youtube.com/live/",
    "youtube.com/v/",
    "youtube.com/e/",
    "youtube.com/watch/",
    "youtu.be/",
    // A watch link whose query puts the id anywhere: `?feature=share&v=`,
    // `?t=30&v=`. Found by `watch_query_id`, not by what follows the marker.
    "youtube.com/watch?",
];

/// The id in a watch link's query, wherever among its parameters `v=` sits.
fn watch_query_id(after_question_mark: &str) -> Option<String> {
    let query = after_question_mark.split(|c: char| c.is_whitespace() || c == '#').next()?;
    let id = query.split('&').find_map(|pair| pair.strip_prefix("v="))?;
    let id: String = id.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(11).collect();
    (id.len() == 11).then_some(id)
}

/// The video id from the first YouTube link in `text`, in any form a link
/// arrives in. The same forms the frontend recognises (lib/format.ts), so a
/// card is made for exactly the links that are drawn as videos.
pub fn youtube_id(text: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for marker in MARKERS {
        let mut from = 0;
        while let Some(at) = text[from..].find(marker) {
            let start = from + at + marker.len();
            let id: String = if marker.ends_with("watch?") {
                watch_query_id(&text[start..]).unwrap_or_default()
            } else {
                text[start..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .take(11)
                    .collect()
            };
            if id.len() == 11 {
                if best.as_ref().is_none_or(|(pos, _)| from + at < *pos) {
                    best = Some((from + at, id));
                }
                break;
            }
            from = start;
        }
    }
    best.map(|(_, id)| id)
}

/// Videos already described, and ones YouTube would not describe (private,
/// removed). Bounded crudely - a full table is emptied - since all it saves
/// is a repeat request.
static KNOWN: LazyLock<Mutex<HashMap<String, Option<Embed>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const KNOWN_LIMIT: usize = 512;

/// A backlog full of links is fetched two at a time, not all at once.
static SLOTS: Semaphore = Semaphore::const_new(2);

const TIMEOUT: Duration = Duration::from_secs(15);

/// Describes the message's YouTube link, if it has one, and puts the card on
/// it. Spawned: the message is already out.
pub fn youtube_later(state: &AppState, account_id: &str, buffer_id: &str, msg_id: &str, body: &str) {
    let Some(id) = youtube_id(body) else { return };
    let (state, account_id, buffer_id, msg_id) =
        (state.clone(), account_id.to_string(), buffer_id.to_string(), msg_id.to_string());
    tokio::spawn(async move {
        let Some(embed) = describe(&state, &account_id, &id).await else { return };
        add_embed(&state, &buffer_id, &msg_id, embed);
    });
}

/// Messages already asked about, and when. A conversation is read many times;
/// a link that could not be described should be tried again, but not on every
/// glance.
static ASKED: LazyLock<Mutex<HashMap<String, std::time::Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(600);
/// How many of a page's messages are described at once, newest first.
const BACKLOG_LIMIT: usize = 30;

/// Describes the links in messages that were stored without a card.
///
/// Live messages are described as they arrive (`Runtime::record_message_at`),
/// but not everything reaches the store that way: Matrix history read back
/// from the server, Discord's catch-up, scrollback from before cards existed,
/// and a link that could not be described when it was said. Reading a
/// conversation is the moment to make up for those, whichever service it is
/// on.
pub fn backlog_later(state: &AppState, buffer_id: &str, messages: &[crate::model::Message]) {
    let Some(account_id) = state.runtime.get_buffer(buffer_id).map(|b| b.account_id) else { return };
    let now = std::time::Instant::now();
    let mut asked = ASKED.lock().unwrap();
    if asked.len() > 4096 {
        asked.clear();
    }
    for message in messages
        .iter()
        .rev()
        .filter(|m| wants_a_card(!m.embeds.is_empty(), &m.kind, &m.body))
        .take(BACKLOG_LIMIT)
    {
        if !first_asking(&mut asked, format!("{buffer_id}\u{0}{}", message.id), now) {
            continue;
        }
        youtube_later(state, &account_id, buffer_id, &message.id, &message.body);
        crate::gifs::gif_later(state, &account_id, buffer_id, &message.id, &message.body);
    }
}

/// Whether a stored message is one that could be given a card: it has none,
/// somebody said it, and it carries a link that is described.
fn wants_a_card(has_cards: bool, kind: &str, body: &str) -> bool {
    !has_cards && kind != "system" && (youtube_id(body).is_some() || crate::gifs::find_link(body).is_some())
}

/// Whether this message has not been asked about lately - and if so, notes that
/// it is being asked about now.
fn first_asking(asked: &mut HashMap<String, std::time::Instant>, key: String, now: std::time::Instant) -> bool {
    if asked.get(&key).is_some_and(|at| now.duration_since(*at) < ASK_AGAIN_AFTER) {
        return false;
    }
    asked.insert(key, now);
    true
}

/// Puts a card on a message beside whatever it already has.
///
/// More than one thing can describe a line - a YouTube link and a GIF in the
/// same message, each worked out on its own and finishing in either order - so
/// a card is added to what is there, not written over it. One that a message
/// already carries of the same link is not added twice.
pub fn add_embed(state: &AppState, buffer_id: &str, msg_id: &str, embed: Embed) {
    let mut embeds = state.store.get_message(buffer_id, msg_id).ok().flatten().map(|m| m.embeds).unwrap_or_default();
    if embeds.iter().any(|e| e.url.is_some() && e.url == embed.url) {
        return;
    }
    embeds.push(embed);
    state.runtime.set_message_embeds(state, buffer_id, msg_id, &embeds);
}

async fn describe(state: &AppState, account_id: &str, id: &str) -> Option<Embed> {
    if let Some(known) = KNOWN.lock().unwrap().get(id) {
        return known.clone();
    }
    let _slot = SLOTS.acquire().await.ok()?;
    // Asked again: the request before this one in the queue may have been
    // for the same video.
    if let Some(known) = KNOWN.lock().unwrap().get(id) {
        return known.clone();
    }

    let router = crate::net::route::router();
    let routed = router.tunnel_all()
        || state.accounts.route_level_of(account_id).is_some_and(|level| level != "clearnet");
    if routed && router.ready(|_| {}).await.is_err() {
        // No route. Not remembered: the next message may find one.
        return None;
    }
    let client = router.client_if("unfurl", routed, |builder| builder.timeout(TIMEOUT));

    let watch = format!("https://www.youtube.com/watch?v={id}");
    let asked = format!(
        "https://www.youtube.com/oembed?format=json&url={}",
        url::form_urlencoded::byte_serialize(watch.as_bytes()).collect::<String>()
    );
    // Asked up to three times. Over Tor - Sneedchat's route - YouTube
    // regularly turns an exit node away with a 403 or 429 that says nothing
    // about the video, and a later try leaves by another one.
    //
    // And when oEmbed is refused outright, the video's own page is read for
    // the same two facts. YouTube answers oEmbed with a 403 to every Tor exit,
    // whichever circuit asks, while the page itself loads - so for an account
    // on Tor the page is the only way a card is ever made.
    let mut found = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(3 * attempt)).await;
        }
        if let Ok(response) = client.get(&asked).send().await {
            let status = response.status();
            if status.is_success() {
                if let Ok(answer) = response.json::<serde_json::Value>().await {
                    found = Some(parse(&answer, &watch));
                    break;
                }
            } else if answers_for_the_video(status.as_u16()) {
                // Private, removed, or not embeddable: YouTube's answer, and it
                // will be the same next time.
                found = Some(None);
                break;
            }
        }
        if let Some(card) = read_page(&client, &watch).await {
            found = Some(Some(card));
            break;
        }
    }
    // Nothing settled: not remembered, so the next message tries afresh.
    let found = found?;
    let mut known = KNOWN.lock().unwrap();
    if known.len() >= KNOWN_LIMIT {
        known.clear();
    }
    known.insert(id.to_string(), found.clone());
    found
}

/// Whether an oEmbed status is about the video rather than about who asked:
/// bad request, unauthorised (age-restricted, members only) and not found.
/// 403 and 429 are a refused client, and 5xx is YouTube's own trouble.
fn answers_for_the_video(status: u16) -> bool {
    matches!(status, 400 | 401 | 404)
}

/// The card, from YouTube's answer. None without a title: a card that says
/// nothing is worse than the thumbnail on its own.
fn parse(answer: &serde_json::Value, watch: &str) -> Option<Embed> {
    let text = |key: &str| answer[key].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    Some(Embed {
        title: Some(text("title")?),
        url: Some(watch.to_string()),
        provider: Some(text("provider_name").unwrap_or_else(|| "YouTube".to_string())),
        author: text("author_name"),
        // YouTube's own red, the bar down the card's edge.
        color: Some(0xFF0000),
        ..Default::default()
    })
}

/// The card from the video's own page, for when oEmbed will not answer.
///
/// The page is a JavaScript application that carries its data inline, and
/// the title and channel are in it as JSON. Nothing promises where, so this
/// looks for the renderers that have held them and takes the first that is
/// there; a page that has none of them - a consent wall, a removed video, a
/// layout YouTube has since changed - makes no card, the same as no answer.
async fn read_page(client: &reqwest::Client, watch: &str) -> Option<Embed> {
    let response = client
        .get(watch)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0")
        .header("Accept-Language", "en-US,en;q=0.9")
        // Declines the consent page an exit in Europe is otherwise sent to.
        .header("Cookie", "SOCS=CAI")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    parse_page(&response.text().await.ok()?, watch)
}

/// The JSON string that follows `marker`, decoded.
fn string_after(page: &str, marker: &str) -> Option<String> {
    let rest = &page[page.find(marker)? + marker.len()..];
    let mut end = None;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => {
                end = Some(i);
                break;
            }
            _ => {}
        }
    }
    let literal = format!("\"{}\"", &rest[..end?]);
    serde_json::from_str::<String>(&literal).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The card from a watch page's text. None without a title.
fn parse_page(page: &str, watch: &str) -> Option<Embed> {
    let title = [
        "\"videoDescriptionHeaderRenderer\":{\"title\":{\"runs\":[{\"text\":\"",
        "\"videoPrimaryInfoRenderer\":{\"title\":{\"runs\":[{\"text\":\"",
        "\"playerOverlayVideoDetailsRenderer\":{\"title\":{\"simpleText\":\"",
    ]
    .iter()
    .find_map(|marker| string_after(page, marker))?;
    // The channel follows the title in the same renderer.
    let author = page
        .find("\"videoDescriptionHeaderRenderer\":{")
        .and_then(|at| string_after(&page[at..], "\"channel\":{\"simpleText\":\""))
        .or_else(|| string_after(page, "\"ownerChannelName\":\""));
    Some(Embed {
        title: Some(title),
        url: Some(watch.to_string()),
        provider: Some("YouTube".to_string()),
        author,
        color: Some(0xFF0000),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_video_in_every_form_a_link_arrives_in() {
        for url in [
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=4",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?t=30&list=PLx&v=dQw4w9WgXcQ&index=2",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&si=x",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
            "https://youtube.com/watch/dQw4w9WgXcQ",
        ] {
            assert_eq!(youtube_id(url).as_deref(), Some("dQw4w9WgXcQ"), "{url}");
        }
        assert_eq!(youtube_id("https://example.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(youtube_id("https://youtu.be/short"), None);
        assert_eq!(youtube_id("no links here"), None);
    }

    #[test]
    fn the_first_link_is_the_one_described() {
        let body = "first https://www.youtube.com/watch?v=aaaaaaaaaaa then https://youtu.be/bbbbbbbbbbb";
        assert_eq!(youtube_id(body).as_deref(), Some("aaaaaaaaaaa"));
        let body = "first https://youtu.be/bbbbbbbbbbb then https://www.youtube.com/watch?v=aaaaaaaaaaa";
        assert_eq!(youtube_id(body).as_deref(), Some("bbbbbbbbbbb"));
    }

    #[test]
    fn a_refused_client_is_not_a_verdict_on_the_video() {
        for status in [400, 401, 404] {
            assert!(answers_for_the_video(status), "{status}");
        }
        for status in [403, 408, 429, 500, 502, 503] {
            assert!(!answers_for_the_video(status), "{status}");
        }
    }

    #[test]
    fn a_card_needs_a_title() {
        let watch = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let card = parse(
            &serde_json::json!({ "title": "Never Gonna Give You Up", "author_name": "Rick Astley", "provider_name": "YouTube" }),
            watch,
        )
        .unwrap();
        assert_eq!(card.title.as_deref(), Some("Never Gonna Give You Up"));
        assert_eq!(card.author.as_deref(), Some("Rick Astley"));
        assert_eq!(card.provider.as_deref(), Some("YouTube"));
        assert_eq!(card.url.as_deref(), Some(watch));
        assert!(parse(&serde_json::json!({ "title": "  ", "author_name": "x" }), watch).is_none());
    }

    #[test]
    fn a_card_from_the_page_when_oembed_is_refused() {
        let watch = "https://www.youtube.com/watch?v=_-agl0pOQfs";
        let page = r#"<script>var x={"contents":{"videoDescriptionHeaderRenderer":{"title":{"runs":[{"text":"Insane Clown Posse -  \"Miracles\" (Official Music Video)"}]},"channel":{"simpleText":"Psychopathic Records"},"views":{"simpleText":"19,675,62"}}}}</script>"#;
        let card = parse_page(page, watch).unwrap();
        assert_eq!(card.title.as_deref(), Some("Insane Clown Posse -  \"Miracles\" (Official Music Video)"));
        assert_eq!(card.author.as_deref(), Some("Psychopathic Records"));
        assert_eq!(card.url.as_deref(), Some(watch));
        assert_eq!(card.provider.as_deref(), Some("YouTube"));
        // The older layout, which names no channel in the same place.
        let page = r#"{"videoPrimaryInfoRenderer":{"title":{"runs":[{"text":"A \u0026 B"}]},"viewCount":{}}}"#;
        let card = parse_page(page, watch).unwrap();
        assert_eq!(card.title.as_deref(), Some("A & B"));
        assert_eq!(card.author, None);
        // A consent wall, or a removed video, has no title: no card.
        assert!(parse_page("<html><title> - YouTube</title></html>", watch).is_none());
    }

    #[test]
    fn ids_with_hyphens_and_underscores_are_whole_in_every_form() {
        // The ones that were lost: an id may start with either, and holds both.
        for url in [
            "https://www.youtube.com/watch?v=_-agl0pOQfs",
            "https://www.youtube.com/watch?v=_-agl0pOQfs&t=3",
            "https://www.youtube.com/watch?t=3&v=_-agl0pOQfs",
            "https://youtu.be/_-agl0pOQfs?si=abc",
            "https://www.youtube.com/shorts/_-agl0pOQfs",
            "https://www.youtube.com/embed/_-agl0pOQfs",
        ] {
            assert_eq!(youtube_id(url).as_deref(), Some("_-agl0pOQfs"), "{url}");
        }
        assert_eq!(youtube_id("https://youtu.be/a-b_c-d_e-f").as_deref(), Some("a-b_c-d_e-f"));
        // Exactly eleven, not a character more.
        assert_eq!(youtube_id("https://youtu.be/aaaaaaaaaaaXYZ").as_deref(), Some("aaaaaaaaaaa"));
    }

    #[test]
    fn a_v_that_is_not_in_the_link_is_not_its_video() {
        // After whitespace the link has ended, and after a # it is a fragment.
        assert_eq!(youtube_id("https://www.youtube.com/watch?x=1 and v=aaaaaaaaaaa"), None);
        assert_eq!(youtube_id("https://www.youtube.com/watch?x=1#v=aaaaaaaaaaa"), None);
        assert_eq!(youtube_id("https://www.youtube.com/watch?v=aaaaaaaaaaa#t=1").as_deref(), Some("aaaaaaaaaaa"));
        // And a character that is not part of an id ends it, short.
        assert_eq!(youtube_id("https://www.youtube.com/watch?v=aaaaa!aaaaa"), None);
    }

    #[test]
    fn both_kinds_of_card_are_youtube_red() {
        let watch = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let card = parse(&serde_json::json!({ "title": "T" }), watch).unwrap();
        assert_eq!(card.color, Some(0xFF0000));
        let page = r#"{"videoPrimaryInfoRenderer":{"title":{"runs":[{"text":"T"}]}}}"#;
        assert_eq!(parse_page(page, watch).unwrap().color, Some(0xFF0000));
    }

    #[test]
    fn a_message_wants_a_card_when_it_has_none_and_a_link_that_is_described() {
        let yt = "look https://youtu.be/dQw4w9WgXcQ";
        assert!(wants_a_card(false, "chat", yt));
        assert!(wants_a_card(false, "chat", "https://tenor.com/view/cat-gif-123"));
        assert!(!wants_a_card(true, "chat", yt), "it already has a card");
        assert!(!wants_a_card(false, "system", yt), "nobody said it");
        assert!(!wants_a_card(false, "chat", "no links here"));
        assert!(!wants_a_card(false, "chat", "https://example.com/page"));
    }

    #[test]
    fn a_message_is_asked_about_once_in_a_while() {
        let mut asked = HashMap::new();
        let t = std::time::Instant::now();
        assert!(first_asking(&mut asked, "a".into(), t), "never asked");
        assert!(!first_asking(&mut asked, "a".into(), t), "asked just now");
        assert!(!first_asking(&mut asked, "a".into(), t + ASK_AGAIN_AFTER - Duration::from_secs(1)), "still too soon");
        assert!(first_asking(&mut asked, "a".into(), t + ASK_AGAIN_AFTER), "long enough to try again");
        assert!(first_asking(&mut asked, "b".into(), t), "another message is its own question");
    }

    #[test]
    fn a_card_is_added_beside_what_a_message_has_and_not_twice() {
        let state = crate::backend::kick::testkit::simulated_daemon("add-embed");
        state.runtime.record_message(
            &state, "kick:tester", "chan", "channel", "bob", "hello", false, "chat", None,
            Some("m1".into()), false, None, Vec::new(), Vec::new(), None,
        );
        let buffer = crate::model::buffer_id("kick:tester", "chan");
        let card = |url: &str| Embed { title: Some("T".into()), url: Some(url.into()), ..Default::default() };
        add_embed(&state, &buffer, "m1", card("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        add_embed(&state, &buffer, "m1", card("https://tenor.com/view/x-gif-1"));
        // The same link again - from the other road it can arrive by.
        add_embed(&state, &buffer, "m1", card("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        let stored = state.store.get_message(&buffer, "m1").unwrap().unwrap();
        let urls: Vec<_> = stored.embeds.iter().filter_map(|e| e.url.as_deref()).collect();
        assert_eq!(urls, vec!["https://www.youtube.com/watch?v=dQw4w9WgXcQ", "https://tenor.com/view/x-gif-1"]);
        // A card with no address is never taken for a repeat of another with none.
        add_embed(&state, &buffer, "m1", Embed { title: Some("one".into()), ..Default::default() });
        add_embed(&state, &buffer, "m1", Embed { title: Some("two".into()), ..Default::default() });
        assert_eq!(state.store.get_message(&buffer, "m1").unwrap().unwrap().embeds.len(), 4);
    }
}
