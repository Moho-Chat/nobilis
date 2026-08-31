use crate::accounts::AccountStore;
use crate::events::EventBus;
use crate::net::tor::TorManager;
use crate::runtime::Runtime;
use crate::store::Store;
use std::sync::Arc;
use tokio::sync::Notify;

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
    /// Fired by the `shutdown` RPC. A client that adopted an already-running
    /// daemon has no child process to signal, so asking over the socket is
    /// the only way it can stop one it did not spawn - see main.rs, which
    /// waits on this alongside SIGTERM and runs the identical clean exit.
    pub shutdown: Arc<Notify>,
    /// Live Discord voice connections and half-built handshakes.
    pub voice: Arc<crate::backend::discord_voice::VoiceState>,
    /// Which sound devices voice uses, and whether it is silenced. Persisted,
    /// so a deliberate mute is still in force after a restart.
    pub voice_prefs: Arc<crate::backend::audio::VoicePrefsStore>,
    pub dcc_prefs: Arc<crate::backend::irc_dcc::DccPrefsStore>,
}
