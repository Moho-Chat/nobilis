//! Looking into a room before deciding to be in it.
//!
//! There was no way. The only way to see what was in a room was to join it,
//! which is a membership event everybody in the room can see - so the
//! decision to look and the decision to join were the same act, and taking
//! the second one back leaves a trace of both.
//!
//! Two halves, because homeservers permit them separately:
//!
//! - **The summary.** Name, topic, how many people, how the room is entered,
//!   and whether its history is world-readable. Answered for any room the
//!   server can reach, joined or not, and enough to decide with.
//! - **The conversation.** Only where the room is world-readable *and* the
//!   homeserver allows peeking at all - many do not, Synapse's own default
//!   among them. Where it refuses, that is said plainly: silence there would
//!   read as an empty room rather than as a door that is shut.

use super::*;

/// What a room is, without being in it.
///
/// Tries the stable endpoint first and the MSC's unstable one after. The
/// module landed in v1.15 and the rooms worth looking into are on servers
/// that have had the unstable spelling for years, so a client that asks only
/// one of them asks the wrong one about half the time.
pub async fn summary(state: &AppState, account_id: &str, room: &str, via: &[String]) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let escaped = url::form_urlencoded::byte_serialize(room.as_bytes()).collect::<String>();
    let mut query = String::new();
    // Somewhere to ask. A room id names a room and says nothing about who has
    // it, so a summary of one this homeserver has never met needs routing -
    // exactly as joining it does.
    for server in via.iter().filter(|s| !s.trim().is_empty()) {
        query.push(if query.is_empty() { '?' } else { '&' });
        query.push_str("via=");
        query.push_str(&url::form_urlencoded::byte_serialize(server.trim().as_bytes()).collect::<String>());
    }

    let stable = format!("{base}/_matrix/client/v1/room_summary/{escaped}{query}");
    let unstable = format!("{base}/_matrix/client/unstable/im.nheko.summary/summary/{escaped}{query}");
    let answer = match http::get_json(&stable, &account.access_token).await {
        Ok(answer) => answer,
        Err(stable_err) => http::get_json(&unstable, &account.access_token)
            .await
            // The stable one's error is the one worth showing: it is what a
            // current server would have said, and the unstable path failing
            // as well usually means the same thing twice.
            .map_err(|_| stable_err)
            .context("asking what that room is")?,
    };

    Ok(serde_json::json!({
        "roomId": answer["room_id"].as_str().unwrap_or(room),
        "name": answer["name"].as_str().unwrap_or(""),
        "topic": answer["topic"].as_str().unwrap_or(""),
        "alias": answer["canonical_alias"].as_str().unwrap_or(""),
        "members": answer["num_joined_members"].as_u64().unwrap_or(0),
        "joinRule": answer["join_rule"].as_str().unwrap_or("public"),
        "membership": answer["membership"].as_str().unwrap_or("leave"),
        "encrypted": answer["encryption"].as_str().is_some(),
        // Whether there is any point offering to read it: a room that is not
        // world-readable will refuse, however willing the homeserver is.
        "worldReadable": answer["world_readable"].as_bool().unwrap_or(false),
        "avatarMxc": answer["avatar_url"].as_str().unwrap_or(""),
    }))
}

/// The last few things said in a room nobody here has joined.
///
/// Read backwards from the end, which is what "look inside" means - the last
/// page of a conversation, not its first.
pub async fn recent(state: &AppState, account_id: &str, room_id: &str, limit: u32) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/messages?dir=b&limit={limit}",
        account.homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>()
    );
    let answer = match http::get_json(&url, &account.access_token).await {
        Ok(answer) => answer,
        // Said plainly rather than passed through. A homeserver that will not
        // peek answers with a bare M_FORBIDDEN naming the room, which reads
        // as "you are banned" rather than as what it is - a setting on your
        // own server, not a judgement by the room's.
        Err(e) if e.to_string().contains("M_FORBIDDEN") => {
            anyhow::bail!("your homeserver does not allow looking into a room you have not joined")
        }
        Err(e) => return Err(e).context("reading the room"),
    };

    // Oldest first, because that is reading order; the endpoint answers in
    // the order it walked backwards.
    let mut lines: Vec<Value> = answer["chunk"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| protocol::event_type(event) == protocol::EVENT_ROOM_MESSAGE)
        .filter_map(|event| {
            let (body, is_action) = protocol::message_body(&event["content"]);
            if body.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "id": protocol::event_id(event).unwrap_or(""),
                "from": protocol::short_sender(event),
                "body": body,
                "isAction": is_action,
                "ts": event["origin_server_ts"].as_i64().map(|ms| ms / 1000).unwrap_or(0),
            }))
        })
        .collect();
    lines.reverse();
    Ok(serde_json::json!({ "messages": lines }))
}
