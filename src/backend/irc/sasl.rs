//! SASL mechanisms for IRC, beyond the PLAIN this started with.
//!
//! Three, in the order a connection should prefer them:
//!
//! - **EXTERNAL** proves who you are with the TLS client certificate already
//!   on the connection, so no password is sent at all. Nothing to intercept
//!   and nothing to reuse elsewhere; where a network supports it, it is the
//!   best answer available.
//! - **SCRAM-SHA-256** proves knowledge of the password without sending it,
//!   and proves the server knew it too - so a server that has been replaced
//!   cannot silently collect passwords.
//! - **PLAIN** sends the password with base64 wrapped round it. An encoding,
//!   not a cipher. Fine inside TLS, which is why it is still here, and
//!   refused outside it.
//!
//! HMAC is implemented here rather than taken from the `hmac` crate: this
//! project's `hmac` is 0.13, which pairs with digest 0.11 hashes, while its
//! `sha2` is 0.10, which needs 0.12. Rather than move a dependency the rest of
//! the daemon is built on, HMAC-SHA-256 is twenty lines and is checked against
//! RFC 4231's vectors; PBKDF2 on top of it is a loop, checked against
//! RFC 7677's.

use super::*;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use sha2::{Digest, Sha256};

const BLOCK: usize = 64;
const HASH: usize = 32;

/// HMAC-SHA-256, per RFC 2104.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; HASH] {
    let mut block = [0u8; BLOCK];
    // A key longer than the block is hashed first; a shorter one is padded
    // with zeros, which `block` already is.
    if key.len() > BLOCK {
        block[..HASH].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        inner_pad[i] ^= block[i];
        outer_pad[i] ^= block[i];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

/// PBKDF2-HMAC-SHA-256 producing one block, which is all SCRAM-SHA-256 wants.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; HASH] {
    let mut salted = Vec::with_capacity(salt.len() + 4);
    salted.extend_from_slice(salt);
    // The block index, which is always 1 for a single-block output.
    salted.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password, &salted);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (o, byte) in out.iter_mut().zip(u.iter()) {
            *o ^= byte;
        }
    }
    out
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| anyhow!("server sent something that is not base64: {e}"))
}

/// A SCRAM-SHA-256 exchange, from the client's side.
///
/// Held as a value rather than run as one function because the exchange is
/// three messages with the server answering in between, and the state that
/// spans them - the nonce we chose and the first message we sent - is exactly
/// what the final signature is computed over. Losing track of either is how a
/// SCRAM implementation ends up "working" while verifying nothing.
pub struct Scram {
    password: String,
    client_first_bare: String,
    /// Kept from the server's first message so the final check can use it.
    salted_password: Option<[u8; HASH]>,
    auth_message: Option<String>,
}

impl Scram {
    /// `nonce` is the client's, and must be fresh per exchange - it is what
    /// stops a recorded exchange being replayed at us.
    pub fn new(username: &str, password: &str, nonce: &str) -> Self {
        // The username is escaped, not rejected: "," and "=" are the message's
        // own separators, and RFC 5802 gives them names rather than banning
        // them from names.
        let user = username.replace('=', "=3D").replace(',', "=2C");
        Self {
            password: password.to_string(),
            client_first_bare: format!("n={user},r={nonce}"),
            salted_password: None,
            auth_message: None,
        }
    }

    /// The first message: `n,,n=<user>,r=<nonce>`.
    ///
    /// The leading `n,,` says no channel binding, which is what a client that
    /// does not do channel binding must say - claiming otherwise is how a
    /// downgrade goes unnoticed.
    pub fn client_first(&self) -> String {
        format!("n,,{}", self.client_first_bare)
    }

