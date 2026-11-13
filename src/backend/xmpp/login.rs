//! Signing in, from the stream header to a session the stream library can carry on with.
//!
//! The library's own sign-in knows SCRAM-SHA-256 and -1, PLAIN and ANONYMOUS over RFC 6120 SASL and nothing else, so this is
//! done here on the raw stream ([`super::wire::Wire`]):
//!
//! - **SASL2** (XEP-0388) where the server offers it, with **Bind2** (XEP-0386: the resource bound and carbons turned on in the
//!   same round trip) and **FAST** (XEP-0484: a token asked for once, then used instead of the password, so a reconnection
//!   is one round trip and the password does not cross the wire again). A token the server refuses is dropped and the password
//!   used. When the library holds a session it can resume, nothing is bound inline: the library resumes it after sign-in.
//! - **RFC 6120 SASL** otherwise, then the stream restart the library does itself.
//! - Either way the mechanism is [`super::sasl::choose`]'s: the strongest SCRAM, channel-bound where both sides can, the
//!   server's signed list checked (XEP-0474), PLAIN only when nothing better was ever on offer.
//!
//! What comes out is the library's [`Connection`]: a stream it believes it opened and authenticated itself.

use super::sasl::{self, Bindings, Floor, Mechanism, Scram};
use super::wire::{StreamError, Wire};
use std::borrow::Cow;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_xmpp::jid::{FullJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::stanzastream::Connection;
use tokio_xmpp::xmlstream::{initiate_stream, StreamHeader, Timeouts};

const NS_SASL: &str = "urn:ietf:params:xml:ns:xmpp-sasl";
const NS_SASL2: &str = "urn:xmpp:sasl:2";
const NS_BIND2: &str = "urn:xmpp:bind:0";
const NS_FAST: &str = "urn:xmpp:fast:0";
const NS_SASL_CB: &str = "urn:xmpp:sasl-cb:0";
const NS_SM: &str = "urn:xmpp:sm:3";
const NS_STREAMS: &str = "http://etherx.jabber.org/streams";

/// How long one answer from the server is waited for while signing in.
const STEP: Duration = Duration::from_secs(30);

/// Parses one element as the server wrote it inside the stream (so `stream:` prefixes and the default namespace resolve).
pub fn parse_fragment(xml: &str) -> Option<Element> {
    let wrapped = format!("<w xmlns='jabber:client' xmlns:stream='{NS_STREAMS}'>{xml}</w>");
    let w: Element = wrapped.parse().ok()?;
    w.children().next().cloned()
}

/// Why signing in did not work.
#[derive(Debug)]
pub enum LoginError {
    /// The server does not accept these credentials: stop and ask.
    Credentials(String),
    /// Nothing to do with the moment: the account or the server is set up so that this cannot work.
    Setup(String),
    /// Worth trying again later, said in words.
    Retry(String),
    /// The server ended the stream with an error.
    Stream(StreamError),
    /// The connection or TLS failed (a refused certificate among them).
    Transport(tokio_xmpp::Error),
}

impl From<std::io::Error> for LoginError {
    fn from(e: std::io::Error) -> Self {
        LoginError::Transport(tokio_xmpp::Error::Io(e))
    }
}

/// What the person's account brings to signing in.
pub struct Creds {
    pub password: String,
    pub floor: Floor,
    /// A FAST token and the mechanism it was made for.
    pub fast: Option<(String, String)>,
    pub user_agent_id: String,
    pub way: sasl::Way,
}

/// A FAST token the server gave.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub token: String,
    pub mechanism: String,
    pub expiry: String,
}

/// What signing in came to.
pub struct Outcome {
    pub connection: Connection,
    /// The mechanism that worked.
    pub mechanism: String,
    pub floor: Floor,
    /// A token to keep for the next sign-in.
    pub token: Option<Token>,
    /// The token we had was refused and should be forgotten.
    pub drop_token: bool,
    /// The session was bound in the same round trip (Bind2), with carbons on if the server offered them.
    pub bound_inline: bool,
    pub sasl2: bool,
}

