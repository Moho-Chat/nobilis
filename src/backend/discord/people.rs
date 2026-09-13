//! The people side: direct messages, friends, blocks and profiles.
//!
//! Grouped because they are one subject on Discord even though they are
//! several endpoints - a direct message, a friend request and a block are
//! three things you do to the same person from the same menu.

use super::*;

/// Shared by both READY's `private_channels` array and the REST /users/@me/
/// channels follow-up fetch (see run_gateway's READY handling) - both hand
/// this the exact same per-channel JSON shape. Returns the new (bufferId,
/// channelId) pair when this channel is genuinely new (so the caller can
/// queue it for history backfill), None if already known.
pub(super) fn register_dm_channel(
    state: &AppState,
    account_id: &str,
    ch: &Value,
    channel_map: &mut HashMap<String, (String, String)>,
    presences: &HashMap<&str, &str>,
) -> Option<(String, String)> {
    let channel_id = ch["id"].as_str()?;
    if channel_map.contains_key(channel_id) {
        return None;
    }
    let name = dm_channel_name(ch);
    let buf = state.runtime.ensure_buffer(state, account_id, &name, "dm");
    state.runtime.set_discord_channel(state, &buf.id, channel_id);
    if let Some(avatar) = dm_avatar_url(ch) {
        state.runtime.set_buffer_avatar(state, &buf.id, &avatar);
    }
    // A DM has no member list to subscribe to - the participants are right
    // here in the channel object, and they are the whole roster.
    set_dm_presence(state, &buf.id, ch, presences);
    ensure_dm_group(state, account_id);
    state.runtime.set_buffer_group(state, &buf.id, &dm_group_id(account_id));
    channel_map.insert(channel_id.to_string(), (name, "dm".to_string()));
    Some((buf.id, channel_id.to_string()))
}

/// The picture to show for a direct message: the other person's.
///
/// A DM's `recipients` array holds everyone except this account, so for a
/// one-to-one conversation it is the one person there is. A group DM has
/// several and no single face to show, which is why this takes the first only
/// when there is exactly one.
pub(super) fn dm_avatar_url(ch: &Value) -> Option<String> {
    let recipients = ch["recipients"].as_array()?;
    if recipients.len() != 1 {
        return None;
    }
    author_avatar_url(&recipients[0]).or_else(|| default_avatar_url(&recipients[0]))
}

/// The picture Discord serves for someone who has never set one.
///
/// Worth resolving rather than falling back to a coloured initial the way
/// message avatars do: a conversation list is a list of faces, and the one
/// entry showing a letter instead reads as broken rather than as a person
/// with no picture. Which of the six is theirs depends on which username
/// scheme they are on - the modern one has no discriminator and derives it
/// from the account id instead.
pub(super) fn default_avatar_url(user: &Value) -> Option<String> {
    let id = user["id"].as_str()?;
    let index = match user["discriminator"].as_str() {
        Some(d) if d != "0" => d.parse::<u64>().unwrap_or(0) % 5,
        _ => (id.parse::<u64>().ok()? >> 22) % 6,
    };
    Some(format!("https://cdn.discordapp.com/embed/avatars/{index}.png"))
}

