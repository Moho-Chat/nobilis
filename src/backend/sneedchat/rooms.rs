//! Which rooms exist, what they are called, and who is in them.
//!
//! The catalogue is the chat's own `rooms/heads?kind=listed`; who is in a
//! room arrives as `presence` frames on the socket.

use super::*;
use protocol::Principal;

/// Who somebody is, on Sneedchat: the name and picture the chat shows for
/// them. The chat knows people by principal, not by forum account, so there
/// is no profile page to point at.
pub fn profile(state: &AppState, account_id: &str, buffer_id: &str, username: &str) -> serde_json::Value {
    let mut profile = crate::profile::pending("sneedchat", account_id, username);
    profile["pending"] = serde_json::json!(false);
    if let Some(members) = state.runtime.get_presence(buffer_id) {
        if let Some(member) = members
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["nick"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(username)))
        {
            if let Some(id) = member["userId"].as_str() {
                profile["id"] = serde_json::json!(id);
            }
            if let Some(avatar) = member["avatarUrl"].as_str() {
                profile["avatarUrl"] = serde_json::json!(avatar);
            }
        }
    }
    profile
}

/// Reads the catalogue in the background and tells everybody when it arrives.
///
/// Not returned from the RPC that asks for it: it is read through the
/// account's own chat session, over the web helper, and the daemon answers
/// one request at a time per client.
pub fn refresh_rooms(state: AppState, account_id: String) {
    tokio::spawn(async move {
        match list_rooms(&state, &account_id).await {
            Ok(rooms) => publish_catalogue(&state, &account_id, &rooms),
            // Not surfaced: the client already has a list to show, and a
            // failure here means it keeps showing it.
            Err(e) => tracing::debug!("sneedchat[{account_id}]: reading the room list: {e:#}"),
        }
    });
}

/// The rooms the chat lists, asked through the account's connection - the
/// list needs a chat session, which only a connected account has.
pub async fn list_rooms(_state: &AppState, account_id: &str) -> Result<Vec<SneedChatRoom>> {
    let chat = super::chat::chat(account_id);
    let session = chat.session.lock().unwrap().clone().ok_or_else(|| anyhow!("connect the account to see its rooms"))?;
    let host = chat.host.lock().unwrap().clone();
    read_catalogue(&session, &host).await
}

/// The listed rooms, as the account names them.
pub(super) async fn read_catalogue(session: &Session, host: &str) -> Result<Vec<SneedChatRoom>> {
    let heads = super::chat::api_get(session, host, "rooms/heads?kind=listed").await?;
    let rooms: Vec<SneedChatRoom> = protocol::parse_heads(&heads.to_string()).into_iter().map(|h| SneedChatRoom { id: h.uuid, name: h.slug }).collect();
    if rooms.is_empty() {
        bail!("the chat listed no rooms");
    }
    Ok(rooms)
}

/// Keeps the catalogue and tells the window, which draws the Join page's list
/// from it.
pub(super) fn publish_catalogue(state: &AppState, account_id: &str, rooms: &[SneedChatRoom]) {
    state.runtime.set_sneedchat_room_catalogue(account_id, rooms.to_vec());
    state.events.emit(
        "sneedchatRooms",
        serde_json::json!({
            "accountId": account_id,
            "rooms": rooms.iter().map(|r| serde_json::json!({ "id": r.id, "name": r.name })).collect::<Vec<_>>(),
        }),
    );
}

/// The account's rooms, made right against the chat's own list once a
/// connection is admitted: rooms saved under the old chat's numbers are moved
/// to the uuid of the room with the same name, and an account that has never
/// had a room is given one as a courtesy.
pub(super) async fn settle_rooms(state: &AppState, account_id: &str, session: &Session, host: &str) {
    let catalogue = match read_catalogue(session, host).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("sneedchat[{account_id}]: reading the room list: {e:#}");
            return;
        }
    };
    publish_catalogue(state, account_id, &catalogue);
    let Some(config) = state.accounts.get_sneedchat(account_id) else { return };
    let (rooms, changed) = adopt_uuids(&config.rooms, &catalogue);
    if changed {
        tracing::info!("sneedchat[{account_id}]: rooms moved to the new chat: {:?}", rooms.iter().map(|r| &r.name).collect::<Vec<_>>());
        let _ = state.accounts.set_sneedchat_rooms(account_id, rooms);
    } else if !config.rooms_chosen && config.rooms.is_empty() {
        if let Some(room) = courtesy_room(&catalogue) {
            tracing::info!("sneedchat[{account_id}]: opening #{} for a new account", room.name);
            let _ = state.accounts.set_sneedchat_rooms(account_id, vec![room]);
        }
    }
}

