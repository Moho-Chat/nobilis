//! What an account may put in a message beyond words: its emoticons, stickers
//! and effects, which are a short list Steam holds for each account.
//!
//! Steam names an emoticon by a word between colons, and draws it from a
//! picture at an address built from the name; a sticker likewise, and is sent as
//! a command (`/sticker name`) that Steam turns into the picture. Which an
//! account may use is what Steam says it has - the free ones every account gets,
//! and what has been earned or bought - so it is asked for rather than listed
//! here, and it is all the picker offers: there is no other set to choose from.
//!
//! In a message an emoticon is only its name; in the window it is a picture
//! whose address is built from that name, so the same name works for an
//! emoticon the account does not have when somebody else sent it.

use super::link::Link;
use super::proto::{ClientEmoticonList, ClientGetEmoticonList, GetEmoticonListRequest, GetEmoticonListResponse};
use super::wire::emsg;
use crate::model::{EmojiEntry, EmojiSource};
use crate::state::AppState;

/// The id of the source that holds the stickers. Kept in the same catalogue as the
/// emoticons but never offered as a section of emoji: they are sent, not typed.
pub const STICKERS: &str = "steam-stickers";

/// The picture for an emoticon, which is also what the window draws.
pub fn picture(name: &str) -> String {
    format!("https://community.fastly.steamstatic.com/economy/emoticon/{name}")
}

/// The picture for a sticker.
pub fn sticker_picture(name: &str) -> String {
    format!("https://community.fastly.steamstatic.com/economy/sticker/{name}")
}

/// Whether the account has this sticker: what it is asked to send must be one it
/// holds, since a command naming any other would be sent as the words it is.
pub fn is_owned_sticker(state: &AppState, account_id: &str, name: &str) -> bool {
    state.runtime.emoji_catalogue(account_id).iter().filter(|s| s.id == STICKERS).any(|s| s.emoji.iter().any(|e| e.name == name))
}

fn valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A name list in the order the picker offers it: most used first, then by
/// name, each once, and only what is a name.
fn ordered(mut have: Vec<(String, u32)>) -> Vec<String> {
    have.retain(|(n, _)| valid(n));
    have.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    have.dedup_by(|a, b| a.0 == b.0);
    have.into_iter().map(|(n, _)| n).collect()
}

/// The emoticons the picker offers.
fn emoticon_entries(list: &ClientEmoticonList) -> Vec<EmojiEntry> {
    let have = list.emoticons.iter().filter_map(|e| Some((e.name.as_deref()?.trim_matches(':').to_string(), e.use_count.unwrap_or(0)))).collect();
    // The id says which service's picture it is, so the window builds the
    // right token for it; the name is what is typed between colons.
    ordered(have).into_iter().map(|name| EmojiEntry { id: format!("steam:{name}"), url: Some(picture(&name)), name, animated: false, locked: false }).collect()
}

/// The stickers the picker offers.
fn sticker_entries(list: &ClientEmoticonList) -> Vec<EmojiEntry> {
    let have = list.stickers.iter().filter_map(|e| Some((e.name.as_deref()?.to_string(), e.use_count.unwrap_or(0)))).collect();
    ordered(have).into_iter().map(|name| EmojiEntry { id: name.clone(), url: Some(sticker_picture(&name)), name, animated: false, locked: false }).collect()
}

fn source(id: &str, name: &str, emoji: Vec<EmojiEntry>) -> EmojiSource {
    EmojiSource { id: id.to_string(), name: name.to_string(), service: "steam".to_string(), kind: "text".to_string(), icon_url: None, buffers: Vec::new(), sendable_anywhere: true, emoji }
}

/// Keeps what Steam says the account has, as what the picker offers.
pub fn keep(state: &AppState, account_id: &str, list: &ClientEmoticonList) {
    let (emoticons, stickers) = (emoticon_entries(list), sticker_entries(list));
    tracing::debug!("steam[{account_id}]: {} emoticon(s), {} sticker(s), {} effect(s)", emoticons.len(), stickers.len(), list.effects.len());
    state.runtime.set_emoji_source(account_id, source("steam", "Steam", emoticons));
    state.runtime.set_emoji_source(account_id, source(STICKERS, "Steam stickers", stickers));
}

