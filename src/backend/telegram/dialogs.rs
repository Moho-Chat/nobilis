//! The conversations an account has: asked for when the connection comes up, and each one's last message with it.
//!
//! Telegram's dialog list answers with every conversation and, in the same response, its last message, so one
//! request gives the list its names, kinds and recent activity without opening any of them. History proper is read
//! when a conversation is opened (see `history`).

use super::media;
use super::messages;
use super::Handle;
use super::peers;
use crate::state::AppState;
use anyhow::Result;
use grammers_client::tl;

/// How many conversations are read on connect. Telegram pages them a hundred at a time.
const LIMIT: usize = 200;

/// Makes a buffer of each of the account's conversations, newest activity first, and records the last message of each.
pub async fn sync(state: &AppState, account_id: &str, handle: &Handle) -> Result<usize> {
    let mut dialogs = handle.client.iter_dialogs();
    let mut count = 0;
    while count < LIMIT {
        let Some(dialog) = dialogs.next().await.map_err(|e| anyhow::anyhow!("{e}"))? else { break };
        let buffer = peers::buffer_for(state, account_id, dialog.peer());
        // Filed again each time: a bot read before bots had a heading of their own was filed with the people.
        // A group is its own entry on the rail and has no heading in the account's list.
        if peers::is_group(dialog.peer()) {
            peers::file_in_rail(state, account_id, &buffer, dialog.peer(), count as i64);
        } else {
            state.runtime.set_buffer_category(state, &buffer.id, Some(peers::category_of(dialog.peer())), buffer.position);
        }
        state.runtime.set_buffer_name_icon(state, &buffer.id, peers::name_icon_of(dialog.peer()));
        if let grammers_client::peer::Peer::User(user) = dialog.peer() {
            handle.names.lock().unwrap_or_else(|e| e.into_inner()).insert(user.id().bare_id_unchecked(), peers::title_of(dialog.peer()));
        }
        if let Some(last) = &dialog.last_message {
            messages::record_named(state, account_id, &handle.own, last, Some(&handle.names));
        }
        // Silenced on the account, perhaps on the phone.
        if let tl::enums::Dialog::Dialog(d) = &dialog.raw {
            state.runtime.set_silenced(state, &buffer.id, super::notify::silent(&d.notify_settings, chrono::Utc::now().timestamp()));
        }
        // Nothing unread there, as Telegram counts it: the window need not call it unread either.
        if matches!(&dialog.raw, tl::enums::Dialog::Dialog(d) if d.unread_count == 0) {
            state.events.emit("bufferRead", serde_json::json!({ "accountId": account_id, "bufferId": buffer.id }));
        }
        if let Some(avatar) = media::avatar_url(handle.own.user_id, dialog.peer()) {
            state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
        }
        // Whether the account may write there, read off the dialog's own copy of the chat (which carries its rights):
        // where it may not, the window shows why in place of the message box.
        let now = chrono::Utc::now().timestamp();
        state.runtime.set_buffer_read_only(state, &buffer.id, peers::read_only_for(dialog.peer(), now));
        count += 1;
    }
    tracing::info!("telegram[{account_id}]: {count} conversations");
    Ok(count)
}
