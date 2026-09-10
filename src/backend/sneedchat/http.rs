//! A small HTTP/1.1 client that rides our [`Transport`].
//!
//! Ported from sneedchat-rs's `net/http.rs`
//! (<https://gitgud.io/jcmoon/sneedchat-rs>). `reqwest` (already used by
//! `backend/discord/http.rs`) cannot use an in-process Arti stream as its
//! connector - it only knows how to dial its own connector or a SOCKS5
//! proxy *URL* - so rather than run two different transport stacks for this
//! one backend, hyper is driven directly. The surface needed is small: GET
//! and form POST, redirects, and a cookie jar shared with the websocket
//! handshake.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderValue, LOCATION, SET_COOKIE};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;

use crate::net::tor::Transport;

/// Cookie store shared between HTTP requests and the websocket upgrade.
///
/// Deliberately simple: the site scopes everything to one host, so
/// attributes beyond name/value aren't tracked. Ordering is stable so the
/// `Cookie` header doesn't churn between requests, which would otherwise
/// look like a shifting client fingerprint to the site's anti-bot gate.
#[derive(Clone, Default)]
pub struct CookieJar {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, name: &str, value: &str) {
        self.inner.lock().unwrap().insert(name.to_string(), value.to_string());
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.inner.lock().unwrap().get(name).cloned()
    }

    pub fn remove(&self, name: &str) {
        self.inner.lock().unwrap().remove(name);
    }

    /// Copy the jar out, for persisting a logged-in session between runs.
    pub fn snapshot(&self) -> std::collections::BTreeMap<String, String> {
        self.inner.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Merge saved cookies in, skipping empty saved values.
    pub fn restore(&self, saved: impl IntoIterator<Item = (String, String)>) {
        let mut jar = self.inner.lock().unwrap();
        for (k, v) in saved {
            if !v.is_empty() {
                jar.insert(k, v);
            }
        }
    }

    /// Absorb `Set-Cookie` headers from a response.
    fn absorb(&self, resp: &hyper::Response<hyper::body::Incoming>) {
        for hv in resp.headers().get_all(SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            let Some(pair) = s.split(';').next() else { continue };
            if let Some((k, v)) = pair.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if v.is_empty() {
                    self.inner.lock().unwrap().remove(k);
                } else {
                    self.set(k, v);
                }
            }
        }
    }

    /// Render the `Cookie` header value, or `None` when the jar is empty.
    pub fn header(&self) -> Option<String> {
        let jar = self.inner.lock().unwrap();
        if jar.is_empty() {
            return None;
        }
        let mut pairs: Vec<_> = jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
        pairs.sort();
        Some(pairs.join("; "))
    }
}

pub struct Response {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: String,
}

#[derive(Clone)]
pub struct HttpClient {
    transport: Transport,
    pub jar: CookieJar,
    user_agent: String,
}

impl HttpClient {
    pub fn new(transport: Transport, jar: CookieJar, user_agent: String) -> Self {
        Self { transport, jar, user_agent }
    }

    pub async fn get(&self, url: &str) -> Result<Response> {
        self.send(Method::GET, url, None).await
    }

    /// POST an `application/x-www-form-urlencoded` body.
    pub async fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<Response> {
        let body = encode_form(fields);
        self.send(Method::POST, url, Some(body)).await
    }

    /// Send a request, following up to 5 redirects.
    pub async fn send(&self, method: Method, url: &str, body: Option<String>) -> Result<Response> {
        let mut url = url.to_string();
        let mut method = method;
        let mut body = body;

        for _ in 0..5 {
            let resp = self.send_once(method.clone(), &url, body.clone()).await?;
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
            if !status.is_redirection() {
                return Ok(resp);
            }
            let Some(loc) = resp.headers.get(LOCATION).and_then(|v| v.to_str().ok()) else {
                return Ok(resp);
            };
            url = resolve_url(&url, loc)?;
            // Per RFC 9110, 303 (and in practice 301/302) turn POST into GET.
            if matches!(status, StatusCode::SEE_OTHER | StatusCode::FOUND | StatusCode::MOVED_PERMANENTLY) {
                method = Method::GET;
                body = None;
            }
        }
        bail!("too many redirects for {url}")
    }

    /// Send exactly one request, without following redirects. Useful when
    /// the redirect itself is the thing being read (e.g. a login POST's
    /// 303, which encodes success/failure/2FA in its `Location`).
    pub async fn send_no_redirect(&self, method: Method, url: &str, body: Option<String>) -> Result<Response> {
        self.send_once(method, url, body).await
    }

