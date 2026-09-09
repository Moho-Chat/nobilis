//! Discord backend: QR-code remote-auth login (`start_qr_login`) plus a
//! minimal real-time gateway client (`spawn`) once an account has a token.
//!
//! The QR flow is Discord's own official cross-device login mechanism (the
//! same one discord.com/app and the desktop client offer for "log in by
//! scanning with your phone") - reverse-engineered but stable, documented at
//! <https://docs.discord.food/remote-authentication/desktop>. Every value
//! encrypted anywhere in this flow (the nonce, the scanning user's identity,
//! and the final token) uses the same scheme: RSA-2048-OAEP-SHA256 against a
//! keypair generated fresh per login attempt, no symmetric layer involved.
//!
//! Auto-reconnects with backoff on any dropped/errored/zombied gateway
//! session (see `gateway::run_gateway_with_retry`) - unlike `backend::irc`,
//! which still has no auto-reconnect and deliberately reports "disconnected"
//! on any drop, Discord's gateway went silently zombie in practice (socket
//! technically alive, no dispatches ever arriving again, no error to
//! surface) often enough in a long-running session that a human had to
//! notice and manually reconnect every time - not viable long-term.
//!
//! # How this folder is arranged
//!
//! This file used to be all of it - seven thousand lines, everything Discord
//! does in one place. What it holds now is the shape:
//!
//! - `gateway` - the websocket everything else hangs off, and its dispatch
//! - `login` - the three doors to a token: QR, password, or one you have
//! - `http` - one client, its pacing, and how a rate limit is obeyed
//! - `messages` - turning what arrives into ours; `send` goes the other way
//! - `guilds` - servers, channels, roles, and what this account may do
//! - `people` - direct messages, friends, blocks, profiles
//! - `presence` - who is here, and what this account is shown as
//! - `history` - reading backwards through a conversation
//! - `media` - thumbnails, and re-signing links Discord expires
//! - `commands` - slash commands and the interactions that answer them
//! - `search` - Discord's own index, asked with its own filters
//! - `calls` - ringing and being rung; `voice` is the audio itself
//!
//! Two things about the arrangement are deliberate and easy to undo by
//! accident. The `pub use` lines below re-export each submodule flat, so the
//! daemon still says `backend::discord::send_message` and does not have to
//! know or care which file that ended up in - the split is this folder's
//! business, not `rpc`'s. And the imports under them are shared: every
//! submodule opens with `use super::*`, which is what puts this list in
//! scope for all of them. They look unused here because nothing in this file
//! uses them; they are used by every file beside it.
pub mod voice;
pub mod calls;
pub mod commands;
pub mod gateway;
pub mod guilds;
pub mod history;
pub mod http;
pub mod login;
pub mod media;
pub mod messages;
pub mod people;
pub mod presence;
pub mod search;
pub mod send;

pub use calls::*;
pub use commands::*;
pub use gateway::*;
pub use guilds::*;
pub use history::*;
pub use http::*;
pub use login::*;
pub use media::*;
pub use messages::*;
pub use people::*;
pub use presence::*;
pub use search::*;
pub use send::*;

use crate::accounts::DiscordAccountConfig;
use crate::model::{self, Attachment, Embed, Reaction, ReplyPreview};
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use futures::{FutureExt, SinkExt, StreamExt};
use rsa::pkcs8::EncodePublicKey;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
