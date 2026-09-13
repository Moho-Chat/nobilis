//! The settings a room keeps about itself.
//!
//! Room state, read when somebody opens the panel rather than cached: these
//! change rarely and are looked at rarely, and a value read at the moment it
//! is shown cannot be stale. What is cached instead is the *permission* -
//! power levels arrive with every room anyway - so the panel knows whether to
//! offer a control before it has asked the server anything.

use super::*;

/// How much of a room a new member can read.
///
/// One of the few room settings with a privacy consequence rather than a
/// cosmetic one, and it was neither shown nor settable: a room created here
/// took whatever the server defaulted to, unseen.
pub const HISTORY_VISIBILITY: &str = "m.room.history_visibility";

/// The four the spec defines, in the order they widen.
///
/// A server may in principle store something else; an unknown value is shown
/// as itself rather than silently corrected, because rewriting a setting
/// nobody here understands is worse than admitting it.
pub const HISTORY_CHOICES: [&str; 4] = ["joined", "invited", "shared", "world_readable"];

/// Reads one piece of a room's state, or nothing when the room has none.
///
/// A 404 here is the ordinary answer rather than a failure: a room that has
/// never had its history visibility set does not carry the event, and the
/// spec's default applies. Every other error is a real one and is reported.
async fn read_state(state: &AppState, account_id: &str, room_id: &str, event_type: &str) -> Result<Option<Value>> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/state/{event_type}",
        account.homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>()
    );
    match http::get_json(&url, &account.access_token).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether an error is the server saying "there is no such state event".
///
/// Matched on the error code rather than the status, because that is what the
/// HTTP layer keeps: it turns a non-2xx into `M_NOT_FOUND: ...` and the
/// status is gone by the time anybody here sees it.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.to_string().contains("M_NOT_FOUND")
}

/// The room's published name, where it has one.
///
/// Best-effort by design: a room without an alias is ordinary, and so is one
/// whose alias this account cannot read. Both mean "use the id instead".
pub async fn canonical_alias(state: &AppState, account_id: &str, room_id: &str) -> Option<String> {
    read_state(state, account_id, room_id, "m.room.canonical_alias")
        .await
        .ok()
        .flatten()
        .and_then(|c| c["alias"].as_str().filter(|a| !a.is_empty()).map(str::to_string))
}

/// What a room says about who can read its past, and whether this account can
/// change it.
pub async fn history_visibility(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let content = read_state(state, account_id, &room_id, HISTORY_VISIBILITY).await?;
    // `shared` is the spec's default, and a room with no event really is
    // shared rather than unknown - saying "unset" would leave somebody
    // guessing about exactly the setting they came here to check.
    let value = content
        .as_ref()
        .and_then(|c| c["history_visibility"].as_str())
        .unwrap_or("shared")
        .to_string();
    Ok(serde_json::json!({
        "value": value,
        "choices": HISTORY_CHOICES,
        "canChange": moderation::can_send_state_in_buffer(state, account_id, buffer_id, HISTORY_VISIBILITY),
    }))
}

/// Sets it.
pub async fn set_history_visibility(state: &AppState, account_id: &str, buffer_id: &str, value: &str) -> Result<()> {
    if !HISTORY_CHOICES.contains(&value) {
        anyhow::bail!("{value} is not a history visibility this client sets");
    }
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    send::put_room_state(
        state,
        account_id,
        &room_id,
        HISTORY_VISIBILITY,
        "",
        serde_json::json!({ "history_visibility": value }),
    )
    .await
}

/// What room version this room is on, and whether it is worth moving.
///
/// An upgrade is not a setting: it makes a *new room* and leaves a tombstone
/// pointing at it, which is why it is offered as an action with its
/// consequences written out rather than as a control that silently rewrites
/// something. moho has followed a tombstone since spaces were supported and
/// could never make one, so the whole operation had to happen in another
/// client.
pub async fn room_version(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let current = state.runtime.matrix_room_version(account_id, &room_id).unwrap_or_default();
    let facts = state.runtime.matrix_server_facts(account_id).unwrap_or_default();
    let default = facts.default_room_version.clone().unwrap_or_default();
    Ok(serde_json::json!({
        "current": current,
        "default": default,
        "available": facts.room_versions,
        // Worth offering only where there is somewhere to go. A room already
        // on the server's default is not "upgradable to itself", and an
        // Upgrade button on one is a button whose only outcome is a new room
        // identical to the old one with everybody's scrollback left behind.
        "behind": !current.is_empty() && !default.is_empty() && current != default,
        // Making the new room is the server's job; making the tombstone in
        // the old one is this account's, and it is the part that can be
        // refused.
        "canUpgrade": moderation::can_send_state_in_buffer(state, account_id, buffer_id, "m.room.tombstone"),
    }))
}

/// Moves a room to a newer version.
///
/// The server makes the replacement room, copies the state it can, and
/// tombstones this one. What it cannot copy is the conversation: the old room
/// keeps its own history and members follow the tombstone as they notice it,
/// which is why this is worth saying out loud before it is done.
pub async fn upgrade_room(state: &AppState, account_id: &str, buffer_id: &str, version: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/upgrade",
        account.homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>()
    );
    let answer = http::post_json(&url, Some(&account.access_token), serde_json::json!({ "new_version": version }))
        .await
        .context("upgrading the room")?;
    let replacement = answer["replacement_room"].as_str().context("the server named no replacement room")?;

    // Joined here rather than left to be noticed. The server puts the
    // upgrading account in the new room in most deployments, but the sync
    // that says so can be seconds away - and an Upgrade that appears to do
    // nothing is worse than one that takes a moment.
    send::join_room(state, account_id, replacement, &[]).await?;
    Ok(serde_json::json!({ "roomId": replacement }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four values, spelled the way the spec spells them - a client that
    /// writes something else writes a setting no server will accept.
    #[test]
    fn the_choices_are_the_specs_four() {
        assert_eq!(HISTORY_CHOICES, ["joined", "invited", "shared", "world_readable"]);
        assert!(!HISTORY_CHOICES.contains(&"world-readable"));
        assert!(!HISTORY_CHOICES.contains(&"public"));
    }

    /// A 404 is the room saying it has never set this, which is an answer and
    /// not a failure. Told apart by the error code, because the HTTP layer
    /// has already turned the status into a message by the time this sees it.
    #[test]
    fn a_missing_state_event_is_not_an_error() {
        assert!(is_not_found(&anyhow::anyhow!("M_NOT_FOUND: Event not found.")));
        assert!(!is_not_found(&anyhow::anyhow!("M_FORBIDDEN: You are not in this room.")));
        assert!(!is_not_found(&anyhow::anyhow!("request failed")));
    }
}