    /// Fetch raw bytes rather than text - for anything that isn't UTF-8,
    /// like an avatar image. `Response.body` is a `String` and would
    /// corrupt binary data via lossy UTF-8 conversion, so this bypasses it
    /// entirely. Follows redirects the same way `send()` does.
    pub async fn get_bytes(&self, url: &str) -> Result<(u16, Bytes)> {
        let mut url = url.to_string();
        for _ in 0..5 {
            let (status, headers, bytes) = self.send_once_raw(Method::GET, &url, None).await?;
            let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            if !status_code.is_redirection() {
                return Ok((status, bytes));
            }
            let Some(loc) = headers.get(LOCATION).and_then(|v| v.to_str().ok()) else {
                return Ok((status, bytes));
            };
            url = resolve_url(&url, loc)?;
        }
        bail!("too many redirects for {url}")
    }

    async fn send_once(&self, method: Method, url: &str, body: Option<String>) -> Result<Response> {
        let (status, headers, bytes) = self.send_once_raw(method, url, body).await?;
        Ok(Response { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() })
    }

    async fn send_once_raw(&self, method: Method, url: &str, body: Option<String>) -> Result<(u16, hyper::HeaderMap, Bytes)> {
        let content_type = body.is_some().then_some("application/x-www-form-urlencoded");
        self.send_once_raw_bytes(method, url, body.map(Bytes::from), content_type, &[]).await
    }

    /// The actual connect/handshake/send/read path everything else in this
    /// client funnels through - `send_once_raw` (string bodies, always
    /// form-urlencoded), `post_multipart_file` and `post_multipart` (raw
    /// file bytes, a caller-supplied `multipart/form-data; boundary=...`
    /// content type) are all thin wrappers over this. `extra_headers` is
    /// for a target site that needs something beyond this client's normal
    /// browser-like profile - postimg.cc's upload endpoint 403s an
    /// otherwise-identical request missing `Origin`/`Referer`, which
    /// Sneedchat's own site never needs, so it isn't part of the fixed
    /// header set below.
    async fn send_once_raw_bytes(&self, method: Method, url: &str, body: Option<Bytes>, content_type: Option<&str>, extra_headers: &[(&str, &str)]) -> Result<(u16, hyper::HeaderMap, Bytes)> {
        let parsed = url::Url::parse(url).with_context(|| format!("parsing URL {url}"))?;
        let host = parsed.host_str().context("URL has no host")?.to_string();
        let tls = parsed.scheme() == "https";
        let port = parsed.port().unwrap_or(if tls { 443 } else { 80 });

        let stream = self.transport.connect(&host, port, tls).await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.context("HTTP handshake")?;
        // The connection future drives IO; it ends when the response is done.
        let conn_task = tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!("http connection closed: {e}");
            }
        });

        let path = match parsed.query() {
            Some(q) => format!("{}?{}", parsed.path(), q),
            None => parsed.path().to_string(),
        };
        // Header set and order are kept stable and browser-like; the site's
        // anti-bot gate fingerprints clients partly on their header profile.
        let mut req = Request::builder()
            .method(method.clone())
            .uri(&path)
            .header("Host", host_header(&host, port, tls))
            .header("User-Agent", &self.user_agent)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.5")
            .header("Accept-Encoding", "identity")
            .header("Connection", "close");

        if let Some(c) = self.jar.header() {
            req = req.header("Cookie", HeaderValue::from_str(&c)?);
        }
        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }
        for (name, value) in extra_headers {
            req = req.header(*name, HeaderValue::from_str(value)?);
        }

        let req = req.body(Full::new(body.unwrap_or_default())).context("building request")?;

        let resp = sender.send_request(req).await.context("sending request")?;
        self.jar.absorb(&resp);

        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let collected = resp.into_body().collect().await.context("reading response body")?;
        conn_task.abort();

        Ok((status, headers, collected.to_bytes()))
    }

    /// POSTs an arbitrary mix of plain fields and one file as a single
    /// `multipart/form-data` request (postimg.cc's upload needs `gallery`/
    /// `optsize`/`expire`/`numfiles`/`upload_session` alongside the file
    /// itself - see backend/sneedchat/mod.rs's send_attachment).
    /// `extra_headers` threads straight through to send_once_raw_bytes.
    pub async fn post_multipart(&self, url: &str, fields: &[MultipartField<'_>], extra_headers: &[(&str, &str)]) -> Result<(u16, Bytes)> {
        let boundary = format!("nobilis{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());

        let mut body = Vec::new();
        for field in fields {
            match field {
                MultipartField::Text { name, value } => {
                    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
                }
                MultipartField::File { name, file_name, content_type, bytes } => {
                    let safe_name = sanitize_upload_filename(file_name);
                    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{safe_name}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
                    body.extend_from_slice(bytes);
                    body.extend_from_slice(b"\r\n");
                }
            }
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let content_type_header = format!("multipart/form-data; boundary={boundary}");

        let mut url = url.to_string();
        let mut body_opt = Some(Bytes::from(body));
        for _ in 0..5 {
            let (status, headers, resp_bytes) = self.send_once_raw_bytes(Method::POST, &url, body_opt.take(), Some(&content_type_header), extra_headers).await?;
            let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            if !status_code.is_redirection() {
                return Ok((status, resp_bytes));
            }
            let Some(loc) = headers.get(LOCATION).and_then(|v| v.to_str().ok()) else {
                return Ok((status, resp_bytes));
            };
            url = resolve_url(&url, loc)?;
        }
        bail!("too many redirects for {url}")
    }
}

/// One part of a `post_multipart` body.
pub enum MultipartField<'a> {
    Text { name: &'a str, value: &'a str },
    File { name: &'a str, file_name: &'a str, content_type: &'a str, bytes: &'a Bytes },
}

/// Restricted to a safe character set rather than trusted verbatim - this
/// lands inside a raw `Content-Disposition` header value, and while a
/// locally-picked file's own name isn't attacker-controlled in any
/// meaningful sense here, there's no reason to let a stray quote or
/// newline in an unusual filename corrupt the header. Never empty - falls
/// back to a generic name so the multipart part always has *a* filename.
fn sanitize_upload_filename(file_name: &str) -> String {
    let safe: String = file_name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    if safe.is_empty() {
        "upload".to_string()
    } else {
        safe
    }
}

/// Omit the port when it's the default for the scheme, as browsers do.
fn host_header(host: &str, port: u16, tls: bool) -> String {
    let default = if tls { 443 } else { 80 };
    if port == default {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

fn resolve_url(base: &str, loc: &str) -> Result<String> {
    let base = url::Url::parse(base)?;
    Ok(base.join(loc)?.to_string())
}

/// Encode fields as `application/x-www-form-urlencoded`.
pub(super) fn encode_form(fields: &[(&str, &str)]) -> String {
    fields.iter().map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v))).collect::<Vec<_>>().join("&")
}

