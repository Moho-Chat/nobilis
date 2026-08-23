//! Discord backend: QR-code remote-auth login (`start_qr_login`) plus a
//! minimal real-time gateway client (`spawn`) once an account has a token.
//!
//! The QR flow is Discord's own official cross-device login mechanism (the
//! same one discord.com/app and the desktop client offer for "log in by
//! scanning with your phone") - reverse-engineered but stable, documented at
//! <https://docs.discord.food/remote-authentication/desktop>. Every value
//! encrypted anywhere in this flow (the nonce, the scanning user's identity,
//! and the final token) uses the same scheme: RSA-2048-OAEP-SHA256 against a
//! keypair generated fresh per login attempt, no symmetric layer involved.
//!
//! Auto-reconnects with backoff on any dropped/errored/zombied gateway
//! session (see run_gateway_with_retry) - unlike backend/irc.rs, which
//! still has no auto-reconnect and deliberately reports "disconnected" on
//! any drop, Discord's gateway went silently zombie in practice (socket
//! technically alive, no dispatches ever arriving again, no error to
//! surface) often enough in a long-running session that a human had to
//! notice and manually reconnect every time - not viable long-term.
use crate::accounts::DiscordAccountConfig;
use crate::model::{self, Attachment, Embed, Reaction, ReplyPreview};
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use futures::{FutureExt, SinkExt, StreamExt};
use rsa::pkcs8::EncodePublicKey;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const REMOTE_AUTH_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const API_BASE: &str = "https://discord.com/api/v10";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

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
struct PendingMfa {
    ticket: String,
    reauth_account_id: Option<String>,
}

fn pending_mfa() -> &'static std::sync::Mutex<std::collections::HashMap<String, PendingMfa>> {
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

