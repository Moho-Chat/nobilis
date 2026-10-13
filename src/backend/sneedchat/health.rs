//! What is down, when Sneedchat will not connect.
//!
//! A room that cannot connect looks the same from inside the socket whatever
//! the reason: the upgrade fails, or the stream closes, or nothing arrives.
//! But the reason is what a person wants - whether to wait for the chat to
//! come back, check their own connection, or give up on Tor for the evening -
//! and each has a different cure here as well. So a failure is followed by
//! three questions, each only asked if the one before it was answered yes:
//!
//! 1. Is the route working? Over Tor, a well-known site is reached through it:
//!    if that fails, Tor is the problem and nothing about the forum is known.
//! 2. Is the forum up? Its front page is asked for. An error or no answer
//!    means the site is down (or, on the open internet, unreachable from
//!    here, which this end cannot tell apart from the site being down).
//! 3. Then the chat is down: the route works and the forum answers, and only
//!    the chat's socket will not.
//!
//! Asked once per account per half minute at most: every room fails together
//! when the chat goes down, and each asking separately would be the same
//! three requests a dozen times over.

use super::*;
use crate::model::BufferLink;

/// Which part is down.
#[derive(Clone, Debug, PartialEq)]
pub enum Outage {
    /// Tor itself - or the proxy standing in for it - is not carrying traffic.
    Tor(String),
    /// The forum does not answer, or answers with an error.
    Site(String),
    /// The forum answers; the chat does not.
    Chat,
}

impl Outage {
    pub fn link(&self, host: &str) -> BufferLink {
        match self {
            Outage::Tor(why) => BufferLink::down("tor", why.clone()),
            Outage::Site(why) => BufferLink::down("site", why.clone()),
            Outage::Chat => BufferLink::down("chat", format!("The chat is down - {host} itself is up")),
        }
    }

    pub fn describe(&self, host: &str) -> String {
        self.link(host).detail.unwrap_or_default()
    }
}

/// A site reached through Tor to tell whether Tor works at all. The Tor
/// Project's own, because it exists to be reached through Tor and is the one
/// site whose being down would be news.
const TOR_PROBE: (&str, u16) = ("check.torproject.org", 443);

/// How long one diagnosis is trusted - see the module comment.
const REMEMBERED: Duration = Duration::from_secs(30);

fn remembered() -> &'static std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Outage)>> {
    static LAST: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Outage)>>> =
        std::sync::OnceLock::new();
    LAST.get_or_init(Default::default)
}

/// What is down for this account, asked of the network or remembered from
/// the last half minute.
pub async fn diagnose(account_id: &str, transport: &Transport, host: &str) -> Outage {
    if let Some((at, outage)) = remembered().lock().unwrap().get(account_id) {
        if at.elapsed() < REMEMBERED {
            return outage.clone();
        }
    }
    let outage = ask(transport, host).await;
    tracing::info!("sneedchat[{account_id}]: {}", outage.describe(host));
    remembered().lock().unwrap().insert(account_id.to_string(), (std::time::Instant::now(), outage.clone()));
    outage
}

/// Forgets the last diagnosis, once something has connected again.
pub fn recovered(account_id: &str) {
    remembered().lock().unwrap().remove(account_id);
}

async fn ask(transport: &Transport, host: &str) -> Outage {
    let routed = !matches!(transport, Transport::Direct);
    if routed {
        let (probe, port) = TOR_PROBE;
        let reached = tokio::time::timeout(Duration::from_secs(30), transport.connect(probe, port, true)).await;
        if !matches!(reached, Ok(Ok(_))) {
            return Outage::Tor(match transport {
                Transport::Socks { host, port } => format!("The Tor proxy at {host}:{port} isn't carrying traffic"),
                _ => "Tor isn't connecting".to_string(),
            });
        }
    }

    let http = http::HttpClient::new(transport.clone(), http::CookieJar::new(), DEFAULT_USER_AGENT.to_string());
    let front = tokio::time::timeout(Duration::from_secs(30), http.get_bytes(&format!("https://{host}/"))).await;
    match front {
        // The anti-bot gate is the forum answering too.
        Ok(Ok((status, _))) if status < 500 => Outage::Chat,
        Ok(Ok((status, _))) => Outage::Site(format!("{host} is down - it answered HTTP {status}")),
        _ if routed => Outage::Site(format!("{host} isn't answering over Tor - Tor itself is working")),
        _ => Outage::Site(format!("Can't reach {host} - the site, or this connection, is down")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each outage is said as what failed, in the cause a client draws by.
    #[test]
    fn each_outage_names_its_part() {
        let host = "kiwifarms.st";
        assert_eq!(Outage::Tor("Tor isn't connecting".into()).link(host).cause.as_deref(), Some("tor"));
        assert_eq!(Outage::Site("down".into()).link(host).cause.as_deref(), Some("site"));
        let chat = Outage::Chat.link(host);
        assert_eq!(chat.cause.as_deref(), Some("chat"));
        assert_eq!(chat.state, "down");
        assert!(chat.detail.unwrap().contains("kiwifarms.st itself is up"));
    }

    /// Against the real forum, over the open internet: with the forum up,
    /// a failing chat is the chat's own fault. Network, so not by default:
    ///   cargo test --release -- --ignored --nocapture health_probe
    #[tokio::test]
    #[ignore]
    async fn health_probe() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let outage = ask(&Transport::Direct, "kiwifarms.st").await;
        println!("{outage:?}");
        let missing = ask(&Transport::Direct, "no-such-host.invalid").await;
        println!("{missing:?}");
        assert!(matches!(missing, Outage::Site(_)));
    }

    /// The same over Tor, to the onion address: the route is checked first,
    /// then the forum.
    ///   cargo test --release -- --ignored --nocapture health_probe_tor
    #[tokio::test]
    #[ignore]
    async fn health_probe_tor() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-health-probe");
        let tor = crate::net::tor::TorManager::new(&dir);
        let client = tor.get_or_bootstrap(|_| {}).await.expect("bootstrapping Tor");
        let outage = ask(&Transport::Tor(client), DEFAULT_ONION).await;
        println!("{outage:?}");
    }
}
