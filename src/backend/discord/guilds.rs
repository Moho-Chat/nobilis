//! Servers: their channels, their roles, and what this account may do.
//!
//! The permission arithmetic here is Discord's own and is done locally on
//! purpose: a channel this account cannot see must not be registered as a
//! buffer at all, and asking the API per channel would be one request per
//! channel per connect.

use super::*;

/// Accepts an invite.
///
/// Discord commonly wants a captcha for this, so the answer may be a question
/// rather than a yes - see `send_answerable`.
pub async fn join_guild(state: &AppState, account_id: &str, invite: &str, captcha: Option<&CaptchaAnswer>) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let code = invite.trim().trim_end_matches('/').rsplit('/').next().unwrap_or(invite.trim());
    send_answerable(
        with_captcha(
            http_client()
                .post(format!("{API_BASE}/invites/{code}"))
                .header("Authorization", &cfg.token)
                .json(&json!({})),
            captcha,
        ),
        "joining that server",
    )
    .await
}

/// Makes an invite to a conversation, and returns the link.
///
/// The other half of `join_guild`, which has been here on its own: this
/// client could accept an invite somebody else made but not make one, so
/// inviting anybody meant opening the official client for a link.
///
/// The three options are Discord's own, and the defaults are its defaults: a
/// day, unlimited uses, and full membership. Zero means "never" for the
/// expiry and "no limit" for the uses, which is Discord's convention and not
/// this client's - it is passed through rather than translated, so what the
/// official client offers is what is offered here.
///
/// Works for a group DM as well as a guild channel. Discord treats both as a
/// channel somebody can be invited to, and so does this.
pub async fn create_invite(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    max_age: i64,
    max_uses: i64,
    temporary: bool,
    captcha: Option<&CaptchaAnswer>,
) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .context("no known Discord channel for this conversation")?;
    let resp = send_write(with_captcha(
        http_client()
            .post(format!("{API_BASE}/channels/{channel_id}/invites"))
            .header("Authorization", &cfg.token)
            .json(&json!({
                "max_age": max_age,
                "max_uses": max_uses,
                "temporary": temporary,
            })),
        captcha,
    ))
    .await
    .context("making an invite")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        // Not known to be captcha'd for a member who already holds the
        // permission, but it costs one branch to be ready if it ever is.
        if let Some(asked) = CaptchaAsked::read(&parsed) {
            return Ok(asked.to_question());
        }
        bail!("{}", discord_error_text(status, &text, "making an invite"));
    }
    let answer: Value = resp.json().await.context("reading the invite back")?;
    // The code is what Discord returns; the link is what a person pastes,
    // and building it here means every caller does not have to know the
    // domain. `discord.gg` rather than `discord.com/invite` because that is
    // the short form the official client copies.
    let code = answer["code"].as_str().context("Discord made an invite with no code in it")?;
    Ok(json!({ "invite": format!("https://discord.gg/{code}") }))
}

/// Creates a brand-new guild owned by this account - Discord's own "Create
/// My Own" server flow, same endpoint real clients use. Discord auto-
/// creates a default #general channel; the gateway's own GUILD_CREATE
/// dispatch for it (handled above in run_gateway) is what actually turns
/// it into a buffer, so nothing else is needed here.
pub async fn create_guild(state: &AppState, account_id: &str, name: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client()
        .post(format!("{API_BASE}/guilds"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "name": name.trim() }))
        )
    .await
        .context("creating Discord guild")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "creating that server"));
    }
    Ok(())
}

pub(super) const PERM_ADMINISTRATOR: u64 = 1 << 3;

pub(super) const PERM_VIEW_CHANNEL: u64 = 1 << 10;

pub(super) const PERM_KICK_MEMBERS: u64 = 1 << 1;

pub(super) const PERM_BAN_MEMBERS: u64 = 1 << 2;

pub(super) const PERM_MANAGE_ROLES: u64 = 1 << 28;

/// Discord's own name for putting somebody in timeout.
pub(super) const PERM_MODERATE_MEMBERS: u64 = 1 << 40;

pub(super) fn parse_perm(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
}

/// This account's own role ids in `guild`. Discord conveniently includes
/// just the requesting user's own member object in a guild's `members`
/// array for user-account gateway sessions - confirmed live (a guild with
/// hundreds of real members still returned a `members` array of length 1,
/// containing exactly our own entry). The REST fallback covers whatever
/// case that isn't true for (large guilds are the only documented one,
/// though not one observed during development).
/// The rules a server wants agreed to before letting somebody speak.
///
/// Returned as Discord gives it - a version, a description, and the fields of
/// the form, of which the one that matters is the TERMS field carrying the
/// rules themselves. Handed over rather than interpreted here, so what is
/// shown is what the server actually wrote.
pub async fn member_verification(state: &AppState, account_id: &str, guild_id: &str) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = http_client()
        .get(format!("{API_BASE}/guilds/{guild_id}/member-verification"))
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("asking Discord for this server's rules")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{}", discord_error_text(status, &text, "reading this server's rules"));
    }
    serde_json::from_str(&text).context("parsing this server's rules")
}

