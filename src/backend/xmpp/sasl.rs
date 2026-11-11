//! Signing in: the SASL mechanisms themselves, written here because the library's stop at SCRAM-SHA-256 with no channel
//! binding and no way of telling a downgraded list from a real one.
//!
//! - **SCRAM** (RFC 5802, 7677) over SHA-1, SHA-256 and SHA-512, each with its `-PLUS` form bound to the TLS channel
//!   (`tls-exporter`, RFC 9266, on TLS 1.3; `tls-server-end-point`, RFC 5929, otherwise), so a password proof cannot be
//!   relayed through somebody else's TLS connection. The server's final signature is checked: a server that does not know
//!   the password cannot pretend it accepted it.
//! - **Downgrade protection** (XEP-0474): a server that signs the list of mechanisms and channel bindings it offered, inside
//!   SCRAM, lets us see whether somebody in between struck the strong ones out. A mismatch stops the sign-in.
//! - **PLAIN** only when the server offers no SCRAM at all, and never for an account that has already signed in with SCRAM
//!   (it is remembered: see [`Floor`]). Every connection is TLS, so PLAIN is never sent in the clear.
//! - **HT-SHA-256-*** (the token mechanisms FAST uses, XEP-0484): a token the server gave us at the last sign-in, proved by an
//!   HMAC over the channel binding, so the next start needs no password round trip.
//!
//! Nothing here touches the network; [`super::login`] carries the messages.

use base64::Engine;
use hmac012::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The TLS channel's binding data, as the connection measured it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bindings {
    /// `tls-exporter` (RFC 9266): only on TLS 1.3.
    pub exporter: Option<Vec<u8>>,
    /// `tls-server-end-point` (RFC 5929): the server certificate's hash.
    pub endpoint: Option<Vec<u8>>,
}

impl Bindings {
    /// The binding of a type, if this connection has one.
    pub fn get(&self, kind: &str) -> Option<&[u8]> {
        match kind {
            "tls-exporter" => self.exporter.as_deref(),
            "tls-server-end-point" => self.endpoint.as_deref(),
            _ => None,
        }
    }

    /// The type to bind with: the server's list (XEP-0440) when it gave one, else what TLS 1.3 or 1.2 means by default.
    pub fn choose(&self, offered: &[String]) -> Option<&'static str> {
        ["tls-exporter", "tls-server-end-point"].into_iter().find(|&kind| self.get(kind).is_some() && (offered.is_empty() || offered.iter().any(|o| o == kind)))
    }
}

/// The certificate hash `tls-server-end-point` binds to: the certificate's own signature hash, SHA-256 for MD5 and SHA-1
/// (RFC 5929 section 4.1). `None` for a signature with no hash of its own (Ed25519), which has no defined binding.
pub fn endpoint_binding(der: &[u8]) -> Option<Vec<u8>> {
    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der).ok()?;
    let oid = cert.signature_algorithm.algorithm.to_id_string();
    let hash = match oid.as_str() {
        // sha384WithRSAEncryption, ecdsa-with-SHA384
        "1.2.840.113549.1.1.12" | "1.2.840.10045.4.3.3" => sha2::Sha384::digest(der).to_vec(),
        // sha512WithRSAEncryption, ecdsa-with-SHA512
        "1.2.840.113549.1.1.13" | "1.2.840.10045.4.3.4" => Sha512::digest(der).to_vec(),
        // Ed25519, Ed448: no hash to take
        "1.3.101.112" | "1.3.101.113" => return None,
        // MD5, SHA-1, SHA-256 (RSA, ECDSA, RSASSA-PSS with its usual SHA-256)
        _ => Sha256::digest(der).to_vec(),
    };
    Some(hash)
}

/// SCRAM's hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hash {
    Sha1,
    Sha256,
    Sha512,
}

