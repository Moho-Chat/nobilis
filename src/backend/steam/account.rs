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

/// Steam's own lists of what is rude, as patterns, one to a line: what the client itself fetches
/// from the community site. `profanity` is dropped by the setting that allows it, and `clean` is
/// what is let through whatever else says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dictionary {
    pub banned: Vec<String>,
    pub profanity: Vec<String>,
    pub clean: Vec<String>,
}

/// The words the account has asked to have hidden, and the patterns they and Steam's lists make.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextFilter {
    /// `ETextFilterSetting`: 0 and 3 use none of Steam's lists, 2 only the banned one, 1 both.
    pub setting: i32,
    pub ignore_friends: bool,
    pub banned: Vec<String>,
    pub clean: Vec<String>,
    pub compiled: Compiled,
}

/// What the lists and the account's own words make: one pattern for what is hidden and one for
/// what is let through, each only as good as the lines in them that are patterns at all.
#[derive(Clone, Debug, Default)]
pub struct Compiled {
    hidden: Option<regex::Regex>,
    allowed: Option<regex::Regex>,
}

impl PartialEq for Compiled {
    fn eq(&self, other: &Self) -> bool {
        self.hidden.as_ref().map(|r| r.as_str()) == other.hidden.as_ref().map(|r| r.as_str())
            && self.allowed.as_ref().map(|r| r.as_str())
                == other.allowed.as_ref().map(|r| r.as_str())
    }
}

/// The lines that are patterns this can read, as one alternation. A line that is not one is left
/// out, as the client leaves out one its own engine cannot compile.
fn alternation(lines: impl IntoIterator<Item = String>) -> Option<String> {
    let usable: Vec<String> = lines
        .into_iter()
        .map(|l| l.trim().trim_start_matches('\u{feff}').to_string())
        .filter(|l| !l.is_empty() && regex::Regex::new(&format!(r"(?i)\b({l})\b")).is_ok())
        .collect();
    (!usable.is_empty()).then(|| usable.join("|"))
}

impl TextFilter {
    /// Builds the patterns the client builds: Steam's lists as the setting says, and the account's own
    /// words on top whatever the setting is.
    pub fn compile(
        setting: i32,
        banned: &[String],
        clean: &[String],
        dictionary: &Dictionary,
    ) -> Compiled {
        let mut hidden: Vec<String> = match setting {
            0 | 3 => Vec::new(),
            2 => dictionary.banned.clone(),
            _ => dictionary
                .profanity
                .iter()
                .chain(&dictionary.banned)
                .cloned()
                .collect(),
        };
        hidden.extend(banned.iter().cloned());
        let allowed: Vec<String> = dictionary.clean.iter().chain(clean).cloned().collect();
        Compiled {
            hidden: alternation(hidden)
                .and_then(|a| regex::Regex::new(&format!(r"(?i)\b({a})\b")).ok()),
            allowed: alternation(allowed)
                .and_then(|a| regex::Regex::new(&format!(r"(?i)^(\b({a})\b)$")).ok()),
        }
    }
}

/// Steam's lists, asked of the community site once a run.
static DICTIONARY: LazyLock<Mutex<Option<Arc<Dictionary>>>> = LazyLock::new(Default::default);

async fn dictionary(key: &str) -> Arc<Dictionary> {
    if let Some(held) = DICTIONARY.lock().unwrap().clone() {
        return held;
    }
    let router = crate::net::route::router();
    let http = router.client_if("steam", router.routed(key), |b| {
        b.timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("moho/", env!("CARGO_PKG_VERSION")))
    });
    let fetch = |kind: &'static str| {
        let http = http.clone();
        async move {
            let url = format!("https://community.fastly.steamstatic.com/textfilter/gettextfilterdictionary?type={kind}&language=english&v=1");
            let raw = http
                .get(url)
                .send()
                .await
                .ok()
                .filter(|r| r.status().is_success())?
                .bytes()
                .await
                .ok()?;
            Some(
                super::web::plain(&raw)
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            )
        }
    };
    let (banned, profanity, clean) =
        tokio::join!(fetch("banned"), fetch("profanity"), fetch("clean_public"));
    let dictionary = Arc::new(Dictionary {
        banned: banned.unwrap_or_default(),
        profanity: profanity.unwrap_or_default(),
        clean: clean.unwrap_or_default(),
    });
    // Kept only when it came: a list that did not is asked for again next time.
    if !dictionary.banned.is_empty() {
        *DICTIONARY.lock().unwrap() = Some(dictionary.clone());
    }
    dictionary
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
                "on": f.compiled.hidden.is_some(),
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
    HELD.lock()
        .unwrap()
        .get(account_id)
        .map(|f| (**f).clone())
        .unwrap_or_default()
}

