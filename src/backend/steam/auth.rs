//! Signing in: turning a name and a password into a token that can be used
//! to sign in again without them.
//!
//! This is Steam's modern authentication service, and it happens on the same
//! connection that will carry the chat, before the account has signed in:
//!
//! 1. Ask for a public key for the account name, and encrypt the password with
//!    it. The key changes, which is why it is asked for each time; the
//!    password never goes over the connection in any other form.
//! 2. Begin a session with the name and the encrypted password. Steam answers
//!    with what it wants before it will let the session through: nothing, a
//!    code from the Steam Guard authenticator, a code it emailed, or an
//!    approval tapped on a phone.
//! 3. Send the code if one is wanted, and poll the session. A phone approval
//!    needs nothing sent - polling is how the answer is found.
//! 4. The poll eventually returns a **refresh token**. That, and not the
//!    password, is what is kept: a long-lived credential that signs the
//!    account in, and that can be revoked from Steam without changing the
//!    password.
//!
//! The password is held for the length of this exchange and no longer.

use super::link::{CallError, Link};
use super::proto::*;
use anyhow::{Context, Result};
use base64::Engine;
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPublicKey};
use std::time::Duration;

/// What moho signs in as: a desktop Steam client, the way every other third-party client does.
///
/// This is part of the token Steam issues and decides how Steam treats the connection that uses it. It was the
/// mobile app's chat session (platform 3, app type 2, website "Mobile") until Steam raised an account alert for an
/// unexpected device: a "phone" that reports a Linux desktop's operating system, has no machine id and logs on again
/// every time its connection is closed does not look like any real device. SteamKit2, ArchiSteamFarm, node-steam-user,
/// node-steam-session and steam-vent all sign in as the desktop client (platform 1, website "Client"), and so does this.
///
/// `EAuthTokenPlatformType` 1 is the desktop client. It has no app type.
const PLATFORM_TYPE: i32 = 1;
const WEBSITE_ID: &str = "Client";
/// `EGamingDeviceType` 1 is an ordinary PC, as node-steam-session sends for the desktop client.
const GAMING_DEVICE_TYPE: u32 = 1;

/// This install's own random number, made once and kept next to the daemon's other files, from which its machine
/// id and logon id are drawn. One per install and not per account: a machine is one machine however many accounts
/// sign in on it, and two installs (a test window, a second computer) are two.
pub fn install_id() -> [u8; 16] {
    // A test never writes to the real data directory.
    #[cfg(test)]
    return [7u8; 16];
    #[cfg(not(test))]
    {
    static ID: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        let path = crate::default_data_dir().join("steam-install-id");
        if let Some(read) = std::fs::read_to_string(&path).ok().and_then(|t| hex::decode(t.trim()).ok()).and_then(|b| <[u8; 16]>::try_from(b).ok()) {
            return read;
        }
        let made: [u8; 16] = rand::random();
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")));
        let _ = std::fs::write(&path, hex::encode(made));
        made
    })
    }
}

