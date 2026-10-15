//! The emotes of Twitch's own that a chat is written in: whether one is animated, when an account's own were
//! last read (they change under it: a reward unlocks one, a 24-hour unlock lapses), and how a message the
//! account sends is turned between the picture the box holds and the word Twitch reads.
//!
//! BetterTTV, FrankerFaceZ and 7TV are not here: they are browser extensions that draw over Twitch's chat, and
//! Twitch itself shows those words as words, so moho does not pretend to.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Who owns emotes an account has: found out once from Twitch and kept, since a channel's name and picture change
/// rarely and a subscription list can name dozens.
#[derive(Clone)]
pub struct Owner {
    pub login: String,
    pub name: String,
    pub picture: String,
}

fn known_owners() -> &'static Mutex<HashMap<String, Owner>> {
    static KNOWN: std::sync::OnceLock<Mutex<HashMap<String, Owner>>> = std::sync::OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// The channels these numbers belong to, by number; one whose owner Twitch would not name is not in the answer.
pub async fn owners(http: &reqwest::Client, who: &super::helix::Caller<'_>, ids: &[String]) -> HashMap<String, Owner> {
    let missing: Vec<String> = {
        let known = known_owners().lock().unwrap();
        ids.iter().filter(|i| !known.contains_key(*i)).cloned().collect()
    };
    if !missing.is_empty() {
        let found = super::helix::users_by_id(http, who, &missing).await;
        let mut known = known_owners().lock().unwrap();
        for (id, login, name, picture) in found {
            known.insert(id, Owner { login, name, picture });
        }
    }
    let known = known_owners().lock().unwrap();
    ids.iter().filter_map(|i| known.get(i).map(|o| (i.clone(), o.clone()))).collect()
}

/// When an account's own emotes in a channel were last fetched, and the channel's number (for fetching them again
/// from anywhere): Twitch lets an emote lapse (a 24-hour unlock, a limited-time one) and gives no word of it, so the
/// list is read again when it is asked for and has gone stale, and the picker shows only what is there.
fn refreshed() -> &'static Mutex<HashMap<String, (std::time::Instant, String)>> {
    static REFRESHED: std::sync::OnceLock<Mutex<HashMap<String, (std::time::Instant, String)>>> = std::sync::OnceLock::new();
    REFRESHED.get_or_init(Default::default)
}

/// Says the account's emotes in a channel were just fetched.
pub fn note_user_emotes(account_id: &str, channel: &str, room_id: &str) {
    refreshed().lock().unwrap().insert(format!("{account_id}|{channel}"), (std::time::Instant::now(), room_id.to_string()));
}

/// The channel's number, and whether the account's emotes there are older than `max_age` (or never fetched).
pub fn user_emotes_state(account_id: &str, channel: &str, max_age: std::time::Duration) -> (Option<String>, bool) {
    match refreshed().lock().unwrap().get(&format!("{account_id}|{channel}")) {
        Some((at, room)) => (Some(room.clone()), at.elapsed() > max_age),
        None => (None, true),
    }
}

/// The words an emote token stands for: `[twitch:emote:25:Kappa]` is sent as `Kappa`.
///
/// The window puts a picture in the message box where an emote was picked, and what that picture is made of is
/// this token (the same one an incoming message carries, so one piece of code draws both). Twitch reads an emote
/// as its word, so that is what leaves.
pub fn tokens_to_names(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[twitch:") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let parsed = after.find(']').and_then(|end| {
            let mut parts = after[1..end].splitn(4, ':');
            let (_, kind, _id, name) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            matches!(kind, "emote" | "aemote").then_some((name, end))
        });
        match parsed {
            Some((name, end)) => {
                out.push_str(name);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("[twitch:");
                rest = &after["[twitch:".len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What the sender's own message is shown as: each word that is one of the sender's emotes becomes its token, so
/// the line the sender sees has the same pictures the others see (Twitch does not send a message back to the one
/// who sent it, so nothing else would draw them). `mine` is the sender's emotes in this channel by word, as the
/// picker holds them (the entry's id is the token); a token already in the text, from the picker, is kept.
pub fn tokens_for_own(text: &str, mine: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        match mine.get(word.as_str()) {
            Some(token) if !word.starts_with("[twitch:") => out.push_str(token),
            _ => out.push_str(word),
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_whitespace() {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Twitch's own, animated or not
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Animated {
    known: HashMap<String, bool>,
    asking: HashSet<String>,
}

fn animated_cache() -> &'static Mutex<Animated> {
    static CACHE: std::sync::OnceLock<Mutex<Animated>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Whether one of Twitch's own emotes is animated, if that is known yet.
///
/// Twitch's picture address has an animated form for those that are and answers
/// 404 for those that are not, and the message carries no word on which, so the
/// first time an emote is seen it is asked about, in the background, and drawn
/// still until the answer is in; every message after that draws it moving.
pub fn animated(id: &str) -> Option<bool> {
    let mut cache = animated_cache().lock().unwrap();
    if let Some(known) = cache.known.get(id) {
        return Some(*known);
    }
    if cache.asking.insert(id.to_string()) {
        // Only where there is something to ask with: a unit test calling this has no runtime.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let id = id.to_string();
            runtime.spawn(async move {
                let yes = match super::client() {
                    Ok(http) => http.head(animated_url(&id)).send().await.map(|r| r.status().is_success()).unwrap_or(false),
                    Err(_) => false,
                };
                let mut cache = animated_cache().lock().unwrap();
                cache.asking.remove(&id);
                // Bounded: a long session sees a great many emotes.
                if cache.known.len() > 20_000 {
                    cache.known.clear();
                }
                cache.known.insert(id, yes);
            });
        } else {
            cache.asking.remove(id);
        }
    }
    None
}

/// Twitch's address for the moving form of one of its own emotes.
pub fn animated_url(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/animated/dark/2.0")
}

/// For a test: says how an emote is, as if it had been asked.
#[cfg(test)]
pub fn say_animated(id: &str, yes: bool) {
    animated_cache().lock().unwrap().known.insert(id.to_string(), yes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_senders_own_line_shows_each_emote_as_a_picture_whether_it_was_picked_or_typed() {
        let mine: HashMap<String, String> = [("Kappa".to_string(), "[twitch:emote:25:Kappa]".to_string())].into_iter().collect();
        assert_eq!(tokens_for_own("hi Kappa  [twitch:emote:9:Pog] plain", &mine), "hi [twitch:emote:25:Kappa]  [twitch:emote:9:Pog] plain");
    }

    #[test]
    fn a_picked_emote_leaves_as_its_word_and_nothing_else_is_touched() {
        assert_eq!(tokens_to_names("hi [twitch:emote:25:Kappa] and [twitch:emote:9:Pog]!"), "hi Kappa and Pog!");
        assert_eq!(tokens_to_names("[twitch:aemote:emotesv2_ab12:Wow]"), "Wow");
        assert_eq!(tokens_to_names("[twitch:cheer:cheer:100] stays, [twitch:bad"), "[twitch:cheer:cheer:100] stays, [twitch:bad");
        assert_eq!(tokens_to_names("plain [text] here"), "plain [text] here");
    }

    #[test]
    fn twitchs_own_animated_form_is_a_different_address_and_an_unasked_emote_is_unknown() {
        assert_eq!(animated_url("301445381"), "https://static-cdn.jtvnw.net/emoticons/v2/301445381/animated/dark/2.0");
        assert_eq!(animated("never-asked-for"), None);
    }
}
