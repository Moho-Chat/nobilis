//! A friend's card: what they wrote on their profile, their level, and what
//! Steam knows of how they are showing.
//!
//! The facts that arrive with their presence - the kind of client, the last
//! time they were seen, what their game says - are already held in the
//! conversation's one-person roster, so the card reads them from there. What
//! needs asking is the profile text and the level.

use super::link::Link;
use super::proto::{FriendProfileInfoRequest, FriendProfileInfoResponse, FriendsSteamLevelsRequest, FriendsSteamLevelsResponse};
use super::steamid;
use super::wire::emsg;
use crate::profile;
use crate::state::AppState;
use serde_json::{json, Value};

/// What somebody wrote about where they are, as one line.
fn place(p: &FriendProfileInfoResponse) -> String {
    [&p.city_name, &p.state_name, &p.country_name].iter().filter_map(|s| s.as_deref()).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ")
}

/// A profile's summary, which is written with Steam's tags, as plain words.
fn summary(text: &str) -> String {
    let mut plain = super::bbcode::to_markup(text).replace('\u{200b}', "");
    // The card shows words, not the marks the log draws bold and struck with.
    for mark in ["**", "~~", "||", "[u]", "[/u]"] {
        plain = plain.replace(mark, "");
    }
    let plain = pictures_as_words(&plain);
    plain.chars().take(600).collect::<String>().trim().to_string()
}