/// Agrees to them, which is what lifts the gate.
///
/// The form is fetched again rather than taken from the caller: what is
/// agreed to has to be what the server is currently asking, and a version
/// captured when the dialog opened may not still be it. Every field is
/// returned as sent with its response set - Discord rejects a form that has
/// been reshaped, and it is the server's form, not ours to edit.
pub async fn accept_member_verification(state: &AppState, account_id: &str, guild_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let form = member_verification(state, account_id, guild_id).await?;
    let version = form["version"].clone();
    let fields: Vec<Value> = form["form_fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            let mut field = f.clone();
            field["response"] = Value::Bool(true);
            field
        })
        .collect();

    let resp = send_write(
        http_client()
        .put(format!("{API_BASE}/guilds/{guild_id}/requests/@me"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "version": version, "form_fields": fields }))
        )
    .await
        .context("agreeing to this server's rules")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{}", discord_error_text(status, &text, "agreeing to this server's rules"));
    }
    // Discord announces the lifted gate with GUILD_MEMBER_UPDATE, which this
    // client does not listen for - so the group is corrected here instead,
    // otherwise the notice would sit there until the next connection.
    state.runtime.clear_discord_guild_pending(state, account_id, guild_id);
    Ok(())
}

/// Reads a guild again and makes the channel list match what it says.
///
/// The list is otherwise built once, at connect, and never revisited - so it
/// drifted from reality in every direction. A channel added by an operator
/// never appeared; one deleted stayed; agreeing to a server's rules opened
/// everything at once and showed none of it; and a permission change that
/// took access away left the channel sitting there until the next restart,
/// which is the worst of the four, since it looks like a channel you can
/// read and cannot.
///
/// Authoritative in both directions, which is the point: what the guild now
/// says is visible is added, and anything held for this guild that it no
/// longer lists - deleted, or no longer permitted - is taken away.
///
/// Two requests, since a guild's channels do not come with it. The position
/// comes from the rail entry rather than either: it is the order somebody
/// dragged their servers into, which REST does not know and which would
/// silently reshuffle the column if rebuilt as zero.
pub(super) async fn resync_guild(state: &AppState, config: &DiscordAccountConfig, guild_id: &str, channel_map: &mut HashMap<String, (String, String)>) {
    let account_id = config.account_id();
    let group_id = guild_group_id(&account_id, guild_id);
    // A burst - one event per channel while a server is rearranged - becomes
    // one pass and at most one more that sees the settled result.
    if !state.runtime.try_start_guild_resync(&group_id) {
        return;
    }
    loop {
        resync_guild_once(state, config, guild_id, channel_map).await;
        if !state.runtime.finish_guild_resync(&group_id) {
            return;
        }
    }
}

pub(super) async fn resync_guild_once(state: &AppState, config: &DiscordAccountConfig, guild_id: &str, channel_map: &mut HashMap<String, (String, String)>) {
    let account_id = config.account_id();
    let fetch = |path: String| async move {
        http_client()
            .get(format!("{API_BASE}/{path}"))
            .header("Authorization", &config.token)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
    };
    let (Some(mut guild), Some(channels)) = (
        fetch(format!("guilds/{guild_id}")).await,
        fetch(format!("guilds/{guild_id}/channels")).await,
    ) else {
        tracing::debug!("discord: could not re-read guild {guild_id}");
        return;
    };
    let Some(list) = channels.as_array().cloned() else {
        tracing::debug!("discord: guild {guild_id} returned no channel list");
        return;
    };
    guild["channels"] = Value::Array(list.clone());
    if let Some(group) = state.runtime.get_buffer_group(&guild_group_id(&account_id, guild_id)) {
        guild["position"] = serde_json::json!(group.position);
    }

    // Adds whatever is newly visible and reports everything it judged
    // visible - which is what tells a channel still ours to read from one
    // that merely used to be, a difference channel_map cannot make since it
    // remembers what was registered rather than what is permitted.
    let visible = register_guild_channels(state, config, &guild, channel_map).await;

    // Anything this guild used to have and no longer offers. Deleted, or
    // still there and no longer ours to read - which look the same from
    // here and want the same answer.
    for buffer_id in state.runtime.discord_buffers_in_guild(&account_id, guild_id) {
        let Some(channel_id) = state.runtime.get_discord_channel(&buffer_id) else { continue };
        if visible.contains(&channel_id) {
            continue;
        }
        channel_map.remove(&channel_id);
        state.runtime.remove_buffer(state, &buffer_id);
    }
}

