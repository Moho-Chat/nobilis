use crate::accounts::AccountStore;
use crate::events::EventBus;
use crate::net::tor::TorManager;
use crate::runtime::Runtime;
use crate::store::Store;
use std::sync::Arc;

/// Shared daemon state, handed to every RPC connection task. Cheap to
/// clone (Arc-wrapped internals) - see daemon/nobilis/api.c's module-level
/// `clients`/`service` globals for the C equivalent of "state every
/// connection handler needs."
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub accounts: Arc<AccountStore>,
    pub events: EventBus,
    pub runtime: Arc<Runtime>,
    /// Shared across every Sneedchat account (and any future Tor-needing
    /// backend) - bootstrapping a circuit is expensive enough that it must
    /// happen once per daemon process, not once per account.
    pub tor: Arc<TorManager>,
}
