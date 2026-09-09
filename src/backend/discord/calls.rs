//! Voice channels and calls, as far as the gateway is concerned.
//!
//! Joining, leaving, ringing and being rung. The audio itself is `voice`
//! beside this - what is here is the part Discord's gateway negotiates
//! before any of it can start.

use super::*;

/// What to call the person a voice state belongs to.
///
/// Discord attaches the member to a voice state, which matters because
/// somebody sitting in a voice channel is frequently in no member list the
/// client has loaded - a nickname first, since that is what they chose to be
/// called here, then the display name, then the account name.
pub(super) fn voice_member_name(vs: &Value) -> Option<&str> {
    vs["member"]["nick"]
        .as_str()
        .or_else(|| vs["member"]["user"]["global_name"].as_str())
        .or_else(|| vs["member"]["user"]["username"].as_str())
        .filter(|n| !n.is_empty())
}

/// Their picture, off the member Discord attaches to a voice state.
///
/// A guild avatar wins over the account's own where one is set: it is the
/// face that server knows them by, and showing the other one in that server's
/// call is the same mistake as showing their global name over their nickname.
///
/// `None` for a direct call, whose voice states carry no member at all - the
/// roster falls back to whatever a message of theirs already taught us.
pub(super) fn voice_member_avatar(vs: &Value) -> Option<String> {
    let member = &vs["member"];
    let user_id = vs["user_id"].as_str()?;
    if let Some(hash) = member["avatar"].as_str() {
        // The per-guild avatar lives under the guild, not the user.
        if let Some(guild_id) = vs["guild_id"].as_str() {
            let ext = if hash.starts_with("a_") { "gif" } else { "png" };
            return Some(format!(
                "https://cdn.discordapp.com/guilds/{guild_id}/users/{user_id}/avatars/{hash}.{ext}"
            ));
        }
    }
    let user = &member["user"];
    author_avatar_url(user).or_else(|| default_avatar_url(user))
}

/// Tells clients that a voice channel's membership changed.
///
/// Carries the guild rather than the channel because a client showing a
/// channel list needs to know that list is stale, and somebody leaving one
/// channel for another changes two of its rows at once.
pub(super) fn announce_voice_membership(state: &AppState, account_id: &str, guild_id: Option<&str>, channel_id: Option<&str>) {
    // A leave carries no guild, so it is recovered from the channel that was
    // left; without this, leaving would never refresh anyone's list.
    let guild = guild_id
        .map(String::from)
        .or_else(|| channel_id.and_then(|c| state.runtime.discord_guild_of_voice_channel(account_id, c)));
    let Some(guild_id) = guild else { return };
    state.events.emit(
        "voiceMembershipChanged",
        json!({ "accountId": account_id, "guildId": guild_id }),
    );
}

/// Joins a voice channel under the given options.
///
/// With `solo` set - the default, and what a caller should use against a
/// server full of strangers - an occupied channel is refused outright and
/// anyone arriving later ends the session. The check lives here rather than in
/// the caller so it cannot be forgotten, but it is an argument rather than a
/// law: in a guild the user controls, a listener joining is the point.
///
/// Joining is opcode 4 on the main gateway, which the server answers with
/// VOICE_STATE_UPDATE (our session id) and VOICE_SERVER_UPDATE (where to
/// connect and with what token).
pub fn join_voice(
    state: &AppState,
    account_id: &str,
    guild_id: Option<&str>,
    channel_id: &str,
    options: voice::VoiceOptions,
) -> Result<()> {
    let config = state.accounts.get_discord(account_id).context("no such Discord account")?;
    let sender = state.runtime.discord_gateway_sender(account_id).context("account is not connected")?;

    if options.solo {
        let occupants = state.runtime.discord_voice_occupants(account_id, channel_id, &config.user_id);
        if !occupants.is_empty() {
            bail!("channel is not empty - {} already in it", occupants.len());
        }
    }
    // Recorded before the join, since the handshake it triggers can complete
    // before this function returns.
    state.voice.set_options(account_id, options);

    sender.send(
        json!({
            "op": 4,
            "d": {
                // Null for a one-to-one call: a DM belongs to no guild, and
                // sending one anyway gets the frame ignored.
                "guild_id": guild_id,
                "channel_id": channel_id,
                // A session that carries no audio joins muted and deafened:
                // showing an open microphone to a room that cannot hear one
                // would misrepresent what is happening.
                "self_mute": !options.transmit,
                "self_deaf": !options.transmit,
                // Discord's own client always sends this field; omitting it
                // gets the frame accepted and then ignored, with no error.
                "self_video": false
            }
        })
        .to_string(),
    )?;
    Ok(())
}

/// Calls someone directly, opening the conversation if there isn't one.
///
/// A one-to-one call is a voice connection to a DM channel, which is most of
/// what makes it different: no guild, and nobody is in it until the other
/// person picks up. Joining alone is silent, so the ring is a separate request
/// - without it you are sitting in an empty channel they never hear about.
pub async fn call_user(state: &AppState, account_id: &str, user_id: &str) -> Result<String> {
    let buffer_id = open_dm(state, account_id, user_id).await?;
    let channel_id = state.runtime.get_discord_channel(&buffer_id).context("the DM has no channel")?;
    start_call(state, account_id, &channel_id).await?;
    Ok(buffer_id)
}

