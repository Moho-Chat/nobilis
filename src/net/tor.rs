//! Transport selection for backends that need to run over Tor: embedded Tor
//! (via Arti, no external `tor` daemon required), an external SOCKS5 proxy
//! (a system Tor daemon or Tor Browser), or a direct connection.
//!
//! Ported from sneedchat-rs's own `net/mod.rs`
//! (<https://gitgud.io/jcmoon/sneedchat-rs>), which needed the exact same
//! thing for the exact same reason: `reqwest` and most other HTTP/websocket
//! clients only know how to dial their own connector or a SOCKS5 proxy
//! *URL* - they have no "here is a raw stream, use it" escape hatch, so
//! anything speaking to a `.onion` through an in-process Tor client has to
//! be built on top of this directly.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arti_client::{TorClient, TorClientConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tor_rtcompat::PreferredRuntime;

/// Any bidirectional byte stream a protocol can be run over, regardless of
/// which transport actually produced it.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub type BoxStream = Box<dyn Stream>;

#[derive(Clone)]
pub enum Transport {
    /// Tor embedded in-process via Arti. No `tor` daemon required.
    Tor(Arc<TorClient<PreferredRuntime>>),
    /// An external SOCKS5 proxy, e.g. a system Tor daemon (127.0.0.1:9050)
    /// or Tor Browser (127.0.0.1:9150). Hostnames are resolved by the proxy
    /// itself (`socks5h` semantics), required for `.onion` and to avoid
    /// leaking DNS queries.
    Socks { host: String, port: u16 },
    Direct,
}

/// Shared, lazily-bootstrapped embedded Tor client - one per daemon process,
/// not one per account. Bootstrapping fetches a directory consensus and
/// builds circuits, which the Tor Project's own docs put at "a few seconds
/// to a minute" on a cold cache; every SneedChat/Sneedchat account shares
/// this single instance rather than each paying that cost independently.
///
/// Uses an `RwLock<Option<...>>` rather than a `OnceCell` (the original
/// design) because the settings UI needs to force a fresh circuit or a full
/// cold restart on demand - a `OnceCell` can only ever be initialized once
/// and has no way to be reset, so it can't support that.
pub struct TorManager {
    client: tokio::sync::RwLock<Option<Result<Arc<TorClient<PreferredRuntime>>, String>>>,
    cache_dir: std::path::PathBuf,
    state_dir: std::path::PathBuf,
    /// How many connection attempts in a row have failed inside Tor, and when
    /// this last resorted to throwing the directories away. See `stumbled`.
    trouble: tokio::sync::Mutex<Trouble>,
}

#[derive(Default)]
struct Trouble {
    in_a_row: u32,
    last_wipe: Option<std::time::Instant>,
}

/// What a run of failures was met with, for the line a person reads.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Recovery {
    /// Not enough failures yet to call it anything but the network.
    Waited,
    /// The client was thrown away; the next attempt bootstraps a new one.
    NewClient,
    /// And so were the directories it remembers the network with.
    FromScratch,
}

/// Failures before the client is rebuilt, and before its directories go too.
///
/// Both are deliberately small. The failure this exists for is instant - a
/// second per attempt, not a timeout - so three of them is a few seconds of
/// evidence, and the escalation costs a bootstrap rather than anything a
/// person notices.
const STUMBLES_BEFORE_NEW_CLIENT: u32 = 3;
const STUMBLES_BEFORE_FROM_SCRATCH: u32 = 6;
/// How long to leave the directories alone after wiping them once.
///
/// Wiping means a cold bootstrap and, more to the point, a new set of guard
/// relays - the small fixed set of first hops that a Tor client deliberately
/// keeps in order to be harder to watch. Rotating those on a schedule the
/// network could provoke is not something to do every minute; if the site is
/// simply unreachable, waiting is the honest answer.
const WIPE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(900);

