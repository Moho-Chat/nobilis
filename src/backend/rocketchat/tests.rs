//! Unit tests for Rocket.Chat backend (Rounds 0–9).

use super::messages::{extract_quote_permalink, extract_reactions, format_body, system_message_text};
use super::world::{parse_rc_timestamp, RoomKind, World};
use crate::accounts::{AccountStore, RocketChatAccountConfig};
use serde_json::json;

#[test]
fn merges_rooms_and_subscriptions_and_formats_titles() {
    let mut w = World {
        own_id: "u1".into(),
        own_username: "alice".into(),
        own_roles: vec!["user".into(), "livechat-agent".into()],
        ..Default::default()
    };
    assert!(w.is_livechat_agent());

    // 1:1 DM
    w.merge_room_json(&json!({
        "_id": "dm1",
        "t": "d",
        "usernames": ["alice", "bob"],
        "uids": ["u1", "u2"],
    }));
    w.merge_subscription_json(&json!({
        "rid": "dm1",
        "t": "d",
        "open": true,
        "unread": 3,
        "userMentions": 1,
        "E2EKey": "abcdefghijklDUMMY",
    }));
    let dm = w.rooms.get("dm1").unwrap();
    assert_eq!(dm.kind, RoomKind::Direct);
    assert_eq!(w.room_title(dm), "bob");
    assert_eq!(dm.unread, 3);
    assert_eq!(dm.user_mentions, 1);
    assert!(dm.encrypted);

    // Multi-user DM
    w.merge_room_json(&json!({
        "_id": "dm2",
        "t": "d",
        "usernames": ["alice", "bob", "carol"],
    }));
    assert_eq!(w.room_title(w.rooms.get("dm2").unwrap()), "bob, carol");

    // Public channel, Team main, Discussion, Livechat
    w.merge_room_json(&json!({ "_id": "c1", "t": "c", "name": "general", "topic": "Welcome" }));
    assert_eq!(w.room_title(w.rooms.get("c1").unwrap()), "#general");

    w.merge_room_json(&json!({ "_id": "t1", "t": "p", "name": "eng", "teamMain": true, "teamId": "team1" }));
    assert!(w.rooms.get("t1").unwrap().team_main);

    w.merge_room_json(&json!({ "_id": "disc1", "t": "p", "name": "abc", "fname": "Release Planning", "prid": "c1" }));
    assert_eq!(w.room_title(w.rooms.get("disc1").unwrap()), "Release Planning");

    w.merge_room_json(&json!({ "_id": "lc1", "t": "l", "fname": "Visitor 42" }));
    assert_eq!(w.room_title(w.rooms.get("lc1").unwrap()), "livechat/Visitor 42");

    assert!(World::is_federated_username("@bob:matrix.org"));
    assert!(!World::is_federated_username("bob"));
}

#[test]
fn parses_quotes_katex_reactions_timestamps_and_system_messages() {
    let (qid, clean) = extract_quote_permalink("[ ](https://open.rocket.chat/channel/general?msg=abc123) Good point!");
    assert_eq!(qid.as_deref(), Some("abc123"));
    assert_eq!(clean, "Good point!");

    assert_eq!(format_body(r"\[E=mc^2\]", true), "$$E=mc^2$$");

    let msg = json!({
        "reactions": {
            ":thumbsup:": { "usernames": ["alice", "bob"] },
            ":rocket:": { "usernames": ["carol"] }
        }
    });
    let mut rx = extract_reactions(&msg, "alice");
    rx.sort_by(|a, b| a.emoji.cmp(&b.emoji));
    assert_eq!(rx.len(), 2);
    assert_eq!(rx[0].emoji, ":rocket:");
    assert!(!rx[0].me);
    assert_eq!(rx[1].emoji, ":thumbsup:");
    assert_eq!(rx[1].count, 2);
    assert!(rx[1].me);

    assert_eq!(parse_rc_timestamp(&json!({ "$date": 1_700_000_000_000i64 })), Some(1_700_000_000));
    assert!(parse_rc_timestamp(&json!("2026-11-27T08:00:00.000Z")).is_some());
    assert!(system_message_text("otr", "bob", "handshake").unwrap().contains("legacy OTR"));
}

#[test]
fn persists_rocketchat_account_config_with_expected_account_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("accounts.toml");
    let store = AccountStore::open(path.clone()).unwrap();
    let cfg = RocketChatAccountConfig {
        server_url: "https://chat.example.com".into(),
        server_host: "chat.example.com".into(),
        user_id: "rcUser42".into(),
        username: "alice".into(),
        auth_token: "secret-token".into(),
        e2ee_passphrase: Some("my-pass".into()),
        made_here: true,
        display_name: Some("Alice".into()),
        default_status: Some("away".into()),
        omnichannel_enabled: true,
        use_tor: true,
        strict_route: false,
    };
    let saved = store.add_rocketchat(cfg).unwrap();
    assert_eq!(saved.account_id(), "rocketchat:chat.example.com:rcUser42");

    let reopened = AccountStore::open(path).unwrap();
    let loaded = reopened.get_rocketchat("rocketchat:chat.example.com:rcUser42").unwrap();
    assert_eq!(loaded.username, "alice");
    assert_eq!(loaded.default_status.as_deref(), Some("away"));
    assert!(loaded.omnichannel_enabled);
    assert!(loaded.use_tor);
}