/// The server's words for a SASL failure, sorted into what to do about it.
fn failure(el: &Element) -> LoginError {
    let condition = el.children().find(|c| c.ns() == NS_SASL).map(|c| c.name().to_string()).unwrap_or_default();
    let text = el.children().find(|c| c.name() == "text").map(|c| c.text().trim().to_string()).unwrap_or_default();
    let said = if text.is_empty() { String::new() } else { format!(" (the server says: {text})") };
    match condition.as_str() {
        "not-authorized" => LoginError::Credentials(format!("The server did not accept this password{said}")),
        "credentials-expired" => LoginError::Credentials(format!("The password has expired: set a new one with the server{said}")),
        "account-disabled" => LoginError::Credentials(format!("The server says this account is disabled{said}")),
        "invalid-authzid" => LoginError::Credentials(format!("The server did not accept this account name{said}")),
        "mechanism-too-weak" | "encryption-required" | "invalid-mechanism" => LoginError::Setup(format!("The server will not use any sign-in method moho will offer it over this connection{said}")),
        "temporary-auth-failure" => LoginError::Retry(format!("The server could not check the password just now{said}")),
        other => LoginError::Retry(format!("Signing in failed ({}){said}", if other.is_empty() { "no reason given" } else { other })),
    }
}

/// Reads one top-level element, past whitespace. A stream error or the stream's end is an error.
async fn read_element<S: AsyncRead + Unpin>(wire: &mut Wire<S>) -> Result<(Element, String), LoginError> {
    loop {
        let skip = wire.buffered().iter().take_while(|b| b.is_ascii_whitespace()).count();
        if skip > 0 {
            wire.take(skip);
        }
        let buffered = wire.buffered();
        let valid = match std::str::from_utf8(buffered) {
            Ok(s) => s,
            Err(e) => std::str::from_utf8(&buffered[..e.valid_up_to()]).unwrap_or(""),
        };
        if valid.starts_with("</stream:stream") {
            return Err(LoginError::Retry("The server closed the stream while signing in".to_string()));
        }
        if let Some(end) = super::websocket::element_end(valid) {
            let raw = valid[..end].to_string();
            wire.take(end);
            tracing::trace!("xmpp sign-in RECV {raw}");
            let el = parse_fragment(&raw).ok_or_else(|| LoginError::Retry("The server sent something that is not XML while signing in".to_string()))?;
            if el.is("error", NS_STREAMS) {
                return Err(LoginError::Stream(super::wire::parse_stream_error(&raw)));
            }
            return Ok((el, raw));
        }
        match tokio::time::timeout(STEP, wire.more()).await {
            Err(_) => return Err(LoginError::Retry("The server stopped answering while signing in".to_string())),
            Ok(Err(e)) => return Err(e.into()),
            Ok(Ok(0)) => return Err(LoginError::Retry("The server closed the connection while signing in".to_string())),
            Ok(Ok(_)) => {}
        }
    }
}

/// Reads the server's stream header. Its `id` is returned.
async fn read_header<S: AsyncRead + Unpin>(wire: &mut Wire<S>) -> Result<String, LoginError> {
    loop {
        let text = String::from_utf8_lossy(wire.buffered()).to_string();
        if let Some(at) = text.find("<stream:stream") {
            if let Some(end) = super::websocket::tag_end(&text[at..]) {
                let tag = text[at..at + end].to_string();
                wire.take((at + end).min(wire.buffered().len()));
                let attrs = super::websocket::attributes(&tag);
                return Ok(super::websocket::attr(&attrs, "id").unwrap_or("").to_string());
            }
        }
        match tokio::time::timeout(STEP, wire.more()).await {
            Err(_) => return Err(LoginError::Retry("The server did not open the stream".to_string())),
            Ok(Err(e)) => return Err(e.into()),
            Ok(Ok(0)) => return Err(LoginError::Retry("The server closed the connection before opening the stream".to_string())),
            Ok(Ok(_)) => {}
        }
    }
}

