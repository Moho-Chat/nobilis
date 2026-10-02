//! Getting a token, by either of the two doors that work.
//!
//! A QR code scanned with a phone, or a token from Discord's own login page
//! signed into in a browser window - which is where a password goes, and
//! where its captcha, two-factor and device checks are Discord's own to run.
//! Both end in the same place - `finish_login` - because what the rest of
//! this backend wants is a token and an account, and it does not care which
//! door it came through.
//!
//! There is no third door. Posting a password at `/auth/login` from here was
//! tried and removed: Discord answers a third-party client with a captcha it
//! cannot render or a flat refusal, and files each attempt against the
//! account's standing.

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

pub(super) async fn run_qr_login(state: &AppState, login_id: &str, reauth_account_id: Option<String>) -> Result<()> {
    let mut request = REMOTE_AUTH_URL.into_client_request()?;
    // The remote-auth gateway rejects the handshake outright without an
    // Origin it recognizes - confirmed in every working reference client.
    request.headers_mut().insert("Origin", HeaderValue::from_static("https://discord.com"));
    let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, crate::net::route::websocket(&crate::net::route::pending_key("discord"), request))
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
        anonymous_client()
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

/// Shared tail of both login routes (QR and browser): identify
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
    let me: Value = http_client_for(&token)
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

    let use_tor = crate::net::route::router().wanted(&crate::net::route::pending_key("discord"));
    let config = DiscordAccountConfig { user_id, username, display_name: None, token, avatar_url, use_tor };
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