impl Hash {
    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => <sha1_010::Sha1 as Digest>::digest(data).to_vec(),
            Hash::Sha256 => Sha256::digest(data).to_vec(),
            Hash::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        macro_rules! mac {
            ($h:ty) => {{
                let mut m = <Hmac<$h> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }};
        }
        match self {
            Hash::Sha1 => mac!(sha1_010::Sha1),
            Hash::Sha256 => mac!(Sha256),
            Hash::Sha512 => mac!(Sha512),
        }
    }

    /// PBKDF2 with this hash's HMAC, one block: what SCRAM calls Hi().
    fn hi(self, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
        let mut block = salt.to_vec();
        block.extend_from_slice(&1u32.to_be_bytes());
        let mut u = self.hmac(password, &block);
        let mut out = u.clone();
        for _ in 1..iterations {
            u = self.hmac(password, &u);
            for (o, x) in out.iter_mut().zip(&u) {
                *o ^= x;
            }
        }
        out
    }

    fn name(self) -> &'static str {
        match self {
            Hash::Sha1 => "SHA-1",
            Hash::Sha256 => "SHA-256",
            Hash::Sha512 => "SHA-512",
        }
    }
}

/// A mechanism moho can sign in with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mechanism {
    Scram { hash: Hash, plus: bool },
    Plain,
    /// FAST's token mechanism; `binding` is the channel binding type it proves (`None` for `-NONE`).
    Ht { binding: Option<&'static str> },
}

impl Mechanism {
    pub fn name(&self) -> String {
        match self {
            Mechanism::Scram { hash, plus } => format!("SCRAM-{}{}", hash.name(), if *plus { "-PLUS" } else { "" }),
            Mechanism::Plain => "PLAIN".to_string(),
            Mechanism::Ht { binding } => format!(
                "HT-SHA-256-{}",
                match *binding {
                    Some("tls-exporter") => "EXPR",
                    Some("tls-server-end-point") => "ENDP",
                    Some("tls-unique") => "UNIQ",
                    _ => "NONE",
                }
            ),
        }
    }

    pub fn parse(name: &str) -> Option<Mechanism> {
        let scram = |hash, plus| Some(Mechanism::Scram { hash, plus });
        match name {
            "SCRAM-SHA-1" => scram(Hash::Sha1, false),
            "SCRAM-SHA-1-PLUS" => scram(Hash::Sha1, true),
            "SCRAM-SHA-256" => scram(Hash::Sha256, false),
            "SCRAM-SHA-256-PLUS" => scram(Hash::Sha256, true),
            "SCRAM-SHA-512" => scram(Hash::Sha512, false),
            "SCRAM-SHA-512-PLUS" => scram(Hash::Sha512, true),
            "PLAIN" => Some(Mechanism::Plain),
            "HT-SHA-256-NONE" => Some(Mechanism::Ht { binding: None }),
            "HT-SHA-256-EXPR" => Some(Mechanism::Ht { binding: Some("tls-exporter") }),
            "HT-SHA-256-ENDP" => Some(Mechanism::Ht { binding: Some("tls-server-end-point") }),
            _ => None,
        }
    }
}

/// How strong a sign-in this account has had. A server that suddenly offers less than it did before is either
/// misconfigured or somebody in the middle, and is refused either way; setting the password again starts over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Floor {
    None,
    Scram,
}

impl Floor {
    pub fn from_config(s: &str) -> Floor {
        if s == "scram" {
            Floor::Scram
        } else {
            Floor::None
        }
    }

    pub fn as_config(self) -> &'static str {
        match self {
            Floor::None => "",
            Floor::Scram => "scram",
        }
    }

    pub fn of(m: &Mechanism) -> Floor {
        match m {
            // A token says nothing about how the password is protected: it does not raise the floor.
            Mechanism::Plain | Mechanism::Ht { .. } => Floor::None,
            Mechanism::Scram { .. } => Floor::Scram,
        }
    }
}

/// The password mechanism to use from what the server offers: the strongest SCRAM, channel-bound when both sides can,
/// then PLAIN only where nothing better is offered and nothing better was ever used. `Err` says why there is none.
pub fn choose(offered: &[String], bindings: &Bindings, cb_offered: &[String], floor: Floor) -> Result<(Mechanism, Option<&'static str>), String> {
    let has = |n: &str| offered.iter().any(|o| o == n);
    let binding = bindings.choose(cb_offered);
    for hash in [Hash::Sha512, Hash::Sha256, Hash::Sha1] {
        let plus = Mechanism::Scram { hash, plus: true };
        if binding.is_some() && has(&plus.name()) {
            return Ok((plus, binding));
        }
    }
    for hash in [Hash::Sha512, Hash::Sha256, Hash::Sha1] {
        let bare = Mechanism::Scram { hash, plus: false };
        if has(&bare.name()) {
            return Ok((bare, None));
        }
    }
    if has("PLAIN") {
        if floor >= Floor::Scram {
            return Err("The server no longer offers the secure sign-in it used before (only PLAIN), so the password was not sent. If the server really changed, set the password again".to_string());
        }
        return Ok((Mechanism::Plain, None));
    }
    Err(format!("The server offers no sign-in method moho can use (it offers {})", if offered.is_empty() { "none".to_string() } else { offered.join(", ") }))
}