fn header(domain: &str, from: &str) -> String {
    format!(
        "<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='{NS_STREAMS}' to='{}' from='{}' version='1.0' xml:lang='en'>",
        super::xml::escape(domain),
        super::xml::escape(from)
    )
}

async fn send<S: AsyncWrite + Unpin>(wire: &mut Wire<S>, xml: &str) -> Result<(), LoginError> {
    wire.write_all(xml.as_bytes()).await?;
    wire.flush().await?;
    Ok(())
}

/// A mechanism under way.
enum Running {
    Scram(Box<Scram>),
    Plain(Vec<u8>),
    Ht { token: String, cb: Vec<u8> },
}

impl Running {
    fn initial(&self, username: &str) -> Vec<u8> {
        match self {
            Running::Scram(s) => s.initial(),
            Running::Plain(p) => p.clone(),
            Running::Ht { token, cb } => sasl::ht_initial(username, token, cb),
        }
    }

    fn respond(&mut self, challenge: &[u8]) -> Result<Vec<u8>, LoginError> {
        match self {
            Running::Scram(s) => s.respond(challenge).map_err(|e| if e.contains("downgrade") { LoginError::Setup(e) } else { LoginError::Retry(e) }),
            _ => Err(LoginError::Retry("the server asked a question this sign-in method does not have".to_string())),
        }
    }

    fn verify(&self, last: &[u8]) -> Result<(), LoginError> {
        match self {
            Running::Scram(s) => s.verify(last).map_err(LoginError::Setup),
            Running::Plain(_) => Ok(()),
            Running::Ht { token, cb } => sasl::ht_verify(token, cb, last).map_err(LoginError::Setup),
        }
    }
}

/// The text of the stream features' list of mechanisms (RFC 6120 or SASL2 form).
fn mechanisms(el: Option<&Element>) -> Vec<String> {
    el.map(|m| m.children().filter(|c| c.name() == "mechanism").map(|c| c.text().trim().to_string()).collect()).unwrap_or_default()
}

/// Starts the password mechanism chosen for this server.
fn start(m: &Mechanism, binding: Option<&'static str>, username: &str, creds: &Creds, bindings: &Bindings, offered: &[String], cb_list: Option<&[String]>) -> Running {
    match m {
        Mechanism::Scram { hash, plus } => {
            let bound = if *plus { binding.and_then(|k| bindings.get(k).map(|d| (k, d))) } else { None };
            // `y`: this connection could have bound, but the server offered nothing to bind with (so a server that did offer
            // -PLUS can tell its list was cut). When it did offer -PLUS that we did not take, XEP-0474 says `n`.
            let offers_plus = offered.iter().any(|o| o.ends_with("-PLUS"));
            let could = !*plus && !offers_plus && (bindings.exporter.is_some() || bindings.endpoint.is_some());
            let expect = Some(sasl::downgrade_hash(*hash, offered, cb_list));
            Running::Scram(Box::new(Scram::new(*hash, username, &creds.password, bound, could, expect)))
        }
        Mechanism::Plain => Running::Plain(sasl::plain(username, &creds.password)),
        // EXTERNAL with no authorisation identity asks for the one the certificate names (RFC 6120 6.3.8); ANONYMOUS says nothing.
        Mechanism::External | Mechanism::Anonymous => Running::Plain(Vec::new()),
        Mechanism::Ht { .. } => unreachable!("a token mechanism is started from a token"),
    }
}

type Boxed = Box<dyn tokio_xmpp::connect::AsyncReadAndWrite>;

