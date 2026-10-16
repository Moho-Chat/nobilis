//! Muting a conversation on the account itself.
//!
//! Telegram keeps "mute until" per chat with the account, so a chat silenced on the phone is silent here and the
//! other way round. It is read off each dialog and from the update Telegram sends when it changes, and written when
//! the window asks.

use super::peers;
use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use grammers_session::types::PeerId;

/// "Forever", as Telegram's own apps write it.
const FOREVER: i32 = i32::MAX;

/// Whether notification settings say the chat is silent at `now`.
pub fn silent(settings: &tl::enums::PeerNotifySettings, now: i64) -> bool {
    let tl::enums::PeerNotifySettings::Settings(s) = settings;
    s.mute_until.is_some_and(|until| i64::from(until) > now)
}

/// A chat's settings changed (on another of the account's apps, or here): the buffer follows.
pub fn changed(state: &AppState, account_id: &str, update: &tl::types::UpdateNotifySettings) {
    let tl::enums::NotifyPeer::Peer(p) = &update.peer else { return };
    let remote = peers::remote_id(PeerId::from(p.peer.clone()));
    let Some(buffer) = state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote.as_str())) else { return };
    state.runtime.set_silenced(state, &buffer.id, silent(&update.notify_settings, chrono::Utc::now().timestamp()));
}

/// Mutes a chat, or takes the mute off.
pub async fn set_muted(state: &AppState, buffer: &Buffer, muted: bool) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let settings = tl::types::InputPeerNotifySettings {
        show_previews: None,
        silent: None,
        mute_until: Some(if muted { FOREVER } else { 0 }),
        sound: None,
        stories_muted: None,
        stories_hide_sender: None,
        stories_sound: None,
    };
    let peer = tl::enums::InputNotifyPeer::Peer(tl::types::InputNotifyPeer { peer: peer.into() });
    handle.client.invoke(&tl::functions::account::UpdateNotifySettings { peer, settings: settings.into() }).await.map_err(|e| explain(&e))?;
    state.runtime.set_silenced(state, &buffer.id, muted);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mute_until: Option<i32>) -> tl::enums::PeerNotifySettings {
        tl::enums::PeerNotifySettings::Settings(tl::types::PeerNotifySettings {
            show_previews: None,
            silent: None,
            mute_until,
            ios_sound: None,
            android_sound: None,
            other_sound: None,
            stories_muted: None,
            stories_hide_sender: None,
            stories_ios_sound: None,
            stories_android_sound: None,
            stories_other_sound: None,
        })
    }

    #[test]
    fn a_chat_is_silent_until_its_mute_runs_out() {
        assert!(silent(&settings(Some(FOREVER)), 1_790_000_000));
        assert!(silent(&settings(Some(1_790_000_100)), 1_790_000_000));
        assert!(!silent(&settings(Some(1_789_999_000)), 1_790_000_000));
        assert!(!silent(&settings(Some(0)), 1_790_000_000));
        assert!(!silent(&settings(None), 1_790_000_000));
    }
}