/// Saved rooms against the catalogue: any not yet named by uuid takes the
/// uuid of the listed room with its name, and one the chat no longer has is
/// dropped. Whether anything changed.
pub(super) fn adopt_uuids(saved: &[SneedChatRoom], catalogue: &[SneedChatRoom]) -> (Vec<SneedChatRoom>, bool) {
    let mut changed = false;
    let mut out: Vec<SneedChatRoom> = Vec::new();
    for room in saved {
        if protocol::is_uuid(&room.id) {
            out.push(room.clone());
            continue;
        }
        changed = true;
        match catalogue.iter().find(|c| c.name == room.name) {
            Some(found) if !out.iter().any(|r| r.id == found.id) => out.push(found.clone()),
            _ => {}
        }
    }
    (out, changed)
}

/// The room a new account is opened in, as a courtesy: the one the site
/// calls general, or else the first it lists.
pub(super) fn courtesy_room(catalogue: &[SneedChatRoom]) -> Option<SneedChatRoom> {
    catalogue.iter().find(|r| r.name == "general").or_else(|| catalogue.first()).cloned()
}

/// The site's own display name reduced to the form this project has always
/// used for a room - lower case, words joined by hyphens, punctuation gone.
/// What the chat calls a room's slug is the same thing.
pub(super) fn slug(label: &str) -> String {
    let mut out = String::new();
    for ch in label.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// What a room's buffer is called. The `#` is part of the name every other
/// part of this daemon and every frontend knows the room by.
pub(super) fn room_buffer_name(room_name: &str) -> String {
    format!("#{room_name}")
}

/// And the room behind a buffer's name - the same pairing read backwards.
pub(super) fn room_name_of(buffer_name: &str) -> &str {
    buffer_name.strip_prefix('#').unwrap_or(buffer_name)
}

/// Publishes who is in a room, as a `presence` frame lists them - the whole
/// list each time.
pub(super) fn publish_presence(state: &AppState, http: &super::http::HttpClient, host: &str, account_id: &str, buffer_name: &str, principals: &[Principal]) {
    let buffer_id = crate::model::buffer_id(account_id, buffer_name);
    let before = published_roster(state, &buffer_id);
    let by_id: std::collections::BTreeMap<String, (String, Option<String>)> = principals
        .iter()
        .map(|p| {
            // Whatever picture they already had stays until a new one is fetched.
            let known = before.get(&p.uuid).and_then(|(_, a)| a.clone());
            (p.uuid.clone(), (p.name.clone(), known))
        })
        .collect();
    publish_roster(state, &buffer_id, &by_id);

    // The pictures, after the list is already on screen: fetching one is slow
    // the first time, and neither the list nor the room should wait for it.
    let wanted: Vec<(String, String)> = principals
        .iter()
        .filter(|p| before.get(&p.uuid).and_then(|(_, a)| a.as_ref()).is_none())
        .filter_map(|p| Some((p.uuid.clone(), super::chat::mirrored(p.avatar.as_deref()?, host))))
        .collect();
    if !wanted.is_empty() {
        tokio::spawn({
            let (state, http, host) = (state.clone(), http.clone(), host.to_string());
            async move { add_pictures(&state, &http, &host, &buffer_id, wanted).await }
        });
    }
}

/// The roster as last published: who is there, and the picture each has.
fn published_roster(state: &AppState, buffer_id: &str) -> std::collections::BTreeMap<String, (String, Option<String>)> {
    state
        .runtime
        .get_presence(buffer_id)
        .and_then(|v| serde_json::from_value::<Vec<serde_json::Value>>(v).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let id = m.get("userId")?.as_str()?.to_string();
            let nick = m.get("nick")?.as_str()?.to_string();
            let avatar = m.get("avatarUrl").and_then(|a| a.as_str()).map(str::to_string);
            Some((id, (nick, avatar)))
        })
        .collect()
}

/// Fetches pictures and puts them on whoever is still in the room.
async fn add_pictures(state: &AppState, http: &super::http::HttpClient, host: &str, buffer_id: &str, wanted: Vec<(String, String)>) {
    let found = futures::future::join_all(wanted.iter().map(|(id, raw)| async move { (id.clone(), super::media::cached_avatar_path(http, host, id, raw).await) })).await;
    // Read afresh: people came and went while these were being fetched, and a
    // roster built from before would bring back whoever has left.
    let mut now = published_roster(state, buffer_id);
    let mut changed = false;
    for (id, path) in found {
        if let (Some(path), Some(entry)) = (path, now.get_mut(&id)) {
            if entry.1.as_deref() != Some(path.as_str()) {
                entry.1 = Some(path);
                changed = true;
            }
        }
    }
    if changed {
        publish_roster(state, buffer_id, &now);
    }
}

