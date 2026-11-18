//! What an administrator may do to a member: remove, ban, silence, let back in, and make an administrator or take
//! that away.
//!
//! The menu beside the member list asks what this account may do here, and offers only that. Telegram has the last
//! word: an administrator who lacks the particular right is refused ("Only an administrator may do that here").

use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_session::types::{PeerId, PeerKind};
use grammers_session::Session;
use serde_json::{json, Value};
use std::time::Duration;

/// How long a mute lasts, as everywhere else in moho.
const MUTE: Duration = Duration::from_secs(10 * 60);

/// What this account may do to others in a conversation, in the shape the member menu reads.
pub async fn permissions(state: &AppState, buffer: &Buffer) -> Result<Value, String> {
    let none = json!({ "canRedactOthers": false, "canKick": false, "canBan": false, "canMute": false, "canOp": false });
    if buffer.kind == "dm" {
        return Ok(none);
    }
    let (handle, peer) = peer_for(state, buffer).await?;
    let me = handle.client.get_me().await.map_err(|e| explain(&e))?;
    let Ok(Some(me)) = me.to_ref().await else { return Ok(none) };
    let Ok(found) = handle.client.get_permissions(peer, me).await else { return Ok(none) };
    let may = found.is_creator() || found.is_admin();
    // A small group can only have a member removed; bans and silences are for supergroups and channels.
    let supergroup = peer.id.kind() == PeerKind::Channel;
    // Making administrators is its own right; in a small group only the creator has it.
    let promote = found.is_creator() || (supergroup && found.can_add_admins());
    Ok(json!({ "canRedactOthers": may, "canKick": may, "canBan": may && supergroup, "canMute": may && supergroup, "canOp": promote }))
}

/// Removes, bans, silences or lets back in one member; or makes them an administrator (`op`) or an ordinary member
/// again (`deop`).
pub async fn moderate(state: &AppState, buffer: &Buffer, user_id: &str, action: &str) -> Result<(), String> {
    let id: i64 = user_id.parse().map_err(|_| "that member cannot be told apart from the others".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let user = handle.session.peer_ref(PeerId::user_unchecked(id)).await.ok().flatten().ok_or_else(|| "Telegram has not told this account about that member yet".to_string())?;
    let supergroup = peer.id.kind() == PeerKind::Channel;
    match action {
        "kick" => handle.client.kick_participant(peer, user).await.map_err(|e| explain(&e)),
        "ban" if supergroup => handle.client.set_banned_rights(peer, user).view_messages(false).await.map_err(|e| explain(&e)),
        "mute" if supergroup => handle.client.set_banned_rights(peer, user).send_messages(false).duration(MUTE).await.map_err(|e| explain(&e)),
        // Every right given back: the ban or silence is lifted. The person is not added back; they may rejoin.
        "unban" | "unmute" if supergroup => handle.client.set_banned_rights(peer, user).await.map_err(|e| explain(&e)),
        "ban" | "mute" | "unban" | "unmute" => Err("Telegram only lets a supergroup or a channel ban or silence somebody".to_string()),
        "op" => promote(handle.client.set_admin_rights(peer, user), broadcast(&handle, peer).await).await.map_err(|e| explain(&e)),
        // No rights at all is an ordinary member again.
        "deop" => handle.client.set_admin_rights(peer, user).await.map_err(|e| explain(&e)),
        _ => Err("Telegram has no such action".to_string()),
    }
}

/// Whether a conversation is a broadcast channel, where an administrator's work is posting rather than moderating.
async fn broadcast(handle: &super::Handle, peer: grammers_session::types::PeerRef) -> bool {
    matches!(handle.client.resolve_peer(peer).await, Ok(grammers_client::peer::Peer::Channel(_)))
}

/// The rights an administrator made from moho is given: what Telegram's own apps tick by default, short of making
/// administrators themselves, which stays the creator's to hand out.
fn promote<F: std::future::Future<Output = Result<(), grammers_client::InvocationError>>>(
    builder: grammers_client::peer::AdminRightsBuilder<F>,
    broadcast: bool,
) -> grammers_client::peer::AdminRightsBuilder<F> {
    builder
        .change_info(true)
        .delete_messages(true)
        .ban_users(true)
        .invite_users(true)
        .pin_messages(true)
        .manage_call(true)
        .post_messages(broadcast)
        .edit_messages(broadcast)
}