/// The machine id Steam's client sends: a binary key-value object of three SHA-1 hashes (named BB3, FF2 and 3B3 in the
/// client), here of strings made from the install's number, so it is the same each time and different for each install.
pub fn machine_id_from(install: &[u8; 16]) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let mut out = vec![0u8];
    out.extend_from_slice(b"MessageObject\0");
    for name in ["BB3", "FF2", "3B3"] {
        let hash = hex::encode(Sha1::digest(format!("moho {name} {}", hex::encode(install)).as_bytes()));
        out.push(1);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(hash.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(&[8, 8]);
    out
}

pub fn machine_id() -> Vec<u8> {
    machine_id_from(&install_id())
}

/// What tells this install's sessions from another's on the same account: Steam ends a session when a later one with
/// the same logon id arrives, and lets those with different ones live side by side. The desktop client sends its own
/// private address here; this sends a number of its own, never zero (which is "none").
pub fn logon_id_from(install: &[u8; 16]) -> u32 {
    (u32::from_le_bytes([install[0], install[1], install[2], install[3]]) | 1) ^ 0xBAAD_F00D
}

pub fn logon_id() -> u32 {
    logon_id_from(&install_id())
}

/// `EOSType` as the connection reports it: Linux 6.x is -184 and Windows 10
/// is 16, sent as the unsigned number the field is.
pub fn os_type() -> u32 {
    if cfg!(windows) {
        16
    } else {
        (-203i32) as u32
    }
}

/// What the account's Steam Guard settings are asking for.
#[derive(Clone, Debug, PartialEq)]
pub enum Guard {
    /// Nothing: the session goes through.
    None,
    /// A code Steam emailed to the account.
    EmailCode,
    /// A code from the Steam Guard authenticator on the person's phone.
    DeviceCode,
    /// An approval tapped in the Steam app on the person's phone.
    DeviceConfirmation,
    /// An approval by following a link in an email.
    EmailConfirmation,
    /// Something this client does not know how to answer.
    Other(i32),
}

impl Guard {
    fn from_wire(kind: i32) -> Guard {
        match kind {
            1 => Guard::None,
            2 => Guard::EmailCode,
            3 => Guard::DeviceCode,
            4 => Guard::DeviceConfirmation,
            5 => Guard::EmailConfirmation,
            other => Guard::Other(other),
        }
    }

    fn code_type(&self) -> Option<i32> {
        match self {
            Guard::EmailCode => Some(2),
            Guard::DeviceCode => Some(3),
            _ => None,
        }
    }
}

/// One way Steam will accept for getting the session through, with the hint it
/// gives (an email address with most of it hidden, for the emailed code).
#[derive(Clone, Debug, PartialEq)]
pub struct Confirmation {
    pub guard: Guard,
    pub hint: Option<String>,
}

/// A sign-in that has begun and is waiting on Steam Guard, or on the poll.
#[derive(Clone, Debug)]
pub struct Begun {
    pub client_id: u64,
    pub request_id: Vec<u8>,
    /// How often Steam asks to be polled.
    pub interval: Duration,
    pub steamid: u64,
    pub confirmations: Vec<Confirmation>,
}

impl Begun {
    /// Whether the session goes through with nothing more from the person.
    pub fn needs_nothing(&self) -> bool {
        self.confirmations.is_empty() || self.confirmations.iter().any(|c| c.guard == Guard::None)
    }
}

/// What a finished sign-in leaves.
#[derive(Clone, Debug, PartialEq)]
pub struct Tokens {
    pub refresh_token: String,
    pub access_token: String,
    pub account_name: Option<String>,
    /// Steam's note of this machine having been approved, to send next time so
    /// it is not asked again.
    pub guard_data: Option<String>,
}

/// The password, encrypted with the public key Steam gave for the account.
pub fn encrypt_password(modulus_hex: &str, exponent_hex: &str, password: &str) -> Result<String> {
    let n = BigUint::parse_bytes(modulus_hex.trim().as_bytes(), 16).context("Steam's password key has no modulus")?;
    let e = BigUint::parse_bytes(exponent_hex.trim().as_bytes(), 16).context("Steam's password key has no exponent")?;
    let key = RsaPublicKey::new(n, e).context("Steam's password key is not a usable key")?;
    let encrypted = key.encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, password.as_bytes()).context("encrypting the password")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
}

/// What Steam shows next to this session in the account's list of signed-in
/// devices, and on the phone when it asks for an approval.
pub fn device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());
    match host {
        Some(host) => format!("moho on {host}"),
        None => "moho".to_string(),
    }
}

fn device_details() -> DeviceDetails {
    DeviceDetails {
        device_friendly_name: Some(device_name()),
        platform_type: Some(PLATFORM_TYPE),
        os_type: Some(os_type() as i32),
        gaming_device_type: Some(GAMING_DEVICE_TYPE),
        // Sent with a password sign-in, as the desktop client's is.
        machine_id: Some(machine_id()),
        ..DeviceDetails::default()
    }
}

