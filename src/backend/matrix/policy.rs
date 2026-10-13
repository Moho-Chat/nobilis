//! What a room refuses, and whose judgement it follows.
//!
//! Three spec modules that share one job and were all invisible here:
//!
//! - **Server ACLs** (`m.room.server_acl`) - which homeservers a room will
//!   take events from. A room that has banned a server gave no sign of it,
//!   so a moderator using moho could not see why a message from another
//!   server never arrived.
//! - **Moderation policy lists** (`m.policy.rule.*`) - the subscribable ban
//!   lists rooms publish and other rooms follow.
//! - **Policy servers** (`m.room.policy`) - the newer module for the same
//!   job, where a named server vets events instead.
//!
//! Read on demand rather than cached with the rest of a room's state. All
//! three are rare, and the whole point of the first is that it explains an
//! absence - which is a question somebody asks, not something they watch.
//!
//! Writing is deliberately narrow: a server can be denied and un-denied, and
//! nothing here will write an empty `allow` list. An ACL with no allowed
//! servers cuts the room off from everybody including its own members, and
//! it cannot be undone from inside the room afterwards - the spec's own
//! warning, and the one edit worth making structurally impossible rather
//! than merely discouraged.

use super::*;

pub const SERVER_ACL: &str = "m.room.server_acl";
pub const POLICY_SERVER: &str = "m.room.policy";

/// The three things a policy rule can be about.
///
/// Still under both spellings: the stable `m.policy.rule.*` and the
/// `org.matrix.mjolnir.*` prefix the ban-list rooms that predate it are still
/// full of. A client that reads only the stable one sees an empty list in
/// most of the rooms that actually have rules.
const RULE_TYPES: [(&str, &str); 6] = [
    ("m.policy.rule.user", "user"),
    ("m.policy.rule.room", "room"),
    ("m.policy.rule.server", "server"),
    ("org.matrix.mjolnir.rule.user", "user"),
    ("org.matrix.mjolnir.rule.room", "room"),
    ("org.matrix.mjolnir.rule.server", "server"),
];

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect()
}

/// Reads a room's whole moderation picture out of its state.
///
/// Takes the state array rather than fetching it, so this is testable and so
/// the caller decides when a room's full state is worth asking for - which on
/// a large room is not something to do on a panel opening.
pub fn read(events: &[&Value]) -> Value {
    let mut acl: Option<&Value> = None;
    let mut policy_server: Option<String> = None;
    let mut rules: Vec<Value> = Vec::new();

    for event in events {
        let kind = event["type"].as_str().unwrap_or("");
        if kind == SERVER_ACL {
            acl = Some(&event["content"]);
            continue;
        }
        if kind == POLICY_SERVER {
            // Removal empties the content rather than deleting the event, so
            // an empty string is "no policy server" and not a server called
            // nothing.
            policy_server = event["content"]["via"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
            continue;
        }
        if let Some((_, about)) = RULE_TYPES.iter().find(|(name, _)| *name == kind) {
            // A rule is withdrawn by emptying its content, the same way every
            // other state event is - so one with no entity is a rule that has
            // been taken back rather than a rule against nobody.
            let Some(entity) = event["content"]["entity"].as_str().filter(|e| !e.is_empty()) else { continue };
            rules.push(serde_json::json!({
                "about": about,
                "entity": entity,
                "recommendation": event["content"]["recommendation"].as_str().unwrap_or("m.ban"),
                "reason": event["content"]["reason"].as_str().unwrap_or(""),
            }));
        }
    }

    // By what they are about and then by who, so the list reads the same way
    // twice - state comes back in no particular order.
    rules.sort_by(|a, b| {
        (a["about"].as_str(), a["entity"].as_str()).cmp(&(b["about"].as_str(), b["entity"].as_str()))
    });

    serde_json::json!({
        "serverAcl": {
            // Whether the room has one at all, which is not the same as one
            // that allows everything: "no ACL" and "an ACL saying *" behave
            // identically and mean different things to whoever reads it.
            "present": acl.is_some(),
            "allow": acl.map(|c| strings(&c["allow"])).unwrap_or_default(),
            "deny": acl.map(|c| strings(&c["deny"])).unwrap_or_default(),
            // Absent means allowed, per the spec - the field exists to say no.
            "allowIpLiterals": acl.and_then(|c| c["allow_ip_literals"].as_bool()).unwrap_or(true),
        },
        "policyServer": policy_server,
        "rules": rules,
    })
}

/// Everything above, for one buffer's room.
pub async fn room_policy(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/state",
        account.homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>()
    );
    let events = http::get_json(&url, &account.access_token).await.context("reading the room's state")?;
    let refs: Vec<&Value> = events.as_array().into_iter().flatten().collect();
    let mut answer = read(&refs);
    answer["canChange"] = serde_json::json!(moderation::can_send_state_in_buffer(state, account_id, buffer_id, SERVER_ACL));
    Ok(answer)
}

