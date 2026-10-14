//! The pictures for the badges beside a name: Twitch's global ones and each
//! channel's own (the subscriber badge is a different picture in every
//! channel, and changes with the months).
//!
//! A message's `badges` tag names each as `set/version`; what it looks like is
//! in a table Twitch publishes, asked for once for the global set and once per
//! channel when it is joined. Through Twitch's GraphQL (see `gql.rs`), because
//! the official API will answer neither without an account.

use super::gql;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One badge as Twitch describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub title: String,
    pub image: String,
}

#[derive(Default)]
struct Held {
    global: Option<Arc<HashMap<String, Picture>>>,
    channels: HashMap<String, Arc<HashMap<String, Picture>>>,
}

fn held() -> &'static Mutex<Held> {
    static HELD: std::sync::OnceLock<Mutex<Held>> = std::sync::OnceLock::new();
    HELD.get_or_init(Default::default)
}

const GLOBAL: &str = "query{ badges { setID version title imageURL(size:DOUBLE) } }";
const CHANNEL: &str = "query($login:String!){ user(login:$login){ broadcastBadges { setID version title imageURL(size:DOUBLE) } } }";

/// Reads a list of badges, whichever query it answered.
pub fn read(list: &Value) -> HashMap<String, Picture> {
    let mut out = HashMap::new();
    for b in list.as_array().into_iter().flatten() {
        let (Some(set), Some(version), Some(image)) = (b["setID"].as_str(), b["version"].as_str(), b["imageURL"].as_str()) else { continue };
        if !image.starts_with("https://") {
            continue;
        }
        out.insert(format!("{set}/{version}"), Picture { title: b["title"].as_str().unwrap_or(set).to_string(), image: image.to_string() });
    }
    out
}

/// The picture for a badge in a channel: the channel's own, else the global one.
pub fn find(channel: &str, set_and_version: &str) -> Option<Picture> {
    let held = held().lock().unwrap();
    held.channels.get(channel).and_then(|c| c.get(set_and_version)).or_else(|| held.global.as_ref().and_then(|g| g.get(set_and_version))).cloned()
}

pub fn have_global() -> bool {
    held().lock().unwrap().global.is_some()
}

pub fn forget(channel: &str) {
    held().lock().unwrap().channels.remove(channel);
}

pub async fn load_global(http: &reqwest::Client, root: &str) {
    if have_global() {
        return;
    }
    let Ok(answer) = gql::call(http, root, &gql::query(GLOBAL, json!({})), &gql::Who::default()).await else { return };
    let table = read(&answer["data"]["badges"]);
    if !table.is_empty() {
        held().lock().unwrap().global = Some(Arc::new(table));
    }
}

pub async fn load_channel(http: &reqwest::Client, root: &str, login: &str) {
    let Ok(answer) = gql::call(http, root, &gql::query(CHANNEL, json!({ "login": login })), &gql::Who::default()).await else { return };
    let table = read(&answer["data"]["user"]["broadcastBadges"]);
    held().lock().unwrap().channels.insert(login.to_string(), Arc::new(table));
}

#[cfg(test)]
pub fn keep_for_test(channel: Option<&str>, table: HashMap<String, Picture>) {
    let mut held = held().lock().unwrap();
    match channel {
        Some(c) => {
            held.channels.insert(c.to_string(), Arc::new(table));
        }
        None => held.global = Some(Arc::new(table)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_badge_list_is_read_by_set_and_version_and_an_address_that_is_not_https_is_refused() {
        let table = read(&json!([
            { "setID": "subscriber", "version": "36", "title": "3-Year Subscriber", "imageURL": "https://static-cdn.jtvnw.net/badges/v1/a/2" },
            { "setID": "moderator", "version": "1", "title": "Moderator", "imageURL": "https://static-cdn.jtvnw.net/badges/v1/b/2" },
            { "setID": "bad", "version": "1", "title": "X", "imageURL": "file:///etc/passwd" },
            { "setID": "no-picture", "version": "1" }
        ]));
        assert_eq!(table.len(), 2);
        assert_eq!(table["subscriber/36"].title, "3-Year Subscriber");
    }

    #[test]
    fn a_channels_own_picture_comes_before_the_global_one() {
        let pic = |t: &str| Picture { title: t.into(), image: format!("https://x/{t}") };
        keep_for_test(None, [("subscriber/0".to_string(), pic("global")), ("vip/1".to_string(), pic("vip"))].into_iter().collect());
        keep_for_test(Some("badgechan"), [("subscriber/0".to_string(), pic("channel"))].into_iter().collect());
        assert_eq!(find("badgechan", "subscriber/0").unwrap().title, "channel");
        assert_eq!(find("badgechan", "vip/1").unwrap().title, "vip");
        assert_eq!(find("other", "subscriber/0").unwrap().title, "global");
        assert!(find("badgechan", "no/such").is_none());
        forget("badgechan");
    }
}
