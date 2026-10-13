//! What the account is, and what Steam holds back from it.
//!
//! A person signing in to a chat client wants to know why something does not
//! work as much as that it does not: an account Steam calls *limited* cannot
//! write to everybody, Family View can hold the friends list back, the text
//! filter changes what is read, and the sessions listed on the account are the
//! places it is signed in. This reads those, shows them, and acts on the ones
//! that are the client's to act on.
//!
//! Also here: keeping the account's sign-in alive. The token that signs it in
//! lasts months, and a client that waits for it to run out finds out by being
//! signed out; Steam will give a fresh one while the old one is still good.

use super::link::Link;
use super::proto::*;
use crate::accounts::SteamAccountConfig;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

/// `EAccountFlags`: a limited account, and one Steam keeps limited.
const LIMITED: u32 = 4096 | 8192;

/// Family View's feature for friends and chat: a bit in the settings, which
/// when the account is locked is the answer to "is this allowed".
const FRIENDS_FEATURE: u32 = 4;

pub fn is_limited(account_flags: u32) -> bool {
    account_flags & LIMITED != 0
}

/// The words the account has asked to have hidden, and when.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextFilter {
    /// `ETextFilterSetting`: 1 on, 2 on with profanity allowed; anything else is off here.
    pub setting: i32,
    pub ignore_friends: bool,
    pub banned: Vec<String>,
    pub clean: Vec<String>,
}

impl TextFilter {
    fn on(&self) -> bool {
        matches!(self.setting, 1 | 2)
    }
}

/// What is known about the account, as the window reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub limited: bool,
    /// Whether Family View is on, and if it holds friends and chat back.
    pub family_view: Option<(bool, bool)>,
    pub filter: Option<TextFilter>,
    pub privacy: Option<PrivacySettings>,
}

fn privacy_word(state: Option<i32>) -> Value {
    match state {
        Some(1) => json!("private"),
        Some(2) => json!("friends"),
        Some(3) => json!("public"),
        _ => Value::Null,
    }
}

impl Facts {
    pub fn to_json(&self) -> Value {
        json!({
            "limited": self.limited,
            "familyView": self.family_view.map(|(enabled, holds_friends)| json!({ "enabled": enabled, "holdsFriendsBack": holds_friends })),
            "textFilter": self.filter.as_ref().map(|f| json!({
                "on": f.on(),
                "setting": match f.setting { 1 => "on", 2 => "on, profanity allowed", 3 => "off", _ => "not in use" },
                "ignoreFriends": f.ignore_friends,
                "words": f.banned.len(),
            })),
            "privacy": self.privacy.as_ref().map(|p| json!({
                "profile": privacy_word(p.privacy_state),
                "inventory": privacy_word(p.privacy_state_inventory),
                "games": privacy_word(p.privacy_state_ownedgames),
                "playtime": privacy_word(p.privacy_state_playtime),
                "friends": privacy_word(p.privacy_state_friendslist),
            })),
        })
    }
}

static HELD: LazyLock<Mutex<HashMap<String, Arc<Facts>>>> = LazyLock::new(Default::default);

pub fn held(account_id: &str) -> Facts {
    HELD.lock().unwrap().get(account_id).map(|f| (**f).clone()).unwrap_or_default()
}

pub fn clear(account_id: &str) {
    HELD.lock().unwrap().remove(account_id);
}

fn keep(state: &AppState, account_id: &str, facts: Facts) {
    let changed = HELD.lock().unwrap().insert(account_id.to_string(), Arc::new(facts.clone())).map(|old| *old != facts).unwrap_or(true);
    if changed {
        state.events.emit("steamAccountFacts", json!({ "accountId": account_id, "facts": facts.to_json() }));
    }
}

/// Reads what Steam says of the account. Each part on its own: one that is
/// refused leaves the rest.
pub async fn load(state: AppState, link: Link, account_id: String, steamid: u64, account_flags: u32) {
    // What is known at once, before the rest is asked for.
    keep(&state, &account_id, Facts { limited: is_limited(account_flags), ..Default::default() });
    let parental_request = GetParentalSettingsRequest { steamid: Some(steamid) };
    let (preferences, words, privacy, parental) = tokio::join!(
        link.call::<_, GetCommunityPreferencesResponse>("Player.GetCommunityPreferences#1", &GetCommunityPreferencesRequest {}, true),
        link.call::<_, GetTextFilterWordsResponse>("Player.GetTextFilterWords#1", &GetTextFilterWordsRequest {}, true),
        link.call::<_, GetPrivacySettingsResponse>("Player.GetPrivacySettings#1", &GetPrivacySettingsRequest {}, true),
        link.call::<_, GetParentalSettingsResponse>("Parental.GetParentalSettings#1", &parental_request, true),
    );
    let filter = preferences.ok().and_then(|p| p.preferences).map(|p| {
        let words = words.ok().and_then(|w| w.words).unwrap_or_default();
        TextFilter { setting: p.text_filter_setting.unwrap_or(0), ignore_friends: p.text_filter_ignore_friends.unwrap_or(true), banned: words.text_filter_custom_banned_words, clean: words.text_filter_custom_clean_words }
    });
    let family_view = parental.ok().and_then(|p| p.settings).map(|s| {
        let enabled = s.is_enabled.unwrap_or(false);
        (enabled, enabled && s.enabled_features.unwrap_or(0) & (1 << FRIENDS_FEATURE) == 0)
    });
    keep(&state, &account_id, Facts { limited: is_limited(account_flags), family_view, filter, privacy: privacy.ok().and_then(|p| p.privacy_settings) });
}