/// This account's own membership of a guild.
///
/// From the payload when it is there, and asked for when it is not. The
/// gateway does not promise our own member object in GUILD_CREATE - which is
/// exactly what made the first attempt at reading the screening flag below
/// report every server as ungated: it read the payload only, found nothing,
/// and took nothing to mean no.
///
/// Fetched once and read twice, since roles and the screening flag both live
/// here and asking separately would be two round trips for one answer.
pub(super) async fn own_member(config: &DiscordAccountConfig, guild: &Value) -> Option<Value> {
    if let Some(members) = guild["members"].as_array() {
        if let Some(me) = members.iter().find(|m| m["user"]["id"].as_str() == Some(config.user_id.as_str())) {
            return Some(me.clone());
        }
    }
    // /users/@me/guilds/{id}/member, not /guilds/{id}/members/@me. The
    // latter is a bot endpoint: given a user token it reads "@me" as a
    // literal id and answers 400 every time, so the fallback here silently
    // never worked - which is why a gated server reported as ungated even
    // after the lookup was added. Confirmed against both endpoints with a
    // live account behind a gate.
    let guild_id = guild["id"].as_str()?;
    let resp = http_client()
        .get(format!("{API_BASE}/users/@me/guilds/{guild_id}/member"))
        .header("Authorization", &config.token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!("discord: no member object for guild {guild_id}: HTTP {}", resp.status());
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// Whether this account has joined the guild but cannot speak in it yet.
///
/// Discord calls it membership screening: a server can require agreement to
/// its rules first, and a member sits `pending` until they give it. Without
/// reading this the client has no idea why a channel refuses everything typed
/// into it - the send simply fails, with an error about permissions that
/// names no cause.
///
/// Absent means not pending, which is the ordinary case and the safe
/// assumption: claiming a gate that is not there would put a notice above
/// every composer for no reason.
pub(super) fn member_is_pending(member: Option<&Value>) -> bool {
    member.and_then(|m| m["pending"].as_bool()).unwrap_or(false)
}

pub(super) fn member_role_ids(member: Option<&Value>) -> Vec<String> {
    member
        .and_then(|m| m["roles"].as_array())
        .map(|r| r.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Discord's documented permission-overwrite algorithm: base permissions
/// (bitwise OR of @everyone's permissions and every role the member has),
/// then the channel's @everyone overwrite, then the union of the member's
/// own role overwrites, then a member-specific overwrite if one exists -
/// each layer applied as `(perms & !deny) | allow`, in that exact order.
/// The guild owner and anyone with ADMINISTRATOR short-circuit to "sees
/// everything," bypassing overwrites entirely, same as Discord's own
/// clients - the owner bypass matters even for an otherwise-roleless
/// owner account (confirmed live: missed 3 channels in a server this
/// account owns outright before this was added).
#[allow(clippy::too_many_arguments)]
pub(super) fn can_view_channel(is_owner: bool, guild_id: &str, roles: &[Value], member_role_ids: &[String], user_id: &str, channel: &Value) -> bool {
    if is_owner {
        return true;
    }
    let mut base: u64 = 0;
    for role in roles {
        let role_id = role["id"].as_str().unwrap_or("");
        if role_id == guild_id || member_role_ids.iter().any(|r| r == role_id) {
            base |= parse_perm(&role["permissions"]);
        }
    }
    if base & PERM_ADMINISTRATOR != 0 {
        return true;
    }

    let mut perms = base;
    let empty = Vec::new();
    let overwrites = channel["permission_overwrites"].as_array().unwrap_or(&empty);

    if let Some(ow) = overwrites.iter().find(|o| o["type"].as_i64() == Some(0) && o["id"].as_str() == Some(guild_id)) {
        perms = (perms & !parse_perm(&ow["deny"])) | parse_perm(&ow["allow"]);
    }

    let mut role_allow = 0u64;
    let mut role_deny = 0u64;
    for ow in overwrites {
        if ow["type"].as_i64() == Some(0) {
            let id = ow["id"].as_str().unwrap_or("");
            if id != guild_id && member_role_ids.iter().any(|r| r == id) {
                role_allow |= parse_perm(&ow["allow"]);
                role_deny |= parse_perm(&ow["deny"]);
            }
        }
    }
    perms = (perms & !role_deny) | role_allow;

    if let Some(ow) = overwrites.iter().find(|o| o["type"].as_i64() == Some(1) && o["id"].as_str() == Some(user_id)) {
        perms = (perms & !parse_perm(&ow["deny"])) | parse_perm(&ow["allow"]);
    }

    perms & PERM_VIEW_CHANNEL != 0
}

/// Shared by both READY's embedded `guilds` array and live GUILD_CREATE
/// dispatches - same per-guild JSON shape either way (id, name, channels,
/// roles, members). A guild with no `channels` array (an "unavailable"
/// stub, or a guild Discord genuinely didn't include full data for) is a
/// harmless no-op. Channels this account can't VIEW_CHANNEL are silently
/// skipped rather than turned into buffers - Discord's own channel list
/// includes every channel in the guild regardless of the requester's
/// access, so without this check, private/role-gated channels the account
/// has no business seeing would show up right alongside ones it can.
/// Returns the channels this account may currently see, so a caller
/// rebuilding a guild can tell "still there" from "no longer ours to read"
/// without repeating the permission reasoning that happens here.
pub(super) async fn register_guild_channels(state: &AppState, config: &DiscordAccountConfig, guild: &Value, channel_map: &mut HashMap<String, (String, String)>) -> std::collections::HashSet<String> {
    let mut visible = std::collections::HashSet::new();
    let Some(channels) = guild["channels"].as_array() else { return visible };
    let Some(guild_id) = guild["id"].as_str() else { return visible };
    let guild_name = guild["name"].as_str().unwrap_or("guild").to_string();
    let roles = guild["roles"].as_array().cloned().unwrap_or_default();
    let is_owner = guild["owner_id"].as_str() == Some(config.user_id.as_str());
    // Only worth the (possible REST) round-trip if it'll actually be used.
    // One lookup for both facts. The screening flag is wanted for every
    // guild, owned or not, so this no longer skips the fetch for an owner
    // the way the roles-only version could.
    let me = own_member(config, guild).await;
    let own_roles = member_role_ids(me.as_ref());

    // This guild's emoji, as a place they come from rather than as a copy on
    // every channel. The per-channel copy below stays: it answers "what does
    // this room own", which is what a message being rendered needs, while
    // this answers "what can this account send", which is what the picker
    // needs - and with Nitro those are different sets (see #205).
    if let Some(emojis) = guild["emojis"].as_array() {
        let entries: Vec<crate::model::EmojiEntry> = emojis
            .iter()
            .filter(|e| e["available"].as_bool().unwrap_or(true))
            .filter_map(|e| {
                Some(crate::model::EmojiEntry {
                    id: e["id"].as_str()?.to_string(),
                    name: e["name"].as_str()?.to_string(),
                    url: None,
                    animated: e["animated"].as_bool().unwrap_or(false),
                    locked: false,
                })
            })
            .collect();
        if !entries.is_empty() {
            state.runtime.set_emoji_source(
                &config.account_id(),
                crate::model::EmojiSource {
                    id: guild_id.to_string(),
                    name: guild_name.clone(),
                    service: "discord".to_string(),
                    kind: "text".to_string(),
                    icon_url: cached_guild_icon(guild_id, guild["icon"].as_str()).await,
                    // Named by channel below, once the visible set is known.
                    buffers: Vec::new(),
                    sendable_anywhere: state.runtime.emoji_unrestricted(&config.account_id()),
                    emoji: entries,
                },
            );
        }
    }

    // Kept where every path that registers a guild passes, rather than in the
    // GUILD_CREATE arm alone: for this kind of account Discord sends the
    // guilds inside READY and GUILD_CREATE never fires, so everything that
    // needed the roles - who may moderate here, which roles can be mentioned -
    // was asking an empty table and quietly getting "nobody" and "none".
    state.runtime.set_discord_guild_roles(guild_id, roles.clone());
    if let Some(owner_id) = guild["owner_id"].as_str() {
        state.runtime.set_discord_guild_owner(guild_id, owner_id);
    }
    // Our own membership, which is where our roles here come from. Fetched
    // just above for the permission check; remembered so an RPC asked later
    // does not have to fetch it again.
    if let Some(me) = &me {
        state.runtime.remember_discord_member(guild_id, &config.user_id, me);
    }
    // An owner sees everything regardless, so the permission check is handed
    // an empty list rather than spending the lookup - but the baseline above
    // wants the real ones either way.
    let member_role_ids = if is_owner { Vec::new() } else { own_roles.clone() };
    let is_pending = member_is_pending(me.as_ref());
    let account_id = config.account_id();
    let mut new_channels: Vec<(String, String)> = Vec::new();

    // Type 2 is a voice channel. Recorded rather than made into a buffer -
    // there is no conversation to show - so a client can list them and join.
    let voice: Vec<(String, String, u64)> = channels
        .iter()
        .filter(|c| c["type"].as_i64() == Some(2))
        .filter_map(|c| {
            Some((
                c["id"].as_str()?.to_string(),
                c["name"].as_str().unwrap_or("voice").to_string(),
                c["user_limit"].as_u64().unwrap_or(0),
            ))
        })
        .collect();
    if !voice.is_empty() {
        state.runtime.set_discord_voice_channels(&account_id, guild_id, voice);
    }

    // Whatever the guild told us about who its people are. Recorded before
    // the voice states below, so anyone already in a channel has a name.
    for m in guild["members"].as_array().into_iter().flatten() {
        if let Some(user_id) = m["user"]["id"].as_str() {
            let nick = m["nick"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| m["user"]["global_name"].as_str().filter(|s| !s.is_empty()))
                .or_else(|| m["user"]["username"].as_str());
            if let Some(nick) = nick {
                state.runtime.remember_discord_name(&account_id, user_id, nick);
            }
        }
    }

    // Who is already in them. This arrives once, with the guild; everything
    // after is VOICE_STATE_UPDATE.
    for vs in guild["voice_states"].as_array().into_iter().flatten() {
        if let (Some(user_id), Some(channel_id)) = (vs["user_id"].as_str(), vs["channel_id"].as_str()) {
            state.runtime.set_discord_voice_presence(
                &account_id,
                user_id,
                Some(channel_id),
                voice_member_name(vs),
                voice_member_avatar(vs).as_deref(),
                crate::runtime::VoiceFlags::from_voice_state(vs),
            );
        }
    }

    // The guild's own rail entry. Registered before its channels so a
    // frontend never briefly sees a buffer pointing at a group it has not
    // heard of. Discord's `position` is the order the user themselves put
    // their servers in, which is worth preserving.
    let group_id = guild_group_id(&account_id, guild_id);
    // Remembered separately from the flag below, which is about what to
    // show: this is what says the channel list will need rebuilding when the
    // gate lifts, and it has to outlive the flag being cleared.
    state.runtime.note_discord_gated(&group_id, is_pending);
    // The roles we have right now, as the baseline a later change is judged
    // against. Set here because this is where they are known: leaving the
    // first member update to establish it meant that update - which is very
    // often the role grant somebody has just gone and earned - was read as
    // "nothing to compare with" and swallowed.
    state.runtime.set_discord_own_roles(&group_id, own_roles.clone());
    state.runtime.upsert_buffer_group(
        state,
        crate::model::BufferGroup {
            id: group_id.clone(),
            account_id: account_id.clone(),
            service: "discord".to_string(),
            kind: "guild".to_string(),
            name: guild_name.clone(),
            // Filled in by the background fetch below once it lands; the rail
            // shows initials until then rather than waiting on the network.
            icon_url: cached_guild_icon(guild_id, guild["icon"].as_str()).await,
            position: guild["position"].as_i64().unwrap_or(0),
            // Joined, but not yet able to speak: a server with membership
            // screening turned on leaves a new member pending until they
            // agree to its rules.
            pending: is_pending,
        },
    );
    cache_guild_icon(
        state.clone(),
        account_id.clone(),
        guild_id.to_string(),
        guild_name.clone(),
        guild["icon"].as_str().map(str::to_string),
        guild["position"].as_i64().unwrap_or(0),
        is_pending,
    );

    // Type 4 is a category: not a channel anyone talks in, but the heading
    // the others are filed under, and it carries its own ordering.
    // Which channel each thread hangs under, by id: its name for the heading
    // and its place for the ordering, so a thread sits with its channel
    // rather than at the end of the list.
    let mut parents: HashMap<String, (String, i64)> = HashMap::new();
    let categories: HashMap<&str, (&str, i64)> = channels
        .iter()
        .filter(|c| c["type"].as_i64() == Some(4))
        .filter_map(|c| Some((c["id"].as_str()?, (c["name"].as_str().unwrap_or("category"), c["position"].as_i64().unwrap_or(0)))))
        .collect();

    for ch in channels {
        // 0 = GUILD_TEXT, 5 = GUILD_ANNOUNCEMENT - the only channel types
        // this milestone renders as buffers (voice/category/forum/etc.
        // skipped).
        let kind_num = ch["type"].as_i64().unwrap_or(-1);
        if kind_num != 0 && kind_num != 5 {
            continue;
        }
        let Some(channel_id) = ch["id"].as_str() else { continue };
        if !can_view_channel(is_owner, guild_id, &roles, &member_role_ids, &config.user_id, ch) {
            continue;
        }
        visible.insert(channel_id.to_string());
        if channel_map.contains_key(channel_id) {
            continue;
        }
        let chan_name = ch["name"].as_str().unwrap_or("channel");
        let name = format!("{guild_name}/#{chan_name}");
        let buf = state.runtime.ensure_buffer(state, &account_id, &name, "channel");
        state.runtime.set_discord_channel(state, &buf.id, channel_id);
        state.runtime.set_discord_guild(&buf.id, guild_id);
        state.runtime.set_buffer_group(state, &buf.id, &group_id);

        // Where Discord itself puts this channel. Uncategorised channels sit
        // above every heading, which is where Discord shows them, so they take
        // a category rank below any real one.
        let parent = ch["parent_id"].as_str().and_then(|p| categories.get(p));
        let channel_pos = ch["position"].as_i64().unwrap_or(0);
        let sort = match parent {
            Some((_, cat_pos)) => (cat_pos + 1) * 10_000 + channel_pos,
            None => channel_pos,
        };
        state.runtime.set_buffer_category(state, &buf.id, parent.map(|(name, _)| *name), sort);

        // Custom emoji are per-guild, not per-channel, but buffers only
        // carry a channel id (see discord_channels) - simplest to just
        // hand each of the guild's channels its own copy of the same
        // list rather than adding a separate guild-id lookup for this.
        // Already present in this same GUILD_CREATE payload (`available:
        // false` covers a guild that dropped below the boost tier a slot
        // needed - Discord still lists it, just unusable), so this is
        // free - no extra REST call, unlike the member roster attempt.
        if let Some(emojis) = guild["emojis"].as_array() {
            let usable: Vec<Value> = emojis
                .iter()
                .filter(|e| e["available"].as_bool().unwrap_or(true))
                .filter_map(|e| {
                    let id = e["id"].as_str()?;
                    let name = e["name"].as_str()?;
                    Some(json!({ "id": id, "name": name, "animated": e["animated"].as_bool().unwrap_or(false) }))
                })
                .collect();
            state.runtime.set_discord_buffer_emojis(&buf.id, usable);
        }
        channel_map.insert(channel_id.to_string(), (name, "channel".to_string()));
        parents.insert(channel_id.to_string(), (chan_name.to_string(), sort));
        new_channels.push((buf.id, channel_id.to_string()));
    }

    // A forum is not a channel anybody talks in: it holds posts, and every
    // post is a thread. So it is registered as the heading its posts are
    // filed under rather than as a buffer that would always be empty.
    for ch in channels {
        let kind_num = ch["type"].as_i64().unwrap_or(-1);
        if kind_num != 15 && kind_num != 16 {
            continue;
        }
        let Some(channel_id) = ch["id"].as_str() else { continue };
        if !can_view_channel(is_owner, guild_id, &roles, &member_role_ids, &config.user_id, ch) {
            continue;
        }
        visible.insert(channel_id.to_string());
        let chan_name = ch["name"].as_str().unwrap_or("forum").to_string();
        let parent = ch["parent_id"].as_str().and_then(|p| categories.get(p));
        let channel_pos = ch["position"].as_i64().unwrap_or(0);
        let sort = match parent {
            Some((_, cat_pos)) => (cat_pos + 1) * 10_000 + channel_pos,
            None => channel_pos,
        };
        parents.insert(channel_id.to_string(), (chan_name, sort));
    }

    // The threads this account is already in, which arrive with the guild.
    // Everything else a channel or forum holds is asked for when it is
    // opened - see list_threads - because a guild can have hundreds and most
    // of them are conversations nobody here is part of.
    for thread in guild["threads"].as_array().cloned().unwrap_or_default() {
        if let Some((buffer_id, thread_id)) =
            register_thread(state, &account_id, guild_id, &guild_name, &group_id, &parents, &thread, channel_map)
        {
            visible.insert(thread_id.clone());
            new_channels.push((buffer_id, thread_id));
        }
    }

    spawn_backfill(state.clone(), config.token.clone(), config.user_id.clone(), config.display_name.clone(), new_channels);
    visible
}

/// Puts one thread on the list, under the channel or forum it belongs to.
///
/// A buffer of its own rather than a panel, because that is what a thread is
/// here: somewhere people are talking, with its own history and its own
/// composer. Discord gives it a channel id like any other, so everything that
/// already works for a channel - sending, backfill, reading - works for it
/// without knowing it is a thread.
///
/// Returns nothing when the thread's parent is not a channel this account can
/// see: a thread is exactly as private as what it hangs under.
pub(super) fn register_thread(
    state: &AppState,
    account_id: &str,
    guild_id: &str,
    guild_name: &str,
    group_id: &str,
    parents: &HashMap<String, (String, i64)>,
    thread: &Value,
    channel_map: &mut HashMap<String, (String, String)>,
) -> Option<(String, String)> {
    let kind_num = thread["type"].as_i64().unwrap_or(-1);
    // 10 is a thread on an announcement, 11 a public one, 12 a private one.
    if !matches!(kind_num, 10 | 11 | 12) {
        return None;
    }
    let thread_id = thread["id"].as_str()?;
    let (parent_name, parent_sort) = parents.get(thread["parent_id"].as_str()?)?;
    let thread_name = thread["name"].as_str().unwrap_or("thread");
    // Named for the thread and filed under the channel, so the list reads as
    // a channel with its conversations under it rather than as a wall of
    // similar-looking names.
    let name = format!("{guild_name}/#{thread_name}");
    if channel_map.contains_key(thread_id) {
        return None;
    }
    let buf = state.runtime.ensure_buffer(state, account_id, &name, "channel");
    state.runtime.set_discord_channel(state, &buf.id, thread_id);
    state.runtime.set_discord_guild(&buf.id, guild_id);
    state.runtime.set_buffer_group(state, &buf.id, group_id);
    // Just after the channel it hangs under, and in the order Discord lists
    // them: a thread's own position is its last message, which is what
    // "recent" means for a conversation.
    state.runtime.set_buffer_category(state, &buf.id, Some(parent_name), parent_sort + 1);
    channel_map.insert(thread_id.to_string(), (name.clone(), "channel".to_string()));
    Some((buf.id, thread_id.to_string()))
}

/// The threads a channel or forum is holding, asked for when it is opened.
///
/// Not fetched at connect: a guild can be holding hundreds, most of them
/// conversations nobody here is part of, and the ones this account has joined
/// already arrive with the guild. This is the rest - what a forum's posts
/// are, and what somebody means by "what else is going on in here".
///
/// Archived threads are included, because on a forum that is most of them:
/// a post nobody has answered in a week is still the post somebody came
/// looking for.
pub async fn list_threads(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let mut out: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Two lists, because Discord keeps them apart: what is live, and what has
    // gone quiet. Neither alone is the answer to "what threads are here".
    let guild_id = state.runtime.get_discord_guild(buffer_id);
    let mut requests: Vec<String> = Vec::new();
    if let Some(guild) = &guild_id {
        requests.push(format!("{API_BASE}/guilds/{guild}/threads/active"));
    }
    requests.push(format!("{API_BASE}/channels/{channel_id}/threads/archived/public?limit=25"));

    for url in requests {
        let resp = match http_client().get(&url).header("Authorization", &cfg.token).send().await {
            Ok(resp) if resp.status().is_success() => resp,
            // A channel with no archive, or one this account may not read the
            // archive of, is not a failure worth refusing the whole list for.
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!("discord: reading threads: {e}");
                continue;
            }
        };
        let answer: Value = match resp.json().await {
            Ok(answer) => answer,
            Err(_) => continue,
        };
        for thread in answer["threads"].as_array().cloned().unwrap_or_default() {
            // The active list covers the whole guild, so the ones under other
            // channels are somebody else's question.
            if thread["parent_id"].as_str() != Some(channel_id.as_str()) {
                continue;
            }
            let Some(id) = thread["id"].as_str() else { continue };
            if !seen.insert(id.to_string()) {
                continue;
            }
            out.push(json!({
                "id": id,
                "name": thread["name"].as_str().unwrap_or("thread"),
                "archived": thread["thread_metadata"]["archived"].as_bool().unwrap_or(false),
                "messageCount": thread["message_count"].as_i64().unwrap_or(0),
                "lastMessageId": thread["last_message_id"],
                "thread": thread,
            }));
        }
    }
    Ok(json!({ "threads": out }))
}

/// What this account may do to other people in a guild.
///
/// Worked out here rather than asked of Discord, because Discord has no
/// endpoint for "what may I do" - a client computes it from the roles it has
/// already been sent, which is what its own client does too. Advisory either
/// way: Discord re-checks every action and refuses it in its own words, so
/// the worst this can be wrong about is which menu entries are offered.
pub fn guild_powers(state: &AppState, account_id: &str, buffer_id: &str) -> Value {
    let nothing = json!({ "canKick": false, "canBan": false, "canMute": false, "canAssignRoles": false });
    let Some(guild_id) = state.runtime.get_discord_guild(buffer_id) else { return nothing };
    let Some(cfg) = state.accounts.get_discord(account_id) else { return nothing };
    // Owning the place is the permission that outranks every role.
    if state.runtime.discord_guild_owner(&guild_id).as_deref() == Some(cfg.user_id.as_str()) {
        return json!({ "canKick": true, "canBan": true, "canMute": true, "canAssignRoles": true });
    }
    let roles = state.runtime.discord_guild_roles(&guild_id);
    let mine = state
        .runtime
        .discord_member(&guild_id, &cfg.user_id)
        .map(|m| member_role_ids(Some(&m)))
        .unwrap_or_default();

    let mut perms: u64 = 0;
    for role in &roles {
        let id = role["id"].as_str().unwrap_or("");
        // The guild's own id is @everyone, which everybody has.
        if id == guild_id || mine.iter().any(|r| r == id) {
            perms |= parse_perm(&role["permissions"]);
        }
    }
    let admin = perms & PERM_ADMINISTRATOR != 0;
    let may = |bit: u64| admin || perms & bit != 0;
    json!({
        "canKick": may(PERM_KICK_MEMBERS),
        "canBan": may(PERM_BAN_MEMBERS),
        // Timeout, which is what "mute" means on Discord.
        "canMute": may(PERM_MODERATE_MEMBERS),
        "canAssignRoles": may(PERM_MANAGE_ROLES),
    })
}

/// The roles a guild has, for a menu that offers to give somebody one.
///
/// @everyone is left out - it is not a role anybody is given - and so is
/// anything managed by a bot or an integration, which Discord refuses to hand
/// out by hand and which would only be an entry that always fails.
pub fn assignable_roles(state: &AppState, buffer_id: &str) -> Value {
    let Some(guild_id) = state.runtime.get_discord_guild(buffer_id) else { return json!([]) };
    let mut roles: Vec<(i64, Value)> = state
        .runtime
        .discord_guild_roles(&guild_id)
        .into_iter()
        .filter(|r| r["id"].as_str() != Some(guild_id.as_str()))
        .filter(|r| !r["managed"].as_bool().unwrap_or(false))
        .map(|r| {
            (
                r["position"].as_i64().unwrap_or(0),
                json!({
                    "id": r["id"].as_str().unwrap_or(""),
                    "name": r["name"].as_str().unwrap_or("role"),
                    // Zero means "no colour of its own", which is how
                    // Discord says a role is drawn like everybody else.
                    "colour": r["color"].as_i64().filter(|c| *c > 0).map(|c| format!("#{c:06x}")),
                }),
            )
        })
        .collect();
    // Most senior first, the way Discord lists them.
    roles.sort_by(|a, b| b.0.cmp(&a.0));
    json!(roles.into_iter().map(|(_, r)| r).collect::<Vec<_>>())
}

/// Removes somebody from a guild, bars them from it, or puts them in timeout.
///
/// One function because they are one gesture with different weights, and
/// because every one of them is Discord's decision rather than this client's:
/// what comes back when it refuses is worth far more than a guess made here
/// about whether it would.
pub async fn moderate_member(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    user_id: &str,
    action: &str,
    minutes: Option<i64>,
    reason: Option<&str>,
) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let guild_id = state.runtime.get_discord_guild(buffer_id).context("that conversation is not in a server")?;
    let http = http_client();
    let base = format!("{API_BASE}/guilds/{guild_id}");
    let (request, doing) = match action {
        "kick" => (http.delete(format!("{base}/members/{user_id}")), "removing them from the server"),
        "ban" => (
            http.put(format!("{base}/bans/{user_id}"))
                // Nothing deleted by default: a ban is about the person, and
                // taking their last week of messages with them is a separate
                // decision that nobody made here.
                .json(&json!({ "delete_message_seconds": 0 })),
            "banning them",
        ),
        "unban" => (http.delete(format!("{base}/bans/{user_id}")), "lifting the ban"),
        "mute" | "timeout" => {
            // Discord takes the moment it ends rather than how long it lasts.
            let until = chrono::Utc::now() + chrono::Duration::minutes(minutes.unwrap_or(10));
            (
                http.patch(format!("{base}/members/{user_id}"))
                    .json(&json!({ "communication_disabled_until": until.to_rfc3339() })),
                "putting them in timeout",
            )
        }
        "unmute" => (
            http.patch(format!("{base}/members/{user_id}"))
                .json(&json!({ "communication_disabled_until": Value::Null })),
            "lifting the timeout",
        ),
        other => bail!("moho does not know how to {other} somebody on Discord"),
    };
    let mut request = request.header("Authorization", &cfg.token);
    // Discord records this against the action in the server's audit log,
    // which is where anybody asking "why was I removed" will look.
    if let Some(reason) = reason.filter(|r| !r.trim().is_empty()) {
        request = request.header("X-Audit-Log-Reason", reason);
    }
    let resp = request.send().await.context(doing)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, doing));
    }
    Ok(())
}

