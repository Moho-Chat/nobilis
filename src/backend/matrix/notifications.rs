//! What is worth interrupting somebody for, and who is not worth hearing.
//!
//! Push rules are the server's answer to the first question and this reads
//! them rather than reimplementing them; the ignore list is the second, and
//! Matrix is the one protocol here that keeps it server-side where it
//! belongs.

use super::*;
use crate::notify::NotifyLevel;
use anyhow::bail;

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
    match read_push_rules(state, account_id, homeserver_url, access_token).await {
        // Onto the rooms already known - restored from the last run, before
        // anything has been said in them.
        Ok(_) => apply_all(state, account_id),
        Err(e) => tracing::debug!("matrix[{account_id}]: reading push rules: {e:#}"),
    }
}

/// Reads the account's push rules and keeps them, saying so if it cannot.
async fn read_push_rules(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str) -> Result<Value> {
    let base = homeserver_url.trim_end_matches('/');
    let rules = http::get_json(&format!("{base}/_matrix/client/v3/pushrules/"), access_token).await?;
    state.runtime.set_matrix_push_rules(account_id, rules.clone());
    Ok(rules)
}

/// Sets how much a room may interrupt, for this account, everywhere it is
/// signed in.
///
/// Written the way Element writes it, because Element is what the same account
/// is most likely signed in to: a room's four states are three different rules
/// and the absence of any.
///
/// - Match the default: no rule of the room's own at all.
/// - All messages: a room rule that notifies, with the default sound.
/// - Mentions and keywords: a room rule that says "do not notify". Mentions and
///   keywords still get through, because they are override and content rules
///   and those are read first.
/// - Mute: an *override* rule for the room, which is read before everything
///   else and so is the only one that silences a mention too.
///
/// The last two were one thing here until now - a mute wrote the room rule, which
/// Element reads as "Mentions and keywords", so a room muted from here was not
/// muted anywhere else.
pub async fn set_room_level(state: &AppState, account_id: &str, buffer_id: &str, level: NotifyLevel) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let rule = |kind: &str| format!("{base}/_matrix/client/v3/pushrules/global/{kind}/{encoded}");
    let token = &account.access_token;

    // A rule that is not there is the ordinary case, not a failure.
    let remove = |kind: &'static str| {
        let url = rule(kind);
        async move {
            match http::delete_json(&url, token).await {
                Err(e) if !format!("{e:#}").contains("M_NOT_FOUND") => Err(e).with_context(|| format!("taking the {kind} rule off the room")),
                _ => Ok(()),
            }
        }
    };

    match level {
        NotifyLevel::Mute => {
            remove("room").await?;
            let body = serde_json::json!({
                "conditions": [{ "kind": "event_match", "key": "room_id", "pattern": room_id }],
                "actions": ["dont_notify"],
            });
            http::put_json(&rule("override"), token, body).await.context("muting the room")?;
        }
        NotifyLevel::Mentions => {
            remove("override").await?;
            http::put_json(&rule("room"), token, serde_json::json!({ "actions": ["dont_notify"] })).await.context("setting the room to mentions only")?;
        }
        NotifyLevel::All => {
            remove("override").await?;
            let body = serde_json::json!({ "actions": ["notify", { "set_tweak": "sound", "value": "default" }] });
            http::put_json(&rule("room"), token, body).await.context("setting the room to every message")?;
        }
        NotifyLevel::Default => {
            remove("override").await?;
            remove("room").await?;
        }
    }
    // A rule written over one somebody had switched off is still switched off
    // on some servers; this is the one thing about it the PUT does not say.
    if matches!(level, NotifyLevel::Mentions | NotifyLevel::All | NotifyLevel::Mute) {
        let kind = if level == NotifyLevel::Mute { "override" } else { "room" };
        let _ = http::put_json(&format!("{}/enabled", rule(kind)), token, serde_json::json!({ "enabled": true })).await;
    }

    // What the server now says, read back rather than assumed. A write it
    // accepted is not the same as a room that is set the way it was asked: a
    // rule above it can still win, or the server can say yes and keep the old
    // answer, and "it worked" with the room unchanged is the worst thing this
    // could report. So the rules are read again, and the room is judged by
    // them; if they do not agree with what was asked, that is the answer.
    let rules = read_push_rules(state, account_id, &account.homeserver_url, &account.access_token)
        .await
        .context("the change was sent, but the server's rules could not be read back to check it")?;
    let now = room_level(&rules, &room_id);
    // Whatever the answer, the room is shown as the server has it.
    state.runtime.set_notify_level(state, buffer_id, now, now);
    if now != level {
        bail!("the server accepted the change but still lists this room as {} on {}", now.as_str(), account.homeserver_url);
    }
    Ok(())
}