/// Tells frontends a conversation has started or stopped ringing.
///
/// Deliberately carries no name or picture. Which conversation it is, is the
/// answer - every frontend already draws that person's name and avatar in its
/// own list, and a second copy sourced from here would be the one that goes
/// stale. A channel with no buffer yet is a DM this account has never opened;
/// there is nothing to ring against, so it is dropped rather than invented.
pub fn announce_call(state: &AppState, account_id: &str, channel_id: &str, ringing: bool) {
    let Some(buffer_id) = state.runtime.discord_buffer_for_channel(account_id, channel_id) else { return };
    if !state.runtime.set_ringing(&buffer_id, account_id, channel_id, ringing) {
        return;
    }
    state.events.emit(
        "incomingCall",
        json!({
            "accountId": account_id,
            "bufferId": buffer_id,
            "channelId": channel_id,
            "ringing": ringing
        }),
    );
}

/// Joins a call that is already ringing, which is what answering one is.
///
/// The same join as placing a call, minus the ring: the other end is already
/// in the channel waiting, and ringing them back would make their client
/// chime at a call they started.
pub fn accept_call(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    let options = voice::VoiceOptions { solo: false, transmit: true };
    join_voice(state, account_id, None, channel_id, options)?;
    // Answered, so it is no longer ringing - said here rather than waiting for
    // Discord to say it, since the person who pressed the button should not
    // watch it go on ringing for a round trip afterwards.
    announce_call(state, account_id, channel_id, false);
    Ok(())
}

/// Turns down a call without ending it for anyone else.
///
/// Names this account as the recipient to stop ringing rather than passing the
/// null that means "everyone in the conversation": in a group DM, declining is
/// a statement about yourself, and hanging up on the other four people is not
/// what pressing it means.
pub async fn decline_call(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let _ = send_write(
        http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/call/stop-ringing"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "recipients": [&cfg.user_id] }))
        )
    .await;
    announce_call(state, account_id, channel_id, false);
    Ok(())
}

/// Joins a DM's voice channel and rings whoever else is in it.
pub async fn start_call(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    // Never solo-only: a call whose whole purpose is somebody else joining
    // cannot also refuse to be joined.
    let options = voice::VoiceOptions { solo: false, transmit: true };
    join_voice(state, account_id, None, channel_id, options)?;
    ring(state, account_id, channel_id).await
}

/// Makes the other end's client ring.
///
/// Sent after joining rather than before: ringing a call you are not yet in is
/// answered by Discord with a call that ends the moment they accept it.
pub async fn ring(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client()
            .post(format!("{API_BASE}/channels/{channel_id}/call/ring"))
            .header("Authorization", &cfg.token)
            // A null recipient list means everyone in the conversation, which
            // for a one-to-one DM is the one person there is.
            .json(&json!({ "recipients": Value::Null })),
    )
    .await
    .context("ringing")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Stops a call ringing, for hanging up before it is answered.
pub async fn stop_ringing(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let _ = send_write(
        http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/call/stop-ringing"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "recipients": Value::Null }))
        )
    .await;
    Ok(())
}

/// Tells the server this account's microphone or output has been silenced.
///
/// Separate from actually stopping the audio, and both are needed: closing the
/// microphone without saying so leaves everyone else looking at a live
/// microphone icon wondering why you have gone quiet.
pub fn announce_voice_flags(state: &AppState, account_id: &str, muted: bool, deafened: bool) -> bool {
    let Some(sender) = state.runtime.discord_gateway_sender(account_id) else { return false };
    let Some((guild_id, channel_id)) = state.voice.current_channel(account_id) else { return false };
    sender
        .send(
            json!({
                "op": 4,
                "d": {
                    // Null on a one-to-one call, which belongs to no guild.
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "self_mute": muted,
                    "self_deaf": deafened,
                    "self_video": false
                }
            })
            .to_string(),
        )
        .is_ok()
}

/// Leaves whatever voice channel this account is in. Safe to call when in none.
pub fn leave_voice(state: &AppState, account_id: &str) -> bool {
    let Some(sender) = state.runtime.discord_gateway_sender(account_id) else { return false };
    // Hanging up before they answer has to stop the ringing too, or their
    // phone goes on buzzing for a call that no longer exists.
    if let Some((guild, channel)) = state.voice.current_channel(account_id) {
        if guild.is_none() {
            let (s2, a2, c2) = (state.clone(), account_id.to_string(), channel);
            tokio::spawn(async move {
                let _ = stop_ringing(&s2, &a2, &c2).await;
            });
        }
    }
    state.runtime.set_discord_voice_self(account_id, None);
    sender
        .send(json!({ "op": 4, "d": { "guild_id": null, "channel_id": null, "self_mute": true, "self_deaf": true } }).to_string())
        .is_ok()
}