pub fn clear(account_id: &str) {
    HELD.lock().unwrap().remove(account_id);
}

fn keep(state: &AppState, account_id: &str, facts: Facts) {
    let changed = HELD
        .lock()
        .unwrap()
        .insert(account_id.to_string(), Arc::new(facts.clone()))
        .map(|old| *old != facts)
        .unwrap_or(true);
    if changed {
        state.events.emit(
            "steamAccountFacts",
            json!({ "accountId": account_id, "facts": facts.to_json() }),
        );
    }
}

/// Reads what Steam says of the account. Each part on its own: one that is
/// refused leaves the rest.
pub async fn load(
    state: AppState,
    link: Link,
    account_id: String,
    steamid: u64,
    account_flags: u32,
) {
    // What is known at once, before the rest is asked for.
    keep(
        &state,
        &account_id,
        Facts {
            limited: is_limited(account_flags),
            ..Default::default()
        },
    );
    let parental_request = GetParentalSettingsRequest {
        steamid: Some(steamid),
    };
    let (preferences, words, privacy, parental) = tokio::join!(
        link.call::<_, GetCommunityPreferencesResponse>(
            "Player.GetCommunityPreferences#1",
            &GetCommunityPreferencesRequest {},
            true
        ),
        link.call::<_, GetTextFilterWordsResponse>(
            "Player.GetTextFilterWords#1",
            &GetTextFilterWordsRequest {},
            true
        ),
        link.call::<_, GetPrivacySettingsResponse>(
            "Player.GetPrivacySettings#1",
            &GetPrivacySettingsRequest {},
            true
        ),
        link.call::<_, GetParentalSettingsResponse>(
            "Parental.GetParentalSettings#1",
            &parental_request,
            true
        ),
    );
    let filter = match preferences.ok().and_then(|p| p.preferences) {
        Some(p) => {
            let words = words.ok().and_then(|w| w.words).unwrap_or_default();
            let setting = p.text_filter_setting.unwrap_or(0);
            // Steam's own lists are needed only where the setting uses them.
            let lists = if matches!(setting, 1 | 2) {
                (*dictionary(&account_id).await).clone()
            } else {
                Dictionary::default()
            };
            let compiled = TextFilter::compile(
                setting,
                &words.text_filter_custom_banned_words,
                &words.text_filter_custom_clean_words,
                &lists,
            );
            Some(TextFilter {
                setting,
                ignore_friends: p.text_filter_ignore_friends.unwrap_or(true),
                banned: words.text_filter_custom_banned_words,
                clean: words.text_filter_custom_clean_words,
                compiled,
            })
        }
        None => None,
    };
    let family_view = parental.ok().and_then(|p| p.settings).map(|s| {
        let enabled = s.is_enabled.unwrap_or(false);
        (
            enabled,
            enabled && s.enabled_features.unwrap_or(0) & (1 << FRIENDS_FEATURE) == 0,
        )
    });
    keep(
        &state,
        &account_id,
        Facts {
            limited: is_limited(account_flags),
            family_view,
            filter,
            privacy: privacy.ok().and_then(|p| p.privacy_settings),
        },
    );
}

/// A message with the words the account hides masked, for somebody the filter applies to: a stranger
/// always, and a friend unless the account says friends are not filtered.
pub fn filtered(account_id: &str, from_friend: bool, text: &str) -> String {
    let facts = HELD.lock().unwrap().get(account_id).cloned();
    let Some(filter) = facts.as_ref().and_then(|f| f.filter.as_ref()) else {
        return text.to_string();
    };
    if (from_friend && filter.ignore_friends) || filter.compiled.hidden.is_none() {
        return text.to_string();
    }
    mask(text, &filter.compiled)
}

