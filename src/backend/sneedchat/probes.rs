//! Live checks against the real site, run by hand.
//!
//! Signing in, clearing the gate, answering the captcha - each of these does
//! the real thing against the real forum, which is the only way any of it can
//! be known to work. `#[ignore]`d, and they take credentials from the
//! environment rather than from anywhere in this repository.


#[cfg(test)]
pub(super) mod live_probe {
    use crate::backend::sneedchat::auth::Session;
    use crate::backend::sneedchat::http::{CookieJar, HttpClient};
    use crate::backend::sneedchat::pow;
    use crate::net::tor::{Transport, TorManager};

    /// Phase-2 live check: fetch the real login page through embedded Tor
    /// and confirm the KiwiFlare gate (if hit) is solved for real, ending
    /// with a normal page response. Not run by default; run explicitly:
    ///   cargo test --release -- --ignored --nocapture sneedchat_http_probe
    #[tokio::test]
    #[ignore]
    async fn sneedchat_http_probe() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let http = HttpClient::new(Transport::Tor(client), CookieJar::new(), crate::backend::sneedchat::DEFAULT_USER_AGENT.to_string());
        let url = format!("https://{}/", crate::backend::sneedchat::DEFAULT_ONION);

        let resp = http.get(&url).await.expect("initial GET failed");
        println!("initial GET {url} -> HTTP {}", resp.status);

        let final_resp = if resp.status == pow::GATE_STATUS {
            println!("hit the KiwiFlare gate, solving...");
            let solved = pow::clear(&http, &url, 8).await.expect("failed to clear the gate");
            println!("solved {solved} challenge(s)");
            http.get(&url).await.expect("GET after clearing gate failed")
        } else {
            resp
        };

        println!("final status: HTTP {}, body length {} bytes", final_resp.status, final_resp.body.len());
        assert!((200..400).contains(&final_resp.status), "expected a normal page response, got HTTP {}", final_resp.status);
    }

    /// Whether the session this account already holds is still good.
    ///
    /// The login form is a fallback: cookies captured earlier are restored
    /// into the jar and the site is asked who it thinks we are, and only a
    /// "nobody" sends this anywhere near a password. So when logging in
    /// starts failing, the question before "why can we not log in" is "why
    /// are we logging in at all" - which is this. Pass the cookies the way
    /// the account stores them:
    ///   SNEEDCHAT_COOKIES='xf_user=…; xf_session=…' \
    ///     cargo test --release -- --ignored --nocapture sneedchat_session_probe
    #[tokio::test]
    #[ignore]
    async fn sneedchat_session_probe() {
        let Ok(cookies) = std::env::var("SNEEDCHAT_COOKIES") else {
            println!("SNEEDCHAT_COOKIES not set, skipping");
            return;
        };
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let jar = CookieJar::new();
        for pair in cookies.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else { continue };
            println!("restoring cookie {name} ({} chars)", value.len());
            jar.set(name.trim(), value.trim());
        }

        let base = format!("https://{}", crate::backend::sneedchat::DEFAULT_ONION);
        let session = Session::new(Transport::Tor(client), base, crate::backend::sneedchat::DEFAULT_USER_AGENT.to_string());
        session.http.jar.restore(jar.snapshot());

        match session.is_authenticated().await {
            Ok(true) => println!("SESSION IS STILL GOOD - the site says we are logged in, user id {:?}", session.user_id()),
            Ok(false) => println!("SESSION IS DEAD - the site does not know us; the account needs a browser sign-in"),
            Err(e) => println!("could not tell: {e:#}"),
        }
    }

    /// Anything the site serves, through Tor, printed.
    ///
    /// A diagnosis tool rather than a check: when the site changes something,
    /// the question is usually "what does it actually send now", and reaching
    /// a `.onion` from a shell is not something this machine can otherwise
    /// do. Give it a path:
    ///   SNEEDCHAT_PATH=/login/ cargo test --release -- --ignored --nocapture sneedchat_fetch_probe
    #[tokio::test]
    #[ignore]
    async fn sneedchat_fetch_probe() {
        let Ok(path) = std::env::var("SNEEDCHAT_PATH") else {
            println!("SNEEDCHAT_PATH not set, skipping");
            return;
        };
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let http = HttpClient::new(Transport::Tor(client), CookieJar::new(), crate::backend::sneedchat::DEFAULT_USER_AGENT.to_string());
        let url = format!("https://{}{}", crate::backend::sneedchat::DEFAULT_ONION, path);
        let mut resp = http.get(&url).await.expect("GET failed");
        if resp.status == pow::GATE_STATUS {
            pow::clear(&http, &url, 8).await.expect("failed to clear the gate");
            resp = http.get(&url).await.expect("GET after clearing gate failed");
        }
        println!("HTTP {} body {} bytes", resp.status, resp.body.len());
        println!("{}", resp.body);
    }
}