/// Percent-encode for `application/x-www-form-urlencoded`.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_seeds_and_renders_stable_header() {
        let jar = CookieJar::new();
        jar.set("xf_user", "abc");
        jar.set("xf_session", "def");
        jar.set("xf_csrf", "ghi");
        let h = jar.header().unwrap();
        assert_eq!(h, "xf_csrf=ghi; xf_session=def; xf_user=abc");
        assert_eq!(h, jar.header().unwrap());
        assert_eq!(jar.get("xf_user").as_deref(), Some("abc"));
    }

    #[test]
    fn empty_jar_has_no_header() {
        assert!(CookieJar::new().header().is_none());
    }

    #[test]
    fn host_header_hides_default_ports() {
        assert_eq!(host_header("a.onion", 80, false), "a.onion");
        assert_eq!(host_header("x.st", 443, true), "x.st");
        assert_eq!(host_header("x.st", 9443, true), "x.st:9443");
    }

    #[test]
    fn sanitizes_a_normal_filename_unchanged() {
        assert_eq!(sanitize_upload_filename("photo-2026_08.PNG"), "photo-2026_08.PNG");
    }

    #[test]
    fn strips_quotes_and_whitespace_from_a_filename() {
        assert_eq!(sanitize_upload_filename("my \"cool\" pic.png"), "mycoolpic.png");
    }

    #[test]
    fn falls_back_to_a_generic_name_when_nothing_survives() {
        assert_eq!(sanitize_upload_filename("😀😀😀"), "upload");
        assert_eq!(sanitize_upload_filename(""), "upload");
    }

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(encode_form(&[("a b", "c&d"), ("n", "1+2")]), "a+b=c%26d&n=1%2B2");
    }

    #[test]
    fn redirects_resolve_relative_locations() {
        assert_eq!(resolve_url("http://h.onion/a/b", "/c").unwrap(), "http://h.onion/c");
        assert_eq!(resolve_url("http://h.onion/a/b", "http://o/x").unwrap(), "http://o/x");
    }
}
