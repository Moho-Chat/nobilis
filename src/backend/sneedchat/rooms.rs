//! Which rooms exist, what they are called, and who is in them.
//!
//! The catalogue is read off the chat page rather than from an endpoint,
//! because the site has no endpoint that lists rooms - a fact worth knowing
//! before wondering why this parses HTML.

use super::*;

/// Who somebody is, on Sneedchat.
///
/// The chat protocol carries a user id, a name and a picture and nothing
/// else - no join date, no rank, no last seen. What it does carry is the id,
/// and the id is a link to the forum profile where all of that lives, so that
/// is what this offers rather than inventing the rest.
pub fn profile(state: &AppState, account_id: &str, buffer_id: &str, username: &str) -> serde_json::Value {
    let mut profile = crate::profile::pending("sneedchat", account_id, username);
    profile["pending"] = serde_json::json!(false);

    // The roster the room already has: their id and picture are in it.
    if let Some(members) = state.runtime.get_presence(buffer_id) {
        if let Some(member) = members
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["nick"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(username)))
        {
            if let Some(id) = member["userId"].as_str() {
                profile["id"] = serde_json::json!(id);
                if let Some(config) = state.accounts.get_sneedchat(account_id) {
                    crate::profile::note(&mut profile, "Profile", format!("https://{}/members/{id}", config.host));
                }
            }
            if let Some(avatar) = member["avatarUrl"].as_str() {
                profile["avatarUrl"] = serde_json::json!(avatar);
            }
        }
    }
    profile
}

/// Which rooms the site has, asked of the site.
///
/// The catalogue was six rooms written into the frontend, with a comment
/// saying no endpoint lists them. There is one - it is simply not an API: the
/// chat page the browser loads carries the room switcher as ordinary markup,
/// `<a class="chat-room" data-id="20">Lolcows</a>`, and that is the same list
/// a person sees down the side of the site.
///
/// Unauthenticated on purpose. The list is the same for everybody and the
/// gate is the only thing in the way, so this needs no account session - which
/// means the room list can be refreshed while an account is signed out or
/// failing to sign in.
/// Reads the catalogue in the background and tells everybody when it arrives.
///
/// Not returned from the RPC that asks for it, because reading it costs a Tor
/// round trip through a proof-of-work gate - about fifteen seconds - and the
/// daemon answers one request at a time per client. A blocking answer here
/// froze the whole window for the duration, which is a far worse thing than a
/// room list that fills in a moment later.
pub fn refresh_rooms(state: AppState, account_id: String) {
    tokio::spawn(async move {
        match list_rooms(&state, &account_id).await {
            Ok(rooms) => {
                state.runtime.set_sneedchat_room_catalogue(&account_id, rooms.clone());
                state.events.emit(
                    "sneedchatRooms",
                    serde_json::json!({
                        "accountId": account_id,
                        "rooms": rooms.iter().map(|r| serde_json::json!({ "id": r.id, "name": r.name })).collect::<Vec<_>>(),
                    }),
                );
            }
            // Not surfaced: the client already has a list to show, and a
            // failure here means it keeps showing it.
            Err(e) => tracing::debug!("sneedchat[{account_id}]: reading the room list: {e:#}"),
        }
    });
}

pub async fn list_rooms(state: &AppState, account_id: &str) -> Result<Vec<SneedChatRoom>> {
    let config = state.accounts.get_sneedchat(account_id).ok_or_else(|| anyhow!("no such account"))?;
    let transport = build_transport(state, &config, account_id).await?;
    let session = Session::new(transport, format!("https://{}", config.host), DEFAULT_USER_AGENT.to_string());
    restore_session(&session, &config);

    let url = format!("https://{}/test-chat", config.host);
    let resp = session.fetch(&url).await.context("fetching the chat page")?;
    if !(200..400).contains(&resp.status) {
        bail!("the chat page answered HTTP {}", resp.status);
    }
    let rooms = parse_rooms(&resp.body);
    if rooms.is_empty() {
        bail!("the chat page listed no rooms - its markup has probably changed");
    }
    Ok(rooms)
}

/// The room switcher, out of the chat page's markup.
///
/// Deliberately a scan for the two things that matter rather than an HTML
/// parse: `data-id` and the text after it. The page is generated markup and
/// its shape will change; a scan that finds nothing is a room list that could
/// not be read, which the caller reports, and not a panic or silent empty.
pub(super) fn parse_rooms(html: &str) -> Vec<SneedChatRoom> {
    let mut rooms = Vec::new();
    for chunk in html.split("class=\"chat-room\"").skip(1) {
        let Some(id) = chunk.split("data-id=\"").nth(1).and_then(|rest| rest.split('"').next()) else { continue };
        let Ok(id) = id.parse::<u32>() else { continue };
        // The link's own text, which is the room's name as the site writes it.
        let Some(label) = chunk.split_once('>').map(|(_, rest)| rest).and_then(|rest| rest.split('<').next()) else {
            continue;
        };
        let name = slug(label);
        if !name.is_empty() && !rooms.iter().any(|r: &SneedChatRoom| r.id == id) {
            rooms.push(SneedChatRoom { id, name });
        }
    }
    rooms
}

