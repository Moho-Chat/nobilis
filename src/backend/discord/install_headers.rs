//! What a Discord client tells the API about the install it runs on: which install, in what language, in what
//! timezone.
//!
//! Discord's own clients send these with every request (`X-Installation-ID`, `X-Discord-Locale`,
//! `X-Discord-Timezone`), so an API that sees a long-lived account from one install, in one place, with one
//! language, sees what it expects. They say nothing about *what* is connecting - that stays `moho/<version>
//! (nobilis)` - and the install id is moho's own, made here once and kept: it is not a copy of anything Discord
//! issued.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::sync::OnceLock;

/// The id of this install: random, made the first time it is needed, and kept in the data directory so that every
/// run - and every account on it - is the same install.
pub fn installation_id() -> String {
    let file = crate::default_data_dir().join("discord-installation-id");
    if let Some(id) = std::fs::read_to_string(&file).ok().map(|t| t.trim().to_string()).filter(|t| well_formed(t)) {
        return id;
    }
    let bytes: [u8; 16] = rand::random();
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&file, &id);
    id
}

fn well_formed(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The language this machine is set to, as Discord writes it (`en-US`): from the environment's usual variables, in
/// their usual order of precedence. `None` when it says nothing, or says "C".
pub fn locale() -> Option<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"].iter().filter_map(|v| std::env::var(v).ok()).find(|v| !v.is_empty())?;
    locale_from(&raw)
}

fn locale_from(raw: &str) -> Option<String> {
    // `en_US.UTF-8@euro` -> `en-US`
    let tag = raw.split(['.', '@']).next()?.replace('_', "-");
    let mut parts = tag.split('-');
    let language = parts.next().filter(|l| (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphabetic()))?;
    if language.eq_ignore_ascii_case("C") || language.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    let language = language.to_ascii_lowercase();
    match parts.next().filter(|r| r.len() == 2 && r.bytes().all(|b| b.is_ascii_alphabetic())) {
        Some(region) => Some(format!("{language}-{}", region.to_ascii_uppercase())),
        None => Some(language),
    }
}

/// The IANA name of this machine's timezone (`America/New_York`), when it can be told.
pub fn timezone() -> Option<String> {
    iana_time_zone::get_timezone().ok().filter(|t| !t.is_empty())
}

/// The headers every API request carries.
pub fn headers() -> &'static HeaderMap {
    static HEADERS: OnceLock<HeaderMap> = OnceLock::new();
    HEADERS.get_or_init(|| {
        let mut pairs: Vec<(&'static str, String)> = vec![("x-installation-id", installation_id())];
        if let Some(locale) = locale() {
            let language = locale.split('-').next().unwrap_or("en").to_string();
            pairs.push(("accept-language", format!("{locale},{language};q=0.9")));
            pairs.push(("x-discord-locale", locale));
        }
        if let Some(zone) = timezone() {
            pairs.push(("x-discord-timezone", zone));
        }
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            if let Ok(value) = HeaderValue::from_str(&value) {
                map.insert(HeaderName::from_static(name), value);
            }
        }
        map
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locale_is_written_the_way_discord_writes_it() {
        assert_eq!(locale_from("en_US.UTF-8").as_deref(), Some("en-US"));
        assert_eq!(locale_from("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(locale_from("fr").as_deref(), Some("fr"));
        assert_eq!(locale_from("pt-br").as_deref(), Some("pt-BR"));
    }

    #[test]
    fn a_machine_with_no_language_set_sends_none() {
        assert_eq!(locale_from("C"), None);
        assert_eq!(locale_from("POSIX"), None);
        assert_eq!(locale_from("C.UTF-8"), None);
        assert_eq!(locale_from(""), None);
        assert_eq!(locale_from("12345"), None);
    }

    #[test]
    fn an_installation_id_is_recognised_only_when_whole() {
        assert!(well_formed("0123456789abcdef0123456789abcdef"));
        assert!(!well_formed("0123"));
        assert!(!well_formed("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
    }

    /// What actually leaves: the API client carries the headers and still says it is moho, and the client for the CDN
    /// and sign-in carries none of them. Asked of a local listener, so nothing reaches Discord.
    #[tokio::test]
    async fn the_api_client_sends_them_and_the_other_does_not() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn request_seen_by_a_listener(client: reqwest::Client) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                String::from_utf8_lossy(&buf[..n]).to_lowercase()
            });
            let _ = client.get(url).send().await;
            server.await.unwrap()
        }
        let api = request_seen_by_a_listener(crate::backend::discord::http::http_client_for("a-test-token")).await;
        assert!(api.contains("x-installation-id: "), "{api}");
        assert!(api.contains("user-agent: moho/"), "{api}");
        assert!(!api.contains("x-super-properties"), "{api}");
        let other = request_seen_by_a_listener(crate::backend::discord::http::anonymous_client()).await;
        assert!(!other.contains("x-installation-id"), "{other}");
    }
}
