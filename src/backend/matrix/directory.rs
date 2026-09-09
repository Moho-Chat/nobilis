//! Finding things: messages inside a room, and rooms on a server.
//!
//! The public room directory is the part with the surprise in it - a server
//! only lists its own rooms, so searching "everywhere" means asking several
//! servers and merging what they say.

use super::*;

/// Searches the homeserver's own copy of the conversation.
///
/// The local scrollback is this window's copy of what it happened to be
/// present for; the server has everything the room has, including what was
/// said before this account joined and what this client has never downloaded.
/// Element searches both, and searching only one of them is why something
/// said last year in a room joined last week could not be found here.
///
/// One room or the whole account: a search worth doing is often "where did
/// somebody say that", and the room is exactly what the person has forgotten.
///
/// An encrypted room returns nothing from this, and honestly so - the server
/// holds ciphertext and cannot read it. That is not a failure to report as
/// one, and the caller is told the room is encrypted so it can say which kind
/// of nothing this is.
pub async fn search_messages(
    state: &AppState,
    account_id: &str,
    room_id: Option<&str>,
    term: &str,
    limit: u32,
) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let mut filter = serde_json::Map::new();
    filter.insert("limit".into(), Value::from(limit.clamp(1, 100)));
    if let Some(room_id) = room_id.filter(|r| !r.is_empty()) {
        filter.insert("rooms".into(), serde_json::json!([room_id]));
    }
    let body = serde_json::json!({
        "search_categories": {
            "room_events": {
                "search_term": term,
                // The message text, which is what somebody searching means.
                // Searching over topics and names as well turns "did anyone
                // mention the deploy" into a list of rooms with deploy in
                // their name.
                "keys": ["content.body"],
                "order_by": "recent",
                "filter": Value::Object(filter),
                // Names for the senders, so a result reads as a message from
                // a person rather than from an mxid.
                "event_context": { "before_limit": 0, "after_limit": 0, "include_profile": true },
            }
        }
    });

    let resp = http::post_json(&format!("{base}/_matrix/client/v3/search"), Some(&account.access_token), body)
        .await
        .context("searching the server")?;
    let events = resp["search_categories"]["room_events"]["results"].as_array().cloned().unwrap_or_default();

    let mut results = Vec::new();
    let mut room_names = serde_json::Map::new();
    for hit in events {
        let event = &hit["result"];
        let Some(event_id) = event["event_id"].as_str() else { continue };
        let Some(room) = event["room_id"].as_str() else { continue };
        let sender = event["sender"].as_str().unwrap_or_default();
        // The display name the server sent alongside the hit, where it did.
        let profile = hit["context"]["profile_info"][sender]["displayname"].as_str();
        let body = event["content"]["body"].as_str().unwrap_or_default();
        if body.is_empty() {
            continue;
        }
        let buffer_id = state.runtime.matrix_buffer_for_room(account_id, room).unwrap_or_default();
        if !buffer_id.is_empty() {
            if let Some((name, _kind)) = state.runtime.get_matrix_room_name(account_id, room) {
                room_names.insert(buffer_id.clone(), Value::from(name));
            }
        }
        results.push(serde_json::json!({
            "id": event_id,
            "bufferId": buffer_id,
            "from": profile.map(|name| name.to_string()).unwrap_or_else(|| protocol::mxid_localpart(sender)),
            "body": body,
            // Matrix counts in milliseconds and everything here counts in
            // seconds.
            "ts": event["origin_server_ts"].as_i64().unwrap_or_default() / 1000,
            "isAction": false,
            "isHighlight": false,
            "kind": "chat",
            "edited": false,
            "isOwn": sender == account.user_id,
        }));
    }

    Ok(serde_json::json!({
        "results": results,
        "roomNames": Value::Object(room_names),
        "count": resp["search_categories"]["room_events"]["count"].as_i64().unwrap_or(results.len() as i64),
    }))
}

