//! Getting a token, by any of the three doors Discord opens.
//!
//! A QR code scanned with a phone, a password with the MFA challenge that
//! usually follows it, or a token somebody already has. All three end in the
//! same place - `finish_login` - because what the rest of this backend wants
//! is a token and an account, and it does not care which door it came
//! through.

use super::*;

pub(super) const REMOTE_AUTH_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";

/// Kicks off a QR login attempt in the background. Progress is reported
/// entirely via broadcast events (discordLoginQr/discordLoginScanned/
/// discordLoginResult, all tagged with `loginId`) rather than the RPC
/// response, since the flow is inherently multi-step and asynchronous - the
/// RPC call that triggers this just returns the loginId immediately (see
/// rpc/methods.rs's addDiscordAccount).
pub fn start_qr_login(state: AppState, login_id: String, reauth_account_id: Option<String>) {
    tokio::spawn(async move {
        // Same catch_unwind safety net as backend::irc::spawn/spawn below -
        // without it, a bug anywhere in this multi-step flow leaves the
        // frontend's QR form waiting forever with no discordLoginResult
        // ever coming back, since a bare panic in a spawned task just
        // vanishes rather than reaching the emit() below.
        let result =
            std::panic::AssertUnwindSafe(run_qr_login(&state, &login_id, reauth_account_id)).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("discord qr login[{login_id}]: {error}");
        state.events.emit("discordLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

/// A password login that stopped at the two-factor step, waiting for a code.
/// Discord hands back a short-lived `ticket` that stands in for the password
/// on the follow-up request, so this is what has to survive between the two
/// RPCs.
pub(super) struct PendingMfa {
    ticket: String,
    reauth_account_id: Option<String>,
}

pub(super) fn pending_mfa() -> &'static std::sync::Mutex<std::collections::HashMap<String, PendingMfa>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, PendingMfa>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// Username/password login, as an alternative to scanning a QR code.
///
/// Discord may answer with any of three things rather than a token: a
/// two-factor challenge (handled by submit_mfa_code below), a captcha, or a
/// plain rejection. The captcha case is reported as such and cannot be worked
/// around from here - QR login is the way through it, since approving on an
/// already-signed-in device is exactly the proof the captcha is asking for.
pub fn start_password_login(
    state: AppState,
    login_id: String,
    login: String,
    password: String,
    reauth_account_id: Option<String>,
) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(run_password_login(
            &state,
            &login_id,
            &login,
            &password,
            reauth_account_id,
        ))
        .catch_unwind()
        .await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("discord password login[{login_id}]: {error}");
        state.events.emit("discordLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

pub(super) async fn run_password_login(
    state: &AppState,
    login_id: &str,
    login: &str,
    password: &str,
    reauth_account_id: Option<String>,
) -> Result<()> {
    state.events.emit("discordLoginStatus", json!({ "loginId": login_id, "detail": "signing in..." }));
    let resp: Value = http_client()
        .post(format!("{API_BASE}/auth/login"))
        // The full shape the endpoint declares, not the three fields that
        // happen to be interesting. Both of the nulls are meaningful absences
        // - no gift code was redeemed on the way in, and the sign-in came from
        // nowhere in particular - and omitting a declared field entirely is
        // one of the things answered with "Invalid Form Body".
        .json(&json!({
            "login": login,
            "password": password,
            "undelete": false,
            "login_source": Value::Null,
            "gift_code_sku_id": Value::Null
        }))
        .send()
        .await
        .context("sending login request")?
        .json()
        .await
        .context("parsing login response")?;

    handle_login_response(state, login_id, resp, reauth_account_id).await
}

/// Both `/auth/login` and `/auth/mfa/totp` answer with the same shape, so one
/// place decides what happened.
pub(super) async fn handle_login_response(
    state: &AppState,
    login_id: &str,
    resp: Value,
    reauth_account_id: Option<String>,
) -> Result<()> {
    if let Some(token) = resp.get("token").and_then(|v| v.as_str()) {
        return finish_login(state, login_id, token.to_string(), reauth_account_id).await;
    }

    // A captcha is a hard stop: solving one is exactly the automated
    // circumvention Discord puts it there to prevent, so this reports it
    // plainly and points at the route that does work.
    if resp.get("captcha_key").is_some() {
        bail!("Discord asked for a captcha, which can't be answered from here - use QR login instead");
    }

    if resp.get("mfa").and_then(|v| v.as_bool()).unwrap_or(false) {
        let ticket = resp
            .get("ticket")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("two-factor challenge without a ticket"))?
            .to_string();
        pending_mfa()
            .lock()
            .unwrap()
            .insert(login_id.to_string(), PendingMfa { ticket, reauth_account_id });
        // Report which factors this account actually has, so a frontend can
        // ask for the right thing rather than always saying "authenticator".
        state.events.emit(
            "discordLoginMfa",
            json!({
                "loginId": login_id,
                "totp": resp.get("totp").and_then(|v| v.as_bool()).unwrap_or(true),
                "sms": resp.get("sms").and_then(|v| v.as_bool()).unwrap_or(false),
                "backup": resp.get("backup").and_then(|v| v.as_bool()).unwrap_or(false),
            }),
        );
        return Ok(());
    }

    // Discord's own wording is the most useful thing to show here.
    let message = resp
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Discord rejected the login and gave no reason");

    // "Invalid Form Body" with nothing wrong in the body is Discord declining
    // to accept a password from a client it does not recognise, rather than a
    // complaint about what was typed. Worth saying plainly and at length,
    // because the obvious response to a vague rejection is to try again, and
    // trying again is what actually costs something: each failed sign-in
    // raises the account's risk score, which is where the warnings on the
    // account come from. This is a dead end, so it says so instead of looking
    // like a typo that another attempt might fix.
    if resp.get("code").and_then(|c| c.as_u64()) == Some(50035) {
        let detail = form_errors(&resp)
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        bail!(
            "Discord refused the sign-in{detail}. It generally will not accept a password from a \
             third-party client, and each attempt counts against the account - which is where the \
             warnings on it are coming from. Use QR login instead: approving on a device already \
             signed in is the proof Discord is actually asking for, and it does not put the \
             account at risk."
        );
    }

    if let Some(detail) = form_errors(&resp) {
        bail!("{message} ({detail})");
    }
    bail!("{message}")
}

/// Second half of a two-factor login: exchange the stored ticket plus the
/// code the user typed for a real token. Accepts an authenticator code or a
/// backup code - Discord takes both on this endpoint.
pub fn submit_mfa_code(state: AppState, login_id: String, code: String) {
    tokio::spawn(async move {
        let result =
            std::panic::AssertUnwindSafe(run_mfa_submit(&state, &login_id, &code)).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("discord mfa[{login_id}]: {error}");
        state.events.emit("discordLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

pub(super) async fn run_mfa_submit(state: &AppState, login_id: &str, code: &str) -> Result<()> {
    let pending = pending_mfa()
        .lock()
        .unwrap()
        .remove(login_id)
        .ok_or_else(|| anyhow!("that login is no longer waiting for a code - start again"))?;

    state.events.emit("discordLoginStatus", json!({ "loginId": login_id, "detail": "checking code..." }));
    let resp: Value = http_client()
        .post(format!("{API_BASE}/auth/mfa/totp"))
        // Codes are commonly pasted with a space in the middle from an
        // authenticator app's own display.
        .json(&json!({ "code": code.replace(char::is_whitespace, ""), "ticket": pending.ticket }))
        .send()
        .await
        .context("sending two-factor code")?
        .json()
        .await
        .context("parsing two-factor response")?;

    // A wrong code comes back as an ordinary rejection; the ticket is spent
    // either way, so the flow restarts rather than silently retrying.
    handle_login_response(state, login_id, resp, pending.reauth_account_id).await
}

pub(super) async fn run_qr_login(state: &AppState, login_id: &str, reauth_account_id: Option<String>) -> Result<()> {
    let mut request = REMOTE_AUTH_URL.into_client_request()?;
    // The remote-auth gateway rejects the handshake outright without an
    // Origin it recognizes - confirmed in every working reference client.
    request.headers_mut().insert("Origin", HeaderValue::from_static("https://discord.com"));
    let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| anyhow!("timed out connecting to Discord's remote-auth gateway"))?
        .context("connecting to Discord's remote-auth gateway")?;
    let (sink, mut stream) = ws.split();

    // A single background task owns the sink so both the read loop below
    // and the heartbeat ticker can send frames without fighting over a
    // shared &mut - the standard split-plus-forwarder pattern for
    // tungstenite when a connection needs to both read continuously and
    // write on its own timer.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let forwarder = tokio::spawn(async move {
        let mut sink = sink;
        while let Some(text) = out_rx.recv().await {
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _forwarder_guard = AbortOnDrop(forwarder);

    // Scoped so the (non-Send) ThreadRng is dropped before the next
    // `.await` - otherwise it poisons this whole async fn's Send-ness,
    // since tokio::spawn requires the outer future to be Send.
    let priv_key = {
        let mut rng = rand::thread_rng();
        RsaPrivateKey::new(&mut rng, 2048).context("generating RSA keypair")?
    };
    let pub_key = RsaPublicKey::from(&priv_key);
    let pub_der = pub_key.to_public_key_der().context("encoding public key")?;
    let encoded_public_key = STANDARD.encode(pub_der.as_bytes());

    // Rendered to a temp file rather than embedded as a base64 data URI in
    // the event itself - keeps every line on the wire small regardless of
    // transport quirks, and QML's Image element loads a file:// URL just as
    // directly. Cleaned up on every exit path (success/error/panic) via
    // this drop guard, including the panic case: unwinding still runs
    // destructors for values already on the stack.
    let qr_path = std::env::temp_dir().join(format!("nobilis-discord-qr-{login_id}.png"));
    struct RemoveOnDrop(std::path::PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _qr_cleanup = RemoveOnDrop(qr_path.clone());

    let hello = next_json(&mut stream).await.context("waiting for hello")?;
    let timeout_ms = hello.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(150_000);
    let heartbeat_interval = hello.get("heartbeat_interval").and_then(|v| v.as_u64()).unwrap_or(41_250).max(1);

    let hb_tx = out_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(rand::random::<u64>() % heartbeat_interval)).await;
        let mut ticker = tokio::time::interval(Duration::from_millis(heartbeat_interval));
        loop {
            ticker.tick().await;
            if hb_tx.send(json!({ "op": "heartbeat" }).to_string()).is_err() {
                break;
            }
        }
    });
    let _heartbeat_guard = AbortOnDrop(heartbeat_task);

    out_tx.send(json!({ "op": "init", "encoded_public_key": encoded_public_key }).to_string())?;

    let flow = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            let msg = next_json(&mut stream).await?;
            let op = msg.get("op").and_then(|v| v.as_str()).unwrap_or("");
            match op {
                "nonce_proof" => {
                    let encrypted = msg["encrypted_nonce"].as_str().ok_or_else(|| anyhow!("nonce_proof missing encrypted_nonce"))?;
                    let ciphertext = STANDARD.decode(encrypted).context("decoding encrypted_nonce")?;
                    let nonce = priv_key.decrypt(Oaep::new::<Sha256>(), &ciphertext).context("decrypting nonce")?;
                    let mut hasher = Sha256::new();
                    hasher.update(&nonce);
                    let proof = URL_SAFE_NO_PAD.encode(hasher.finalize());
                    out_tx.send(json!({ "op": "nonce_proof", "proof": proof }).to_string())?;
                }
                "pending_remote_init" => {
                    let fingerprint = msg["fingerprint"].as_str().ok_or_else(|| anyhow!("pending_remote_init missing fingerprint"))?;
                    let url = format!("https://discord.com/ra/{fingerprint}");
                    write_qr_file(&url, &qr_path).context("rendering QR code")?;
                    state.events.emit(
                        "discordLoginQr",
                        json!({ "loginId": login_id, "qrCodePath": qr_path.to_string_lossy(), "url": url }),
                    );
                }
                "pending_ticket" => {
                    let encrypted = msg["encrypted_user_payload"]
                        .as_str()
                        .ok_or_else(|| anyhow!("pending_ticket missing encrypted_user_payload"))?;
                    let ciphertext = STANDARD.decode(encrypted).context("decoding encrypted_user_payload")?;
                    let plaintext = priv_key.decrypt(Oaep::new::<Sha256>(), &ciphertext).context("decrypting user payload")?;
                    let text = String::from_utf8_lossy(&plaintext);
                    // "user_id:discriminator:avatar_hash:username"
                    let username = text.split(':').nth(3).unwrap_or("someone").to_string();
                    state.events.emit("discordLoginScanned", json!({ "loginId": login_id, "username": username }));
                }
                "pending_login" => {
                    let ticket = msg["ticket"].as_str().ok_or_else(|| anyhow!("pending_login missing ticket"))?.to_string();
                    return Ok(ticket);
                }
                "cancel" => bail!("login was cancelled on your device"),
                "heartbeat_ack" => {}
                other => tracing::debug!("discord qr login: unhandled op {other:?}"),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("QR code expired - open the form again for a new one"))??;

    let ticket = flow;
    let resp: Value = send_write(
        http_client()
        .post("https://discord.com/api/v9/users/@me/remote-auth/login")
        .json(&json!({ "ticket": ticket }))
        )
    .await
        .context("exchanging ticket for token")?
        .json()
        .await
        .context("parsing token exchange response")?;
    let encrypted_token = resp
        .get("encrypted_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("token exchange response missing encrypted_token (captcha challenge?)"))?;
    let ciphertext = STANDARD.decode(encrypted_token).context("decoding encrypted_token")?;
    let token_bytes = priv_key.decrypt(Oaep::new::<Sha256>(), &ciphertext).context("decrypting token")?;
    let token = String::from_utf8(token_bytes).context("token was not valid UTF-8")?;

    finish_login(state, login_id, token, reauth_account_id).await
}

/// Shared tail of every login route (QR, password, password+MFA): identify
/// who the token belongs to, save it, and connect.
///
/// `reauth_account_id` is set when refreshing an existing account's token
/// rather than adding a new one. The account store keys Discord accounts by
/// user id and upserts, so a matching re-auth naturally replaces the dead
/// token - but logging into a *different* account from a re-auth prompt would
/// silently add a second account instead of fixing the one the user asked
/// about, so that mismatch is refused.
/// Signs in with a token a frontend obtained from Discord's own login page.
///
/// The way in that actually works. Posting credentials at `/auth/login` is met
/// with a captcha this cannot render, a device check it cannot answer, and a
/// warning filed against the account for having tried - so a frontend that can
/// open a browser window lets Discord's page handle all of it and brings back
/// only the result.
///
/// Nothing is trusted about the token beyond its shape: `finish_login` spends
/// it on a profile request straight away, and a token that is not one fails
/// there rather than being written to the account file.
pub async fn finish_token_login(
    state: &AppState,
    login_id: &str,
    token: String,
    reauth_account_id: Option<String>,
) -> Result<()> {
    if token.trim().is_empty() {
        bail!("the sign-in window returned an empty token");
    }
    finish_login(state, login_id, token, reauth_account_id).await
}

pub(super) async fn finish_login(
    state: &AppState,
    login_id: &str,
    token: String,
    reauth_account_id: Option<String>,
) -> Result<()> {
    let me: Value = http_client()
        .get(format!("{API_BASE}/users/@me"))
        .header("Authorization", &token)
        .send()
        .await
        .context("fetching Discord profile")?
        .json()
        .await
        .context("parsing Discord profile response")?;
    let user_id = me.get("id").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("profile response missing id"))?.to_string();
    let username = me
        .get("global_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| me.get("username").and_then(|v| v.as_str()))
        .unwrap_or("Discord user")
        .to_string();
    let avatar_url = me
        .get("avatar")
        .and_then(|v| v.as_str())
        .map(|hash| format!("https://cdn.discordapp.com/avatars/{user_id}/{hash}.png"));

    let config = DiscordAccountConfig { user_id, username, display_name: None, token, avatar_url };
    if let Some(expected) = reauth_account_id {
        if config.account_id() != expected {
            bail!("that is a different Discord account - re-authenticating {expected} needs the same account");
        }
    }
    let saved = state.accounts.add_discord(config)?;
    let account = crate::accounts::discord_account_to_json(&saved, "connecting");
    // spawn() resets any existing connection for this account first, so a
    // re-auth replaces the dead session rather than racing it.
    spawn(state.clone(), saved);

    state.events.emit("discordLoginResult", json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}

pub(super) fn write_qr_file(url: &str, path: &std::path::Path) -> Result<()> {
    let code = qrcode::QrCode::new(url.as_bytes())?;
    let image = code.render::<image::Luma<u8>>().min_dimensions(300, 300).build();
    image.save_with_format(path, image::ImageFormat::Png).context("encoding QR code as PNG")?;
    // Owner-only - the fingerprint it encodes is a short-lived credential
    // (whoever completes the scan-and-approve flow against it gets a login
    // ticket), same spirit as accounts.toml's 0600 permissions.
    let _ = crate::secure::restrict_file_to_owner(path);
    Ok(())
}

#[cfg(test)]
mod login_tests {
    use super::form_errors;
    use serde_json::json;

    /// "Invalid Form Body" alone is unactionable; the field underneath it is
    /// the only part worth reading.
    #[test]
    fn a_field_complaint_is_pulled_out_of_the_rejection() {
        let resp = json!({
            "code": 50035,
            "message": "Invalid Form Body",
            "errors": { "login": { "_errors": [{ "code": "BASE_TYPE_REQUIRED", "message": "This field is required" }] } }
        });
        assert_eq!(form_errors(&resp).as_deref(), Some("login: This field is required"));
    }

    /// Discord answers some rejections with no `errors` map at all, and an
    /// empty parenthetical appended to the message reads like a bug.
    #[test]
    fn a_rejection_with_no_field_detail_reports_nothing() {
        assert!(form_errors(&json!({ "code": 50035, "message": "Invalid Form Body" })).is_none());
        assert!(form_errors(&json!({ "message": "Login or password is invalid." })).is_none());
    }

    /// A field named with no reason given still names the field.
    #[test]
    fn a_field_with_no_message_is_still_named() {
        let resp = json!({ "errors": { "password": {} } });
        assert_eq!(form_errors(&resp).as_deref(), Some("password"));
    }
}