/// Gives somebody a role, or takes it away.
pub async fn set_member_role(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    user_id: &str,
    role_id: &str,
    give: bool,
) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let guild_id = state.runtime.get_discord_guild(buffer_id).context("that conversation is not in a server")?;
    let url = format!("{API_BASE}/guilds/{guild_id}/members/{user_id}/roles/{role_id}");
    let http = http_client();
    let request = if give { http.put(url).json(&json!({})) } else { http.delete(url) };
    let doing = if give { "giving them the role" } else { "taking the role away" };
    let resp = request.header("Authorization", &cfg.token).send().await.context(doing)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, doing));
    }
    Ok(())
}

/// Opens a thread as a conversation of its own, joining it if need be.
///
/// Discord will not deliver a thread's messages to somebody who is not in it,
/// and reading one is how you end up in it - which is why opening is a call
/// rather than a lookup. Joining an archived thread also un-archives it for
/// this account, which is what makes an old forum post readable at all.
pub async fn open_thread(state: &AppState, account_id: &str, buffer_id: &str, thread_id: &str) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let guild_id = state.runtime.get_discord_guild(buffer_id).context("that conversation is not in a server")?;
    let http = http_client();

    // Best-effort: already being a member answers 204 as well, and a thread
    // that refuses the join may still be readable.
    let joined = send_write(
        http.put(format!("{API_BASE}/channels/{thread_id}/thread-members/@me"))
            .header("Authorization", &cfg.token),
    )
    .await;
    if let Err(e) = &joined {
        tracing::debug!("discord: joining thread {thread_id}: {e}");
    }

    let resp = http
        .get(format!("{API_BASE}/channels/{thread_id}"))
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("opening the thread")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "opening the thread"));
    }
    let thread: Value = resp.json().await.context("reading the thread")?;

    // Named and filed the same way the ones that arrive with the guild are,
    // by the same function - so a thread opened by hand and a thread this
    // account was already in are the same kind of thing in the list.
    let guild_name = state
        .runtime
        .get_buffer(buffer_id)
        .map(|b| b.name.split('/').next().unwrap_or("guild").to_string())
        .unwrap_or_else(|| "guild".to_string());
    let group_id = state
        .runtime
        .get_buffer_group(buffer_id)
        .map(|g| g.id)
        .unwrap_or_default();
    let parent_id = state.runtime.get_discord_channel(buffer_id).unwrap_or_default();
    let parent_name = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| b.name.split('#').nth(1).map(str::to_string))
        .unwrap_or_else(|| "threads".to_string());
    let mut parents: HashMap<String, (String, i64)> = HashMap::new();
    parents.insert(parent_id, (parent_name, 0));

    let mut channel_map: HashMap<String, (String, String)> = HashMap::new();
    let opened = register_thread(state, account_id, &guild_id, &guild_name, &group_id, &parents, &thread, &mut channel_map);
    let name = format!("{guild_name}/#{}", thread["name"].as_str().unwrap_or("thread"));
    let id = opened
        .map(|(buffer_id, _)| buffer_id)
        .unwrap_or_else(|| crate::model::buffer_id(account_id, &name));
    // Its history, so the conversation is there when the buffer opens rather
    // than filling in a moment later.
    backfill_channel_history(state, &cfg.token, &cfg.user_id, cfg.display_name.as_deref(), &id, thread_id).await;
    Ok(json!({ "bufferId": id }))
}