    /// Answers the server's first message with the proof.
    pub fn client_final(&mut self, server_first: &str) -> Result<String> {
        let (mut nonce, mut salt, mut iterations) = (None, None, None);
        for field in server_first.split(',') {
            match field.split_once('=') {
                Some(("r", v)) => nonce = Some(v.to_string()),
                Some(("s", v)) => salt = Some(unb64(v)?),
                Some(("i", v)) => iterations = v.parse::<u32>().ok(),
                // `e=` is the server refusing, and says why.
                Some(("e", v)) => bail!("server refused SASL: {v}"),
                _ => {}
            }
        }
        let nonce = nonce.ok_or_else(|| anyhow!("server's SCRAM reply carried no nonce"))?;
        let salt = salt.ok_or_else(|| anyhow!("server's SCRAM reply carried no salt"))?;
        let iterations = iterations.ok_or_else(|| anyhow!("server's SCRAM reply carried no iteration count"))?;
        // The server chooses this, and a server choosing 1 would be asking for
        // a password that is barely stretched at all.
        if iterations < 4096 {
            bail!("server asked for {iterations} SCRAM iterations, which is too few to be safe");
        }
        // Our nonce must be a prefix of the combined one, or the server is not
        // answering the message we sent.
        let ours = self.client_first_bare.rsplit_once("r=").map(|(_, n)| n).unwrap_or_default();
        if !nonce.starts_with(ours) {
            bail!("server's SCRAM nonce does not extend ours");
        }

        // `c=biws` is base64("n,,") - the same no-channel-binding claim, sent
        // back so the server can see it was not tampered with in transit.
        let client_final_bare = format!("c=biws,r={nonce}");
        let salted = pbkdf2_sha256(self.password.as_bytes(), &salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key: [u8; HASH] = Sha256::digest(client_key).into();
        let auth_message = format!("{},{server_first},{client_final_bare}", self.client_first_bare);
        let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());

        let mut proof = client_key;
        for (p, sig) in proof.iter_mut().zip(client_signature.iter()) {
            *p ^= sig;
        }

        self.salted_password = Some(salted);
        self.auth_message = Some(auth_message);
        Ok(format!("{client_final_bare},p={}", b64(&proof)))
    }

    /// Checks the server's final message.
    ///
    /// Not optional politeness: this is the half of SCRAM that proves the
    /// server also knew the password. Skipping it leaves the exchange no
    /// better than PLAIN against a server that has been replaced.
    pub fn verify(&self, server_final: &str) -> Result<()> {
        let salted = self.salted_password.ok_or_else(|| anyhow!("SCRAM finished out of order"))?;
        let auth_message = self.auth_message.as_ref().ok_or_else(|| anyhow!("SCRAM finished out of order"))?;

        for field in server_final.split(',') {
            match field.split_once('=') {
                Some(("e", v)) => bail!("server refused SASL: {v}"),
                Some(("v", v)) => {
                    let server_key = hmac_sha256(&salted, b"Server Key");
                    let expected = hmac_sha256(&server_key, auth_message.as_bytes());
                    return if unb64(v)? == expected {
                        Ok(())
                    } else {
                        // Worth being blunt about: the password was never
                        // sent, so this is not a failed login. It is a server
                        // that cannot prove it is the one you registered with.
                        bail!("the server could not prove it knows this account's password - do not trust this connection")
                    };
                }
                _ => {}
            }
        }
        bail!("server's final SCRAM message carried no signature")
    }
}

