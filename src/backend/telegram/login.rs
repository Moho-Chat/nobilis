//! Adding an account: the sign-in a person takes part in.
//!
//! The person types an `api_id`, an `api_hash` and a phone number. Telegram sends a code to the account's other
//! sessions (or by SMS), the person types it, and if the account has a cloud password that is asked for next.
//! Neither the code nor the password is kept: what the account keeps is the session Telegram issued.
//!
//! Like Steam's, it is a conversation that can take minutes, so it runs as its own task and talks to the window
//! through events:
//!
//! - `telegramLoginStatus` - what it is doing, for a line of text
//! - `telegramLoginNeeds` - what Telegram is asking for: `step` is `code` or `password` (with the password's hint),
//!   and `error` says why the last answer was refused
//! - `telegramLoginQr` - a picture to scan, for a sign-in by QR code (below)
//! - `telegramLoginResult` - done, with the account, or the reason it failed
//!
//! and the window answers with `submitTelegramCode` and `submitTelegramPassword`.
//!
//! # By QR code
//!
//! The other way in is to scan a code with Telegram on a phone that is already signed in (Settings, Devices, Link
//! Desktop Device): no number is typed and no code is sent. Telegram gives a login token that lasts about half a
//! minute; it is shown as a QR code and asked for again when it runs out, and Telegram says when it was scanned
//! (`updateLoginToken`; the token is asked for again every so often too, in case that update does not reach a
//! connection that has no account yet). The scanned token comes back as the account, or - when the account lives
//! in another datacenter - as a pointer to it, where the token is handed in and the key made there becomes the
//! session's home. An account with a cloud password still asks for it, after the scan.

use super::link::{self, Link};
use super::session::{self, FileSession};
use crate::accounts::TelegramAccountConfig;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use grammers_client::SignInError;
use grammers_client::tl;
use grammers_session::Session;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a sign-in may wait for a person. Telegram's own code is good for a few minutes, and a form left open
/// overnight should not leave a task.
const PATIENCE: Duration = Duration::from_secs(5 * 60);

/// What the window can hand to a sign-in that is waiting.
enum Answer {
    Code(String),
    Password(String),
}

/// The sign-ins waiting for an answer, by id, and the way to give each one.
fn waiting() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<Answer>>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<Answer>>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

fn give(login_id: &str, answer: Answer) -> Result<()> {
    let sender = waiting().lock().unwrap().get(login_id).cloned().ok_or_else(|| anyhow!("that sign-in is not waiting for anything any more"))?;
    sender.send(answer).map_err(|_| anyhow!("that sign-in has ended"))
}

/// Gives a sign-in the code Telegram sent.
pub fn submit_code(login_id: &str, code: &str) -> Result<()> {
    // Telegram's own client shows the code as "12345"; a pasted one may carry spaces or a dash.
    let code: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if code.is_empty() {
        bail!("type the code first");
    }
    give(login_id, Answer::Code(code))
}

/// Gives a sign-in the account's cloud password.
pub fn submit_password(login_id: &str, password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("type the password first");
    }
    give(login_id, Answer::Password(password.to_string()))
}

/// Stops a sign-in that is waiting, because the form was closed.
pub fn cancel(login_id: &str) {
    waiting().lock().unwrap().remove(login_id);
}

/// A phone number as Telegram wants it: digits with a `+` in front.
pub fn normalize_phone(typed: &str) -> Option<String> {
    let digits: String = typed.chars().filter(|c| c.is_ascii_digit()).collect();
    // The shortest numbers in use are about seven digits and the longest fifteen.
    (7..=15).contains(&digits.len()).then(|| format!("+{digits}"))
}

/// How the account is being signed in.
enum Method {
    /// A phone number, then the code sent to it.
    Phone(String),
    /// A QR code scanned from a phone that is already signed in.
    Qr,
}

/// Starts a sign-in by phone number. Answers at once; the rest is events.
pub fn start(state: AppState, login_id: String, api_id: i32, api_hash: String, phone: String) {
    begin(state, login_id, api_id, api_hash, Method::Phone(phone));
}

