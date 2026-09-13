//! Matrix backend: full end-to-end encryption via `matrix-sdk-crypto` (the
//! standalone, vodozemac-backed Olm/Megolm state machine `matrix-sdk`
//! itself uses internally - not the full `matrix-sdk` crate, which would
//! fight this project's hand-rolled-per-backend convention with its own
//! opinionated HTTP/sync/state-store stack).
//!
//! Unlike Sneedchat (one websocket per room, since that protocol only lets
//! a connection join a single room) or Discord (one persistent gateway
//! connection), Matrix's `/sync` endpoint delivers events for every joined
//! room over a single long-polled HTTP connection - so this backend needs
//! exactly one connection per account.
//!
//! # How this folder is arranged
//!
//! The sync loop and what it feeds:
//!
//! - `sync` - the long poll, its retry, and the walk over each response
//! - `timeline` - one event, turned into whatever it means
//! - `receipts` - who has read how far, in both directions
//! - `notifications` - push rules, and the server-side ignore list
//!
//! What this client asks for and says:
//!
//! - `send` - messages, edits, reactions, redactions, stickers, pins
//! - `history` - reading backwards, and sideways into a thread
//! - `directory` - searching a room, and finding rooms across servers
//! - `media` - fetching, caching and uploading, encrypted or not
//! - `profile` - who somebody is, and what this account shows
//!
//! And the parts that were already their own files:
//!
//! - `auth` - login, SSO, and the device_id that keeps an Olm identity alive
//! - `http` - the thin Client-Server API client everything goes through
//! - `crypto` - the OlmMachine wrapper; `verification` and `backup` beside it
//! - `protocol` - event-type constants and small extraction helpers
//! - `rooms` - room id to buffer name and kind; `roomstate` for avatars
//!   and power levels; `moderation` for what may be done with them
//! - `calls`, `polls`, `stickers`, `markup` - one feature each
//! - `probes` - live checks against a real homeserver, run by hand
//!
//! As in the other backends, the submodules are re-exported flat so callers
//! say `backend::matrix::send_message` without knowing which file that is,
//! and each opens with `use super::*` to share the imports below.

pub mod auth;
pub mod directory;
pub mod directs;
pub mod history;
pub mod media;
pub mod notifications;
pub mod probes;
pub mod profile;
pub mod receipts;
pub mod relock;
pub mod send;
pub mod server;
pub mod sync;
pub mod timeline;

pub use directory::*;
pub use history::*;
pub use media::*;
pub use notifications::*;
pub use profile::*;
pub use receipts::*;
// Not `pub use`: this is asked and answered inside the sync loop, and its
// names would say nothing useful in the backend's namespace.
use relock::*;
pub use send::*;
pub use server::*;
pub use sync::*;
// Not `pub use`: an event's handling is this folder's business alone, but
// the files beside it want the helpers.
use timeline::*;
pub use auth::*;
pub mod backup;
pub mod crypto;
pub mod http;
pub mod markup;
pub mod moderation;
pub mod permalinks;
pub mod polls;
pub mod calls;
pub mod protocol;
pub mod roomsettings;
pub mod roomstate;
pub mod widgets;
pub mod sliding;
pub mod dehydration;
pub mod ssss;
pub mod stickers;
pub mod tags;
pub mod rooms;
pub mod verification;

use crate::accounts::MatrixAccountConfig;
use crate::model;
use crate::model::Attachment;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{Context, Result};
use futures::FutureExt;
use serde_json::Value;
use std::time::Duration;