/// Signs in on a fresh connection. `wrap` turns the wire into the stream the library is given (the CSI tap goes around it).
pub async fn login<S>(mut wire: Wire<S>, bindings: Bindings, identity: &Jid, creds: &Creds, resumable: bool, wrap: impl FnOnce(Wire<S>) -> Boxed, timeouts: Timeouts) -> Result<Outcome, LoginError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let domain = identity.domain().as_str().to_string();
    let bare = identity.to_bare().to_string();
    let username = match identity.node() {
        Some(n) => n.as_str().to_string(),
        None if creds.way == sasl::Way::Anonymous => String::new(),
        None => return Err(LoginError::Setup("the address has no user part".to_string())),
    };
    send(&mut wire, &header(&domain, &bare)).await?;
    let stream_id = read_header(&mut wire).await?;
    let (features, _) = read_element(&mut wire).await?;
    if !features.is("features", NS_STREAMS) {
        return Err(LoginError::Retry(format!("the server sent <{}> where its features belong", features.name())));
    }
    let cb_el = super::xml::child(&features, "sasl-channel-binding", NS_SASL_CB);
    let cb_list: Option<Vec<String>> = cb_el.map(|c| c.children().filter_map(|t| t.attr("type").map(str::to_string)).collect());
    let sasl2 = super::xml::child(&features, "authentication", NS_SASL2);
    let sasl1 = super::xml::child(&features, "mechanisms", NS_SASL);
    let offered = mechanisms(sasl2.or(sasl1));
    let (chosen, binding) = sasl::choose(&offered, &bindings, cb_list.as_deref().unwrap_or(&[]), creds.floor, creds.way).map_err(LoginError::Setup)?;
    tracing::info!("xmpp[{bare}]: the server offers {} over {}; moho uses {}{}", offered.join(" "), if sasl2.is_some() { "SASL2" } else { "SASL" }, chosen.name(), binding.map(|b| format!(" bound to {b}")).unwrap_or_default());

    let Some(sasl2) = sasl2.cloned() else {
        // RFC 6120: one mechanism, then the stream restarts and the library takes over at the new header.
        let mut running = start(&chosen, binding, &username, creds, &bindings, &offered, cb_list.as_deref());
        let initial = running.initial(&username);
        send(&mut wire, &format!("<auth xmlns='{NS_SASL}' mechanism='{}'>{}</auth>", chosen.name(), sasl::b64(&initial))).await?;
        loop {
            let (el, _) = read_element(&mut wire).await?;
            match el.name() {
                "challenge" if el.ns() == NS_SASL => {
                    let answer = running.respond(&sasl::unb64(&el.text()).map_err(LoginError::Retry)?)?;
                    send(&mut wire, &format!("<response xmlns='{NS_SASL}'>{}</response>", sasl::b64(&answer))).await?;
                }
                "success" if el.ns() == NS_SASL => {
                    running.verify(&sasl::unb64(&el.text()).map_err(LoginError::Retry)?)?;
                    break;
                }
                "failure" if el.ns() == NS_SASL => return Err(failure(&el)),
                other => return Err(LoginError::Retry(format!("the server sent <{other}> while signing in"))),
            }
        }
        let pending = initiate_stream(wrap(wire), tokio_xmpp::parsers::ns::JABBER_CLIENT, StreamHeader { to: Some(Cow::Owned(domain.clone())), from: Some(Cow::Owned(bare.clone())), id: None }, timeouts).await.map_err(LoginError::from)?;
        let (features, stream) = pending.recv_features().await.map_err(|e| LoginError::Transport(e.into()))?;
        let connection = Connection { stream: stream.box_stream(), features, identity: identity.clone() };
        return Ok(Outcome { connection, mechanism: chosen.name(), floor: Floor::of(&chosen), token: None, drop_token: false, bound_inline: false, sasl2: false });
    };

    // SASL2.
    let inline = super::xml::child(&sasl2, "inline", NS_SASL2);
    let fast_offered = mechanisms(inline.and_then(|i| super::xml::child(i, "fast", NS_FAST)));
    let bind2 = inline.and_then(|i| super::xml::child(i, "bind", NS_BIND2));
    let bind_features: Vec<String> = bind2.and_then(|b| super::xml::child(b, "inline", NS_BIND2)).map(|i| i.children().filter_map(|f| f.attr("var").map(str::to_string)).collect()).unwrap_or_default();
    let sm_inline = inline.and_then(|i| super::xml::child(i, "sm", NS_SM)).is_some();
    let bind_now = bind2.is_some() && !resumable;
    let user_agent = format!("<user-agent id='{}'><software>moho</software></user-agent>", super::xml::escape(&creds.user_agent_id));
    let bind_xml = if bind_now {
        let carbons = if bind_features.iter().any(|f| f == "urn:xmpp:carbons:2") { "<enable xmlns='urn:xmpp:carbons:2'/>" } else { "" };
        format!("<bind xmlns='{NS_BIND2}'><tag>moho</tag>{carbons}</bind>")
    } else {
        String::new()
    };

    let mut drop_token = false;
    let token_mech = creds.fast.as_ref().and_then(|(t, m)| sasl::fast_usable(m, &fast_offered, &bindings).map(|mech| (t.clone(), mech)));
    if creds.fast.is_some() && token_mech.is_none() {
        // A token for a mechanism the server or this connection can no longer use is no good to anyone.
        drop_token = true;
    }
    let mut attempts: Vec<(Mechanism, Running)> = Vec::new();
    if let Some((token, mech)) = token_mech {
        let cb = match &mech {
            Mechanism::Ht { binding: Some(kind) } => bindings.get(kind).unwrap_or_default().to_vec(),
            _ => vec![],
        };
        attempts.push((mech, Running::Ht { token, cb }));
    }
    attempts.push((chosen.clone(), start(&chosen, binding, &username, creds, &bindings, &offered, cb_list.as_deref())));
    // A token is for an account; a session with no account gets none.
    let request = if creds.way == sasl::Way::Anonymous { None } else { sasl::fast_to_request(&fast_offered, &bindings) };

    for (mech, mut running) in attempts {
        let using_token = matches!(mech, Mechanism::Ht { .. });
        let fast_xml = if using_token {
            format!("<fast xmlns='{NS_FAST}'/>")
        } else if let Some(r) = &request {
            format!("<request-token xmlns='{NS_FAST}' mechanism='{}'/>", r.name())
        } else {
            String::new()
        };
        let initial = running.initial(&username);
        send(&mut wire, &format!("<authenticate xmlns='{NS_SASL2}' mechanism='{}'><initial-response>{}</initial-response>{user_agent}{bind_xml}{fast_xml}</authenticate>", mech.name(), sasl::b64(&initial))).await?;
        let success = loop {
            let (el, _) = read_element(&mut wire).await?;
            match el.name() {
                "challenge" if el.ns() == NS_SASL2 => {
                    let answer = running.respond(&sasl::unb64(&el.text()).map_err(LoginError::Retry)?)?;
                    send(&mut wire, &format!("<response xmlns='{NS_SASL2}'>{}</response>", sasl::b64(&answer))).await?;
                }
                "success" if el.ns() == NS_SASL2 => break Some(el),
                "failure" if el.ns() == NS_SASL2 => {
                    if using_token {
                        tracing::info!("xmpp[{bare}]: the server refused the sign-in token; signing in with the password");
                        drop_token = true;
                        break None;
                    }
                    return Err(failure(&el));
                }
                "continue" if el.ns() == NS_SASL2 => {
                    let tasks: Vec<String> = super::xml::child(&el, "tasks", NS_SASL2).map(|t| t.children().map(|c| c.text()).collect()).unwrap_or_default();
                    return Err(LoginError::Setup(format!("The server asks for a further step moho cannot do ({})", tasks.join(", "))));
                }
                other => return Err(LoginError::Retry(format!("the server sent <{other}> while signing in"))),
            }
        };
        let Some(success) = success else { continue };
        let additional = super::xml::child_text(&success, "additional-data", NS_SASL2).unwrap_or_default();
        running.verify(&sasl::unb64(&additional).map_err(LoginError::Retry)?)?;
        let authorized = super::xml::child_text(&success, "authorization-identifier", NS_SASL2).unwrap_or_default();
        let token = super::xml::child(&success, "token", NS_FAST).and_then(|t| {
            let mechanism = if using_token { mech.name() } else { request.as_ref()?.name() };
            Some(Token { token: t.attr("token")?.to_string(), mechanism, expiry: t.attr("expiry").unwrap_or("").to_string() })
        });
        let bound_jid = if bind_now && super::xml::child(&success, "bound", NS_BIND2).is_some() { authorized.trim().parse::<FullJid>().ok() } else { None };
        let virtual_header = format!("<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='{NS_STREAMS}' from='{}' id='{}' version='1.0'>", super::xml::escape(&domain), super::xml::escape(&stream_id));
        // XEP-0388: success is followed by the authenticated stream's features, with no restart - Bind2 or not. A server that
        // leaves them out is waited for a moment and then made up for; anything else it sends instead is passed on after them.
        let mut after = String::new();
        let real = match tokio::time::timeout(Duration::from_secs(5), read_element(&mut wire)).await {
            Ok(Ok((el, raw))) if el.is("features", NS_STREAMS) => Some((el, raw)),
            Ok(Ok((_, raw))) => {
                after = raw;
                None
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => None,
        };
        let features = match (&bound_jid, real) {
            (None, Some((_, raw))) => raw,
            (None, None) => return Err(LoginError::Retry("the server did not say what the signed-in stream offers".to_string())),
            (Some(_), real) => {
                // Bound already: binding is offered to the library (and answered by the wire, not the server), with what the
                // server says the stream offers - or, if it said nothing, what Bind2 listed: stream management and client
                // state indication.
                let bind = "<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/>";
                match real {
                    Some((_, raw)) => with_bind(&raw, bind),
                    None => {
                        let sm = if sm_inline || bind_features.iter().any(|f| f == NS_SM) { format!("<sm xmlns='{NS_SM}'/>") } else { String::new() };
                        let csi = if bind_features.iter().any(|f| f == "urn:xmpp:csi:0") { "<csi xmlns='urn:xmpp:csi:0'/>" } else { "" };
                        format!("<stream:features>{bind}{sm}{csi}</stream:features>")
                    }
                }
            }
        };
        let features = format!("{features}{after}");
        let mut played = virtual_header.into_bytes();
        played.extend_from_slice(features.as_bytes());
        wire.hand_over(&played, bound_jid.as_ref().map(|j| j.to_string()));
        let pending = initiate_stream(wrap(wire), tokio_xmpp::parsers::ns::JABBER_CLIENT, StreamHeader { to: Some(Cow::Owned(domain.clone())), from: Some(Cow::Owned(bare.clone())), id: None }, timeouts).await.map_err(LoginError::from)?;
        let (features, stream) = pending.recv_features().await.map_err(|e| LoginError::Transport(e.into()))?;
        let identity = match &bound_jid {
            Some(j) => Jid::from(j.clone()),
            None => identity.clone(),
        };
        let connection = Connection { stream: stream.box_stream(), features, identity };
        return Ok(Outcome { connection, mechanism: mech.name(), floor: Floor::of(&mech), token, drop_token, bound_inline: bound_jid.is_some(), sasl2: true });
    }
    Err(LoginError::Retry("signing in ended without an answer".to_string()))
}

