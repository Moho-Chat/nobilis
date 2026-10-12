//! The emoticons an account has, for the picker and for `:name:` completion.
//!
//! Steam names an emoticon by a word between colons, and draws it from a
//! picture at an address built from the name. Which ones an account may send
//! is what Steam says it has - the free ones every account gets, and what has
//! been earned or bought - so it is asked for rather than listed here.
//!
//! In a message an emoticon is only its name; in the window it is a picture
//! whose address is built from that name, so the same name works for an
//! emoticon the account does not have when somebody else sent it.

use super::link::Link;
use super::proto::{GetEmoticonListRequest, GetEmoticonListResponse};
use crate::model::{EmojiEntry, EmojiSource};
use crate::state::AppState;

/// The picture for an emoticon, which is also what the window draws.
pub fn picture(name: &str) -> String {
    format!("https://community.fastly.steamstatic.com/economy/emoticon/{name}")
}

/// What the picker offers, most used first and then by name.
fn entries(list: &GetEmoticonListResponse) -> Vec<EmojiEntry> {
    let mut have: Vec<(&str, u32)> = list
        .emoticons
        .iter()
        .filter_map(|e| Some((e.name.as_deref().map(|n| n.trim_matches(':'))?, e.use_count.unwrap_or(0))))
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .collect();
    have.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    have.dedup_by(|a, b| a.0 == b.0);
    have.into_iter()
        // The id says which service's picture it is, so the window builds the
        // right token for it; the name is what is typed between colons.
        .map(|(name, _)| EmojiEntry { id: format!("steam:{name}"), name: name.to_string(), url: Some(picture(name)), animated: false, locked: false })
        .collect()
}

/// Asks Steam for the account's emoticons and offers them. Quietly nothing on
/// failure: no picker section is better than an error at sign-in.
pub async fn load(state: AppState, link: Link, account_id: String) {
    let list: GetEmoticonListResponse = match link.call("Player.GetEmoticonList#1", &GetEmoticonListRequest {}, true).await {
        Ok(l) => l,
        Err(e) => {
            tracing::info!("steam[{account_id}]: could not ask for emoticons: {e}");
            return;
        }
    };
    let emoji = entries(&list);
    tracing::debug!("steam[{account_id}]: {} emoticon(s)", emoji.len());
    state.runtime.set_emoji_source(
        &account_id,
        EmojiSource { id: "steam".to_string(), name: "Steam".to_string(), service: "steam".to_string(), kind: "text".to_string(), icon_url: None, buffers: Vec::new(), sendable_anywhere: true, emoji },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::steam::proto::EmoticonInfo;

    fn have(name: &str, used: u32) -> EmoticonInfo {
        EmoticonInfo { name: Some(name.into()), use_count: Some(used), ..Default::default() }
    }

    #[test]
    fn the_picker_gets_the_most_used_first_then_by_name_and_each_once() {
        let list = GetEmoticonListResponse { emoticons: vec![have("zed", 0), have("Alpha", 0), have("steamhappy", 9), have("steamhappy", 9), have(":colon:", 3), have("bad name", 100), have("", 5)] };
        let got = entries(&list);
        assert_eq!(got.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["steamhappy", "colon", "Alpha", "zed"], "a name that is not a name is not offered");
        assert_eq!(got[0].id, "steam:steamhappy");
        assert_eq!(got[0].url.as_deref(), Some("https://community.fastly.steamstatic.com/economy/emoticon/steamhappy"));
    }

    #[tokio::test]
    async fn they_are_asked_for_and_offered_to_the_account() {
        use crate::backend::steam::link::{self, fake::*};
        use prost::Message;
        let state = crate::backend::kick::testkit::simulated_daemon("steam-emoticons");
        let url = serve(Box::new(|p| {
            assert_eq!(p.header.target_job_name.as_deref(), Some("Player.GetEmoticonList#1"));
            Some(vec![answer(&p, 1, GetEmoticonListResponse { emoticons: vec![have("steamsad", 1)] }.encode_to_vec())])
        }))
        .await;
        let (link, _told) = link::open_with("test", &url, std::time::Duration::from_secs(5)).await.unwrap();
        load(state.clone(), link, "steam:me".to_string()).await;
        let sources = state.runtime.emoji_catalogue("steam:me");
        assert_eq!(sources.len(), 1);
        assert_eq!((sources[0].service.as_str(), sources[0].kind.as_str(), sources[0].emoji.len()), ("steam", "text", 1));
    }
}
