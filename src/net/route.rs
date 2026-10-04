//! Which connections go through Tor or a SOCKS5 proxy, and how.
//!
//! Two kinds of choice meet here. Each account says whether *it* is routed -
//! the switch on its card - and the daemon-wide settings say what routed
//! means (moho's own Tor, or a SOCKS5 proxy somebody runs themselves) and
//! whether *everything* is routed, accounts or not: link sniffing, previews,
//! media.
//!
//! Every routed connection goes to one SOCKS5 address. For a proxy of
//! somebody's own that is simply its address. For moho's own Tor it is a
//! small relay on the loopback interface (`relay`), so that the services
//! whose libraries only know how to dial a SOCKS5 proxy - the IRC client,
//! reqwest, and Chromium itself - can use embedded Tor exactly as they would
//! an external one. Nothing is started until something asks to be routed.
//!
//! Routing fails closed. A connection that should be routed and cannot be -
//! Tor not up yet, the proxy refusing - fails, rather than quietly going out
//! directly, which would be the one outcome somebody switching this on is
//! trying to prevent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::tor::{BoxStream, TorManager};

/// Where a routed client is pointed while there is nowhere real to point it:
/// the discard port on loopback, which refuses. A request made in that
/// window fails at once instead of leaving directly.
const NOWHERE: &str = "socks5h://127.0.0.1:9";

/// The daemon-wide half of the choice. Persisted in `net.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetSettings {
    /// "embedded" for moho's own Tor, "proxy" for a SOCKS5 proxy of one's own.
    #[serde(default = "embedded")]
    pub tor_mode: String,
    /// The proxy, as `socks5h://host:port`, when `tor_mode` is "proxy".
    #[serde(default)]
    pub proxy: Option<String>,
    /// Route everything, not only the accounts that ask: link sniffing,
    /// previews, media, the window's own requests.
    #[serde(default)]
    pub tunnel_all: bool,
}

fn embedded() -> String {
    "embedded".to_string()
}

impl Default for NetSettings {
    fn default() -> Self {
        Self { tor_mode: embedded(), proxy: None, tunnel_all: false }
    }
}

pub struct Router {
    path: PathBuf,
    tor: Arc<TorManager>,
    settings: Mutex<NetSettings>,
    /// Which keys - an account id, or a credential an account sends - want
    /// to be routed.
    wants: Mutex<HashMap<String, bool>>,
    /// The SOCKS5 address routed traffic goes to, once there is one.
    socks: Mutex<Option<(String, u16)>>,
    relay: tokio::sync::Mutex<Option<(u16, tokio::task::JoinHandle<()>)>>,
    clients: Mutex<HashMap<(&'static str, String), reqwest::Client>>,
}

static ROUTER: OnceLock<Arc<Router>> = OnceLock::new();

/// The daemon's router. Set once at startup.
pub fn router() -> &'static Arc<Router> {
    #[cfg(test)]
    {
        // Tests run no startup: they get a router that routes nothing.
        ROUTER.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("nobilis-test-router-{}", std::process::id()));
            Arc::new(Router::open(dir.join("net.toml"), Arc::new(TorManager::new(&dir)), None))
        })
    }
    #[cfg(not(test))]
    {
        ROUTER.get().expect("the router is installed at startup")
    }
}

pub fn install(router: Arc<Router>) {
    let _ = ROUTER.set(router);
}

