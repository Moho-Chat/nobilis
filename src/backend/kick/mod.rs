//! Kick - livestream chat, read over the same Pusher websocket kick.com's own
//! player page uses.
//!
//! Two things make this backend shaped differently from the others here.
//!
//! **A channel is a streamer, not a room you are a member of.** There is no
//! join to perform and no membership to hold: naming a handle is the whole of
//! it, and the chat is public to anybody with a browser. So a Kick account
//! with no credential at all is useful - it reads every channel it is pointed
//! at - and signing in buys exactly two things, sending and knowing what you
//! are subscribed to. That is why the token is optional throughout rather than
//! a precondition for connecting.
//!
//! **One websocket carries every channel.** Pusher multiplexes: each channel
//! is a `chatrooms.<id>.v2` subscription on the one connection, so watching
//! twenty streamers costs one socket rather than twenty. Sneedchat next door
//! opens one per room because its protocol has no such verb; this one does,
//! and joining a channel while connected is a subscribe frame rather than a
//! reconnect.

//!
//! # How this folder is arranged
//!
//! - `socket` - the Pusher connection, its subscriptions, the frame envelope
//! - `chat` - what is said in a channel, and who said it
//! - `cards` - polls and predictions, which are the same shape twice
//! - `live` - whether a channel is streaming, and who just started
//! - `api` - the HTTP half; `emotes` for what a channel's are
//!
//! As in the other backends, the submodules are re-exported flat so callers
//! say `backend::kick::backfill` without knowing which file that is, and each
//! opens with `use super::*` to share the imports below.
pub mod cards;
pub mod chat;
pub mod live;
pub mod socket;
#[cfg(test)]
mod testkit;
pub mod api;

pub use cards::*;
pub use chat::*;
pub use live::*;
pub use socket::*;
pub mod emotes;

use crate::accounts::KickAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{Context, Result};
use futures::{FutureExt, SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

impl Watched {
    /// Records that somebody spoke, and answers whether the list changed
    /// enough to be worth re-announcing.
    ///
    /// Somebody already in the list is not a change: a busy channel would
    /// otherwise emit a roster every few hundred milliseconds, all of them
    /// nearly identical. Their badges changing is - somebody subscribes, or
    /// is given moderator, and the list should follow.
    fn heard(&mut self, slug: &str, speaker: Speaker) -> bool {
        let list = self.speakers.entry(slug.to_string()).or_default();
        if let Some(existing) = list.iter_mut().find(|s| s.nick == speaker.nick) {
            let changed = existing.badge != speaker.badge;
            existing.badge = speaker.badge;
            return changed;
        }
        list.insert(0, speaker);
        list.truncate(SPEAKERS_REMEMBERED);
        true
    }

    fn roster(&self, slug: &str) -> Vec<serde_json::Value> {
        self.speakers
            .get(slug)
            .map(|list| {
                list.iter()
                    .map(|s| {
                        serde_json::json!({
                            "nick": s.nick,
                            "userId": s.user_id.clone().unwrap_or_default(),
                            "prefix": s.badge.clone().unwrap_or_default(),
                            "away": false,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn add(&mut self, channel: &api::Channel) {
        self.by_chatroom.insert(channel.chatroom_id, channel.slug.clone());
        self.by_channel.insert(channel.id, channel.slug.clone());
    }

    /// Forgets a channel, and names every subscription it was using so the
    /// caller can cancel each one. All four, or the socket keeps delivering
    /// events for a conversation that has been closed.
    fn remove(&mut self, slug: &str) -> Vec<String> {
        self.speakers.remove(slug);
        let mut names = Vec::new();
        if let Some(room) = self.by_chatroom.iter().find(|(_, s)| *s == slug).map(|(i, _)| *i) {
            self.by_chatroom.remove(&room);
            names.push(format!("chatrooms.{room}.v2"));
            names.push(format!("chatrooms.{room}"));
            names.push(format!("chatroom_{room}"));
        }
        if let Some(channel) = self.by_channel.iter().find(|(_, s)| *s == slug).map(|(i, _)| *i) {
            self.by_channel.remove(&channel);
            names.push(format!("channel.{channel}"));
        }
        names
    }

    fn chatroom(&self, id: u64) -> Option<&String> {
        self.by_chatroom.get(&id)
    }
}
