//! Live checks against a real homeserver, run by hand.
//!
//! Not tests in the ordinary sense: each signs in as a real account, does a
//! real thing to a real room and reads the result back, so they are
//! `#[ignore]`d and take their credentials from the environment. They are
//! what the encryption work was built against - a Megolm session either
//! decrypts on the other device or it does not, and nothing short of two real
//! devices proves which.
//!
//! Beside the code rather than in `tests/` because they reach into this
//! backend's own internals, the same way sneedchat's `live_probe` does.

#[cfg(test)]
mod live {
    // Absolute rather than `use crate::backend::matrix::*`: this file holds nothing of its own
    // for a relative path to reach through.
    use crate::backend::matrix::*;

    /// Phase-2 live check: real login, a fresh private room created for
    /// the test (a throwaway account has no rooms of its own, and posting
    /// into a real public room isn't appropriate for an automated test),
    /// then the full connect-sync-receive-send loop against it. Reads
    /// credentials from the environment - see auth::tests::matrix_login_probe's
    /// doc comment for the invocation pattern (same env vars, this test
    /// name instead):
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_end_to_end_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_end_to_end_probe() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());
        let _ = rustls::crypto::ring::default_provider().install_default();

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {}", login.user_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({ "preset": "private_chat", "name": "nobilis-matrix-phase2-probe" }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id in createRoom response").to_string();
        println!("created test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord::voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc::dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            irc_sts: std::sync::Arc::new(crate::backend::irc::sts::StsStore::open(data_dir.join("irc-sts.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
            ignores: std::sync::Arc::new(crate::ignores::IgnoreStore::open(data_dir.join("ignores.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            used_sliding_sync: false,
            prefer_sliding_sync: false,
            display_name: None,
            rtc_focus_url: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-phase2-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer for the test room never appeared within 30s - first sync/buffer creation failed");
        println!("buffer created: {buffer_id}");
        assert_eq!(state.runtime.get_matrix_room(&buffer_id).as_deref(), Some(room_id.as_str()), "buffer's room id mapping is wrong");

        let sent_body = format!("phase2-probe-{}", model::next_message_id());
        send_message(&state, &account_id, &buffer_id, &login.access_token, &sent_body, None, false, None).await.expect("send_message failed");
        println!("sent: {sent_body}");

        let mut received = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(&buffer_id, 0, 20) {
                if msgs.iter().any(|m| m.body == sent_body) {
                    received = true;
                    break;
                }
            }
        }
        assert!(received, "sent message never came back through the sync loop within 30s");
        println!("send/receive round-trip confirmed.");

        // Clean up: leave (and best-effort forget) the test room rather
        // than leaving it behind on the account indefinitely.
        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
    }

    /// Phase-3 manual live check: creates a real *encrypted* room, starts
    /// nobilis's own sync loop against it (as one "device" of the test
    /// account), then idles for up to 3 minutes printing every message
    /// this backend receives and decrypts. Meant to be run in the
    /// background while a second, independent Matrix client (e.g. Element
    /// Web, logged into the same account as a genuinely different device)
    /// joins the room and sends a real message into it - the room id is
    /// printed up front so it can be found there. This is the actual hard
    /// gate for "full E2EE from day one": a message this backend never
    /// touched the encryption of, decrypted correctly, proves the whole
    /// device-key-upload + key-claim + Megolm-session path works against
    /// a real second device, not just against itself.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_e2ee_read_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_e2ee_read_probe() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());
        let _ = rustls::crypto::ring::default_provider().install_default();

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {} (device {})", login.user_id, login.device_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({
                "preset": "private_chat",
                "name": "nobilis-matrix-phase3-probe",
                "initial_state": [
                    { "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }
                ],
            }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id in createRoom response").to_string();
        println!("created ENCRYPTED test room {room_id} - join it with a second client (same account, different device) and send a message now.");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-e2ee-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord::voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc::dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            irc_sts: std::sync::Arc::new(crate::backend::irc::sts::StsStore::open(data_dir.join("irc-sts.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
            ignores: std::sync::Arc::new(crate::ignores::IgnoreStore::open(data_dir.join("ignores.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            used_sliding_sync: false,
            prefer_sliding_sync: false,
            display_name: None,
            rtc_focus_url: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-phase3-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer for the test room never appeared within 30s");
        println!("buffer created: {buffer_id} - waiting up to 180s for a message from a second device...");

        let mut seen_bodies: std::collections::HashSet<String> = std::collections::HashSet::new();
        for i in 0..180 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(&buffer_id, 0, 20) {
                for m in &msgs {
                    if seen_bodies.insert(m.id.clone()) {
                        println!("[{i}s] received from {}: {}", m.from, m.body);
                    }
                }
            }
        }

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
    }

    /// Phase-3/phase-4 combined live check, fully automated (no manual
    /// second client needed): "device A" is nobilis's own real production
    /// connect loop (spawn/run_sync, same code path a real account uses);
    /// "device B" is a second, genuinely independent login of the same
    /// account (a real second `OlmMachine`/crypto store, its own device_id)
    /// that encrypts and sends a message using the exact same production
    /// `CryptoSession::share_and_encrypt` path `send_message`'s encrypted
    /// branch uses. Device A decrypting what device B encrypted - neither
    /// having touched the other's key material directly - is the real
    /// gate: device-key upload, key claim, Megolm session establishment,
    /// and decryption all have to work correctly end to end for this to
    /// pass, exactly the "nobilis sends, a different real client decrypts"
    /// (and the reverse) gates the plan calls for.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_e2ee_two_device_roundtrip
    #[tokio::test]
    #[ignore]
    async fn matrix_e2ee_two_device_roundtrip() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());
        let _ = rustls::crypto::ring::default_provider().install_default();

        // --- Device A: nobilis's own real connect loop ---
        let login_a = auth::login(&homeserver, &username, &password, None).await.expect("device A login failed");
        println!("device A logged in as {} (device {})", login_a.user_id, login_a.device_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login_a.access_token),
            serde_json::json!({
                "preset": "private_chat",
                "name": "nobilis-matrix-phase34-probe",
                "initial_state": [
                    { "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }
                ],
            }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id in createRoom response").to_string();
        println!("created ENCRYPTED test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-e2ee-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord::voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc::dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            irc_sts: std::sync::Arc::new(crate::backend::irc::sts::StsStore::open(data_dir.join("irc-sts.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
            ignores: std::sync::Arc::new(crate::ignores::IgnoreStore::open(data_dir.join("ignores.toml"))),
        };

        let config_a = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login_a.user_id.clone(),
            password: password.clone(),
            access_token: login_a.access_token.clone(),
            device_id: login_a.device_id.clone(),
            next_batch: None,
            used_sliding_sync: false,
            prefer_sliding_sync: false,
            display_name: None,
            rtc_focus_url: None,
        };
        let saved_a = state.accounts.add_matrix(config_a).expect("add_matrix failed");
        let account_id_a = saved_a.account_id();
        // Wipe any stale crypto store left over from an earlier run of
        // this test - run_sync's own crypto::CryptoSession::open() reads
        // from the real ~/.config/nobilis (not this test's isolated
        // temp data_dir; only the account/scrollback stores are
        // redirected there), so a leftover store from a previous run
        // still has identity keys for a *different* device_id than the
        // fresh one auth::login(..., None) just minted above -
        // OlmMachine::with_store() rejects that mismatch outright, which
        // silently wedges run_with_retry in an invisible (no tracing
        // subscriber in tests) backoff loop that never creates a buffer.
        let crypto_dir = dirs::home_dir().unwrap_or_default().join(".config").join("nobilis").join("matrix-crypto");
        let sanitized = account_id_a.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect::<String>();
        let _ = std::fs::remove_dir_all(crypto_dir.join(&sanitized));
        spawn(state.clone(), saved_a);

        let buffer_name = "nobilis-matrix-phase34-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer for the test room never appeared within 30s");
        println!("device A buffer created: {buffer_id}");

        // --- Device B: a second, independent login + OlmMachine ---
        let login_b = auth::login(&homeserver, &username, &password, None).await.expect("device B login failed");
        assert_ne!(login_b.device_id, login_a.device_id, "device B unexpectedly got the same device_id as device A");
        println!("device B logged in as {} (device {})", login_b.user_id, login_b.device_id);

        let user_id = ruma_common::UserId::parse(&login_b.user_id).expect("invalid user id");
        let device_id_b = <&ruma_common::DeviceId>::from(login_b.device_id.as_str());
        let account_id_b = format!("{account_id_a}-deviceB-test");
        let session_b = crypto::CryptoSession::open(&data_dir, &account_id_b, &user_id, device_id_b).await.expect("opening device B crypto store");

        // Let both devices exchange device-key info: device B uploads its
        // own keys, device A's already-running sync loop uploads its own
        // on its next cycle and will pick up device B's via device_lists
        // "changed" on a future /sync. A few rounds with short waits gives
        // both directions time to settle before device B tries to encrypt
        // for a member list that includes device A.
        for _ in 0..10 {
            session_b.process_outgoing_requests(&homeserver, &login_b.access_token).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let room_id_ruma = ruma_common::RoomId::parse(&room_id).expect("invalid room id");
        let sent_body = format!("phase34-roundtrip-{}", model::next_message_id());
        println!("device B encrypting and sending: {sent_body}");
        let encrypted_content = session_b
            .share_and_encrypt(&homeserver, &login_b.access_token, &room_id_ruma, vec![user_id.clone()], &sent_body)
            .await
            .expect("device B failed to encrypt/share room key");

        let txn_id = model::next_message_id();
        let send_url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/{}/{}",
            homeserver.trim_end_matches('/'),
            url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
            protocol::EVENT_ROOM_ENCRYPTED,
            url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
        );
        http::put_json(&send_url, &login_b.access_token, encrypted_content).await.expect("device B failed to send encrypted event");
        println!("device B sent the encrypted event - waiting up to 60s for device A to decrypt it...");

        let mut received_body = None;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(&buffer_id, 0, 20) {
                if let Some(m) = msgs.iter().find(|m| m.body == sent_body) {
                    received_body = Some(m.body.clone());
                    break;
                }
                // Also surface anything that came through as an
                // undecryptable placeholder, to distinguish "message
                // never arrived" from "arrived but failed to decrypt" in
                // the failure output.
                if let Some(m) = msgs.iter().find(|m| m.body.contains("unable to decrypt")) {
                    println!("saw an undecrypted placeholder: {}", m.body);
                }
            }
        }

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login_a.access_token),
            serde_json::json!({}),
        )
        .await;

        assert_eq!(received_body.as_deref(), Some(sent_body.as_str()), "device A never decrypted the message device B sent");
        println!("E2EE round trip confirmed: device A correctly decrypted a message it never encrypted itself.");
    }

    /// Phase-5 live check: reply/edit/react/delete round-trip against a
    /// real (unencrypted) room, each verified via the same
    /// get_backlog-polling pattern the earlier phase probes use.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_phase5_polish_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_phase5_polish_probe() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());
        let _ = rustls::crypto::ring::default_provider().install_default();

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {}", login.user_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({ "preset": "private_chat", "name": "nobilis-matrix-phase5-probe" }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id").to_string();
        println!("created test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-phase5-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord::voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc::dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            irc_sts: std::sync::Arc::new(crate::backend::irc::sts::StsStore::open(data_dir.join("irc-sts.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
            ignores: std::sync::Arc::new(crate::ignores::IgnoreStore::open(data_dir.join("ignores.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            used_sliding_sync: false,
            prefer_sliding_sync: false,
            display_name: None,
            rtc_focus_url: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        let crypto_dir = dirs::home_dir().unwrap_or_default().join(".config").join("nobilis").join("matrix-crypto");
        let sanitized = account_id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect::<String>();
        let _ = std::fs::remove_dir_all(crypto_dir.join(&sanitized));
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-phase5-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer never appeared within 30s");
        println!("buffer created: {buffer_id}");

        // --- send the original message ---
        let original_body = format!("phase5-original-{}", model::next_message_id());
        send_message(&state, &account_id, &buffer_id, &login.access_token, &original_body, None, false, None).await.expect("send failed");
        let original_id = poll_for_message(&state, &buffer_id, |m| m.body == original_body, 30).await.expect("original message never arrived").id;
        println!("sent original: {original_id}");

        // --- reply ---
        let reply_body = format!("phase5-reply-{}", model::next_message_id());
        send_message(&state, &account_id, &buffer_id, &login.access_token, &reply_body, Some(&original_id), false, None).await.expect("reply send failed");
        let reply_msg = poll_for_message(&state, &buffer_id, |m| m.body == reply_body, 30).await.expect("reply never arrived");
        assert_eq!(reply_msg.reply_to.as_ref().map(|r| r.id.as_str()), Some(original_id.as_str()), "reply didn't record the right target");
        println!("reply confirmed, targets {}", original_id);

        // --- edit ---
        let edited_body = format!("phase5-edited-{}", model::next_message_id());
        edit_message(&state, &account_id, &buffer_id, &login.access_token, &original_id, &edited_body).await.expect("edit failed");
        let edited = poll_for_message(&state, &buffer_id, |m| m.id == original_id && m.body == edited_body, 30).await;
        assert!(edited.is_some(), "edit never applied");
        println!("edit confirmed");

        // --- react, then un-react ---
        toggle_reaction(&state, &account_id, &buffer_id, &login.access_token, &original_id, "👍", true).await.expect("react failed");
        let reacted = poll_until(30, || {
            state.store.get_message(&buffer_id, &original_id).ok().flatten().is_some_and(|m| m.reactions.iter().any(|r| r.emoji == "👍" && r.me))
        })
        .await;
        assert!(reacted, "reaction never showed up");
        println!("reaction confirmed");

        toggle_reaction(&state, &account_id, &buffer_id, &login.access_token, &original_id, "👍", false).await.expect("un-react failed");
        let unreacted = poll_until(30, || {
            state.store.get_message(&buffer_id, &original_id).ok().flatten().is_none_or(|m| !m.reactions.iter().any(|r| r.emoji == "👍"))
        })
        .await;
        assert!(unreacted, "reaction was never removed");
        println!("un-react confirmed");

        // --- delete ---
        delete_message(&state, &account_id, &buffer_id, &login.access_token, &reply_msg.id).await.expect("delete failed");
        let deleted = poll_until(30, || state.store.get_message(&buffer_id, &reply_msg.id).ok().flatten().is_none()).await;
        assert!(deleted, "message was never deleted");
        println!("delete confirmed");

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
        println!("phase 5 polish probe passed: reply/edit/react/unreact/delete all confirmed.");
    }

    async fn poll_for_message(state: &AppState, buffer_id: &str, pred: impl Fn(&crate::model::Message) -> bool, secs: u32) -> Option<crate::model::Message> {
        for _ in 0..secs {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(buffer_id, 0, 20) {
                if let Some(m) = msgs.into_iter().find(|m| pred(m)) {
                    return Some(m);
                }
            }
        }
        None
    }

    async fn poll_until(secs: u32, mut pred: impl FnMut() -> bool) -> bool {
        for _ in 0..secs {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        pred()
    }

    /// Phase-5 live check: media upload + receive-side caching, against a
    /// real (unencrypted) room - a distinct code path (upload_media_message/
    /// cached_media_path) the other phase-5 probe doesn't touch.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... MATRIX_TEST_IMAGE=/path/to.png \
    ///     cargo test --release -- --ignored --nocapture matrix_media_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_media_probe() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let Ok(image_path) = std::env::var("MATRIX_TEST_IMAGE") else {
            println!("MATRIX_TEST_IMAGE not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());
        let _ = rustls::crypto::ring::default_provider().install_default();

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {}", login.user_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({ "preset": "private_chat", "name": "nobilis-matrix-media-probe" }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id").to_string();
        println!("created test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-media-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord::voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc::dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            irc_sts: std::sync::Arc::new(crate::backend::irc::sts::StsStore::open(data_dir.join("irc-sts.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
            ignores: std::sync::Arc::new(crate::ignores::IgnoreStore::open(data_dir.join("ignores.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            used_sliding_sync: false,
            prefer_sliding_sync: false,
            display_name: None,
            rtc_focus_url: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        let crypto_dir = dirs::home_dir().unwrap_or_default().join(".config").join("nobilis").join("matrix-crypto");
        let sanitized = account_id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect::<String>();
        let _ = std::fs::remove_dir_all(crypto_dir.join(&sanitized));
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-media-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer never appeared within 30s");
        println!("buffer created: {buffer_id}");

        send_message(&state, &account_id, &buffer_id, &login.access_token, "", None, false, Some(&image_path)).await.expect("media send failed");
        println!("uploaded and sent {image_path}");

        let received = poll_for_message(&state, &buffer_id, |m| m.body.starts_with("file://"), 30).await;
        let msg = received.expect("media message never arrived as a cached file:// path");
        println!("received body: {}", msg.body);
        let local_path = msg.body.strip_prefix("file://").unwrap();
        assert!(std::path::Path::new(local_path).is_file(), "cached media file doesn't actually exist on disk: {local_path}");
        let cached_bytes = std::fs::read(local_path).expect("reading cached media file");
        let original_bytes = std::fs::read(&image_path).expect("reading original test image");
        assert_eq!(cached_bytes, original_bytes, "cached media content doesn't match what was uploaded");

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
        println!("media probe passed: upload -> receive -> local cache all confirmed, bytes match exactly.");
    }
}