/// Starts a sign-in by QR code. Answers at once; the rest is events.
pub fn start_qr(state: AppState, login_id: String, api_id: i32, api_hash: String) {
    begin(state, login_id, api_id, api_hash, Method::Qr);
}

fn begin(state: AppState, login_id: String, api_id: i32, api_hash: String, method: Method) {
    let (tx, rx) = mpsc::unbounded_channel();
    waiting().lock().unwrap().insert(login_id.clone(), tx);
    tokio::spawn(async move {
        let outcome = flow(&state, &login_id, api_id, &api_hash, method, rx).await;
        waiting().lock().unwrap().remove(&login_id);
        match outcome {
            Ok(Some(saved)) => {
                let account = crate::accounts::telegram_account_to_json(&saved, "connecting");
                super::connect::spawn(state.clone(), saved);
                state.events.emit("telegramLoginResult", json!({ "loginId": login_id, "success": true, "account": account }));
            }
            // The form was closed; nobody is waiting for an answer.
            Ok(None) => {}
            Err(e) => {
                let error = format!("{e:#}");
                tracing::info!("telegram: sign-in {login_id}: {error}");
                state.events.emit("telegramLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
            }
        }
    });
}

fn status(state: &AppState, login_id: &str, text: &str) {
    state.events.emit("telegramLoginStatus", json!({ "loginId": login_id, "text": text }));
}

fn needs(state: &AppState, login_id: &str, step: &str, hint: Option<&str>, error: Option<&str>) {
    state.events.emit("telegramLoginNeeds", json!({ "loginId": login_id, "step": step, "hint": hint, "error": error }));
}

/// What was typed, or `None` when the form was closed or nobody answered in time.
async fn answer(rx: &mut mpsc::UnboundedReceiver<Answer>) -> Option<Answer> {
    tokio::time::timeout(PATIENCE, rx.recv()).await.ok().flatten()
}

async fn flow(state: &AppState, login_id: &str, api_id: i32, api_hash: &str, method: Method, mut rx: mpsc::UnboundedReceiver<Answer>) -> Result<Option<TelegramAccountConfig>> {
    if api_id <= 0 || api_hash.trim().is_empty() {
        bail!("an api_id and an api_hash are needed: get them at my.telegram.org/apps");
    }
    let typed_phone = match &method {
        Method::Phone(phone) => Some(normalize_phone(phone).ok_or_else(|| anyhow!("that does not look like a phone number; type it with its country code"))?),
        Method::Qr => None,
    };
    status(state, login_id, "Connecting to Telegram");
    let session = Arc::new(FileSession::fresh(session::path_for_login(login_id)));
    let mut link = link::open(state, &crate::net::route::pending_key("telegram"), api_id, session.clone()).await?;
    let outcome = match &typed_phone {
        Some(phone) => signed_in(state, login_id, &link, api_hash, phone, &mut rx).await,
        None => scanned(state, login_id, &mut link, api_id, api_hash, &mut rx).await,
    };
    let user = match outcome {
        Ok(Some(user)) => user,
        other => {
            link.close().await;
            session.delete();
            return other.map(|_| None);
        }
    };
    let config = TelegramAccountConfig {
        api_id,
        api_hash: api_hash.trim().to_string(),
        // What was typed, or - signed in by QR code, where nothing was - what Telegram says the number is.
        phone: typed_phone.or_else(|| user.phone().map(|p| format!("+{}", p.trim_start_matches('+')))).unwrap_or_default(),
        user_id: user.id().bare_id_unchecked(),
        username: user.username().map(str::to_string),
        name: user.full_name(),
        use_tor: crate::net::route::router().wanted(&crate::net::route::pending_key("telegram")),
        ..Default::default()
    };
    // The session is this account's now: under its own name, and written before the connection that made it closes.
    session.move_to(session::path_for(config.user_id)).context("saving the session")?;
    link.close().await;
    let saved = state.accounts.add_telegram(config)?;
    Ok(Some(saved))
}

/// The conversation with Telegram, up to being signed in.
async fn signed_in(state: &AppState, login_id: &str, link: &Link, api_hash: &str, phone: &str, rx: &mut mpsc::UnboundedReceiver<Answer>) -> Result<Option<grammers_client::peer::User>> {
    let client = &link.client;
    if client.is_authorized().await.context("asking Telegram whether this session is signed in")? {
        bail!("this session is already signed in");
    }
    status(state, login_id, "Asking Telegram to send a code");
    let token = client.request_login_code(phone, api_hash).await.map_err(|e| anyhow!("{}", explain(&e.to_string())))?;
    needs(state, login_id, "code", None, None);
    loop {
        let Some(given) = answer(rx).await else { return Ok(None) };
        match given {
            Answer::Code(code) => match client.sign_in(&token, &code).await {
                Ok(user) => return Ok(Some(user)),
                Err(SignInError::PasswordRequired(password)) => {
                    let hint = password.hint().map(str::to_string);
                    needs(state, login_id, "password", hint.as_deref(), None);
                    return ask_password(state, login_id, client, password, rx).await;
                }
                Err(SignInError::InvalidCode) => needs(state, login_id, "code", None, Some("That code is not right. Check it and try again")),
                Err(SignInError::SignUpRequired) => bail!("this number has no Telegram account. Make one with Telegram's own app first"),
                Err(e) => bail!("{}", explain(&e.to_string())),
            },
            Answer::Password(_) => needs(state, login_id, "code", None, Some("Telegram has not asked for a password")),
        }
    }
}

/// How long the QR code is offered before the sign-in gives up: a form left open should not keep a task.
const QR_PATIENCE: Duration = Duration::from_secs(10 * 60);

/// How often the token is asked for again while nothing has been heard of a scan.
const QR_POLL: Duration = Duration::from_secs(15);

/// What asking for the login token came to.
#[derive(Debug)]
enum Exported {
    /// A token to show, and when it stops being good (seconds since the epoch, Telegram's clock).
    Token { token: Vec<u8>, expires: i32 },
    /// Scanned and approved: the account.
    Signed(Box<tl::types::auth::Authorization>),
    /// Scanned, but the account has a cloud password.
    NeedsPassword,
}

/// The link a phone's camera opens in Telegram.
pub fn qr_address(token: &[u8]) -> String {
    use base64::Engine;
    format!("tg://login?token={}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token))
}

fn qr_path(login_id: &str) -> std::path::PathBuf {
    crate::media_cache::transient_dir().join(format!("moho-telegram-qr-{}.png", login_id.replace(|c: char| !c.is_ascii_alphanumeric(), "-")))
}

/// Asks Telegram for the login token, and follows it to the account's datacenter if it was scanned.
async fn export_token(client: &grammers_client::Client, session: &FileSession, api_id: i32, api_hash: &str) -> Result<Exported> {
    let asked = client.invoke(&tl::functions::auth::ExportLoginToken { api_id, api_hash: api_hash.to_string(), except_ids: Vec::new() }).await;
    let answer = match asked {
        Ok(answer) => answer,
        Err(e) if e.is("SESSION_PASSWORD_NEEDED") => return Ok(Exported::NeedsPassword),
        Err(e) => bail!("{}", explain(&e.to_string())),
    };
    match answer {
        tl::enums::auth::LoginToken::Token(t) => Ok(Exported::Token { token: t.token, expires: t.expires }),
        tl::enums::auth::LoginToken::Success(s) => signed(s.authorization),
        // The account is kept in another datacenter: the token is handed in there, and the key made there is the
        // session's from then on.
        tl::enums::auth::LoginToken::MigrateTo(m) => {
            let imported = client.invoke_in_dc(m.dc_id, &tl::functions::auth::ImportLoginToken { token: m.token }).await;
            match imported {
                Ok(tl::enums::auth::LoginToken::Success(s)) => {
                    session.set_home_dc_id(m.dc_id).await.map_err(|e| anyhow!("{e}"))?;
                    signed(s.authorization)
                }
                Ok(_) => bail!("Telegram did not finish the sign-in in the account's datacenter"),
                Err(e) if e.is("SESSION_PASSWORD_NEEDED") => {
                    session.set_home_dc_id(m.dc_id).await.map_err(|e| anyhow!("{e}"))?;
                    Ok(Exported::NeedsPassword)
                }
                Err(e) => bail!("{}", explain(&e.to_string())),
            }
        }
    }
}

fn signed(authorization: tl::enums::auth::Authorization) -> Result<Exported> {
    match authorization {
        tl::enums::auth::Authorization::Authorization(a) => Ok(Exported::Signed(Box::new(a))),
        tl::enums::auth::Authorization::SignUpRequired(_) => bail!("this number has no Telegram account. Make one with Telegram's own app first"),
    }
}

/// What the library does when it signs in by code, which it does not do for a login token: the account's own entry
/// in the session, and where its updates stand.
async fn settle(client: &grammers_client::Client, session: &FileSession, authorization: tl::types::auth::Authorization) -> Result<grammers_client::peer::User> {
    use grammers_session::types::{PeerInfo, UpdateState, UpdatesState};
    let update_state = client.invoke(&tl::functions::updates::GetState {}).await.ok();
    let user = grammers_client::peer::User::from_raw(client, authorization.user);
    let auth = user.to_ref().await.map_err(|e| anyhow!("{e}"))?.context("Telegram did not say who signed in")?.auth;
    session
        .cache_peer(PeerInfo::User { id: user.id().bare_id_unchecked(), auth: Some(auth), bot: Some(user.is_bot()), is_self: Some(true) })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(tl::enums::updates::State::State(state)) = update_state {
        session
            .set_update_state(UpdateState::All(UpdatesState { pts: state.pts, qts: state.qts, date: state.date, seq: state.seq, channels: Vec::new() }))
            .await
            .map_err(|e| anyhow!("{e}"))?;
    }
    Ok(user)
}

/// The conversation with Telegram, up to being signed in by QR code.
async fn scanned(state: &AppState, login_id: &str, link: &mut Link, api_id: i32, api_hash: &str, rx: &mut mpsc::UnboundedReceiver<Answer>) -> Result<Option<grammers_client::peer::User>> {
    let client = link.client.clone();
    let session = link.session.clone();
    if client.is_authorized().await.context("asking Telegram whether this session is signed in")? {
        bail!("this session is already signed in");
    }
    // Telegram says a token was scanned with an update; a connection that cannot read it is covered by asking again.
    let mut stream = match link.updates.take() {
        Some(updates) => client.stream_updates(updates, grammers_mtsender::UpdatesConfiguration { catch_up: false }).await.ok(),
        None => None,
    };
    let began = tokio::time::Instant::now();
    let mut shown: Vec<u8> = Vec::new();
    status(state, login_id, "Asking Telegram for a code to scan");
    loop {
        if began.elapsed() > QR_PATIENCE {
            bail!("The code was not scanned in time. Start again to get a new one");
        }
        match export_token(&client, &session, api_id, api_hash).await? {
            Exported::Signed(authorization) => return Ok(Some(settle(&client, &session, *authorization).await?)),
            Exported::NeedsPassword => return ask_cloud_password(state, login_id, &client, rx).await,
            Exported::Token { token, expires } => {
                if token != shown {
                    let path = qr_path(login_id);
                    super::super::steam::login::write_qr(&qr_address(&token), &path)?;
                    state.events.emit("telegramLoginQr", json!({ "loginId": login_id, "path": path.to_string_lossy(), "expires": expires }));
                    shown = token;
                }
                // Until it is scanned, it runs out, or it is time to ask again.
                let now = chrono::Utc::now().timestamp();
                let until = Duration::from_secs((i64::from(expires) - now - 2).clamp(1, QR_POLL.as_secs() as i64) as u64);
                let heard = async {
                    match stream.as_mut() {
                        Some(stream) => loop {
                            match stream.next().await {
                                Ok(grammers_client::update::Update::Raw(raw)) if matches!(raw.raw, tl::enums::Update::LoginToken) => return,
                                Ok(_) => {}
                                // Not readable here: the poll is what notices a scan.
                                Err(_) => return std::future::pending().await,
                            }
                        },
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(until) => {}
                    _ = heard => {}
                    given = rx.recv() => {
                        // The form was closed.
                        if given.is_none() {
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }
}

/// Asks for the cloud password of an account that was scanned in, and signs it in.
async fn ask_cloud_password(state: &AppState, login_id: &str, client: &grammers_client::Client, rx: &mut mpsc::UnboundedReceiver<Answer>) -> Result<Option<grammers_client::peer::User>> {
    let password: tl::types::account::Password = client.invoke(&tl::functions::account::GetPassword {}).await.map_err(|e| anyhow!("{}", explain(&e.to_string())))?.into();
    let token = grammers_client::client::PasswordToken::new(password);
    needs(state, login_id, "password", token.hint(), None);
    ask_password(state, login_id, client, token, rx).await
}

async fn ask_password(state: &AppState, login_id: &str, client: &grammers_client::Client, mut token: grammers_client::client::PasswordToken, rx: &mut mpsc::UnboundedReceiver<Answer>) -> Result<Option<grammers_client::peer::User>> {
    loop {
        let Some(given) = answer(rx).await else { return Ok(None) };
        let Answer::Password(password) = given else { continue };
        match client.check_password(token, password.as_bytes()).await {
            Ok(user) => return Ok(Some(user)),
            Err(SignInError::InvalidPassword(again)) => {
                needs(state, login_id, "password", again.hint(), Some("That password is not right"));
                token = again;
            }
            Err(e) => bail!("{}", explain(&e.to_string())),
        }
    }
}

/// Telegram's own wording for what went wrong, said so a person can act on it.
pub fn explain(raw: &str) -> String {
    let upper = raw.to_uppercase();
    for (code, words) in [
        ("API_ID_INVALID", "Telegram does not accept that api_id and api_hash pair. Check them at my.telegram.org/apps"),
        ("PHONE_NUMBER_INVALID", "Telegram says that is not a valid phone number"),
        ("PHONE_NUMBER_BANNED", "Telegram has banned that phone number"),
        ("PHONE_CODE_EXPIRED", "That code has expired. Start again to get a new one"),
        ("PHONE_CODE_INVALID", "That code is not right"),
        ("PASSWORD_HASH_INVALID", "That password is not right"),
        ("FLOOD_WAIT", "Telegram says too many attempts were made. Wait before trying again"),
        ("SESSION_PASSWORD_NEEDED", "This account has a cloud password"),
        ("AUTH_KEY_UNREGISTERED", "Telegram has ended this session"),
        ("USER_DEACTIVATED", "Telegram says this account has been deleted"),
    ] {
        if upper.contains(code) {
            return words.to_string();
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phone_number_is_made_the_way_telegram_wants_it() {
        assert_eq!(normalize_phone("+1 (555) 123-4567").as_deref(), Some("+15551234567"));
        assert_eq!(normalize_phone("44 20 7946 0958").as_deref(), Some("+442079460958"));
        assert_eq!(normalize_phone("12345"), None);
        assert_eq!(normalize_phone("not a number"), None);
    }

    #[test]
    fn answers_for_a_sign_in_that_is_not_waiting_are_refused_in_words() {
        assert!(submit_code("telegram-none", "12345").unwrap_err().to_string().contains("not waiting"));
        assert!(submit_code("telegram-none", "abc").unwrap_err().to_string().contains("type the code"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        waiting().lock().unwrap().insert("telegram-t".into(), tx);
        submit_code("telegram-t", "1 2-3 4 5").unwrap();
        submit_password("telegram-t", "hunter2").unwrap();
        assert!(matches!(rx.try_recv(), Ok(Answer::Code(c)) if c == "12345"), "spaces and dashes in a pasted code are dropped");
        assert!(matches!(rx.try_recv(), Ok(Answer::Password(p)) if p == "hunter2"));
        cancel("telegram-t");
        assert!(submit_code("telegram-t", "12345").is_err());
    }

    #[test]
    fn a_login_token_is_offered_as_telegrams_link_with_the_token_in_base64url() {
        // The alphabet is the URL-safe one, and there is no padding: a phone reads the link as it is written.
        assert_eq!(qr_address(&[0xfb, 0xff]), "tg://login?token=-_8");
        assert_eq!(qr_address(&[1, 2, 3, 4]), "tg://login?token=AQIDBA");
        assert!(qr_address(&[0; 32]).starts_with("tg://login?token="));
    }

    #[test]
    fn a_scanned_codes_picture_is_written_where_the_window_can_read_it() {
        let path = std::env::temp_dir().join(format!("moho-telegram-qr-test-{}.png", std::process::id()));
        super::super::super::steam::login::write_qr(&qr_address(&[7; 32]), &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn a_sign_in_by_qr_is_waited_on_like_one_by_phone_and_given_up_by_closing_the_form() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        waiting().lock().unwrap().insert("telegram-qr".into(), tx);
        cancel("telegram-qr");
        assert!(rx.try_recv().is_err());
        assert!(submit_password("telegram-qr", "x").is_err(), "nothing is waiting for a password once the form is closed");
    }

    #[test]
    fn telegrams_refusals_are_said_so_a_person_can_act_on_them() {
        assert!(explain("rpc error 400: API_ID_INVALID").contains("my.telegram.org"));
        assert!(explain("PHONE_CODE_EXPIRED").contains("expired"));
        assert_eq!(explain("something unforeseen"), "something unforeseen");
    }

    /// Against Telegram's own test servers, which have no real accounts: a number `99966XYYYY` signs in with the code
    /// `XXXXX` (its digit X, five times). Needs the person's `api_id` and `api_hash` (the test servers take a real pair):
    ///
    /// `TELEGRAM_LIVE_API_ID=... TELEGRAM_LIVE_API_HASH=... TELEGRAM_LIVE_PHONE=+99966XYYYY cargo test --release live_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_a_test_number_signs_in_and_out_on_telegrams_test_servers() {
        use grammers_session::types::DcOption;
        use grammers_session::Session;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let api_id: i32 = std::env::var("TELEGRAM_LIVE_API_ID").unwrap().parse().unwrap();
        let api_hash = std::env::var("TELEGRAM_LIVE_API_HASH").unwrap();
        let phone = normalize_phone(&std::env::var("TELEGRAM_LIVE_PHONE").unwrap()).unwrap();
        assert!(phone.starts_with("+99966"), "only a test number, which has no real account behind it");
        let code: String = std::iter::repeat_n(phone.chars().nth(6).unwrap(), 5).collect();

        let state = crate::backend::kick::testkit::simulated_daemon("telegram-live");
        let path = std::env::temp_dir().join(format!("nobilis-telegram-live-{}.session", std::process::id()));
        let session = Arc::new(FileSession::fresh(path.clone()));
        // Telegram's test datacenter 2 stands where the real one does.
        let mut dc: DcOption = session.dc_option(2).unwrap().unwrap();
        dc.ipv4 = "149.154.167.40:443".parse().unwrap();
        dc.ipv6 = "[::ffff:149.154.167.40]:443".parse().unwrap();
        session.set_dc_option(&dc).await.unwrap();
        session.set_home_dc_id(2).await.unwrap();

        let link = link::open(&state, "telegram:live-test", api_id, session.clone()).await.unwrap();
        assert!(!link.client.is_authorized().await.unwrap());
        let token = link.client.request_login_code(&phone, &api_hash).await.unwrap();
        let user = match link.client.sign_in(&token, &code).await {
            Ok(user) => user,
            Err(e) => panic!("{}", explain(&e.to_string())),
        };
        println!("signed in as {} ({})", user.full_name(), user.id().bare_id_unchecked());
        assert!(link.client.is_authorized().await.unwrap());
        assert!(session.has_key());
        session.save().unwrap();
        let reopened = FileSession::open(path.clone()).unwrap();
        assert!(reopened.has_key(), "the saved session still has its key");
        link.client.sign_out().await.unwrap();
        link.close().await;
        std::fs::remove_file(path).ok();
    }
}