/// A DM channel's display name, derived from its `recipients` array (their
/// display name if set, else username) - shared by register_dm_channel
/// above and open_dm below, the two places a raw Discord channel object
/// needs turning into a buffer name.
pub(super) fn dm_channel_name(ch: &Value) -> String {
    ch["recipients"]
        .as_array()
        .filter(|r| !r.is_empty())
        .map(|r| {
            r.iter()
                .filter_map(|u| u["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| u["username"].as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Accepts a real Discord invite - the same "join a server" action any
/// user client offers, not a bot-only capability (this backend always
/// runs on a real user token - see this module's own doc comment). `invite`
/// may be a bare code or a full discord.gg/xxx (or discord.com/invite/xxx)
/// URL; only the trailing path segment (the actual code) is ever sent.
/// Who somebody is, on Discord.
///
/// The account's age needs no request at all: every Discord id is a timestamp
/// with a counter on the end (see profile::snowflake_created), which is the
/// one fact people actually want from a profile and the one the rate-limited
/// endpoints are worst at giving.
///
/// The rest comes from the user-profile endpoint, which for a guild member
/// carries the day they joined *this* guild and their roles in it. It is
/// refused for somebody who shares no space with this account, and refused
/// under rate limiting - in both cases what is already known still shows.
pub async fn profile(state: &AppState, account_id: &str, buffer_id: &str, user_id: &str, name: &str) -> Value {
    let mut profile = crate::profile::pending("discord", account_id, name);
    profile["pending"] = json!(false);
    profile["id"] = json!(user_id);
    crate::profile::set(&mut profile, "createdTs", crate::profile::snowflake_created(user_id).map(|ts| json!(ts)));

    let Some(config) = state.accounts.get_discord(account_id) else { return profile };
    let guild_id = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| b.group_id)
        .and_then(|g| g.rsplit_once("guild:").map(|(_, id)| id.to_string()));

    let mut url = format!("{API_BASE}/users/{user_id}/profile?with_mutual_guilds=false");
    if let Some(guild) = &guild_id {
        url.push_str(&format!("&guild_id={guild}"));
    }

    // What the member window already saw, which is the only place a user
    // token learns when somebody joined a guild - see remember_discord_member.
    if let Some(guild) = &guild_id {
        if let Some(member) = state.runtime.discord_member(guild, user_id) {
            if let Some(joined) = member["joined_at"].as_str().and_then(parse_iso8601_seconds) {
                profile["joinedTs"] = json!(joined);
            }
            if let Some(nick) = member["nick"].as_str().filter(|s| !s.is_empty()) {
                crate::profile::note(&mut profile, "Nickname here", nick);
            }
            let held = member["roles"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let names = state.runtime.discord_role_names(guild, held);
            if !names.is_empty() {
                profile["roles"] = json!(names);
            }
            profile["isModerator"] = json!(state.runtime.discord_roles_moderate(guild, held));
            if let Some(status) = member["presence"]["status"].as_str() {
                profile["status"] = json!(status);
            }
        }
    }

    let response = http_client().get(&url).header("Authorization", &config.token).send().await;
    let Ok(body) = response else { return profile };
    let Ok(body) = body.json::<Value>().await else { return profile };

    let user = &body["user"];
    if let Some(display) = user["global_name"].as_str().filter(|s| !s.is_empty()) {
        profile["name"] = json!(display);
        profile["handle"] = json!(user["username"].as_str().unwrap_or(name));
    } else if let Some(username) = user["username"].as_str() {
        profile["name"] = json!(username);
    }
    if let (Some(avatar), Some(id)) = (user["avatar"].as_str(), user["id"].as_str()) {
        profile["avatarUrl"] = json!(format!("https://cdn.discordapp.com/avatars/{id}/{avatar}.png?size=128"));
    }
    if let Some(bio) = user["bio"].as_str().filter(|s| !s.trim().is_empty()) {
        crate::profile::note(&mut profile, "About", bio.trim());
    }
    if let Some(since) = body["premium_since"].as_str() {
        crate::profile::note(&mut profile, "Nitro since", since.split('T').next().unwrap_or(since));
    }

    // The same facts from the endpoint, where it answered - it does not for
    // somebody this account shares nothing with, which is why the member
    // window above is the primary source rather than the fallback.
    let member = &body["guild_member"];
    if profile.get("joinedTs").is_none() {
        if let Some(joined) = member["joined_at"].as_str().and_then(parse_iso8601_seconds) {
            profile["joinedTs"] = json!(joined);
        }
    }

    profile
}

/// Seconds since the epoch from an ISO 8601 timestamp, which is how Discord
/// writes every date it sends.
pub(super) fn parse_iso8601_seconds(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|dt| dt.timestamp())
}

/// Starts (or reuses - Discord's own API dedups this server-side) a DM
/// with `target_user_id`, the real numeric snowflake rather than a
/// username: this backend has no guild-wide member search to look one up
/// by name (a real REST API restriction for a user-token client, not a
/// missing feature here - see this module's own userlist-investigation
/// history), so getting someone's id the same way any Discord client
/// requires for a non-contact (right-click their profile -> Copy User ID,
/// Developer Mode on) is unavoidable. Returns the resulting buffer id.
pub async fn open_dm(state: &AppState, account_id: &str, target_user_id: &str) -> Result<String> {
    open_dm_with(state, account_id, &[target_user_id.to_string()]).await
}

/// Starts a conversation with one person or with several.
///
/// One endpoint answers both. With a single recipient Discord returns the
/// existing one-to-one DM if there already is one, so opening a conversation
/// you already have does not make a second. With more than one it always makes
/// a new group - that is Discord's own behaviour rather than a choice here:
/// two groups holding the same three people are different conversations, and
/// the API offers no way to ask for "the" one.
///
/// The body differs by count and the two forms are not interchangeable.
/// `recipient_id` takes a single id and yields a DM; `recipients` takes a list
/// and yields a group. Sending a one-element list where the singular was meant
/// creates a *group* of two rather than reusing the plain DM, which then sits
/// beside it as a near-duplicate that behaves subtly differently.
pub async fn open_dm_with(state: &AppState, account_id: &str, user_ids: &[String]) -> Result<String> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    if user_ids.is_empty() {
        bail!("a conversation needs at least one other person");
    }
    // Discord's own ceiling. Checked here so the answer is a sentence rather
    // than an opaque 400 from the API.
    if user_ids.len() > GROUP_DM_MAX_OTHERS {
        bail!(
            "a Discord group message holds ten people including you - that is {} too many",
            user_ids.len() - GROUP_DM_MAX_OTHERS
        );
    }
    let body = if user_ids.len() == 1 {
        json!({ "recipient_id": user_ids[0] })
    } else {
        json!({ "recipients": user_ids })
    };
    let resp = send_write(
        http_client()
        .post(format!("{API_BASE}/users/@me/channels"))
        .header("Authorization", &cfg.token)
        .json(&body)
        )
    .await
        .context("opening Discord DM")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "opening that conversation"));
    }
    let ch: Value = resp.json().await.context("invalid JSON response")?;
    let channel_id = ch["id"].as_str().context("no channel id in response")?;
    let name = dm_channel_name(&ch);
    let buffer = state.runtime.ensure_buffer(state, account_id, &name, "dm");
    state.runtime.set_discord_channel(state, &buffer.id, channel_id);
    if let Some(avatar) = dm_avatar_url(&ch) {
        state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
    }
    // Opened by hand rather than from READY, so there is no presence snapshot
    // to seed from; their first status update fills it in.
    set_dm_presence(state, &buffer.id, &ch, &HashMap::new());
    ensure_dm_group(state, account_id);
    state.runtime.set_buffer_group(state, &buffer.id, &dm_group_id(account_id));
    Ok(buffer.id)
}

