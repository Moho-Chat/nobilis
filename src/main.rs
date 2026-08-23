mod accounts;
mod backend;
mod events;
mod model;
mod net;
mod nickserv;
mod rpc;
mod runtime;
mod state;
mod store;

use accounts::AccountStore;
use anyhow::{bail, Context, Result};
use events::EventBus;
use fs2::FileExt;
use runtime::Runtime;
use state::AppState;
use std::path::PathBuf;
use std::sync::Arc;
use store::Store;

struct Options {
    data_dir: PathBuf,
    socket_path: Option<PathBuf>,
}

fn parse_args() -> Options {
    let mut data_dir = dirs::home_dir()
        .expect("no home directory")
        .join(".config")
        .join("nobilis");
    let mut socket_path = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                if let Some(v) = args.next() {
                    data_dir = PathBuf::from(v);
                }
            }
            "--socket-path" => {
                if let Some(v) = args.next() {
                    socket_path = Some(PathBuf::from(v));
                }
            }
            other => eprintln!("nobilis: ignoring unrecognized argument {other:?}"),
        }
    }

    Options { data_dir, socket_path }
}

/// One-time move of this daemon's state from the directories it used while it
/// was still called `chatd` and lived inside the moho repository. Everything
/// here is genuinely irreplaceable - saved accounts and their credentials,
/// persisted scrollback, and above all the Matrix crypto store, whose device
/// keys cannot be regenerated: losing it means re-verifying every session and
/// permanently losing access to any encrypted history not backed up to the
/// homeserver.
///
/// Runs before the singleton lock is taken, so nothing is holding files open
/// in either directory yet. Deliberately conservative: it only ever moves a
/// directory into a destination that does not exist, so a second run, a
/// partially-completed move, or a fresh install with no old data all reduce to
/// no-ops rather than clobbering anything.
fn migrate_from_moho_dirs(data_dir: &std::path::Path) {
    let Some(home) = dirs::home_dir() else { return };
    let cache_root = dirs::cache_dir().unwrap_or_else(|| home.join(".cache"));

    let migrations = [
        ("config", home.join(".config").join("moho"), data_dir.to_path_buf()),
        ("cache", cache_root.join("moho"), cache_root.join("nobilis")),
    ];

    for (label, old, new) in migrations {
        if !old.is_dir() || new.exists() {
            continue;
        }
        if let Some(parent) = new.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("could not create {} while migrating {label}: {e}", parent.display());
                continue;
            }
        }
        // A plain rename: both paths are under the same root in every normal
        // install, and unlike a copy it cannot leave two diverging copies of
        // an account store behind if it fails halfway.
        match std::fs::rename(&old, &new) {
            Ok(()) => tracing::info!("migrated {label} from {} to {}", old.display(), new.display()),
            Err(e) => tracing::warn!(
                "could not migrate {label} from {} to {}: {e} - starting with an empty {}",
                old.display(),
                new.display(),
                new.display()
            ),
        }
    }
}

