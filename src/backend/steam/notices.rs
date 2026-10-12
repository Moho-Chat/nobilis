//! Steam's warning about a conversation.
//!
//! Steam can mark a conversation as suspicious - the person may not be who they
//! appear to be, a common shape for a scam - and tells the account as a notice.
//! It is said once in the conversation, in words, where the person will see it
//! before they trust what follows. The notice is the same fact each time Steam
//! raises it, so it is stored under an id of its own and said no more than once.

use super::proto::{DismissSessionNotice, SessionNoticeNotification};
use super::steamid;
use crate::state::AppState;

/// `EChatSessionNotice`.
pub const SUSPICIOUS: i32 = 1;

/// What to tell somebody about a notice, or nothing for one moho does not know.
pub fn words(notice_type: i32) -> Option<&'static str> {
    match notice_type {
        SUSPICIOUS => Some("Steam warns that this person may not be who they appear to be. Be careful with links, offers and requests for anything - and do not give out your account details."),
        _ => None,
    }
}

/// Says the notice in the conversation, once. `friend` is who it is about.
pub fn say(state: &AppState, account_id: &str, conversation: &str, friend: u32, notice_type: i32) {
    let Some(text) = words(notice_type) else { return };
    state.runtime.record_message_at(
        state,
        account_id,
        conversation,
        "dm",
        "Steam",
        text,
        false,
        "system",
        None,
        Some(format!("notice:{friend}:{notice_type}")),
        false,
        None,
        Vec::new(),
        Vec::new(),
        None,
        None,
        None,
        None,
    );
}

/// A notice that arrived live, for a conversation that exists.
pub fn heard(state: &AppState, account_id: &str, conversation: &str, notice: &SessionNoticeNotification) {
    let (Some(friend), Some(kind)) = (notice.steamid_friend.and_then(steamid::account_of), notice.notice_type) else { return };
    // Raised, not lifted: there is nothing to say when Steam stops saying it.
    if notice.active.unwrap_or(true) {
        say(state, account_id, conversation, friend, kind);
    }
}

/// Tells Steam a notice has been seen, so it is not raised again.
pub fn dismiss(link: &super::link::Link, friend: u64, notice_type: i32) -> Result<(), String> {
    link.notify("FriendMessages.DismissSessionNotice#1", &DismissSessionNotice { steamid_friend: Some(friend), notice_type: Some(notice_type) }).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;

    const ACCOUNT: &str = "steam:tester";

    fn said(state: &AppState) -> Vec<(String, String, bool)> {
        state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "Gabe"), 0, 20).unwrap().into_iter().map(|m| (m.kind, m.body, m.is_own)).collect()
    }

    #[test]
    fn a_suspicious_notice_is_a_line_in_the_conversation_said_once() {
        let state = simulated_daemon("steam-notice-once");
        let notice = SessionNoticeNotification { steamid_friend: Some(steamid::steamid_of(100)), notice_type: Some(SUSPICIOUS), active: Some(true) };
        heard(&state, ACCOUNT, "Gabe", &notice);
        heard(&state, ACCOUNT, "Gabe", &notice);
        let got = said(&state);
        assert_eq!(got.len(), 1, "raised twice, said once: {got:?}");
        assert_eq!(got[0].0, "system");
        assert!(got[0].1.contains("may not be who they appear to be"));
    }

    #[test]
    fn a_notice_that_is_lifted_or_not_known_says_nothing() {
        let state = simulated_daemon("steam-notice-quiet");
        heard(&state, ACCOUNT, "Gabe", &SessionNoticeNotification { steamid_friend: Some(steamid::steamid_of(100)), notice_type: Some(SUSPICIOUS), active: Some(false) });
        heard(&state, ACCOUNT, "Gabe", &SessionNoticeNotification { steamid_friend: Some(steamid::steamid_of(100)), notice_type: Some(99), active: Some(true) });
        heard(&state, ACCOUNT, "Gabe", &SessionNoticeNotification { steamid_friend: None, notice_type: Some(SUSPICIOUS), active: Some(true) });
        assert!(said(&state).is_empty());
    }

    #[tokio::test]
    async fn dismissing_tells_steam_which_notice_about_whom() {
        use crate::backend::steam::link::{self, fake::*};
        use prost::Message;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = serve({
            let seen = seen.clone();
            Box::new(move |p| {
                seen.lock().unwrap().push(p);
                Some(vec![])
            })
        })
        .await;
        let (link, _told) = link::open_with("test", &url, std::time::Duration::from_secs(5)).await.unwrap();
        dismiss(&link, steamid::steamid_of(100), SUSPICIOUS).unwrap();
        for _ in 0..100 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].header.target_job_name.as_deref(), Some("FriendMessages.DismissSessionNotice#1"));
        let body = DismissSessionNotice::decode(seen[0].body.as_slice()).unwrap();
        assert_eq!((body.steamid_friend, body.notice_type), (Some(steamid::steamid_of(100)), Some(1)));
    }
}
