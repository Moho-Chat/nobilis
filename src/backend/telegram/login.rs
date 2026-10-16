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
//! - `telegramLoginResult` - done, with the account, or the reason it failed
//!
//! and the window answers with `submitTelegramCode` and `submitTelegramPassword`.

use super::link::{self, Link};
use super::session::{self, FileSession};
use crate::accounts::TelegramAccountConfig;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use grammers_client::SignInError;
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

/// Starts a sign-in. Answers at once; the rest is events.
pub fn start(state: AppState, login_id: String, api_id: i32, api_hash: String, phone: String) {
    let (tx, rx) = mpsc::unbounded_channel();
    waiting().lock().unwrap().insert(login_id.clone(), tx);
    tokio::spawn(async move {
        let outcome = flow(&state, &login_id, api_id, &api_hash, &phone, rx).await;
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

async fn flow(state: &AppState, login_id: &str, api_id: i32, api_hash: &str, phone: &str, mut rx: mpsc::UnboundedReceiver<Answer>) -> Result<Option<TelegramAccountConfig>> {
    if api_id <= 0 || api_hash.trim().is_empty() {
        bail!("an api_id and an api_hash are needed: get them at my.telegram.org/apps");
    }
    let phone = normalize_phone(phone).ok_or_else(|| anyhow!("that does not look like a phone number; type it with its country code"))?;
    status(state, login_id, "Connecting to Telegram");
    let session = Arc::new(FileSession::fresh(session::path_for_login(login_id)));
    let link = link::open(state, &crate::net::route::pending_key("telegram"), api_id, session.clone()).await?;
    let outcome = signed_in(state, login_id, &link, api_hash, &phone, &mut rx).await;
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
        phone,
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
