//! Joining and leaving: a public chat by its @name or t.me link, a private one by its invite link, and leaving what
//! the account is in.
//!
//! A chat that was joined arrives as a dialog, so the list is read again afterwards and the window opens the new
//! conversation as it does for any service.

use super::dialogs;
use super::peers;
use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_session::types::PeerKind;

/// What a person typed to name somewhere to join.
#[derive(Debug, PartialEq)]
pub enum Target {
    /// A public name: a person, a group or a channel.
    Username(String),
    /// The secret part of a private invite link.
    Invite(String),
}

/// Reads `@name`, `name`, `t.me/name`, `https://t.me/+hash`, `t.me/joinchat/hash` and `tg://join?invite=hash`.
pub fn parse_target(input: &str) -> Option<Target> {
    let text = input.trim();
    if let Some(rest) = text.strip_prefix("tg://join?invite=") {
        return clean_invite(rest).map(Target::Invite);
    }
    let rest = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")).unwrap_or(text);
    let path = ["t.me/", "telegram.me/", "telegram.dog/"].iter().find_map(|host| rest.strip_prefix(host));
    let Some(path) = path else {
        let name = text.strip_prefix('@').unwrap_or(text);
        return valid_username(name).then(|| Target::Username(name.to_string()));
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let first = parts.next()?;
    if first == "joinchat" {
        return clean_invite(parts.next()?).map(Target::Invite);
    }
    if let Some(hash) = first.strip_prefix('+') {
        return clean_invite(hash).map(Target::Invite);
    }
    valid_username(first).then(|| Target::Username(first.to_string()))
}

fn clean_invite(hash: &str) -> Option<String> {
    let hash = hash.trim();
    (!hash.is_empty() && hash.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| hash.to_string())
}

/// Telegram's rule: 5 to 32 letters, digits and underscores, starting with a letter.
fn valid_username(name: &str) -> bool {
    (5..=32).contains(&name.len()) && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Joins what was named, and makes its conversation.
pub async fn join(state: &AppState, account_id: &str, input: &str) -> Result<(), String> {
    let target = parse_target(input).ok_or_else(|| format!("\"{}\" is not a Telegram @name or t.me link", input.trim()))?;
    let handle = state.runtime.telegram_handle(account_id).ok_or_else(|| "that Telegram account is not connected".to_string())?;
    match target {
        Target::Invite(hash) => {
            handle.client.invoke(&tl::functions::messages::ImportChatInvite { hash }).await.map_err(|e| match &e {
                grammers_client::InvocationError::Rpc(r) if r.name == "INVITE_HASH_EXPIRED" || r.name == "INVITE_HASH_INVALID" => "That invite link is no longer valid".to_string(),
                grammers_client::InvocationError::Rpc(r) if r.name == "INVITE_REQUEST_SENT" => "Your request to join was sent: an administrator has to accept it".to_string(),
                _ => explain(&e),
            })?;
        }
        Target::Username(name) => {
            let peer = handle.client.resolve_username(&name).await.map_err(|e| explain(&e))?.ok_or_else(|| format!("Nobody on Telegram is called @{name}"))?;
            match &peer {
                // A person: the conversation with them is all there is to open.
                Peer::User(_) => {
                    let buffer = peers::buffer_for(state, account_id, &peer);
                    if let Some(avatar) = super::media::avatar_url(handle.own.user_id, &peer) {
                        state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
                    }
                    return Ok(());
                }
                _ => {
                    let peer_ref = peer.to_ref().await.ok().flatten().ok_or_else(|| "Telegram did not say how to reach that chat".to_string())?;
                    handle.client.join_chat(peer_ref).await.map_err(|e| match &e {
                        grammers_client::InvocationError::Rpc(r) if r.name == "INVITE_REQUEST_SENT" => "Your request to join was sent: an administrator has to accept it".to_string(),
                        _ => explain(&e),
                    })?;
                }
            }
        }
    }
    dialogs::sync(state, account_id, &handle).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Leaves a conversation: a channel or supergroup is left, a small group is left, a private chat is cleared.
pub async fn leave(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    match peer.id.kind() {
        PeerKind::Channel => {
            handle.client.invoke(&tl::functions::channels::LeaveChannel { channel: peer.into() }).await.map_err(|e| explain(&e))?;
        }
        PeerKind::Chat => {
            handle.client.invoke(&tl::functions::messages::DeleteChatUser { revoke_history: false, chat_id: peer.into(), user_id: tl::enums::InputUser::UserSelf }).await.map_err(|e| explain(&e))?;
        }
        _ => {
            // History is deleted from this account's side only; the other person keeps theirs.
            handle.client.invoke(&tl::functions::messages::DeleteHistory { just_clear: false, revoke: false, peer: peer.into(), max_id: 0, min_date: None, max_date: None }).await.map_err(|e| explain(&e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_links_are_read() {
        let user = |n: &str| Some(Target::Username(n.to_string()));
        let invite = |h: &str| Some(Target::Invite(h.to_string()));
        assert_eq!(parse_target("@abcd"), None, "too short for Telegram, so not a name");
        assert_eq!(parse_target("@disclosetv"), user("disclosetv"));
        assert_eq!(parse_target("disclosetv"), user("disclosetv"));
        assert_eq!(parse_target("https://t.me/disclosetv"), user("disclosetv"));
        assert_eq!(parse_target("t.me/disclosetv/1234?single"), user("disclosetv"));
        assert_eq!(parse_target("https://t.me/+AbC_d-123"), invite("AbC_d-123"));
        assert_eq!(parse_target("https://t.me/joinchat/AbC_d-123"), invite("AbC_d-123"));
        assert_eq!(parse_target("tg://join?invite=AbC_d-123"), invite("AbC_d-123"));
        assert_eq!(parse_target("two words"), None);
        assert_eq!(parse_target("1startsdigit"), None);
        assert_eq!(parse_target(""), None);
    }
}
