//! Live checks against the real site, run by hand.
//!
//! Signing in, clearing the gate, answering the captcha - each of these does
//! the real thing against the real forum, which is the only way any of it can
//! be known to work. `#[ignore]`d, and they take credentials from the
//! environment rather than from anywhere in this repository.


#[cfg(test)]
pub(super) mod live_probe {
    use crate::backend::sneedchat::auth::{Credentials, Session, TwoFactor};
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

    /// What the chat page says about which rooms exist. Not run by default:
    ///   cargo test --release -- --ignored --nocapture sneedchat_rooms_probe
    ///
    /// The room catalogue has been a literal in the frontend since the start,
    /// with a comment saying no endpoint lists them. This is how that gets
    /// settled: fetch the page a browser would and print every fragment that
    /// mentions a room, so the answer comes from the site rather than from
    /// assumption.
    #[tokio::test]
    #[ignore]
    async fn sneedchat_rooms_probe() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let http = HttpClient::new(Transport::Tor(client), CookieJar::new(), crate::backend::sneedchat::DEFAULT_USER_AGENT.to_string());
        let url = format!("https://{}/test-chat", crate::backend::sneedchat::DEFAULT_ONION);

        let mut resp = http.get(&url).await.expect("GET /chat failed");
        if resp.status == pow::GATE_STATUS {
            pow::clear(&http, &url, 8).await.expect("failed to clear the gate");
            resp = http.get(&url).await.expect("GET after clearing gate failed");
        }
        println!("HTTP {} body {} bytes", resp.status, resp.body.len());

