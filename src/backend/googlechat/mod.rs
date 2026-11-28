//! Google Chat: the private web-client protocol ("Dynamite") that chat.google.com itself speaks, for consumer Gmail
//! and Google Workspace accounts alike.
//!
//! Plan and verification checklists: Gitea issue #30 (the user's comments 780 and 787). Account id: `googlechat:<gaia
//! id>`, Google's numeric id for the person.
//!
//! There is no OAuth sign-in a third-party client can use here: Google's public Chat API is Workspace-only and wants a
//! Cloud project of its own. So the account is the session a real browser sign-in made - Google's own pages, its own
//! 2-Step Verification and passkeys, in a window moho opens and throws away - kept as the cookies that session left. The
//! same cookies the web app sends are what the daemon sends.
//!
//! The wire, learned from mautrix-googlechat (`pkg/gchatmeow`, AGPL-3.0) and the reverse-engineered `googlechat.proto`
//! it carries (purple-googlechat's, "use it for whatever you want"):
//!
//! - `GET /u/0/mole/world` returns the web app's `WIZ_global_data`, whose `SMqcke` is the XSRF token every RPC needs and
//!   whose `qwAQke == "AccountsSignInUi"` says the cookies no longer sign anybody in ([`session`]).
//! - RPCs are binary protobuf POSTed to `/u/0/api/<name>` ([`api`]).
//! - Live events come down Google's BrowserChannel long poll at `/u/0/webchannel/events`, framed by UTF-16 length and
//!   carried as "pblite" - protobuf written as JSON arrays ([`channel`], [`chunk`], [`pblite`]).
//! - Text has no markup on the wire: a message is plain text plus annotation spans counted in UTF-16 units
//!   ([`format`]).

#![allow(dead_code)]

pub mod api;
pub mod channel;
pub mod chunk;
pub mod connect;
pub mod format;
pub mod media;
pub mod messages;
pub mod pblite;
pub mod people;
pub mod retention;
pub mod rpc;
pub mod send;
pub mod session;
pub mod space_settings;
pub mod world;

pub use connect::spawn;

use std::sync::{Arc, Mutex};

/// The types generated from `proto/googlechat.proto` by `proto/gen.sh`. Committed, so a build needs no protoc.
#[allow(clippy::all, clippy::pedantic, rustdoc::all, non_camel_case_types, unused_qualifications)]
pub mod pb {
    include!("proto/googlechat.rs");
}

/// The account's id here, from Google's id for the person.
pub fn account_id(gaia: &str) -> String {
    format!("googlechat:{gaia}")
}

/// What a connected account holds: the session that makes the requests, and what is known of its conversations.
#[derive(Clone)]
pub struct Handle {
    pub account_id: String,
    pub api: Arc<api::Api>,
    pub world: Arc<Mutex<world::World>>,
}

impl Handle {
    pub fn world(&self) -> std::sync::MutexGuard<'_, world::World> {
        self.world.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A conversation, as Google names it: a direct message (one person or a few) or a space.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Dm(String),
    Space(String),
}

impl Group {
    pub fn from_pb(id: Option<&pb::GroupId>) -> Option<Group> {
        match id?.id.as_ref()? {
            pb::group_id::Id::DmId(dm) => dm.dm_id.clone().filter(|s| !s.is_empty()).map(Group::Dm),
            pb::group_id::Id::SpaceId(space) => space.space_id.clone().filter(|s| !s.is_empty()).map(Group::Space),
        }
    }

    pub fn to_pb(&self) -> pb::GroupId {
        pb::GroupId {
            id: Some(match self {
                Group::Dm(id) => pb::group_id::Id::DmId(pb::DmId { dm_id: Some(id.clone()) }),
                Group::Space(id) => pb::group_id::Id::SpaceId(pb::SpaceId { space_id: Some(id.clone()) }),
            }),
        }
    }

    /// How the conversation is kept as a buffer's remote id: `dm:<id>` or `space:<id>`, the same as the bridge's.
    pub fn remote_id(&self) -> String {
        match self {
            Group::Dm(id) => format!("dm:{id}"),
            Group::Space(id) => format!("space:{id}"),
        }
    }

    pub fn from_remote_id(remote: &str) -> Option<Group> {
        if let Some(id) = remote.strip_prefix("dm:") {
            return Some(Group::Dm(id.to_string()));
        }
        remote.strip_prefix("space:").map(|id| Group::Space(id.to_string()))
    }

    pub fn is_dm(&self) -> bool {
        matches!(self, Group::Dm(_))
    }
}

/// A user's id as the protocol writes it.
pub fn user_id(gaia: &str) -> pb::UserId {
    pb::UserId { id: Some(gaia.to_string()), r#type: None }
}

pub fn member_id(gaia: &str) -> pb::MemberId {
    pb::MemberId { id: Some(pb::member_id::Id::UserId(user_id(gaia))) }
}

/// Google's microseconds as the unix seconds the store keeps.
pub fn micros_to_secs(micros: i64) -> i64 {
    micros / 1_000_000
}

pub fn now_micros() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_ids_round_trip() {
        let dm = Group::Dm("abc".into());
        assert_eq!(Group::from_remote_id(&dm.remote_id()), Some(dm.clone()));
        assert_eq!(Group::from_pb(Some(&dm.to_pb())), Some(dm));
        let space = Group::Space("AAAA".into());
        assert_eq!(Group::from_remote_id("space:AAAA"), Some(space.clone()));
        assert_eq!(Group::from_pb(Some(&space.to_pb())), Some(space));
        assert_eq!(Group::from_remote_id("nope"), None);
    }
}