/// `[steam:emote:name]` as `:name:`, which is how it is read in words.
fn pictures_as_words(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("[steam:") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match tail.find(']').and_then(|end| tail[7..end].split_once(':').map(|(_, name)| (end, name))) {
            Some((end, name)) => {
                out.push_str(&format!(":{name}:"));
                rest = &tail[end + 1..];
            }
            None => {
                out.push_str("[steam:");
                rest = &tail[7..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Adds what the two answers say.
fn fill(card: &mut Value, info: Option<&FriendProfileInfoResponse>, level: Option<u32>) {
    if let Some(level) = level.filter(|l| *l > 0) {
        profile::note(card, "Steam level", level.to_string());
    }
    let Some(info) = info else { return };
    profile::set(card, "createdTs", info.time_created.filter(|t| *t > 0).map(|t| json!(t)));
    profile::note(card, "Real name", info.real_name.clone().unwrap_or_default());
    profile::note(card, "Location", place(info));
    profile::note(card, "Headline", info.headline.clone().unwrap_or_default());
    profile::note(card, "Summary", summary(info.summary.as_deref().unwrap_or_default()));
}

/// Adds what is already known from how they are showing.
fn fill_from_presence(card: &mut Value, member: &Value) {
    profile::set(card, "status", member["status"].as_str().map(|s| json!(s)));
    profile::set(card, "avatarUrl", member["avatarUrl"].as_str().map(|s| json!(s)));
    profile::set(card, "activity", Some(member["activity"].clone()).filter(|a| !a.is_null()));
    profile::note(card, "On", member["client"].as_str().unwrap_or_default());
    profile::note(card, "Doing", member["rich"].as_str().unwrap_or_default());
    // Only for somebody who is not here: for somebody who is, it is now.
    if member["status"].as_str() == Some("offline") {
        profile::set(card, "lastActiveTs", member["lastSeen"].as_u64().filter(|t| *t > 0).map(|t| json!(t)));
    }
}

async fn ask_profile(link: &Link, friend: u64) -> Option<FriendProfileInfoResponse> {
    let answer: FriendProfileInfoResponse = link.ask(emsg::CLIENT_FRIEND_PROFILE_INFO, &FriendProfileInfoRequest { steamid_friend: Some(friend) }).await.ok()?;
    (answer.eresult.unwrap_or(super::eresult::FAIL) == super::eresult::OK).then_some(answer)
}

async fn ask_level(link: &Link, friend: u32) -> Option<u32> {
    let answer: FriendsSteamLevelsResponse = link.ask(emsg::CLIENT_FS_GET_FRIENDS_STEAM_LEVELS, &FriendsSteamLevelsRequest { accountids: vec![friend] }).await.ok()?;
    answer.friends.iter().find(|f| f.accountid == Some(friend)).and_then(|f| f.level)
}

/// The card for the person a conversation is with.
pub async fn build(state: &AppState, account_id: &str, buffer_id: &str, who: &str) -> Value {
    let mut card = profile::pending("steam", account_id, who);
    card["pending"] = json!(false);
    let buffer = state.runtime.get_buffer(buffer_id);
    let friend = buffer.as_ref().and_then(|b| b.remote_id.as_deref()).and_then(|id| id.parse::<u64>().ok()).filter(|id| steamid::account_of(*id).is_some());
    let Some(friend) = friend else { return card };
    card["id"] = json!(friend.to_string());
    if let Some(member) = state.runtime.get_presence(buffer_id).and_then(|m| m.as_array().and_then(|a| a.first().cloned())) {
        fill_from_presence(&mut card, &member);
    }
    if let Some(link) = state.runtime.steam_link(account_id) {
        let number = steamid::account_of(friend).unwrap_or_default();
        let (info, level) = tokio::join!(ask_profile(&link, friend), ask_level(&link, number));
        fill(&mut card, info.as_ref(), level);
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(card: &Value) -> Vec<(String, String)> {
        card["extra"].as_array().map(|a| a.iter().map(|e| (e["label"].as_str().unwrap().to_string(), e["value"].as_str().unwrap().to_string())).collect()).unwrap_or_default()
    }

    #[test]
    fn what_they_wrote_is_on_the_card_in_words() {
        let mut card = profile::pending("steam", "steam:me", "Gabe");
        let info = FriendProfileInfoResponse {
            eresult: Some(1),
            time_created: Some(1300000000),
            real_name: Some("Gabe N".into()),
            city_name: Some("Bellevue".into()),
            state_name: Some("".into()),
            country_name: Some("United States".into()),
            headline: Some("Hello".into()),
            summary: Some("About [b]me[/b] and [emoticon]steamhappy[/emoticon] \\[x]".into()),
            ..Default::default()
        };
        fill(&mut card, Some(&info), Some(42));
        assert_eq!(card["createdTs"], 1300000000);
        assert_eq!(
            labels(&card),
            [("Steam level", "42"), ("Real name", "Gabe N"), ("Location", "Bellevue, United States"), ("Headline", "Hello"), ("Summary", "About me and :steamhappy: [x]")]
                .map(|(a, b)| (a.to_string(), b.to_string())),
            "an empty part of a place is left out, and the summary is words"
        );
    }

    #[test]
    fn nothing_known_is_nothing_shown() {
        let mut card = profile::pending("steam", "steam:me", "Gabe");
        fill(&mut card, None, None);
        fill(&mut card, Some(&FriendProfileInfoResponse::default()), Some(0));
        assert!(labels(&card).is_empty());
        assert!(card.get("createdTs").is_none());
    }

    #[test]
    fn how_they_are_showing_comes_from_their_presence() {
        let mut card = profile::pending("steam", "steam:me", "Gabe");
        fill_from_presence(&mut card, &json!({ "status": "online", "client": "Mobile", "rich": "In a match", "avatarUrl": "https://a/b.jpg", "activity": { "kind": "game", "text": "Dota 2" }, "lastSeen": 100 }));
        assert_eq!(card["status"], "online");
        assert_eq!(card["activity"]["text"], "Dota 2");
        assert!(card.get("lastActiveTs").is_none(), "last seen is for somebody who is away");
        assert_eq!(labels(&card), [("On".to_string(), "Mobile".to_string()), ("Doing".to_string(), "In a match".to_string())]);

        let mut gone = profile::pending("steam", "steam:me", "Gabe");
        fill_from_presence(&mut gone, &json!({ "status": "offline", "lastSeen": 1790000000 }));
        assert_eq!(gone["lastActiveTs"], 1790000000u64);
        assert!(labels(&gone).is_empty(), "nothing to say of a client that is not there");
    }
}
