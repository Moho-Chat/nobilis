//! What is worth interrupting somebody for, and who is not worth hearing.
//!
//! Push rules are the server's answer to the first question and this reads
//! them rather than reimplementing them; the ignore list is the second, and
//! Matrix is the one protocol here that keeps it server-side where it
//! belongs.

use super::*;

/// Says that this account is composing something in a room.
///
/// Matrix wants a timeout with the notice and cancels with `typing: false`,
/// unlike Discord's single fire-and-expire - so a caller that stops typing
/// can actually say so rather than waiting the notice out.
/// Reads this account's push rules, once, at connect.
///
/// Matrix keeps them on the server so every client agrees: a room muted on a
/// phone is muted here, and a keyword added on one client notifies on all of
/// them. moho read none of them, so it agreed with nothing.
///
/// Stored whole and interpreted at the point of use - see notify_decision.
/// Only a subset of the rule language is honoured, and honestly: the two
/// kinds people actually set are a per-room mute and a keyword.
/// Reads `m.ignored_user_list` - the account's own block list.
///
/// Account data rather than a local preference, which is the whole point:
/// blocking somebody on one machine and hearing from them on the next is not
/// blocking them. A server that filters them out of sync is doing the same
/// thing from its end; this client honours the list either way, since not
/// every homeserver does.
pub(super) async fn fetch_ignored_users(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str, user_id: &str) {
    let base = homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/user/{}/account_data/m.ignored_user_list",
        url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>()
    );
    match http::get_json(&url, access_token).await {
        Ok(content) => apply_ignored_users(state, account_id, &content),
        // A 404 is an account that has never ignored anybody, which is the
        // ordinary case and not worth a word.
        Err(e) => tracing::debug!("matrix[{account_id}]: reading the ignore list: {e:#}"),
    }
}

/// Takes an `m.ignored_user_list` content and makes it this account's list.
pub fn apply_ignored_users(state: &AppState, account_id: &str, content: &Value) {
    let users: std::collections::HashSet<String> = content["ignored_users"]
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    state.runtime.set_matrix_ignored(account_id, users.clone());
    let mut listed: Vec<&String> = users.iter().collect();
    listed.sort();
    state.events.emit(
        "matrixIgnored",
        serde_json::json!({ "accountId": account_id, "users": listed }),
    );
}

/// Adds somebody to the ignore list, or takes them off it.
///
/// The list is read back from the server first for the same reason pinning is:
/// this is a whole-map replacement, and writing a stale copy would un-ignore
/// whoever was added from another client since this one last looked.
pub async fn set_ignored_user(state: &AppState, account_id: &str, user_id: &str, ignored: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/user/{}/account_data/m.ignored_user_list",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    let mut users: std::collections::BTreeMap<String, Value> = match http::get_json(&url, &account.access_token).await {
        Ok(content) => content["ignored_users"]
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        Err(_) => Default::default(),
    };
    if ignored {
        // The value is an empty object by spec - the key is the whole
        // statement.
        users.insert(user_id.to_string(), serde_json::json!({}));
    } else {
        users.remove(user_id);
    }
    let content = serde_json::json!({ "ignored_users": users });
    http::put_json(&url, &account.access_token, content.clone()).await.context("writing the ignore list")?;
    apply_ignored_users(state, account_id, &content);
    Ok(())
}

pub(super) async fn fetch_push_rules(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str) {
    let base = homeserver_url.trim_end_matches('/');
    match http::get_json(&format!("{base}/_matrix/client/v3/pushrules/"), access_token).await {
        Ok(rules) => state.runtime.set_matrix_push_rules(account_id, rules),
        Err(e) => tracing::debug!("matrix[{account_id}]: reading push rules: {e:#}"),
    }
}

/// Mutes a room for this account, everywhere it is signed in - or stops.
///
/// A per-room push rule, which is what Element writes and what a phone reads.
/// Muting only locally was the old behaviour and is still what the client
/// does for services with no such idea; on Matrix the account itself can hold
/// the answer, so it should.
pub async fn set_room_muted(state: &AppState, account_id: &str, buffer_id: &str, muted: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/pushrules/global/room/{encoded}");

    if muted {
        // An empty action list is how the current spec spells "notify about
        // nothing"; `dont_notify` is the older word for it and servers still
        // accept both. The new one is sent, since that is what a current
        // client reading it back expects to find.
        http::put_json(&url, &account.access_token, serde_json::json!({ "actions": [] })).await.context("muting the room")?;
    } else {
        http::delete_json(&url, &account.access_token).await.context("unmuting the room")?;
    }
    state.runtime.set_silenced(buffer_id, muted);

    // The rules are cached; re-read rather than patch the copy, so what is
    // held is what the server actually has.
    fetch_push_rules(state, account_id, &account.homeserver_url, &account.access_token).await;
    Ok(())
}

