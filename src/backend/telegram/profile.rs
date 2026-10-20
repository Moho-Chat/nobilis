//! The card for a person: their name, handle, picture, what they wrote about themselves and when they were last seen.

use super::media;
use super::messages;
use super::peers;
use super::send::{explain, peer_for};
use crate::profile;
use crate::state::AppState;
use grammers_client::peer::{Peer, User};
use grammers_client::tl;
use grammers_session::Session;
use serde_json::{json, Value};

/// What they were last seen doing, in the card's words, from Telegram's coarse status.
pub fn presence(status: &tl::enums::UserStatus) -> (Option<&'static str>, Option<i64>) {
    use tl::enums::UserStatus as S;
    match status {
        S::Online(_) => (Some("online"), None),
        S::Offline(o) => (Some("offline"), Some(i64::from(o.was_online))),
        S::Recently(_) => (Some("offline"), None),
        S::LastWeek(_) | S::LastMonth(_) => (Some("offline"), None),
        S::Empty => (None, None),
    }
}

/// How Telegram words its coarse "last seen" when it will not give a time.
fn coarse(status: &tl::enums::UserStatus) -> Option<&'static str> {
    use tl::enums::UserStatus as S;
    match status {
        S::Recently(_) => Some("recently"),
        S::LastWeek(_) => Some("within a week"),
        S::LastMonth(_) => Some("within a month"),
        _ => None,
    }
}

/// The card for the person a conversation is with, or a member of a group the window asked about.
pub async fn build(state: &AppState, account_id: &str, buffer_id: &str, who: &str, user_id: &str) -> Value {
    let mut card = profile::pending("telegram", account_id, who);
    card["pending"] = json!(false);
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return card };
    // A direct message is with one person; in a group the member asked about is named by number.
    let target = if buffer.kind == "dm" { buffer.remote_id.clone() } else { Some(user_id.to_string()) };
    let Some(target) = target.and_then(|t| peers::peer_id_of(&t)) else { return card };
    let Ok((handle, _)) = peer_for(state, &buffer).await else { return card };
    let Some(peer) = handle.session.peer_ref(target).await.ok().flatten() else { return card };
    card["id"] = json!(target.bare_id_unchecked().to_string());
    let full = handle.client.invoke(&tl::functions::users::GetFullUser { id: peer.into() }).await;
    let tl::enums::users::UserFull::Full(full) = match full {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("telegram profile: {}", explain(&e));
            return card;
        }
    };
    let tl::enums::UserFull::Full(info) = &full.full_user;
    if let Some(user) = full.users.iter().find(|u| u.id() == target.bare_id_unchecked()).cloned() {
        let user = User::from_raw(&handle.client, user);
        let as_peer = Peer::User(user.clone());
        card["name"] = json!(messages::name_of(&as_peer));
        if let Some(username) = user.username() {
            card["handle"] = json!(format!("@{username}"));
        }
        profile::set(&mut card, "avatarUrl", media::avatar_url(handle.own.user_id, &as_peer).map(|u| json!(u)));
        let (status, seen) = presence(user.status());
        profile::set(&mut card, "status", status.map(|s| json!(s)));
        profile::set(&mut card, "lastActiveTs", seen.map(|t| json!(t)));
        if let Some(words) = coarse(user.status()) {
            profile::note(&mut card, "Last seen", words);
        }
        if user.is_bot() {
            profile::note(&mut card, "Account", "A bot");
        }
        if user.verified() {
            profile::note(&mut card, "Account", "Verified by Telegram");
        }
        if let Some(phone) = user.phone() {
            profile::note(&mut card, "Phone", format!("+{phone}"));
        }
    }
    if let Some(about) = &info.about {
        profile::note(&mut card, "About", about.clone());
    }
    if let Some(tl::enums::Birthday::Birthday(b)) = &info.birthday {
        profile::note(&mut card, "Birthday", match b.year {
            Some(year) => format!("{}.{}.{year}", b.day, b.month),
            None => format!("{}.{}", b.day, b.month),
        });
    }
    if info.common_chats_count > 0 {
        profile::note(&mut card, "Groups in common", info.common_chats_count.to_string());
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_seen_is_a_time_when_telegram_gives_one_and_a_word_when_it_does_not() {
        let offline = tl::enums::UserStatus::Offline(tl::types::UserStatusOffline { was_online: 1790000000 });
        assert_eq!(presence(&offline), (Some("offline"), Some(1790000000)));
        assert_eq!(presence(&tl::enums::UserStatus::Online(tl::types::UserStatusOnline { expires: 1 })), (Some("online"), None));
        let recently = tl::enums::UserStatus::Recently(tl::types::UserStatusRecently { by_me: false });
        assert_eq!(presence(&recently), (Some("offline"), None));
        assert_eq!(coarse(&recently), Some("recently"));
        assert_eq!(coarse(&offline), None);
        assert_eq!(presence(&tl::enums::UserStatus::Empty), (None, None));
    }
}
