//! Adding an account: the sign-in a person takes part in.
//!
//! The password is typed once, here, and this is where it ends: it is
//! encrypted for Steam, sent, and dropped when the sign-in finishes. What the
//! account keeps afterwards is the token Steam hands back.
//!
//! Signing in is a conversation that can take minutes, because Steam may
//! want something from a person on a phone. So it runs as its own task and
//! talks to the window through events rather than answering one request:
//!
//! - `steamLoginStatus` - what it is doing, for a line of text
//! - `steamLoginNeeds` - what Steam is asking for: a code from the
//!   authenticator, a code it emailed, an approval in the Steam app, or
//!   several of those (any one will do)
//! - `steamLoginResult` - done, with the account or the reason it failed
//!
//! and the window answers a code with `submitSteamGuardCode`.

use super::auth::{self, Begun, Guard, Kind};
use super::connect;
use super::link::{self, Identity};
use super::proto::ClientHello;
use super::wire::emsg;
use super::cm;
use crate::accounts::SteamAccountConfig;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a sign-in may wait for a person. Steam gives up on a session after
/// about this long, and a form left open overnight should not leave a task.
const PATIENCE: Duration = Duration::from_secs(5 * 60);

/// The sign-ins waiting for a code, by id, and the way to give each one.
fn waiting() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<String>>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Gives a sign-in the code it asked for.
pub fn submit_code(login_id: &str, code: &str) -> Result<()> {
    let code = code.trim();
    if code.is_empty() {
        bail!("type the code first");
    }
    let sender = waiting().lock().unwrap().get(login_id).cloned().ok_or_else(|| anyhow!("that sign-in is not waiting for a code any more"))?;
    sender.send(code.to_string()).map_err(|_| anyhow!("that sign-in has ended"))
}

/// Stops a sign-in that is waiting, because the form was closed.
pub fn cancel(login_id: &str) {
    waiting().lock().unwrap().remove(login_id);
}

/// What the window is asked for, from what Steam allows.
///
/// Each option is a way in; any one of them will do. Ones this client has no
/// answer to are left out, and if nothing is left the sign-in cannot go on.
pub fn options_of(begun: &Begun) -> Vec<serde_json::Value> {
    begun
        .confirmations
        .iter()
        .filter_map(|c| match &c.guard {
            Guard::DeviceCode => Some(json!({ "kind": "deviceCode" })),
            Guard::EmailCode => Some(json!({ "kind": "emailCode", "hint": c.hint })),
            Guard::DeviceConfirmation => Some(json!({ "kind": "approval", "hint": c.hint })),
            Guard::EmailConfirmation => Some(json!({ "kind": "emailApproval", "hint": c.hint })),
            Guard::None | Guard::Other(_) => None,
        })
        .collect()
}

/// Which kind of code to say a typed one is: the authenticator's if that is
/// on offer, otherwise the emailed one.
pub fn guard_for_code(begun: &Begun) -> Option<Guard> {
    [Guard::DeviceCode, Guard::EmailCode].into_iter().find(|g| begun.confirmations.iter().any(|c| &c.guard == g))
}