/// flock()-based singleton lock, same purpose as
/// daemon/nobilis/nobilis.c's acquire_singleton_lock(): prevent two nobilis
/// processes racing the same socket/account-store. The lock file's fd is
/// deliberately leaked (`std::mem::forget`) for the process lifetime - it
/// releases automatically on exit/crash.
fn acquire_singleton_lock(data_dir: &std::path::Path) -> Result<()> {
    let lock_path = data_dir.join("nobilis.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    if file.try_lock_exclusive().is_err() {
        bail!("another nobilis instance is already running (lock held on {})", lock_path.display());
    }
    std::mem::forget(file); // keep the lock for the process lifetime
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Both the `irc` crate's tls-rust feature and the Discord backend's
    // websocket/HTTP clients pull in rustls, but via different transitive
    // paths that don't agree on a default crypto backend (ring vs
    // aws-lc-rs) - left unresolved, rustls refuses to guess and panics on
    // the first TLS handshake of the process. Pin one explicitly, once.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let opts = parse_args();
    migrate_from_moho_dirs(&opts.data_dir);
    std::fs::create_dir_all(&opts.data_dir)
        .with_context(|| format!("creating {}", opts.data_dir.display()))?;

    if let Err(e) = acquire_singleton_lock(&opts.data_dir) {
        tracing::info!("{e}, exiting");
        return Ok(());
    }

    let store = Store::open(&opts.data_dir.join("scrollback.db"))
        .context("opening scrollback store")?;
    let accounts = AccountStore::open(opts.data_dir.join("accounts.toml"))
        .context("opening account store")?;

    let state = AppState {
        store: Arc::new(store),
        accounts: Arc::new(accounts),
        events: EventBus::new(),
        runtime: Arc::new(Runtime::new()),
        tor: Arc::new(net::tor::TorManager::new(&opts.data_dir)),
        shutdown: Arc::new(tokio::sync::Notify::new()),
        voice: Arc::new(backend::discord_voice::VoiceState::new()),
    };

    // Reconnect every saved account, same as
    // daemon/nobilis/actions.c's nobilis_reconnect_saved_accounts() - without
    // this, an account created in a previous run just sits there after a
    // restart.
    for cfg in state.accounts.all_irc() {
        backend::irc::spawn(state.clone(), cfg);
    }
    for cfg in state.accounts.all_discord() {
        backend::discord::spawn(state.clone(), cfg);
    }
    for cfg in state.accounts.all_sockchat() {
        backend::sockchat::spawn(state.clone(), cfg);
    }
    for cfg in state.accounts.all_matrix() {
        backend::matrix::spawn(state.clone(), cfg);
    }

    tokio::spawn(run_housekeeping(state.clone()));

    let socket_path = opts.socket_path.unwrap_or_else(rpc::default_socket_path);
    let rpc_state = state.clone();

    let shutdown_rpc = state.shutdown.clone();
    tokio::select! {
        result = rpc::start(rpc_state, socket_path) => result,
        // Either route - a signal, or a client asking over the socket -
        // takes the same clean exit below.
        _ = shutdown_signal() => {
            // A bare process kill just drops every TCP connection without
            // telling the server - confirmed live against Libera.Chat, an
            // account killed this way can leave a "ghost" session holding
            // the nick hostage until the network's own ping-timeout
            // notices the dead peer, causing the *next* connect attempt to
            // fail with "Nickname is already in use". Send real QUITs and
            // give them a moment to reach the network before exiting.
            tracing::info!("shutting down, sending QUIT to all connected accounts");
            state.runtime.quit_all("Leaving");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            Ok(())
        }
        _ = shutdown_rpc.notified() => {
            tracing::info!("shutdown requested over the socket, sending QUIT to all connected accounts");
            state.runtime.quit_all("Leaving");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            Ok(())
        }
    }
}

/// Keeps two genuinely unbounded-over-time growth vectors in check for as
/// long as this daemon process stays up: scrollback (a busy buffer left
/// open for months of uptime never stops growing on its own) and the
/// Sneedchat avatar cache (every distinct poster ever seen gets a
/// permanently-cached file - see backend/sockchat/mod.rs's
/// cached_avatar_path). Neither is a one-time startup cost, so this
/// re-runs periodically rather than once.
async fn run_housekeeping(state: AppState) {
    const SCROLLBACK_KEEP_PER_BUFFER: i64 = 5000;

    // Let the initial reconnect burst above settle before the first pass.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    loop {
        match state.store.prune_old_messages(SCROLLBACK_KEEP_PER_BUFFER) {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!("scrollback: pruned {n} row(s) beyond {SCROLLBACK_KEEP_PER_BUFFER} kept per buffer");
                if let Err(e) = state.store.incremental_vacuum() {
                    tracing::debug!("scrollback: incremental_vacuum failed: {e}");
                }
            }
            Err(e) => tracing::warn!("scrollback: pruning failed: {e}"),
        }

        backend::sockchat::sweep_avatar_cache().await;
        backend::sockchat::sweep_attachment_cache().await;
        backend::matrix::sweep_media_cache().await;
        backend::discord::sweep_thumbnail_cache().await;
        backend::discord::sweep_guild_icon_cache().await;

        tokio::time::sleep(backend::sockchat::AVATAR_CACHE_SWEEP_INTERVAL).await;
    }
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok(); };
    #[cfg(unix)]
    let terminate = async {
        let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        sig.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