/// The FAST mechanism to use with a token made for `token_mechanism`, if the server still offers it and the binding it needs
/// is there.
pub fn fast_usable(token_mechanism: &str, fast_offered: &[String], bindings: &Bindings) -> Option<Mechanism> {
    if !fast_offered.iter().any(|m| m == token_mechanism) {
        return None;
    }
    let m = Mechanism::parse(token_mechanism)?;
    match &m {
        Mechanism::Ht { binding: Some(kind) } if bindings.get(kind).is_none() => None,
        Mechanism::Ht { .. } => Some(m),
        _ => None,
    }
}

/// The FAST mechanism to ask a token for: the most strongly bound one the server offers that this connection can prove.
pub fn fast_to_request(fast_offered: &[String], bindings: &Bindings) -> Option<Mechanism> {
    ["HT-SHA-256-EXPR", "HT-SHA-256-ENDP", "HT-SHA-256-NONE"].iter().find_map(|n| fast_usable(n, fast_offered, bindings))
}

/// XEP-0474: what the server signs - its mechanisms, and its channel binding types if it listed any, each list sorted.
pub fn downgrade_hash(hash: Hash, mechanisms: &[String], cb_types: Option<&[String]>) -> String {
    let mut m: Vec<&str> = mechanisms.iter().map(String::as_str).collect();
    m.sort_unstable();
    let mut s = m.join("\u{1e}");
    if let Some(cb) = cb_types {
        let mut c: Vec<&str> = cb.iter().map(String::as_str).collect();
        c.sort_unstable();
        s.push('\u{1f}');
        s.push_str(&c.join("\u{1e}"));
    }
    B64.encode(hash.digest(s.as_bytes()))
}

/// One SCRAM exchange, client side.
pub struct Scram {
    hash: Hash,
    gs2: String,
    cb_data: Vec<u8>,
    client_first_bare: String,
    nonce: String,
    password: String,
    /// What a XEP-0474 server must have signed; `None` when the check cannot be made.
    expect_h: Option<String>,
    server_signature: Option<Vec<u8>>,
}

/// SASL's `saslname`: `=` and `,` escaped.
fn saslname(s: &str) -> String {
    s.replace('=', "=3D").replace(',', "=2C")
}

fn random_nonce() -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(b).replace('+', "-").replace('/', "_")
}

impl Scram {
    /// `binding` is the channel binding type for a `-PLUS` mechanism; for a plain one, `client_could_bind` says whether this
    /// connection had a binding the server did not offer to use (`y`, so that a server that does offer `-PLUS` can see the
    /// list was tampered with) or none (`n`).
    pub fn new(hash: Hash, username: &str, password: &str, binding: Option<(&str, &[u8])>, client_could_bind: bool, expect_h: Option<String>) -> Scram {
        Self::with_nonce(hash, username, password, binding, client_could_bind, expect_h, random_nonce())
    }

    fn with_nonce(hash: Hash, username: &str, password: &str, binding: Option<(&str, &[u8])>, client_could_bind: bool, expect_h: Option<String>, nonce: String) -> Scram {
        let (gs2, cb_data) = match binding {
            Some((kind, data)) => (format!("p={kind},,"), data.to_vec()),
            None if client_could_bind => ("y,,".to_string(), vec![]),
            None => ("n,,".to_string(), vec![]),
        };
        let client_first_bare = format!("n={},r={nonce}", saslname(username));
        Scram { hash, gs2, cb_data, client_first_bare, nonce, password: password.to_string(), expect_h, server_signature: None }
    }

    pub fn initial(&self) -> Vec<u8> {
        format!("{}{}", self.gs2, self.client_first_bare).into_bytes()
    }