/// How many other people a Discord group message holds - ten including you.
pub(super) const GROUP_DM_MAX_OTHERS: usize = 9;

/// Adds somebody to a group conversation already under way.
///
/// Only groups. Discord answers this on a one-to-one DM with a 403, because
/// there is nothing there to add to: growing a two-person DM into a group
/// means making a new group, which is `open_dm_with` with the whole list.
pub async fn add_to_group_dm(state: &AppState, account_id: &str, channel_id: &str, user_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client()
        .put(format!("{API_BASE}/channels/{channel_id}/recipients/{user_id}"))
        .header("Authorization", &cfg.token)
        .json(&json!({}))
        )
    .await
        .context("adding somebody to the group")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "adding somebody to the group"));
    }
    Ok(())
}

/// Removes somebody from a group conversation.
///
/// Removing yourself is how you leave one, and Discord treats it as the same
/// call - which is why this does not refuse it. The caller decides what it
/// means; `close_dm` is the one that says "leave" out loud.
pub async fn remove_from_group_dm(state: &AppState, account_id: &str, channel_id: &str, user_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client()
        .delete(format!("{API_BASE}/channels/{channel_id}/recipients/{user_id}"))
        .header("Authorization", &cfg.token)
        )
    .await
        .context("removing somebody from the group")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "removing somebody from the group"));
    }
    Ok(())
}

/// Sends a friend request - a pending request the target still has to
/// accept, exactly like clicking "Add Friend" in any real Discord client
/// (the request never completes to a full friendship synchronously here).
/// `username` accepts either a modern unique username or a legacy
/// `name#1234` pair; the discriminator half only still means anything for
/// accounts that never migrated off the old system.
///
/// Discord asks for a captcha on this one more than on anything else, so the
/// answer may be a question rather than a yes: see `send_answerable`. Handed
/// one, the same request goes out again with it attached.
pub async fn add_friend(state: &AppState, account_id: &str, username: &str, captcha: Option<&CaptchaAnswer>) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let (name, discriminator) = match username.trim().rsplit_once('#') {
        Some((n, d)) if d.chars().all(|c| c.is_ascii_digit()) && !d.is_empty() => (n, Some(d)),
        _ => (username.trim(), None),
    };
    send_answerable(
        with_captcha(
            http_client()
                .post(format!("{API_BASE}/users/@me/relationships"))
                .header("Authorization", &cfg.token)
                .json(&json!({ "username": name, "discriminator": discriminator })),
            captcha,
        ),
        "adding a friend",
    )
    .await
}

