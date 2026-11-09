//! Saying things: what the window's send, react, delete and edit do for a Signal conversation.
//!
//! Each asks the account's thread to do it (see `commands`), waits for the answer, and - only once Signal has taken the
//! message - draws it. A message that did not go is not left in the conversation as though it had.

use super::{commands, compose, ids, ranges, Command, Done, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use presage::store::Thread;
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a send may take before it is called lost. Signal itself retries; the window should not wait forever.
const PATIENCE: Duration = Duration::from_secs(90);

fn target(state: &AppState, buffer: &Buffer) -> Result<(Handle, Thread), String> {
    let handle = state.runtime.signal_handle(&buffer.account_id).ok_or("that Signal account is not connected")?;
    let thread = buffer.remote_id.as_deref().and_then(ids::parse_remote_id).ok_or("that conversation is not a Signal one")?;
    Ok((handle, thread))
}

async fn ask(handle: &Handle, make: impl FnOnce(Done) -> Command) -> Result<u64, String> {
    let (done, answer) = oneshot::channel();
    handle.commands.send(make(done)).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(PATIENCE, answer).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}

/// Sends text, optionally as a reply to the message `reply_to`.
pub async fn text(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (plain, styles) = compose::parse(body);
    if plain.trim().is_empty() {
        return Err("there is nothing to send".to_string());
    }
    let quote = reply_to.and_then(|id| commands::quote_of(state, &buffer.id, id));
    let reply = reply_to.zip(quote.as_ref()).map(|(id, q)| crate::model::ReplyPreview {
        id: id.to_string(),
        from: q.author_aci.as_deref().and_then(|a| uuid::Uuid::parse_str(a).ok()).map(|a| name_here(state, buffer, &a, &handle)).unwrap_or_default(),
        body: q.text.clone().unwrap_or_default(),
        thread: false,
        forwarded: false,
        thread_root: None,
        chat: None,
    });
    let (plain_for_wire, ranges_for_wire) = (plain.clone(), styles.clone());
    let at = ask(&handle, |done| Command::Send { thread, body: plain_for_wire, ranges: ranges_for_wire, quote, done }).await?;
    let drawn = ranges::render(&plain, &styles, |_| "someone".to_string());
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_else(|| "Me".to_string());
    state.runtime.record_message_at(
        state, &buffer.account_id, &buffer.name, &buffer.kind, &own, &drawn.body, false, "chat", reply, Some(ids::message_id(at, &handle.own)), false, None, Vec::new(), Vec::new(), Some(handle.own.to_string()), Some((at / 1000) as i64), drawn.html, None,
    );
    Ok(())
}

/// The name already on a message in this conversation, for a quote's author.
fn name_here(state: &AppState, buffer: &Buffer, author: &uuid::Uuid, handle: &Handle) -> String {
    if *author == handle.own {
        return state.runtime.own_identity(&buffer.account_id).unwrap_or_default();
    }
    state
        .store
        .get_backlog(&buffer.id, i64::MAX, 200)
        .ok()
        .and_then(|rows| rows.into_iter().find(|m| m.sender_id.as_deref() == Some(author.to_string().as_str())).map(|m| m.from))
        .unwrap_or_default()
}

/// Reacts to message `msg_id`, or takes the reaction back.
pub async fn react(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (target_ms, target_author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    let emoji_owned = emoji.to_string();
    ask(&handle, |done| Command::React { thread, target_ms, target_author, emoji: emoji_owned, remove: !add, done }).await?;
    state.runtime.update_reaction(state, &buffer.id, msg_id, emoji, true, add);
    Ok(())
}

/// Takes one of the account's own messages back, for everyone.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (target_ms, author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    if author != handle.own {
        return Err("Only your own messages can be taken back".to_string());
    }
    ask(&handle, |done| Command::Delete { thread, target_ms, done }).await?;
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(())
}

/// Changes one of the account's own messages.
pub async fn edit(state: &AppState, buffer: &Buffer, msg_id: &str, body: &str) -> Result<(), String> {
    let (handle, thread) = target(state, buffer)?;
    let (target_ms, author) = ids::parse_message_id(msg_id).ok_or("that is not a Signal message")?;
    if author != handle.own {
        return Err("Only your own messages can be changed".to_string());
    }
    let (plain, styles) = compose::parse(body);
    if plain.trim().is_empty() {
        return Err("a message cannot be changed to nothing; delete it instead".to_string());
    }
    let (wire_body, wire_ranges) = (plain.clone(), styles);
    ask(&handle, |done| Command::Edit { thread, target_ms, body: wire_body, ranges: wire_ranges, done }).await?;
    let held = state.store.get_message(&buffer.id, msg_id).ok().flatten();
    let (embeds, attachments) = held.map(|m| (m.embeds, m.attachments)).unwrap_or_default();
    state.runtime.update_message(state, &buffer.id, msg_id, &plain, &embeds, &attachments);
    Ok(())
}
