//! One folder per protocol, and nothing else in here.
//!
//! Each of these is a way of reaching a chat service, and each owns whatever
//! that service needs - its gateway, its file transfers, its authentication,
//! its own peculiar dialect of markup. They are siblings because that is what
//! they are to the daemon: `runtime.rs` knows what a message is, and each of
//! these knows how one particular service says it.
//!
//! Nothing that is not a protocol lives here. Talking to the sound devices is
//! not a protocol, which is why `crate::audio` is at the top of the crate
//! beside `crate::net` rather than sitting in this list looking like a service
//! nobody can name.
pub mod discord;
pub mod irc;
pub mod kick;
pub mod matrix;
pub mod sneedchat;
