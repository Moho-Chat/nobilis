//! How an XMPP account reaches its server: where the connection goes, how the server is trusted, and the TLS around it.
//!
//! The library's own connectors are not used, for three reasons that all come from this being a client people point at
//! servers of their own: they trust the system's roots and nothing else (a server with its own CA, or a self-signed
//! certificate, could never be used), they resolve names themselves (so an account on Tor would leak its lookups, and
//! could not reach an onion server at all), and they say nothing about why a certificate was refused.
//!
//! What is here instead is [`Connector`], which the library takes as its `ServerConnector`:
//!
//! - the TCP connection comes from `net::route`, so an account routed over Tor or SOCKS5 is, and its lookups are the
//!   proxy's;
//! - DNS SRV is asked (`_xmpp-client._tcp`, or `_xmpps-client._tcp` for direct TLS) only when the connection is direct
//!   and the account has not named a host;
//! - the certificate is checked against the system's roots, plus the CAs the account lists, or is accepted when it is
//!   exactly the one the person pinned - and what the server showed, if it was refused, is kept so the window can offer
//!   it to be pinned (see [`Seen`]);
//! - STARTTLS (RFC 6120) or direct TLS (XEP-0368, ALPN `xmpp-client`), TLS 1.2 or later, and nothing else: there is no
//!   plaintext mode and no switch that accepts any certificate.

use crate::net::route::TcpOrSocks;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::io;
use std::sync::{Arc, Mutex};
use tokio::io::BufStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tokio_xmpp::connect::{DnsConfig, ServerConnector};
use tokio_xmpp::jid::Jid;
use tokio_xmpp::parsers::starttls::{self, Request};
use tokio_xmpp::xmlstream::{initiate_stream, PendingFeaturesRecv, ReadError, StreamHeader, Timeouts, XmppStream, XmppStreamElement};
use tokio_xmpp::Error;

/// The ports a connection uses when neither DNS nor the account says.
pub const PORT_STARTTLS: u16 = 5222;
pub const PORT_DIRECT: u16 = 5223;

/// What the server showed for its certificate on the last attempt, kept whether it was accepted or not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    /// SHA-256 of the certificate (DER), as lower-case hex: what a pin is.
    pub sha256: String,
    /// Why it was refused, in the TLS library's words; empty when it was accepted.
    pub refused: String,
    /// The certificate itself (DER), for showing its details.
    pub der: Vec<u8>,
}

/// Where the last certificate seen is kept, shared between a connector and whoever reports on it.
pub type SeenSlot = Arc<Mutex<Option<Seen>>>;

/// The certificate each stopped account was refused by, so that the window can show it and the person can pin it. Only
/// what the server showed is ever pinned from here: a fingerprint the window sends is compared with this one.
fn refused() -> &'static Mutex<std::collections::HashMap<String, Seen>> {
    static REFUSED: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Seen>>> = std::sync::OnceLock::new();
    REFUSED.get_or_init(Default::default)
}

pub fn remember_refused(account_id: &str, seen: Seen) {
    refused().lock().unwrap().insert(account_id.to_string(), seen);
}

pub fn forget_refused(account_id: &str) {
    refused().lock().unwrap().remove(account_id);
}

pub fn refused_by(account_id: &str) -> Option<Seen> {
    refused().lock().unwrap().get(account_id).cloned()
}

/// The transport settings of one account.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The key `net::route` knows the account by.
    pub account_id: String,
    /// A host to connect to instead of asking DNS; empty to ask.
    pub host: String,
    /// A port to use instead of the default; 0 for the default.
    pub port: u16,
    /// TLS from the first byte (XEP-0368) rather than STARTTLS.
    pub direct: bool,
    /// Extra trusted certificates, PEM.
    pub ca_pem: String,
    /// One certificate trusted whatever signed it (SHA-256, hex); empty for none.
    pub pin_sha256: String,
}

pub fn sha256_hex(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect()
}

/// A pin as typed or pasted: hex, with or without colons or spaces, any case. `None` when it is not 32 bytes of hex.
pub fn normalise_pin(pin: &str) -> Option<String> {
    let hex: String = pin.chars().filter(|c| !c.is_whitespace() && *c != ':').collect::<String>().to_ascii_lowercase();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// The system's roots, or the bundled ones when the system has none (a container, a minimal install).
fn base_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(native.certs);
    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    roots
}