impl From<tokio_xmpp::Error> for LoginError {
    fn from(e: tokio_xmpp::Error) -> Self {
        LoginError::Transport(e)
    }
}

/// A stream error, in words, and whether it is worth trying again.
pub fn stream_error_words(e: &StreamError) -> (String, bool) {
    let said = if e.text.is_empty() { String::new() } else { format!(" (the server says: {})", e.text) };
    let (words, retry) = match e.condition.as_str() {
        "conflict" => ("Another device signed in with the same resource", true),
        "system-shutdown" => ("The server is shutting down", true),
        "reset" => ("The server reset the connection", true),
        "connection-timeout" => ("The server ended an idle connection", true),
        "policy-violation" => ("The server says this account broke its rules (often: too much, too fast)", true),
        "resource-constraint" => ("The server is too busy", true),
        "internal-server-error" | "remote-connection-failed" | "undefined-condition" => ("The server had a problem", true),
        "see-other-host" => ("The server sent this account to another host", true),
        "host-unknown" => ("The server does not serve this address's domain", false),
        "host-gone" => ("The server no longer serves this address's domain", false),
        "not-authorized" => ("The server refused this account", false),
        "improper-addressing" | "invalid-from" => ("The server did not accept the address this account signs in as", false),
        "unsupported-version" | "unsupported-encoding" | "invalid-namespace" | "bad-format" | "not-well-formed" | "invalid-xml" | "restricted-xml" | "unsupported-stanza-type" | "unsupported-feature" | "bad-namespace-prefix" => {
            ("The server and moho could not understand each other", true)
        }
        _ => ("The server ended the connection", true),
    };
    (format!("{words}{said}"), retry)
}