/// Starts a sign-in. Answers at once; the rest is events.
pub fn start(state: AppState, login_id: String, username: String, password: String, kind: Kind) {
    let (code_tx, code_rx) = mpsc::unbounded_channel();
    waiting().lock().unwrap().insert(login_id.clone(), code_tx);
    tokio::spawn(async move {
        let outcome = async {
            let key = crate::net::route::pending_key("steam");
            let servers = cm::discover(&key, 0).await?;
            let urls: Vec<String> = servers.iter().take(4).map(|s| s.url()).collect();
            flow(&state, &login_id, &username, &password, kind, &urls, code_rx).await
        }
        .await;
        waiting().lock().unwrap().remove(&login_id);
        match outcome {
            Ok(Some(saved)) => {
                let account = finish(&saved);
                connect::spawn(state.clone(), saved);
                state.events.emit("steamLoginResult", json!({ "loginId": login_id, "success": true, "account": account }));
            }
            // The form was closed; nobody is waiting for an answer.
            Ok(None) => {}
            Err(e) => {
                let error = format!("{e:#}");
                tracing::info!("steam: sign-in {login_id}: {error}");
                state.events.emit("steamLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
            }
        }
    });
}

/// What a finished sign-in is worth to the window: the account as it lists,
/// and the route the account will be reached by remembered for it.
pub fn finish(saved: &SteamAccountConfig) -> serde_json::Value {
    crate::net::route::router().mark(&saved.account_id(), saved.use_tor);
    serde_json::to_value(crate::accounts::steam_account_to_json(saved, "connecting", None)).expect("an account serialises")
}

/// The sign-in itself, on the first of `urls` that answers.
pub async fn flow(
    state: &AppState,
    login_id: &str,
    username: &str,
    password: &str,
    kind: Kind,
    urls: &[String],
    mut codes: mpsc::UnboundedReceiver<String>,
) -> Result<Option<SteamAccountConfig>> {
    let status = |detail: &str| state.events.emit("steamLoginStatus", json!({ "loginId": login_id, "detail": detail }));
    let key = crate::net::route::pending_key("steam");
    let username = username.trim().to_lowercase();
    if username.is_empty() || password.is_empty() {
        bail!("type the account name and the password");
    }

    status("connecting to Steam");
    let mut opened = None;
    let mut last = anyhow!("no Steam server to connect to");
    for url in urls {
        match link::open(&key, url).await {
            Ok(ok) => {
                opened = Some(ok);
                break;
            }
            Err(e) => last = e,
        }
    }
    let (link, _told) = opened.ok_or(last).context("reaching Steam")?;
    link.send(emsg::CLIENT_HELLO, &ClientHello { protocol_version: Some(connect::PROTOCOL_VERSION) })?;

    status("signing in");
    let mut begun = auth::begin(&link, &username, password, None, kind).await.map_err(|e| anyhow!("{e}"))?;
    let steamid = begun.steamid;

    let options = options_of(&begun);
    if !begun.needs_nothing() {
        if options.is_empty() {
            bail!("Steam asked for a kind of confirmation this version of moho cannot give");
        }
        state.events.emit("steamLoginNeeds", json!({ "loginId": login_id, "options": options }));
    }

    let deadline = tokio::time::Instant::now() + PATIENCE;
    let tokens = loop {
        tokio::select! {
            code = codes.recv() => {
                // The form was closed: nobody is waiting.
                let Some(code) = code else { return Ok(None) };
                let Some(guard) = guard_for_code(&begun) else { continue };
                if let Err(e) = auth::send_code(&link, &begun, &guard, &code).await {
                    // Said and asked again: a mistyped code is the ordinary case.
                    state.events.emit("steamLoginNeeds", json!({ "loginId": login_id, "options": options, "error": e.to_string() }));
                    continue;
                }
                status("checking the code");
            }
            _ = tokio::time::sleep(begun.interval) => {
                if tokio::time::Instant::now() >= deadline {
                    bail!("Steam was left waiting too long; start again");
                }
                if let Some(tokens) = auth::poll(&link, &mut begun).await.map_err(|e| anyhow!("{e}"))? {
                    break tokens;
                }
            }
        }
    };

    let config = SteamAccountConfig {
        username: tokens.account_name.clone().map(|n| n.to_lowercase()).unwrap_or(username),
        steamid,
        display_name: None,
        refresh_token: tokens.refresh_token,
        guard_data: tokens.guard_data,
        session: kind.name().to_string(),
        use_tor: crate::net::route::router().wanted(&key),
        strict_route: false,
    };
    if steamid != 0 {
        link.set_identity(Identity { steamid, session: 0 });
    }
    let saved = state.accounts.add_steam(config).context("saving the account")?;
    Ok(Some(saved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;
    use crate::backend::steam::auth::Confirmation;
    use crate::backend::steam::eresult;
    use crate::backend::steam::link::fake::*;
    use crate::backend::steam::proto::*;
    use crate::backend::steam::wire::Packet;
    use prost::Message;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;
    use std::sync::Arc;

    fn begun(guards: &[(Guard, Option<&str>)]) -> Begun {
        Begun {
            client_id: 1,
            request_id: vec![],
            interval: Duration::from_millis(50),
            steamid: 76561197960287930,
            confirmations: guards.iter().map(|(g, h)| Confirmation { guard: g.clone(), hint: h.map(str::to_string) }).collect(),
        }
    }

    #[test]
    fn the_window_is_offered_every_way_in_that_this_client_can_do() {
        let b = begun(&[(Guard::DeviceCode, None), (Guard::DeviceConfirmation, Some("Steam app")), (Guard::EmailCode, Some("a***@x.com")), (Guard::Other(6), None)]);
        let kinds: Vec<_> = options_of(&b).iter().map(|o| o["kind"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["deviceCode", "approval", "emailCode"], "a way this client cannot answer is not offered");
        assert_eq!(options_of(&b)[2]["hint"], "a***@x.com");
        assert!(options_of(&begun(&[(Guard::None, None)])).is_empty());
    }

    #[test]
    fn a_typed_code_is_the_authenticators_when_that_is_offered_and_otherwise_the_emailed_one() {
        assert_eq!(guard_for_code(&begun(&[(Guard::EmailCode, None), (Guard::DeviceCode, None)])), Some(Guard::DeviceCode));
        assert_eq!(guard_for_code(&begun(&[(Guard::EmailCode, None)])), Some(Guard::EmailCode));
        assert_eq!(guard_for_code(&begun(&[(Guard::DeviceConfirmation, None)])), None, "an approval takes no code");
    }

    #[test]
    fn a_code_for_a_sign_in_that_is_not_waiting_is_refused_with_a_reason() {
        assert!(submit_code("nobody", "ABCDE").unwrap_err().to_string().contains("not waiting"));
        assert!(submit_code("nobody", "   ").unwrap_err().to_string().contains("type the code"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        waiting().lock().unwrap().insert("login-x".into(), tx);
        submit_code("login-x", " ab12c ").unwrap();
        assert_eq!(rx.try_recv().unwrap(), "ab12c");
        cancel("login-x");
        assert!(submit_code("login-x", "x").is_err(), "closing the form ends the wait");
    }

    /// A Steam that wants the authenticator's code (and would take a phone
    /// approval), accepts only the right code, and then issues tokens.
    fn steam(private_n: String, private_e: String, right_code: &'static str, seen: Arc<Mutex<Vec<Packet>>>) -> Script {
        let mut approved = false;
        Box::new(move |req| {
            seen.lock().unwrap().push(req.clone());
            let method = req.header.target_job_name.clone().unwrap_or_default();
            let out = match method.as_str() {
                "Authentication.GetPasswordRSAPublicKey#1" => answer(&req, 1, GetPasswordRsaPublicKeyResponse { publickey_mod: Some(private_n.clone()), publickey_exp: Some(private_e.clone()), timestamp: Some(1) }.encode_to_vec()),
                "Authentication.BeginAuthSessionViaCredentials#1" => answer(
                    &req,
                    1,
                    BeginAuthViaCredentialsResponse {
                        client_id: Some(99),
                        request_id: Some(vec![1]),
                        interval: Some(0.5),
                        steamid: Some(76561197960287930),
                        allowed_confirmations: vec![
                            AllowedConfirmation { confirmation_type: Some(3), associated_message: None },
                            AllowedConfirmation { confirmation_type: Some(4), associated_message: Some("Steam app".into()) },
                        ],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ),
                "Authentication.UpdateAuthSessionWithSteamGuardCode#1" => {
                    let sent = UpdateAuthWithGuardCodeRequest::decode(req.body.as_slice()).unwrap();
                    if sent.code.as_deref() == Some(right_code) {
                        approved = true;
                        answer(&req, 1, Vec::new())
                    } else {
                        answer(&req, eresult::TWO_FACTOR_CODE_MISMATCH, Vec::new())
                    }
                }
                "Authentication.PollAuthSessionStatus#1" if approved => answer(
                    &req,
                    1,
                    PollAuthSessionResponse { refresh_token: Some("refresh".into()), access_token: Some("access".into()), account_name: Some("Someone".into()), new_guard_data: Some("remember".into()), ..Default::default() }.encode_to_vec(),
                ),
                "Authentication.PollAuthSessionStatus#1" => answer(&req, 1, Vec::new()),
                _ => answer(&req, 1, Vec::new()),
            };
            Some(vec![out])
        })
    }

    fn keys() -> (String, String) {
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
        let public = private.to_public_key();
        (public.n().to_str_radix(16), public.e().to_str_radix(16))
    }

    async fn next_event(rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>, name: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let e = rx.recv().await.unwrap();
                if e.name == name {
                    return e.data;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {name} event"))
    }

    #[tokio::test]
    async fn a_person_signs_in_mistypes_the_code_and_then_gets_it_right() {
        let state = simulated_daemon("steam-login-whole");
        let mut rx = state.events.subscribe();
        let (n, e) = keys();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(steam(n, e, "RIGHT", seen.clone())).await;
        let (code_tx, code_rx) = mpsc::unbounded_channel();

        let task = tokio::spawn({
            let state = state.clone();
            async move { flow(&state, "login-1", " Someone ", "hunter2", Kind::Chat, &[url], code_rx).await }
        });

        // Steam asks, offering the authenticator and a phone approval.
        let needs = next_event(&mut rx, "steamLoginNeeds").await;
        assert_eq!(needs["loginId"], "login-1");
        let kinds: Vec<_> = needs["options"].as_array().unwrap().iter().map(|o| o["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["deviceCode", "approval"]);
        assert!(needs.get("error").is_none());

        // A wrong code is said, and asked for again, and the sign-in carries on.
        code_tx.send("WRONG".into()).unwrap();
        let again = next_event(&mut rx, "steamLoginNeeds").await;
        assert!(again["error"].as_str().unwrap().contains("Steam Guard"), "{again}");

        code_tx.send("RIGHT".into()).unwrap();
        let saved = task.await.unwrap().unwrap().expect("an account");
        let account = finish(&saved);
        assert_eq!(account["id"], "steam:someone");
        assert_eq!(account["service"], "steam");
        assert_eq!(account["hasPassword"], true);

        // What was kept: the token and the machine note, never the password.
        let saved = state.accounts.get_steam("steam:someone").expect("saved under the lower-case name Steam gave");
        assert_eq!((saved.refresh_token.as_str(), saved.guard_data.as_deref(), saved.steamid), ("refresh", Some("remember"), 76561197960287930));
        assert_eq!(saved.session, "chat");
        let file = std::fs::read_to_string(state.accounts.path_for_tests()).unwrap();
        assert!(file.contains("refresh") && !file.contains("hunter2"), "the token is on disk and the password is not: {file}");

        // Everything Steam was sent about the password was encrypted.
        let begin = seen.lock().unwrap().iter().find(|p| p.header.target_job_name.as_deref() == Some("Authentication.BeginAuthSessionViaCredentials#1")).cloned().unwrap();
        assert!(!String::from_utf8_lossy(&begin.body).contains("hunter2"));
    }

    #[tokio::test]
    async fn a_wrong_password_is_reported_by_the_form_in_steams_words() {
        let state = simulated_daemon("steam-login-badpass");
        let (n, e) = keys();
        let url = serve(Box::new(move |req| {
            let method = req.header.target_job_name.clone().unwrap_or_default();
            Some(vec![if method.contains("GetPasswordRSAPublicKey") {
                answer(&req, 1, GetPasswordRsaPublicKeyResponse { publickey_mod: Some(n.clone()), publickey_exp: Some(e.clone()), timestamp: Some(1) }.encode_to_vec())
            } else {
                answer(&req, eresult::INVALID_PASSWORD, Vec::new())
            }])
        }))
        .await;
        let (_tx, rx) = mpsc::unbounded_channel();
        let err = flow(&state, "login-2", "someone", "wrong", Kind::Chat, &[url], rx).await.unwrap_err();
        assert!(err.to_string().contains("password"), "{err:#}");
        assert!(state.accounts.get_steam("steam:someone").is_none(), "nothing is saved for a sign-in that failed");
    }

    #[tokio::test]
    async fn an_empty_form_and_an_unreachable_steam_are_said_plainly() {
        let state = simulated_daemon("steam-login-bad");
        let (_tx, rx) = mpsc::unbounded_channel();
        assert!(flow(&state, "l", "", "x", Kind::Chat, &[], rx).await.unwrap_err().to_string().contains("account name"));
        let (_tx, rx) = mpsc::unbounded_channel();
        assert!(flow(&state, "l", "x", "", Kind::Chat, &[], rx).await.unwrap_err().to_string().contains("password"));
        let (_tx, rx) = mpsc::unbounded_channel();
        let err = flow(&state, "l", "someone", "x", Kind::Chat, &["ws://127.0.0.1:1".to_string()], rx).await.unwrap_err();
        assert!(format!("{err:#}").contains("reaching Steam"), "{err:#}");
    }

    #[tokio::test]
    async fn closing_the_form_ends_the_wait_without_saving_anything() {
        let state = simulated_daemon("steam-login-closed");
        let (n, e) = keys();
        let url = serve(steam(n, e, "RIGHT", Arc::new(Mutex::new(Vec::new())))).await;
        let (code_tx, code_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn({
            let state = state.clone();
            async move { flow(&state, "login-3", "someone", "x", Kind::Chat, &[url], code_rx).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(code_tx);
        assert!(tokio::time::timeout(Duration::from_secs(5), task).await.expect("it ends").unwrap().unwrap().is_none());
        assert!(state.accounts.get_steam("steam:someone").is_none());
    }
}