/// Asks Steam for what the account has. The full list - stickers and effects as
/// well - is asked for and arrives as a message of its own, which the
/// connection hands to `keep`; the emoticons alone are asked for in the
/// ordinary way at the same time, so there is a picker while that is awaited
/// and one if it never comes. Quietly nothing on failure: no picker section is
/// better than an error at sign-in.
pub async fn load(state: AppState, link: Link, account_id: String) {
    let _ = link.send(emsg::CLIENT_GET_EMOTICON_LIST, &ClientGetEmoticonList {});
    match link.call::<_, GetEmoticonListResponse>("Player.GetEmoticonList#1", &GetEmoticonListRequest {}, true).await {
        Ok(l) => {
            // The full list may have come first; what it says is not replaced
            // by this, which knows less.
            if state.runtime.emoji_catalogue(&account_id).iter().all(|s| s.id != STICKERS) {
                let list = ClientEmoticonList {
                    emoticons: l.emoticons.into_iter().map(|e| super::proto::EmoticonOwned { name: e.name, count: e.count, time_last_used: e.time_last_used, use_count: e.use_count, time_received: e.time_received, appid: e.appid }).collect(),
                    ..Default::default()
                };
                keep(&state, &account_id, &list);
            }
        }
        Err(e) => tracing::info!("steam[{account_id}]: could not ask for emoticons: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::steam::proto::{EmoticonOwned, StickerOwned};

    fn have(name: &str, used: u32) -> EmoticonOwned {
        EmoticonOwned { name: Some(name.into()), use_count: Some(used), ..Default::default() }
    }

    fn sticker(name: &str, used: u32) -> StickerOwned {
        StickerOwned { name: Some(name.into()), use_count: Some(used), ..Default::default() }
    }

    #[test]
    fn the_picker_gets_the_most_used_first_then_by_name_and_each_once() {
        let list = ClientEmoticonList { emoticons: vec![have("zed", 0), have("Alpha", 0), have("steamhappy", 9), have("steamhappy", 9), have(":colon:", 3), have("bad name", 100), have("", 5)], ..Default::default() };
        let got = emoticon_entries(&list);
        assert_eq!(got.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["steamhappy", "colon", "Alpha", "zed"], "a name that is not a name is not offered");
        assert_eq!(got[0].id, "steam:steamhappy");
        assert_eq!(got[0].url.as_deref(), Some("https://community.fastly.steamstatic.com/economy/emoticon/steamhappy"));
    }

    #[test]
    fn stickers_are_offered_by_name_with_their_own_pictures() {
        let list = ClientEmoticonList { stickers: vec![sticker("b", 0), sticker("CatSurprise", 3), sticker("bad name", 9)], ..Default::default() };
        let got = sticker_entries(&list);
        assert_eq!(got.iter().map(|e| (e.id.as_str(), e.url.as_deref().unwrap())).collect::<Vec<_>>(), [("CatSurprise", "https://community.fastly.steamstatic.com/economy/sticker/CatSurprise"), ("b", "https://community.fastly.steamstatic.com/economy/sticker/b")]);
    }

    #[test]
    fn only_a_sticker_the_account_has_may_be_sent() {
        let state = crate::backend::kick::testkit::simulated_daemon("steam-sticker-owned");
        keep(&state, "steam:me", &ClientEmoticonList { stickers: vec![sticker("CatSurprise", 1)], ..Default::default() });
        assert!(is_owned_sticker(&state, "steam:me", "CatSurprise"));
        assert!(!is_owned_sticker(&state, "steam:me", "Other"));
        assert!(!is_owned_sticker(&state, "steam:somebody", "CatSurprise"), "another account's is not this one's");
        assert!(!is_owned_sticker(&state, "steam:me", "Cat Surprise; and more"));
    }

    #[test]
    fn the_full_list_is_kept_as_two_sources() {
        let state = crate::backend::kick::testkit::simulated_daemon("steam-emoticons-keep");
        keep(&state, "steam:me", &ClientEmoticonList { emoticons: vec![have("steamsad", 1)], stickers: vec![sticker("CatSurprise", 1), sticker("Wave", 0)], effects: vec![] });
        let sources = state.runtime.emoji_catalogue("steam:me");
        let by = |id: &str| sources.iter().find(|s| s.id == id).map(|s| s.emoji.len());
        assert_eq!((by("steam"), by(STICKERS)), (Some(1), Some(2)));
    }

    #[tokio::test]
    async fn the_emoticons_alone_are_offered_when_the_full_list_never_comes_and_do_not_replace_it_when_it_does() {
        use crate::backend::steam::link::{self, fake::*};
        use prost::Message;
        let state = crate::backend::kick::testkit::simulated_daemon("steam-emoticons-fallback");
        let url = serve(Box::new(|p| {
            if p.emsg == emsg::CLIENT_GET_EMOTICON_LIST {
                // Not answered.
                return Some(vec![]);
            }
            assert_eq!(p.header.target_job_name.as_deref(), Some("Player.GetEmoticonList#1"));
            Some(vec![answer(&p, 1, GetEmoticonListResponse { emoticons: vec![crate::backend::steam::proto::EmoticonInfo { name: Some("steamsad".into()), ..Default::default() }] }.encode_to_vec())])
        }))
        .await;
        let (link, _told) = link::open_with("test", &url, std::time::Duration::from_secs(2)).await.unwrap();
        load(state.clone(), link, "steam:me".to_string()).await;
        let sources = state.runtime.emoji_catalogue("steam:me");
        assert_eq!(sources.iter().find(|s| s.id == "steam").map(|s| s.emoji.len()), Some(1));
        assert_eq!(sources.iter().find(|s| s.id == STICKERS).map(|s| s.emoji.len()), Some(0));

        // The full list, once it has come, is not overwritten by a later fallback.
        keep(&state, "steam:other", &ClientEmoticonList { emoticons: vec![have("a", 0), have("b", 0)], stickers: vec![sticker("S", 0)], effects: vec![] });
        let (link, _told) = link::open_with("test", &serve(Box::new(|p| Some(vec![answer(&p, 1, GetEmoticonListResponse::default().encode_to_vec())]))).await, std::time::Duration::from_secs(2)).await.unwrap();
        load(state.clone(), link, "steam:other".to_string()).await;
        let other = state.runtime.emoji_catalogue("steam:other");
        assert_eq!(other.iter().find(|s| s.id == STICKERS).map(|s| s.emoji.len()), Some(1));
        assert_eq!(other.iter().find(|s| s.id == "steam").map(|s| s.emoji.len()), Some(2));
    }
}