/// A nonce for one exchange: printable, comma-free, and never reused.
pub fn nonce() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..24).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 1.
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2 - a key shorter than the block.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6 - a key longer than the block, which is hashed first.
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// RFC 7677's worked example, end to end.
    #[test]
    fn scram_matches_rfc_7677() {
        let mut scram = Scram::new("user", "pencil", "rOprNGfwEbeRWgbNEkqO");
        assert_eq!(scram.client_first(), "n,,n=user,r=rOprNGfwEbeRWgbNEkqO");

        let server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let client_final = scram.client_final(server_first).unwrap();
        assert_eq!(
            client_final,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );

        scram.verify("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap();
    }

    #[test]
    fn a_server_that_cannot_prove_itself_is_refused() {
        let mut scram = Scram::new("user", "pencil", "rOprNGfwEbeRWgbNEkqO");
        scram
            .client_final("r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096")
            .unwrap();
        // One byte different from the real signature.
        let err = scram.verify("v=7rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap_err();
        assert!(err.to_string().contains("could not prove"), "{err}");
    }

    #[test]
    fn refuses_a_nonce_that_is_not_ours_extended() {
        let mut scram = Scram::new("user", "pencil", "ourOwnNonce");
        let err = scram.client_final("r=somebodyElsesNonce,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap_err();
        assert!(err.to_string().contains("does not extend ours"), "{err}");
    }

    #[test]
    fn refuses_an_iteration_count_that_stretches_nothing() {
        let mut scram = Scram::new("user", "pencil", "n");
        let err = scram.client_final("r=n2,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=1").unwrap_err();
        assert!(err.to_string().contains("too few"), "{err}");
    }

    #[test]
    fn passes_the_servers_own_refusal_through() {
        let mut scram = Scram::new("user", "pencil", "n");
        let err = scram.client_final("e=unknown-user").unwrap_err();
        assert!(err.to_string().contains("unknown-user"), "{err}");
    }

    #[test]
    fn escapes_the_separators_a_username_may_contain() {
        let scram = Scram::new("a,b=c", "pw", "nonce");
        assert_eq!(scram.client_first(), "n,,n=a=2Cb=3Dc,r=nonce");
    }

    #[test]
    fn a_nonce_is_fresh_and_safe_to_put_in_a_message() {
        let a = nonce();
        let b = nonce();
        assert_ne!(a, b);
        assert!(!a.contains(','), "a comma would end the field early");
        assert_eq!(a.len(), 24);
    }
}

/// Registers with SASL, saying whether the account actually authenticated.
///
/// `false` means the server does not offer SASL and registration finished the
/// ordinary way instead. It is not a failure: plenty of small networks have
/// never implemented it, and refusing to connect to one because a checkbox was
/// ticked would be worse than connecting the way every other client does. The
/// caller uses the answer to decide whether NickServ still has a job to do.
///
/// A server that *does* offer SASL and then rejects the credentials is a real
/// error and stays one. That is a wrong password, and quietly carrying on
/// unauthenticated is how somebody ends up sitting in a channel under an
/// unregistered nick believing they are identified.
pub(super) async fn register_with_sasl(state: &AppState, account_id: &str, sender: &Sender, stream: &mut ClientStream, config: &IrcAccountConfig) -> Result<bool> {
    // SASL PLAIN is the password with base64 wrapped round it - an encoding,
    // not a cipher. On a cleartext link it is the password in the clear to
    // anything on the path, which is worse than NickServ only in that the
    // person doing it believes "SASL" means it is protected.
    if !sasl_transport_ok(config.ssl, config.allow_plaintext_sasl) {
        bail!(
            "refusing to send SASL credentials over an unencrypted connection to {} - \
             turn on TLS, or allow plaintext SASL for this account if the network really has no TLS port",
            config.host
        );
    }

    state.runtime.report_progress(state, account_id, "Requesting SASL capability...");
    sender.send_cap_req(&[Capability::Sasl])?;
    // NAK and the timeout mean the same thing here - this server has no SASL -
    // and both are answered by registering normally. Watching for NAK as well
    // as ACK is what turns the common case from a 20-second stall into an
    // immediate answer.
    let offered = wait_for(stream, |m| {
        matches!(
            &m.command,
            Command::CAP(_, CapSubCommand::ACK, _, _) | Command::CAP(_, CapSubCommand::NAK, _, _)
        )
    })
    .await
    .ok()
    .is_some_and(|m| matches!(&m.command, Command::CAP(_, CapSubCommand::ACK, _, _)));

    if !offered {
        state.runtime.report_progress(state, account_id, "Server has no SASL; registering normally...");
        end_cap_and_register(sender, stream, config).await?;
        return Ok(false);
    }

    // Strongest first. A server that will not take the one we chose says so
    // with 908 and lists what it does take, so the fallback is the server's
    // own answer rather than a guess made here.
    let mut tried: Vec<SaslMechanism> = Vec::new();
    let mut next = Some(preferred_mechanism(config));
    while let Some(mechanism) = next {
        tried.push(mechanism);
        match attempt_sasl(state, account_id, sender, stream, config, mechanism).await {
            Ok(()) => {
                end_cap_and_register(sender, stream, config).await?;
                return Ok(true);
            }
            Err(SaslRefusal::Fatal(e)) => return Err(e),
            Err(SaslRefusal::TryAnother(offered)) => {
                // Only what the server named, only what we can actually do,
                // and never one already tried - or a server that keeps
                // offering the same mechanism would loop forever.
                next = offered
                    .iter()
                    .filter_map(|name| SaslMechanism::parse(name))
                    .find(|m| !tried.contains(m));
                if next.is_none() {
                    bail!(
                        "SASL authentication failed - the server accepts {}, and this account is set up for {}",
                        if offered.is_empty() { "nothing this client speaks".to_string() } else { offered.join(", ") },
                        tried.iter().map(|m| m.name()).collect::<Vec<_>>().join(", ")
                    );
                }
                state.runtime.report_progress(
                    state,
                    account_id,
                    &format!("Server refused {}; trying {}...", tried.last().unwrap().name(), next.unwrap().name()),
                );
            }
        }
    }
    bail!("SASL authentication failed - check the SASL username and password for this account")
}

/// Which mechanism this account should lead with.
///
/// A certificate is a deliberate act, so its presence is taken as meaning it:
/// somebody who went and configured one wants EXTERNAL, and falling back to
/// sending the password would defeat the point of having set it up.
///
/// Otherwise PLAIN, which is not the strongest and is the right default
/// anyway. SCRAM-SHA-256 is barely deployed on IRC - Ergo has it; Libera,
/// Rizon and most of the rest offer PLAIN and EXTERNAL and nothing else - and
/// the spec only *recommends* that a server answer an unknown mechanism with
/// the list of ones it has. Leading with SCRAM would therefore break SASL
/// outright on the networks people actually use, on servers that decline to
/// say why. So SCRAM is there for whoever names it, and the 908 fallback picks
/// it up automatically on servers that do advertise properly.
pub(super) fn preferred_mechanism(config: &IrcAccountConfig) -> SaslMechanism {
    if let Some(named) = config.sasl_mechanism.as_deref().and_then(SaslMechanism::parse) {
        return named;
    }
    if config.sasl_cert_path.as_deref().is_some_and(|p| !p.is_empty()) {
        return SaslMechanism::External;
    }
    SaslMechanism::Plain
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SaslMechanism {
    External,
    ScramSha256,
    Plain,
}

impl SaslMechanism {
    fn name(self) -> &'static str {
        match self {
            Self::External => "EXTERNAL",
            Self::ScramSha256 => "SCRAM-SHA-256",
            Self::Plain => "PLAIN",
        }
    }

    pub(super) fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "EXTERNAL" => Some(Self::External),
            "SCRAM-SHA-256" | "SCRAM_SHA_256" | "SCRAM-SHA256" => Some(Self::ScramSha256),
            "PLAIN" => Some(Self::Plain),
            _ => None,
        }
    }
}

