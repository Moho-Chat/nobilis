//! Reading backwards through a room, and sideways into a thread.

use super::*;

/// The first event in a room at or after a moment.
///
/// `/timestamp_to_event` is the whole of "jump to date": the server knows
/// where in a room a given day is, and a client that had to find out by
/// paging backwards would read a month of a busy room to reach the start of
/// it. Everything either side of this already exists here - the event it
/// names is handed to the same jump a search result takes.
///
/// Forwards by default, because a date means "that day" rather than
/// "whatever came before it": asking backwards from midnight on the 3rd lands
/// on the last message of the 2nd, which is the wrong day. Backwards is kept
/// for the case the spec exists to answer - a date after everything in the
/// room, where forwards finds nothing at all.
///
/// v1 rather than v3: it was added after the v3 client API was frozen, and
/// lives under /client/v1 on every server that has it.
pub async fn event_at(state: &AppState, account_id: &str, buffer_id: &str, ts_ms: i64, forwards: bool) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let url = format!(
        "{}/_matrix/client/v1/rooms/{}/timestamp_to_event?ts={ts_ms}&dir={}",
        account.homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        if forwards { "f" } else { "b" }
    );
    let resp = http::get_json(&url, &account.access_token).await.context("asking where that day is")?;
    let event_id = resp["event_id"].as_str().context("the server named no event there")?;
    Ok(serde_json::json!({
        "eventId": event_id,
        // Seconds, like every other timestamp this daemon hands out; the
        // endpoint answers in milliseconds.
        "ts": resp["origin_server_ts"].as_i64().map(|ms| ms / 1000),
    }))
}

/// Reads a thread from the server, then hands back everything known about it.
///
/// `/relations` is the only way to see a thread whole: its replies are
/// ordinary timeline events, so a room read backwards would find them only by
/// paging back far enough to have crossed all of them, and a thread that has
/// been quiet for a month is arbitrarily far back.
///
/// What comes back is read out of the store rather than built here, so a
/// thread shows the same messages in the same shape whether they arrived
/// live, in room history, or through this - and so anything already stored
/// keeps its edits and reactions instead of being replaced by a plainer copy.
pub async fn fetch_thread(state: &AppState, account_id: &str, buffer_id: &str, root_id: &str) -> Result<Vec<crate::model::Message>> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let root = url::form_urlencoded::byte_serialize(root_id.as_bytes()).collect::<String>();
    // v1, not v3: relations were added to the spec after the v3 client API was
    // frozen, and live under /client/v1 for every server that has them.
    let url = format!("{base}/_matrix/client/v1/rooms/{room}/relations/{root}/m.thread?dir=f&limit=100");

    match http::get_json(&url, &account.access_token).await {
        Ok(resp) => {
            let session = state.runtime.get_matrix_machine(account_id);
            for event in resp["chunk"].as_array().into_iter().flatten() {
                store_thread_event(state, account_id, buffer_id, &room_id, &account.user_id, session.as_deref(), event).await;
            }
        }
        // A thread nobody has added to, a server that does not implement
        // relations, or a root that has been redacted. What is already stored
        // is still worth showing, so this is not fatal.
        Err(e) => tracing::debug!("matrix: reading thread {root_id}: {e:#}"),
    }

    state.store.thread_messages(buffer_id, root_id).map_err(Into::into)
}

/// Fetches the conversation around one event and stores it.
///
/// What makes a pinned message or a search result reachable: both name a
/// message that may be years older than anything this client has, and paging
/// backwards to it would mean reading the whole room in between. The server
/// will hand over that one moment directly, so this asks for it - and stores
/// the messages either side of it too, because arriving at a line with no
/// conversation around it is arriving nowhere.
///
/// Returns when the message was sent, which is what a client needs to go and
/// read it out of the store.
/// Reads forward from an event, for a reader working back towards the present
/// from somewhere they jumped to.
///
/// Two requests rather than one: `/messages` pages from a token rather than
/// from an event, and the only place to get a token pointing at one
/// particular moment is `/context` for that event - which is what `end` is.
///
/// Returns how many messages were stored, so a caller can tell "here is more"
/// from "there is no more".
pub async fn load_newer(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str) -> Result<usize> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let event = url::form_urlencoded::byte_serialize(event_id.as_bytes()).collect::<String>();

    let anchored = http::get_json(&format!("{base}/_matrix/client/v3/rooms/{room}/context/{event}?limit=1"), &account.access_token)
        .await
        .context("finding that part of the room")?;
    let end = anchored["end"].as_str().context("the server gave no way to read forward from there")?;
    let from = url::form_urlencoded::byte_serialize(end.as_bytes()).collect::<String>();
    let resp = http::get_json(
        &format!("{base}/_matrix/client/v3/rooms/{room}/messages?dir=f&limit=50&from={from}"),
        &account.access_token,
    )
    .await
    .context("reading the rest of the room")?;

    let session = state.runtime.get_matrix_machine(account_id);
    let mut added = 0usize;
    // dir=f already reads oldest first, unlike the backward page.
    for event in resp["chunk"].as_array().cloned().unwrap_or_default() {
        if store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), &event).await {
            added += 1;
        }
    }
    Ok(added)
}