/// The site's own display name reduced to the form this project has always
/// used for a room - lower case, words joined by hyphens, punctuation gone.
///
/// Not a new convention: run over the rooms the catalogue already listed it
/// reproduces every one of them exactly ("Beauty Parlor" -> beauty-parlor,
/// "SPORTS!!" -> sports), which is what says the rule is the right one rather
/// than merely a plausible one.
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

#[cfg(test)]
pub(super) mod room_list_tests {
    use super::{parse_rooms, slug};

    /// The six rooms the catalogue held were written by hand from the site.
    /// If the rule that turns a display name into one of them is right, it
    /// reproduces all six - and it does.
    #[test]
    fn the_sites_names_become_the_names_this_project_already_used() {
        assert_eq!(slug("General"), "general");
        assert_eq!(slug("Gunt"), "gunt");
        assert_eq!(slug("Keno Kasino"), "keno-kasino");
        assert_eq!(slug("Fishtank"), "fishtank");
        assert_eq!(slug("Beauty Parlor"), "beauty-parlor");
        assert_eq!(slug("SPORTS!!"), "sports");
        // And the one that was missing.
        assert_eq!(slug("Lolcows"), "lolcows");
    }

    #[test]
    fn reads_the_room_switcher_out_of_the_page() {
        let html = concat!(
            "<div id=\"chat-rooms\">",
            "<a class=\"chat-room\" role=\"button\" href=\"#1\" data-id=\"1\">General</a>",
            "<a class=\"chat-room\" role=\"button\" href=\"#20\" data-id=\"20\">Lolcows</a>",
            "<a class=\"chat-room\" role=\"button\" href=\"#19\" data-id=\"19\">SPORTS!!</a>",
            "</div>"
        );
        let rooms = parse_rooms(html);
        assert_eq!(rooms.len(), 3);
        assert_eq!((rooms[0].id, rooms[0].name.as_str()), (1, "general"));
        assert_eq!((rooms[1].id, rooms[1].name.as_str()), (20, "lolcows"));
        assert_eq!((rooms[2].id, rooms[2].name.as_str()), (19, "sports"));
    }

    /// Markup that no longer says what this expects reads as no rooms, which
    /// the caller turns into an error rather than an empty catalogue - the
    /// difference between "the site has no rooms" and "this could not tell".
    #[test]
    fn markup_that_changed_reads_as_nothing_rather_than_as_nonsense() {
        assert!(parse_rooms("<div>no switcher here</div>").is_empty());
        assert!(parse_rooms("<a class=\"chat-room\" data-id=\"x\">Bad</a>").is_empty());
    }
}

/// Shared by sendMessage/editMessage/deleteMessage's SneedChat branches -
/// looks up which configured room a buffer belongs to and returns that
/// room's own permanent connection (see `run_room`). There's no "switch
/// active room" step needed since every configured room is already
/// connected simultaneously.
pub(super) fn room_sender_for_buffer(state: &AppState, account_id: &str, buffer_name: &str) -> Result<tokio::sync::mpsc::UnboundedSender<String>> {
    let cfg = state.accounts.get_sneedchat(account_id).ok_or_else(|| anyhow!("no such account"))?;
    let room_name = room_name_of(buffer_name);
    let rooms = effective_rooms(&cfg);
    let room = rooms.iter().find(|r| r.name == room_name).ok_or_else(|| anyhow!("\"{buffer_name}\" isn't one of this account's configured rooms"))?;
    state.runtime.sneedchat_sender(account_id, room.id).ok_or_else(|| anyhow!("not currently connected to this room"))
}

/// What a room's buffer is called.
///
/// The `#` is not decoration - it is part of the name every other part of this
/// daemon and every frontend knows the room by. Writing whispers into
/// `room.name` instead of this made a second, empty buffer per room named
/// without it, which is the "new chat buffer" that opened when one was sent:
/// not a window being opened, but a conversation being written somewhere
/// nobody was looking. One definition so the two cannot disagree again.
pub(super) fn room_buffer_name(room_name: &str) -> String {
    format!("#{room_name}")
}

/// And the room behind a buffer's name - the same pairing read backwards.
pub(super) fn room_name_of(buffer_name: &str) -> &str {
    buffer_name.strip_prefix('#').unwrap_or(buffer_name)
}

/// Any live room connection for this account.
///
/// A whisper belongs to no room, so it does not matter which carries it - but
/// there has to be one, since the only way to say anything at all is over a
/// room's socket.
pub(super) fn any_room_sender(state: &AppState, account_id: &str) -> Result<tokio::sync::mpsc::UnboundedSender<String>> {
    let cfg = state.accounts.get_sneedchat(account_id).ok_or_else(|| anyhow!("no such account"))?;
    effective_rooms(&cfg)
        .iter()
        .find_map(|r| state.runtime.sneedchat_sender(account_id, r.id))
        .ok_or_else(|| anyhow!("not connected to Sneedchat"))
}

