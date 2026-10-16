//! Saying things in a conversation: send, answer, edit, delete, "typing", "read".
//!
//! Each starts from the buffer: its `remote_id` names the peer, and the session turns that into the reference
//! Telegram wants with it (an id with the authority to use it). A message that was sent is recorded here as it is
//! returned, because Telegram does not send an account its own message back as an update.

use super::markup;
use super::messages;
use super::peers;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::message::InputMessage;
use grammers_client::{tl, InvocationError};
use grammers_session::types::PeerRef;
use grammers_session::Session;

/// What Telegram allows in one message, counted the way it counts (UTF-16 units).
pub const MAX_UNITS: usize = 4096;

/// Telegram's refusal, said so a person can act on it.
pub fn explain(error: &InvocationError) -> String {
    match error {
        InvocationError::Rpc(rpc) => {
            let n = rpc.value.unwrap_or(0);
            match rpc.name.as_str() {
                "FLOOD_WAIT" | "FLOOD_PREMIUM_WAIT" => format!("Telegram says to wait {n} seconds before doing that again"),
                "SLOWMODE_WAIT" => format!("Slow mode is on here: wait {n} seconds between messages"),
                "CHAT_WRITE_FORBIDDEN" | "CHAT_SEND_PLAIN_FORBIDDEN" => peers::NO_SEND_PERMISSION.to_string(),
                "USER_BANNED_IN_CHANNEL" => "You are banned from sending messages in this chat".to_string(),
                "USER_IS_BLOCKED" => "This person has blocked you".to_string(),
                "USER_PRIVACY_RESTRICTED" => "This person's privacy settings do not let you message them".to_string(),
                "PEER_FLOOD" => "Telegram is limiting how many people this account may start a conversation with".to_string(),
                "MESSAGE_TOO_LONG" => format!("That message is too long: Telegram takes at most {MAX_UNITS} characters"),
                "MESSAGE_EMPTY" => "There is nothing to send".to_string(),
                "MESSAGE_NOT_MODIFIED" => "That is already what the message says".to_string(),
                "MESSAGE_EDIT_TIME_EXPIRED" => "Telegram no longer lets this message be edited".to_string(),
                "MESSAGE_ID_INVALID" | "MESSAGE_NOT_FOUND" => "That message is no longer there".to_string(),
                "CHAT_ADMIN_REQUIRED" => "Only an administrator may do that here".to_string(),
                "MEDIA_EMPTY" | "FILE_PART_INVALID" => "Telegram would not take that file".to_string(),
                "FILE_REFERENCE_EXPIRED" => "Telegram needs the message read again before that file can be downloaded".to_string(),
                other => format!("Telegram refused: {}", other.to_lowercase().replace('_', " ")),
            }
        }
        InvocationError::Io(e) => format!("Telegram could not be reached: {e}"),
        other => format!("{other}"),
    }
}