async fn run_password_login(
    state: &AppState,
    login_id: &str,
    login: &str,
    password: &str,
    reauth_account_id: Option<String>,
) -> Result<()> {
    state.events.emit("discordLoginStatus", json!({ "loginId": login_id, "detail": "signing in..." }));
    let resp: Value = http_client()
        .post(format!("{API_BASE}/auth/login"))
        .json(&json!({ "login": login, "password": password, "undelete": false }))
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
async fn handle_login_response(
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

async fn run_mfa_submit(state: &AppState, login_id: &str, code: &str) -> Result<()> {
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

async fn run_qr_login(state: &AppState, login_id: &str, reauth_account_id: Option<String>) -> Result<()> {
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
    let resp: Value = http_client()
        .post("https://discord.com/api/v9/users/@me/remote-auth/login")
        .json(&json!({ "ticket": ticket }))
        .send()
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
async fn finish_login(
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

fn write_qr_file(url: &str, path: &std::path::Path) -> Result<()> {
    let code = qrcode::QrCode::new(url.as_bytes())?;
    let image = code.render::<image::Luma<u8>>().min_dimensions(300, 300).build();
    image.save_with_format(path, image::ImageFormat::Png).context("encoding QR code as PNG")?;
    // Owner-only - the fingerprint it encodes is a short-lived credential
    // (whoever completes the scan-and-approve flow against it gets a login
    // ticket), same spirit as accounts.toml's 0600 permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Shared by both READY's `private_channels` array and the REST /users/@me/
/// channels follow-up fetch (see run_gateway's READY handling) - both hand
/// this the exact same per-channel JSON shape. Returns the new (bufferId,
/// channelId) pair when this channel is genuinely new (so the caller can
/// queue it for history backfill), None if already known.
fn register_dm_channel(
    state: &AppState,
    account_id: &str,
    ch: &Value,
    channel_map: &mut HashMap<String, (String, String)>,
    presences: &HashMap<&str, &str>,
) -> Option<(String, String)> {
    let channel_id = ch["id"].as_str()?;
    if channel_map.contains_key(channel_id) {
        return None;
    }
    let name = dm_channel_name(ch);
    let buf = state.runtime.ensure_buffer(state, account_id, &name, "dm");
    state.runtime.set_discord_channel(&buf.id, channel_id);
    if let Some(avatar) = dm_avatar_url(ch) {
        state.runtime.set_buffer_avatar(state, &buf.id, &avatar);
    }
    // A DM has no member list to subscribe to - the participants are right
    // here in the channel object, and they are the whole roster.
    set_dm_presence(state, &buf.id, ch, presences);
    ensure_dm_group(state, account_id);
    state.runtime.set_buffer_group(state, &buf.id, &dm_group_id(account_id));
    channel_map.insert(channel_id.to_string(), (name, "dm".to_string()));
    Some((buf.id, channel_id.to_string()))
}

/// The picture to show for a direct message: the other person's.
///
/// A DM's `recipients` array holds everyone except this account, so for a
/// one-to-one conversation it is the one person there is. A group DM has
/// several and no single face to show, which is why this takes the first only
/// when there is exactly one.
fn dm_avatar_url(ch: &Value) -> Option<String> {
    let recipients = ch["recipients"].as_array()?;
    if recipients.len() != 1 {
        return None;
    }
    author_avatar_url(&recipients[0]).or_else(|| default_avatar_url(&recipients[0]))
}

/// The picture Discord serves for someone who has never set one.
///
/// Worth resolving rather than falling back to a coloured initial the way
/// message avatars do: a conversation list is a list of faces, and the one
/// entry showing a letter instead reads as broken rather than as a person
/// with no picture. Which of the six is theirs depends on which username
/// scheme they are on - the modern one has no discriminator and derives it
/// from the account id instead.
fn default_avatar_url(user: &Value) -> Option<String> {
    let id = user["id"].as_str()?;
    let index = match user["discriminator"].as_str() {
        Some(d) if d != "0" => d.parse::<u64>().unwrap_or(0) % 5,
        _ => (id.parse::<u64>().ok()? >> 22) % 6,
    };
    Some(format!("https://cdn.discordapp.com/embed/avatars/{index}.png"))
}

/// A DM channel's display name, derived from its `recipients` array (their
/// display name if set, else username) - shared by register_dm_channel
/// above and open_dm below, the two places a raw Discord channel object
/// needs turning into a buffer name.
fn dm_channel_name(ch: &Value) -> String {
    ch["recipients"]
        .as_array()
        .filter(|r| !r.is_empty())
        .map(|r| {
            r.iter()
                .filter_map(|u| u["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| u["username"].as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Accepts a real Discord invite - the same "join a server" action any
/// user client offers, not a bot-only capability (this backend always
/// runs on a real user token - see this module's own doc comment). `invite`
/// may be a bare code or a full discord.gg/xxx (or discord.com/invite/xxx)
/// URL; only the trailing path segment (the actual code) is ever sent.
pub async fn join_guild(state: &AppState, account_id: &str, invite: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let code = invite.trim().trim_end_matches('/').rsplit('/').next().unwrap_or(invite.trim());
    let resp = http_client()
        .post(format!("{API_BASE}/invites/{code}"))
        .header("Authorization", &cfg.token)
        .json(&json!({}))
        .send()
        .await
        .context("accepting Discord invite")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Starts (or reuses - Discord's own API dedups this server-side) a DM
/// with `target_user_id`, the real numeric snowflake rather than a
/// username: this backend has no guild-wide member search to look one up
/// by name (a real REST API restriction for a user-token client, not a
/// missing feature here - see this module's own userlist-investigation
/// history), so getting someone's id the same way any Discord client
/// requires for a non-contact (right-click their profile -> Copy User ID,
/// Developer Mode on) is unavoidable. Returns the resulting buffer id.
pub async fn open_dm(state: &AppState, account_id: &str, target_user_id: &str) -> Result<String> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = http_client()
        .post(format!("{API_BASE}/users/@me/channels"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "recipient_id": target_user_id }))
        .send()
        .await
        .context("opening Discord DM")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    let ch: Value = resp.json().await.context("invalid JSON response")?;
    let channel_id = ch["id"].as_str().context("no channel id in response")?;
    let name = dm_channel_name(&ch);
    let buffer = state.runtime.ensure_buffer(state, account_id, &name, "dm");
    state.runtime.set_discord_channel(&buffer.id, channel_id);
    if let Some(avatar) = dm_avatar_url(&ch) {
        state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
    }
    // Opened by hand rather than from READY, so there is no presence snapshot
    // to seed from; their first status update fills it in.
    set_dm_presence(state, &buffer.id, &ch, &HashMap::new());
    ensure_dm_group(state, account_id);
    state.runtime.set_buffer_group(state, &buffer.id, &dm_group_id(account_id));
    Ok(buffer.id)
}

/// Sends a friend request - a pending request the target still has to
/// accept, exactly like clicking "Add Friend" in any real Discord client
/// (the request never completes to a full friendship synchronously here).
/// `username` accepts either a modern unique username or a legacy
/// `name#1234` pair; the discriminator half only still means anything for
/// accounts that never migrated off the old system.
pub async fn add_friend(state: &AppState, account_id: &str, username: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let (name, discriminator) = match username.trim().rsplit_once('#') {
        Some((n, d)) if d.chars().all(|c| c.is_ascii_digit()) && !d.is_empty() => (n, Some(d)),
        _ => (username.trim(), None),
    };
    let resp = http_client()
        .post(format!("{API_BASE}/users/@me/relationships"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "username": name, "discriminator": discriminator }))
        .send()
        .await
        .context("sending Discord friend request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Creates a brand-new guild owned by this account - Discord's own "Create
/// My Own" server flow, same endpoint real clients use. Discord auto-
/// creates a default #general channel; the gateway's own GUILD_CREATE
/// dispatch for it (handled above in run_gateway) is what actually turns
/// it into a buffer, so nothing else is needed here.
pub async fn create_guild(state: &AppState, account_id: &str, name: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = http_client()
        .post(format!("{API_BASE}/guilds"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "name": name.trim() }))
        .send()
        .await
        .context("creating Discord guild")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Builds a `https://discord.com/channels/...` deep link to a specific
/// message - the fallback for an attachment link whose signature has
/// expired (see runtime.rs's discord_guild_id doc comment for why: every
/// `cdn.discordapp.com`/`media.discordapp.net` link Discord hands out is
/// signed with an `ex=`/`is=`/`hm=` query string that lapses roughly 24h
/// after issue, and Discord's own `/attachments/refresh-urls` endpoint -
/// which a real client uses to silently re-sign one on demand - hard-
/// rejects this backend's user-token requests with a blanket 401 even for
/// a URL that hasn't expired yet, confirmed live against both a genuinely
/// expired and a genuinely fresh URL; it isn't a missing-header problem,
/// it's a client fingerprint gate this backend has no way to pass). Opening
/// the real message in an actual Discord client instead always works,
/// since that client gets its own freshly-signed link. `guild_id` is
/// `None` for a DM (no guild at all - `@me` is Discord's own link
/// convention there).
pub fn message_link(channel_id: &str, guild_id: Option<&str>, message_id: &str) -> String {
    format!("https://discord.com/channels/{}/{channel_id}/{message_id}", guild_id.unwrap_or("@me"))
}

const PERM_ADMINISTRATOR: u64 = 1 << 3;
const PERM_VIEW_CHANNEL: u64 = 1 << 10;

fn parse_perm(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
}

/// This account's own role ids in `guild`. Discord conveniently includes
/// just the requesting user's own member object in a guild's `members`
/// array for user-account gateway sessions - confirmed live (a guild with
/// hundreds of real members still returned a `members` array of length 1,
/// containing exactly our own entry). The REST fallback covers whatever
/// case that isn't true for (large guilds are the only documented one,
/// though not one observed during development).
async fn own_guild_role_ids(config: &DiscordAccountConfig, guild: &Value) -> Vec<String> {
    if let Some(members) = guild["members"].as_array() {
        if let Some(me) = members.iter().find(|m| m["user"]["id"].as_str() == Some(config.user_id.as_str())) {
            return me["roles"].as_array().map(|r| r.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        }
    }
    let Some(guild_id) = guild["id"].as_str() else { return Vec::new() };
    match http_client().get(format!("{API_BASE}/guilds/{guild_id}/members/@me")).header("Authorization", &config.token).send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(member) => member["roles"].as_array().map(|r| r.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

/// Discord's documented permission-overwrite algorithm: base permissions
/// (bitwise OR of @everyone's permissions and every role the member has),
/// then the channel's @everyone overwrite, then the union of the member's
/// own role overwrites, then a member-specific overwrite if one exists -
/// each layer applied as `(perms & !deny) | allow`, in that exact order.
/// The guild owner and anyone with ADMINISTRATOR short-circuit to "sees
/// everything," bypassing overwrites entirely, same as Discord's own
/// clients - the owner bypass matters even for an otherwise-roleless
/// owner account (confirmed live: missed 3 channels in a server this
/// account owns outright before this was added).
#[allow(clippy::too_many_arguments)]
fn can_view_channel(is_owner: bool, guild_id: &str, roles: &[Value], member_role_ids: &[String], user_id: &str, channel: &Value) -> bool {
    if is_owner {
        return true;
    }
    let mut base: u64 = 0;
    for role in roles {
        let role_id = role["id"].as_str().unwrap_or("");
        if role_id == guild_id || member_role_ids.iter().any(|r| r == role_id) {
            base |= parse_perm(&role["permissions"]);
        }
    }
    if base & PERM_ADMINISTRATOR != 0 {
        return true;
    }

    let mut perms = base;
    let empty = Vec::new();
    let overwrites = channel["permission_overwrites"].as_array().unwrap_or(&empty);

    if let Some(ow) = overwrites.iter().find(|o| o["type"].as_i64() == Some(0) && o["id"].as_str() == Some(guild_id)) {
        perms = (perms & !parse_perm(&ow["deny"])) | parse_perm(&ow["allow"]);
    }

    let mut role_allow = 0u64;
    let mut role_deny = 0u64;
    for ow in overwrites {
        if ow["type"].as_i64() == Some(0) {
            let id = ow["id"].as_str().unwrap_or("");
            if id != guild_id && member_role_ids.iter().any(|r| r == id) {
                role_allow |= parse_perm(&ow["allow"]);
                role_deny |= parse_perm(&ow["deny"]);
            }
        }
    }
    perms = (perms & !role_deny) | role_allow;

    if let Some(ow) = overwrites.iter().find(|o| o["type"].as_i64() == Some(1) && o["id"].as_str() == Some(user_id)) {
        perms = (perms & !parse_perm(&ow["deny"])) | parse_perm(&ow["allow"]);
    }

    perms & PERM_VIEW_CHANNEL != 0
}

/// Shared by both READY's embedded `guilds` array and live GUILD_CREATE
/// dispatches - same per-guild JSON shape either way (id, name, channels,
/// roles, members). A guild with no `channels` array (an "unavailable"
/// stub, or a guild Discord genuinely didn't include full data for) is a
/// harmless no-op. Channels this account can't VIEW_CHANNEL are silently
/// skipped rather than turned into buffers - Discord's own channel list
/// includes every channel in the guild regardless of the requester's
/// access, so without this check, private/role-gated channels the account
/// has no business seeing would show up right alongside ones it can.
async fn register_guild_channels(state: &AppState, config: &DiscordAccountConfig, guild: &Value, channel_map: &mut HashMap<String, (String, String)>) {
    let Some(channels) = guild["channels"].as_array() else { return };
    let Some(guild_id) = guild["id"].as_str() else { return };
    let guild_name = guild["name"].as_str().unwrap_or("guild").to_string();
    let roles = guild["roles"].as_array().cloned().unwrap_or_default();
    let is_owner = guild["owner_id"].as_str() == Some(config.user_id.as_str());
    // Only worth the (possible REST) round-trip if it'll actually be used.
    let member_role_ids = if is_owner { Vec::new() } else { own_guild_role_ids(config, guild).await };
    let account_id = config.account_id();
    let mut new_channels: Vec<(String, String)> = Vec::new();

    // Type 2 is a voice channel. Recorded rather than made into a buffer -
    // there is no conversation to show - so a client can list them and join.
    let voice: Vec<(String, String, u64)> = channels
        .iter()
        .filter(|c| c["type"].as_i64() == Some(2))
        .filter_map(|c| {
            Some((
                c["id"].as_str()?.to_string(),
                c["name"].as_str().unwrap_or("voice").to_string(),
                c["user_limit"].as_u64().unwrap_or(0),
            ))
        })
        .collect();
    if !voice.is_empty() {
        state.runtime.set_discord_voice_channels(&account_id, guild_id, voice);
    }

    // Whatever the guild told us about who its people are. Recorded before
    // the voice states below, so anyone already in a channel has a name.
    for m in guild["members"].as_array().into_iter().flatten() {
        if let Some(user_id) = m["user"]["id"].as_str() {
            let nick = m["nick"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| m["user"]["global_name"].as_str().filter(|s| !s.is_empty()))
                .or_else(|| m["user"]["username"].as_str());
            if let Some(nick) = nick {
                state.runtime.remember_discord_name(&account_id, user_id, nick);
            }
        }
    }

    // Who is already in them. This arrives once, with the guild; everything
    // after is VOICE_STATE_UPDATE.
    for vs in guild["voice_states"].as_array().into_iter().flatten() {
        if let (Some(user_id), Some(channel_id)) = (vs["user_id"].as_str(), vs["channel_id"].as_str()) {
            state.runtime.set_discord_voice_state(&account_id, user_id, Some(channel_id), voice_member_name(vs));
        }
    }

    // The guild's own rail entry. Registered before its channels so a
    // frontend never briefly sees a buffer pointing at a group it has not
    // heard of. Discord's `position` is the order the user themselves put
    // their servers in, which is worth preserving.
    let group_id = guild_group_id(&account_id, guild_id);
    state.runtime.upsert_buffer_group(
        state,
        crate::model::BufferGroup {
            id: group_id.clone(),
            account_id: account_id.clone(),
            service: "discord".to_string(),
            kind: "guild".to_string(),
            name: guild_name.clone(),
            // Filled in by the background fetch below once it lands; the rail
            // shows initials until then rather than waiting on the network.
            icon_url: cached_guild_icon(guild_id, guild["icon"].as_str()).await,
            position: guild["position"].as_i64().unwrap_or(0),
        },
    );
    cache_guild_icon(state.clone(), account_id.clone(), guild_id.to_string(), guild_name.clone(), guild["icon"].as_str().map(str::to_string), guild["position"].as_i64().unwrap_or(0));

    // Type 4 is a category: not a channel anyone talks in, but the heading
    // the others are filed under, and it carries its own ordering.
    let categories: HashMap<&str, (&str, i64)> = channels
        .iter()
        .filter(|c| c["type"].as_i64() == Some(4))
        .filter_map(|c| Some((c["id"].as_str()?, (c["name"].as_str().unwrap_or("category"), c["position"].as_i64().unwrap_or(0)))))
        .collect();

    for ch in channels {
        // 0 = GUILD_TEXT, 5 = GUILD_ANNOUNCEMENT - the only channel types
        // this milestone renders as buffers (voice/category/forum/etc.
        // skipped).
        let kind_num = ch["type"].as_i64().unwrap_or(-1);
        if kind_num != 0 && kind_num != 5 {
            continue;
        }
        let Some(channel_id) = ch["id"].as_str() else { continue };
        if channel_map.contains_key(channel_id) {
            continue;
        }
        if !can_view_channel(is_owner, guild_id, &roles, &member_role_ids, &config.user_id, ch) {
            continue;
        }
        let chan_name = ch["name"].as_str().unwrap_or("channel");
        let name = format!("{guild_name}/#{chan_name}");
        let buf = state.runtime.ensure_buffer(state, &account_id, &name, "channel");
        state.runtime.set_discord_channel(&buf.id, channel_id);
        state.runtime.set_discord_guild(&buf.id, guild_id);
        state.runtime.set_buffer_group(state, &buf.id, &group_id);

        // Where Discord itself puts this channel. Uncategorised channels sit
        // above every heading, which is where Discord shows them, so they take
        // a category rank below any real one.
        let parent = ch["parent_id"].as_str().and_then(|p| categories.get(p));
        let channel_pos = ch["position"].as_i64().unwrap_or(0);
        let sort = match parent {
            Some((_, cat_pos)) => (cat_pos + 1) * 10_000 + channel_pos,
            None => channel_pos,
        };
        state.runtime.set_buffer_category(state, &buf.id, parent.map(|(name, _)| *name), sort);

        // Custom emoji are per-guild, not per-channel, but buffers only
        // carry a channel id (see discord_channels) - simplest to just
        // hand each of the guild's channels its own copy of the same
        // list rather than adding a separate guild-id lookup for this.
        // Already present in this same GUILD_CREATE payload (`available:
        // false` covers a guild that dropped below the boost tier a slot
        // needed - Discord still lists it, just unusable), so this is
        // free - no extra REST call, unlike the member roster attempt.
        if let Some(emojis) = guild["emojis"].as_array() {
            let usable: Vec<Value> = emojis
                .iter()
                .filter(|e| e["available"].as_bool().unwrap_or(true))
                .filter_map(|e| {
                    let id = e["id"].as_str()?;
                    let name = e["name"].as_str()?;
                    Some(json!({ "id": id, "name": name, "animated": e["animated"].as_bool().unwrap_or(false) }))
                })
                .collect();
            state.runtime.set_discord_buffer_emojis(&buf.id, usable);
        }
        channel_map.insert(channel_id.to_string(), (name, "channel".to_string()));
        new_channels.push((buf.id, channel_id.to_string()));
    }

    spawn_backfill(state.clone(), config.token.clone(), config.user_id.clone(), config.display_name.clone(), new_channels);
}

/// Fires off history backfill for a batch of newly-registered buffers as a
/// background task, sequentially with a small delay between requests - a
/// guild can easily have 30+ channels (confirmed live), and firing that
/// many REST requests at once risks Discord's rate limiter; a one-time
/// startup cost taking a few extra seconds in the background is a fine
/// trade for not tripping it.
fn spawn_backfill(state: AppState, token: String, user_id: String, display_name: Option<String>, targets: Vec<(String, String)>) {
    if targets.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for (buffer_id, channel_id) in targets {
            backfill_channel_history(&state, &token, &user_id, display_name.as_deref(), &buffer_id, &channel_id).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

/// Backfills up to 50 recent messages for a channel/DM with no scrollback
/// yet. Unlike IRC (which has no server-side history at all - nobilis only
/// ever knows what it's personally seen live), Discord actually retains
/// and exposes history, so starting every buffer blank here would look
/// broken compared to Discord's own client. Seed-once, not sync: a buffer
/// that already has *any* stored messages (from an earlier backfill or
/// from live traffic already recorded) is left alone - there's no
/// per-message dedup against Discord's own message ids in this store, so
/// re-fetching on every reconnect would just duplicate rows.
///
/// Deliberately bypasses Runtime::record_message - that path also emits
/// the live "message" broadcast event and (for DMs/highlights) a
/// "notification" event, neither of which should fire for messages from
/// potentially weeks ago just because this is the first time nobilis has
/// seen the channel.
async fn backfill_channel_history(state: &AppState, token: &str, user_id: &str, own_display_name: Option<&str>, buffer_id: &str, channel_id: &str) {
    match state.store.has_messages(buffer_id) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!("discord: checking existing history for {buffer_id}: {e}");
            return;
        }
    }

    let resp = match http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", "50")])
        .header("Authorization", token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("discord: fetching history for {buffer_id}: {e}");
            return;
        }
    };
    if !resp.status().is_success() {
        // Missing READ_MESSAGE_HISTORY, rate-limited, etc. - not worth
        // treating as an error, just leaves that buffer without backfill.
        return;
    }
    let messages: Vec<Value> = match resp.json().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("discord: parsing history for {buffer_id}: {e}");
            return;
        }
    };
    store_history_messages(state, buffer_id, &messages, user_id, own_display_name);
    state.runtime.refresh_buffer_activity(state, buffer_id);
}

/// Discord sometimes puts the actually-useful content in `embeds` rather
/// than `content`/`attachments`, and treating those two fields as the
/// whole message (as this used to) silently drops real content in two
/// confirmed-live cases:
///
/// - A plain link (e.g. a klipy.com GIF page) gets server-side resolved
///   into a rich embed carrying the *actual* playable media URL (often
///   with a real file extension, unlike the original link) in
///   `embed.video.url` / `embed.image.url` - our own extension-based
///   embed detection has nothing to match against the original bare link,
///   but matches these resolved URLs fine.
/// - Some bot/webhook messages (bridges, log relays) ship an empty
///   `content` with their entire payload in an embed's `description`
///   (confirmed against a real Sneedchat IRC-bridge webhook message) -
///   without this, those messages showed up as blank.
///
/// Attachments beyond the first are also now included (previously only
/// `attachments[0]` was ever read, silently dropping additional files).
/// Returns None only when there is truly nothing renderable at all.
fn extract_body(d: &Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let content = d["content"].as_str().unwrap_or("");
    if !content.is_empty() {
        parts.push(content.to_string());
    }
    if let Some(embeds) = d["embeds"].as_array() {
        for embed in embeds {
            if let Some(url) = embed["video"]["url"].as_str() {
                parts.push(url.to_string());
            } else if let Some(url) = embed["image"]["url"].as_str() {
                parts.push(url.to_string());
            } else if embed["type"].as_str() == Some("gifv") {
                if let Some(url) = embed["thumbnail"]["url"].as_str() {
                    parts.push(url.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Downloads a preview of each image attachment and records where it landed.
///
/// Runs in the background rather than inline: a message should appear the
/// moment it arrives, not after its picture has been fetched. The stored
/// message is updated once the copies exist, and the change is broadcast so
/// anything already showing that message picks them up.
pub fn cache_thumbnails(state: AppState, buffer_id: String, msg_id: String, attachments: Vec<Attachment>) {
    if !attachments.iter().any(|a| a.kind == "image" && a.thumbnail_path.is_none()) {
        return;
    }
    tokio::spawn(async move {
        let mut updated = attachments;
        let mut any = false;
        for att in &mut updated {
            if att.kind != "image" || att.thumbnail_path.is_some() {
                continue;
            }
            let Some(url) = att.url.as_deref() else { continue };
            let Some(src) = thumbnail_source(url, att.width.unwrap_or(0), att.height.unwrap_or(0)) else { continue };
            if let Some(path) = fetch_thumbnail(&src, url).await {
                att.thumbnail_path = Some(path);
                any = true;
            }
        }
        if !any {
            return;
        }
        // Not an edit: the message text is untouched, only where its preview
        // can be found locally.
        if let Err(e) = state.store.update_message_attachments(&buffer_id, &msg_id, &updated) {
            tracing::debug!("discord: recording thumbnails: {e}");
            return;
        }
        state.events.emit(
            "messageUpdated",
            json!({ "bufferId": buffer_id, "id": msg_id, "edited": false, "attachments": updated }),
        );
    });
}

async fn fetch_thumbnail(src: &str, cache_key: &str) -> Option<String> {
    let dir = thumbnail_cache_dir();
    // Keyed by the *unsigned* part of the URL - the signature changes on every
    // refresh, so including it would cache the same picture repeatedly.
    let stable = cache_key.split('?').next().unwrap_or(cache_key);
    let mut hasher = Sha256::new();
    hasher.update(stable.as_bytes());
    let path = dir.join(format!("{:x}", hasher.finalize()));

    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Some(format!("file://{}", path.display()));
    }
    tokio::fs::create_dir_all(&dir).await.ok()?;

    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), http_client().get(src).send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    tokio::fs::write(&path, &bytes).await.ok()?;
    Some(format!("file://{}", path.display()))
}

/// Buffers with a re-sign already running, so a burst of getBacklog calls
/// (opening, scrolling, opening again) issues one sweep rather than several
/// against a rate-limited endpoint.
fn resigning() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// How many history pages one sweep will pull. Each covers ~50 messages, so
/// this reaches a few screenfuls; past that the remaining stale messages are
/// left for the per-message refresh a click triggers, rather than hammering a
/// rate-limited endpoint on every buffer open.
const MAX_RESIGN_FETCHES: usize = 3;

/// Re-signs every expired attachment across a page of scrollback, in as few
/// requests as it can manage.
///
/// Discord's own client batches this through `/attachments/refresh-urls`,
/// which refuses user tokens here. But a history read re-signs every
/// attachment in the page it returns, so one fetch of ~50 messages does the
/// same job for a screenful - far better than one request per image, which is
/// what refreshing each on click costs.
///
/// Runs in the background: the buffer opens immediately showing cached
/// previews, and re-signed links arrive as messageUpdated events.
pub fn resign_stale_attachments(state: AppState, buffer_id: String, messages: &[crate::model::Message]) {
    let stale = stale_message_ids(messages);
    if stale.is_empty() {
        return;
    }

    if !resigning().lock().unwrap().insert(buffer_id.clone()) {
        return;
    }

    tokio::spawn(async move {
        if let Err(e) = run_resign(&state, &buffer_id, stale).await {
            tracing::debug!("discord: re-signing {buffer_id}: {e}");
        }
        resigning().lock().unwrap().remove(&buffer_id);
    });
}

/// Which messages in a page carry a lapsed attachment link, newest first.
///
/// Newest first because those are the ones most likely to be on screen, so
/// the first fetch re-signs what the reader is actually looking at.
///
/// Attachments with no `url` at all are nobilis's own locally-cached media
/// (Matrix, Sneedchat) and never expire, so a non-Discord buffer produces an
/// empty list here and costs nothing.
fn stale_message_ids(messages: &[crate::model::Message]) -> Vec<String> {
    let mut ids: Vec<String> = messages
        .iter()
        .filter(|m| {
            m.attachments
                .iter()
                .any(|a| a.url.as_deref().is_some_and(attachment_expired))
        })
        .map(|m| m.id.clone())
        .collect();
    // Discord ids are snowflakes: lexicographically ordered for equal length,
    // and longer means newer, so sort by length first.
    ids.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| b.cmp(a)));
    ids
}

async fn run_resign(state: &AppState, buffer_id: &str, mut stale: Vec<String>) -> Result<()> {
    let buffer = state.runtime.get_buffer(buffer_id).context("no such buffer")?;
    let config = state.accounts.get_discord(&buffer.account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel")?;

    for _ in 0..MAX_RESIGN_FETCHES {
        let Some(anchor) = stale.first().cloned() else { break };
        // `around` centres the page on the message, so one fetch covers what
        // sits either side of it - usually the rest of the same screenful.
        let resp = http_client()
            .get(format!("{API_BASE}/channels/{channel_id}/messages"))
            .query(&[("limit", "50"), ("around", anchor.as_str())])
            .header("Authorization", &config.token)
            .send()
            .await
            .context("fetching a page to re-sign")?;
        if !resp.status().is_success() {
            bail!("Discord refused the request ({})", resp.status());
        }
        let page: Vec<Value> = resp.json().await.context("parsing the re-signed page")?;
        if page.is_empty() {
            break;
        }

        let mut handled = 0usize;
        for message in &page {
            let Some(id) = message["id"].as_str() else { continue };
            if !stale.iter().any(|s| s == id) {
                continue;
            }
            let attachments = merge_cached_thumbnails(state, buffer_id, id, extract_attachments(message));
            if attachments.is_empty() {
                continue;
            }
            if state.store.update_message_attachments(buffer_id, id, &attachments).unwrap_or(false) {
                state.events.emit(
                    "messageUpdated",
                    json!({ "bufferId": buffer_id, "id": id, "edited": false, "attachments": attachments.clone() }),
                );
            }
            // The link is valid again right now, and this message reached a
            // re-sign because its preview was needed - so take one while it can
            // still be fetched, and the next expiry has something to show.
            cache_thumbnails(state.clone(), buffer_id.to_string(), id.to_string(), attachments);
            handled += 1;
        }

        let covered: std::collections::HashSet<&str> = page.iter().filter_map(|m| m["id"].as_str()).collect();
        // Drop everything this page spanned, not just what it re-signed - a
        // message inside the returned range that came back without
        // attachments was deleted or edited, and asking again won't change
        // that. Without this the anchor would not advance and the loop would
        // refetch the same page.
        stale.retain(|id| !covered.contains(id.as_str()));
        if stale.is_empty() {
            break;
        }
        // Nothing in range matched and nothing was dropped: give up rather
        // than spin.
        if handled == 0 && covered.is_empty() {
            break;
        }
    }
    Ok(())
}

/// Cached previews are keyed by the unsigned URL, so they stay valid across a
/// re-sign and are carried over rather than re-fetched.
fn merge_cached_thumbnails(
    state: &AppState,
    buffer_id: &str,
    message_id: &str,
    attachments: Vec<Attachment>,
) -> Vec<Attachment> {
    match state.store.get_message(buffer_id, message_id) {
        Ok(Some(old)) => attachments
            .into_iter()
            .enumerate()
            .map(|(i, mut a)| {
                if let Some(prev) = old.attachments.get(i) {
                    a.thumbnail_path = a.thumbnail_path.or_else(|| prev.thumbnail_path.clone());
                }
                a
            })
            .collect(),
        _ => attachments,
    }
}

/// Re-signs a message's attachment links by asking Discord for the message
/// again.
///
/// Discord signs CDN links when it serves them, so a fresh read of the same
/// message carries fresh signatures - which is the way in, because the
/// dedicated `/attachments/refresh-urls` endpoint refuses this backend's
/// user-token requests outright (see message_link's doc comment). The
/// messages endpoint used here is the same one history paging already uses,
/// so it is known to work with this token.
///
/// `around` rather than fetching the message by id directly: the single
/// message endpoint is bot-only, while `around` is what a user client uses.
pub async fn refresh_attachments(state: &AppState, buffer_id: &str, message_id: &str) -> Result<Vec<Attachment>> {
    let buffer = state.runtime.get_buffer(buffer_id).context("no such buffer")?;
    let config = state.accounts.get_discord(&buffer.account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this buffer")?;

    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", "1"), ("around", message_id)])
        .header("Authorization", &config.token)
        .send()
        .await
        .context("re-fetching the message")?;
    if !resp.status().is_success() {
        bail!("Discord refused the request ({})", resp.status());
    }
    let messages: Vec<Value> = resp.json().await.context("parsing the re-fetched message")?;
    let message = messages
        .iter()
        .find(|m| m["id"].as_str() == Some(message_id))
        .context("Discord no longer has that message")?;

    let attachments = extract_attachments(message);
    if attachments.is_empty() {
        bail!("that message no longer has any attachments");
    }
    let attachments = merge_cached_thumbnails(state, buffer_id, message_id, attachments);

    state.store.update_message_attachments(buffer_id, message_id, &attachments)?;
    state.events.emit(
        "messageUpdated",
        json!({ "bufferId": buffer_id, "id": message_id, "edited": false, "attachments": attachments.clone() }),
    );
    // Same reasoning as the batch sweep: capture a preview while this link is
    // freshly signed, so the next expiry is not another round-trip.
    cache_thumbnails(state.clone(), buffer_id.to_string(), message_id.to_string(), attachments.clone());
    Ok(attachments)
}

/// Rail entry id for one guild. Scoped by account so two accounts in the same
/// guild get their own entry rather than colliding on one.
pub fn guild_group_id(account_id: &str, guild_id: &str) -> String {
    format!("{account_id}|guild:{guild_id}")
}

/// The rail entry holding an account's direct messages, matching how Discord's
/// own client gives DMs a place in the server column rather than scattering
/// them among the guilds.
pub fn dm_group_id(account_id: &str) -> String {
    format!("{account_id}|dms")
}

/// Registers the account's direct-message rail entry. Idempotent, and only
/// called once a DM actually exists, so an account with no DMs does not get an
/// empty entry sitting in the rail.
fn ensure_dm_group(state: &AppState, account_id: &str) {
    state.runtime.upsert_buffer_group(
        state,
        crate::model::BufferGroup {
            id: dm_group_id(account_id),
            account_id: account_id.to_string(),
            service: "discord".to_string(),
            kind: "dms".to_string(),
            name: "Direct Messages".to_string(),
            icon_url: None,
            // Above the guilds, where Discord puts it.
            position: -1,
        },
    );
}

fn guild_icon_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("discord-icons")
}

pub async fn sweep_guild_icon_cache() {
    super::sockchat::sweep_cache_dir(&guild_icon_cache_dir(), GUILD_ICON_CACHE_MAX_BYTES, "discord guild icon").await;
}

/// Guild icons are small and there are only as many as the user has servers,
/// so this is a much smaller cap than the message thumbnail cache.
const GUILD_ICON_CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Where a guild's icon would already be on disk, if it has been fetched.
///
/// Keyed by the icon hash as well as the guild id, so a server changing its
/// icon fetches the new one rather than showing the old one forever.
async fn cached_guild_icon(guild_id: &str, icon_hash: Option<&str>) -> Option<String> {
    let hash = icon_hash?;
    let path = guild_icon_cache_dir().join(format!("{guild_id}-{hash}.png"));
    tokio::fs::try_exists(&path).await.unwrap_or(false).then(|| format!("file://{}", path.display()))
}

/// Fetches a guild's icon in the background and re-registers the rail entry
/// once it lands.
///
/// Background rather than inline: this runs while connecting, and a user with
/// thirty servers should not wait on thirty image fetches before any of their
/// channels appear. A guild with no icon set is not an error - the rail draws
/// initials for it, the same as Discord does.
fn cache_guild_icon(state: AppState, account_id: String, guild_id: String, name: String, icon_hash: Option<String>, position: i64) {
    let Some(hash) = icon_hash else { return };
    tokio::spawn(async move {
        let dir = guild_icon_cache_dir();
        let path = dir.join(format!("{guild_id}-{hash}.png"));
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            // Animated icons have an a_ prefix and are served as .gif; asking
            // for .png yields a still frame of the same thing, which is what a
            // rail wants anyway.
            let url = format!("https://cdn.discordapp.com/icons/{guild_id}/{hash}.png?size=128");
            let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(20), http_client().get(&url).send()).await else {
                tracing::debug!("discord: guild icon fetch for {guild_id} timed out");
                return;
            };
            if !resp.status().is_success() {
                tracing::debug!("discord: guild icon for {guild_id} returned HTTP {}", resp.status());
                return;
            }
            let Ok(bytes) = resp.bytes().await else { return };
            if tokio::fs::create_dir_all(&dir).await.is_err() || tokio::fs::write(&path, &bytes).await.is_err() {
                return;
            }
        }
        state.runtime.upsert_buffer_group(
            &state,
            crate::model::BufferGroup {
                id: guild_group_id(&account_id, &guild_id),
                account_id,
                service: "discord".to_string(),
                kind: "guild".to_string(),
                name,
                icon_url: Some(format!("file://{}", path.display())),
                position,
            },
        );
    });
}

/// Where cached Discord thumbnails live. Every CDN link Discord serves is
/// signed and lapses roughly a day later, so a message read back out of
/// scrollback after that has a URL that no longer loads. A small local copy
/// taken while the link still works is what lets an old message still show
/// its picture rather than a dead box.
fn thumbnail_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("discord-thumbnails")
}

/// Deliberately smaller than the attachment caches: these are previews, not
/// the originals, and the full-size image is always one refresh away.
const THUMBNAIL_CACHE_MAX_BYTES: u64 = 100 * 1024 * 1024;

pub async fn sweep_thumbnail_cache() {
    super::sockchat::sweep_cache_dir(&thumbnail_cache_dir(), THUMBNAIL_CACHE_MAX_BYTES, "discord thumbnail").await;
}

/// The width to ask Discord's media proxy for. Big enough to look right in a
/// message list at any sane window size, small enough that caching one per
/// image is cheap.
const THUMBNAIL_WIDTH: u32 = 480;

/// Rewrites a CDN link into a resized one through Discord's media proxy,
/// which is the same trick its own clients use for previews. Returns None for
/// anything that isn't a Discord-hosted image, so nothing else gets proxied.
fn thumbnail_source(url: &str, width: u32, height: u32) -> Option<String> {
    if !url.starts_with("https://cdn.discordapp.com/") && !url.starts_with("https://media.discordapp.net/") {
        return None;
    }
    let proxied = url.replacen("https://cdn.discordapp.com/", "https://media.discordapp.net/", 1);
    // Preserve the aspect ratio: asking for a square would letterbox it.
    let (w, h) = if width == 0 || height == 0 {
        (THUMBNAIL_WIDTH, THUMBNAIL_WIDTH)
    } else if width >= height {
        (THUMBNAIL_WIDTH, (height * THUMBNAIL_WIDTH / width).max(1))
    } else {
        ((width * THUMBNAIL_WIDTH / height).max(1), THUMBNAIL_WIDTH)
    };
    let sep = if proxied.contains('?') { '&' } else { '?' };
    Some(format!("{proxied}{sep}width={w}&height={h}"))
}

/// The `ex=` query parameter is the link's expiry, as a hex unix timestamp.
/// Reading it lets a stale link be recognised before it is requested, rather
/// than after a failed load.
fn attachment_expired(url: &str) -> bool {
    let Some(ex) = url.split(['?', '&']).find_map(|p| p.strip_prefix("ex=")) else { return false };
    let Ok(expiry) = u64::from_str_radix(ex, 16) else { return false };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now >= expiry
}

/// Discord's own `attachments` array, kept structured instead of being
/// flattened into body text. Discord already reports filename, size,
/// dimensions and content type per file, so there is nothing to infer - and
/// unlike Matrix or Sneedchat these URLs are directly loadable by a frontend,
/// so no local cache copy is needed and `path` stays unset.
/// Whether a Discord message carries nothing worth storing.
///
/// Discord emits messages that are pure protocol noise (a pin notice, a
/// thread-created marker), and those genuinely have nothing to show. An
/// uncaptioned picture is not one of them: its body is empty because the
/// content is the attachment. Attachment URLs were once appended to the body,
/// which hid this distinction - testing the body alone now drops the most
/// ordinary kind of image post there is.
fn is_empty_message(body: &str, embeds: &[Embed], attachments: &[Attachment]) -> bool {
    body.is_empty() && embeds.is_empty() && attachments.is_empty()
}

fn extract_attachments(d: &Value) -> Vec<Attachment> {
    let Some(atts) = d["attachments"].as_array() else { return Vec::new() };
    atts.iter()
        .filter_map(|att| {
            let url = att["url"].as_str()?;
            let mimetype = att["content_type"].as_str().map(str::to_string);
            let kind = match mimetype.as_deref().unwrap_or("") {
                m if m.starts_with("image/") => "image",
                m if m.starts_with("video/") => "video",
                m if m.starts_with("audio/") => "audio",
                _ => "file",
            };
            Some(Attachment {
                kind: kind.to_string(),
                filename: att["filename"].as_str().map(str::to_string),
                size: att["size"].as_u64(),
                width: att["width"].as_u64().map(|v| v as u32),
                height: att["height"].as_u64().map(|v| v as u32),
                url: Some(url.to_string()),
                mimetype,
                ..Default::default()
            })
        })
        .collect()
}

/// A rich embed's title/description/color/timestamp/url, structured
/// rather than flattened into plain body text (see model.rs's Embed doc
/// comment - this replaced extract_body's old title+description dump).
/// Only embeds that actually carry a title or description become one of
/// these; a pure image/video/gifv embed has neither and is already fully
/// represented by extract_body's own media-URL handling above, so
/// including it here too would just render an empty box under the media.
fn extract_embeds(d: &Value) -> Vec<Embed> {
    let Some(embeds) = d["embeds"].as_array() else { return Vec::new() };
    embeds
        .iter()
        .filter_map(|embed| {
            let title = embed["title"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());
            let description = embed["description"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());
            if title.is_none() && description.is_none() {
                return None;
            }
            Some(Embed {
                title,
                description,
                color: embed["color"].as_i64(),
                timestamp: embed["timestamp"].as_str().map(|s| s.to_string()),
                url: embed["url"].as_str().map(|s| s.to_string()),
            })
        })
        .collect()
}

/// Discord-native replies: `message_reference.message_id` names what's
/// Discord's raw `<@id>`/`<@!id>` mention tokens resolved to `@name` using
/// the message's own `mentions` array (already carries id/username/
/// global_name for everyone pinged - no extra lookup needed). When the
/// mentioned id is *this* account's own user, the local display-name
/// override (config.display_name, see accounts.rs's set_display_name) is
/// substituted instead of Discord's real name - purely a local rendering
/// choice, never sent anywhere.
/// Builds a message author's real avatar CDN URL from their `author`
/// object (works identically whether that author is someone else or this
/// account's own user - Discord's gateway echo of a self-sent message
/// carries the same full `author` object as any other message). `None`
/// when the user has no avatar hash set (a legacy/never-customized
/// account) - the frontend falls back to a colored initial in that case,
/// same as it already does for IRC.
fn author_avatar_url(author: &Value) -> Option<String> {
    let id = author["id"].as_str()?;
    let hash = author["avatar"].as_str()?;
    let ext = if hash.starts_with("a_") { "gif" } else { "png" };
    Some(format!("https://cdn.discordapp.com/avatars/{id}/{hash}.{ext}"))
}

fn resolve_mentions(body: &str, d: &Value, own_user_id: &str, own_display_name: Option<&str>) -> String {
    let Some(mentions) = d["mentions"].as_array() else { return body.to_string() };
    let mut out = body.to_string();
    for m in mentions {
        let Some(id) = m["id"].as_str() else { continue };
        let name = if id == own_user_id {
            own_display_name
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| m["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| m["username"].as_str()).unwrap_or("you").to_string())
        } else {
            m["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| m["username"].as_str()).unwrap_or("someone").to_string()
        };
        out = out.replace(&format!("<@{id}>"), &format!("@{name}"));
        out = out.replace(&format!("<@!{id}>"), &format!("@{name}"));
    }
    out
}

/// Whether this account's own user is directly pinged in the message -
/// read straight from Discord's own resolved `mentions` array rather than
/// reconstructed via substring matching, which is both more reliable and
/// the only way this would ever fire at all: the raw body still contains
/// `<@id>` tokens (not the account's nick) at the point highlight
/// detection needs an answer, so a generic nick-substring check (fine for
/// IRC, which has no structured mention data) can never match a Discord
/// mention.
fn mentions_own_user(d: &Value, own_user_id: &str) -> bool {
    d["mentions"].as_array().map(|arr| arr.iter().any(|m| m["id"].as_str() == Some(own_user_id))).unwrap_or(false)
}

/// A cached snapshot of the message being replied to, taken at receive
/// time - not a live reference. Discord hands us the referenced message's
/// author/content inline with the reply itself (`referenced_message`), so
/// there's no need to look anything up, and the preview still means
/// something even if the original later scrolls out of local history or
/// gets deleted. `id` is what the frontend's "jump to" click targets if
/// the original happens to already be loaded.
fn extract_reply(d: &Value) -> Option<ReplyPreview> {
    let reply_id = d["message_reference"]["message_id"].as_str()?;
    let referenced = &d["referenced_message"];
    if referenced.is_null() {
        // Reference exists but Discord didn't resolve it - still worth a
        // "replying to a message" placeholder rather than nothing at all.
        return Some(ReplyPreview { id: reply_id.to_string(), from: String::new(), body: String::new() });
    }
    let author = &referenced["author"];
    let from = author["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| author["username"].as_str()).unwrap_or("unknown").to_string();
    let body = extract_body(referenced).unwrap_or_default();
    Some(ReplyPreview { id: reply_id.to_string(), from, body })
}

/// Shared by both the initial backfill and extend_history - Discord
/// returns messages newest-first; both store oldest-first so scrollback
/// reads top-to-bottom chronologically, matching getBacklog's contract.
fn store_history_messages(state: &AppState, buffer_id: &str, messages: &[Value], user_id: &str, own_display_name: Option<&str>) {
    for msg in messages.iter().rev() {
        let Some(msg_id) = msg["id"].as_str() else { continue };
        let author = &msg["author"];
        let from = author["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| author["username"].as_str()).unwrap_or("unknown");
        let is_own = author["id"].as_str() == Some(user_id);
        let embeds = extract_embeds(msg);
        let attachments = extract_attachments(msg);
        let body = extract_body(msg).unwrap_or_default();
        if is_empty_message(&body, &embeds, &attachments) {
            continue;
        }
        let body = resolve_mentions(&body, msg, user_id, own_display_name);
        let reply_to = extract_reply(msg);
        let reactions = extract_reactions(msg);
        let avatar_url = author_avatar_url(author);
        let ts = msg["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        if let Err(e) = state.store.append_message(buffer_id, msg_id, from, &body, ts, false, false, "chat", reply_to.as_ref(), &reactions, is_own, avatar_url.as_deref(), &embeds, &attachments, None) {
            tracing::warn!("discord: storing history message: {e}");
            continue;
        }
        // Backfilled messages need previews as much as live ones do - more so,
        // since a channel read for the first time is all history and none of it
        // would otherwise survive its links expiring.
        cache_thumbnails(state.clone(), buffer_id.to_string(), msg_id.to_string(), attachments);
    }
}

/// Same `<:name:id>` / plain-unicode emoji key convention as the live
/// REACTION_ADD/REMOVE handling - Discord already reports a snapshot
/// count and whether *we* are among the reactors, so this is a much
/// simpler direct read rather than reconstructing state incrementally.
fn extract_reactions(msg: &Value) -> Vec<Reaction> {
    let Some(arr) = msg["reactions"].as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|r| {
            let count = r["count"].as_i64()?;
            let name = r["emoji"]["name"].as_str().unwrap_or("?");
            let is_custom = r["emoji"]["id"].as_str().is_some();
            let emoji = match r["emoji"]["id"].as_str() {
                Some(id) => format!("<:{name}:{id}>"),
                None => name.to_string(),
            };
            let me = r["me"].as_bool().unwrap_or(false);
            let animated = is_custom && r["emoji"]["animated"].as_bool().unwrap_or(false);
            Some(Reaction { emoji, count, me, animated })
        })
        .collect()
}

/// Extends a buffer's history further back using Discord's own message
/// pagination (`before=<oldest known message id>`) - the initial backfill
/// (see backfill_channel_history) only ever seeds the most recent 50, so
/// this is what lets scrolling all the way up in a long-lived channel/DM
/// keep going instead of hitting a wall. A no-op if nothing is stored yet
/// (the initial backfill owns that case - there's no "oldest" to page
/// before) or if a request for this same buffer is already in flight
/// (getBacklog can be called concurrently by more than one connected
/// client - see the multi-screen-instance lesson from the QR login bug).
/// Re-reads a channel's recent history and stores only what is missing.
///
/// Unlike extend_history this deliberately re-reads a range already stored,
/// so it exists to repair scrollback rather than to deepen it: a message that
/// Discord sent but that was never written locally (dropped by a storage bug,
/// or missed while the daemon was down) has no other way back. Everything
/// already present is left untouched, so it is safe to run repeatedly.
///
/// Returns how many messages were recovered.
pub async fn refill_history(state: &AppState, buffer_id: &str, limit: u32) -> Result<usize> {
    let buffer = state.runtime.get_buffer(buffer_id).context("no such buffer")?;
    let config = state.accounts.get_discord(&buffer.account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this buffer")?;

    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", limit.clamp(1, 100).to_string().as_str())])
        .header("Authorization", &config.token)
        .send()
        .await
        .context("re-reading history")?;
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Honour Discord's own pacing rather than guessing at a backoff.
        let wait = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["retry_after"].as_f64())
            .unwrap_or(5.0);
        bail!("rate limited, retry after {wait:.1}s");
    }
    if !resp.status().is_success() {
        // No READ_MESSAGE_HISTORY on this channel is a normal outcome for a
        // sweep across every buffer, not a failure worth stopping for.
        bail!("Discord refused the request ({})", resp.status());
    }
    let messages: Vec<Value> = resp.json().await.context("parsing re-read history")?;

    let ids: Vec<&str> = messages.iter().filter_map(|m| m["id"].as_str()).collect();
    let known = state.store.existing_msg_ids(buffer_id, &ids)?;
    let missing: Vec<Value> = messages
        .into_iter()
        .filter(|m| m["id"].as_str().is_some_and(|id| !known.contains(id)))
        .collect();
    if missing.is_empty() {
        return Ok(0);
    }

    // Same storage path as any other history read, so recovered messages get
    // the current guard and preview caching rather than a parallel copy.
    store_history_messages(state, buffer_id, &missing, &config.user_id, config.display_name.as_deref());
    state.runtime.refresh_buffer_activity(state, buffer_id);
    Ok(missing.len())
}

pub async fn extend_history(state: &AppState, token: &str, user_id: &str, own_display_name: Option<&str>, buffer_id: &str, channel_id: &str) {
    if !state.runtime.try_start_discord_history_fetch(buffer_id) {
        return;
    }
    let result: Result<()> = async {
        let Some(before_id) = state.store.oldest_msg_id(buffer_id)? else { return Ok(()) };
        let resp = http_client()
            .get(format!("{API_BASE}/channels/{channel_id}/messages"))
            .query(&[("limit", "50"), ("before", before_id.as_str())])
            .header("Authorization", token)
            .send()
            .await
            .context("fetching more history")?;
        if resp.status().is_success() {
            let messages: Vec<Value> = resp.json().await.context("parsing more history")?;
            store_history_messages(state, buffer_id, &messages, user_id, own_display_name);
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("discord: extending history for {buffer_id}: {e}");
    }
    state.runtime.finish_discord_history_fetch(buffer_id);
}

/// Discord's gateway close code for "the token in your IDENTIFY payload is
/// invalid/revoked" - documented at discord.com/developers/docs/topics/
/// opcodes-and-status-codes#gateway-close-event-codes, and confirmed live
/// against this project's own account going through exactly this (see
/// GatewayAuthFailed's doc comment). Distinct from op 9 "invalid session"
/// (a resumable-session issue mid-connection, already handled by the plain
/// reconnect-with-backoff path below) - this one means the *credential*
/// itself is dead, not just this particular session.
const CLOSE_CODE_AUTH_FAILED: u16 = 4004;

/// Marker error so run_gateway_with_retry can tell "the token is
/// permanently dead" apart from every other (transient, worth retrying)
/// failure via a plain downcast, without run_gateway itself needing to
/// know anything about retry policy. Surfaced after this backend's own
/// account got deauthed mid-session (2026-08-20) with no clear signal
/// beyond the raw close frame - previously indistinguishable from any
/// other dropped connection, so the account just sat retrying the same
/// dead token forever, showing a generic "connecting".
#[derive(Debug)]
struct GatewayAuthFailed;

impl std::fmt::Display for GatewayAuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Discord rejected this login (session revoked) - reconnect from the Accounts pane")
    }
}

impl std::error::Error for GatewayAuthFailed {}

async fn next_json<S>(stream: &mut S) -> Result<Value>
where
    S: futures::Stream<Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(text))) => return serde_json::from_str(&text).context("parsing gateway JSON"),
            Some(Ok(WsMessage::Close(frame))) => {
                if frame.as_ref().is_some_and(|f| u16::from(f.code) == CLOSE_CODE_AUTH_FAILED) {
                    return Err(GatewayAuthFailed.into());
                }
                bail!("connection closed: {frame:?}")
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => bail!("websocket error: {e}"),
            None => bail!("connection ended unexpectedly"),
        }
    }
}

/// Spawns the background task that keeps a Discord account's gateway
/// connection alive - the real-time equivalent of backend::irc::spawn.
/// Called both right after a fresh QR login and, from main.rs, for every
/// saved Discord account on daemon startup.
pub fn spawn(state: AppState, config: DiscordAccountConfig) {
    let account_id = config.account_id();
    // Guarantee at most one live gateway session per account - the same
    // guard backend::irc::spawn() needed (see Runtime::reset_connection's
    // doc comment): without this, a stale/zombie session left over from an
    // earlier connect (one whose socket silently died without the read
    // loop ever erroring - confirmed live: an account that still showed
    // "connected" kept a channel_map from hours earlier and simply stopped
    // receiving *any* live gateway dispatches for it) could sit alongside
    // a fresh one, or an old task's eventual cleanup could stomp a newer
    // connection's state. A no-op for the common case (nothing to reset).
    // Also what makes disconnect()/removeAccount reliably stop the retry
    // loop below - it works by aborting this same task_handle, regardless
    // of whether that loop is mid-connection or mid-backoff-sleep.
    state.runtime.reset_connection(&account_id);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_gateway_with_retry(&state, &config, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Keeps re-establishing the gateway connection for as long as this task
/// lives - a fresh attempt (after a backoff wait, past the first) on every
/// disconnect: a network blip, Discord requesting a reconnect (op 7), an
/// invalidated session (op 9), or a detected-zombie connection (missed
/// heartbeat ack - see run_gateway). No "give up permanently" case for an
/// ordinary transient failure - it just keeps failing visibly (the account
/// shows "connecting" with the last error as detail) rather than settling
/// into a silent, permanently-stuck state, matching what a real Discord
/// client does.
///
/// The one deliberate exception is GatewayAuthFailed (gateway close code
/// 4004 - the token itself is dead, not just this session): retrying with
/// the exact same credential can never succeed, so this loop stops and
/// leaves the account in ConnState::AuthFailed instead of retrying forever
/// against a token that will keep getting rejected. The only way out is a
/// fresh login (spawn() gets called again from there, replacing this task)
/// - see accounts.rs's add_discord doc comment for why that upserts the
/// same account id (same buffers/scrollback) rather than creating a new one.
///
/// Otherwise only stops when this task itself is aborted from outside: an
/// explicit disconnect, account removal, or a newer spawn() superseding it
/// via reset_connection().
async fn run_gateway_with_retry(state: &AppState, config: &DiscordAccountConfig, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        let result = std::panic::AssertUnwindSafe(run_gateway(state, config)).catch_unwind().await;
        let detail = match result {
            Ok(Ok(())) => "gateway session ended".to_string(),
            Ok(Err(e)) => {
                if e.downcast_ref::<GatewayAuthFailed>().is_some() {
                    tracing::warn!("discord[{account_id}]: {e} - giving up, needs a fresh login");
                    state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&e.to_string()));
                    return;
                }
                tracing::warn!("discord[{account_id}]: {e}");
                e.to_string()
            }
            Err(_) => {
                tracing::error!("discord[{account_id}]: connection task panicked");
                "internal error (see nobilis logs)".to_string()
            }
        };
        // set_conn_state() first (in case run_gateway got as far as
        // Connected before dying, which report_progress() alone can't
        // correct - it only ever re-stamps an *already*-"connecting"
        // state), then report_progress() for the live countdown text.
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        state.runtime.report_progress(state, account_id, &format!("{detail} - reconnecting in {}s...", delay.as_secs()));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

async fn run_gateway(state: &AppState, config: &DiscordAccountConfig) -> Result<()> {
    let account_id = config.account_id();
    let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(GATEWAY_URL))
        .await
        .map_err(|_| anyhow!("timed out connecting to Discord's gateway"))?
        .context("connecting to Discord's gateway")?;
    let (sink, mut stream) = ws.split();

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

    let hello = next_json(&mut stream).await.context("waiting for gateway hello")?;
    let heartbeat_interval = hello["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).max(1);

    // Discord's own documented client contract: if an ack (op 11) hasn't
    // arrived by the time the next heartbeat is due, the connection is
    // "zombied" and must be dropped and re-established, not just
    // heartbeat-retried forever - the read loop's own next_json().await
    // would otherwise wait on a socket that looks alive (never errors,
    // never closes) but has simply stopped receiving anything at all, the
    // exact "shows connected, never gets another message" failure this
    // whole retry mechanism exists to catch. ack_pending starts true so
    // the very first tick (before any heartbeat has been sent) can't
    // false-positive as a missed ack.
    let ack_pending = Arc::new(AtomicBool::new(false));
    let stale_notify = Arc::new(Notify::new());
    let hb_tx = out_tx.clone();
    let hb_ack_pending = ack_pending.clone();
    let hb_stale_notify = stale_notify.clone();
    let heartbeat_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(rand::random::<u64>() % heartbeat_interval)).await;
        let mut ticker = tokio::time::interval(Duration::from_millis(heartbeat_interval));
        loop {
            ticker.tick().await;
            if hb_ack_pending.swap(true, Ordering::SeqCst) {
                hb_stale_notify.notify_one();
                break;
            }
            if hb_tx.send(json!({ "op": 1, "d": Value::Null }).to_string()).is_err() {
                break;
            }
        }
    });
    let _heartbeat_guard = AbortOnDrop(heartbeat_task);

    // Deliberately minimal - no "intents" field (that's a bot-gateway-only
    // concept; a user token gets everything its own account can see
    // regardless), matching what working self-bot clients send.
    // Exposed so setAccountStatus can push a presence update onto this
    // connection rather than waiting for a reconnect to carry it.
    let account_id = config.account_id();
    state.runtime.set_discord_gateway_sender(&account_id, out_tx.clone());
    struct ClearSenderOnDrop<'a>(&'a AppState, String);
    impl Drop for ClearSenderOnDrop<'_> {
        fn drop(&mut self) {
            self.0.runtime.clear_discord_gateway_sender(&self.1);
        }
    }
    let _sender_guard = ClearSenderOnDrop(state, account_id.clone());

    out_tx.send(
        json!({
            "op": 2,
            "d": {
                "token": config.token,
                "properties": { "os": "linux", "browser": "nobilis", "device": "nobilis" },
                "compress": false,
                "large_threshold": 50,
                // Carried in IDENTIFY as well as pushed live, so a status set
                // before a reconnect survives it.
                "presence": presence_payload(&state.runtime.account_status(&account_id)),
            }
        })
        .to_string(),
    )?;

    // channel_id -> (buffer name, buffer kind), rebuilt fresh from READY/
    // GUILD_CREATE each connection - see runtime.rs's discord_channels for
    // the reverse (persistent) mapping sendMessage needs.
    let mut channel_map: HashMap<String, (String, String)> = HashMap::new();

    loop {
        let msg = tokio::select! {
            result = next_json(&mut stream) => result?,
            _ = stale_notify.notified() => bail!("no heartbeat ack received - connection is zombied"),
        };
        let op = msg.get("op").and_then(|v| v.as_i64()).unwrap_or(-1);
        match op {
            0 => {
                let t = msg.get("t").and_then(|v| v.as_str()).unwrap_or("");
                let d = &msg["d"];
                match t {
                    "READY" => {
                        let username = d["user"]["global_name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or_else(|| d["user"]["username"].as_str())
                            .unwrap_or("me")
                            .to_string();
                        state.runtime.set_own_identity(&account_id, &username);
                        if let Some(hash) = d["user"]["avatar"].as_str() {
                            let url = format!("https://cdn.discordapp.com/avatars/{}/{hash}.png", config.user_id);
                            if state.accounts.set_discord_avatar_url(&account_id, &url).unwrap_or(false) {
                                state.events.emit("accountAvatarChanged", json!({ "accountId": account_id, "avatarUrl": url }));
                            }
                        }

                        // Friends list: READY's own `relationships` array
                        // (type 1 = friend - 2/3/4 are blocked/incoming-
                        // request/outgoing-request, not shown here) plus
                        // whatever initial status `presences` already
                        // carries for each. Not every friend necessarily has
                        // a presences entry yet at this point (Discord only
                        // guarantees one once a PRESENCE_UPDATE has actually
                        // been seen for them this session) - those default
                        // to "offline" until their first PRESENCE_UPDATE
                        // arrives, same as a real client briefly shows before
                        // its own presence subscription catches up.
                        // Whatever READY already knows about who is around.
                        // Used for the friends list and for direct message
                        // rosters alike: without it every conversation opens
                        // showing the other person offline until they happen
                        // to change status, which for somebody idle all day
                        // is never.
                        let presences: HashMap<&str, &str> = d["presences"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| Some((p["user"]["id"].as_str()?, p["status"].as_str().unwrap_or("offline"))))
                            .collect();

                        if let Some(relationships) = d["relationships"].as_array() {
                            let friends: Vec<Value> = relationships
                                .iter()
                                .filter(|r| r["type"].as_i64() == Some(1))
                                .filter_map(|r| {
                                    let user = &r["user"];
                                    let user_id = user["id"].as_str()?;
                                    Some(json!({
                                        "userId": user_id,
                                        "username": user["username"].as_str().unwrap_or("unknown"),
                                        "globalName": user["global_name"].as_str(),
                                        "avatarUrl": author_avatar_url(user),
                                        "status": presences.get(user_id).copied().unwrap_or("offline"),
                                    }))
                                })
                                .collect();
                            state.runtime.set_discord_friends(&account_id, friends);
                        }

                        let mut new_dm_buffers: Vec<(String, String)> = Vec::new();
                        if let Some(dms) = d["private_channels"].as_array() {
                            for ch in dms {
                                if let Some(pair) = register_dm_channel(state, &account_id, ch, &mut channel_map, &presences) {
                                    new_dm_buffers.push(pair);
                                }
                            }
                        }
                        state.runtime.set_conn_state(state, &account_id, ConnState::Connected, None);
                        tracing::info!("discord[{account_id}]: ready as {username}");

                        // For an account in relatively few guilds, READY's own
                        // `guilds` array already carries full guild objects
                        // (name + channels) directly - confirmed live, zero
                        // GUILD_CREATE dispatches ever arrived for a 4-guild
                        // account despite ready.guilds having all 4 with
                        // channels embedded. GUILD_CREATE (below) still
                        // exists as a fallback for accounts where Discord
                        // *does* send guilds separately (a documented
                        // behavior for large accounts) or a guild joined
                        // mid-session - register_guild_channels no-ops
                        // harmlessly on a guild it's already seen via either
                        // path (channel_map dedup).
                        if let Some(guilds) = d["guilds"].as_array() {
                            for g in guilds {
                                register_guild_channels(state, config, g, &mut channel_map).await;
                            }
                        }

                        // READY's own private_channels array isn't reliably
                        // complete for user accounts (confirmed live: two
                        // active DM conversations kept getting MESSAGE_UPDATE
                        // traffic for channel ids that never appeared there) -
                        // fetch the full list over REST as a follow-up rather
                        // than trusting the gateway snapshot alone.
                        match http_client().get(format!("{API_BASE}/users/@me/channels")).header("Authorization", &config.token).send().await {
                            Ok(resp) => {
                                let body = resp.text().await.unwrap_or_default();
                                match serde_json::from_str::<Vec<Value>>(&body) {
                                    Ok(channels) => {
                                        for ch in &channels {
                                            if let Some(pair) = register_dm_channel(state, &account_id, ch, &mut channel_map, &presences) {
                                                new_dm_buffers.push(pair);
                                            }
                                        }
                                    }
                                    Err(e) => tracing::warn!("discord[{account_id}]: parsing /users/@me/channels: {e}"),
                                }
                            }
                            Err(e) => tracing::warn!("discord[{account_id}]: fetching /users/@me/channels: {e}"),
                        }

                        spawn_backfill(state.clone(), config.token.clone(), config.user_id.clone(), config.display_name.clone(), new_dm_buffers);
                    }
                    "GUILD_CREATE" => {
                        register_guild_channels(state, config, d, &mut channel_map).await;
                    }
                    "MESSAGE_CREATE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, kind)) = channel_map.get(channel_id).cloned() else { continue };
                        let author = &d["author"];
                        let from = author["global_name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or_else(|| author["username"].as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let embeds = extract_embeds(d);
                        let attachments = extract_attachments(d);
                        let body = extract_body(d).unwrap_or_default();
                        if is_empty_message(&body, &embeds, &attachments) {
                            continue;
                        }
                        let body = resolve_mentions(&body, d, &config.user_id, config.display_name.as_deref());
                        let is_mention = mentions_own_user(d, &config.user_id);
                        let reply_to = extract_reply(d);
                        let real_msg_id = d["id"].as_str().map(|s| s.to_string());
                        let avatar_url = author_avatar_url(author);
                        // Discord's gateway echoes a user's own sent messages
                        // back through this same dispatch (that's how its
                        // official multi-device sync works) - unlike IRC,
                        // sendMessage below deliberately does NOT also
                        // record locally, so this is the only place a sent
                        // message gets appended, exactly once. Passing
                        // Discord's own id through (rather than letting
                        // record_message generate one) is what lets a later
                        // edit/delete/reaction on this exact message find it.
                        // Cache previews before the links expire, so this
                        // message still shows its pictures when it is read
                        // back out of scrollback tomorrow.
                        let thumb_target = (
                            model::buffer_id(&account_id, &buffer_name),
                            real_msg_id.clone().unwrap_or_default(),
                            attachments.clone(),
                        );
                        state.runtime.record_message(state, &account_id, &buffer_name, &kind, &from, &body, false, "chat", reply_to, real_msg_id, is_mention, avatar_url, embeds, attachments, None);
                        if !thumb_target.1.is_empty() {
                            cache_thumbnails(state.clone(), thumb_target.0, thumb_target.1, thumb_target.2);
                        }
                    }
                    "MESSAGE_UPDATE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let Some(msg_id) = d["id"].as_str() else { continue };
                        // Edits to embed-only unfurls (Discord re-sends the
                        // message once it's finished resolving a link
                        // preview) carry no `edited_timestamp` and nothing
                        // useful to update - only act on real content edits.
                        if d["edited_timestamp"].is_null() {
                            continue;
                        }
                        let embeds = extract_embeds(d);
                        let attachments = extract_attachments(d);
                        let body = extract_body(d).unwrap_or_default();
                        let body = resolve_mentions(&body, d, &config.user_id, config.display_name.as_deref());
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        state.runtime.update_message(state, &buffer_id, msg_id, &body, &embeds, &attachments);
                    }
                    "MESSAGE_DELETE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let Some(msg_id) = d["id"].as_str() else { continue };
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        state.runtime.delete_message(state, &buffer_id, msg_id);
                    }
                    "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let Some(msg_id) = d["message_id"].as_str() else { continue };
                        // Discord's own `<:name:id>` shorthand for custom
                        // emoji - unambiguous, and lets the frontend later
                        // regex-detect it to render the actual image via
                        // the emoji CDN. Plain unicode emoji (no id) pass
                        // through as-is and render natively in any font.
                        let emoji_name = d["emoji"]["name"].as_str().unwrap_or("?");
                        let emoji_key = match d["emoji"]["id"].as_str() {
                            Some(id) => format!("<:{emoji_name}:{id}>"),
                            None => emoji_name.to_string(),
                        };
                        let is_me = d["user_id"].as_str() == Some(config.user_id.as_str());
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        state.runtime.update_reaction(state, &buffer_id, msg_id, &emoji_key, is_me, t == "MESSAGE_REACTION_ADD");
                    }
                    "GUILD_MEMBER_LIST_UPDATE" => {
                        let Some(guild_id) = d["guild_id"].as_str() else { continue };
                        let Some(buffer_id) = state.runtime.discord_member_list_target(&account_id, guild_id) else { continue };
                        update_member_list(state, &buffer_id, d);
                    }

                    "VOICE_STATE_UPDATE" => {
                        let Some(user_id) = d["user_id"].as_str() else { continue };
                        let channel_id = d["channel_id"].as_str();
                        state.runtime.set_discord_voice_state(&account_id, user_id, channel_id, voice_member_name(d));
                        announce_voice_membership(state, &account_id, d["guild_id"].as_str(), channel_id);

                        if user_id == config.user_id {
                            // Our own move. The session id here is half of what
                            // a voice connection needs; VOICE_SERVER_UPDATE
                            // carries the other half.
                            state.runtime.set_discord_voice_self(&account_id, channel_id);
                            state.events.emit(
                                "discordVoiceState",
                                json!({
                                    "accountId": account_id,
                                    "channelId": channel_id,
                                    "sessionId": d["session_id"].as_str()
                                }),
                            );
                            super::discord_voice::note_voice_state(
                                state,
                                &account_id,
                                d["guild_id"].as_str(),
                                channel_id,
                                d["session_id"].as_str(),
                            )
                            .await;
                        } else if let (Some(ours), Some(theirs)) = (state.runtime.discord_voice_self(&account_id), channel_id) {
                            // Somebody else arrived where we are. Leaving is
                            // the daemon's job rather than the caller's: by the
                            // time a client could react it would already have
                            // been in a channel with a stranger.
                            if ours == theirs && state.voice.options(&account_id).solo {
                                tracing::info!("discord[{account_id}]: leaving voice - another user joined");
                                leave_voice(state, &account_id);
                                state.events.emit(
                                    "discordVoiceLeft",
                                    json!({ "accountId": account_id, "reason": "someone else joined", "userId": user_id }),
                                );
                            }
                        }
                    }

                    "VOICE_STATE_UPDATE_OLD" => {
                        let Some(user_id) = d["user_id"].as_str() else { continue };
                        let channel_id = d["channel_id"].as_str();
                        state.runtime.set_discord_voice_state(&account_id, user_id, channel_id, voice_member_name(d));
                        announce_voice_membership(state, &account_id, d["guild_id"].as_str(), channel_id);

                        if user_id == config.user_id {
                            // Our own move. The session id here is half of what
                            // a voice connection needs; VOICE_SERVER_UPDATE
                            // carries the other half.
                            state.runtime.set_discord_voice_self(&account_id, channel_id);
                            state.events.emit(
                                "discordVoiceState",
                                json!({
                                    "accountId": account_id,
                                    "channelId": channel_id,
                                    "sessionId": d["session_id"].as_str()
                                }),
                            );
                            super::discord_voice::note_voice_state(
                                state,
                                &account_id,
                                d["guild_id"].as_str(),
                                channel_id,
                                d["session_id"].as_str(),
                            )
                            .await;
                        } else if let (Some(ours), Some(theirs)) = (state.runtime.discord_voice_self(&account_id), channel_id) {
                            // Somebody else arrived where we are. Leaving is
                            // the daemon's job rather than the caller's: by the
                            // time a client could react it would already have
                            // been in a channel with a stranger.
                            if ours == theirs && state.voice.options(&account_id).solo {
                                tracing::info!("discord[{account_id}]: leaving voice - another user joined");
                                leave_voice(state, &account_id);
                                state.events.emit(
                                    "discordVoiceLeft",
                                    json!({ "accountId": account_id, "reason": "someone else joined", "userId": user_id }),
                                );
                            }
                        }
                    }

                    "VOICE_SERVER_UPDATE" => {
                        // The endpoint and token an audio implementation would
                        // open its own connection to. Reported rather than
                        // used: this build establishes the session only.
                        state.events.emit(
                            "discordVoiceServer",
                            json!({
                                "accountId": account_id,
                                "guildId": d["guild_id"].as_str(),
                                "endpoint": d["endpoint"].as_str(),
                                "hasToken": d["token"].as_str().is_some()
                            }),
                        );
                        super::discord_voice::note_voice_server(
                            state,
                            &account_id,
                            d["guild_id"].as_str(),
                            d["endpoint"].as_str(),
                            d["token"].as_str(),
                        )
                        .await;
                    }

                    "PRESENCE_UPDATE" => {
                        // Fires for guild members too, not just friends -
                        // update_discord_presence itself is the friends-only
                        // gate (a no-op, no event emitted, for any user_id
                        // not already in this account's friends list).
                        let Some(user_id) = d["user"]["id"].as_str() else { continue };
                        let status = d["status"].as_str().unwrap_or("offline");
                        if state.runtime.update_discord_presence(&account_id, user_id, status) {
                            state.events.emit("discordPresenceUpdate", json!({ "accountId": account_id, "userId": user_id, "status": status }));
                        }
                        // Also refresh any roster this person appears in, so
                        // an open channel's list follows them going online or
                        // away without waiting for a fresh subscription.
                        update_presence_in_rosters(state, user_id, status);
                    }
                    _ => {}
                }
            }
            7 => bail!("gateway requested a reconnect"),
            9 => bail!("session invalidated by gateway"),
            // Heartbeat ack - clears the flag heartbeat_task checks before
            // sending the *next* one, so a real ack landing between ticks
            // is exactly what keeps this connection from ever being
            // declared zombied in the first place.
            11 => ack_pending.store(false, Ordering::SeqCst),
            _ => {}
        }
    }
}

/// Builds the `message_reference` object Discord expects for a native
/// reply - the same mechanism its own clients use, not a quoted-text
/// convention layered on top.
fn reply_reference(reply_to_id: Option<&str>) -> Option<Value> {
    reply_to_id.map(|id| json!({ "message_id": id }))
}

/// REST message send - Discord's gateway is receive-only from the client's
/// perspective for user accounts; sending is always a plain HTTP POST.
pub async fn send_message(state: &AppState, buffer_id: &str, token: &str, body: &str, reply_to_id: Option<&str>) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let mut payload = json!({ "content": body });
    if let Some(reference) = reply_reference(reply_to_id) {
        payload["message_reference"] = reference;
    }
    let resp = http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/messages"))
        .header("Authorization", token)
        .json(&payload)
        .send()
        .await
        .context("sending Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// The "+" attachment button's backend: a single multipart POST carrying
/// both the message JSON (as a `payload_json` part) and the file bytes
/// (as a `files[0]` part) - Discord's documented way to send an attachment
/// inline with a message in one request, no separate upload-then-attach
/// step needed for files under the account's size limit.
pub async fn send_attachment(state: &AppState, buffer_id: &str, token: &str, body: &str, attachment_path: &str, reply_to_id: Option<&str>) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let path = std::path::Path::new(attachment_path);
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let bytes = tokio::fs::read(path).await.with_context(|| format!("reading {attachment_path}"))?;
    let mut payload = json!({ "content": body });
    if let Some(reference) = reply_reference(reply_to_id) {
        payload["message_reference"] = reference;
    }
    let form = reqwest::multipart::Form::new()
        .text("payload_json", payload.to_string())
        .part("files[0]", reqwest::multipart::Part::bytes(bytes).file_name(file_name));
    let resp = http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/messages"))
        .header("Authorization", token)
        .multipart(form)
        .send()
        .await
        .context("uploading Discord attachment")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// PATCH .../messages/{id} - editing your own message. Discord scopes
/// this to the message author (enforced server-side; there's no separate
/// permission check needed here).
pub async fn edit_message(state: &AppState, buffer_id: &str, token: &str, msg_id: &str, body: &str) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let resp = http_client()
        .patch(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}"))
        .header("Authorization", token)
        .json(&json!({ "content": body }))
        .send()
        .await
        .context("editing Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// DELETE .../messages/{id} - deleting your own message. Same
/// author-only enforcement as edit; also works for a moderator with
/// MANAGE_MESSAGES, which Discord itself decides, not this code.
pub async fn delete_message(state: &AppState, buffer_id: &str, token: &str, msg_id: &str) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let resp = http_client()
        .delete(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}"))
        .header("Authorization", token)
        .send()
        .await
        .context("deleting Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Discord's reaction endpoint wants a custom emoji as `name:id` (no
/// angle brackets, no leading `a:` animated marker), but every reaction
/// this app already tracks (extract_reactions above, the live
/// MESSAGE_REACTION_ADD/REMOVE handling) stores it wrapped as `<:name:id>`
/// - that's the one place besides here needing the raw form, so it's
/// unwrapped here rather than changing the stored shape everywhere else.
/// A plain Unicode emoji (no wrapper) passes through unchanged.
fn reaction_path_segment(emoji: &str) -> &str {
    emoji.strip_prefix("<:").or_else(|| emoji.strip_prefix("<a:")).and_then(|s| s.strip_suffix('>')).unwrap_or(emoji)
}

/// PUT/DELETE .../messages/{id}/reactions/{emoji}/@me - adding or
/// removing *our own* reaction (Discord's reaction endpoints are
/// per-reactor; there's no "set the count directly" concept). Built via
/// `Url::path_segments_mut` rather than hand-rolled string formatting so
/// the emoji segment - raw Unicode bytes for a standard emoji, a `:`
/// inside a custom one - gets properly percent-encoded rather than
/// landing in the URL unescaped.
pub async fn toggle_reaction(state: &AppState, buffer_id: &str, token: &str, msg_id: &str, emoji: &str, add: bool) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let mut url = url::Url::parse(&format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}/reactions")).context("building reaction URL")?;
    url.path_segments_mut().map_err(|_| anyhow!("reaction URL cannot be a base"))?.push(reaction_path_segment(emoji)).push("@me");

    let client = http_client();
    let req = if add { client.put(url) } else { client.delete(url) };
    let resp = req.header("Authorization", token).send().await.context("toggling Discord reaction")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        attachment_expired, default_avatar_url, dm_avatar_url, extract_attachments, extract_body,
        extract_embeds, is_empty_message, reaction_path_segment, stale_message_ids, thumbnail_source,
    };
    use crate::model::{Attachment, Message};
    use serde_json::json;

    fn msg(id: &str, urls: &[&str]) -> Message {
        Message {
            id: id.into(),
            buffer_id: "b".into(),
            from: "x".into(),
            body: String::new(),
            ts: 0,
            is_action: false,
            is_highlight: false,
            kind: "chat".into(),
            reply_to: None,
            edited: false,
            reactions: Vec::new(),
            is_own: false,
            avatar_url: None,
            embeds: Vec::new(),
            attachments: urls
                .iter()
                .map(|u| Attachment {
                    kind: "image".into(),
                    url: Some((*u).to_string()),
                    ..Default::default()
                })
                .collect(),
            sender_id: None,
        }
    }

    const PAST: &str = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=386d4380&is=1&hm=2";
    const FUTURE: &str = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=f4143f80&is=1&hm=2";

    #[test]
    fn only_messages_with_a_lapsed_link_need_re_signing() {
        let page = vec![msg("100", &[FUTURE]), msg("101", &[PAST]), msg("102", &[])];
        assert_eq!(stale_message_ids(&page), vec!["101"]);
    }

    #[test]
    fn a_message_is_stale_if_any_of_its_attachments_is() {
        let page = vec![msg("100", &[FUTURE, PAST])];
        assert_eq!(stale_message_ids(&page), vec!["100"]);
    }

    #[test]
    fn stale_ids_come_back_newest_first() {
        // Snowflakes: longer is newer, and equal lengths sort lexically.
        let page = vec![msg("100", &[PAST]), msg("1000", &[PAST]), msg("300", &[PAST])];
        assert_eq!(stale_message_ids(&page), vec!["1000", "300", "100"]);
    }

    #[test]
    fn locally_cached_media_never_looks_stale() {
        // Matrix and Sneedchat attachments carry a path, not a url, so a
        // non-Discord buffer costs nothing here.
        let mut m = msg("100", &[]);
        m.attachments = vec![Attachment {
            kind: "image".into(),
            path: Some("file:///cache/x.png".into()),
            ..Default::default()
        }];
        assert!(stale_message_ids(&[m]).is_empty());
    }

    #[test]
    fn reads_the_expiry_out_of_a_signed_cdn_link() {
        // ex= is a hex unix timestamp. Year 2000 is long gone; year 2100 is not.
        let past = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=386d4380&is=1&hm=2";
        let future = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=f4143f80&is=1&hm=2";
        assert!(attachment_expired(past));
        assert!(!attachment_expired(future));
        // An unsigned link has no expiry to read, so it is never "expired".
        assert!(!attachment_expired("https://example.com/a.png"));
    }

    #[test]
    fn everyone_has_a_face_even_without_an_avatar() {
        // A conversation list is a list of faces; one entry showing a letter
        // instead reads as broken rather than as a person with no picture.
        let modern = json!({ "id": "1339667204756475924", "discriminator": "0" });
        let url = default_avatar_url(&modern).expect("a modern account still has a default");
        assert!(url.starts_with("https://cdn.discordapp.com/embed/avatars/"), "unexpected: {url}");
        let index: u64 = url.trim_start_matches("https://cdn.discordapp.com/embed/avatars/").trim_end_matches(".png").parse().unwrap();
        assert!(index < 6, "modern accounts pick one of six, got {index}");

        // The old scheme derives it from the discriminator instead.
        let legacy = json!({ "id": "80351110224678912", "discriminator": "0007" });
        let url = default_avatar_url(&legacy).unwrap();
        assert!(url.ends_with("/2.png"), "0007 % 5 is 2, got {url}");
    }

    #[test]
    fn a_direct_message_is_headed_by_the_other_person() {
        // And a group has no single face, so it gets none rather than an
        // arbitrary one of several.
        let one = json!({ "recipients": [{ "id": "1", "avatar": "abc", "discriminator": "0" }] });
        assert_eq!(
            dm_avatar_url(&one).as_deref(),
            Some("https://cdn.discordapp.com/avatars/1/abc.png")
        );
        let group = json!({ "recipients": [{ "id": "1", "avatar": "a" }, { "id": "2", "avatar": "b" }] });
        assert!(dm_avatar_url(&group).is_none(), "a group DM was given one member's face");
    }

    #[test]
    fn builds_a_proxied_thumbnail_preserving_aspect_ratio() {
        let src = thumbnail_source("https://cdn.discordapp.com/attachments/1/2/a.png?ex=1", 1000, 500)
            .expect("should proxy a Discord link");
        // Resizing goes through the media proxy, not the raw CDN host.
        assert!(src.starts_with("https://media.discordapp.net/"), "{src}");
        assert!(src.contains("width=480"), "{src}");
        assert!(src.contains("height=240"), "{src}");
        // The existing query string is kept, not replaced.
        assert!(src.contains("ex=1"), "{src}");
    }

    #[test]
    fn taller_than_wide_is_bounded_by_height() {
        let src = thumbnail_source("https://media.discordapp.net/attachments/1/2/a.png", 500, 1000).unwrap();
        assert!(src.contains("width=240"), "{src}");
        assert!(src.contains("height=480"), "{src}");
    }

    #[test]
    fn only_discord_hosted_images_are_proxied() {
        assert!(thumbnail_source("https://example.com/a.png", 10, 10).is_none());
    }

    #[test]
    fn keeps_discord_attachment_metadata_instead_of_flattening_it_to_a_url() {
        let d = json!({
            "content": "look at this",
            "attachments": [{
                "url": "https://cdn.discordapp.com/attachments/1/2/cat.png",
                "filename": "cat.png",
                "size": 12345,
                "width": 800,
                "height": 600,
                "content_type": "image/png"
            }]
        });
        let atts = extract_attachments(&d);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].kind, "image");
        assert_eq!(atts[0].filename.as_deref(), Some("cat.png"));
        assert_eq!(atts[0].mimetype.as_deref(), Some("image/png"));
        assert_eq!(atts[0].size, Some(12345));
        assert_eq!((atts[0].width, atts[0].height), (Some(800), Some(600)));
        // Directly loadable, so no local copy is made.
        assert!(atts[0].path.is_none());

        // The body keeps what the user actually typed - the attachment URL is
        // no longer appended to it.
        assert_eq!(extract_body(&d).as_deref(), Some("look at this"));
    }

    #[test]
    fn classifies_non_image_attachments_by_content_type() {
        let d = json!({ "attachments": [
            { "url": "https://cdn/x.mp4", "content_type": "video/mp4" },
            { "url": "https://cdn/x.ogg", "content_type": "audio/ogg" },
            { "url": "https://cdn/x.zip", "content_type": "application/zip" },
            { "url": "https://cdn/x.bin" }
        ]});
        let atts = extract_attachments(&d);
        let kinds: Vec<&str> = atts.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(kinds, ["video", "audio", "file", "file"]);
    }

    #[test]
    fn an_attachment_only_message_still_has_a_renderable_body_or_attachments() {
        // No text at all: the body is now empty rather than being the URL, so
        // the attachment list is the only thing carrying the message.
        let d = json!({ "content": "", "attachments": [{ "url": "https://cdn/a.png", "content_type": "image/png" }] });
        assert!(extract_body(&d).is_none());
        assert_eq!(extract_attachments(&d).len(), 1);
    }

    #[test]
    fn an_uncaptioned_picture_is_not_an_empty_message() {
        // The regression this guards: moving attachment URLs out of the body
        // made an uncaptioned image look empty, and both the live and history
        // paths dropped it instead of storing it.
        let d = json!({ "content": "", "attachments": [{ "url": "https://cdn/a.png", "content_type": "image/png" }] });
        let body = extract_body(&d).unwrap_or_default();
        assert!(!is_empty_message(&body, &extract_embeds(&d), &extract_attachments(&d)));
    }

    #[test]
    fn a_message_with_no_text_embeds_or_attachments_is_empty() {
        // Pin notices and thread markers really do have nothing to show.
        let d = json!({ "content": "", "attachments": [], "embeds": [] });
        let body = extract_body(&d).unwrap_or_default();
        assert!(is_empty_message(&body, &extract_embeds(&d), &extract_attachments(&d)));
    }


    #[test]
    fn unwraps_a_static_custom_emoji() {
        assert_eq!(reaction_path_segment("<:pepege:123456789>"), "pepege:123456789");
    }

    #[test]
    fn unwraps_an_animated_custom_emoji() {
        assert_eq!(reaction_path_segment("<a:vibing:987654321>"), "vibing:987654321");
    }

    #[test]
    fn leaves_a_plain_unicode_emoji_unchanged() {
        assert_eq!(reaction_path_segment("🔥"), "🔥");
    }
}

/// Discord's presence shape for one of our statuses.
///
/// Discord's own vocabulary matches ours for both values, so they pass
/// through unchanged - the mapping only exists so an unrecognised value
/// cannot put the account into some unintended state.
fn presence_payload(status: &str) -> serde_json::Value {
    let discord_status = match status {
        "idle" => "idle",
        _ => "online",
    };
    json!({ "status": discord_status, "since": 0, "activities": [], "afk": status == "idle" })
}

/// Sets an account's status.
///
/// Two places have to agree. The gateway opcode changes how this *session*
/// presents right now, which is what other people see immediately; the account
/// setting is what Discord treats as the user's chosen status, and is what
/// every new session starts from. Setting only the opcode leaves the account
/// still holding its old choice - which is how an account left on "invisible"
/// keeps reverting - and setting only the account is slow to show.
pub async fn apply_status(state: &AppState, account_id: &str, status: &str) -> bool {
    if let Some(sender) = state.runtime.discord_gateway_sender(account_id) {
        // Opcode 3 is presence update.
        let _ = sender.send(json!({ "op": 3, "d": presence_payload(status) }).to_string());
    }

    let Some(config) = state.accounts.get_discord(account_id) else { return false };
    let discord_status = match status {
        "idle" => "idle",
        _ => "online",
    };
    match http_client()
        .patch(format!("{API_BASE}/users/@me/settings"))
        .header("Authorization", &config.token)
        .json(&json!({ "status": discord_status }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) => {
            tracing::debug!("discord: setting status returned HTTP {}", resp.status());
            false
        }
        Err(e) => {
            tracing::debug!("discord: setting status failed: {e}");
            false
        }
    }
}

/// Asks Discord for a channel's member list.
///
/// A user token cannot use REQUEST_GUILD_MEMBERS the way a bot does - the
/// member list is instead a "lazy guild" subscription (opcode 14) naming the
/// channel and the ranges of the list to send, which the server answers with
/// GUILD_MEMBER_LIST_UPDATE dispatches. This is what Discord's own client
/// does when you open a channel, which is also why the roster only exists for
/// channels somebody is actually looking at.
pub fn request_member_list(state: &AppState, buffer_id: &str) -> bool {
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return false };
    let account_id = buffer.account_id;
    let Some(sender) = state.runtime.discord_gateway_sender(&account_id) else { return false };
    let Some(channel_id) = state.runtime.get_discord_channel(buffer_id) else { return false };
    let Some(guild_id) = state.runtime.get_discord_guild(buffer_id) else {
        // A DM has no guild and no member list to subscribe to; its
        // participants are already known from the channel itself.
        return false;
    };

    // The reply names the guild and a permissions-derived list id, never the
    // channel, so remember which buffer this was for.
    state.runtime.set_discord_member_list_target(&account_id, &guild_id, buffer_id);

    // Ranges are 100-member windows; one covers any channel we would show.
    sender
        .send(
            json!({
                "op": 14,
                "d": {
                    "guild_id": guild_id,
                    "typing": true,
                    "threads": false,
                    "activities": true,
                    "channels": { channel_id: [[0, 99]] }
                }
            })
            .to_string(),
        )
        .is_ok()
}

/// Rebuilds a channel's roster from a GUILD_MEMBER_LIST_UPDATE.
///
/// The list arrives as a series of ops over a windowed view: SYNC carries a
/// whole range of entries, while INSERT/UPDATE/DELETE adjust it as people come
/// and go. Entries are either a group header - a role name, or the online and
/// offline buckets - or a member.
///
/// Only SYNC is acted on. The incremental ops move members between roles and
/// buckets by position within a list this client does not otherwise model, and
/// applying them half-understood would corrupt the roster; re-opening the
/// channel asks for a fresh SYNC, which is what Discord's own client does when
/// its view changes.
fn update_member_list(state: &AppState, buffer_id: &str, d: &Value) {
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let mut members: Vec<Value> = Vec::new();
    let mut saw_sync = false;

    for op in d["ops"].as_array().into_iter().flatten() {
        if op["op"].as_str() != Some("SYNC") {
            continue;
        }
        saw_sync = true;
        for item in op["items"].as_array().into_iter().flatten() {
            let Some(member) = item.get("member") else { continue };
            let user = &member["user"];
            let Some(user_id) = user["id"].as_str() else { continue };
            // Server nickname first, then the account's chosen display name,
            // then the raw username - the same order Discord itself shows.
            let nick = member["nick"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| user["global_name"].as_str().filter(|s| !s.is_empty()))
                .or_else(|| user["username"].as_str())
                .unwrap_or("unknown");
            let status = member["presence"]["status"].as_str().unwrap_or("offline");
            // Names are learned here and remembered for anywhere they are
            // needed. Voice is the case that matters: Discord attaches a
            // member to a voice state only when someone moves, so anyone
            // already sitting in a channel when we connect would otherwise be
            // shown as a raw snowflake forever.
            state.runtime.remember_discord_name(&account_id, user_id, nick);
            members.push(json!({
                "nick": nick,
                "userId": user_id,
                "prefix": "",
                // Only actually offline counts as away. Idle and do-not-disturb
                // are still connected - Discord lists them with everyone else
                // who is present, and their own status word says the rest.
                "away": status == "offline",
                "status": status
            }));
        }
    }

    if !saw_sync {
        return;
    }

    members.sort_by(|a, b| {
        let (an, bn) = (a["nick"].as_str().unwrap_or(""), b["nick"].as_str().unwrap_or(""));
        an.to_lowercase().cmp(&bn.to_lowercase()).then_with(|| an.cmp(bn))
    });
    let member_list = json!(members);
    state.runtime.set_presence(buffer_id, member_list.clone());
    state.events.emit("presenceChange", json!({ "bufferId": buffer_id, "members": member_list }));
}

/// A DM's roster: whoever is in it.
///
/// Unlike a guild channel this needs no subscription - Discord hands the
/// recipients over with the channel itself. Their status is not included
/// there, so everyone starts unknown and PRESENCE_UPDATE fills it in; for a
/// friend that is usually immediate, since READY already carried it.
fn set_dm_presence(state: &AppState, buffer_id: &str, channel: &Value, presences: &HashMap<&str, &str>) {
    let mut members: Vec<Value> = channel["recipients"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let user_id = r["id"].as_str()?;
            let nick = r["global_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| r["username"].as_str())
                .unwrap_or("unknown");
            let status = presences.get(user_id).copied().unwrap_or("offline");
            Some(json!({ "nick": nick, "userId": user_id, "prefix": "", "away": status == "offline", "status": status }))
        })
        .collect();
    if members.is_empty() {
        return;
    }
    members.sort_by(|a, b| a["nick"].as_str().unwrap_or("").to_lowercase().cmp(&b["nick"].as_str().unwrap_or("").to_lowercase()));
    let member_list = json!(members);
    state.runtime.set_presence(buffer_id, member_list.clone());
    state.events.emit("presenceChange", json!({ "bufferId": buffer_id, "members": member_list }));
}

/// Updates one person's status wherever they are currently listed.
///
/// PRESENCE_UPDATE arrives for anyone the account can see, which is far more
/// people than are in any open roster - so this only touches buffers that
/// already list them, and says nothing otherwise.
fn update_presence_in_rosters(state: &AppState, user_id: &str, status: &str) {
    for buffer in state.runtime.list_buffers() {
        let Some(existing) = state.runtime.get_presence(&buffer.id) else { continue };
        let Some(members) = existing.as_array() else { continue };
        if !members.iter().any(|m| m["userId"].as_str() == Some(user_id)) {
            continue;
        }
        let updated: Vec<Value> = members
            .iter()
            .map(|m| {
                if m["userId"].as_str() != Some(user_id) {
                    return m.clone();
                }
                let mut m = m.clone();
                m["status"] = json!(status);
                // Same rule the initial sync uses; the two disagreeing would
                // move someone between groups on their next presence change.
                m["away"] = json!(status == "offline");
                m
            })
            .collect();
        let member_list = json!(updated);
        state.runtime.set_presence(&buffer.id, member_list.clone());
        state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": member_list }));
    }
}

/// What to call the person a voice state belongs to.
///
/// Discord attaches the member to a voice state, which matters because
/// somebody sitting in a voice channel is frequently in no member list the
/// client has loaded - a nickname first, since that is what they chose to be
/// called here, then the display name, then the account name.
fn voice_member_name(vs: &Value) -> Option<&str> {
    vs["member"]["nick"]
        .as_str()
        .or_else(|| vs["member"]["user"]["global_name"].as_str())
        .or_else(|| vs["member"]["user"]["username"].as_str())
        .filter(|n| !n.is_empty())
}

/// Tells clients that a voice channel's membership changed.
///
/// Carries the guild rather than the channel because a client showing a
/// channel list needs to know that list is stale, and somebody leaving one
/// channel for another changes two of its rows at once.
fn announce_voice_membership(state: &AppState, account_id: &str, guild_id: Option<&str>, channel_id: Option<&str>) {
    // A leave carries no guild, so it is recovered from the channel that was
    // left; without this, leaving would never refresh anyone's list.
    let guild = guild_id
        .map(String::from)
        .or_else(|| channel_id.and_then(|c| state.runtime.discord_guild_of_voice_channel(account_id, c)));
    let Some(guild_id) = guild else { return };
    state.events.emit(
        "voiceMembershipChanged",
        json!({ "accountId": account_id, "guildId": guild_id }),
    );
}

/// Joins a voice channel under the given options.
///
/// With `solo` set - the default, and what a caller should use against a
/// server full of strangers - an occupied channel is refused outright and
/// anyone arriving later ends the session. The check lives here rather than in
/// the caller so it cannot be forgotten, but it is an argument rather than a
/// law: in a guild the user controls, a listener joining is the point.
///
/// Joining is opcode 4 on the main gateway, which the server answers with
/// VOICE_STATE_UPDATE (our session id) and VOICE_SERVER_UPDATE (where to
/// connect and with what token).
pub fn join_voice(
    state: &AppState,
    account_id: &str,
    guild_id: Option<&str>,
    channel_id: &str,
    options: super::discord_voice::VoiceOptions,
) -> Result<()> {
    let config = state.accounts.get_discord(account_id).context("no such Discord account")?;
    let sender = state.runtime.discord_gateway_sender(account_id).context("account is not connected")?;

    if options.solo {
        let occupants = state.runtime.discord_voice_occupants(account_id, channel_id, &config.user_id);
        if !occupants.is_empty() {
            bail!("channel is not empty - {} already in it", occupants.len());
        }
    }
    // Recorded before the join, since the handshake it triggers can complete
    // before this function returns.
    state.voice.set_options(account_id, options);

    sender.send(
        json!({
            "op": 4,
            "d": {
                // Null for a one-to-one call: a DM belongs to no guild, and
                // sending one anyway gets the frame ignored.
                "guild_id": guild_id,
                "channel_id": channel_id,
                // A session that carries no audio joins muted and deafened:
                // showing an open microphone to a room that cannot hear one
                // would misrepresent what is happening.
                "self_mute": !options.transmit,
                "self_deaf": !options.transmit,
                // Discord's own client always sends this field; omitting it
                // gets the frame accepted and then ignored, with no error.
                "self_video": false
            }
        })
        .to_string(),
    )?;
    Ok(())
}

/// Calls someone directly, opening the conversation if there isn't one.
///
/// A one-to-one call is a voice connection to a DM channel, which is most of
/// what makes it different: no guild, and nobody is in it until the other
/// person picks up. Joining alone is silent, so the ring is a separate request
/// - without it you are sitting in an empty channel they never hear about.
pub async fn call_user(state: &AppState, account_id: &str, user_id: &str) -> Result<String> {
    let buffer_id = open_dm(state, account_id, user_id).await?;
    let channel_id = state.runtime.get_discord_channel(&buffer_id).context("the DM has no channel")?;
    start_call(state, account_id, &channel_id).await?;
    Ok(buffer_id)
}

/// Joins a DM's voice channel and rings whoever else is in it.
pub async fn start_call(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    // Never solo-only: a call whose whole purpose is somebody else joining
    // cannot also refuse to be joined.
    let options = super::discord_voice::VoiceOptions { solo: false, transmit: true };
    join_voice(state, account_id, None, channel_id, options)?;
    ring(state, account_id, channel_id).await
}

/// Makes the other end's client ring.
///
/// Sent after joining rather than before: ringing a call you are not yet in is
/// answered by Discord with a call that ends the moment they accept it.
pub async fn ring(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/call/ring"))
        .header("Authorization", &cfg.token)
        // A null recipient list means everyone in the conversation, which for
        // a one-to-one DM is the one person there is.
        .json(&json!({ "recipients": Value::Null }))
        .send()
        .await
        .context("ringing")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Stops a call ringing, for hanging up before it is answered.
pub async fn stop_ringing(state: &AppState, account_id: &str, channel_id: &str) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let _ = http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/call/stop-ringing"))
        .header("Authorization", &cfg.token)
        .json(&json!({ "recipients": Value::Null }))
        .send()
        .await;
    Ok(())
}

/// Tells the server this account's microphone or output has been silenced.
///
/// Separate from actually stopping the audio, and both are needed: closing the
/// microphone without saying so leaves everyone else looking at a live
/// microphone icon wondering why you have gone quiet.
pub fn announce_voice_flags(state: &AppState, account_id: &str, muted: bool, deafened: bool) -> bool {
    let Some(sender) = state.runtime.discord_gateway_sender(account_id) else { return false };
    let Some((guild_id, channel_id)) = state.voice.current_channel(account_id) else { return false };
    sender
        .send(
            json!({
                "op": 4,
                "d": {
                    // Null on a one-to-one call, which belongs to no guild.
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "self_mute": muted,
                    "self_deaf": deafened,
                    "self_video": false
                }
            })
            .to_string(),
        )
        .is_ok()
}

/// Leaves whatever voice channel this account is in. Safe to call when in none.
pub fn leave_voice(state: &AppState, account_id: &str) -> bool {
    let Some(sender) = state.runtime.discord_gateway_sender(account_id) else { return false };
    // Hanging up before they answer has to stop the ringing too, or their
    // phone goes on buzzing for a call that no longer exists.
    if let Some((guild, channel)) = state.voice.current_channel(account_id) {
        if guild.is_none() {
            let (s2, a2, c2) = (state.clone(), account_id.to_string(), channel);
            tokio::spawn(async move {
                let _ = stop_ringing(&s2, &a2, &c2).await;
            });
        }
    }
    state.runtime.set_discord_voice_self(account_id, None);
    sender
        .send(json!({ "op": 4, "d": { "guild_id": null, "channel_id": null, "self_mute": true, "self_deaf": true } }).to_string())
        .is_ok()
}
