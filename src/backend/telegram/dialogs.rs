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

/// What the account's folders look at in a conversation, noted for working out what each folder holds.
fn note_for_folders(account_id: &str, dialog: &grammers_client::peer::Dialog) {
    use super::folders::{Facts, Kind};
    use grammers_client::peer::Peer;
    let (kind, contact) = match dialog.peer() {
        Peer::User(user) if user.is_bot() => (Kind::Bot, false),
        Peer::User(user) => (Kind::Person, user.contact()),
        Peer::Group(_) | Peer::Community(_) => (Kind::Group, false),
        Peer::Channel(_) => (Kind::Broadcast, false),
    };
    let tl::enums::Dialog::Dialog(d) = &dialog.raw else { return };
    let facts = Facts {
        kind,
        contact,
        muted: super::notify::silent(&d.notify_settings, chrono::Utc::now().timestamp()),
        unread: d.unread_count > 0 || d.unread_mark,
        // Telegram keeps its archive as the folder numbered 1.
        archived: d.folder_id == Some(1),
    };
    super::folders::note(account_id, peers::remote_id(dialog.peer().id()), facts);
}

/// Makes a buffer of each of the account's conversations, newest activity first, and records the last message of each.
pub async fn sync(state: &AppState, account_id: &str, handle: &Handle) -> Result<usize> {
    let mut dialogs = handle.client.iter_dialogs();
    let mut count = 0;
    while count < LIMIT {
        let Some(dialog) = dialogs.next().await.map_err(|e| anyhow::anyhow!("{e}"))? else { break };
        note_for_folders(account_id, &dialog);
        // A group with Topics is a conversation for each topic, found by asking the group.
        if super::topics::is_forum(dialog.peer()) {
            peers::rail_for(state, account_id, dialog.peer(), count as i64);
            if let Err(e) = super::topics::sync(state, account_id, handle, dialog.peer()).await {
                tracing::warn!("telegram[{account_id}]: reading a group's topics: {e:#}");
            }
            if let Some(last) = &dialog.last_message {
                messages::record_named(state, account_id, &handle.own, last, Some(&handle.names));
            }
            count += 1;
            continue;
        }
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
