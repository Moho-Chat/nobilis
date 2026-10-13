//! Steam's warning about a conversation.
//!
//! Steam can mark a conversation as suspicious - the person may not be who they
//! appear to be, a common shape for a scam - and tells the account as a notice.
//! It is said twice, so that it is not missed: once in the conversation, in
//! words, as a warning line that stays in its place in the log, and again as a
//! banner over the conversation that stays until the person dismisses it.
//! Dismissing tells Steam, which does not raise it again; if Steam does raise a
//! notice again - a new one - the banner comes back. The line is the same fact
//! each time Steam raises it, so it is stored under an id of its own and said
//! no more than once.

use super::proto::{DismissSessionNotice, SessionNoticeNotification};
use super::steamid;
use crate::model::BufferNotice;
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
    let Some(text) = words(notice_type) else {
        return;
    };
    state.runtime.record_message_at(
        state,
        account_id,
        conversation,
        "dm",
        "Steam",
        text,
        false,
        // A kind of its own, which the window draws in red so it is not scrolled past.
        "warning",
        None,
        Some(notice_id(friend, notice_type)),
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

/// The id a notice is kept under: the same in the log and in what says whether
/// it has been dismissed.
pub fn notice_id(friend: u32, notice_type: i32) -> String {
    format!("notice:{friend}:{notice_type}")
}

/// Puts back the banners that were up when moho last ran and have not been
/// dismissed. Steam does not say a notice again at every sign-in, so what was
/// not seen would otherwise be gone with the restart. A warning that was said in
/// the log before anything recorded it is one nobody dismissed, and is shown.
pub fn restore(state: &AppState, account_id: &str) {
    show_pending(state, account_id, None);
}

/// The same for one conversation, for the moment it is made or renamed: a
/// conversation known by its number at the start is renamed when Steam says
/// what it is called, and the new one has nothing on it until this is said again.
pub fn restore_buffer(state: &AppState, account_id: &str, buffer_id: &str) {
    show_pending(state, account_id, Some(buffer_id));
}

fn show_pending(state: &AppState, account_id: &str, only: Option<&str>) {
    let Ok(held) = state.store.account_notices(account_id) else {
        return;
    };
    for (buffer, id, recorded) in held {
        if only.is_some_and(|only| only != buffer) {
            continue;
        }
        let kind = id
            .rsplit(':')
            .next()
            .and_then(|t| t.parse::<i32>().ok())
            .unwrap_or(SUSPICIOUS);
        let Some(text) = words(kind) else { continue };
        match recorded.as_deref() {
            Some("dismissed") => {}
            other => {
                if other.is_none() {
                    let _ = state.store.set_notice_state(&buffer, &id, "pending");
                }
                state.runtime.set_buffer_notice(
                    state,
                    &buffer,
                    Some(BufferNotice {
                        kind: "suspicious".to_string(),
                        text: text.to_string(),
                    }),
                );
            }
        }
    }
}

/// Dismissed: the banner is taken off, and it stays off across a restart until
/// Steam raises a notice again.
pub fn dismissed(state: &AppState, buffer_id: &str, friend: u32, notice_type: i32) {
    let _ = state
        .store
        .set_notice_state(buffer_id, &notice_id(friend, notice_type), "dismissed");
    state.runtime.set_buffer_notice(state, buffer_id, None);
}

/// A notice that arrived live, for a conversation that exists.
pub fn heard(
    state: &AppState,
    account_id: &str,
    conversation: &str,
    notice: &SessionNoticeNotification,
) {
    let (Some(friend), Some(kind)) = (
        notice.steamid_friend.and_then(steamid::account_of),
        notice.notice_type,
    ) else {
        return;
    };
    let buffer = crate::model::buffer_id(account_id, conversation);
    // Raised: said in the log, and held over the conversation until dismissed.
    // Lifted: the banner goes, and the line stays where it was said.
    if notice.active.unwrap_or(true) {
        say(state, account_id, conversation, friend, kind);
        if let Some(text) = words(kind) {
            // Raised afresh, so not dismissed any more, whatever came before.
            let _ = state
                .store
                .set_notice_state(&buffer, &notice_id(friend, kind), "pending");
            state.runtime.set_buffer_notice(
                state,
                &buffer,
                Some(BufferNotice {
                    kind: "suspicious".to_string(),
                    text: text.to_string(),
                }),
            );
        }
    } else if words(kind).is_some() {
        dismissed(state, &buffer, friend, kind);
    }
}

/// Tells Steam a notice has been seen, so it is not raised again.
pub fn dismiss(link: &super::link::Link, friend: u64, notice_type: i32) -> Result<(), String> {
    link.notify(
        "FriendMessages.DismissSessionNotice#1",
        &DismissSessionNotice {
            steamid_friend: Some(friend),
            notice_type: Some(notice_type),
        },
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;

    const ACCOUNT: &str = "steam:tester";

    fn said(state: &AppState) -> Vec<(String, String, bool)> {
        state
            .store
            .get_backlog(&crate::model::buffer_id(ACCOUNT, "Gabe"), 0, 20)
            .unwrap()
            .into_iter()
            .map(|m| (m.kind, m.body, m.is_own))
            .collect()
    }

    #[test]
    fn a_suspicious_notice_is_a_line_in_the_conversation_said_once() {
        let state = simulated_daemon("steam-notice-once");
        let notice = SessionNoticeNotification {
            steamid_friend: Some(steamid::steamid_of(100)),
            notice_type: Some(SUSPICIOUS),
            active: Some(true),
        };
        heard(&state, ACCOUNT, "Gabe", &notice);
        heard(&state, ACCOUNT, "Gabe", &notice);
        let got = said(&state);
        assert_eq!(got.len(), 1, "raised twice, said once: {got:?}");
        assert_eq!(got[0].0, "warning");
        let held = state
            .runtime
            .get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe"))
            .unwrap()
            .notice;
        assert_eq!(
            held.as_ref().map(|n| n.kind.as_str()),
            Some("suspicious"),
            "and held over the conversation"
        );
        assert!(got[0].1.contains("may not be who they appear to be"));
    }

    #[test]
    fn the_banner_is_taken_off_when_dismissed_and_comes_back_when_steam_says_it_again() {
        let state = simulated_daemon("steam-notice-banner");
        let raised = SessionNoticeNotification {
            steamid_friend: Some(steamid::steamid_of(100)),
            notice_type: Some(SUSPICIOUS),
            active: Some(true),
        };
        heard(&state, ACCOUNT, "Gabe", &raised);
        let id = crate::model::buffer_id(ACCOUNT, "Gabe");
        assert!(state.runtime.get_buffer(&id).unwrap().notice.is_some());
        // Dismissed: the banner goes, and the line in the log stays.
        dismissed(&state, &id, 100, SUSPICIOUS);
        assert!(state.runtime.get_buffer(&id).unwrap().notice.is_none());
        assert_eq!(said(&state).len(), 1);
        // Steam raising it again is a new notice.
        heard(&state, ACCOUNT, "Gabe", &raised);
        assert!(
            state.runtime.get_buffer(&id).unwrap().notice.is_some(),
            "a new notice is shown"
        );
        assert_eq!(said(&state).len(), 1, "though the log is not told twice");
        // Steam lifting it takes the banner off.
        heard(
            &state,
            ACCOUNT,
            "Gabe",
            &SessionNoticeNotification {
                active: Some(false),
                ..raised
            },
        );
        assert!(state.runtime.get_buffer(&id).unwrap().notice.is_none());
    }

    #[test]
    fn a_banner_not_dismissed_is_there_after_a_restart_and_one_dismissed_is_not() {
        let state = simulated_daemon("steam-notice-restart");
        let id = crate::model::buffer_id(ACCOUNT, "Gabe");
        heard(
            &state,
            ACCOUNT,
            "Gabe",
            &SessionNoticeNotification {
                steamid_friend: Some(steamid::steamid_of(100)),
                notice_type: Some(SUSPICIOUS),
                active: Some(true),
            },
        );
        // The window and the daemon are restarted: nothing is held but what was kept.
        state.runtime.set_buffer_notice(&state, &id, None);
        restore(&state, ACCOUNT);
        assert!(
            state.runtime.get_buffer(&id).unwrap().notice.is_some(),
            "not dismissed, so still there"
        );
        dismissed(&state, &id, 100, SUSPICIOUS);
        restore(&state, ACCOUNT);
        assert!(
            state.runtime.get_buffer(&id).unwrap().notice.is_none(),
            "dismissed, so not brought back"
        );
    }

    #[test]
    fn a_warning_said_in_the_log_before_anything_recorded_it_is_shown_once() {
        let state = simulated_daemon("steam-notice-legacy");
        let id = crate::model::buffer_id(ACCOUNT, "Gabe");
        // As it was stored before there was a banner: a system line, nothing more.
        state.runtime.record_message_at(
            &state,
            ACCOUNT,
            "Gabe",
            "dm",
            "Steam",
            "Steam warns that this person may not be who they appear to be.",
            false,
            "system",
            None,
            Some(notice_id(100, SUSPICIOUS)),
            false,
            None,
            vec![],
            vec![],
            None,
            None,
            None,
            None,
        );
        assert!(state.runtime.get_buffer(&id).unwrap().notice.is_none());
        restore(&state, ACCOUNT);
        assert!(
            state.runtime.get_buffer(&id).unwrap().notice.is_some(),
            "nobody dismissed it, so it is up"
        );
        dismissed(&state, &id, 100, SUSPICIOUS);
        restore(&state, ACCOUNT);
        assert!(state.runtime.get_buffer(&id).unwrap().notice.is_none());
    }

    #[test]
    fn a_notice_that_is_lifted_or_not_known_says_nothing() {
        let state = simulated_daemon("steam-notice-quiet");
        heard(
            &state,
            ACCOUNT,
            "Gabe",
            &SessionNoticeNotification {
                steamid_friend: Some(steamid::steamid_of(100)),
                notice_type: Some(SUSPICIOUS),
                active: Some(false),
            },
        );
        heard(
            &state,
            ACCOUNT,
            "Gabe",
            &SessionNoticeNotification {
                steamid_friend: Some(steamid::steamid_of(100)),
                notice_type: Some(99),
                active: Some(true),
            },
        );
        heard(
            &state,
            ACCOUNT,
            "Gabe",
            &SessionNoticeNotification {
                steamid_friend: None,
                notice_type: Some(SUSPICIOUS),
                active: Some(true),
            },
        );
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
        let (link, _told) = link::open_with("test", &url, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        dismiss(&link, steamid::steamid_of(100), SUSPICIOUS).unwrap();
        for _ in 0..100 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].header.target_job_name.as_deref(),
            Some("FriendMessages.DismissSessionNotice#1")
        );
        let body = DismissSessionNotice::decode(seen[0].body.as_slice()).unwrap();
        assert_eq!(
            (body.steamid_friend, body.notice_type),
            (Some(steamid::steamid_of(100)), Some(1))
        );
    }
}