/// Checks a certificate as the account asks: the pin if it matches, otherwise the chain against the roots. What was
/// shown is written down either way.
#[derive(Debug)]
struct Trust {
    chain: Arc<WebPkiServerVerifier>,
    pin: Option<String>,
    seen: SeenSlot,
}

impl ServerCertVerifier for Trust {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], server_name: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, TlsError> {
        let sha256 = sha256_hex(end_entity);
        let pinned = self.pin.as_deref() == Some(sha256.as_str());
        let verdict = if pinned { Ok(ServerCertVerified::assertion()) } else { self.chain.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) };
        let refused = verdict.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
        *self.seen.lock().unwrap() = Some(Seen { sha256, refused, der: end_entity.as_ref().to_vec() });
        verdict
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.chain.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.chain.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.chain.supported_verify_schemes()
    }
}

/// The TLS configuration for an account.
pub fn tls_config(settings: &Settings, seen: &SeenSlot) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = base_roots();
    if !settings.ca_pem.trim().is_empty() {
        let mut added = 0;
        for cert in CertificateDer::pem_slice_iter(settings.ca_pem.as_bytes()) {
            let cert = cert.map_err(|e| anyhow!("the certificate authority file for this account is not PEM: {e}"))?;
            roots.add(cert).map_err(|e| anyhow!("a certificate authority listed for this account is not usable: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err(anyhow!("the certificate authority text for this account holds no certificate"));
        }
    }
    let chain = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build().context("building the certificate checker")?;
    let pin = match settings.pin_sha256.trim() {
        "" => None,
        pin => Some(normalise_pin(pin).ok_or_else(|| anyhow!("the pinned certificate is not a SHA-256 fingerprint"))?),
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Trust { chain, pin, seen: seen.clone() }))
        .with_no_client_auth();
    if settings.direct {
        // XEP-0368: how a server that shares its port with something else knows this is XMPP.
        config.alpn_protocols = vec![b"xmpp-client".to_vec()];
    }
    Ok(Arc::new(config))
}

/// The connector the library takes: made again for each connection, from the account as it is.
#[derive(Clone)]
pub struct Connector {
    pub settings: Settings,
    tls: Arc<ClientConfig>,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connector").field("settings", &self.settings).finish()
    }
}

impl Connector {
    pub fn new(settings: Settings, seen: &SeenSlot) -> Result<Self> {
        let tls = tls_config(&settings, seen)?;
        Ok(Self { settings, tls })
    }

    /// The TCP connection, through the account's route.
    async fn tcp(&self, domain: &str) -> Result<TcpOrSocks, Error> {
        let s = &self.settings;
        let default_port = if s.direct { PORT_DIRECT } else { PORT_STARTTLS };
        let router = crate::net::route::router();
        if !s.host.is_empty() || router.routed(&s.account_id) {
            // Named, or routed: the proxy resolves the name (an onion address cannot be resolved at all), and SRV is not
            // asked, since asking would be a lookup that does not go through the proxy.
            let host = if s.host.is_empty() { domain } else { s.host.as_str() };
            let port = if s.port != 0 { s.port } else { default_port };
            return router.connect(&s.account_id, host, port).await.map_err(|e| Error::Io(io::Error::other(format!("{e:#}"))));
        }
        let dns = if s.direct { DnsConfig::srv_xmpps(domain) } else { DnsConfig::srv_default_client(domain) };
        Ok(TcpOrSocks::Tcp(dns.resolve().await?))
    }

    async fn tls<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(&self, stream: S, domain: &str) -> Result<(TlsStream<S>, sasl::common::ChannelBinding), Error> {
        let name = ServerName::try_from(domain.to_owned()).map_err(|e| Error::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
        let tls = TlsConnector::from(self.tls.clone()).connect(name, stream).await.map_err(Error::Io)?;
        // The binding for SCRAM-*-PLUS, which TLS 1.3 gives (RFC 9266). TLS 1.2's tls-unique is not offered.
        let binding = match tls.get_ref().1.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => match tls.get_ref().1.export_keying_material(vec![0u8; 32], b"EXPORTER-Channel-Binding", None) {
                Ok(data) => sasl::common::ChannelBinding::TlsExporter(data),
                Err(_) => sasl::common::ChannelBinding::None,
            },
            _ => sasl::common::ChannelBinding::None,
        };
        Ok((tls, binding))
    }
}

type Stream = BufStream<TlsStream<TcpOrSocks>>;

impl ServerConnector for Connector {
    type Stream = Stream;