/// Why one mechanism did not work.
///
/// The distinction is the whole point: a refusal that names other mechanisms
/// is worth answering with one of them, while a wrong password is not - and
/// retrying PLAIN after SCRAM already established the password is wrong would
/// send that password in the clear for no reason.
pub(super) enum SaslRefusal {
    Fatal(anyhow::Error),
    TryAnother(Vec<String>),
}

impl From<anyhow::Error> for SaslRefusal {
    fn from(e: anyhow::Error) -> Self {
        Self::Fatal(e)
    }
}

pub(super) async fn attempt_sasl(
    state: &AppState,
    account_id: &str,
    sender: &Sender,
    stream: &mut ClientStream,
    config: &IrcAccountConfig,
    mechanism: SaslMechanism,
) -> std::result::Result<(), SaslRefusal> {
    state.runtime.report_progress(state, account_id, &format!("Starting SASL {}...", mechanism.name()));
    sender.send(Command::AUTHENTICATE(mechanism.name().to_string())).map_err(anyhow::Error::from)?;

    let user = config.sasl_user.clone().unwrap_or_else(|| config.nick.clone());
    let pass = config.password.clone().unwrap_or_default();

    match mechanism {
        // Nothing to send but the authorisation identity, and an empty one
        // means "whoever the certificate says".
        SaslMechanism::External => {
            expect_challenge(stream, "+").await?;
            sender.send_sasl("+").map_err(anyhow::Error::from)?;
        }
        SaslMechanism::Plain => {
            expect_challenge(stream, "+").await?;
            let payload = base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"));
            sender.send_sasl(payload).map_err(anyhow::Error::from)?;
        }
        SaslMechanism::ScramSha256 => {
            let mut scram = crate::backend::irc::sasl::Scram::new(&user, &pass, &crate::backend::irc::sasl::nonce());
            expect_challenge(stream, "+").await?;
            sender
                .send_sasl(base64::engine::general_purpose::STANDARD.encode(scram.client_first()))
                .map_err(anyhow::Error::from)?;

            let server_first = read_challenge(stream).await?;
            let client_final = scram.client_final(&server_first).map_err(SaslRefusal::Fatal)?;
            sender
                .send_sasl(base64::engine::general_purpose::STANDARD.encode(client_final))
                .map_err(anyhow::Error::from)?;

            let server_final = read_challenge(stream).await?;
            // Checked before the success numeric is believed: a server that
            // cannot prove it knew the password is not one to be logged in to,
            // whatever it says next.
            scram.verify(&server_final).map_err(SaslRefusal::Fatal)?;
            sender.send_sasl("+").map_err(anyhow::Error::from)?;
        }
    }

    let result = wait_for(stream, |m| {
        matches!(
            &m.command,
            Command::Response(Response::RPL_SASLSUCCESS, _)
                | Command::Response(Response::ERR_SASLFAIL, _)
                | Command::Response(Response::ERR_SASLTOOLONG, _)
                | Command::Response(Response::ERR_SASLABORT, _)
                | Command::Response(Response::RPL_SASLMECHS, _)
        )
    })
    .await
    .map_err(|_| SaslRefusal::Fatal(anyhow!("timed out waiting for SASL result")))?;

    match &result.command {
        Command::Response(Response::RPL_SASLSUCCESS, _) => Ok(()),
        // The server listing what it does take, which is the one refusal
        // worth answering with a different mechanism.
        Command::Response(Response::RPL_SASLMECHS, args) => Err(SaslRefusal::TryAnother(
            args.last().map(|list| list.split(',').map(|m| m.trim().to_string()).collect()).unwrap_or_default(),
        )),
        _ => Err(SaslRefusal::Fatal(anyhow!(
            "SASL {} was refused - check the SASL username and password for this account",
            mechanism.name()
        ))),
    }
}