        let body = resp.body.clone();
        for (i, line) in body.lines().enumerate() {
            let lower = line.to_lowercase();
            if lower.contains("room") || lower.contains("chat.ws") || lower.contains("channel") {
                println!("{i}: {}", line.trim().chars().take(400).collect::<String>());
            }
        }
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
            Ok(false) => println!("SESSION IS DEAD - the site does not know us, which is why a login is attempted"),
            Err(e) => println!("could not tell: {e:#}"),
        }
    }

    /// What the login page's captcha demands, and whether it can be answered
    /// without a browser.
    ///
    /// The login form carries a Tartarus (`.ttrs`) captcha whose widget is a
    /// SHA-256 proof of work in several rounds - the same shape as the gate in
    /// `pow.rs`, and answerable the same way, *unless* the server asks for a
    /// Monocle browser assessment instead, which nothing headless can produce.
    /// This asks it and prints the answer. Not run by default:
    ///   cargo test --release -- --ignored --nocapture sneedchat_captcha_probe
    #[tokio::test]
    #[ignore]
    async fn sneedchat_captcha_probe() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");
        let http = HttpClient::new(Transport::Tor(client), CookieJar::new(), crate::backend::sneedchat::DEFAULT_USER_AGENT.to_string());
        let base = format!("https://{}", crate::backend::sneedchat::DEFAULT_ONION);

        // The login page first, both for the site key and for the clearance
        // cookie the gate hands out - the captcha lives behind the same gate.
        let login = format!("{base}/login/");
        let mut page = http.get(&login).await.expect("GET /login/ failed");
        if page.status == pow::GATE_STATUS {
            pow::clear(&http, &login, 8).await.expect("failed to clear the gate");
            page = http.get(&login).await.expect("GET after clearing gate failed");
        }
        let sitekey = page
            .body
            .split("data-sitekey=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("no data-sitekey on the login page")
            .to_string();
        println!("sitekey: {sitekey}");

        // The widget opens with a GET of `<apiBase>/start?key=...`; every
        // later step is a POST to /verify with the nonce it found.
        let url = format!("{base}/.ttrs/captcha/start?key={sitekey}");
        match http.get(&url).await {
            Ok(resp) => println!("GET /.ttrs/captcha/start -> HTTP {}\n{}", resp.status, resp.body.chars().take(1500).collect::<String>()),
            Err(e) => println!("GET /.ttrs/captcha/start -> {e:#}"),
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

    /// What the login form actually asks for.
    ///
    /// The login is posted by echoing the form's own fields back with the
    /// username and password filled in, so a field the site adds is a field
    /// this client sends empty. When a login starts being refused for a
    /// reason that is not the password, this is the thing to look at first -
    /// it prints every input on the form and every captcha provider named
    /// anywhere on the page. Not run by default:
    ///   cargo test --release -- --ignored --nocapture sneedchat_login_form_probe
    #[tokio::test]
    #[ignore]
    async fn sneedchat_login_form_probe() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let http = HttpClient::new(Transport::Tor(client), CookieJar::new(), crate::backend::sneedchat::DEFAULT_USER_AGENT.to_string());
        let url = format!("https://{}/login/", crate::backend::sneedchat::DEFAULT_ONION);
        let mut resp = http.get(&url).await.expect("GET /login/ failed");
        if resp.status == pow::GATE_STATUS {
            pow::clear(&http, &url, 8).await.expect("failed to clear the gate");
            resp = http.get(&url).await.expect("GET after clearing gate failed");
        }
        println!("HTTP {} body {} bytes", resp.status, resp.body.len());

        match crate::backend::sneedchat::form::form_section(&resp.body, "/login/login") {
            Some(section) => {
                println!("--- fields the form carries ---");
                for (name, value) in crate::backend::sneedchat::form::inputs(section) {
                    let shown = if value.len() > 24 { format!("{}… ({} chars)", &value[..24], value.len()) } else { value };
                    println!("  {name} = {shown:?}");
                }
            }
            None => println!("!! no /login/login form section found - the layout has changed"),
        }

        println!("--- captcha markers anywhere on the page ---");
        for marker in [
            "recaptcha", "hcaptcha", "turnstile", "captcha_question", "captcha",
            "data-sitekey", "cf-challenge", "friendly-challenge",
        ] {
            let hits = resp.body.to_lowercase().matches(marker).count();
            if hits > 0 {
                println!("  {marker}: {hits}");
            }
        }
        for line in resp.body.lines() {
            let lower = line.to_lowercase();
            if lower.contains("captcha") || lower.contains("sitekey") || lower.contains("turnstile") {
                println!("  > {}", line.trim().chars().take(300).collect::<String>());
            }
        }
    }

    /// Phase-3 live check: a real login. Reads credentials from the
    /// environment rather than taking them as literals anywhere in this
    /// repo or conversation - set them in your own shell before running:
    ///
    ///   SNEEDCHAT_USERNAME=... SNEEDCHAT_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture sneedchat_login_probe
    ///
    /// Add SNEEDCHAT_TOTP_SECRET=... too if the account has 2FA enabled.
    /// Skips (rather than failing) if SNEEDCHAT_USERNAME/PASSWORD aren't set,
    /// so this is safe to leave in the suite without real credentials
    /// present in CI or anyone else's environment.
    #[tokio::test]
    #[ignore]
    async fn sneedchat_login_probe() {
        let Ok(username) = std::env::var("SNEEDCHAT_USERNAME") else {
            println!("SNEEDCHAT_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("SNEEDCHAT_PASSWORD") else {
            println!("SNEEDCHAT_PASSWORD not set, skipping");
            return;
        };
        let two_factor = match std::env::var("SNEEDCHAT_TOTP_SECRET") {
            Ok(secret) => TwoFactor::Totp(crate::backend::sneedchat::totp::decode_secret(&secret).expect("SNEEDCHAT_TOTP_SECRET is not valid base32")),
            Err(_) => TwoFactor::None,
        };

        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("nobilis-tor-spike");
        let manager = TorManager::new(&dir);
        let client = manager.get_or_bootstrap(|msg| println!("progress: {msg}")).await.expect("Tor bootstrap failed");

        let base = format!("https://{}", crate::backend::sneedchat::DEFAULT_ONION);
        let session = Session::new(Transport::Tor(client), base, crate::backend::sneedchat::DEFAULT_USER_AGENT.to_string());
        let creds = Credentials { username, password };

        println!("logging in...");
        session.ensure_authenticated(&creds, &two_factor).await.expect("login failed");
        println!("authenticated. user id: {:?}", session.user_id());
        assert!(session.is_authenticated().await.expect("post-login check failed"), "session reports not authenticated right after logging in");
    }
}