/// A message with the words the account hides masked, for somebody the filter
/// applies to: a stranger always, and a friend unless the account says friends
/// are not filtered. Steam's own list of what is rude is Steam's and is not
/// here; these are the words the account added to it.
pub fn filtered(account_id: &str, from_friend: bool, text: &str) -> String {
    let facts = HELD.lock().unwrap().get(account_id).cloned();
    let Some(filter) = facts.as_ref().and_then(|f| f.filter.as_ref()) else { return text.to_string() };
    if !filter.on() || (from_friend && filter.ignore_friends) || filter.banned.is_empty() {
        return text.to_string();
    }
    mask(text, &filter.banned, &filter.clean)
}

/// Replaces each whole word that is in `banned` and not in `clean` with hearts, as Steam does.
pub fn mask(text: &str, banned: &[String], clean: &[String]) -> String {
    let hidden = |word: &str| {
        let lower = word.to_lowercase();
        banned.iter().any(|b| b.to_lowercase() == lower) && !clean.iter().any(|c| c.to_lowercase() == lower)
    };
    let mut out = String::new();
    let mut word = String::new();
    for c in text.chars().chain(std::iter::once('\0')) {
        if c.is_alphanumeric() || c == '\'' {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            if hidden(&word) {
                out.extend(std::iter::repeat_n('\u{2665}', word.chars().count()));
            } else {
                out.push_str(&word);
            }
            word.clear();
        }
        if c != '\0' {
            out.push(c);
        }
    }
    out
}

/// A place the account is signed in.
#[derive(Debug, PartialEq)]
pub struct Session {
    pub id: u64,
    pub description: String,
    /// `EAuthTokenPlatformType`: 1 launcher, 2 browser, 3 phone.
    pub platform: i32,
    pub last_seen: u32,
    pub place: String,
    pub current: bool,
}