/// How often to poll, from what Steam asked for: never faster than twice a
/// second, so a server that says zero is not hammered, and never slower than
/// every thirty, so one that says an hour is not left alone. Five seconds when
/// it said nothing.
fn poll_interval(asked: Option<f32>) -> Duration {
    Duration::from_secs_f32(asked.filter(|s| s.is_finite()).unwrap_or(5.0).clamp(0.5, 30.0))
}

/// Starts a sign-in with a name and a password.
///
/// `guard_data` is what a previous sign-in on this machine was given to
/// remember it by, if there was one.
pub async fn begin(link: &Link, account_name: &str, password: &str, guard_data: Option<&str>) -> Result<Begun, CallError> {
    let key: GetPasswordRsaPublicKeyResponse = link
        .call("Authentication.GetPasswordRSAPublicKey#1", &GetPasswordRsaPublicKeyRequest { account_name: Some(account_name.to_string()) }, false)
        .await?;
    let encrypted = encrypt_password(key.publickey_mod.as_deref().unwrap_or_default(), key.publickey_exp.as_deref().unwrap_or_default(), password)
        .map_err(|e| CallError::Unreadable(format!("{e:#}")))?;

    let request = BeginAuthViaCredentialsRequest {
        device_friendly_name: Some(device_name()),
        account_name: Some(account_name.to_string()),
        encrypted_password: Some(encrypted),
        encryption_timestamp: key.timestamp,
        remember_login: Some(true),
        platform_type: Some(PLATFORM_TYPE),
        // Persistent: a token that outlives this run, which is the point.
        persistence: Some(1),
        website_id: Some(WEBSITE_ID.to_string()),
        device_details: Some(device_details()),
        guard_data: guard_data.filter(|g| !g.is_empty()).map(str::to_string),
        ..BeginAuthViaCredentialsRequest::default()
    };
    let begun: BeginAuthViaCredentialsResponse = link.call("Authentication.BeginAuthSessionViaCredentials#1", &request, false).await?;

    Ok(Begun {
        client_id: begun.client_id.unwrap_or_default(),
        request_id: begun.request_id.unwrap_or_default(),
        interval: poll_interval(begun.interval),
        steamid: begun.steamid.unwrap_or_default(),
        confirmations: begun
            .allowed_confirmations
            .into_iter()
            .map(|c| Confirmation { guard: Guard::from_wire(c.confirmation_type.unwrap_or_default()), hint: c.associated_message.filter(|m| !m.is_empty()) })
            .collect(),
    })
}

/// Gives Steam the code that was asked for.
pub async fn send_code(link: &Link, begun: &Begun, guard: &Guard, code: &str) -> Result<(), CallError> {
    let Some(code_type) = guard.code_type() else {
        return Err(CallError::Unreadable("that kind of confirmation takes no code".to_string()));
    };
    let _: UpdateAuthWithGuardCodeResponse = link
        .call(
            "Authentication.UpdateAuthSessionWithSteamGuardCode#1",
            &UpdateAuthWithGuardCodeRequest { client_id: Some(begun.client_id), steamid: Some(begun.steamid), code: Some(code.trim().to_string()), code_type: Some(code_type) },
            false,
        )
        .await?;
    Ok(())
}

/// What a failure while waiting for a sign-in to be approved means: for this, Steam's "not found"
/// and "expired" are the same - the sign-in ran out before anybody approved it - which the Steam
/// client treats as one thing; the agreement page and the rate limit are said as the general table
/// says them.
pub fn waiting_failure(error: CallError) -> String {
    match error {
        CallError::Refused(super::eresult::FILE_NOT_FOUND | super::eresult::EXPIRED, _) => "that sign-in ran out before it was approved; start again".to_string(),
        other => other.to_string(),
    }
}