impl Router {
    /// Opens `net.toml`, or starts from `fallback` - the Tor settings that
    /// used to live on Sneedchat accounts - when there is none yet.
    pub fn open(path: PathBuf, tor: Arc<TorManager>, fallback: Option<NetSettings>) -> Self {
        let settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .or(fallback)
            .unwrap_or_default();
        Self {
            path,
            tor,
            settings: Mutex::new(settings),
            wants: Mutex::new(HashMap::new()),
            socks: Mutex::new(None),
            relay: tokio::sync::Mutex::new(None),
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn settings(&self) -> NetSettings {
        self.settings.lock().unwrap().clone()
    }

    /// Changes the settings. Anything already pointed somewhere is forgotten,
    /// so the next routed connection is set up again the new way.
    pub async fn set_settings(&self, next: NetSettings) -> Result<()> {
        if next.tor_mode == "proxy" {
            parse_socks(next.proxy.as_deref().unwrap_or(""))?;
        }
        *self.settings.lock().unwrap() = next.clone();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&self.path, toml::to_string_pretty(&next)?).context("saving the network settings")?;
        *self.socks.lock().unwrap() = None;
        self.clients.lock().unwrap().clear();
        if let Some((_, task)) = self.relay.lock().await.take() {
            task.abort();
        }
        Ok(())
    }

    /// Says whether connections under `key` should be routed.
    pub fn mark(&self, key: &str, routed: bool) {
        if key.is_empty() {
            return;
        }
        self.wants.lock().unwrap().insert(key.to_string(), routed);
    }

    /// Whether `key` itself asked to be routed, apart from everything being
    /// routed. What an account being created inherits from its add form.
    pub fn wanted(&self, key: &str) -> bool {
        self.wants.lock().unwrap().get(key).copied().unwrap_or(false)
    }

    /// Whether connections under `key` are routed: the key asked to be, or
    /// everything is.
    pub fn routed(&self, key: &str) -> bool {
        self.settings.lock().unwrap().tunnel_all || self.wants.lock().unwrap().get(key).copied().unwrap_or(false)
    }

    /// Whether requests that belong to no account are routed: uploads to a
    /// file host, an export's media, a link preview fetched here.
    ///
    /// When everything is routed, of course - and also whenever any account
    /// is, because such a request is usually on some account's behalf (a
    /// picture uploaded to be posted on a routed IRC network) and cannot say
    /// whose. Going the safer way costs speed; going the other way could put
    /// a routed account's upload on its owner's real address.
    pub fn general_routed(&self) -> bool {
        self.tunnel_all() || self.wants.lock().unwrap().iter().any(|(key, routed)| *routed && !key.ends_with(":pending"))
    }

    /// Makes the route ready if requests that belong to no account are
    /// routed. For async callers about to make one.
    pub async fn ready_for_general(&self) -> Result<()> {
        if self.general_routed() {
            self.ready(|_| {}).await?;
        }
        Ok(())
    }

    pub fn tunnel_all(&self) -> bool {
        self.settings.lock().unwrap().tunnel_all
    }

    /// Makes routed connections possible, and says where they go: starts
    /// moho's Tor and its relay, or reads the proxy's address. Cheap once
    /// done. Called by whatever is about to make a routed connection, so that
    /// nothing is started for an account that is not routed.
    pub async fn ready(&self, on_progress: impl FnOnce(&str)) -> Result<(String, u16)> {
        if let Some(found) = self.socks.lock().unwrap().clone() {
            return Ok(found);
        }
        let settings = self.settings();
        let found = if settings.tor_mode == "proxy" {
            parse_socks(settings.proxy.as_deref().unwrap_or(""))?
        } else {
            // Bootstrapped before the relay is offered, so the first thing
            // through it does not wait out a cold start on its own clock.
            self.tor.get_or_bootstrap(on_progress).await.context("starting Tor")?;
            let mut relay = self.relay.lock().await;
            let port = match relay.as_ref() {
                Some((port, _)) => *port,
                None => {
                    let (port, task) = start_relay(self.tor.clone()).await?;
                    *relay = Some((port, task));
                    port
                }
            };
            ("127.0.0.1".to_string(), port)
        };
        *self.socks.lock().unwrap() = Some(found.clone());
        Ok(found)
    }

    /// The SOCKS5 address, if one is ready.
    pub fn socks_now(&self) -> Option<(String, u16)> {
        self.socks.lock().unwrap().clone()
    }

    /// The proxy a client for `key` should use: none when it is not routed,
    /// the SOCKS5 address when it is, and nowhere at all when it is routed
    /// and nothing is ready yet.
    #[cfg(test)]
    pub fn proxy_for(&self, key: &str) -> Option<String> {
        self.proxy_if(self.routed(key))
    }

    /// The same, for a caller that has decided for itself whether it is
    /// routed.
    pub fn proxy_if(&self, routed: bool) -> Option<String> {
        if !routed {
            return None;
        }
        Some(match self.socks_now() {
            Some((host, port)) => format!("socks5h://{host}:{port}"),
            None => NOWHERE.to_string(),
        })
    }

    /// An HTTP client built by `build`, routed or not as the caller decided -
    /// by an account's key, or as a service.
    ///
    /// Cached per service and per destination, so the usual case - nothing
    /// routed - is the one client each service always had.
    pub fn client_if(
        &self,
        service: &'static str,
        routed: bool,
        build: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> reqwest::Client {
        let proxy = self.proxy_if(routed);
        let cache_key = (service, proxy.clone().unwrap_or_default());
        if let Some(client) = self.clients.lock().unwrap().get(&cache_key) {
            return client.clone();
        }
        let mut builder = build(reqwest::Client::builder());
        if let Some(proxy) = &proxy {
            match reqwest::Proxy::all(proxy) {
                Ok(p) => builder = builder.proxy(p),
                // Should not happen - the address was built here - but if it
                // did, a client with no proxy would leave directly.
                Err(_) => builder = builder.proxy(reqwest::Proxy::all(NOWHERE).expect("a fixed address parses")),
            }
        } else {
            // No proxy from the environment either: whether this is routed is
            // decided here, not by an HTTPS_PROXY somebody once exported.
            builder = builder.no_proxy();
        }
        let client = builder.build().unwrap_or_else(|_| reqwest::Client::new());
        // Not kept when it goes nowhere. A client asked for before the route
        // was ready points at the address that refuses everything, which is
        // right for that moment - but cached, every later caller got it too,
        // long after Tor was up.
        if proxy.as_deref() != Some(NOWHERE) {
            self.clients.lock().unwrap().insert(cache_key, client.clone());
        }
        client
    }

    /// A TCP connection to `host:port` for `key`, through the SOCKS5 address
    /// when it is routed. For what is not plain HTTP: websockets.
    pub async fn connect(&self, key: &str, host: &str, port: u16) -> Result<TcpOrSocks> {
        if !self.routed(key) {
            let stream = TcpStream::connect((host, port)).await.with_context(|| format!("connecting to {host}:{port}"))?;
            return Ok(TcpOrSocks::Tcp(stream));
        }
        let (ph, pp) = self.ready(|_| {}).await?;
        let stream = tokio_socks::tcp::Socks5Stream::connect((ph.as_str(), pp), (host, port))
            .await
            .with_context(|| format!("connecting to {host}:{port} through the proxy"))?;
        Ok(TcpOrSocks::Socks(stream))
    }
}

/// A connection made directly or through SOCKS5, as one type a websocket
/// library can take.
pub enum TcpOrSocks {
    Tcp(TcpStream),
    Socks(tokio_socks::tcp::Socks5Stream<TcpStream>),
}

impl tokio::io::AsyncRead for TcpOrSocks {
    fn poll_read(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TcpOrSocks::Tcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            TcpOrSocks::Socks(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for TcpOrSocks {
    fn poll_write(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            TcpOrSocks::Tcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            TcpOrSocks::Socks(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TcpOrSocks::Tcp(s) => std::pin::Pin::new(s).poll_flush(cx),
            TcpOrSocks::Socks(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            TcpOrSocks::Tcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            TcpOrSocks::Socks(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Opens a websocket for `key`, routed as `key` is.
pub async fn websocket<R>(
    key: &str,
    request: R,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpOrSocks>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)>
where
    R: tokio_tungstenite::tungstenite::client::IntoClientRequest + Unpin,
{
    let request = request.into_client_request().context("building the websocket request")?;
    let uri = request.uri().clone();
    let host = uri.host().ok_or_else(|| anyhow!("a websocket address with no host"))?.to_string();
    let tls = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let stream = router().connect(key, &host, port).await?;
    tokio_tungstenite::client_async_tls(request, stream).await.context("opening the websocket")
}

/// The key for a service's requests made before there is an account - the
/// sign-in itself, and what its form asks while it is being filled in. The
/// add form's switch decides it.
pub fn pending_key(service: &str) -> String {
    format!("{service}:pending")
}

/// Reads `socks5://host:port` or `socks5h://host:port`.
fn parse_socks(url: &str) -> Result<(String, u16)> {
    let url = url.trim();
    if url.is_empty() {
        bail!("no proxy address is set - enter one like socks5h://127.0.0.1:9050");
    }
    let with_scheme = if url.contains("://") { url.to_string() } else { format!("socks5h://{url}") };
    let parsed = url::Url::parse(&with_scheme).with_context(|| format!("reading the proxy address {url}"))?;
    if !matches!(parsed.scheme(), "socks5" | "socks5h") {
        bail!("the proxy has to be SOCKS5 (socks5h://host:port), not {}", parsed.scheme());
    }
    let host = parsed.host_str().ok_or_else(|| anyhow!("the proxy address has no host"))?.to_string();
    Ok((host, parsed.port().unwrap_or(9050)))
}

/// A SOCKS5 server on loopback that sends every connection through moho's
/// own Tor. No authentication, because Chromium cannot send any; reachable
/// only from this machine.
async fn start_relay(tor: Arc<TorManager>) -> Result<(u16, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("opening the Tor relay")?;
    let port = listener.local_addr()?.port();
    tracing::info!("net: Tor relay listening on 127.0.0.1:{port}");
    let task = tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else { continue };
            let tor = tor.clone();
            tokio::spawn(async move {
                if let Err(e) = relay_one(client, tor).await {
                    tracing::debug!("net: relayed connection ended: {e:#}");
                }
            });
        }
    });
    Ok((port, task))
}

async fn relay_one(mut client: TcpStream, tor: Arc<TorManager>) -> Result<()> {
    let (host, port) = socks_handshake(&mut client).await?;
    let tor_client = tor.get_or_bootstrap(|_| {}).await?;
    let mut upstream: BoxStream = match tor_client.connect((host.as_str(), port)).await {
        Ok(stream) => Box::new(stream),
        Err(e) => {
            // General failure, so the far end is told rather than left hanging.
            let _ = client.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            return Err(anyhow!("Tor could not reach {host}:{port}: {e}"));
        }
    };
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// The server half of a SOCKS5 greeting and CONNECT, as far as the address.
async fn socks_handshake<S: AsyncReadExt + AsyncWriteExt + Unpin>(stream: &mut S) -> Result<(String, u16)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 {
        bail!("not SOCKS5");
    }
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 0xff]).await?;
        bail!("the client wants authentication");
    }
    stream.write_all(&[5, 0]).await?;
    let mut request = [0u8; 4];
    stream.read_exact(&mut request).await?;
    if request[1] != 1 {
        // Command not supported: only CONNECT. UDP in particular cannot go
        // through Tor.
        stream.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        bail!("only CONNECT is relayed");
    }
    let host = match request[3] {
        1 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            stream.read_exact(&mut name).await?;
            String::from_utf8(name).context("a host name that is not text")?
        }
        4 => {
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        other => bail!("unknown address type {other}"),
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router_at(name: &str) -> Router {
        let dir = std::env::temp_dir().join(format!("nobilis-route-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Router::open(dir.join("net.toml"), Arc::new(TorManager::new(&dir)), None)
    }

    #[test]
    fn nothing_is_routed_until_asked() {
        let r = router_at("none");
        assert!(!r.routed("discord:1"));
        assert_eq!(r.proxy_for("discord:1"), None);
    }

    /// The one that matters: routed and not ready must not mean direct.
    #[test]
    fn a_routed_key_with_nothing_ready_goes_nowhere_rather_than_direct() {
        let r = router_at("closed");
        r.mark("discord:1", true);
        assert_eq!(r.proxy_for("discord:1").as_deref(), Some(NOWHERE));
        assert_eq!(r.proxy_for("discord:2"), None, "only the account that asked");
    }

    /// A client made before the route is ready goes nowhere - and must not
    /// be what everybody gets afterwards. Cached, it was: a Tor Kick account
    /// built its client before Tor started and failed every lookup for the
    /// rest of the session.
    #[test]
    fn a_client_that_goes_nowhere_is_not_kept() {
        let r = router_at("early");
        r.client_if("svc", true, |b| b);
        assert!(r.clients.lock().unwrap().is_empty(), "the nowhere client was cached");
        *r.socks.lock().unwrap() = Some(("127.0.0.1".into(), 9150));
        r.client_if("svc", true, |b| b);
        assert_eq!(r.clients.lock().unwrap().len(), 1, "a client with a real route is kept");
    }

    #[tokio::test]
    async fn tunnelling_everything_routes_every_key_and_a_proxy_is_used_as_given() {
        let r = router_at("all");
        r.set_settings(NetSettings { tor_mode: "proxy".into(), proxy: Some("socks5h://127.0.0.1:9150".into()), tunnel_all: true })
            .await
            .unwrap();
        assert!(r.routed("anything"));
        assert_eq!(r.ready(|_| {}).await.unwrap(), ("127.0.0.1".to_string(), 9150));
        assert_eq!(r.proxy_for("matrix:@a:b").as_deref(), Some("socks5h://127.0.0.1:9150"));
    }

    #[tokio::test]
    async fn a_proxy_mode_without_an_address_is_refused() {
        let r = router_at("bad");
        assert!(r.set_settings(NetSettings { tor_mode: "proxy".into(), proxy: None, tunnel_all: false }).await.is_err());
        assert!(r.set_settings(NetSettings { tor_mode: "proxy".into(), proxy: Some("http://x:1".into()), tunnel_all: false }).await.is_err());
    }

    #[test]
    fn settings_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("nobilis-route-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tor = Arc::new(TorManager::new(&dir));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let wanted = NetSettings { tor_mode: "embedded".into(), proxy: None, tunnel_all: true };
        rt.block_on(Router::open(dir.join("net.toml"), tor.clone(), None).set_settings(wanted.clone())).unwrap();
        assert_eq!(Router::open(dir.join("net.toml"), tor, None).settings(), wanted);
    }

    #[tokio::test]
    async fn the_relay_reads_a_socks5_connect_request() {
        let (mut a, mut b) = tokio::io::duplex(256);
        let server = tokio::spawn(async move { socks_handshake(&mut b).await });
        a.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0u8; 2];
        a.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0]);
        let mut req = vec![5, 1, 0, 3, 11];
        req.extend_from_slice(b"example.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        a.write_all(&req).await.unwrap();
        assert_eq!(server.await.unwrap().unwrap(), ("example.com".to_string(), 443));
    }
}
