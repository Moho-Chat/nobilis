//! The conversations an account has: asked for when the connection comes up, and each one's last message with it.
//!
//! Telegram's dialog list answers with every conversation and, in the same response, its last message, so one
//! request gives the list its names, kinds and recent activity without opening any of them. History proper is read
//! when a conversation is opened (see `history`).

use super::messages::{self, Own};
use super::peers;
use crate::state::AppState;
use anyhow::Result;
use grammers_client::Client;

/// How many conversations are read on connect. Telegram pages them a hundred at a time.
const LIMIT: usize = 200;

/// Makes a buffer of each of the account's conversations, newest activity first, and records the last message of each.
pub async fn sync(state: &AppState, account_id: &str, client: &Client, own: &Own) -> Result<usize> {
    let mut dialogs = client.iter_dialogs();
    let mut count = 0;
    while count < LIMIT {
        let Some(dialog) = dialogs.next().await.map_err(|e| anyhow::anyhow!("{e}"))? else { break };
        let buffer = peers::buffer_for(state, account_id, dialog.peer());
        if let Some(last) = &dialog.last_message {
            messages::record(state, account_id, own, last);
        }
        let _ = buffer;
        count += 1;
    }
    tracing::info!("telegram[{account_id}]: {count} conversations");
    Ok(count)
}