/// The rooms this account actually talks in.
///
/// An account with none configured still connects - to #general, which is
/// where a Sneedchat session lands by default and what makes a freshly added
/// account usable before anybody has been to Settings to choose rooms.
///
/// Shared with the send path deliberately. The connect side had this fallback
/// and the send side read the stored list directly, so an account with no
/// rooms configured connected to #general, received messages there, and then
/// refused to send with "#general isn't one of this account's configured
/// rooms" - true of the stored config and plainly untrue of the connection
/// the user was looking at. One definition of "which rooms" means the two
/// cannot disagree again.
pub(super) fn effective_rooms(config: &SneedChatAccountConfig) -> Vec<SneedChatRoom> {
    if config.rooms.is_empty() {
        vec![SneedChatRoom { id: 1, name: "general".to_string() }]
    } else {
        config.rooms.clone()
    }
}

/// Applies a roster delta to a room and republishes it.
///
/// The server sends the full roster on join and single entries afterwards,
/// both under the same key and in the same shape, so both are simply merged -
/// the roster is emptied when the room is joined, which is what makes that
/// safe. Departures arrive separately, keyed by id with a presence flag.
///
/// Ordering is by name here rather than left to the client: every other
/// backend hands over a sorted roster, and a five-hundred-name list arriving
/// in map order would be unreadable.
/// The site owner's forum account. SneedChat carries no rank information at
/// all - the roster's user objects are id, name, avatar and last activity, and
/// the only `permissions` frame describes our *own* ability to view and send -
/// so there is no wire signal to derive staff from. This one id is a fact
/// about the site rather than something the protocol tells us, which is why it
/// is the only such marking: guessing at moderators without data would be
/// worse than showing everyone as an ordinary member.
pub(super) const SITE_OWNER_ID: &str = "1";

pub(super) fn update_roster(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    joined: &[protocol::WireUser],
    left: &[String],
) {
    let buffer_id = crate::model::buffer_id(account_id, buffer_name);
    let mut by_id: std::collections::BTreeMap<String, String> = state
        .runtime
        .get_presence(&buffer_id)
        .and_then(|v| serde_json::from_value::<Vec<serde_json::Value>>(v).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let id = m.get("userId")?.as_str()?.to_string();
            let nick = m.get("nick")?.as_str()?.to_string();
            Some((id, nick))
        })
        .collect();

    for user in joined {
        by_id.insert(user.id.clone(), user.username.clone());
    }
    for id in left {
        by_id.remove(id);
    }

    let mut members: Vec<(String, String)> = by_id.into_iter().collect();
    members.sort_by(|(_, a), (_, b)| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
    let member_list = serde_json::json!(members
        .into_iter()
        .map(|(id, nick)| {
            // "~" is the owner prefix the frontend already ranks by, shared
            // with IRC rather than inventing a Sneedchat-only convention.
            let prefix = if id == SITE_OWNER_ID { "~" } else { "" };
            serde_json::json!({ "nick": nick, "userId": id, "prefix": prefix, "away": false })
        })
        .collect::<Vec<_>>());

    // Persisted as well as broadcast, so a client that subscribes later gets
    // the roster from subscribe's replay rather than waiting for the next
    // arrival or departure - which in a quiet room could be a long wait.
    state.runtime.set_presence(&buffer_id, member_list.clone());
    state.events.emit(
        "presenceChange",
        serde_json::json!({ "bufferId": buffer_id, "members": member_list }),
    );
}

#[cfg(test)]
pub(super) mod buffer_name_tests {
    use super::*;

    #[test]
    fn knows_its_own_name_either_way_round() {
        assert!(name_matches("Ancient Pioneer", "ancientpioneer", "Ancient Pioneer"));
        assert!(name_matches("Ancient Pioneer", "ancientpioneer", "ancientpioneer"));
        // Trimmed and case-insensitive, because the echo is matched against
        // whichever name the site happened to be showing.
        assert!(name_matches("Ancient Pioneer", "ancientpioneer", "  ancient pioneer  "));
        // Somebody else, which must never match - a false match drops a real
        // whisper instead of a duplicate line.
        assert!(!name_matches("Ancient Pioneer", "ancientpioneer", "no-exit"));
        // An account with no display name set still matches on its login name,
        // and an empty candidate matches nothing.
        assert!(name_matches("", "ancientpioneer", "AncientPioneer"));
        assert!(!name_matches("", "ancientpioneer", ""));
        assert!(!name_matches("", "", "anyone"));
    }

    #[test]
    fn a_room_and_its_buffer_name_are_one_pairing() {
        // The bug this exists to prevent: whispers were written to
        // `room.name`, while every room's buffer is named with a "#" in front.
        // That is not a typo with no consequence - it made a second, empty
        // buffer per room, which is the "new chat buffer" that opened when a
        // whisper was sent.
        assert_eq!(room_buffer_name("general"), "#general");
        assert_eq!(room_name_of(&room_buffer_name("general")), "general");
        assert_eq!(room_name_of("#fishtank"), "fishtank");
        // Tolerant read-back: a caller that already stripped it gets the same
        // answer rather than a different one.
        assert_eq!(room_name_of("fishtank"), "fishtank");
    }
}
