//! Calls: asking Stoat for a way into a channel's call.
//!
//! Stoat's calls are held on a LiveKit media server, like Element's. The daemon asks for the token (`join_call`) - the
//! one thing that needs the session - and the window, which is a LiveKit client already, joins with it and sends the
//! microphone, the camera and a shared screen. Who is in each channel's call comes as events (see `events`), so a
//! channel can say who is talking before anybody joins.

use super::Handle;
use crate::model::Buffer;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// What the window needs to join: the media server's address and the account's token for it.
pub async fn join(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    // The instance says which media servers it has, and asks for one by name. A call this account is still in somewhere
    // else (another device, or a window that went away without leaving) is let go of: joining here is the newer wish.
    let node = handle.instance.voice_node.clone().ok_or_else(|| anyhow!("this instance has no voice"))?;
    // Beginning a call in a direct message or a group rings the others (on their phones, where the app is asleep); joining
    // one already going rings nobody.
    let recipients: Vec<String> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        match world.channels.get(channel) {
            Some(c) if matches!(c.kind, super::world::Kind::Direct | super::world::Kind::Group) && !world.voice.contains_key(channel) => c.recipients.iter().filter(|r| **r != world.own_id).cloned().collect(),
            _ => Vec::new(),
        }
    };
    let mut body = json!({ "node": node, "force_disconnect": true });
    if !recipients.is_empty() {
        body["recipients"] = json!(recipients);
    }
    let answer = handle.api.post(&format!("/channels/{channel}/join_call"), &body).await.map_err(|e| anyhow!("{e}"))?;
    let (token, url) = (answer["token"].as_str(), answer["url"].as_str());
    match (token, url) {
        (Some(token), Some(url)) => Ok(json!({ "token": token, "url": url, "note": super::limits::call_note(handle) })),
        _ => Err(anyhow!("Stoat did not give a way into the call")),
    }
}

/// Turns down a ring: Stoat stops ringing this account for the call. It is no error if it had already stopped.
pub async fn decline(state: &crate::state::AppState, handle: &Handle, buffer: &Buffer) -> Result<()> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    let own = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.ringing.remove(channel);
        world.own_id.clone()
    };
    state.events.emit("incomingCall", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "channelId": channel, "ringing": false }));
    let _ = handle.api.put(&format!("/channels/{channel}/end_ring/{own}"), None).await;
    Ok(())
}