/// Answers a friend request: accepts it, or refuses it.
///
/// Accepting is the same call that sends one - PUT on the person - which is
/// how Discord models it: a request already sent the other way makes the pair
/// mutual. Refusing and withdrawing are both DELETE, and so is unfriending;
/// what differs is only which of the three you were in.
pub async fn answer_friend_request(state: &AppState, account_id: &str, user_id: &str, accept: bool) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let http = http_client();
    let url = format!("{API_BASE}/users/@me/relationships/{user_id}");
    let request = if accept { http.put(url).json(&json!({})) } else { http.delete(url) };
    let doing = if accept { "accepting the friend request" } else { "declining the friend request" };
    let resp = request.header("Authorization", &cfg.token).send().await.context(doing)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, doing));
    }
    // The gateway says so too, and that is what updates the list - this only
    // has to have happened.
    Ok(())
}

/// Blocks somebody, or lifts it.
///
/// A relationship of type 2, which is what Discord's own "Block" does: it is
/// account-wide, it follows to every client, and - unlike an ignore kept here
/// - the person is told, in the sense that their messages to you stop being
/// delivered and their friend requests stop arriving.
///
/// The gateway's own RELATIONSHIP_ADD says it happened, the same way it does
/// for a friend; this only has to make the request.
pub async fn set_blocked(state: &AppState, account_id: &str, user_id: &str, blocked: bool) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let http = http_client();
    let url = format!("{API_BASE}/users/@me/relationships/{user_id}");
    let doing = if blocked { "blocking them" } else { "unblocking them" };
    let request = if blocked {
        http.put(url).json(&json!({ "type": 2 }))
    } else {
        // The same DELETE that unfriends: what it undoes is whichever
        // relationship you were in.
        http.delete(url)
    };
    let resp = send_write(request.header("Authorization", &cfg.token)).await.context(doing)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, doing));
    }
    Ok(())
}

/// The rail entry holding an account's direct messages, matching how Discord's
/// own client gives DMs a place in the server column rather than scattering
/// them among the guilds.
pub fn dm_group_id(account_id: &str) -> String {
    format!("{account_id}|dms")
}

/// Registers the account's direct-message rail entry. Idempotent, and only
/// called once a DM actually exists, so an account with no DMs does not get an
/// empty entry sitting in the rail.
pub(super) fn ensure_dm_group(state: &AppState, account_id: &str) {
    state.runtime.upsert_buffer_group(
        state,
        crate::model::BufferGroup {
            id: dm_group_id(account_id),
            account_id: account_id.to_string(),
            service: "discord".to_string(),
            kind: "dms".to_string(),
            name: "Direct Messages".to_string(),
            icon_url: None,
            // Above the guilds, where Discord puts it.
            position: -1,
            pending: false,
        },
    );
}

/// Fetches the conversation around one message and stores it.
///
/// The counterpart of `extend_history` for a message somebody has named
/// rather than scrolled to: a pin or a search result can be years older than
/// anything stored here, and paging backwards to it would mean reading the
/// whole channel in between. Discord will hand over that one moment directly.
///
/// The messages either side of it are stored too - fifty of them, Discord's
/// own window - because arriving at a line with no conversation around it is
/// arriving nowhere.
///
/// Returns when the message was sent, which is what a client needs to go and
/// read it out of the store.
/// One relationship, in the shape the friends list draws.
///
/// The kind travels with it because the three are answered differently: a
/// friend can be messaged, an incoming request accepted or refused, and an
/// outgoing one only withdrawn.
pub(super) fn friend_json(relationship: &Value, presences: &HashMap<&str, &str>) -> Option<Value> {
    let user = &relationship["user"];
    let user_id = user["id"].as_str()?;
    let kind = match relationship["type"].as_i64() {
        Some(3) => "incoming",
        Some(4) => "outgoing",
        _ => "friend",
    };
    Some(json!({
        "userId": user_id,
        "username": user["username"].as_str().unwrap_or("unknown"),
        "globalName": user["global_name"].as_str(),
        "avatarUrl": author_avatar_url(user),
        "status": presences.get(user_id).copied().unwrap_or("offline"),
        "kind": kind,
    }))
}

