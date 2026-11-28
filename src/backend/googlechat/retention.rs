//! History off: Google Chat's 24-hour messages.
//!
//! A conversation with history turned off keeps each message for a day, then deletes it - for everybody, on the
//! server. moho keeps scrollback of its own, so it has to delete them too, at the same time, or the "off the record"
//! conversation would be on the record here.
//!
//! What the wire says (seen live 2026-11-27, `update_group_retention_settings` in a test space):
//!
//! - every message sent while history is off carries `retention_settings { state: EPHEMERAL_ONE_DAY, expiry_timestamp
//!   }` - the time it will be deleted, in microseconds; that is what each message's expiry is taken from (and its
//!   sending time plus a day, when the time is missing);
//! - the conversation's own setting is in the conversation list (`read_state.retention_settings`, `group_lite`'s) and
//!   in `get_group`;
//! - turning it on or off posts a system message carrying a `GroupRetentionSettingsUpdatedMetaData` annotation (type
//!   19) with the new setting.
//!
//! Turning history off does not touch what was already said: only messages sent while it is off expire.
//!
//! The expiry of each message is kept in the store (`message_expiry`), so a message still leaves at its time when moho
//! was not running then: the sweep at the next start takes everything that is due.

use super::{pb, Group, Handle};
use crate::model::BufferNotice;
use crate::state::AppState;
use pb::retention_settings::RetentionState;

/// A day, which is how long a message lives with history off.
pub const EPHEMERAL_SECS: i64 = 24 * 60 * 60;

/// How often scrollback is looked through for what has expired.
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The notice kind a conversation with history off carries.
pub const NOTICE_KIND: &str = "historyOff";

pub fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Whether a retention setting is history-off.
pub fn is_ephemeral(settings: Option<&pb::RetentionSettings>) -> bool {
    settings.is_some_and(|r| r.state() == RetentionState::EphemeralOneDay)
}

/// When a message will be deleted by the server (unix seconds), if it was sent with history off.
pub fn message_expiry(message: &pb::Message) -> Option<i64> {
    let settings = message.retention_settings.as_ref().filter(|r| is_ephemeral(Some(r)))?;
    let at = settings.expiry_timestamp();
    if at > 0 {
        return Some(super::micros_to_secs(at));
    }
    let sent = super::micros_to_secs(message.create_time());
    (sent > 0).then_some(sent + EPHEMERAL_SECS)
}

/// A conversation's setting in the conversation list.
pub fn world_item_history_off(item: &pb::WorldItemLite) -> bool {
    is_ephemeral(item.read_state.as_ref().and_then(|r| r.retention_settings.as_ref()))
        || is_ephemeral(item.group_lite.as_ref().and_then(|g| g.retention_settings.as_ref()))
}

/// The new setting a "history turned on/off" system message announces, if this is one: `Some(true)` for off.
pub fn announced_change(message: &pb::Message) -> Option<bool> {
    message.annotations.iter().find_map(|a| match &a.metadata {
        Some(pb::annotation::Metadata::GroupRetentionSettingsUpdated(m)) => Some(is_ephemeral(m.retention_settings.as_ref())),
        _ => None,
    })
}

/// The words over a conversation with history off.
pub fn notice_text(group: &Group) -> String {
    let what = if group.is_dm() { "conversation" } else { "space" };
    format!("History is off in this {what}: new messages are deleted after 24 hours, here as well as on Google Chat.")
}

/// Records a conversation's setting and shows it on its buffer.
pub fn set_history_off(state: &AppState, handle: &Handle, group: &Group, off: bool) {
    {
        let mut world = handle.world();
        if let Some(c) = world.conversations.get_mut(group) {
            c.history_off = off;
        }
    }
    show(state, handle, group);
}

/// Puts the conversation's setting on its buffer (or takes it off), as it is in the world.
pub fn show(state: &AppState, handle: &Handle, group: &Group) {
    let off = handle.world().conversations.get(group).is_some_and(|c| c.history_off);
    let Some(buffer) = super::world::buffer_of(state, &handle.account_id, group) else { return };
    let current = buffer.notice.as_ref().map(|n| n.kind.as_str());
    if off {
        state.runtime.set_buffer_notice(state, &buffer.id, Some(BufferNotice { kind: NOTICE_KIND.into(), text: notice_text(group) }));
    } else if current == Some(NOTICE_KIND) {
        state.runtime.set_buffer_notice(state, &buffer.id, None);
    }
}