/// Features as the server wrote them, with a binding offer added: `<stream:features/>` or `...</stream:features>`.
fn with_bind(raw: &str, bind: &str) -> String {
    let t = raw.trim_end();
    if let Some(head) = t.strip_suffix("</stream:features>") {
        return format!("{head}{bind}</stream:features>");
    }
    if let Some(head) = t.strip_suffix("/>") {
        return format!("{head}>{bind}</stream:features>");
    }
    format!("<stream:features>{bind}</stream:features>")
}

/// A host named by `see-other-host`: `host`, `host:port` or `[v6]:port`.
pub fn other_host(value: &str) -> Option<(String, u16)> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(rest) = v.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(0);
        return Some((host.to_string(), port));
    }
    match v.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((v.to_string(), 0)),
    }
}

/// A fresh random UUID (version 4), for the user agent's id.
pub fn new_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fragment_with_the_stream_prefix_parses() {
        let el = parse_fragment("<stream:features><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>PLAIN</mechanism></mechanisms></stream:features>").unwrap();
        assert!(el.is("features", NS_STREAMS));
        assert_eq!(mechanisms(super::super::xml::child(&el, "mechanisms", NS_SASL)), vec!["PLAIN".to_string()]);
    }

    #[test]
    fn a_stream_error_is_read_and_said() {
        let e = super::super::wire::parse_stream_error("<stream:error><see-other-host xmlns='urn:ietf:params:xml:ns:xmpp-streams'>[2001:db8::1]:9222</see-other-host><text xmlns='urn:ietf:params:xml:ns:xmpp-streams'>moving</text></stream:error>");
        assert_eq!((e.condition.as_str(), e.text.as_str(), e.other_host.as_str()), ("see-other-host", "moving", "[2001:db8::1]:9222"));
        assert_eq!(other_host(&e.other_host), Some(("2001:db8::1".to_string(), 9222)));
        assert_eq!(other_host("xmpp.example.org"), Some(("xmpp.example.org".to_string(), 0)));
        assert_eq!(other_host("xmpp.example.org:5223"), Some(("xmpp.example.org".to_string(), 5223)));
        let (words, retry) = stream_error_words(&StreamError { condition: "conflict".into(), text: String::new(), other_host: String::new() });
        assert!(words.contains("Another device") && retry);
        assert!(!stream_error_words(&StreamError { condition: "host-unknown".into(), text: String::new(), other_host: String::new() }).1);
    }

    #[test]
    fn sasl_failures_are_sorted_into_what_to_do() {
        let f = |c: &str| failure(&parse_fragment(&format!("<failure xmlns='urn:xmpp:sasl:2'><{c} xmlns='{NS_SASL}'/><text>why</text></failure>")).unwrap());
        assert!(matches!(f("not-authorized"), LoginError::Credentials(m) if m.contains("why")));
        assert!(matches!(f("temporary-auth-failure"), LoginError::Retry(_)));
        assert!(matches!(f("mechanism-too-weak"), LoginError::Setup(_)));
    }

    #[test]
    fn binding_is_added_to_features_however_they_were_written() {
        let b = "<bind/>";
        assert_eq!(with_bind("<stream:features><sm xmlns='urn:xmpp:sm:3'/></stream:features>", b), "<stream:features><sm xmlns='urn:xmpp:sm:3'/><bind/></stream:features>");
        assert_eq!(with_bind("<stream:features/>", b), "<stream:features><bind/></stream:features>");
    }

    #[test]
    fn a_uuid_is_version_four() {
        let u = new_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(["8", "9", "a", "b"].contains(&&u[19..20]));
        assert_ne!(u, new_uuid());
    }
}
