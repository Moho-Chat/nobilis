//! What an account keeps about each friend: the name it calls them, and whether
//! their messages notify it.
//!
//! Steam holds these for the account, so they follow it to every client: a
//! nickname set here is the name on the phone, and one set there is the name
//! here. They are read when the account signs in, and changes - made anywhere -
//! arrive as notifications. Setting one asks for what the account has now,
//! changes the one thing, and sends it back whole, because the message that
//! sets them replaces what was held and would otherwise forget the rest.

use super::friends::NOTIFY_NEVER;
use super::link::Link;
use super::proto::{
    GetPerFriendPreferencesRequest, GetPerFriendPreferencesResponse, IgnoreFriendRequest,
    IgnoreFriendResponse, PerFriendPreferences, SetPerFriendPreferencesRequest,
    SetPerFriendPreferencesResponse,
};

/// Everything the account keeps about its friends.
pub async fn load(link: &Link) -> Result<Vec<PerFriendPreferences>, String> {
    let answer: GetPerFriendPreferencesResponse = link
        .call(
            "Player.GetPerFriendPreferences#1",
            &GetPerFriendPreferencesRequest {},
            true,
        )
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    Ok(answer.preferences)
}

/// What is held about one friend, or nothing yet.
fn held_about(all: Vec<PerFriendPreferences>, account: u32) -> PerFriendPreferences {
    all.into_iter()
        .find(|p| p.accountid == Some(account))
        .unwrap_or(PerFriendPreferences {
            accountid: Some(account),
            ..Default::default()
        })
}

async fn change(
    link: &Link,
    account: u32,
    edit: impl FnOnce(&mut PerFriendPreferences),
) -> Result<(), String> {
    let mut prefs = held_about(load(link).await?, account);
    edit(&mut prefs);
    let _: SetPerFriendPreferencesResponse = link
        .call(
            "Player.SetPerFriendPreferences#1",
            &SetPerFriendPreferencesRequest {
                preferences: Some(prefs),
            },
            true,
        )
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    Ok(())
}

/// Sets what the account calls a friend; empty takes the name back.
pub async fn set_nickname(link: &Link, account: u32, nickname: &str) -> Result<(), String> {
    let nickname = nickname.trim().chars().take(64).collect::<String>();
    change(link, account, |p| {
        p.nickname = (!nickname.is_empty()).then_some(nickname)
    })
    .await
}

/// Whether a friend's messages notify the account: never, or as everybody's do.
pub async fn set_muted(link: &Link, account: u32, muted: bool) -> Result<(), String> {
    change(link, account, |p| {
        p.notifications_showmessages = muted.then_some(NOTIFY_NEVER)
    })
    .await
}

/// Blocks a friend, or lifts it. Blocking somebody takes them off the friends
/// list and stops them writing to the account.
pub async fn block(link: &Link, friend: u64, block: bool) -> Result<(), String> {
    let _: IgnoreFriendResponse = link
        .call(
            "Player.IgnoreFriend#1",
            &IgnoreFriendRequest {
                steamid: Some(friend),
                unignore: Some(!block),
            },
            true,
        )
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::steam::link::{self, fake::*};
    use prost::Message;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// What a scripted Steam was asked, in order: the method and the body.
    type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// A Steam holding preferences for one friend, which keeps what it is sent.
    fn steam(held: Vec<PerFriendPreferences>, seen: Seen) -> Script {
        Box::new(move |p| {
            let method = p.header.target_job_name.clone().unwrap_or_default();
            seen.lock().unwrap().push((method.clone(), p.body.clone()));
            let body = match method.as_str() {
                "Player.GetPerFriendPreferences#1" => GetPerFriendPreferencesResponse {
                    preferences: held.clone(),
                }
                .encode_to_vec(),
                "Player.SetPerFriendPreferences#1" => {
                    SetPerFriendPreferencesResponse {}.encode_to_vec()
                }
                _ => IgnoreFriendResponse {
                    friend_relationship: Some(5),
                }
                .encode_to_vec(),
            };
            Some(vec![answer(&p, 1, body)])
        })
    }

    async fn link_to(script: Script) -> Link {
        let url = serve(script).await;
        link::open_with("test", &url, Duration::from_secs(5))
            .await
            .unwrap()
            .0
    }

    fn sent(seen: &Seen) -> PerFriendPreferences {
        let seen = seen.lock().unwrap();
        let (_, body) = seen
            .iter()
            .rev()
            .find(|(m, _)| m == "Player.SetPerFriendPreferences#1")
            .expect("a set");
        SetPerFriendPreferencesRequest::decode(body.as_slice())
            .unwrap()
            .preferences
            .unwrap()
    }

    #[tokio::test]
    async fn a_nickname_is_set_without_forgetting_the_rest_of_what_is_held() {
        let held = vec![PerFriendPreferences {
            accountid: Some(100),
            nickname: Some("Old".into()),
            notifications_showmessages: Some(2),
            sounds_showmessages: Some(1),
            ..Default::default()
        }];
        let seen = Arc::new(Mutex::new(Vec::new()));
        let link = link_to(steam(held, seen.clone())).await;
        set_nickname(&link, 100, "  Buddy  ").await.unwrap();
        let first = sent(&seen);
        assert_eq!(
            (first.accountid, first.nickname.as_deref()),
            (Some(100), Some("Buddy"))
        );
        assert_eq!(
            (first.notifications_showmessages, first.sounds_showmessages),
            (Some(2), Some(1)),
            "what else is held goes back as it was"
        );
        set_nickname(&link, 100, "   ").await.unwrap();
        assert_eq!(sent(&seen).nickname, None, "empty takes the name back");
    }

    #[tokio::test]
    async fn muting_a_friend_is_the_never_setting_and_unmuting_returns_it_to_the_default() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let link = link_to(steam(
            vec![PerFriendPreferences {
                accountid: Some(100),
                nickname: Some("Keep".into()),
                ..Default::default()
            }],
            seen.clone(),
        ))
        .await;
        set_muted(&link, 100, true).await.unwrap();
        assert_eq!(
            (
                sent(&seen).notifications_showmessages,
                sent(&seen).nickname.as_deref()
            ),
            (Some(NOTIFY_NEVER), Some("Keep"))
        );
        set_muted(&link, 100, false).await.unwrap();
        assert_eq!(sent(&seen).notifications_showmessages, None);
        // A friend nothing is held about starts from nothing.
        set_muted(&link, 7, true).await.unwrap();
        assert_eq!(
            (sent(&seen).accountid, sent(&seen).nickname),
            (Some(7), None)
        );
    }

    #[tokio::test]
    async fn blocking_names_the_person_and_unblocking_says_so() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let link = link_to(steam(vec![], seen.clone())).await;
        block(&link, 76561197960265829, true).await.unwrap();
        block(&link, 76561197960265829, false).await.unwrap();
        let seen = seen.lock().unwrap();
        let asks: Vec<IgnoreFriendRequest> = seen
            .iter()
            .filter(|(m, _)| m == "Player.IgnoreFriend#1")
            .map(|(_, b)| IgnoreFriendRequest::decode(b.as_slice()).unwrap())
            .collect();
        assert_eq!(
            asks.iter()
                .map(|a| (a.steamid, a.unignore))
                .collect::<Vec<_>>(),
            [
                (Some(76561197960265829), Some(false)),
                (Some(76561197960265829), Some(true))
            ]
        );
    }
}