/// Leaves a room, for real, on the server.
///
/// Closing a conversation used to remove the buffer and nothing else, so it
/// came back on the next sync and the account was still in the room as far as
/// everyone else in it was concerned - the client had hidden it rather than
/// left it. Also forgets the room afterwards: leaving alone keeps it in the
/// account's `rooms.leave` forever, which every client shows as a room you
/// have left rather than one that is gone. Forgetting is best-effort, since a
/// server may refuse it and the leave is the part that matters.
/// Reads a page of older history and writes it into local scrollback.
///
/// Before this there was no `/messages` call at all, so scrollback was only
/// ever what the daemon had watched happen live: joining a room with years
/// behind it showed nothing above the first sync.
///
/// Returns how many messages were added. Zero means the room has been read
/// back to its beginning, or as far as the server will serve.
///
/// Written straight to the store rather than through record_message: these
/// are old, and the notification path would announce every one of them.
/// Searches the public room directories, the way Element's room explorer does
/// - except across every homeserver at once rather than one at a time.
///
/// A directory is per homeserver: ours lists what our server has been told
/// about, and finding a room on a server we have never spoken to means asking
/// that server directly. Element makes you pick one from a dropdown and shows
/// its results alone, so finding something means knowing where to look first.
/// Here every server is asked together and the answers become one list, which
/// is what somebody searching for a room by name actually wants.
///
/// Which servers: ours, wherever this account already has rooms, and anything
/// the caller names. The middle one is what makes this useful without being
/// told anything - the servers somebody's rooms are on are the servers their
/// community lives on.
///
/// One server being slow, dead, or refusing federation does not fail the
/// search: each is a separate request and the answers are merged from
/// whichever came back, because a partial list is worth having and a failed
/// search is not.
pub async fn search_public_rooms(
    state: &AppState,
    account_id: &str,
    query: &str,
    servers: &[String],
    since: &serde_json::Map<String, Value>,
    limit: u32,
) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/').to_string();
    let token = account.access_token.clone();

    // Paging asks only the servers that gave us a place to continue from.
    // Asking them all would hand a server with no token its *first* page
    // again, so "show more" would append rooms already on screen - which is
    // exactly what it did.
    let targets: Vec<String> = if since.is_empty() {
        directory_targets(state, account_id, &account.user_id, &base, &token, servers).await
    } else {
        since.keys().cloned().collect()
    };
    let requests = targets.iter().map(|server| {
        let base = base.clone();
        let token = token.clone();
        let since = since.get(server).and_then(|v| v.as_str()).unwrap_or("").to_string();
        async move { (server.clone(), directory_page(&base, &token, server, query, &since, limit).await) }
    });
    let answers = futures::future::join_all(requests).await;

    let joined = state.runtime.matrix_joined_rooms(account_id);
    // Our own server has no name in a request - the spec spells "ask locally"
    // as the absence of the parameter - but it very much has one to a reader,
    // and a list of servers with a blank in it says less than nothing.
    let own_server = account.user_id.rsplit(':').next().unwrap_or("").to_string();
    let named = |server: &str| if server.is_empty() { own_server.clone() } else { server.to_string() };

    let mut rooms: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut next = serde_json::Map::new();
    let mut answered: Vec<Value> = Vec::new();

    for (server, answer) in answers {
        let resp = match answer {
            Ok(resp) => resp,
            // Why, not just that: "did not answer" covers a server that is
            // down, one that will not federate its directory, and one that
            // rate-limited us, and those are three different things to do
            // next about it.
            Err(e) => {
                answered.push(serde_json::json!({ "server": named(&server), "error": format!("{e:#}"), "rooms": 0 }));
                continue;
            }
        };
        if let Some(token) = resp["next_batch"].as_str() {
            next.insert(server.clone(), Value::String(token.to_string()));
        }
        let before = rooms.len();
        for room in resp["chunk"].as_array().into_iter().flatten() {
            let room_id = room["room_id"].as_str().unwrap_or("").to_string();
            // The same room is listed by every server that knows it. Whoever
            // answered first keeps it, which is arbitrary and does not matter:
            // the entries describe one room and differ only in how stale each
            // server's copy of its member count is.
            if room_id.is_empty() || !seen.insert(room_id.clone()) {
                continue;
            }
            rooms.push(serde_json::json!({
                "roomId": room_id,
                "name": room["name"].as_str().unwrap_or(""),
                "alias": room["canonical_alias"].as_str().unwrap_or(""),
                "topic": room["topic"].as_str().unwrap_or(""),
                "members": room["num_joined_members"].as_i64().unwrap_or(0),
                // Deliberately no icon. A directory hands back `mxc://` URIs,
                // which name media on a homeserver and are not URLs anything
                // can load - fetching one needs the access token. Passing them
                // through drew a broken image against every room that had an
                // icon and the coloured initial only against the rooms that
                // had none, which is precisely the wrong way round.
                //
                // Downloading them here was tried and measured: fifty rooms
                // spread across as many media servers did not finish inside a
                // six second budget, so a search that was fast became one that
                // was slow *and* still showed no icons. A room's initial costs
                // nothing and is what every room without an icon shows anyway.
                // Which directory answered. Shown, because in one merged list
                // the server a room lives on is the thing that says what kind
                // of place it is - and it is the routing hint a room with no
                // published alias needs to be joined at all.
                "via": named(&server),
                "joined": joined.contains(&room_id),
                // How to get in. Most rooms in a directory are "public" and
                // are simply joined; a "knock" room has to be asked, and a
                // "restricted" one is open to members of a space this account
                // may not be in. A client that shows one Join button for all
                // three offers a button that fails for two of them.
                "joinRule": room["join_rule"].as_str().unwrap_or("public"),
            }));
        }
        // What this server actually contributed, after the rooms every other
        // server had already listed were dropped. That is the honest number:
        // a server whose whole page was rooms somebody else had listed added
        // nothing to what is on screen, however many it returned.
        answered.push(serde_json::json!({ "server": named(&server), "rooms": rooms.len() - before }));
    }

    // Busiest first, across all of them. Each server returns its own list in
    // its own order, so concatenating without this would sort by which server
    // happened to answer rather than by anything about the rooms.
    rooms.sort_by(|a, b| b["members"].as_i64().unwrap_or(0).cmp(&a["members"].as_i64().unwrap_or(0)));

    // By name, which does not change. Sorting by what each contributed put
    // them in a different order after every search, so a switch somebody was
    // reaching for moved as they reached - and the count beside it is what
    // says which gave the most anyway.
    answered.sort_by(|a, b| a["server"].as_str().unwrap_or("").cmp(b["server"].as_str().unwrap_or("")));

    Ok(serde_json::json!({
        "rooms": rooms,
        "next": next,
        // One entry per homeserver asked, named, with what it contributed or
        // why it contributed nothing. A count of servers said none of this,
        // and "3 homeservers" is not something anybody can check.
        "servers": answered,
    }))
}

