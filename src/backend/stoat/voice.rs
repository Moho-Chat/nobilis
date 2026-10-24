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
    let answer = handle.api.post(&format!("/channels/{channel}/join_call"), &json!({ "node": node, "force_disconnect": true })).await.map_err(|e| anyhow!("{e}"))?;
    let (token, url) = (answer["token"].as_str(), answer["url"].as_str());
    match (token, url) {
        (Some(token), Some(url)) => Ok(json!({ "token": token, "url": url })),
        _ => Err(anyhow!("Stoat did not give a way into the call")),
    }
}
