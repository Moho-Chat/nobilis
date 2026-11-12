//! XMPP: the open, federated messaging protocol (RFC 6120/6121 and the extensions the common clients share - Conversations,
//! Dino, Gajim, Monal and the rest). An account is a JID on a server of its own choosing, with no operator behind it.
//!
//! The plan, the milestones and what has been checked is Gitea issue #23. In short: the xmpp-rs stack (`tokio-xmpp` and
//! `xmpp-parsers`, MPL-2.0) carries the stream, stream management and the typed stanzas; what it does not have is written
//! here on top of its XML types.
//!
//! # How this folder is arranged
//!
//! - `transport` - reaching the server: routing (Tor, SOCKS5), DNS SRV, TLS and who is trusted, STARTTLS and direct TLS
//! - `connect` - an account's connection: signing in, staying signed in, and stopping for the right reasons
//! - `iq` - what is asked of this client, and the answers RFC 6120 requires of it
//!
//! # What moho tells the server about itself
//!
//! The resource is `moho.<8 characters>`, made once per account and kept, so a restart is the same device. Nothing else
//! is said that the protocol does not require.

pub mod account;
pub mod adhoc;
pub mod archive_prefs;
pub mod avatars;
pub mod blocking;
pub mod bookmarks;
pub mod calls;
pub mod caps;
pub mod connect;
pub mod dataform;
pub mod dispatch;
pub mod incoming;
pub mod init;
pub mod invites;
pub mod login;
pub mod iq;
pub mod mam;
pub mod mds;
pub mod media;
pub mod messages;
pub mod muc;
pub mod omemo;
pub mod pep;
pub mod muc_admin;
pub mod ns;
pub mod roster;
pub mod sasl;
pub mod send;
pub mod tap;
pub mod register;
pub mod session;
pub mod transport;
pub mod upload;
pub mod websocket;
pub mod wire;
pub mod xml;

pub use connect::spawn;
pub use session::Handle;
