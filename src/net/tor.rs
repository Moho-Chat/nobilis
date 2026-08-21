//! Transport selection for backends that need to run over Tor: embedded Tor
//! (via Arti, no external `tor` daemon required), an external SOCKS5 proxy
//! (a system Tor daemon or Tor Browser), or a direct connection.
//!
//! Ported from sockchat-rs's own `net/mod.rs`
//! (<https://gitgud.io/jcmoon/sockchat-rs>), which needed the exact same
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
/// to a minute" on a cold cache; every SockChat/Sneedchat account shares
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
}

impl TorManager {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            client: tokio::sync::RwLock::new(None),
            cache_dir: data_dir.join("tor-cache"),
            state_dir: data_dir.join("tor-state"),
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
