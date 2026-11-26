//! "Sneedchat" - SneedChat, the XenForo-based chat plugin used by Kiwi Farms
//! (`kiwifarms.st` / its `.onion`). Reachable only through Tor in practice;
//! see `net::tor` for the embedded-Tor/SOCKS5-proxy transport this backend
//! runs on. Ported from sneedchat-rs (<https://gitgud.io/jcmoon/sneedchat-rs>).
//!
//! A single websocket can only ever be joined to one room at a time - the
//! server has no "subscribe to several" verb, only `/join <room_id>`, which
//! *switches* the one active room. To show several rooms at once, this
//! backend instead opens one persistent websocket per configured room, all
//! sharing a single login/session (see `run` and `run_room`): logging in
//! once, then fanning out, rather than repeating the whole login (and its
//! proof-of-work solve) once per room. Each room's own websocket reconnects
//! independently with exponential backoff, mirroring
//! backend::discord::run_gateway_with_retry's shape.

//!
//! # How this folder is arranged
//!
//! - `connect` - signing in, and keeping the account up
//! - `rooms` - the catalogue, the naming, and the roster
//! - `chat` - one websocket per room, and the frames it carries
//! - `send` - messages, whispers, edits, attachments
//! - `media` - avatars and attachments, fetched through Tor and cached
//! - `hosting` - the chat's own file hosting, where attachments go by default
//! - `postimg` - an upload host for pictures, when chosen instead
//! - `auth`, `pow` - getting past the door and staying in
//! - `form`, `http`, `protocol`, `smilies` - the site's own shapes
//! - `probes` - live checks against the real site, run by hand
//!
//! As in the other backends, the submodules are re-exported flat so callers
//! say `backend::sneedchat::send_message` without knowing which file that is,
//! and each opens with `use super::*` to share the imports below.
pub mod catalogue;
pub mod chat;
pub mod connect;
pub mod media;
pub mod hosting;
pub mod postimg;
pub mod probes;
pub mod rooms;
pub mod live;
pub mod send;
pub mod auth;

pub use connect::*;
pub use media::*;
pub use postimg::*;
pub use rooms::*;
pub use live::{leave_room, set_rooms};
pub use send::*;
pub mod form;
pub mod health;
pub mod http;
pub mod pow;
pub mod protocol;
pub mod smilies;
pub mod stream;

use crate::accounts::{SneedChatAccountConfig, SneedChatRoom};
use crate::net::tor::Transport;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use auth::Session;
use bytes::Bytes;
use futures::FutureExt;
use std::time::Duration;
