//! Signing in by QR code: moho shows the code, the person's phone scans it.
//!
//! MSC4108, as matrix-rust-sdk 0.18 - and so Element X - speaks it. moho is
//! always the new device and never has a camera, so only the side that
//! *shows* the code is here. In order:
//!
//! 1. A rendezvous session on the homeserver: a mailbox both devices can
//!    read and write over plain HTTP, guarded by ETags so neither writes over
//!    the other.
//! 2. The QR code: the mailbox's address and a fresh Curve25519 key.
//! 3. The phone writes an ECIES-encrypted `MATRIX_QR_CODE_LOGIN_INITIATE`;
//!    moho answers `MATRIX_QR_CODE_LOGIN_OK`. Everything after this is sealed,
//!    so the homeserver carries ciphertext only.
//! 4. The phone shows two digits derived from the shared secret, and the
//!    person types them here - which is what proves that the device on the
//!    other end of the mailbox is the phone in their hand.
//! 5. The phone names the homeserver (`m.login.protocols`). moho starts an
//!    OAuth device-authorization grant, as a device whose id is the
//!    Curve25519 key of a new Olm account, and passes the grant over
//!    (`m.login.protocol`); the phone approves it, and moho polls for its
//!    tokens.
//! 6. moho says it is in (`m.login.success`), and the phone sends the
//!    account's secrets (`m.login.secrets`): the private cross-signing keys
//!    and the backup key. They go into the new device's crypto store before
//!    its keys are ever uploaded, so the device arrives signed - verified -
//!    and can read the history the backup holds.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use matrix_sdk_crypto::types::qr_login::{Msc4108IntentData, QrCodeData};
use matrix_sdk_crypto::types::SecretsBundle;
use matrix_sdk_crypto::vodozemac::ecies::{Ecies, EstablishedEcies, InitialMessage, Message};
use matrix_sdk_crypto::vodozemac::hpke::DigitMode;
use serde_json::{json, Value};

use super::*;

const LOGIN_INITIATE: &str = "MATRIX_QR_CODE_LOGIN_INITIATE";
const LOGIN_OK: &str = "MATRIX_QR_CODE_LOGIN_OK";
/// How long the code stays on screen waiting to be scanned.
const SCAN_WAIT: Duration = Duration::from_secs(300);
/// How long each later step may take: the person reading two digits off
/// their phone, or approving the sign-in on it.
const STEP_WAIT: Duration = Duration::from_secs(180);
/// Between looks at the mailbox, as the reference client does.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// The digits each waiting sign-in has been told, by login id.
fn check_codes() -> &'static Mutex<HashMap<String, tokio::sync::oneshot::Sender<u8>>> {
    static CODES: OnceLock<Mutex<HashMap<String, tokio::sync::oneshot::Sender<u8>>>> = OnceLock::new();
    CODES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Starts a QR sign-in in the background. Progress and the outcome arrive as