fn place(event: Option<&TokenUsageEvent>) -> String {
    event.map(|e| [&e.city, &e.state, &e.country].iter().filter_map(|p| p.as_deref()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

pub fn read_sessions(answer: &EnumerateTokensResponse) -> Vec<Session> {
    let mut sessions: Vec<Session> = answer
        .refresh_tokens
        .iter()
        .filter_map(|t| {
            let id = t.token_id?;
            Some(Session {
                id,
                description: t.token_description.clone().filter(|d| !d.is_empty()).unwrap_or_else(|| "a signed-in session".to_string()),
                platform: t.platform_type.unwrap_or(0),
                last_seen: t.last_seen.as_ref().and_then(|l| l.time).or(t.time_updated).unwrap_or(0),
                place: place(t.last_seen.as_ref()),
                current: answer.requesting_token == Some(id),
            })
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_seen));
    sessions
}

/// Where the account is signed in.
pub async fn sessions(link: &Link) -> Result<Vec<Session>, String> {
    let answer: EnumerateTokensResponse = link
        .call("Authentication.EnumerateTokens#1", &EnumerateTokensRequest { include_revoked: Some(false) }, true)
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    Ok(read_sessions(&answer))
}

/// Signs one of them out, for good: whoever has it is no longer signed in.
pub async fn revoke(link: &Link, steamid: u64, token_id: u64) -> Result<(), String> {
    let _: RevokeRefreshTokenResponse = link
        .call("Authentication.RevokeRefreshToken#1", &RevokeRefreshTokenRequest { token_id: Some(token_id), steamid: Some(steamid) }, true)
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    Ok(())
}

/// When a token stops working, in unix seconds, from its own claims.
pub fn expires_at(token: &str) -> Option<u64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&raw).ok()?["exp"].as_u64()
}

/// How close to running out is too close: Steam renews a token while it is good, and a
/// month leaves a long time to try again after a failure.
const RENEW_WITHIN_SECONDS: u64 = 30 * 24 * 3600;

pub fn due_for_renewal(token: &str, now: u64) -> bool {
    expires_at(token).is_some_and(|exp| exp.saturating_sub(now) < RENEW_WITHIN_SECONDS)
}

/// Asks Steam for a fresh token if this one is near its end, and keeps it.
pub async fn renew_if_due(state: &AppState, link: &Link, config: &SteamAccountConfig) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if !due_for_renewal(&config.refresh_token, now) {
        return;
    }
    let request = GenerateAccessTokenRequest { refresh_token: Some(config.refresh_token.clone()), steamid: Some(config.steamid), renewal_type: Some(1) };
    match link.call::<_, GenerateAccessTokenResponse>("Authentication.GenerateAccessTokenForApp#1", &request, true).await {
        Ok(answer) => {
            if let Some(fresh) = answer.refresh_token.filter(|t| !t.is_empty() && *t != config.refresh_token) {
                let id = config.account_id();
                let _ = state.accounts.mutate_steam(&id, |a| a.refresh_token = fresh);
                tracing::info!("steam[{id}]: the sign-in token was renewed");
            }
        }
        Err(e) => tracing::info!("steam[{}]: the sign-in token could not be renewed: {e}", config.account_id()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limited_account_is_known_by_its_flags() {
        assert!(is_limited(4096));
        assert!(is_limited(8192 | 1));
        assert!(!is_limited(1 | 16384));
    }

    #[test]
    fn what_the_account_hides_is_hidden_as_whole_words_with_hearts() {
        let banned = vec!["heck".to_string(), "Darn".to_string()];
        let clean = vec!["darn".to_string()];
        assert_eq!(mask("what the heck, a Heckle and DARN it", &banned, &[]), "what the \u{2665}\u{2665}\u{2665}\u{2665}, a Heckle and \u{2665}\u{2665}\u{2665}\u{2665} it");
        assert_eq!(mask("darn", &banned, &clean), "darn", "a word the account marked clean is not hidden");
        assert_eq!(mask("nothing here", &banned, &clean), "nothing here");
        assert_eq!(mask("", &banned, &clean), "");
    }

    #[test]
    fn the_filter_applies_to_strangers_and_to_friends_only_if_the_account_says_so() {
        let id = "steam:filter-test";
        let words = || TextFilter { setting: 1, ignore_friends: true, banned: vec!["heck".into()], clean: vec![] };
        HELD.lock().unwrap().insert(id.into(), Arc::new(Facts { filter: Some(words()), ..Default::default() }));
        assert_eq!(filtered(id, false, "oh heck"), "oh \u{2665}\u{2665}\u{2665}\u{2665}");
        assert_eq!(filtered(id, true, "oh heck"), "oh heck", "friends are not filtered by default");
        HELD.lock().unwrap().insert(id.into(), Arc::new(Facts { filter: Some(TextFilter { ignore_friends: false, ..words() }), ..Default::default() }));
        assert_eq!(filtered(id, true, "oh heck"), "oh \u{2665}\u{2665}\u{2665}\u{2665}");
        HELD.lock().unwrap().insert(id.into(), Arc::new(Facts { filter: Some(TextFilter { setting: 3, ..words() }), ..Default::default() }));
        assert_eq!(filtered(id, false, "oh heck"), "oh heck", "switched off");
        assert_eq!(filtered("steam:nobody", false, "oh heck"), "oh heck", "no filter known");
        clear(id);
    }

    #[test]
    fn the_sessions_are_listed_newest_first_with_the_one_in_use_marked() {
        let answer = EnumerateTokensResponse {
            refresh_tokens: vec![
                RefreshTokenDescription { token_id: Some(1), token_description: Some("old phone".into()), platform_type: Some(3), last_seen: Some(TokenUsageEvent { time: Some(100), city: Some("Leeds".into()), country: Some("GB".into()), ..Default::default() }), ..Default::default() },
                RefreshTokenDescription { token_id: Some(2), token_description: Some("moho on host".into()), platform_type: Some(3), time_updated: Some(900), ..Default::default() },
                RefreshTokenDescription { token_description: Some("no id".into()), ..Default::default() },
            ],
            requesting_token: Some(2),
        };
        let got = read_sessions(&answer);
        assert_eq!(got.iter().map(|s| (s.id, s.description.as_str(), s.current)).collect::<Vec<_>>(), [(2, "moho on host", true), (1, "old phone", false)]);
        assert_eq!(got[1].place, "Leeds, GB");
    }

    #[test]
    fn a_token_is_renewed_when_it_is_within_a_month_of_its_end() {
        use base64::Engine;
        let token = |exp: u64| format!("h.{}.s", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"1","exp":{exp}}}"#)));
        let now = 1_800_000_000;
        assert!(due_for_renewal(&token(now + 10 * 24 * 3600), now));
        assert!(due_for_renewal(&token(now - 5), now), "one that has run out is as due as one can be");
        assert!(!due_for_renewal(&token(now + 100 * 24 * 3600), now));
        assert!(!due_for_renewal("not a token", now), "nothing is asked for a token that cannot be read");
    }

    #[test]
    fn the_facts_read_in_words_for_the_window() {
        let facts = Facts {
            limited: true,
            family_view: Some((true, true)),
            filter: Some(TextFilter { setting: 2, ignore_friends: true, banned: vec!["a".into(), "b".into()], clean: vec![] }),
            privacy: Some(PrivacySettings { privacy_state: Some(3), privacy_state_friendslist: Some(1), ..Default::default() }),
        };
        let j = facts.to_json();
        assert_eq!((j["limited"].as_bool(), j["familyView"]["holdsFriendsBack"].as_bool(), j["textFilter"]["words"].as_u64()), (Some(true), Some(true), Some(2)));
        assert_eq!((j["privacy"]["profile"].as_str(), j["privacy"]["friends"].as_str(), j["privacy"]["games"].is_null()), (Some("public"), Some("private"), true));
        assert_eq!(Facts::default().to_json()["familyView"], Value::Null);
    }
}
