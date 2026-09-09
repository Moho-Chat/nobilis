mod accounts;
mod audio;
mod backend;
mod commands;
mod events;
mod highlights;
mod ignores;
mod ipc;
mod model;
mod net;
mod profile;
mod rpc;
mod runtime;
mod secure;
mod state;
mod store;
mod upload;

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

/// Where this daemon keeps accounts, keys and its lock.
///
/// `dirs::config_dir` rather than a hardcoded `~/.config`, which is an XDG
/// convention and not a universal one: on Windows it produced a dotfile
/// directory in the profile root, which works but is not where anything else
/// on that system looks. Confirmed against a real Windows 11 guest, which put
/// the lock in C:\Users\John\.config rather than in AppData.
///
/// An existing `~/.config/nobilis` still wins, so nobody's accounts move out
/// from under them. That matters on Linux beyond the obvious: the old path
/// ignored XDG_CONFIG_HOME, so a machine that sets it would otherwise find a
/// different directory than the one it has been using all along.
pub fn default_data_dir() -> PathBuf {
    let home = dirs::home_dir().expect("no home directory");
    let legacy = home.join(".config").join("nobilis");
    if legacy.is_dir() {
        return legacy;
    }
    dirs::config_dir().unwrap_or_else(|| home.join(".config")).join("nobilis")
}

fn parse_args() -> Options {
    let mut data_dir = default_data_dir();
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

/// Logging, at a level somebody can actually see.
///
/// `fmt::init()` on its own filtered almost everything out, so the daemon's
/// own account of what it was doing - which room it joined, why a connection
/// dropped, what a backend made of a frame it did not understand - went
/// nowhere, in a process whose output a frontend already pipes somewhere
/// useful. Two protocol bugs were reasoned about from first principles and got
/// wrong because the thing that would have answered them was switched off.
///
/// So: info by default, and RUST_LOG still wins where somebody sets it.
/// Parsed here rather than through tracing-subscriber's env-filter, which is
/// not one of its default features and is not worth a dependency change to
/// read one word.
fn init_logging() {
    let level = match std::env::var("RUST_LOG").unwrap_or_default().to_lowercase().as_str() {
        "" | "info" => tracing::Level::INFO,
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        // A per-target directive ("nobilis=debug"), which this does not parse.
        // The most useful reading of "somebody asked for more" is more.
        other => {
            if other.contains("trace") {
                tracing::Level::TRACE
            } else if other.contains("debug") {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            }
        }
    };
    tracing_subscriber::fmt().with_max_level(level).init();
}


/// Stops a long-running daemon from keeping every byte it has ever needed.
///
/// glibc gives each thread that allocates its own arena, up to eight per core,
/// and never gives an arena's free pages back to the system on its own. A
/// process with a tokio worker per core - thirty-three of them on the machine
/// this was measured on - therefore spreads its allocation over dozens of
/// arenas, and its resident size becomes the high-water mark of everything it
/// has ever done at once rather than what it is holding. Measured on a daemon
/// that had been up half a day: fourteen megabytes in the main heap and two
/// hundred and seventy-seven megabytes across a hundred and forty-seven
/// anonymous mappings, thirty-two of them full-size arenas.
///
/// This is the half that has to happen before there is a second thread to have
/// an arena - which is why main is not `#[tokio::main]` any more. The macro
/// builds the runtime, and therefore every worker thread, before a line of the
/// body runs; setting the cap in there set it after the arenas it was meant to
/// prevent had already been handed out. Measured that way round too: still
/// thirty-two of them.
///
/// Linux and glibc only. A musl build has neither symbol and needs neither -
/// its allocator returns memory as it goes.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn cap_malloc_arenas() {
    // SAFETY: mallopt takes two ints and touches nothing of ours. Fewer arenas
    // mean more threads sharing one, which a daemon that spends its life
    // waiting on sockets can afford - and glibc's per-thread cache still
    // absorbs the small allocations without reaching an arena at all.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 4);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn cap_malloc_arenas() {}

/// And the half that hands the free pages back, once the runtime exists.
///
/// Capping the arenas stops the spread; nothing in glibc returns what is
/// already free inside one. `malloc_trim` walks them releasing whole free
/// pages - a few milliseconds, and nothing else in the process notices.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_the_heap_periodically() {
    tokio::spawn(async {
        // Not on startup and not often: the point is what a long day leaves
        // behind, and trimming what a busy minute is about to reuse would be
        // work for nothing.
        let mut every = tokio::time::interval(std::time::Duration::from_secs(300));
        every.tick().await;
        loop {
            every.tick().await;
            // SAFETY: releases free pages held by the allocator; nothing that
            // is still allocated moves or is touched.
            unsafe {
                libc::malloc_trim(0);
            }
        }
    });
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_the_heap_periodically() {}

/// The runtime, built by hand so the allocator can be spoken to first.
fn main() -> Result<()> {
    cap_malloc_arenas();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?
        .block_on(run())
}

async fn run() -> Result<()> {
    init_logging();
    trim_the_heap_periodically();

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
        voice: Arc::new(backend::discord::voice::VoiceState::new()),
        voice_prefs: Arc::new(crate::audio::VoicePrefsStore::open(opts.data_dir.join("voice.toml"))),
        dcc_prefs: Arc::new(backend::irc::dcc::DccPrefsStore::open(opts.data_dir.join("dcc.toml"))),
        highlights: Arc::new(highlights::HighlightStore::open(opts.data_dir.join("highlights.toml"))),
        ignores: Arc::new(ignores::IgnoreStore::open(opts.data_dir.join("ignores.toml"))),
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
    for cfg in state.accounts.all_sneedchat() {
        backend::sneedchat::spawn(state.clone(), cfg);
    }
    for cfg in state.accounts.all_matrix() {
        backend::matrix::spawn(state.clone(), cfg);
    }
    for cfg in state.accounts.all_kick() {
        backend::kick::spawn(state.clone(), cfg);
    }

    // What was going on last time, before anything is served: a window that
    // connects immediately should see the same list it was looking at.
    backend::irc::dcc::restore_transfers(&state);

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
/// permanently-cached file - see backend/sneedchat/mod.rs's
/// cached_avatar_path). Neither is a one-time startup cost, so this
/// re-runs periodically rather than once.
async fn run_housekeeping(state: AppState) {
    const SCROLLBACK_KEEP_PER_BUFFER: i64 = 5000;

    // Ahead of the wait: this is a one-time local correction, and holding it
    // back a minute would only mean a minute of pictures that should move
    // sitting still.
    backend::discord::retire_still_thumbnails().await;

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

        // Kept to the same number the runtime holds, so the list does not
        // grow without bound across restarts while showing only the newest.
        match state.store.prune_transfers(crate::runtime::DCC_KEEP as i64) {
            Ok(0) | Err(_) => {}
            Ok(n) => tracing::info!("transfers: forgot {n} old record(s)"),
        }

        backend::sneedchat::sweep_avatar_cache().await;
        backend::sneedchat::sweep_attachment_cache().await;
        backend::matrix::sweep_media_cache().await;
        backend::discord::sweep_thumbnail_cache().await;
        backend::discord::sweep_guild_icon_cache().await;

        tokio::time::sleep(backend::sneedchat::AVATAR_CACHE_SWEEP_INTERVAL).await;
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