/// Starts a sign-in that a phone will approve by scanning a code. Answers with
/// the address the code carries; the session is then polled as any other.
pub async fn begin_qr(link: &Link) -> Result<(Begun, String), CallError> {
    let request = BeginAuthViaQrRequest { device_friendly_name: Some(device_name()), platform_type: Some(PLATFORM_TYPE), device_details: Some(device_details()), website_id: Some(WEBSITE_ID.to_string()) };
    let begun: BeginAuthViaQrResponse = link.call("Authentication.BeginAuthSessionViaQR#1", &request, false).await?;
    let url = begun.challenge_url.filter(|u| !u.is_empty()).ok_or_else(|| CallError::Unreadable("Steam gave no code to scan".to_string()))?;
    Ok((
        Begun { client_id: begun.client_id.unwrap_or_default(), request_id: begun.request_id.unwrap_or_default(), interval: poll_interval(begun.interval), steamid: 0, confirmations: Vec::new() },
        url,
    ))
}

/// Who a token is for: its subject, in the middle part of the token. A scan
/// does not say, and an account is kept by its number.
pub fn steamid_of_token(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    claims["sub"].as_str()?.parse().ok()
}

/// What one look at a session that is waiting for a scan found.
pub struct Polled {
    pub tokens: Option<Tokens>,
    /// The code runs out and is replaced: the address to draw now.
    pub new_challenge: Option<String>,
}

/// Asks once whether the session has gone through. `None` means not yet.
pub async fn poll(link: &Link, begun: &mut Begun) -> Result<Option<Tokens>, CallError> {
    Ok(poll_session(link, begun).await?.tokens)
}