/// The account's connection and Telegram's reference to the conversation a buffer is.
pub async fn peer_for(state: &AppState, buffer: &Buffer) -> Result<(Handle, PeerRef), String> {
    let handle = state.runtime.telegram_handle(&buffer.account_id).ok_or_else(|| "that Telegram account is not connected".to_string())?;
    let id = buffer.remote_id.as_deref().and_then(peers::peer_id_of).ok_or_else(|| "this is not a Telegram conversation".to_string())?;
    let peer = handle.session.peer_ref(id).await.ok().flatten().ok_or_else(|| "Telegram has not told this account about that conversation yet".to_string())?;
    Ok((handle, peer))
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Text from the message box, marks taken out and entities in place, if Telegram would take it.
pub fn checked(body: &str) -> Result<(String, Vec<tl::enums::MessageEntity>), String> {
    let (text, entities) = markup::parse(body);
    if text.trim().is_empty() {
        return Err("There is nothing to send".to_string());
    }
    if units(&text) > MAX_UNITS {
        return Err(format!("That message is too long: Telegram takes at most {MAX_UNITS} characters, and this has {}", units(&text)));
    }
    Ok((text, entities))
}

/// The message box's text as a message.
pub fn input_for(body: &str) -> Result<InputMessage, String> {
    let (text, entities) = checked(body)?;
    Ok(InputMessage::new().text(text).fmt_entities(entities))
}

/// Sends a message, as an answer to another if `reply_to` names one.
pub async fn send(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    let input = input_for(body)?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let input = input.reply_to(reply_to.and_then(|id| id.parse::<i32>().ok()));
    let sent = handle.client.send_message(peer, input).await.map_err(|e| explain(&e))?;
    messages::record(state, &buffer.account_id, &handle.own, &sent);
    Ok(())
}

/// Changes what one of the account's own messages says.
pub async fn edit(state: &AppState, buffer: &Buffer, msg_id: &str, body: &str) -> Result<(), String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot be edited".to_string())?;
    let input = input_for(body)?;
    let (handle, peer) = peer_for(state, buffer).await?;
    handle.client.edit_message(peer, id, input).await.map_err(|e| explain(&e))?;
    let (text, _) = markup::parse(body);
    state.runtime.update_message(state, &buffer.id, msg_id, &text, &[], &[]);
    Ok(())
}

/// Removes a message: for everyone where Telegram allows it, or only from this account's own view.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str, for_everyone: bool) -> Result<(), String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot be deleted".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    if for_everyone || peer.id.kind() == grammers_session::types::PeerKind::Channel {
        handle.client.delete_messages(peer, &[id]).await.map_err(|e| explain(&e))?;
    } else {
        handle.client.invoke(&tl::functions::messages::DeleteMessages { revoke: false, id: vec![id] }).await.map_err(|e| explain(&e))?;
    }
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(())
}

/// Says the account is typing. Telegram lets it lapse after a few seconds, so there is no "stopped".
pub async fn typing(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    handle.client.action(peer).oneshot(tl::enums::SendMessageAction::SendMessageTypingAction).await.map_err(|e| explain(&e))
}

/// Says everything in the conversation has been read, so its unread count goes down on the account's other apps.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    handle.client.mark_as_read(peer).await.map_err(|e| explain(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::RpcError;

    fn rpc(code: i32, name: &str, value: Option<u32>) -> InvocationError {
        InvocationError::Rpc(RpcError { code, name: name.to_string(), value, caused_by: None })
    }

    #[test]
    fn telegrams_refusals_are_said_in_words_with_their_numbers() {
        assert_eq!(explain(&rpc(420, "FLOOD_WAIT", Some(30))), "Telegram says to wait 30 seconds before doing that again");
        assert_eq!(explain(&rpc(420, "SLOWMODE_WAIT", Some(10))), "Slow mode is on here: wait 10 seconds between messages");
        assert_eq!(explain(&rpc(403, "CHAT_WRITE_FORBIDDEN", None)), peers::NO_SEND_PERMISSION);
        assert!(explain(&rpc(400, "MESSAGE_TOO_LONG", None)).contains("4096"));
        assert_eq!(explain(&rpc(400, "SOMETHING_NEW_HAPPENED", None)), "Telegram refused: something new happened");
    }

    #[test]
    fn what_cannot_be_sent_is_stopped_before_anything_is_sent() {
        assert!(checked("   ").unwrap_err().contains("nothing to send"));
        let long = "x".repeat(MAX_UNITS + 1);
        assert!(checked(&long).unwrap_err().contains("too long"));
        assert!(checked(&"x".repeat(MAX_UNITS)).is_ok());
        // An emoji is two units, and Telegram counts units.
        assert!(checked(&"😀".repeat(MAX_UNITS / 2 + 1)).unwrap_err().contains("too long"));
        // Formatting marks do not count against the limit: they are taken out.
        let (text, entities) = checked("**hello**").unwrap();
        assert_eq!((text.as_str(), entities.len()), ("hello", 1));
    }
}