/// Orders and publishes a roster: sorted here rather than left to the client,
/// since every other backend hands over a sorted one and a five-hundred-name
/// list in map order would be unreadable.
fn publish_roster(state: &AppState, buffer_id: &str, by_id: &std::collections::BTreeMap<String, (String, Option<String>)>) {
    let mut members: Vec<(&String, &(String, Option<String>))> = by_id.iter().collect();
    members.sort_by(|(_, (a, _)), (_, (b, _))| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
    let member_list = serde_json::json!(members
        .into_iter()
        .map(|(id, (nick, avatar))| serde_json::json!({ "nick": nick, "userId": id, "prefix": "", "away": false, "avatarUrl": avatar }))
        .collect::<Vec<_>>());
    // Persisted as well as broadcast, so a client that subscribes later gets
    // the roster from subscribe's replay rather than waiting for the next one.
    state.runtime.set_presence(buffer_id, member_list.clone());
    state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer_id, "members": member_list }));
}

#[cfg(test)]
mod tests {
    use super::*;

    const FISH: &str = "01a1f0f8-2d29-7ac3-b779-b3b0904d6dd6";
    const GUNT: &str = "01a1f0f8-2d21-782a-b666-f218122ea33b";

    fn room(id: &str, name: &str) -> SneedChatRoom {
        SneedChatRoom { id: id.into(), name: name.into() }
    }

    #[test]
    fn the_sites_names_become_the_names_this_project_already_used() {
        assert_eq!(slug("Gunt"), "gunt");
        assert_eq!(slug("Fishtank"), "fishtank");
        assert_eq!(slug("Beauty Parlor"), "beauty-parlor");
        assert_eq!(slug("SPORTS!!"), "sports");
        assert_eq!(slug("Lolcows"), "lolcows");
    }

    #[test]
    fn old_room_numbers_become_the_uuids_of_the_same_rooms() {
        let catalogue = [room(FISH, "fishtank"), room(GUNT, "gunt")];
        // Fishtank was 16 on the old chat; "general" exists no more.
        let (rooms, changed) = adopt_uuids(&[room("16", "fishtank"), room("1", "general")], &catalogue);
        assert!(changed);
        assert_eq!(rooms, vec![room(FISH, "fishtank")]);
        // Already moved: nothing to do.
        let (rooms, changed) = adopt_uuids(&[room(FISH, "fishtank")], &catalogue);
        assert!(!changed);
        assert_eq!(rooms.len(), 1);
        // Two old entries for the same room make one.
        let (rooms, _) = adopt_uuids(&[room("16", "fishtank"), room("99", "fishtank")], &catalogue);
        assert_eq!(rooms.len(), 1);
    }

    #[test]
    fn a_new_account_opens_the_room_the_site_calls_general_or_its_first() {
        assert_eq!(courtesy_room(&[room(FISH, "fishtank"), room("x", "general")]), Some(room("x", "general")));
        assert_eq!(courtesy_room(&[room(FISH, "fishtank"), room(GUNT, "gunt")]), Some(room(FISH, "fishtank")));
        assert_eq!(courtesy_room(&[]), None);
    }

    #[test]
    fn a_room_and_its_buffer_name_are_one_pairing() {
        assert_eq!(room_buffer_name("fishtank"), "#fishtank");
        assert_eq!(room_name_of(&room_buffer_name("fishtank")), "fishtank");
        assert_eq!(room_name_of("fishtank"), "fishtank");
    }

    #[tokio::test]
    async fn presence_is_the_whole_list_sorted() {
        let state = crate::backend::kick::testkit::simulated_daemon("sneedchat-presence");
        let http = super::super::http::HttpClient::new(crate::net::tor::Transport::Direct, super::super::http::CookieJar::new(), "t".into());
        let p = |id: &str, name: &str| Principal { uuid: id.into(), name: name.into(), ..Default::default() };
        publish_presence(&state, &http, "example.invalid", "sneedchat:t", "#fishtank", &[p("2", "bob"), p("1", "Amy")]);
        let list = state.runtime.get_presence(&crate::model::buffer_id("sneedchat:t", "#fishtank")).unwrap();
        let names: Vec<&str> = list.as_array().unwrap().iter().map(|m| m["nick"].as_str().unwrap()).collect();
        assert_eq!(names, ["Amy", "bob"]);
        publish_presence(&state, &http, "example.invalid", "sneedchat:t", "#fishtank", &[p("2", "bob")]);
        let list = state.runtime.get_presence(&crate::model::buffer_id("sneedchat:t", "#fishtank")).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
    }
}