/// Notes when a stored message expires. Answers false if it has expired already, so the caller does not keep it.
pub fn note(state: &AppState, buffer_id: &str, msg_id: &str, message: &pb::Message) -> bool {
    let Some(at) = message_expiry(message) else { return true };
    if at <= now_secs() {
        state.runtime.delete_message(state, buffer_id, msg_id);
        return false;
    }
    if let Err(e) = state.store.note_expiry(buffer_id, msg_id, at) {
        tracing::warn!("googlechat: could not note when a message expires: {e}");
    }
    true
}

/// Takes out of scrollback every message of this account whose time is up.
pub fn sweep(state: &AppState, account_id: &str) -> usize {
    let due = match state.store.expired_messages(&format!("{account_id}|"), now_secs()) {
        Ok(due) => due,
        Err(e) => {
            tracing::warn!("googlechat[{account_id}]: could not read what has expired: {e}");
            return 0;
        }
    };
    for (buffer_id, msg_id) in &due {
        state.runtime.delete_message(state, buffer_id, msg_id);
        let _ = state.store.forget_expiry(buffer_id, msg_id);
    }
    if !due.is_empty() {
        tracing::info!("googlechat[{account_id}]: {} message(s) sent with history off expired", due.len());
    }
    due.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ephemeral(expiry: i64) -> Option<pb::RetentionSettings> {
        Some(pb::RetentionSettings { state: Some(RetentionState::EphemeralOneDay as i32), expiry_timestamp: Some(expiry) })
    }

    #[test]
    fn a_message_expires_when_the_server_says() {
        let m = pb::Message { create_time: Some(1_000_000_000_000), retention_settings: ephemeral(1_086_400_000_000), ..Default::default() };
        assert_eq!(message_expiry(&m), Some(1_086_400));
        // Without a time, a day after it was sent.
        let m = pb::Message { create_time: Some(1_000_000_000_000), retention_settings: ephemeral(0), ..Default::default() };
        assert_eq!(message_expiry(&m), Some(1_000_000 + EPHEMERAL_SECS));
        // Sent with history on: kept.
        let m = pb::Message {
            create_time: Some(1_000_000_000_000),
            retention_settings: Some(pb::RetentionSettings { state: Some(RetentionState::Permanent as i32), expiry_timestamp: None }),
            ..Default::default()
        };
        assert_eq!(message_expiry(&m), None);
        assert_eq!(message_expiry(&pb::Message::default()), None);
    }

    #[test]
    fn reads_the_announcement_as_it_came_over_the_wire() {
        // The system message the web app's toggle produced, annotation as pblite: type 19 and field 17.
        let annotation = serde_json::json!([19, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, [[2, "1795922125141311"], ["115783107897314926498"]], null, null, 3]);
        let a: pb::Annotation = super::super::pblite::decode(&annotation, "Annotation").unwrap();
        let m = pb::Message { annotations: vec![a], ..Default::default() };
        assert_eq!(announced_change(&m), Some(true));
        let on = pb::Message {
            annotations: vec![pb::Annotation {
                metadata: Some(pb::annotation::Metadata::GroupRetentionSettingsUpdated(pb::GroupRetentionSettingsUpdatedMetaData {
                    retention_settings: Some(pb::RetentionSettings { state: Some(RetentionState::Permanent as i32), expiry_timestamp: None }),
                    initiator: None,
                })),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(announced_change(&on), Some(false));
        assert_eq!(announced_change(&pb::Message::default()), None);
    }

    #[test]
    fn the_conversation_list_says_which_have_history_off() {
        let item = pb::WorldItemLite {
            read_state: Some(pb::GroupReadState { retention_settings: ephemeral(0), ..Default::default() }),
            ..Default::default()
        };
        assert!(world_item_history_off(&item));
        assert!(!world_item_history_off(&pb::WorldItemLite::default()));
    }
}