/// Waits for the server's `AUTHENTICATE` and insists it is what was expected.
pub(super) async fn expect_challenge(stream: &mut ClientStream, wanted: &str) -> std::result::Result<(), SaslRefusal> {
    let got = read_challenge(stream).await?;
    if got != wanted {
        return Err(SaslRefusal::Fatal(anyhow!("server answered AUTHENTICATE with {got:?} rather than {wanted:?}")));
    }
    Ok(())
}

/// The server's next `AUTHENTICATE` payload, decoded.
///
/// A bare `+` means "nothing", and is passed through as itself rather than
/// decoded - it is not base64 for an empty string, it is the protocol's way of
/// writing one.
pub(super) async fn read_challenge(stream: &mut ClientStream) -> std::result::Result<String, SaslRefusal> {
    let message = wait_for(stream, |m| {
        matches!(
            &m.command,
            Command::AUTHENTICATE(_)
                | Command::Response(Response::ERR_SASLFAIL, _)
                | Command::Response(Response::RPL_SASLMECHS, _)
        )
    })
    .await
    .map_err(|_| SaslRefusal::Fatal(anyhow!("server stopped answering during SASL")))?;

    match &message.command {
        Command::AUTHENTICATE(payload) if payload == "+" => Ok("+".to_string()),
        Command::AUTHENTICATE(payload) => base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| SaslRefusal::Fatal(anyhow!("server's SASL challenge is not base64: {e}")))
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|e| SaslRefusal::Fatal(anyhow!("server's SASL challenge is not text: {e}")))
            }),
        Command::Response(Response::RPL_SASLMECHS, args) => Err(SaslRefusal::TryAnother(
            args.last().map(|list| list.split(',').map(|m| m.trim().to_string()).collect()).unwrap_or_default(),
        )),
        _ => Err(SaslRefusal::Fatal(anyhow!("SASL was refused before it finished"))),
    }
}