/// Rail entry id for one guild. Scoped by account so two accounts in the same
/// guild get their own entry rather than colliding on one.
pub fn guild_group_id(account_id: &str, guild_id: &str) -> String {
    format!("{account_id}|guild:{guild_id}")
}

/// Leaves a guild.
///
/// Not undoable from here: rejoining needs an invite, and for a guild you were
/// invited to once, years ago, there may be nobody left to ask. A frontend
/// offering this should say so before it happens rather than after.
pub async fn leave_guild(state: &AppState, account_id: &str, guild_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client()
            .delete(format!("{API_BASE}/users/@me/guilds/{guild_id}"))
            .header("Authorization", &cfg.token)
            // Discord distinguishes leaving from being removed; this is a leave.
            .json(&json!({ "lurking": false })),
    )
    .await
    .context("leaving the server")?;
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

    /// A member update arrives for a nickname as readily as for a role, and
    /// only one of those changes what may be read.
    ///
    /// Connecting sets the baseline, so the first change after it counts.
    /// Leaving the first update to establish it meant that update - very
    /// often the role grant somebody has just gone and earned - was read as
    /// "nothing to compare with" and swallowed.
    #[test]
    fn only_a_real_role_change_counts() {
        let rt = crate::runtime::Runtime::new();
        let g = "discord:me|guild:1";

        // What connecting does.
        assert!(!rt.set_discord_own_roles(g, vec!["a".into()]), "the baseline is not a change");
        // And the grant that follows it is.
        assert!(rt.set_discord_own_roles(g, vec!["a".into(), "member".into()]), "gained a role");
        assert!(!rt.set_discord_own_roles(g, vec!["a".into(), "member".into()]), "same roles again");
        assert!(rt.set_discord_own_roles(g, vec!["a".into()]), "lost one");
        // Order is Discord's business, not a change.
        assert!(rt.set_discord_own_roles(g, vec!["a".into(), "b".into()]));
        assert!(!rt.set_discord_own_roles(g, vec!["b".into(), "a".into()]), "reordered is unchanged");
    }

    /// A burst of role edits collapses into one pass and a single follow-up
    /// that sees the settled result, rather than one two-request re-read per
    /// event while a server is being rearranged.
    #[test]
    fn a_burst_of_changes_collapses() {
        let rt = crate::runtime::Runtime::new();
        let g = "discord:me|guild:1";

        assert!(rt.try_start_guild_resync(g), "first claim runs");
        assert!(!rt.try_start_guild_resync(g), "second is folded into the first");
        assert!(!rt.try_start_guild_resync(g), "and so is the third");
        assert!(rt.finish_guild_resync(g), "something arrived while it ran, so run once more");
        assert!(!rt.finish_guild_resync(g), "and nothing since, so stop");
        // Free again afterwards.
        assert!(rt.try_start_guild_resync(g));
    }

    /// The rebuild is owed once and only for a guild that was actually
    /// gated. Ordinary member updates - a nickname, a role - arrive on the
    /// same event and are no reason to re-read a whole guild.
    #[test]
    fn a_rebuild_is_owed_once_and_only_where_it_was_gated() {
        let rt = crate::runtime::Runtime::new();
        let gated = "discord:me|guild:1";
        let plain = "discord:me|guild:2";

        rt.note_discord_gated(gated, true);
        rt.note_discord_gated(plain, false);

        assert!(rt.take_discord_gated(gated), "a gated guild owes a rebuild");
        assert!(!rt.take_discord_gated(gated), "and owes it only once");
        assert!(!rt.take_discord_gated(plain), "an ungated one never did");
    }

    /// The first attempt at this read the gateway payload only, found no
    /// member object for us, and reported every server as ungated. Absent
    /// still has to mean "no gate" - it is the ordinary case - which is
    /// exactly why the caller has to look somewhere else before deciding.
    #[test]
    fn a_missing_member_is_not_a_gate() {
        assert!(!member_is_pending(None));
        assert!(!member_is_pending(Some(&json!({ "roles": [] }))));
        assert!(!member_is_pending(Some(&json!({ "pending": false }))));
        assert!(member_is_pending(Some(&json!({ "pending": true }))));
    }

    #[test]
    fn roles_come_off_the_same_member_object() {
        assert_eq!(member_role_ids(None), Vec::<String>::new());
        assert_eq!(
            member_role_ids(Some(&json!({ "roles": ["1", "2"] }))),
            vec!["1".to_string(), "2".to_string()]
        );
        // A member object with no roles at all is ordinary, not an error.
        assert_eq!(member_role_ids(Some(&json!({ "pending": true }))), Vec::<String>::new());
    }
}