/// Whether a room's messages should announce themselves, per the account's
/// own push rules.
///
/// Two questions are asked of them, because two are all anybody sets: is this
/// room muted, and does this message contain a word somebody asked to be told
/// about. Everything else in the rule language - conditions on message counts,
/// sender display names, arbitrary event fields - is left to the server, whose
/// job it is, and to the clients that edit it.
pub(super) fn push_rule_verdict(rules: &Value, room_id: &str, body: &str) -> (bool, bool) {
    let muted = rules["global"]["room"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| rule["rule_id"].as_str() == Some(room_id))
        .filter(|rule| rule["enabled"].as_bool().unwrap_or(true))
        .any(|rule| silences(&rule["actions"]));

    let lower = body.to_lowercase();
    let keyword = rules["global"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| rule["enabled"].as_bool().unwrap_or(true))
        // The default content rule is the account's own name, which this
        // client already matches for itself; skipping it keeps one mechanism
        // rather than two disagreeing about the same thing.
        .filter(|rule| rule["rule_id"].as_str() != Some(".m.rule.contains_user_name"))
        .filter_map(|rule| rule["pattern"].as_str())
        .any(|pattern| matches_keyword(&lower, &pattern.to_lowercase()));

    (muted, keyword)
}

/// Whether a rule's actions amount to "say nothing".
///
/// Both spellings: the old `dont_notify` action, and the newer form where an
/// empty action list means the same thing.
pub(super) fn silences(actions: &Value) -> bool {
    let Some(actions) = actions.as_array() else { return false };
    actions.is_empty() || actions.iter().any(|a| a.as_str() == Some("dont_notify"))
}

/// A keyword match, on word boundaries - Matrix's own globbing allows `*`,
/// and a bare word must not match inside a longer one.
pub(super) fn matches_keyword(body: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if pattern.contains('*') {
        let mut cursor = 0usize;
        for part in pattern.split('*').filter(|p| !p.is_empty()) {
            match body[cursor..].find(part) {
                Some(at) => cursor += at + part.len(),
                None => return false,
            }
        }
        return true;
    }
    body.split(|c: char| !c.is_alphanumeric()).any(|word| word == pattern)
}

#[cfg(test)]
mod push_rule_tests {
    use super::{matches_keyword, push_rule_verdict};
    use serde_json::json;

    #[test]
    fn a_muted_room_is_muted_here_too() {
        let rules = json!({ "global": { "room": [{ "rule_id": "!quiet:example.org", "actions": ["dont_notify"] }] } });
        assert_eq!(push_rule_verdict(&rules, "!quiet:example.org", "anything").0, true);
        assert_eq!(push_rule_verdict(&rules, "!other:example.org", "anything").0, false);
        // The newer spelling of the same thing.
        let empty = json!({ "global": { "room": [{ "rule_id": "!quiet:example.org", "actions": [] }] } });
        assert_eq!(push_rule_verdict(&empty, "!quiet:example.org", "x").0, true);
        // A rule somebody switched off says nothing.
        let off = json!({ "global": { "room": [{ "rule_id": "!quiet:example.org", "actions": [], "enabled": false }] } });
        assert_eq!(push_rule_verdict(&off, "!quiet:example.org", "x").0, false);
    }

    #[test]
    fn a_keyword_notifies() {
        let rules = json!({ "global": { "content": [{ "rule_id": "moho", "pattern": "moho", "actions": ["notify"] }] } });
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "look at MOHO today").1, true);
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "nothing here").1, false);
        // Inside a longer word is not the word.
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "mohoism").1, false);
    }

    /// The default rule is the account's own name, which this client already
    /// matches for itself - honouring it too would be two mechanisms
    /// disagreeing about one thing.
    #[test]
    fn the_built_in_name_rule_is_left_to_the_client() {
        let rules = json!({ "global": { "content": [
            { "rule_id": ".m.rule.contains_user_name", "pattern": "someone", "actions": ["notify"] }
        ]}});
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "hello someone").1, false);
    }

    #[test]
    fn globs_match_the_way_matrix_writes_them() {
        assert!(matches_keyword("deploying the release now", "deploy*"));
        assert!(matches_keyword("a build failed", "*failed"));
        assert!(!matches_keyword("a build passed", "*failed"));
    }
}