/// One homeserver's directory page, or an error that only costs that server.
pub(super) async fn directory_page(base: &str, token: &str, server: &str, query: &str, since: &str, limit: u32) -> Result<Value> {
    let mut url = format!("{base}/_matrix/client/v3/publicRooms");
    if !server.is_empty() {
        url.push_str("?server=");
        url.push_str(&url::form_urlencoded::byte_serialize(server.as_bytes()).collect::<String>());
    }
    let mut body = serde_json::json!({ "limit": limit });
    if !query.trim().is_empty() {
        body["filter"] = serde_json::json!({ "generic_search_term": query.trim() });
    }
    if !since.is_empty() {
        body["since"] = Value::String(since.to_string());
    }
    // POST rather than GET: only the POST form takes a search term at all, and
    // the GET form's absence of one is why a directory browser without this
    // could only ever show the first page of the whole server.
    http::post_json(&url, Some(token), body).await.context("searching the room directory")
}

/// Which homeservers to ask: ours, the ones this account has rooms on, and
/// whatever the caller added - deduplicated, and with our own written as the
/// empty string because that is how the spec spells "no server parameter, ask
/// locally".
pub(super) async fn directory_targets(
    state: &AppState,
    account_id: &str,
    own_user_id: &str,
    base: &str,
    access_token: &str,
    extra: &[String],
) -> Vec<String> {
    let mut targets: Vec<String> = vec![String::new()];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Our own server by name as well as locally would ask the same directory
    // twice and list every one of its rooms twice with it.
    if let Some(own) = own_user_id.rsplit(':').next() {
        seen.insert(own.to_string());
    }

    // Asked of the server rather than read off the buffers this window has
    // built, because those appear over the first minute of a session: a
    // search run before a room's buffer existed silently left that room's
    // homeserver out, and the results looked like the server had nothing.
    let mut from_rooms: Vec<String> = state.runtime.matrix_joined_rooms(account_id).into_iter().collect();
    if let Ok(resp) = http::get_json(&format!("{base}/_matrix/client/v3/joined_rooms"), access_token).await {
        from_rooms.extend(resp["joined_rooms"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)));
    }

    let named = from_rooms.iter().filter_map(|id| server_of_room(id).map(str::to_string));
    for server in named.chain(extra.iter().map(|s| server_name(s))) {
        if !server.is_empty() && seen.insert(server.clone()) {
            targets.push(server);
        }
    }
    targets
}

