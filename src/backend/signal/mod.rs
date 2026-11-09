//! Signal, as a secondary device linked to an account that lives on somebody's phone.
//!
//! The protocol itself - the ratchets, sealed sender, groups, the storage service - is Signal's own `libsignal`, reached
//! through Whisperfish's `libsignal-service-rs` and `presage` (all AGPL-3.0, see Cargo.toml for why that is accepted and
//! how the dependency graph was made to resolve). This module is what moho adds around them: where the account's keys
//! and conversations are kept (`store`), the sign-in by QR code (`link`), and, to come, the connection and the translation into
//! the daemon's unified model.

// The store is built before the connection that uses it (issue 35, Round 0/2); each piece is used as it lands.
#![allow(dead_code)]

pub mod link;
pub mod local;
pub mod store;
pub mod unlink;