/// A DM's roster: whoever is in it.
///
/// Unlike a guild channel this needs no subscription - Discord hands the
/// recipients over with the channel itself. Their status is not included
/// there, so everyone starts unknown and PRESENCE_UPDATE fills it in; for a
/// friend that is usually immediate, since READY already carried it.
pub(super) fn set_dm_presence(state: &AppState, buffer_id: &str, channel: &Value, presences: &HashMap<&str, &str>) {
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let mut members: Vec<Value> = channel["recipients"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let user_id = r["id"].as_str()?;
            let nick = r["global_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| r["username"].as_str())
                .unwrap_or("unknown");
            // Learned here as well as shown, the same as the guild member
            // list does. A DM is the only place some people are ever seen,
            // and an event that names them by id alone - a typing notice
            // carries no member object outside a guild - had nothing to look
            // them up in and fell back to calling them "Someone".
            state.runtime.remember_discord_name(&account_id, user_id, nick);
            let status = presences.get(user_id).copied().unwrap_or("offline");
            Some(json!({ "nick": nick, "userId": user_id, "prefix": "", "away": status == "offline", "status": status }))
        })
        .collect();
    if members.is_empty() {
        return;
    }
    members.sort_by(|a, b| a["nick"].as_str().unwrap_or("").to_lowercase().cmp(&b["nick"].as_str().unwrap_or("").to_lowercase()));
    let member_list = json!(members);
    state.runtime.set_presence(buffer_id, member_list.clone());
    state.events.emit("presenceChange", json!({ "bufferId": buffer_id, "members": member_list }));
}

/// Somebody joined or left a group message, told by the gateway.
///
/// The other half of `addToDiscordGroupDm`/`removeFromDiscordGroupDm`: this
/// client could change who is in a group and never heard about a change made
/// anywhere else, so the member list was right only for the changes made from
/// this window. Somebody added from a phone was absent here, and their
/// messages arrived from a person not in the list.
///
/// The roster already on screen is edited rather than rebuilt, because the
/// event carries one user and not the channel - there is no `recipients`
/// array here to re-derive from, and asking Discord for one would be a
/// request per membership change to learn what the event already said.
pub(super) fn recipient_changed(state: &AppState, account_id: &str, channel_id: &str, user: &Value, joined: bool) {
    let Some(buffer_id) = state.runtime.discord_buffer_for_channel(account_id, channel_id) else { return };
    let Some(user_id) = user["id"].as_str() else { return };

    let before: Vec<Value> = state
        .runtime
        .get_presence(&buffer_id)
        .and_then(|p| p.as_array().cloned())
        .unwrap_or_default();
    if joined {
        // Learned as well as shown, the same as the initial roster does: a
        // group message is the only place some people are ever seen, and an
        // event naming them by id alone would otherwise have nothing to look
        // them up in.
        state.runtime.remember_discord_name(account_id, user_id, &display_name(user));
    }
    let member_list = json!(roster_after(before, user, joined));
    state.runtime.set_presence(&buffer_id, member_list.clone());
    state.events.emit("presenceChange", json!({ "bufferId": buffer_id, "members": member_list }));
}

/// The roster a membership change leaves behind.
///
/// Pure, and separate from the event handling, because this is the part that
/// can be wrong quietly: a list that grows a duplicate, or loses somebody, or
/// stops being sorted, all look like a working member list until somebody
/// counts. The caller has the state; this has the arithmetic.
///
/// Removal happens on both paths on purpose. Discord can deliver a second ADD
/// for somebody already there - a reconnect, a duplicated dispatch - and
/// appending blindly would show them twice.
pub(super) fn roster_after(mut members: Vec<Value>, user: &Value, joined: bool) -> Vec<Value> {
    let Some(user_id) = user["id"].as_str() else { return members };
    members.retain(|m| m["userId"].as_str() != Some(user_id));
    if joined {
        members.push(json!({
            "nick": display_name(user),
            "userId": user_id,
            "prefix": "",
            // Their status is not in this event. Offline is the honest
            // placeholder and PRESENCE_UPDATE corrects it, which is exactly
            // how everybody else in this list arrived.
            "away": true,
            "status": "offline",
            "avatarUrl": super::messages::author_avatar_url(user),
        }));
        members.sort_by(|a, b| {
            a["nick"].as_str().unwrap_or("").to_lowercase().cmp(&b["nick"].as_str().unwrap_or("").to_lowercase())
        });
    }
    members
}