/// The homeserver named inside a room id, where there is one.
///
/// Room ids used to be `!opaque:server.example` and this could be taken as
/// "everything after the colon". Room version 12 ids are the hash of the
/// create event and carry no server at all - `!phpQp7HD_h1IuRlU61Vop1...` and
/// nothing else - so splitting on a colon that is not there returned the whole
/// room id as if it were a hostname. That was then asked to search its own
/// directory, and the server answered M_BAD_JSON, which is exactly what it
/// should say about `?server=!phpQp7HD...`.
///
/// A room whose id names no server is not a lost cause elsewhere - it is
/// reachable through the people in it - but it has nothing to contribute to a
/// list of directories to search, so it is skipped here.
pub(super) fn server_of_room(room_id: &str) -> Option<&str> {
    let (_, server) = room_id.split_once(':')?;
    // A hostname, not merely "text after a colon": the point is to catch
    // anything that would be sent as ?server= and be nonsense there.
    let plausible = !server.is_empty()
        && server.contains('.')
        && server.chars().all(|c| c.is_ascii_alphanumeric() || "-._:[]".contains(c));
    plausible.then_some(server)
}

/// A homeserver's name, from whatever somebody typed. A bare hostname, not a
/// URL: this names a server in the Matrix sense, and one typed with a scheme
/// would be rejected by ours.
pub(super) fn server_name(typed: &str) -> String {
    typed.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string()
}

#[cfg(test)]
mod directory_tests {
    use super::{server_name, server_of_room};

    /// Room version 12 ids are a hash and nothing else. Reading a server out
    /// of one gave the whole room id as a hostname, which was then asked to
    /// search its own directory and answered M_BAD_JSON - a real error in the
    /// server list, against a "server" that was a room.
    #[test]
    fn a_room_id_only_names_a_server_when_it_has_one() {
        assert_eq!(server_of_room("!abc:matrix.org"), Some("matrix.org"));
        assert_eq!(server_of_room("!phpQp7HD_h1IuRlU61Vop1-1RL5GxApK3Foo8E6KHxM"), None);
        assert_eq!(server_of_room("!abc:"), None);
        // A hostname has a dot in it; "localhost" is not something to go
        // asking a public directory of.
        assert_eq!(server_of_room("!abc:localhost"), None);
        assert_eq!(server_of_room("!abc:matrix.example.com:8448"), Some("matrix.example.com:8448"));
    }

    #[test]
    fn a_server_is_named_however_somebody_typed_it() {
        assert_eq!(server_name("matrix.org"), "matrix.org");
        assert_eq!(server_name("  https://matrix.org/  "), "matrix.org");
        assert_eq!(server_name("http://glowers.club"), "glowers.club");
        assert_eq!(server_name(""), "");
    }
}
