//! 7TV emotes, which is most of what a Kick channel actually says.
//!
//! ## Why only 7TV
//!
//! The issue this answers named three providers. Only one of them serves Kick:
//! `api.betterttv.net/3/cached/users/kick/<id>` answers 404 where the same
//! path with `twitch` answers 200, and FrankerFaceZ has no Kick route at all.
//! Both are Twitch products that never followed. So this is 7TV, and naming
//! the other two would be promising something that cannot be delivered.
//!
//! ## The id that matters
//!
//! 7TV keys its sets by Kick's **user** id, not the channel id, and those are
//! different numbers: xqc is user 676 and channel 668, and asking with 668
//! answers 404. moho stored only the channel id, which is why `user_id` had to
//! be captured before any of this could work - a lookup with the wrong number
//! would have failed silently and looked like a channel with no emotes.
//!
//! ## Why these are not Kick emotes
//!
//! A Kick emote arrives in the message as `[emote:1082364:xqcAM]`, carrying
//! its own id, so it resolves anywhere. A 7TV emote arrives as the bare word
//! `xqcL` and means nothing without the channel's set - it is a word that
//! happens to be a picture in one room and ordinary text everywhere else.
//! That is why these are kept per channel and substituted by name.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

/// Everybody's emotes, wherever they are. Fetched once and shared: the global
/// set is the same for every channel, and a request per channel for identical
/// content is a request wasted.
const GLOBAL_SET: &str = "https://7tv.io/v3/emote-sets/global";

/// Which size to ask for. 2x is what other clients draw inline - 1x is
/// visibly soft on a modern display, and 4x is four times the bytes for
/// something rendered twenty pixels tall.
const SIZE: &str = "2x.webp";

/// How many emotes to take from one set.
///
/// A cap rather than a limit anybody should hit: xqc's channel has 965, which
/// is fine, but the field is attacker-controlled in the sense that a channel
/// owner chooses it, and an unbounded map is sent to every window.
const MAX_EMOTES: usize = 2000;

/// Reads a 7TV emote list into name -> picture.
///
/// The host URL arrives protocol-relative (`//cdn.7tv.app/...`), which is a
/// convention from when pages could be http; it is made absolute here so the
/// window never has to know that.
pub fn read_set(emotes: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for emote in emotes.as_array().into_iter().flatten().take(MAX_EMOTES) {
        let Some(name) = emote["name"].as_str().filter(|n| !n.is_empty()) else { continue };
        let Some(host) = emote["data"]["host"]["url"].as_str().filter(|u| !u.is_empty()) else { continue };
        let host = host.strip_prefix("//").map(|h| format!("https://{h}")).unwrap_or_else(|| host.to_string());
        out.insert(name.to_string(), format!("{host}/{SIZE}"));
    }
    out
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let resp = http.get(url).send().await.context("asking 7TV")?;
    if !resp.status().is_success() {
        anyhow::bail!("7TV answered {}", resp.status());
    }
    resp.json().await.context("unreadable answer from 7TV")
}

/// The emotes everybody has.
pub async fn global(http: &reqwest::Client) -> BTreeMap<String, String> {
    match fetch(http, GLOBAL_SET).await {
        Ok(v) => read_set(&v["emotes"]),
        Err(e) => {
            tracing::debug!("kick: 7TV global emotes: {e}");
            BTreeMap::new()
        }
    }
}

/// One channel's own, on top of the global ones.
///
/// A channel with no 7TV link answers 404, which is the ordinary case rather
/// than a failure - most channels have none, and a warning per channel per
/// connect would be noise for a thing that is working correctly.
pub async fn for_channel(http: &reqwest::Client, user_id: u64) -> BTreeMap<String, String> {
    match fetch(http, &format!("https://7tv.io/v3/users/kick/{user_id}")).await {
        Ok(v) => read_set(&v["emote_set"]["emotes"]),
        Err(e) => {
            tracing::debug!("kick: 7TV emotes for user {user_id}: {e}");
            BTreeMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set() -> serde_json::Value {
        json!([
            { "name": "GAMBA", "data": { "host": { "url": "//cdn.7tv.app/emote/01G3W" } } },
            { "name": "xqcL", "data": { "host": { "url": "//cdn.7tv.app/emote/01ABC" } } }
        ])
    }

    /// The shape 7TV actually answers with, read off a live response.
    #[test]
    fn reads_a_set_into_words_and_pictures() {
        let got = read_set(&set());
        assert_eq!(got.get("GAMBA").unwrap(), "https://cdn.7tv.app/emote/01G3W/2x.webp");
        assert_eq!(got.len(), 2);
    }

    /// Protocol-relative is how 7TV writes it, and the window should never
    /// have to know that.
    #[test]
    fn a_protocol_relative_host_is_made_absolute() {
        assert!(read_set(&set()).values().all(|u| u.starts_with("https://")));
        // One that is already absolute is left alone.
        let absolute = json!([{ "name": "a", "data": { "host": { "url": "https://example.net/e" } } }]);
        assert_eq!(read_set(&absolute).get("a").unwrap(), "https://example.net/e/2x.webp");
    }

    /// An entry missing either half is not half an emote.
    #[test]
    fn an_unusable_entry_is_skipped_rather_than_guessed_at() {
        let odd = json!([
            { "name": "", "data": { "host": { "url": "//cdn/x" } } },
            { "name": "noHost" },
            { "name": "ok", "data": { "host": { "url": "//cdn/y" } } }
        ]);
        let got = read_set(&odd);
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("ok"));
    }

    #[test]
    fn nothing_at_all_is_no_emotes_rather_than_a_panic() {
        assert!(read_set(&json!(null)).is_empty());
        assert!(read_set(&json!([])).is_empty());
    }
}
