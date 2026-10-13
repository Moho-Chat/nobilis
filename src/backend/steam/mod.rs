//! Steam - the friends list and direct messages of a Steam account, over the
//! same connection managers the official client uses.
//!
//! Steam is a directory of servers ("connection managers") that a client opens
//! a WebSocket to and speaks protobuf over. Everything a chat client does is
//! a message on that one connection: signing in, the friends list, who is
//! online, a message sent, a message received. There is no separate chat
//! server; the same socket that tells you a friend started a game delivers the
//! message they sent.
//!
//! # How this folder is arranged
//!
//! - `proto` - the messages, written out from Valve's own definitions
//! - `wire` - how a packet is laid out, and the batches that hold packets
//! - `steamid` - the two forms of an account's identity
//! - `eresult` - the result codes, said in words
//! - `auth` - signing in with a name and password, through Steam Guard, to a token
//! - `presence` - Steam's states and moho's
//! - `friends` - who the friends are, how they are doing, what each conversation is called
//! - `chat` - direct messages, in and out
//! - `login` - adding an account: the sign-in a person takes part in
//! - `connect` - an account's connection: sign in with the token, listen, reconnect
//! - `publish` - the roster as the window sees it: a conversation each, named, with a dot
//! - `cm` - finding a connection manager to connect to
//! - `link` - one connection: asking it things, and hearing what it says

pub mod auth;
pub mod bbcode;
pub mod chat;
pub mod cm;
pub mod connect;
pub mod emoticons;
pub mod eresult;
pub mod friends;
pub mod friendship;
pub mod groups;
pub mod invites;
pub mod history;
pub mod link;
pub mod login;
pub mod account;
pub mod apps;
pub mod blocked;
pub mod notices;
pub mod prefs;
pub mod presence;
pub mod profile;
#[cfg(test)]
mod probe;
pub mod proto;
pub mod publish;
pub mod reactions;
pub mod steamid;
pub mod web;
pub mod wire;

pub use connect::spawn;
pub use presence::apply_status;

use crate::state::AppState;

/// Tells Steam this session is being left, so it is not shown as signed in
/// for a while afterwards as one that was merely dropped. Written to the
/// connection; the caller keeps it open long enough for that to be sent.
pub fn say_goodbye(link: &link::Link) {
    let _ = link.send(wire::emsg::CLIENT_LOG_OFF, &proto::ClientLogOff {});
}

/// Ends a session politely: says goodbye, and lets go of the connection a
/// moment later, which is how long the goodbye takes to be written.
pub fn sign_off(link: link::Link) {
    say_goodbye(&link);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        drop(link);
    });
}

/// Says why a refusal may be what it is, when the account is one Steam holds back.
pub fn explained(account_id: &str, why: String) -> String {
    let facts = account::held(account_id);
    if facts.family_view.is_some_and(|(_, holds_friends)| holds_friends) {
        return format!("{why} (Family View is on for this account and holds friends and chat back; unlock it in Steam to chat)");
    }
    why
}

/// Sends a direct message in the conversation `buffer` is, and stores it.
pub async fn send_message(state: &AppState, buffer: &crate::model::Buffer, body: &str) -> Result<(), String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let recipient = buffer.remote_id.clone().ok_or("this conversation is not with a Steam account")?;
    let sent = chat::send(&link, &recipient, body).await.map_err(|why| {
        tracing::info!("steam[{}]: a message to {recipient} was not sent: {why}", buffer.account_id);
        explained(&buffer.account_id, why)
    })?;
    tracing::info!("steam[{}]: sent a message to {recipient} as {}", buffer.account_id, sent.id);
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_default();
    state.runtime.record_message_at(
        state,
        &buffer.account_id,
        &buffer.name,
        "dm",
        &own,
        &sent.shown,
        false,
        "chat",
        None,
        Some(sent.id),
        false,
        None,
        sent.embeds,
        Vec::new(),
        None,
        Some(i64::from(sent.timestamp)).filter(|t| *t > 0),
        None,
        None,
    );
    Ok(())
}

/// Sends files to a friend, and the words that came with them.
///
/// Steam has no caption: the words go first as a message of their own and each
/// file is a message after it, which is how its own client sends the two. The
/// messages are written into the conversation by Steam and come back to this
/// session as the echo of what another session sent, so nothing is recorded
/// here. Stops at the first file Steam refuses and says why.
pub async fn send_files(
    state: &AppState,
    buffer: &crate::model::Buffer,
    caption: &str,
    files: &[(String, bool)],
    progress: Option<&crate::upload::Progress>,
) -> Result<(), String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let friend = buffer.remote_id.clone().ok_or("this conversation is not with a Steam account")?;
    let config = state.accounts.get_steam(&buffer.account_id).ok_or("this Steam account is not connected")?;
    // Refused before anything is sent, so a bad file does not leave the words
    // sent and the pictures not.
    for (path, _) in files {
        let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
        let size = std::fs::metadata(path).map(|m| m.len()).map_err(|e| format!("reading {name}: {e}"))?;
        if let Some(why) = web::refusal(name, size) {
            return Err(why);
        }
    }
    if !caption.trim().is_empty() {
        send_message(state, buffer, caption).await?;
    }
    for (path, spoiler) in files {
        let url = web::send_file(&link, &buffer.account_id, config.steamid, &config.refresh_token, &friend, path, *spoiler, progress)
            .await
            .inspect_err(|why| tracing::info!("steam[{}]: a file to {friend} was not sent: {why}", buffer.account_id))?;
        tracing::info!("steam[{}]: sent a file to {friend}: {url}", buffer.account_id);
    }
    Ok(())
}