pub async fn load_context(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str) -> Result<i64> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let event = url::form_urlencoded::byte_serialize(event_id.as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/rooms/{room}/context/{event}?limit=30");
    let resp = http::get_json(&url, &account.access_token).await.context("reading that part of the room")?;

    let session = state.runtime.get_matrix_machine(account_id);
    // Oldest first, so the store reads in the order it was said: the events
    // before this one arrive newest-first from the server.
    let before: Vec<Value> = resp["events_before"].as_array().cloned().unwrap_or_default();
    for event in before.iter().rev() {
        store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), event).await;
    }
    let target = resp["event"].clone();
    store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), &target).await;
    for event in resp["events_after"].as_array().cloned().unwrap_or_default() {
        store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), &event).await;
    }

    // From the event itself rather than from the store: an event that would
    // not decrypt is not in the store, and the client still needs to know
    // where in the room it was to read what surrounds it.
    target["origin_server_ts"]
        .as_i64()
        .map(|ms| ms / 1000)
        .context("the server did not say when that message was sent")
}

pub async fn backfill(state: &AppState, account_id: &str, buffer_id: &str, limit: u32) -> Result<usize> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let Some(from) = state.runtime.matrix_back_token(account_id, &room_id) else { return Ok(0) };

    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let from_enc = url::form_urlencoded::byte_serialize(from.as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/rooms/{room}/messages?dir=b&limit={limit}&from={from_enc}");
    let resp = http::get_json(&url, &account.access_token).await?;

    // The sync loop's own session, not a second one: opening the crypto
    // store twice would mean two writers to the same SQLite file for no
    // gain. Absent only when the account is not connected, in which case
    // encrypted history is skipped rather than waited for.
    let session = state.runtime.get_matrix_machine(account_id);
    let mut added = 0usize;
    // dir=b returns newest first; stored oldest first so scrollback reads in
    // the order it was said.
    let chunk: Vec<Value> = resp["chunk"].as_array().cloned().unwrap_or_default();
    for event in chunk.iter().rev() {
        if store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), event).await {
            added += 1;
        }
    }

    // Where the next page starts. Absent means the room has been read to its
    // beginning, and the token is left alone so a later call does not loop
    // over the same page forever.
    if let Some(end) = resp["end"].as_str() {
        state.runtime.set_matrix_back_token(account_id, &room_id, end, false);
    }
    Ok(added)
}

/// Changes one of a room's own state events - its name or its topic.
///
/// Both are the same shape of call with a different type, and both are
/// refused by the server if this account's power level is too low, which is
/// The threads a room has, newest activity first.
///
/// A thread can be opened from its root message and continued, but a root
/// that has scrolled past is unreachable without this: the room knows its
/// threads and only the server can list them.
///
/// Each one carries what the panel needs to be worth opening - who started
/// it, how many replies it has, whether this account has said anything in it
/// - which is exactly what the server sends alongside the root event.
pub async fn list_threads(state: &AppState, account_id: &str, buffer_id: &str, limit: u32) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded_room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    // Client v1, not v3: threads arrived after v3 was frozen, the same way
    // /relations did.
    //
    // `include=all` is sent rather than left to the default, which the spec
    // says is that anyway: Conduit's deserializer treats the parameter as
    // required and answers M_BAD_JSON without it, so two of the four rooms
    // tested here could not list their threads at all. Synapse accepts it
    // either way, so saying it costs nothing and fixes a whole homeserver.
    let url = format!(
        "{base}/_matrix/client/v1/rooms/{encoded_room}/threads?include=all&limit={}",
        limit.clamp(1, 100)
    );
    let resp = http::get_json(&url, &account.access_token).await.context("listing threads")?;

    let session = state.runtime.get_matrix_machine(account_id);
    let room = ruma_common::RoomId::parse(&room_id).ok();
    let members = state.runtime.get_matrix_room_members(account_id, &room_id);
    let mut threads = Vec::new();
    for root in resp["chunk"].as_array().into_iter().flatten() {
        let Some(root_id) = root["event_id"].as_str() else { continue };
        let root = read_event(&session, room.as_deref(), root).await;
        let sender = root["sender"].as_str().unwrap_or_default();
        let relation = &root["unsigned"]["m.relations"]["m.thread"];
        // The most recent reply, which is what somebody scanning a list of
        // threads is actually looking at.
        let latest = read_event(&session, room.as_deref(), &relation["latest_event"]).await;
        let name_of = |user: &str| {
            members.get(user).cloned().unwrap_or_else(|| protocol::mxid_localpart(user))
        };
        threads.push(serde_json::json!({
            "rootId": root_id,
            "from": name_of(sender),
            "body": root["content"]["body"].as_str().unwrap_or("This message cannot be read here."),
            "ts": root["origin_server_ts"].as_i64().unwrap_or_default() / 1000,
            "replies": relation["count"].as_i64().unwrap_or(0),
            // Whether this account has said anything in it - Element sorts
            // its own list by this, and it is the difference between "a
            // thread happened" and "a thread you are in happened".
            "joined": relation["current_user_participated"].as_bool().unwrap_or(false),
            "lastFrom": latest["sender"].as_str().map(|u| name_of(u)),
            "lastBody": latest["content"]["body"].as_str(),
            "lastTs": latest["origin_server_ts"].as_i64().map(|ms| ms / 1000),
        }));
    }
    Ok(serde_json::json!({ "bufferId": buffer_id, "threads": threads, "next": resp["next_batch"].clone() }))
}
