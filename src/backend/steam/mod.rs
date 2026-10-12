//! Steam - the friends list and direct messages of a Steam account, over the
//! same connection managers the official client uses.
//!
//! Steam is a directory of servers ("connection managers") that a client opens
//! a WebSocket to and speaks protobuf over. Everything a chat client does is
//! a message on that one connection: signing in, the friends list, who is
//! online, a message sent, a message received. There is no separate chat
//! server; the same socket that tells you a friend started a game delivers the
//! message they sent.
//!
//! # How this folder is arranged
//!
//! - `proto` - the messages, written out from Valve's own definitions
//! - `wire` - how a packet is laid out, and the batches that hold packets
//! - `steamid` - the two forms of an account's identity
//! - `eresult` - the result codes, said in words
//! - `auth` - signing in with a name and password, through Steam Guard, to a token
//! - `presence` - Steam's states and moho's
//! - `friends` - who the friends are, how they are doing, what each conversation is called
//! - `cm` - finding a connection manager to connect to
//! - `link` - one connection: asking it things, and hearing what it says
//!
//! Not yet wired into the daemon: the layers below are built and tested first.
#![allow(dead_code)]

pub mod auth;
pub mod cm;
pub mod eresult;
pub mod friends;
pub mod link;
pub mod presence;
pub mod proto;
pub mod steamid;
pub mod wire;
