//! What this account may do to a guild's members, and doing it: removing, banning, timing out, and giving or taking
//! roles (`http-api/guild-members.mdx`, `guild-moderation.mdx`).
//!
//! What the member menu offers is read off this account's own permissions; Fluxer checks again and says why in words.
//! Roles rank by `position`, higher is more powerful, and an account may only hand out roles below its own highest
//! (the owner, any).

use super::world::{BAN_MEMBERS, KICK_MEMBERS, MANAGE_MESSAGES, MANAGE_ROLES, MODERATE_MEMBERS};
use super::Handle;
use crate::model::Buffer;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// The guild a conversation belongs to.
pub fn guild_of(handle: &Handle, buffer: &Buffer) -> Result<String> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))?;
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel).and_then(|c| c.guild.clone()).ok_or_else(|| anyhow!("this conversation is not in a community"))
}

/// The account's highest role position in a guild; the owner outranks everything.
pub fn own_top(world: &super::world::World, guild: &super::world::Guild) -> i64 {
    if guild.owner == world.own_id {
        return i64::MAX;
    }
    guild.members.get(&world.own_id).map(|m| m.roles.iter().filter_map(|r| guild.roles.get(r)).map(|r| r.position).max().unwrap_or(0)).unwrap_or(0)
}

/// A colour as the window writes it, `#rrggbb`, from Fluxer's 24-bit number (0 is none).
pub fn colour_of(n: u32) -> Option<String> {
    (n != 0).then(|| format!("#{n:06x}"))
}

/// What the menu may offer: the moderating this account's permissions allow, and the roles it may hand out.
pub fn powers(handle: &Handle, buffer: &Buffer) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let Some(channel) = buffer.remote_id.as_deref().and_then(|c| world.channels.get(c)) else { return json!({}) };
    let Some(guild) = channel.guild.as_ref().and_then(|g| world.guilds.get(g)) else { return json!({}) };
    let permissions = world.permissions(channel);
    let top = own_top(&world, guild);
    // Roles not above the account's own highest: Fluxer breaks a tie in position itself, and refuses in words where the
    // account may not.
    let held: Vec<&String> = guild.members.get(&world.own_id).map(|m| m.roles.iter().collect()).unwrap_or_default();
    let mut roles: Vec<(&String, &super::world::Role)> = guild.roles.iter().filter(|(id, r)| **id != guild.id && r.position <= top && (top == i64::MAX || !held.contains(id))).collect();
    roles.sort_by_key(|(_, r)| -r.position);
    json!({
        "canKick": permissions & KICK_MEMBERS != 0,
        "canBan": permissions & BAN_MEMBERS != 0,
        "canMute": permissions & MODERATE_MEMBERS != 0,
        "canAssignRoles": permissions & MANAGE_ROLES != 0,
        "canRedactOthers": permissions & MANAGE_MESSAGES != 0,
        "roles": if permissions & MANAGE_ROLES != 0 { roles.into_iter().map(|(id, r)| json!({ "id": id, "name": r.name, "colour": colour_of(r.color) })).collect::<Vec<_>>() } else { Vec::new() },
    })
}

/// When a timeout of this many minutes ends, as Fluxer reads it (at most 28 days, as Discord's).
pub fn timeout_until(now: i64, minutes: i64) -> String {
    chrono::DateTime::from_timestamp(now + minutes.clamp(1, 28 * 24 * 60) * 60, 0).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
}

/// Removes, bans or times out a member (`kick`, `ban`, `mute`), or lifts a ban or a timeout (`unban`, `unmute`).
pub async fn moderate(handle: &Handle, buffer: &Buffer, user: &str, action: &str, minutes: Option<i64>, reason: Option<&str>) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let member = format!("/guilds/{guild}/members/{user}");
    match action {
        "kick" => handle.api.delete(&member).await.map(|_| ()),
        "ban" => handle.api.put(&format!("/guilds/{guild}/bans/{user}"), Some(&json!({ "reason": reason, "delete_message_seconds": 0 }))).await.map(|_| ()),
        "unban" => handle.api.delete(&format!("/guilds/{guild}/bans/{user}")).await.map(|_| ()),
        "mute" => handle.api.patch(&member, &json!({ "communication_disabled_until": timeout_until(chrono::Utc::now().timestamp(), minutes.unwrap_or(10)) })).await.map(|_| ()),
        "unmute" => handle.api.patch(&member, &json!({ "communication_disabled_until": null })).await.map(|_| ()),
        other => bail!("Fluxer has no way to {other} a member"),
    }
    .map_err(|e| anyhow!("{e}"))
}

/// Gives a member a role, or takes it away.
pub async fn set_role(handle: &Handle, buffer: &Buffer, user: &str, role: &str, give: bool) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let path = format!("/guilds/{guild}/members/{user}/roles/{role}");
    if give { handle.api.put(&path, None).await } else { handle.api.delete(&path).await }.map(|_| ()).map_err(|e| anyhow!("{e}"))
}

/// Makes an invite to a channel's guild, and answers the link to give out (the instance's own invite address).
pub async fn invite(handle: &Handle, buffer: &Buffer) -> Result<String> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))?;
    let made = handle.api.post(&format!("/channels/{channel}/invites"), &json!({})).await.map_err(|e| anyhow!("{e}"))?;
    let code = made["code"].as_str().ok_or_else(|| anyhow!("Fluxer did not give an invite"))?;
    let base = if handle.instance.invite.is_empty() { format!("{}/invite", handle.instance.webapp) } else { handle.instance.invite.clone() };
    Ok(format!("{}/{code}", base.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_ends_the_stated_minutes_from_now_and_never_beyond_28_days() {
        assert_eq!(timeout_until(0, 10), "1970-01-01T00:10:00.000Z");
        assert_eq!(timeout_until(0, 0), "1970-01-01T00:01:00.000Z");
        assert_eq!(timeout_until(0, 1_000_000), "1970-01-29T00:00:00.000Z");
    }

    #[test]
    fn a_colour_is_written_as_the_window_does() {
        assert_eq!(colour_of(0xff8800).as_deref(), Some("#ff8800"));
        assert_eq!(colour_of(0), None);
    }
}