/// What to call somebody: the display name they set, or their username.
fn display_name(user: &Value) -> String {
    user["global_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| user["username"].as_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Closes a direct message, the way pressing the x beside one does.
///
/// Only for a direct message. A guild's channel cannot be left on its own -
/// you are in it because you are in the guild - so closing one of those is a
/// local matter, and leaving the guild is a different action with much larger
/// consequences.
pub async fn close_dm(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client()
        .delete(format!("{API_BASE}/channels/{channel_id}"))
        .header("Authorization", &cfg.token)
        )
    .await
        .context("closing the conversation")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn everyone_has_a_face_even_without_an_avatar() {
        // A conversation list is a list of faces; one entry showing a letter
        // instead reads as broken rather than as a person with no picture.
        let modern = json!({ "id": "1339667204756475924", "discriminator": "0" });
        let url = default_avatar_url(&modern).expect("a modern account still has a default");
        assert!(url.starts_with("https://cdn.discordapp.com/embed/avatars/"), "unexpected: {url}");
        let index: u64 = url.trim_start_matches("https://cdn.discordapp.com/embed/avatars/").trim_end_matches(".png").parse().unwrap();
        assert!(index < 6, "modern accounts pick one of six, got {index}");

        // The old scheme derives it from the discriminator instead.
        let legacy = json!({ "id": "80351110224678912", "discriminator": "0007" });
        let url = default_avatar_url(&legacy).unwrap();
        assert!(url.ends_with("/2.png"), "0007 % 5 is 2, got {url}");
    }

    fn person(id: &str, nick: &str) -> Value {
        json!({ "nick": nick, "userId": id, "prefix": "", "away": false, "status": "online" })
    }

    /// The member list was right only for changes made from this window.
    /// Somebody added from a phone was absent here, with their messages
    /// arriving from a person not in the list.
    #[test]
    fn somebody_added_elsewhere_joins_the_list() {
        let before = vec![person("1", "Beth"), person("2", "Dave")];
        let after = roster_after(before, &json!({ "id": "3", "global_name": "Carol" }), true);
        let names: Vec<&str> = after.iter().map(|m| m["nick"].as_str().unwrap()).collect();
        // In place, rather than appended: a list people read is a sorted one.
        assert_eq!(names, vec!["Beth", "Carol", "Dave"]);
        // Unknown until PRESENCE_UPDATE says otherwise, which is how everybody
        // else in this list arrived too.
        let carol = after.iter().find(|m| m["userId"] == "3").unwrap();
        assert_eq!(carol["status"], "offline");
    }

    #[test]
    fn somebody_removed_elsewhere_leaves_it() {
        let before = vec![person("1", "Beth"), person("2", "Dave")];
        let after = roster_after(before, &json!({ "id": "2", "username": "dave" }), false);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["userId"], "1");
    }

    /// Discord can deliver the same ADD twice - a reconnect, a duplicated
    /// dispatch - and a list that grows a second copy of somebody looks like
    /// a working member list until somebody counts.
    #[test]
    fn adding_somebody_twice_does_not_double_them() {
        let before = vec![person("1", "Beth")];
        let user = json!({ "id": "1", "global_name": "Beth" });
        let once = roster_after(before, &user, true);
        let twice = roster_after(once.clone(), &user, true);
        assert_eq!(once.len(), 1);
        assert_eq!(twice.len(), 1);
    }

    /// A name they never set falls back to the username, and an event with
    /// no user at all changes nothing rather than emptying the room.
    #[test]
    fn a_person_is_named_however_they_can_be() {
        let after = roster_after(vec![], &json!({ "id": "9", "username": "quiet" }), true);
        assert_eq!(after[0]["nick"], "quiet");

        let unchanged = roster_after(vec![person("1", "Beth")], &json!({}), true);
        assert_eq!(unchanged.len(), 1);
        assert_eq!(unchanged[0]["userId"], "1");
    }

    #[test]
    fn a_direct_message_is_headed_by_the_other_person() {
        // And a group has no single face, so it gets none rather than an
        // arbitrary one of several.
        let one = json!({ "recipients": [{ "id": "1", "avatar": "abc", "discriminator": "0" }] });
        assert_eq!(
            dm_avatar_url(&one).as_deref(),
            Some("https://cdn.discordapp.com/avatars/1/abc.png")
        );
        let group = json!({ "recipients": [{ "id": "1", "avatar": "a" }, { "id": "2", "avatar": "b" }] });
        assert!(dm_avatar_url(&group).is_none(), "a group DM was given one member's face");
    }
}
