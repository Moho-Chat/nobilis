//! A message's address, and what it actually says.
//!
//! The two things somebody reaches for when reporting a problem, and this
//! client had neither. A permalink is how a message is quoted to somebody who
//! is not in the room; the source is how a malformed event is described to
//! whoever can fix it. Without them a bug in a room can only be described in
//! prose - which is exactly the situation where prose is least use.
//!
//! Both are in Element's message menu, and both are read on demand: a link is
//! built from what the room already is, and the source is fetched from the
//! server, because what this client holds is what it *made* of the event
//! rather than the event.

use super::*;

/// A matrix.to link to one message.
///
/// By alias where the room has one, because an alias names a room to a person
/// - `#moho:poa.st` says what `!vjSZEo74ZIjCajgM:poa.st` cannot - and because
/// an alias carries its own server, so the link needs no routing hints.
///
/// Failing that, the room id and a `via`: a room id says nothing about who
/// has the room, so a client that has never met it cannot join or peek from
/// an id alone. The hints are servers we can name with certainty - the
/// domains in the room id and in this account's own user id - rather than a
/// guess at who else is in it.
pub async fn message_link(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;

    let alias = roomsettings::canonical_alias(state, account_id, &room_id).await;
    let url = match alias {
        Some(alias) => format!("https://matrix.to/#/{}/{}", escape(&alias), escape(event_id)),
        None => {
            let mut url = format!("https://matrix.to/#/{}/{}", escape(&room_id), escape(event_id));
            for (i, server) in via(&room_id, &account.user_id).iter().enumerate() {
                url.push(if i == 0 { '?' } else { '&' });
                url.push_str("via=");
                url.push_str(&escape(server));
            }
            url
        }
    };
    Ok(serde_json::json!({ "url": url }))
}

/// The servers worth naming in a link, most authoritative first.
///
/// Two at most, and both are facts rather than guesses: the room id's own
/// domain is the server the room was made on, and this account's is a server
/// known to be in it, since this account is. Element names the servers of the
/// room's most powerful members, which is better and needs the member list;
/// these two are enough to resolve a link and cannot be wrong.
fn via(room_id: &str, own_user_id: &str) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    for id in [room_id, own_user_id] {
        if let Some((_, server)) = id.split_once(':') {
            let server = server.to_string();
            if !server.is_empty() && !servers.contains(&server) {
                servers.push(server);
            }
        }
    }
    servers
}

fn escape(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// The event as the server holds it, and as this client can read it.
///
/// Two halves in an encrypted room, which is what Element shows and what is
/// actually useful: the envelope is what was sent and what any other client
/// would see, and the plaintext is what it turned out to say. In an
/// unencrypted room there is one of each and they are the same thing, so only
/// the one is given.
///
/// Fetched rather than taken from the timeline. What is held here is what
/// this client *made* of the event - a row with a body and a sender - and the
/// whole point of looking at the source is that what it made may be wrong.
pub async fn event_source(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/event/{}",
        account.homeserver_url.trim_end_matches('/'),
        escape(&room_id),
        escape(event_id)
    );
    let event = http::get_json(&url, &account.access_token).await.context("reading the event")?;

    let decrypted = if event["type"].as_str() == Some(protocol::EVENT_ROOM_ENCRYPTED) {
        let session = state.runtime.get_matrix_machine(account_id);
        let room = ruma_common::RoomId::parse(&room_id).ok();
        match (session, room) {
            (Some(session), Some(room)) => crypto::decrypt_room_event(&session, &event, &room).await.ok(),
            _ => None,
        }
    } else {
        None
    };

    Ok(serde_json::json!({
        "eventId": event_id,
        "roomId": room_id,
        "event": event,
        // Absent rather than null-and-equal when there is nothing to add: a
        // panel showing the same JSON twice teaches nobody anything.
        "decrypted": decrypted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A room id says nothing about who has the room, so a link built from
    /// one needs somewhere to ask. Both hints are facts - the server the room
    /// was made on, and a server known to be in it because we are on it.
    #[test]
    fn a_link_names_servers_it_can_be_sure_of() {
        assert_eq!(via("!abc:example.org", "@me:poa.st"), vec!["example.org", "poa.st"]);
        // Same server on both sides is one hint, not the same one twice.
        assert_eq!(via("!abc:poa.st", "@me:poa.st"), vec!["poa.st"]);
        // Nothing to split on is no hint rather than a broken one.
        assert!(via("nonsense", "alsononsense").is_empty());
    }

    /// A room id begins with `!` and a user id with `@`; neither survives a
    /// URL unescaped, and a link with a bare `!` in it is one that resolves
    /// to the wrong room or to none.
    #[test]
    fn ids_are_escaped_into_the_link() {
        assert_eq!(escape("!abc:example.org"), "%21abc%3Aexample.org");
        assert_eq!(escape("$event"), "%24event");
        assert_eq!(escape("#moho:poa.st"), "%23moho%3Apoa.st");
    }
}