/// Adds or removes one server from a room's deny list.
///
/// One entry at a time and only the deny list. The allow list is the
/// dangerous half - a room whose allow list stops matching its own members'
/// server is a room nobody in it can speak in, and the state event that did
/// it can then no longer be changed - so this reads the current ACL, edits
/// only `deny`, and refuses outright to write one whose allow list is empty.
pub async fn set_denied(state: &AppState, account_id: &str, buffer_id: &str, server: &str, denied: bool) -> Result<()> {
    let server = server.trim();
    if server.is_empty() {
        anyhow::bail!("no server named");
    }
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let existing = roomsettings::read_state_event(state, account_id, &room_id, SERVER_ACL)
        .await?
        .unwrap_or_else(|| serde_json::json!({}));

    let mut allow = strings(&existing["allow"]);
    let mut deny = strings(&existing["deny"]);
    // A room with no ACL at all is one that allows everybody, and writing a
    // deny list without saying so would turn "everyone except this server"
    // into "nobody at all".
    if allow.is_empty() {
        allow.push("*".to_string());
    }

    deny.retain(|s| s != server);
    if denied {
        deny.push(server.to_string());
    }
    deny.sort();

    let content = serde_json::json!({
        "allow": allow,
        "deny": deny,
        "allow_ip_literals": existing["allow_ip_literals"].as_bool().unwrap_or(true),
    });
    send::put_room_state(state, account_id, &room_id, SERVER_ACL, "", content).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refs(events: &[Value]) -> Vec<&Value> {
        events.iter().collect()
    }

    #[test]
    fn a_rooms_acl_is_read_as_it_stands() {
        let events = vec![json!({
            "type": "m.room.server_acl",
            "content": { "allow": ["*"], "deny": ["evil.example"], "allow_ip_literals": false }
        })];
        let got = read(&refs(&events));
        assert_eq!(got["serverAcl"]["present"], true);
        assert_eq!(got["serverAcl"]["deny"], json!(["evil.example"]));
        assert_eq!(got["serverAcl"]["allowIpLiterals"], false);

        // No ACL is not the same as an ACL that allows everything: the two
        // behave identically and mean different things to whoever reads it.
        let bare = read(&[]);
        assert_eq!(bare["serverAcl"]["present"], false);
        // And absent means allowed, because the field exists to say no.
        assert_eq!(bare["serverAcl"]["allowIpLiterals"], true);
    }

    /// Most rooms that actually carry rules predate the stable event type
    /// and are full of the mjolnir prefix. A client reading only the stable
    /// one shows an empty list in exactly the rooms that have the most.
    #[test]
    fn both_spellings_of_a_policy_rule_are_read() {
        let events = vec![
            json!({ "type": "m.policy.rule.server", "content": { "entity": "spam.example", "recommendation": "m.ban", "reason": "spam" } }),
            json!({ "type": "org.matrix.mjolnir.rule.user", "content": { "entity": "@bad:example.org", "recommendation": "m.ban" } }),
            // Withdrawn by emptying the content, as every state event is.
            json!({ "type": "m.policy.rule.user", "content": {} }),
        ];
        let got = read(&refs(&events));
        let rules = got["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2, "{rules:?}");
        // Sorted, so the list reads the same way twice - state comes back in
        // no particular order.
        assert_eq!(rules[0]["about"], "server");
        assert_eq!(rules[0]["entity"], "spam.example");
        assert_eq!(rules[1]["about"], "user");
        // A rule that named no recommendation is a ban, which is what every
        // list means by one.
        assert_eq!(rules[1]["recommendation"], "m.ban");
    }

    #[test]
    fn a_policy_server_is_read_and_an_emptied_one_is_not() {
        let named = vec![json!({ "type": "m.room.policy", "content": { "via": "policy.example" } })];
        assert_eq!(read(&refs(&named))["policyServer"], "policy.example");

        let withdrawn = vec![json!({ "type": "m.room.policy", "content": {} })];
        assert!(read(&refs(&withdrawn))["policyServer"].is_null());
    }
}