    /// The answer to the server's first message.
    pub fn respond(&mut self, server_first: &[u8]) -> Result<Vec<u8>, String> {
        let server_first = std::str::from_utf8(server_first).map_err(|_| "the server's SCRAM message is not text".to_string())?;
        let attr = |k: char| server_first.split(',').find_map(|p| p.strip_prefix(&format!("{k}=")).map(str::to_string));
        if let Some(e) = attr('e') {
            return Err(format!("the server refused the sign-in: {e}"));
        }
        let nonce = attr('r').ok_or("the server's SCRAM message has no nonce")?;
        if !nonce.starts_with(&self.nonce) || nonce.len() <= self.nonce.len() {
            return Err("the server's SCRAM nonce does not continue ours".to_string());
        }
        let salt = B64.decode(attr('s').ok_or("the server's SCRAM message has no salt")?).map_err(|_| "the server's SCRAM salt is not base64".to_string())?;
        let iterations: u32 = attr('i').and_then(|i| i.parse().ok()).ok_or("the server's SCRAM message has no iteration count")?;
        if iterations < 4096 && self.hash != Hash::Sha1 || iterations < 1024 {
            return Err(format!("the server asks for only {iterations} SCRAM iterations, too few to protect the password"));
        }
        if iterations > 10_000_000 {
            return Err(format!("the server asks for {iterations} SCRAM iterations, more than any server needs"));
        }
        // XEP-0474: the server signed what it offered. Only a mismatch is refused: most servers do not sign.
        if let (Some(h), Some(expected)) = (attr('h'), &self.expect_h) {
            if &h != expected {
                return Err("The list of sign-in methods was changed on the way from the server (XEP-0474 downgrade protection): somebody may be in the middle of this connection. The password was not sent".to_string());
            }
        }
        let password = stringprep::saslprep(&self.password).map_err(|_| "the password has characters SASL cannot carry".to_string())?;
        let salted = self.hash.hi(password.as_bytes(), &salt, iterations);
        let client_key = self.hash.hmac(&salted, b"Client Key");
        let stored_key = self.hash.digest(&client_key);
        let mut channel = self.gs2.as_bytes().to_vec();
        channel.extend_from_slice(&self.cb_data);
        let without_proof = format!("c={},r={nonce}", B64.encode(channel));
        let auth_message = format!("{},{server_first},{without_proof}", self.client_first_bare);
        let signature = self.hash.hmac(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&signature).map(|(a, b)| a ^ b).collect();
        let server_key = self.hash.hmac(&salted, b"Server Key");
        self.server_signature = Some(self.hash.hmac(&server_key, auth_message.as_bytes()));
        Ok(format!("{without_proof},p={}", B64.encode(proof)).into_bytes())
    }

    /// Checks the server's last message: it proves the server knew the password too.
    pub fn verify(&self, server_final: &[u8]) -> Result<(), String> {
        let text = std::str::from_utf8(server_final).map_err(|_| "the server's last SCRAM message is not text".to_string())?;
        if let Some(e) = text.split(',').find_map(|p| p.strip_prefix("e=")) {
            return Err(format!("the server refused the sign-in: {e}"));
        }
        let v = text.split(',').find_map(|p| p.strip_prefix("v=")).ok_or("the server did not prove it knows the password")?;
        let got = B64.decode(v.trim()).map_err(|_| "the server's proof is not base64".to_string())?;
        match &self.server_signature {
            Some(expected) if *expected == got => Ok(()),
            _ => Err("the server's proof that it knows the password is wrong: this may not be the real server".to_string()),
        }
    }
}

/// PLAIN's single message (RFC 4616).
pub fn plain(username: &str, password: &str) -> Vec<u8> {
    format!("\0{username}\0{password}").into_bytes()
}

/// FAST's token mechanism (draft-schmaus-kitten-sasl-ht): the user name and an HMAC of "Initiator" and the channel
/// binding, keyed with the token.
pub fn ht_initial(username: &str, token: &str, cb_data: &[u8]) -> Vec<u8> {
    let mut msg = b"Initiator".to_vec();
    msg.extend_from_slice(cb_data);
    let mut out = username.as_bytes().to_vec();
    out.push(0);
    out.extend(Hash::Sha256.hmac(token.as_bytes(), &msg));
    out
}