/// What the account's push rules say about one room, as Element would read
/// them.
///
/// The same order Element reads in, which is the order the rules are applied
/// in: an override rule that matches the whole room is a mute and beats
/// everything; failing that the room's own rule decides between mentions only
/// and every message; failing that nothing was said, and the account's
/// defaults stand.
///
/// "Do not notify" is both `["dont_notify"]` and an empty list - the older word
/// and the newer spelling - and a rule somebody switched off says nothing.
pub(super) fn room_level(rules: &Value, room_id: &str) -> NotifyLevel {
    let enabled = |rule: &&Value| rule["enabled"].as_bool().unwrap_or(true);
    let rules_of = |kind: &str| -> Vec<&Value> { rules["global"][kind].as_array().into_iter().flatten().filter(enabled).collect() };

    // A mute is an override rule that matches this room and nothing but this
    // room. Named for the room by Element; matched on what it says as well, so
    // one made by a client that names it differently is still a mute.
    let muted = rules_of("override").into_iter().any(|rule| {
        let only_this_room = rule["conditions"].as_array().is_some_and(|conditions| {
            conditions.len() == 1
                && conditions[0]["kind"].as_str() == Some("event_match")
                && conditions[0]["key"].as_str() == Some("room_id")
                && conditions[0]["pattern"].as_str() == Some(room_id)
        });
        (only_this_room || rule["rule_id"].as_str() == Some(room_id)) && silences(&rule["actions"])
    });
    if muted {
        return NotifyLevel::Mute;
    }

    match rules_of("room").into_iter().find(|rule| rule["rule_id"].as_str() == Some(room_id)) {
        Some(rule) if silences(&rule["actions"]) => NotifyLevel::Mentions,
        Some(rule) if rule["actions"].as_array().is_some_and(|a| a.iter().any(|action| action.as_str() == Some("notify"))) => NotifyLevel::All,
        _ => NotifyLevel::Default,
    }
}

/// Puts what the push rules say onto one room, if they have been read.
pub(super) fn apply_room(state: &AppState, account_id: &str, buffer_id: &str, room_id: &str) {
    let Some(rules) = state.runtime.matrix_push_rules(account_id) else { return };
    let level = room_level(&rules, room_id);
    state.runtime.set_notify_level(state, buffer_id, level, level);
}

/// Puts what the push rules say onto every room this account has.
///
/// Run when the rules are first read and whenever another client changes
/// them: a room quietened in Element is quiet here the moment the server says
/// so, rather than the next time somebody speaks in it.
pub(super) fn apply_all(state: &AppState, account_id: &str) {
    let prefix = format!("{account_id}|");
    for buffer in state.runtime.list_buffers() {
        if !buffer.id.starts_with(&prefix) {
            continue;
        }
        if let Some(room_id) = state.runtime.get_matrix_room(&buffer.id) {
            apply_room(state, account_id, &buffer.id, &room_id);
        }
    }
}

/// The account's push rules, as another client has just changed them.
///
/// They arrive as the `m.push_rules` account data event, whose content is the
/// whole rule set in the same shape the rules endpoint answers with.
pub(super) fn push_rules_changed(state: &AppState, account_id: &str, content: &Value) {
    if !content["global"].is_object() {
        return;
    }
    state.runtime.set_matrix_push_rules(account_id, content.clone());
    apply_all(state, account_id);
}