impl TorManager {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            client: tokio::sync::RwLock::new(None),
            cache_dir: data_dir.join("tor-cache"),
            state_dir: data_dir.join("tor-state"),
            trouble: tokio::sync::Mutex::new(Trouble::default()),
        }
    }

    /// Cache/state directories the embedded client persists circuit and
    /// consensus data to. Exposed so callers (e.g. the "restart from
    /// scratch" RPC) can wipe them without re-deriving the path themselves.
    pub fn cache_dirs(&self) -> (std::path::PathBuf, std::path::PathBuf) {
        (self.cache_dir.clone(), self.state_dir.clone())
    }

    /// Returns the shared embedded Tor client, bootstrapping it on first
    /// call (or on the first call after a `restart()`). `on_progress` is
    /// called (possibly zero times) with a human-readable status while a
    /// fresh bootstrap is in flight - a caller already past this point
    /// (client cached from an earlier call) never sees it, which is the
    /// common case after the first account connects.
    pub async fn get_or_bootstrap(&self, on_progress: impl FnOnce(&str)) -> Result<Arc<TorClient<PreferredRuntime>>> {
        // A cheap fast-path check first so `on_progress` is never called
        // once the client is already up.
        if let Some(result) = self.client.read().await.as_ref() {
            return result.clone().map_err(anyhow::Error::msg);
        }

        let mut guard = self.client.write().await;
        // Re-check: another task may have bootstrapped while we waited for
        // the write lock.
        if let Some(result) = guard.as_ref() {
            return result.clone().map_err(anyhow::Error::msg);
        }
        on_progress("Bootstrapping Tor circuit (this can take up to a minute the first time)...");
        let result = bootstrap(&self.cache_dir, &self.state_dir).await.map_err(|e| format!("{e:#}"));
        *guard = Some(result.clone());
        result.map_err(anyhow::Error::msg)
    }

    /// Drops the current embedded client (if any), forcing the next
    /// `get_or_bootstrap()` call to rebuild it from scratch - a fresh
    /// `TorClient` means fresh circuits. Existing backend connections that
    /// already cloned the old `Arc<TorClient>` keep running over it until
    /// they reconnect; callers of `restart()` are expected to re-spawn
    /// affected accounts afterward so they actually pick up the new client.
    pub async fn restart(&self) {
        *self.client.write().await = None;
    }

    /// A connection through Tor worked. Forgets the failures before it.
    pub async fn note_success(&self) {
        self.trouble.lock().await.in_a_row = 0;
    }

    /// A connection failed inside Tor, and this decides what to do about it.
    ///
    /// The reason this exists: a client whose hidden-service lookups have gone
    /// bad fails in about a second, and fails that way every time. A retry
    /// loop cannot tell that from a site being down, so it backs off to a
    /// minute and settles there - and stays there, because nothing in the loop
    /// can repair Tor. Observed on a real account: hours of a one-second
    /// failure repeating, with a working session sitting unused behind it, and
    /// the only way out a settings button nobody knew to press.
    ///
    /// So the loop is given a way out. First the client, which costs a
    /// bootstrap; then the directories, which costs a cold one. Both are what
    /// that settings button does, arrived at by the daemon noticing rather
    /// than by a person guessing - and measured on that same account, it was
    /// the second that fixed it.
    pub async fn stumbled(&self) -> Recovery {
        let mut trouble = self.trouble.lock().await;
        trouble.in_a_row += 1;
        let can_wipe = trouble.last_wipe.is_none_or(|at| at.elapsed() > WIPE_COOLDOWN);

        if trouble.in_a_row >= STUMBLES_BEFORE_FROM_SCRATCH && can_wipe {
            trouble.in_a_row = 0;
            trouble.last_wipe = Some(std::time::Instant::now());
            // Order matters: the client is holding these open, so it goes
            // first. A directory that will not delete is not fatal - the
            // fresh client is still worth having, and saying so beats
            // failing the recovery over a file.
            self.restart().await;
            for dir in [&self.cache_dir, &self.state_dir] {
                if dir.exists() {
                    if let Err(e) = std::fs::remove_dir_all(dir) {
                        tracing::warn!("could not clear {}: {e}", dir.display());
                    }
                }
            }
            tracing::warn!("Tor has failed {STUMBLES_BEFORE_FROM_SCRATCH} times running; starting it from scratch");
            return Recovery::FromScratch;
        }

        if trouble.in_a_row == STUMBLES_BEFORE_NEW_CLIENT {
            self.restart().await;
            tracing::warn!("Tor has failed {STUMBLES_BEFORE_NEW_CLIENT} times running; rebuilding the client");
            return Recovery::NewClient;
        }

        Recovery::Waited
    }
}

/// Whether a failure was Tor's own rather than the thing on the other end.
///
/// Asked of the whole chain rather than the message: everything the embedded
/// client reports arrives as an `arti_client::Error` somewhere under whatever
/// context the caller added, and a connection made through a SOCKS proxy or
/// straight out has nothing of the sort in it - which is exactly the
/// distinction, since throwing away Arti's directories helps only the first.
pub fn is_tor_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.downcast_ref::<arti_client::Error>().is_some())
}

async fn bootstrap(cache_dir: &std::path::Path, state_dir: &std::path::Path) -> Result<Arc<TorClient<PreferredRuntime>>> {
    let mut cfg = TorClientConfig::builder();
    cfg.storage()
        .cache_dir(arti_client::config::CfgPath::new_literal(cache_dir.to_path_buf()))
        .state_dir(arti_client::config::CfgPath::new_literal(state_dir.to_path_buf()));
    let cfg = cfg.build().context("building Tor client config")?;
    TorClient::create_bootstrapped(cfg).await.context("bootstrapping Tor")
}

impl Transport {
    /// Parse a `socks5://host:port` or `socks5h://host:port` proxy URL.
    pub fn socks_from_url(url: &str) -> Result<Self> {
        let parsed = url::Url::parse(url).with_context(|| format!("parsing proxy URL {url}"))?;
        match parsed.scheme() {
            "socks5" | "socks5h" => {}
            other => bail!("unsupported proxy scheme {other:?}; expected socks5 or socks5h"),
        }
        let host = parsed.host_str().context("proxy URL has no host")?.to_string();
        let port = parsed.port().unwrap_or(9050);
        Ok(Transport::Socks { host, port })
    }