    async fn connect(&self, jid: &Jid, ns: &'static str, timeouts: Timeouts) -> Result<(PendingFeaturesRecv<Self::Stream>, sasl::common::ChannelBinding), Error> {
        let domain = jid.domain().as_str();
        let tcp = self.tcp(domain).await?;
        if self.settings.direct {
            let (tls, binding) = self.tls(tcp, domain).await?;
            let pending = initiate_stream(BufStream::new(tls), ns, StreamHeader { to: Some(Cow::Borrowed(domain)), from: Some(Cow::Borrowed(jid.to_bare().as_str())), id: None }, timeouts).await?;
            return Ok((pending, binding));
        }
        // STARTTLS: open the stream in the clear only to be offered TLS and to ask for it; nothing is sent that a
        // listener could use (no credentials are, until TLS is up), and a server that does not offer it is refused.
        let plain = initiate_stream(BufStream::new(tcp), ns, StreamHeader { to: Some(Cow::Borrowed(domain)), from: None, id: None }, timeouts).await?;
        let (features, mut plain): (_, XmppStream<_>) = plain.recv_features().await?;
        if !features.can_starttls() {
            return Err(Error::Io(io::Error::other("the server does not offer STARTTLS, and moho does not connect without encryption")));
        }
        plain.send(&XmppStreamElement::Starttls(starttls::Nonza::Request(Request))).await?;
        loop {
            match plain.next().await.map(|v| v.and_then(|v| v.into_read_error())) {
                Some(Ok(XmppStreamElement::Starttls(starttls::Nonza::Proceed(_)))) => break,
                Some(Ok(XmppStreamElement::Starttls(starttls::Nonza::Failure(_)))) => return Err(Error::Io(io::Error::other("the server refused STARTTLS"))),
                Some(Ok(_)) | Some(Err(ReadError::SoftTimeout)) => {}
                Some(Err(ReadError::HardError(e))) => return Err(e.into()),
                Some(Err(ReadError::ParseError(e))) => return Err(Error::Io(io::Error::new(io::ErrorKind::InvalidData, e))),
                None | Some(Err(ReadError::StreamFooterReceived)) => return Err(Error::Disconnected),
            }
        }
        let raw = plain.into_inner().into_inner();
        let (tls, binding) = self.tls(raw, domain).await?;
        let pending = initiate_stream(BufStream::new(tls), ns, StreamHeader { to: Some(Cow::Borrowed(domain)), from: None, id: None }, timeouts).await?;
        Ok((pending, binding))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_hex_whatever_way_it_was_written() {
        let hex = "ab".repeat(32);
        assert_eq!(normalise_pin(&hex).as_deref(), Some(hex.as_str()));
        let colons = (0..32).map(|_| "AB").collect::<Vec<_>>().join(":");
        assert_eq!(normalise_pin(&colons).as_deref(), Some(hex.as_str()));
        assert_eq!(normalise_pin(&format!("  {}  ", hex.to_uppercase())).as_deref(), Some(hex.as_str()));
        assert_eq!(normalise_pin("abcd"), None);
        assert_eq!(normalise_pin(&"zz".repeat(32)), None);
    }

    #[test]
    fn a_certificate_has_the_fingerprint_a_pin_is_compared_with() {
        // SHA-256 of the empty string, as any tool prints it.
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    fn settings(ca: &str, pin: &str) -> Settings {
        Settings { account_id: "xmpp:a@example.org".into(), host: String::new(), port: 0, direct: false, ca_pem: ca.into(), pin_sha256: pin.into() }
    }

    #[test]
    fn a_config_is_made_with_nothing_extra_and_refuses_a_bad_pin_or_a_file_that_is_not_a_certificate() {
        let seen: SeenSlot = Default::default();
        assert!(tls_config(&settings("", ""), &seen).is_ok());
        assert!(tls_config(&settings("", &"cd".repeat(32)), &seen).is_ok());
        assert!(tls_config(&settings("", "not a fingerprint"), &seen).is_err());
        assert!(tls_config(&settings("this is not PEM", ""), &seen).is_err());
        assert!(tls_config(&settings("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n", ""), &seen).is_err());
    }

    #[test]
    fn direct_tls_says_xmpp_client_in_alpn_and_starttls_says_nothing() {
        let seen: SeenSlot = Default::default();
        let direct = Settings { direct: true, ..settings("", "") };
        assert_eq!(tls_config(&direct, &seen).unwrap().alpn_protocols, vec![b"xmpp-client".to_vec()]);
        assert!(tls_config(&settings("", ""), &seen).unwrap().alpn_protocols.is_empty());
    }
}