/// The same, and what else the answer said.
pub async fn poll_session(link: &Link, begun: &mut Begun) -> Result<Polled, CallError> {
    let answer: PollAuthSessionResponse = link
        .call(
            "Authentication.PollAuthSessionStatus#1",
            &PollAuthSessionRequest { client_id: Some(begun.client_id), request_id: Some(begun.request_id.clone()), token_to_revoke: None },
            false,
        )
        .await?;
    // A session that was handed a new id carries on under it.
    if let Some(id) = answer.new_client_id.filter(|id| *id != 0) {
        begun.client_id = id;
    }
    let new_challenge = answer.new_challenge_url.filter(|u| !u.is_empty());
    let tokens = answer.refresh_token.filter(|t| !t.is_empty()).map(|refresh_token| Tokens {
        refresh_token,
        access_token: answer.access_token.unwrap_or_default(),
        account_name: answer.account_name.filter(|n| !n.is_empty()),
        guard_data: answer.new_guard_data.filter(|g| !g.is_empty()),
    });
    Ok(Polled { tokens, new_challenge })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::steam::link::{self, fake::*};

    #[test]
    fn a_sign_in_that_ran_out_is_said_so_and_the_rest_by_the_general_table() {
        assert!(waiting_failure(CallError::Refused(9, None)).contains("ran out"));
        assert!(waiting_failure(CallError::Refused(27, None)).contains("ran out"));
        assert!(waiting_failure(CallError::Refused(84, None)).contains("limiting"));
        let page = waiting_failure(CallError::Refused(118, Some("https://login.steampowered.com/agree".into())));
        assert!(page.contains("agreement") && page.contains("https://login.steampowered.com/agree"), "{page}");
    }
    use crate::backend::steam::wire::Packet;
    use prost::Message;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;
    use std::sync::{Arc, Mutex};

    fn keypair() -> (RsaPrivateKey, String, String) {
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
        let public = private.to_public_key();
        let (n, e) = (public.n().to_str_radix(16), public.e().to_str_radix(16));
        (private, n, e)
    }

    #[test]
    fn the_password_is_encrypted_so_only_steams_key_can_read_it() {
        let (private, n, e) = keypair();
        let encrypted = encrypt_password(&n, &e, "hunter2").unwrap();
        assert!(!encrypted.contains("hunter2"));
        let bytes = base64::engine::general_purpose::STANDARD.decode(&encrypted).unwrap();
        assert_eq!(private.decrypt(Pkcs1v15Encrypt, &bytes).unwrap(), b"hunter2");
        // Encrypted afresh each time, so two sign-ins do not look the same.
        assert_ne!(encrypted, encrypt_password(&n, &e, "hunter2").unwrap());
    }

    #[test]
    fn a_key_steam_did_not_really_send_is_refused_not_used() {
        assert!(encrypt_password("", "010001", "x").is_err());
        assert!(encrypt_password("zz", "010001", "x").is_err());
        assert!(encrypt_password("0A", "010001", "x").is_err(), "too small to hold a password");
    }

    #[test]
    fn what_steam_asks_for_is_told_apart() {
        assert_eq!(Guard::from_wire(3), Guard::DeviceCode);
        assert_eq!(Guard::from_wire(2), Guard::EmailCode);
        assert_eq!(Guard::from_wire(4), Guard::DeviceConfirmation);
        assert_eq!(Guard::from_wire(5), Guard::EmailConfirmation);
        assert_eq!(Guard::from_wire(1), Guard::None);
        assert_eq!(Guard::DeviceCode.code_type(), Some(3));
        assert_eq!(Guard::EmailCode.code_type(), Some(2));
        assert_eq!(Guard::DeviceConfirmation.code_type(), None, "an approval takes no code");
        assert_eq!(Guard::from_wire(99), Guard::Other(99));
    }

    /// A Steam that wants a code from the authenticator, then issues tokens.
    fn steam_with_guard(private: &RsaPrivateKey, n: String, e: String, seen: Arc<Mutex<Vec<Packet>>>) -> Script {
        let _ = private;
        let mut polls = 0;
        Box::new(move |req| {
            seen.lock().unwrap().push(req.clone());
            let method = req.header.target_job_name.clone().unwrap_or_default();
            let reply = match method.as_str() {
                "Authentication.GetPasswordRSAPublicKey#1" => {
                    GetPasswordRsaPublicKeyResponse { publickey_mod: Some(n.clone()), publickey_exp: Some(e.clone()), timestamp: Some(1790000005000) }.encode_to_vec()
                }
                "Authentication.BeginAuthSessionViaCredentials#1" => BeginAuthViaCredentialsResponse {
                    client_id: Some(1234),
                    request_id: Some(vec![9, 8, 7]),
                    interval: Some(0.5),
                    steamid: Some(76561197960287930),
                    allowed_confirmations: vec![
                        AllowedConfirmation { confirmation_type: Some(3), associated_message: None },
                        AllowedConfirmation { confirmation_type: Some(4), associated_message: Some("Steam app".into()) },
                    ],
                    ..Default::default()
                }
                .encode_to_vec(),
                "Authentication.UpdateAuthSessionWithSteamGuardCode#1" => UpdateAuthWithGuardCodeResponse::default().encode_to_vec(),
                "Authentication.PollAuthSessionStatus#1" => {
                    polls += 1;
                    if polls < 2 {
                        PollAuthSessionResponse::default().encode_to_vec()
                    } else {
                        PollAuthSessionResponse {
                            refresh_token: Some("the-refresh-token".into()),
                            access_token: Some("the-access-token".into()),
                            account_name: Some("someone".into()),
                            new_guard_data: Some("remember-me".into()),
                            ..Default::default()
                        }
                        .encode_to_vec()
                    }
                }
                other => panic!("unexpected call {other}"),
            };
            Some(vec![answer(&req, 1, reply)])
        })
    }

    #[tokio::test]
    async fn signing_in_with_a_code_goes_through_every_step_in_order() {
        let (private, n, e) = keypair();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(steam_with_guard(&private, n, e, seen.clone())).await;
        let (link, _told) = link::open("steam:test", &url).await.unwrap();

        let mut begun = begin(&link, "someone", "hunter2", Some("old-guard")).await.unwrap();
        assert_eq!(begun.steamid, 76561197960287930);
        assert_eq!(begun.interval, Duration::from_millis(500));
        assert_eq!(begun.confirmations[0].guard, Guard::DeviceCode);
        assert_eq!(begun.confirmations[1], Confirmation { guard: Guard::DeviceConfirmation, hint: Some("Steam app".into()) });
        assert!(!begun.needs_nothing());

        send_code(&link, &begun, &Guard::DeviceCode, " AB12C ").await.unwrap();
        assert_eq!(poll(&link, &mut begun).await.unwrap(), None, "not yet");
        let tokens = poll(&link, &mut begun).await.unwrap().expect("tokens now");
        assert_eq!(tokens.refresh_token, "the-refresh-token");
        assert_eq!(tokens.account_name.as_deref(), Some("someone"));
        assert_eq!(tokens.guard_data.as_deref(), Some("remember-me"));

        let seen = seen.lock().unwrap();
        let methods: Vec<_> = seen.iter().map(|p| p.header.target_job_name.clone().unwrap()).collect();
        assert_eq!(
            methods,
            [
                "Authentication.GetPasswordRSAPublicKey#1",
                "Authentication.BeginAuthSessionViaCredentials#1",
                "Authentication.UpdateAuthSessionWithSteamGuardCode#1",
                "Authentication.PollAuthSessionStatus#1",
                "Authentication.PollAuthSessionStatus#1",
            ]
        );
        // None of it as the account: there is no account signed in yet.
        assert!(seen.iter().all(|p| p.header.steamid.is_none()));

        let begin_req = BeginAuthViaCredentialsRequest::decode(seen[1].body.as_slice()).unwrap();
        assert_eq!(begin_req.account_name.as_deref(), Some("someone"));
        assert_eq!(begin_req.encryption_timestamp, Some(1790000005000));
        assert_eq!(begin_req.persistence, Some(1));
        assert_eq!(begin_req.guard_data.as_deref(), Some("old-guard"));
        assert_eq!(begin_req.platform_type, Some(1), "it signs in as a desktop client, like every other third-party client");
        assert_eq!(begin_req.website_id.as_deref(), Some("Client"));
        let details = begin_req.device_details.unwrap();
        assert_eq!((details.platform_type, details.app_type, details.gaming_device_type), (Some(1), None, Some(1)));
        assert_eq!(details.machine_id.as_deref(), Some(machine_id().as_slice()), "and with its install's machine id");
        assert!(!begin_req.encrypted_password.unwrap().contains("hunter2"));

        let code_req = UpdateAuthWithGuardCodeRequest::decode(seen[2].body.as_slice()).unwrap();
        assert_eq!((code_req.client_id, code_req.steamid), (Some(1234), Some(76561197960287930)));
        assert_eq!(code_req.code.as_deref(), Some("AB12C"), "spaces around a pasted code are dropped");
        assert_eq!(code_req.code_type, Some(3));

        let poll_req = PollAuthSessionRequest::decode(seen[3].body.as_slice()).unwrap();
        assert_eq!((poll_req.client_id, poll_req.request_id), (Some(1234), Some(vec![9, 8, 7])));
    }

    #[tokio::test]
    async fn a_wrong_password_says_so_and_a_code_where_none_is_wanted_is_refused() {
        let (_private, n, e) = keypair();
        let url = serve(Box::new(move |req| {
            let method = req.header.target_job_name.clone().unwrap_or_default();
            Some(vec![if method.contains("GetPasswordRSAPublicKey") {
                answer(&req, 1, GetPasswordRsaPublicKeyResponse { publickey_mod: Some(n.clone()), publickey_exp: Some(e.clone()), timestamp: Some(1) }.encode_to_vec())
            } else {
                answer(&req, 5, Vec::new())
            }])
        }))
        .await;
        let (link, _told) = link::open("steam:test", &url).await.unwrap();
        let err = begin(&link, "someone", "wrong", None).await.unwrap_err();
        assert!(matches!(err, CallError::Refused(5, _)), "{err:?}");
        assert!(err.to_string().contains("password"));

        let begun = Begun { client_id: 1, request_id: vec![], interval: Duration::from_secs(1), steamid: 1, confirmations: vec![] };
        assert!(send_code(&link, &begun, &Guard::DeviceConfirmation, "123").await.is_err(), "an approval takes no code");
    }

    #[tokio::test]
    async fn an_account_without_steam_guard_needs_nothing_more() {
        let (_private, n, e) = keypair();
        let url = serve(Box::new(move |req| {
            let method = req.header.target_job_name.clone().unwrap_or_default();
            Some(vec![if method.contains("GetPasswordRSAPublicKey") {
                answer(&req, 1, GetPasswordRsaPublicKeyResponse { publickey_mod: Some(n.clone()), publickey_exp: Some(e.clone()), timestamp: Some(1) }.encode_to_vec())
            } else {
                answer(
                    &req,
                    1,
                    BeginAuthViaCredentialsResponse {
                        client_id: Some(1),
                        request_id: Some(vec![1]),
                        interval: Some(1.0),
                        steamid: Some(2),
                        allowed_confirmations: vec![AllowedConfirmation { confirmation_type: Some(1), associated_message: None }],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
            }])
        }))
        .await;
        let (link, _told) = link::open("steam:test", &url).await.unwrap();
        assert!(begin(&link, "someone", "x", None).await.unwrap().needs_nothing());
    }

    #[test]
    fn the_polling_interval_is_kept_to_something_sensible() {
        assert_eq!(poll_interval(Some(0.0)), Duration::from_millis(500));
        assert_eq!(poll_interval(Some(3600.0)), Duration::from_secs(30));
        assert_eq!(poll_interval(Some(2.0)), Duration::from_secs(2));
        assert_eq!(poll_interval(None), Duration::from_secs(5));
        assert_eq!(poll_interval(Some(f32::NAN)), Duration::from_secs(5), "not a number is not an answer");
        assert_eq!(poll_interval(Some(-3.0)), Duration::from_millis(500));
    }

    #[test]
    fn the_session_is_named_for_the_machine_it_is_on() {
        assert!(device_name().starts_with("moho"));
        assert_eq!(os_type(), if cfg!(windows) { 16 } else { 4294967093 });
    }

    #[test]
    fn a_token_says_whose_it_is_in_its_subject() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"iss":"steam","sub":"76561197960287930","aud":["mobile"]}"#);
        assert_eq!(steamid_of_token(&format!("eyJhbGciOiJFZERTQSJ9.{payload}.signature")), Some(76561197960287930));
        assert_eq!(steamid_of_token("not a token"), None);
        assert_eq!(steamid_of_token("a.b.c"), None);
    }

    #[test]
    fn the_machine_id_is_a_key_value_object_of_three_hashes_that_is_the_same_for_one_install_and_not_for_another() {
        let one = machine_id_from(&[1u8; 16]);
        assert_eq!(one, machine_id_from(&[1u8; 16]));
        assert_ne!(one, machine_id_from(&[2u8; 16]));
        // 0x00 "MessageObject" 0x00, then three strings named BB3, FF2 and 3B3 of forty hex digits, then 0x08 0x08.
        assert!(one.starts_with(b"\0MessageObject\0\x01BB3\0"));
        assert!(one.ends_with(&[0, 8, 8]));
        let text = String::from_utf8_lossy(&one).to_string();
        for name in ["BB3", "FF2", "3B3"] {
            assert!(text.contains(name));
        }
        assert_eq!(one.len(), 1 + 14 + 3 * (1 + 3 + 1 + 40 + 1) + 2);
        assert_ne!(logon_id_from(&[1u8; 16]), 0);
        assert_ne!(logon_id_from(&[1u8; 16]), logon_id_from(&[2u8; 16]));
        // Never the same as the zero a client with no id of its own sends.
        assert_ne!(logon_id_from(&[0xFF; 16]), 0);
    }
}