    /// Open a stream to `host:port`, applying TLS if requested.
    pub async fn connect(&self, host: &str, port: u16, tls: bool) -> Result<BoxStream> {
        let stream: BoxStream = match self {
            Transport::Tor(client) => {
                let s = client.connect((host, port)).await.with_context(|| format!("Tor connect to {host}:{port}"))?;
                Box::new(s)
            }
            Transport::Socks { host: ph, port: pp } => {
                let s = tokio_socks::tcp::Socks5Stream::connect((ph.as_str(), *pp), (host, port))
                    .await
                    .with_context(|| format!("SOCKS5 connect to {host}:{port} via {ph}:{pp}"))?;
                Box::new(s)
            }
            Transport::Direct => {
                if host.ends_with(".onion") {
                    bail!("cannot reach a .onion address without Tor");
                }
                let s = TcpStream::connect((host, port)).await.with_context(|| format!("TCP connect to {host}:{port}"))?;
                Box::new(s)
            }
        };

        if !tls {
            return Ok(stream);
        }
        let connector = TlsConnector::from(tls_config()?);
        let name = ServerName::try_from(host.to_string()).with_context(|| format!("invalid TLS server name {host}"))?;
        let s = connector.connect(name, stream).await.with_context(|| format!("TLS handshake with {host}"))?;
        Ok(Box::new(s))
    }

    pub fn describe(&self) -> String {
        match self {
            Transport::Tor(_) => "embedded Tor".to_string(),
            Transport::Socks { host, port } => format!("SOCKS5 {host}:{port}"),
            Transport::Direct => "direct".to_string(),
        }
    }
}

/// Shared rustls config for outbound TLS through any transport above. Built
/// once; parsing the root store isn't free.
fn tls_config() -> Result<Arc<ClientConfig>> {
    use std::sync::OnceLock;
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    if let Some(c) = CFG.get() {
        return Ok(Arc::clone(c));
    }
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let cfg = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let cfg = Arc::new(cfg);
    let _ = CFG.set(Arc::clone(&cfg));
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase-1 go/no-go spike for the Sneedchat plan: does embedded Tor
    /// The escalation ladder, without a network in sight.
    ///
    /// What matters here is that it escalates at all and then stops: a
    /// recovery that fired on every failure would rotate the guard set
    /// whenever the site went down, and one that fired once and never again
    /// would leave the client stuck exactly the way this exists to prevent.
    #[tokio::test]
    async fn trouble_escalates_and_then_holds_off() {
        let dir = std::env::temp_dir().join(format!("nobilis-tor-ladder-{}", std::process::id()));
        let manager = TorManager::new(&dir);

        // A couple of failures is the network's business, not ours.
        assert_eq!(manager.stumbled().await, Recovery::Waited);
        assert_eq!(manager.stumbled().await, Recovery::Waited);
        // The third says the client itself is suspect.
        assert_eq!(manager.stumbled().await, Recovery::NewClient);
        for _ in 0..2 {
            assert_eq!(manager.stumbled().await, Recovery::Waited);
        }
        // And the sixth stops believing its directories.
        assert_eq!(manager.stumbled().await, Recovery::FromScratch);

        // Having just wiped, it will not wipe again on the next run of
        // failures - it climbs to the client and stays there.
        for _ in 0..2 {
            assert_eq!(manager.stumbled().await, Recovery::Waited);
        }
        assert_eq!(manager.stumbled().await, Recovery::NewClient);
        for _ in 0..2 {
            assert_eq!(manager.stumbled().await, Recovery::Waited);
        }
        assert_eq!(manager.stumbled().await, Recovery::Waited, "wiped twice inside the cooldown");

        // A connection that works clears the slate behind it.
        manager.note_success().await;
        assert_eq!(manager.stumbled().await, Recovery::Waited);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// actually bootstrap and reach the real hidden service from *this*
    /// environment? Bootstrap needs to reach Tor directory authorities and
    /// relays on arbitrary ports - a sandboxed network egress policy could
    /// block that independently of whether this code is correct, which is
    /// exactly what this is here to surface early. Not run by default
    /// (`cargo test` alone skips it); run explicitly with:
    ///   cargo test --release -- --ignored --nocapture tor_bootstrap
    #[tokio::test]
    #[ignore]
    async fn tor_bootstrap_and_connect_to_kiwifarms_onion() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);

        let client = manager
            .get_or_bootstrap(|msg| println!("progress: {msg}"))
            .await
            .expect("Tor bootstrap failed");
        println!("Tor bootstrapped, {} client refs", Arc::strong_count(&client));

        let transport = Transport::Tor(client);
        let host = "kiwifarmsaaf4t2h7gc3dfc5ojhmqruw2nit3uejrpiagrxeuxiyxcyd.onion";
        let stream = transport.connect(host, 443, true).await;
        match &stream {
            Ok(_) => println!("connected + TLS handshake OK to {host}:443"),
            Err(e) => println!("connect failed: {e:#}"),
        }
        stream.expect("connect+TLS to the onion service failed");
    }
}