/// Replaces each whole word the patterns hide with a heart for every letter, unless the word is one the
/// allowed pattern says is clean, as Steam does.
pub fn mask(text: &str, patterns: &Compiled) -> String {
    let Some(hidden) = &patterns.hidden else {
        return text.to_string();
    };
    hidden
        .replace_all(text, |found: &regex::Captures| {
            let word = &found[0];
            if patterns.allowed.as_ref().is_some_and(|a| a.is_match(word)) {
                word.to_string()
            } else {
                "\u{2665}".repeat(word.chars().count())
            }
        })
        .into_owned()
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
    event
        .map(|e| {
            [&e.city, &e.state, &e.country]
                .iter()
                .filter_map(|p| p.as_deref())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

pub fn read_sessions(answer: &EnumerateTokensResponse) -> Vec<Session> {
    let mut sessions: Vec<Session> = answer
        .refresh_tokens
        .iter()
        .filter_map(|t| {
            let id = t.token_id?;
            Some(Session {
                id,
                description: t
                    .token_description
                    .clone()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "a signed-in session".to_string()),
                platform: t.platform_type.unwrap_or(0),
                last_seen: t
                    .last_seen
                    .as_ref()
                    .and_then(|l| l.time)
                    .or(t.time_updated)
                    .unwrap_or(0),
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
        .call(
            "Authentication.EnumerateTokens#1",
            &EnumerateTokensRequest {
                include_revoked: Some(false),
            },
            true,
        )
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    Ok(read_sessions(&answer))
}

/// Signs one of them out, for good: whoever has it is no longer signed in.
pub async fn revoke(link: &Link, steamid: u64, token_id: u64) -> Result<(), String> {
    let _: RevokeRefreshTokenResponse = link
        .call(
            "Authentication.RevokeRefreshToken#1",
            &RevokeRefreshTokenRequest {
                token_id: Some(token_id),
                steamid: Some(steamid),
            },
            true,
        )
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    Ok(())
}

/// When a token stops working, in unix seconds, from its own claims.
pub fn expires_at(token: &str) -> Option<u64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !due_for_renewal(&config.refresh_token, now) {
        return;
    }
    let request = GenerateAccessTokenRequest {
        refresh_token: Some(config.refresh_token.clone()),
        steamid: Some(config.steamid),
        renewal_type: Some(1),
    };
    match link
        .call::<_, GenerateAccessTokenResponse>(
            "Authentication.GenerateAccessTokenForApp#1",
            &request,
            true,
        )
        .await
    {
        Ok(answer) => {
            if let Some(fresh) = answer
                .refresh_token
                .filter(|t| !t.is_empty() && *t != config.refresh_token)
            {
                let id = config.account_id();
                let _ = state
                    .accounts
                    .mutate_steam(&id, |a| a.refresh_token = fresh);
                tracing::info!("steam[{id}]: the sign-in token was renewed");
            }
        }
        Err(e) => tracing::info!(
            "steam[{}]: the sign-in token could not be renewed: {e}",
            config.account_id()
        ),
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

    fn lists() -> Dictionary {
        Dictionary {
            banned: vec!["bann?ed".into(), "[unclosed".into()],
            profanity: vec!["f+u+d+ge".into()],
            clean: vec!["banned together".into(), "bannedx".into()],
        }
    }

    #[test]
    fn what_the_lists_and_the_account_hide_is_hidden_as_whole_words_with_a_heart_each() {
        let own = ["heck".to_string(), "Darn".to_string()];
        let on = TextFilter::compile(1, &own, &["darn".to_string()], &lists());
        assert_eq!(
            mask("what the heck, a Heckle and DARN it", &on),
            "what the \u{2665}\u{2665}\u{2665}\u{2665}, a Heckle and DARN it",
            "the account's own words, and what it marked clean is let through"
        );
        assert_eq!(mask("fuuudge and banned and bannedx", &on), "\u{2665}\u{2665}\u{2665}\u{2665}\u{2665}\u{2665}\u{2665} and \u{2665}\u{2665}\u{2665}\u{2665}\u{2665}\u{2665} and bannedx", "Steam's lists, with a line that is no pattern left out and a clean word let through");
        assert_eq!(
            mask("banned together", &on),
            "\u{2665}\u{2665}\u{2665}\u{2665}\u{2665}\u{2665} together",
            "as in the client, a clean entry lets through only a match that is the whole of it"
        );
        assert_eq!(mask("nothing here", &on), "nothing here");
        // The setting that allows profanity uses only the banned list.
        let mild = TextFilter::compile(2, &[], &[], &lists());
        assert_eq!(
            mask("fudge banned", &mild),
            "fudge \u{2665}\u{2665}\u{2665}\u{2665}\u{2665}\u{2665}"
        );
        // Switched off: Steam's lists are not used, but the account's own words still are, as in the client.
        let off = TextFilter::compile(3, &own, &[], &lists());
        assert_eq!(
            mask("banned heck", &off),
            "banned \u{2665}\u{2665}\u{2665}\u{2665}"
        );
    }

    #[test]
    fn the_filter_applies_to_strangers_and_to_friends_only_if_the_account_says_so() {
        let id = "steam:filter-test";
        let words = || TextFilter {
            setting: 1,
            ignore_friends: true,
            banned: vec!["heck".into()],
            clean: vec![],
            compiled: TextFilter::compile(1, &["heck".to_string()], &[], &Dictionary::default()),
        };
        HELD.lock().unwrap().insert(
            id.into(),
            Arc::new(Facts {
                filter: Some(words()),
                ..Default::default()
            }),
        );
        assert_eq!(
            filtered(id, false, "oh heck"),
            "oh \u{2665}\u{2665}\u{2665}\u{2665}"
        );
        assert_eq!(
            filtered(id, true, "oh heck"),
            "oh heck",
            "friends are not filtered by default"
        );
        HELD.lock().unwrap().insert(
            id.into(),
            Arc::new(Facts {
                filter: Some(TextFilter {
                    ignore_friends: false,
                    ..words()
                }),
                ..Default::default()
            }),
        );
        assert_eq!(
            filtered(id, true, "oh heck"),
            "oh \u{2665}\u{2665}\u{2665}\u{2665}"
        );
        assert_eq!(
            filtered("steam:nobody", false, "oh heck"),
            "oh heck",
            "no filter known"
        );
        clear(id);
    }

    /// Steam's real lists from the community site, and what they make of a sentence. The words used are the
    /// mildest on the list. Needs the network.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn steams_own_lists_are_fetched_and_hide_what_they_say() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let d = dictionary("steam:filter-probe").await;
        println!(
            "FILTER lists: {} banned, {} profanity, {} clean",
            d.banned.len(),
            d.profanity.len(),
            d.clean.len()
        );
        let on = TextFilter::compile(1, &[], &[], &d);
        let mild = TextFilter::compile(2, &[], &[], &d);
        println!(
            "FILTER on:    {:?}",
            mask("what the fuck, a fukushima trip and pussycat", &on)
        );
        println!(
            "FILTER mild:  {:?}",
            mask("what the fuck, a fukushima trip and pussycat", &mild)
        );
        assert!(d.banned.len() > 20 && d.profanity.len() > 20);
        assert_eq!(
            mask("what the fuck", &on),
            "what the \u{2665}\u{2665}\u{2665}\u{2665}"
        );
        assert_eq!(
            mask("fukushima pussycat", &on),
            "fukushima pussycat",
            "clean words are let through"
        );
        assert_eq!(
            mask("what the fuck", &mild),
            "what the fuck",
            "the setting that allows profanity"
        );
    }

    #[test]
    fn the_sessions_are_listed_newest_first_with_the_one_in_use_marked() {
        let answer = EnumerateTokensResponse {
            refresh_tokens: vec![
                RefreshTokenDescription {
                    token_id: Some(1),
                    token_description: Some("old phone".into()),
                    platform_type: Some(3),
                    last_seen: Some(TokenUsageEvent {
                        time: Some(100),
                        city: Some("Leeds".into()),
                        country: Some("GB".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                RefreshTokenDescription {
                    token_id: Some(2),
                    token_description: Some("moho on host".into()),
                    platform_type: Some(3),
                    time_updated: Some(900),
                    ..Default::default()
                },
                RefreshTokenDescription {
                    token_description: Some("no id".into()),
                    ..Default::default()
                },
            ],
            requesting_token: Some(2),
        };
        let got = read_sessions(&answer);
        assert_eq!(
            got.iter()
                .map(|s| (s.id, s.description.as_str(), s.current))
                .collect::<Vec<_>>(),
            [(2, "moho on host", true), (1, "old phone", false)]
        );
        assert_eq!(got[1].place, "Leeds, GB");
    }

    #[test]
    fn a_token_is_renewed_when_it_is_within_a_month_of_its_end() {
        use base64::Engine;
        let token = |exp: u64| {
            format!(
                "h.{}.s",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(format!(r#"{{"sub":"1","exp":{exp}}}"#))
            )
        };
        let now = 1_800_000_000;
        assert!(due_for_renewal(&token(now + 10 * 24 * 3600), now));
        assert!(
            due_for_renewal(&token(now - 5), now),
            "one that has run out is as due as one can be"
        );
        assert!(!due_for_renewal(&token(now + 100 * 24 * 3600), now));
        assert!(
            !due_for_renewal("not a token", now),
            "nothing is asked for a token that cannot be read"
        );
    }

    #[test]
    fn the_facts_read_in_words_for_the_window() {
        let facts = Facts {
            limited: true,
            family_view: Some((true, true)),
            filter: Some(TextFilter {
                setting: 2,
                ignore_friends: true,
                banned: vec!["a".into(), "b".into()],
                clean: vec![],
                compiled: Compiled::default(),
            }),
            privacy: Some(PrivacySettings {
                privacy_state: Some(3),
                privacy_state_friendslist: Some(1),
                ..Default::default()
            }),
        };
        let j = facts.to_json();
        assert_eq!(
            (
                j["limited"].as_bool(),
                j["familyView"]["holdsFriendsBack"].as_bool(),
                j["textFilter"]["words"].as_u64()
            ),
            (Some(true), Some(true), Some(2))
        );
        assert_eq!(
            (
                j["privacy"]["profile"].as_str(),
                j["privacy"]["friends"].as_str(),
                j["privacy"]["games"].is_null()
            ),
            (Some("public"), Some("private"), true)
        );
        assert_eq!(Facts::default().to_json()["familyView"], Value::Null);
    }
}