/// What the server proves with in return, if it says anything.
pub fn ht_verify(token: &str, cb_data: &[u8], additional: &[u8]) -> Result<(), String> {
    let mut msg = b"Responder".to_vec();
    msg.extend_from_slice(cb_data);
    if additional.is_empty() || Hash::Sha256.hmac(token.as_bytes(), &msg) == additional {
        Ok(())
    } else {
        Err("the server's answer to the sign-in token is wrong: this may not be the real server".to_string())
    }
}

pub fn b64(data: &[u8]) -> String {
    if data.is_empty() {
        "=".to_string()
    } else {
        B64.encode(data)
    }
}

pub fn unb64(text: &str) -> Result<Vec<u8>, String> {
    let t = text.trim();
    if t.is_empty() || t == "=" {
        return Ok(vec![]);
    }
    B64.decode(t).map_err(|_| "the server sent something that is not base64".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn scram_sha1_matches_rfc5802() {
        let mut c = Scram::with_nonce(Hash::Sha1, "user", "pencil", None, false, None, "fyko+d2lbbFgONRv9qkxdawL".into());
        assert_eq!(c.initial(), b"n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL");
        let r = c.respond(b"r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096").unwrap();
        assert_eq!(String::from_utf8(r).unwrap(), "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=");
        assert!(c.verify(b"v=rmF9pqV8S7suAoZWja4dJRkFsKQ=").is_ok());
        assert!(c.verify(b"v=AAAApqV8S7suAoZWja4dJRkFsKQ=").is_err(), "a server that does not know the password is caught");
    }

    #[test]
    fn scram_sha256_matches_rfc7677() {
        let mut c = Scram::with_nonce(Hash::Sha256, "user", "pencil", None, false, None, "rOprNGfwEbeRWgbNEkqO".into());
        let r = c.respond(b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap();
        assert_eq!(String::from_utf8(r).unwrap(), "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
        assert!(c.verify(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").is_ok());
    }

    #[test]
    fn a_nonce_that_does_not_continue_ours_or_too_few_iterations_is_refused() {
        let mut c = Scram::with_nonce(Hash::Sha256, "user", "pencil", None, false, None, "abc".into());
        assert!(c.respond(b"r=xyz123,s=QSXCR+Q6sek8bf92,i=4096").is_err());
        assert!(c.respond(b"r=abc123,s=QSXCR+Q6sek8bf92,i=100").is_err());
        assert!(c.respond(b"e=other-error").is_err());
    }

    #[test]
    fn channel_binding_goes_into_the_gs2_header_and_the_proof() {
        let c = Scram::with_nonce(Hash::Sha256, "u", "p", Some(("tls-exporter", b"abc")), true, None, "n1".into());
        assert_eq!(c.initial(), b"p=tls-exporter,,n=u,r=n1");
        let mut c = Scram::with_nonce(Hash::Sha256, "u", "p", Some(("tls-exporter", b"abc")), true, None, "n1".into());
        let r = String::from_utf8(c.respond(b"r=n1x,s=QSXCR+Q6sek8bf92,i=4096").unwrap()).unwrap();
        assert!(r.starts_with(&format!("c={},", B64.encode(b"p=tls-exporter,,abc"))), "{r}");
        let y = Scram::with_nonce(Hash::Sha256, "u", "p", None, true, None, "n1".into());
        assert_eq!(y.initial(), b"y,,n=u,r=n1", "a client that could bind says so when the server offered no -PLUS");
    }

    #[test]
    fn xep0474_example_hash_and_a_mismatch_stops() {
        let mechs = s(&["SCRAM-SHA-1", "SCRAM-SHA-1-PLUS"]);
        let cb = s(&["tls-server-end-point", "tls-exporter"]);
        // The hash of 'SCRAM-SHA-1\x1eSCRAM-SHA-1-PLUS\x1ftls-exporter\x1etls-server-end-point', computed here.
        let h = downgrade_hash(Hash::Sha1, &mechs, Some(&cb));
        assert_eq!(h, B64.encode(<sha1_010::Sha1 as Digest>::digest(b"SCRAM-SHA-1\x1eSCRAM-SHA-1-PLUS\x1ftls-exporter\x1etls-server-end-point")));
        assert_eq!(downgrade_hash(Hash::Sha1, &s(&["SCRAM-SHA-1-PLUS", "SCRAM-SHA-1"]), Some(&s(&["tls-exporter", "tls-server-end-point"]))), h, "order does not matter");
        let mut c = Scram::with_nonce(Hash::Sha1, "user", "pencil", None, false, Some(h.clone()), "fyko".into());
        assert!(c.respond(format!("r=fykoX,s=QSXCR+Q6sek8bf92,i=4096,h={h}").as_bytes()).is_ok());
        let mut c = Scram::with_nonce(Hash::Sha1, "user", "pencil", None, false, Some(h), "fyko".into());
        let e = c.respond(b"r=fykoX,s=QSXCR+Q6sek8bf92,i=4096,h=AAAA").unwrap_err();
        assert!(e.contains("downgrade"), "{e}");
    }

    #[test]
    fn the_strongest_mechanism_is_chosen_and_bound_when_possible() {
        let all = s(&["PLAIN", "SCRAM-SHA-1", "SCRAM-SHA-1-PLUS", "SCRAM-SHA-256", "SCRAM-SHA-256-PLUS", "SCRAM-SHA-512", "SCRAM-SHA-512-PLUS"]);
        let tls13 = Bindings { exporter: Some(vec![1]), endpoint: Some(vec![2]) };
        assert_eq!(choose(&all, &tls13, &s(&["tls-exporter"]), Floor::None).unwrap(), (Mechanism::Scram { hash: Hash::Sha512, plus: true }, Some("tls-exporter")));
        assert_eq!(choose(&all, &tls13, &s(&["tls-server-end-point"]), Floor::None).unwrap().1, Some("tls-server-end-point"));
        assert_eq!(choose(&all, &Bindings::default(), &[], Floor::None).unwrap().0, Mechanism::Scram { hash: Hash::Sha512, plus: false });
        assert_eq!(choose(&s(&["PLAIN", "SCRAM-SHA-1"]), &tls13, &[], Floor::None).unwrap().0, Mechanism::Scram { hash: Hash::Sha1, plus: false });
        assert_eq!(choose(&s(&["PLAIN"]), &tls13, &[], Floor::None).unwrap().0, Mechanism::Plain);
        assert!(choose(&s(&["PLAIN"]), &tls13, &[], Floor::Scram).is_err(), "an account that used SCRAM is not downgraded to PLAIN");
        assert!(choose(&s(&["DIGEST-MD5", "X-OAUTH2"]), &tls13, &[], Floor::None).is_err());
    }

    #[test]
    fn fast_mechanisms_need_their_binding() {
        let offered = s(&["HT-SHA-256-NONE", "HT-SHA-256-EXPR", "HT-SHA-256-ENDP"]);
        let tls12 = Bindings { exporter: None, endpoint: Some(vec![2]) };
        assert_eq!(fast_to_request(&offered, &tls12).unwrap().name(), "HT-SHA-256-ENDP");
        assert_eq!(fast_to_request(&offered, &Bindings::default()).unwrap().name(), "HT-SHA-256-NONE");
        assert!(fast_usable("HT-SHA-256-EXPR", &offered, &tls12).is_none());
        assert!(fast_usable("HT-SHA-256-EXPR", &s(&["HT-SHA-256-NONE"]), &Bindings { exporter: Some(vec![1]), endpoint: None }).is_none(), "no longer offered");
        for n in ["SCRAM-SHA-512-PLUS", "PLAIN", "HT-SHA-256-NONE", "HT-SHA-256-EXPR", "HT-SHA-256-ENDP"] {
            assert_eq!(Mechanism::parse(n).unwrap().name(), n);
        }
    }

    #[test]
    fn ht_proves_with_the_token_over_the_binding() {
        let a = ht_initial("alice", "secret", b"");
        assert!(a.starts_with(b"alice\0") && a.len() == 6 + 32);
        assert_ne!(ht_initial("alice", "secret", b"cb"), a);
        let mut responder = b"Responder".to_vec();
        responder.extend_from_slice(b"cb");
        let good = Hash::Sha256.hmac(b"secret", &responder);
        assert!(ht_verify("secret", b"cb", &good).is_ok());
        assert!(ht_verify("secret", b"cb", b"nope").is_err());
    }
}
