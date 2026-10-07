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
        state.runtime.set_message_embeds(&state, &buffer_id, &msg_id, &[embed]);
    });
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
    let mut found = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(3 * attempt)).await;
        }
        let Ok(response) = client.get(&asked).send().await else { continue };
        let status = response.status();
        if status.is_success() {
            let Ok(answer) = response.json::<serde_json::Value>().await else { continue };
            found = Some(parse(&answer, &watch));
            break;
        } else if answers_for_the_video(status.as_u16()) {
            // Private, removed, or not embeddable: YouTube's answer, and it
            // will be the same next time.
            found = Some(None);
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
}