/// Whether a room's messages should announce themselves, per the account's
/// own push rules.
///
/// Two questions are asked of them, because two are all anybody sets: is this
/// room muted, and does this message contain a word somebody asked to be told
/// about. Everything else in the rule language - conditions on message counts,
/// sender display names, arbitrary event fields - is left to the server, whose
/// job it is, and to the clients that edit it.
pub(super) fn push_rule_verdict(rules: &Value, room_id: &str, body: &str) -> (NotifyLevel, bool) {
    let level = room_level(rules, room_id);

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

    (level, keyword)
}

/// Whether the account still has the spec's own rule suppressing notices.
///
/// `.m.rule.suppress_notices` is an override rule every homeserver ships
/// enabled, and an override beats the content rules that would otherwise
/// notify - so on a stock account a bot's notice never raises anything, even
/// one carrying somebody's name. Asked of the account's real rules rather
/// than assumed, because it is a rule like any other and somebody who has
/// turned it off meant to.
pub(super) fn notices_are_suppressed(rules: &Value) -> bool {
    rules["global"]["override"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| rule["rule_id"].as_str() == Some(".m.rule.suppress_notices"))
        .any(|rule| rule["enabled"].as_bool().unwrap_or(true) && silences(&rule["actions"]))
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
    use super::notices_are_suppressed;
    use super::{matches_keyword, push_rule_verdict, room_level};
    use crate::notify::NotifyLevel;
    use serde_json::json;

    const ROOM: &str = "!q:example.org";

    /// The four states, each written the way Element writes it - which is the
    /// only reason a change made there is read as the same thing here.
    #[test]
    fn each_level_is_read_the_way_element_writes_it() {
        let nothing = json!({ "global": {} });
        assert_eq!(room_level(&nothing, ROOM), NotifyLevel::Default);

        let mute = json!({ "global": { "override": [{
            "rule_id": ROOM, "enabled": true, "default": false,
            "conditions": [{ "kind": "event_match", "key": "room_id", "pattern": ROOM }],
            "actions": ["dont_notify"]
        }] } });
        assert_eq!(room_level(&mute, ROOM), NotifyLevel::Mute);

        let mentions = json!({ "global": { "room": [{ "rule_id": ROOM, "enabled": true, "actions": ["dont_notify"] }] } });
        assert_eq!(room_level(&mentions, ROOM), NotifyLevel::Mentions);

        let all = json!({ "global": { "room": [{
            "rule_id": ROOM, "enabled": true,
            "actions": ["notify", { "set_tweak": "sound", "value": "default" }]
        }] } });
        assert_eq!(room_level(&all, ROOM), NotifyLevel::All);
    }

    /// What moho wrote before this: a *room* rule with nothing to do. Element
    /// reads that as mentions only, and so does this now - which is the point,
    /// since a room it called muted was never muted anywhere else.
    #[test]
    fn a_room_rule_that_says_nothing_is_mentions_only_not_a_mute() {
        let old = json!({ "global": { "room": [{ "rule_id": ROOM, "actions": [] }] } });
        assert_eq!(room_level(&old, ROOM), NotifyLevel::Mentions);
        let spelled_old = json!({ "global": { "room": [{ "rule_id": ROOM, "actions": ["dont_notify"] }] } });
        assert_eq!(room_level(&spelled_old, ROOM), NotifyLevel::Mentions);
    }

    #[test]
    fn a_mute_beats_the_room_rule_and_both_are_per_room() {
        let both = json!({ "global": {
            "override": [{ "rule_id": ROOM, "conditions": [{ "kind": "event_match", "key": "room_id", "pattern": ROOM }], "actions": [] }],
            "room": [{ "rule_id": ROOM, "actions": ["notify"] }]
        } });
        assert_eq!(room_level(&both, ROOM), NotifyLevel::Mute);
        assert_eq!(room_level(&both, "!other:example.org"), NotifyLevel::Default);
    }

    /// A rule somebody switched off says nothing, and the spec's own override
    /// rules are not somebody's mute.
    #[test]
    fn rules_switched_off_or_not_about_this_room_are_not_a_level() {
        let off = json!({ "global": {
            "override": [{ "rule_id": ROOM, "enabled": false, "actions": [] }],
            "room": [{ "rule_id": ROOM, "enabled": false, "actions": ["dont_notify"] }]
        } });
        assert_eq!(room_level(&off, ROOM), NotifyLevel::Default);

        let stock = json!({ "global": { "override": [{ "rule_id": ".m.rule.suppress_notices", "actions": [] }] } });
        assert_eq!(room_level(&stock, ROOM), NotifyLevel::Default);

        // An override that silences more than this room is not this room's mute.
        let wide = json!({ "global": { "override": [{
            "rule_id": "somebody's own",
            "conditions": [
                { "kind": "event_match", "key": "room_id", "pattern": ROOM },
                { "kind": "event_match", "key": "sender", "pattern": "@bot:example.org" }
            ],
            "actions": []
        }] } });
        assert_eq!(room_level(&wide, ROOM), NotifyLevel::Default);
    }

    #[test]
    fn the_verdict_carries_the_level_beside_the_keyword() {
        let rules = json!({ "global": {
            "room": [{ "rule_id": ROOM, "actions": ["dont_notify"] }],
            "content": [{ "rule_id": "moho", "pattern": "moho", "actions": ["notify"] }]
        } });
        assert_eq!(push_rule_verdict(&rules, ROOM, "look at moho").0, NotifyLevel::Mentions);
        assert!(push_rule_verdict(&rules, ROOM, "look at moho").1);
    }

    #[test]
    fn a_keyword_notifies() {
        let rules = json!({ "global": { "content": [{ "rule_id": "moho", "pattern": "moho", "actions": ["notify"] }] } });
        assert!(push_rule_verdict(&rules, "!r:example.org", "look at MOHO today").1);
        assert!(!push_rule_verdict(&rules, "!r:example.org", "nothing here").1);
        // Inside a longer word is not the word.
        assert!(!push_rule_verdict(&rules, "!r:example.org", "mohoism").1);
    }

    /// The default rule is the account's own name, which this client already
    /// matches for itself - honouring it too would be two mechanisms
    /// disagreeing about one thing.
    #[test]
    fn the_built_in_name_rule_is_left_to_the_client() {
        let rules = json!({ "global": { "content": [
            { "rule_id": ".m.rule.contains_user_name", "pattern": "someone", "actions": ["notify"] }
        ]}});
        assert!(!push_rule_verdict(&rules, "!r:example.org", "hello someone").1);
    }

    #[test]
    fn globs_match_the_way_matrix_writes_them() {
        assert!(matches_keyword("deploying the release now", "deploy*"));
        assert!(matches_keyword("a build failed", "*failed"));
        assert!(!matches_keyword("a build passed", "*failed"));
    }

    /// Every homeserver ships `.m.rule.suppress_notices` enabled, and an
    /// override beats the content rules - so on a stock account a bot's
    /// notice raises nothing, even one carrying somebody's name.
    #[test]
    fn a_stock_account_does_not_announce_notices() {
        let stock = json!({ "global": { "override": [
            { "rule_id": ".m.rule.master", "enabled": false, "actions": [] },
            { "rule_id": ".m.rule.suppress_notices", "enabled": true, "actions": ["dont_notify"] }
        ]}});
        assert!(notices_are_suppressed(&stock));

        // Somebody who turned it off meant to.
        let off = json!({ "global": { "override": [
            { "rule_id": ".m.rule.suppress_notices", "enabled": false, "actions": ["dont_notify"] }
        ]}});
        assert!(!notices_are_suppressed(&off));

        // And an account whose rules have not been read yet says nothing
        // either way rather than inventing a rule.
        assert!(!notices_are_suppressed(&json!({})));
    }
}