/// events tagged with `login_id`, like the other ways in.
pub fn start_qr_login(state: AppState, login_id: String, homeserver_url: String) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(try_qr_login(&state, &login_id, &homeserver_url)).catch_unwind().await;
        check_codes().lock().unwrap().remove(&login_id);
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("matrix qr login[{login_id}]: {error}");
        state.events.emit("matrixLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

/// The two digits the phone is showing, typed in by the person.
pub fn confirm_check_code(login_id: &str, code: u8) -> Result<()> {
    let waiting = check_codes().lock().unwrap().remove(login_id);
    match waiting {
        Some(sender) => sender.send(code).map_err(|_| anyhow::anyhow!("that sign-in is no longer waiting for a code")),
        None => bail!("that sign-in is not waiting for a code"),
    }
}

async fn try_qr_login(state: &AppState, login_id: &str, homeserver_url: &str) -> Result<()> {
    let status = |detail: &str| state.events.emit("matrixLoginStatus", json!({ "loginId": login_id, "detail": detail }));

    status("finding the server...");
    let homeserver_url = http::resolve_homeserver(homeserver_url).await?;
    oidc::auth_metadata(&homeserver_url)
        .await?
        .context("this homeserver holds its own accounts, so it cannot sign in by QR code - use a password")?;

    // 2. The code, made again whenever its mailbox expires - Synapse keeps
    // an untouched one for a minute - until it is scanned or the person has
    // had long enough. The key stays the same; only the mailbox changes.
    status("making a code...");
    let ecies = Ecies::new();
    let path = qr_path(login_id);
    let give_up = tokio::time::Instant::now() + SCAN_WAIT;
    let mut shown = 0u32;
    let (mailbox, opening) = loop {
        if tokio::time::Instant::now() >= give_up {
            let _ = std::fs::remove_file(&path);
            bail!("the code was not scanned in time");
        }
        let mut mailbox = Mailbox::open(&homeserver_url).await?;
        let qr = QrCodeData::new_msc4108(ecies.public_key(), url::Url::parse(&mailbox.url)?, Msc4108IntentData::Login);
        // A new file each time, so the window draws the new code rather than
        // a cached copy of the old one.
        shown += 1;
        let _ = std::fs::remove_file(&path);
        let path_now = path.with_file_name(format!("{}-{shown}.png", path.file_stem().and_then(|s| s.to_str()).unwrap_or("qr")));
        write_qr(&qr.to_bytes(), &path_now)?;
        state.events.emit("matrixQrCode", json!({ "loginId": login_id, "qrPath": path_now.display().to_string() }));
        if shown == 1 {
            tracing::info!("matrix qr login[{login_id}]: showing a code for {homeserver_url}");
        }
        let waited = mailbox.receive_or_expire(give_up.saturating_duration_since(tokio::time::Instant::now())).await;
        let _ = std::fs::remove_file(&path_now);
        match waited {
            Ok(Some(opening)) => break (mailbox, opening),
            Ok(None) => continue,
            Err(e) => return Err(e.context("waiting for the code to be scanned")),
        }
    };

    // 3. The phone's opening message, and our answer.
    let initial = InitialMessage::decode(&opening).context("the scanning device sent something that is not a QR sign-in")?;
    let inbound = ecies.establish_inbound_channel(&initial).context("opening the secure channel")?;
    if inbound.message != LOGIN_INITIATE.as_bytes() {
        bail!("the scanning device did not start a sign-in");
    }
    let mut channel = Channel { mailbox, ecies: inbound.ecies };
    channel.send_text(LOGIN_OK).await?;

    // 4. The two digits.
    let (tx, rx) = tokio::sync::oneshot::channel();
    check_codes().lock().unwrap().insert(login_id.to_string(), tx);
    state.events.emit("matrixQrCheckCode", json!({ "loginId": login_id }));
    status("enter the two digits your phone shows...");
    let typed = tokio::time::timeout(STEP_WAIT, rx)
        .await
        .context("nobody entered the code in time")?
        .context("the sign-in was cancelled")?;
    // The original MSC4108 rendering, which is the one this ECIES channel
    // speaks: a leading zero allowed. (MSC4388's HPKE channel forbids one;
    // vodozemac now asks which is meant.)
    if typed != channel.ecies.check_code().to_digit(DigitMode::AllowLeadingZero) {
        let _ = channel.send_json(failure("user_cancelled")).await;
        bail!("those digits do not match the ones on your phone - start again, and check you scanned this screen");
    }

    // 5. Which homeserver, then the grant.
    status("waiting for your phone...");
    let protocols = channel.receive_json(STEP_WAIT).await?;
    if protocols["type"] != "m.login.protocols" {
        let _ = channel.send_json(failure("unexpected_message_received")).await;
        bail!("your phone sent {} where it should have named the server", protocols["type"]);
    }
    if !protocols["protocols"].as_array().is_some_and(|p| p.iter().any(|p| p == "device_authorization_grant")) {
        let _ = channel.send_json(failure("unsupported_protocol")).await;
        bail!("your phone offers no sign-in method moho supports");
    }
    let homeserver_url = match protocols["homeserver"].as_str() {
        Some(named) if !named.is_empty() => http::resolve_homeserver(named).await?,
        _ => homeserver_url,
    };
    let metadata = oidc::auth_metadata(&homeserver_url).await?.context("your account's server does not use OIDC accounts")?;
    let client_id = oidc::register_client(&metadata).await?;
    // The device id is the new account's identity key, as the protocol
    // requires; the same account becomes this device's crypto identity.
    let account = matrix_sdk_crypto::vodozemac::olm::Account::new();
    let device_id = account.curve25519_key().to_base64();
    let grant = oidc::request_device_code(&metadata, &client_id, &device_id).await?;
    channel
        .send_json(json!({
            "type": "m.login.protocol",
            "protocol": "device_authorization_grant",
            "device_id": device_id,
            "device_authorization_grant": {
                "verification_uri": grant.verification_uri,
                "verification_uri_complete": grant.verification_uri_complete,
            },
        }))
        .await?;
    let accepted = channel.receive_json(STEP_WAIT).await?;
    match accepted["type"].as_str() {
        Some("m.login.protocol_accepted") => {}
        Some("m.login.failure") => bail!("your phone refused the sign-in ({})", accepted["reason"].as_str().unwrap_or("no reason given")),
        other => {
            let _ = channel.send_json(failure("unexpected_message_received")).await;
            bail!("your phone sent {other:?} where it should have accepted the sign-in");
        }
    }

    status("approve the sign-in on your phone...");
    let deadline = tokio::time::Instant::now() + grant.expires_in;
    let mut interval = grant.interval;
    let (access_token, refresh_token) = loop {
        if tokio::time::Instant::now() >= deadline {
            let _ = channel.send_json(failure("authorization_expired")).await;
            bail!("the sign-in was not approved in time");
        }
        tokio::time::sleep(interval).await;
        match oidc::poll_once(&metadata, &client_id, &grant.device_code).await {
            oidc::Poll::Pending => {}
            oidc::Poll::SlowDown => interval += Duration::from_secs(5),
            oidc::Poll::Stopped(why) => {
                let refused = why.contains("refused");
                let _ = channel
                    .send_json(if refused { json!({ "type": "m.login.declined" }) } else { failure("authorization_expired") })
                    .await;
                bail!("{why}");
            }
            oidc::Poll::Approved { access_token, refresh_token, .. } => break (access_token, refresh_token),
        }
    };

    status("signing in...");
    let whoami = http::get_json(&format!("{}/_matrix/client/v3/account/whoami", homeserver_url.trim_end_matches('/')), &access_token)
        .await
        .context("asking the homeserver who this token belongs to")?;
    let user_id = whoami["user_id"].as_str().context("the homeserver did not say who this is")?.to_string();
    if whoami["device_id"].as_str().is_some_and(|d| d != device_id) {
        let _ = channel.send_json(failure("device_not_found")).await;
        bail!("the homeserver gave this sign-in a different device than the one asked for");
    }
    // Named outright, for the same reason as the code sign-in: a grant has
    // nowhere to carry a display name, and "Unknown device" is the line that
    // gets revoked in a hurry.
    let _ = http::put_json(
        &format!(
            "{}/_matrix/client/v3/devices/{}",
            homeserver_url.trim_end_matches('/'),
            url::form_urlencoded::byte_serialize(device_id.as_bytes()).collect::<String>()
        ),
        &access_token,
        json!({ "display_name": DEVICE_DISPLAY_NAME }),
    )
    .await;

    // 6. The secrets.
    channel.send_json(json!({ "type": "m.login.success" })).await?;
    status("receiving your encryption keys...");
    let secrets = match channel.receive_json(STEP_WAIT).await {
        Ok(message) if message["type"] == "m.login.secrets" => match serde_json::from_value::<SecretsBundle>(message) {
            Ok(bundle) => Some(bundle),
            Err(e) => {
                tracing::warn!("matrix qr login[{login_id}]: the secrets did not parse: {e}");
                None
            }
        },
        Ok(other) => {
            tracing::warn!("matrix qr login[{login_id}]: no secrets came - got {}", other["type"]);
            None
        }
        Err(e) => {
            tracing::warn!("matrix qr login[{login_id}]: no secrets came: {e:#}");
            None
        }
    };

    let config = MatrixAccountConfig {
        use_tor: crate::net::route::router().wanted(&crate::net::route::pending_key("matrix")),
        // Never strict at sign-in; the account store keeps an existing
        // account's strict routing across a re-login.
        strict_route: false,
        homeserver_url: homeserver_url.clone(),
        user_id: user_id.clone(),
        password: String::new(),
        access_token,
        device_id: device_id.clone(),
        oauth_refresh_token: refresh_token,
        oauth_client_id: client_id,
        next_batch: None,
        used_sliding_sync: false,
        prefer_sliding_sync: false,
        dehydration_enabled: false,
        display_name: None,
        rtc_focus_url: None,
    };
    let account_id = config.account_id();
    let ruma_user = ruma_common::UserId::parse(&user_id).context("the homeserver named an invalid user")?;
    crypto::CryptoSession::create_with_account(
        &sync::config_dir(),
        &account_id,
        &ruma_user,
        <&ruma_common::DeviceId>::from(device_id.as_str()),
        account,
        secrets.as_ref(),
    )
    .await?;

    let saved = state.accounts.add_matrix(config)?;
    let account_json = crate::accounts::matrix_account_to_json(&saved, "connecting", false);
    spawn(state.clone(), saved);
    state.events.emit("matrixLoginResult", json!({ "loginId": login_id, "success": true, "account": account_json }));
    tracing::info!(
        "matrix qr login[{login_id}]: signed in as {user_id}, {}",
        if secrets.is_some() { "verified, with the account's keys" } else { "without the account's keys" }
    );

    // The history the backup holds, once the account's session is up.
    let backup_key = secrets.as_ref().and_then(|s| s.backup.as_ref()).map(|b| match b {
        matrix_sdk_crypto::types::BackupSecrets::MegolmBackupV1Curve25519AesSha2(b) => b.key.to_base58(),
    });
    if let Some(key) = backup_key {
        let state = state.clone();
        tokio::spawn(async move {
            for _ in 0..60 {
                if state.runtime.get_matrix_machine(&account_id).is_some() {
                    match backup::restore_from_recovery_key(&state, &account_id, &key).await {
                        Ok(summary) => tracing::info!("matrix qr login: restored {} of {} backed-up keys", summary.imported_keys, summary.total_keys),
                        Err(e) => tracing::warn!("matrix qr login: restoring the backup: {e:#}"),
                    }
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
    Ok(())
}

/// Whether a homeserver says it keeps MSC4108 sign-in mailboxes. A claim,
/// not a guarantee - the mailbox itself is what proves it - but a server
/// that does not make the claim certainly cannot.
pub async fn advertises_qr(homeserver_url: &str) -> bool {
    let url = format!("{}/_matrix/client/versions", homeserver_url.trim_end_matches('/'));
    match http::get_json_anonymous(&url).await {
        Ok(versions) => versions["unstable_features"]["org.matrix.msc4108"].as_bool().unwrap_or(false),
        Err(_) => false,
    }
}

fn failure(reason: &str) -> Value {
    json!({ "type": "m.login.failure", "reason": reason })
}

fn qr_path(login_id: &str) -> std::path::PathBuf {
    crate::media_cache::transient_dir().join(format!("moho-matrix-qr-{}.png", login_id.replace(|c: char| !c.is_ascii_alphanumeric(), "-")))
}

/// The code as an image. Binary data in the QR code's byte mode, which is
/// what the scanning side reads.
fn write_qr(bytes: &[u8], path: &std::path::Path) -> Result<()> {
    let code = qrcode::QrCode::new(bytes)?;
    let image = code.render::<image::Luma<u8>>().min_dimensions(300, 300).build();
    image.save_with_format(path, image::ImageFormat::Png).context("encoding the QR code as PNG")?;
    // The code is a credential for as long as it is shown.
    let _ = crate::secure::restrict_file_to_owner(path);
    Ok(())
}

/// The MSC4108 rendezvous session: one mailbox, written and read over HTTP,
/// each write conditional on the last version seen.
struct Mailbox {
    client: reqwest::Client,
    url: String,
    etag: String,
}

impl Mailbox {
    async fn open(homeserver_url: &str) -> Result<Self> {
        let client = http::anonymous_client();
        let endpoint = format!("{}/_matrix/client/unstable/org.matrix.msc4108/rendezvous", homeserver_url.trim_end_matches('/'));
        let resp = client
            .post(&endpoint)
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            // Said outright: reqwest sends an empty body with no length at
            // all, and Synapse answers that with 400 rather than a mailbox.
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .body("")
            .send()
            .await
            .context("asking the homeserver for a sign-in mailbox")?;
        let status = resp.status();
        if matches!(status.as_u16(), 404 | 405) {
            bail!("this homeserver does not offer sign-in by QR code");
        }
        if !status.is_success() {
            bail!("the homeserver refused a sign-in mailbox: HTTP {status}");
        }
        let etag = header(&resp, reqwest::header::ETAG).context("the mailbox came without an ETag")?;
        let body: Value = resp.json().await.context("reading the mailbox's address")?;
        let url = body["url"].as_str().context("the homeserver gave no mailbox address")?.to_string();
        Ok(Self { client, url, etag })
    }

    /// The next message the other device writes.
    async fn receive(&mut self, within: Duration) -> Result<String> {
        match self.receive_or_expire(within).await? {
            Some(message) => Ok(message),
            None => bail!("the sign-in mailbox expired"),
        }
    }

    /// The same, saying `None` when the mailbox has expired rather than
    /// failing - which is what an unscanned code's mailbox does, after a
    /// minute on Synapse, and is answered with a fresh one.
    async fn receive_or_expire(&mut self, within: Duration) -> Result<Option<String>> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if tokio::time::Instant::now() >= deadline {
                bail!("nothing arrived in time");
            }
            let resp = self
                .client
                .get(&self.url)
                .header(reqwest::header::IF_NONE_MATCH, &self.etag)
                .send()
                .await
                .context("reading the sign-in mailbox")?;
            let status = resp.status().as_u16();
            if let Some(etag) = header(&resp, reqwest::header::ETAG) {
                self.etag = etag;
            }
            match status {
                304 => tokio::time::sleep(POLL_EVERY).await,
                200 => {
                    let body = resp.text().await.unwrap_or_default();
                    if body.is_empty() {
                        tokio::time::sleep(POLL_EVERY).await;
                        continue;
                    }
                    return Ok(Some(body));
                }
                404 => return Ok(None),
                other => bail!("the sign-in mailbox answered HTTP {other}"),
            }
        }
    }

    async fn send(&mut self, body: String) -> Result<()> {
        let resp = self
            .client
            .put(&self.url)
            .header(reqwest::header::IF_MATCH, &self.etag)
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(body)
            .send()
            .await
            .context("writing to the sign-in mailbox")?;
        if !resp.status().is_success() {
            bail!("the sign-in mailbox refused a message: HTTP {}", resp.status());
        }
        self.etag = header(&resp, reqwest::header::ETAG).context("the mailbox answered without an ETag")?;
        Ok(())
    }
}

fn header(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// The mailbox, with everything sealed for the other device.
struct Channel {
    mailbox: Mailbox,
    ecies: EstablishedEcies,
}

impl Channel {
    async fn send_text(&mut self, text: &str) -> Result<()> {
        let sealed = self.ecies.encrypt(text.as_bytes()).encode();
        self.mailbox.send(sealed).await
    }

    async fn send_json(&mut self, value: Value) -> Result<()> {
        self.send_text(&value.to_string()).await
    }

    async fn receive_json(&mut self, within: Duration) -> Result<Value> {
        let sealed = self.mailbox.receive(within).await?;
        let message = Message::decode(&sealed).context("a sign-in message that is not sealed")?;
        let plain = self.ecies.decrypt(&message).context("a sign-in message that would not open")?;
        serde_json::from_slice(&plain).context("a sign-in message that is not JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk_crypto::types::qr_login::QrCodeIntent;

    /// What the phone reads off the screen must come back as what was put in.
    #[test]
    fn the_code_carries_the_mailbox_and_the_key() {
        let ecies = Ecies::new();
        let mailbox = url::Url::parse("https://matrix.example/_matrix/client/unstable/org.matrix.msc4108/rendezvous/abc").unwrap();
        let qr = QrCodeData::new_msc4108(ecies.public_key(), mailbox.clone(), Msc4108IntentData::Login);
        let read = QrCodeData::from_bytes(&qr.to_bytes()).expect("the code reads back");
        assert_eq!(read.public_key(), ecies.public_key());
        assert_eq!(read.intent(), QrCodeIntent::Login);
    }

    /// Both ends of the channel agree on the two digits, and a phone that
    /// opens the channel with the initiate message is understood.
    #[test]
    fn the_channel_opens_and_both_ends_show_the_same_digits() {
        let moho = Ecies::new();
        let moho_key = moho.public_key();
        let phone = Ecies::new().establish_outbound_channel(moho_key, LOGIN_INITIATE.as_bytes()).unwrap();
        let inbound = moho.establish_inbound_channel(&InitialMessage::decode(&phone.message.encode()).unwrap()).unwrap();
        assert_eq!(inbound.message, LOGIN_INITIATE.as_bytes());
        assert_eq!(inbound.ecies.check_code().to_digit(DigitMode::AllowLeadingZero), phone.ecies.check_code().to_digit(DigitMode::AllowLeadingZero));

        let (mut moho, mut phone) = (inbound.ecies, phone.ecies);
        let sealed = moho.encrypt(LOGIN_OK.as_bytes()).encode();
        assert_eq!(phone.decrypt(&Message::decode(&sealed).unwrap()).unwrap(), LOGIN_OK.as_bytes());
    }

    /// The secrets message is the bundle with a `type` beside it.
    #[test]
    fn the_secrets_message_reads_as_a_bundle() {
        let message = json!({
            "type": "m.login.secrets",
            "cross_signing": {
                "master_key": "bMnVpkHI4S2wXRxy+IpaKM5PIAUUkl6DE+n0YLIW/qs",
                "user_signing_key": "8tlgLjUrrb/zGJo4YKGhDTIDCEjtJTAS/Sh2AGNLuIo",
                "self_signing_key": "pfDknmP5a0fVVRE54zhkUgJfzbNmvKcNfIWEW796bQs"
            },
            "backup": {
                "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
                "key": "bYYv3aFLQ49jMNcOjuTtBY9EKDby2x1m3gfX81nIKRQ",
                "backup_version": "9"
            }
        });
        let bundle: SecretsBundle = serde_json::from_value(message).expect("parses");
        assert!(bundle.backup.is_some());
    }
}
