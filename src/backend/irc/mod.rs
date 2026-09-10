//! IRC: the oldest protocol here, and the one that assumes the least.
//!
//! No accounts, no history, no delivery receipts - a connection, a nickname
//! that is only yours while you hold it, and lines of text. Everything above
//! that is an extension some networks have and others do not, which is why so
//! much of this backend is about finding out what the server in front of it
//! can actually do.
//!
//! # How this folder is arranged
//!
//! - `connect` - getting registered and staying on: sockets, caps, retries
//! - `incoming` - the dispatch over what the server says
//! - `send` - what this client says, and how it learns the server heard
//! - `presence` - who is here, via MONITOR, ISON, or a netsplit
//! - `who` - asking the server who is in a room, and their hostmasks
//! - `typing` - the one notification IRC learned late
//! - `history` - CHATHISTORY, where a network has it
//!
//! And three that are IRC's own and nothing else's: `dcc`, its direct file
//! transfer; `sasl`, how a client proves who it is during registration; and
//! `nickserv`, the bot every network has for owning a nickname.
//!
//! As in `backend::discord`, the submodules are re-exported flat so callers
//! say `backend::irc::send_message` without knowing which file that is, and
//! each file opens with `use super::*` to share the import list below.
pub mod connect;
pub mod history;
pub mod incoming;
pub mod presence;
pub mod who;
pub mod send;
pub mod typing;
pub mod dcc;
pub mod nickserv;
pub mod sasl;

pub use connect::*;
pub use history::*;
// Not `pub use`: the dispatch is nobody else's business, but its helpers
// are wanted by the files beside it.
use incoming::*;
pub use presence::*;
// Not `pub use`: `who` is asked and answered inside this folder, and its
// names (`ask`, `read_who`) are too plain to sit in the backend's namespace.
use who::*;
pub use send::*;
pub use typing::*;
pub use sasl::*;

use crate::accounts::IrcAccountConfig;
use crate::model::MemberRank;
use nickserv::NickservWait;
use crate::runtime::{ConnState, IrcHandle};
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use futures::prelude::*;
use irc::client::prelude::*;
use irc::client::ClientStream;
use irc::proto::CapSubCommand;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
