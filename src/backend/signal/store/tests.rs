use super::*;
use chrono::{TimeZone, Utc};
use presage::{
    libsignal_service::{
        content::{ContentBody, Metadata},
        pre_keys::{KyberPreKeyStoreExt, PreKeysStore},
        prelude::{Content, ProfileKey, SessionStoreExt, Uuid},
        proto::DataMessage,
        protocol::{
            Aci, DeviceId, Direction, GenericSignedPreKey, IdentityChange, IdentityKeyPair,
            IdentityKeyStore, KeyPair, KyberPreKeyId, KyberPreKeyRecord, KyberPreKeyStore, Pni, ProtocolAddress,
            ServiceId, SessionRecord, SessionStore, SignedPreKeyId, SignedPreKeyRecord, SignedPreKeyStore, Timestamp,
        },
    },
    model::{contacts::Contact, groups::Group},
    proto::{verified, Verified},
    store::{ContentsStore, Thread},
};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn store() -> SignalStore {
    SignalStore::open_in_memory(OnNewIdentity::Trust).unwrap()
}

fn aci(n: u128) -> ServiceId {
    Aci::from(Uuid::from_u128(n)).into()
}

fn address(service_id: &ServiceId, device: u32) -> ProtocolAddress {
    ProtocolAddress::new(service_id.service_id_string(), DeviceId::try_from(device).unwrap())
}

fn text(sender: ServiceId, ts: u64, body: &str) -> Content {
    let at = Utc.timestamp_millis_opt(ts as i64).unwrap();
    Content {
        metadata: Metadata {
            sender,
            destination: aci(99),
            sender_device: 1.try_into().unwrap(),
            server_guid: None,
            client_timestamp: at,
            server_timestamp: at + chrono::Duration::milliseconds(7),
            needs_receipt: true,
            unidentified_sender: true,
            was_plaintext: false,
            pni_verified: None,
        },
        body: ContentBody::DataMessage(DataMessage { body: Some(body.to_owned()), ..Default::default() }),
    }
}

fn body_of(content: &Content) -> Option<&str> {
    match &content.body {
        ContentBody::DataMessage(DataMessage { body, .. }) => body.as_deref(),
        _ => None,
    }
}

fn contact(n: u128, name: &str, position: u32) -> Contact {
    Contact {
        uuid: Uuid::from_u128(n),
        phone_number: None,
        name: name.to_owned(),
        verified: Verified::default(),
        profile_key: vec![n as u8; 32],
        expire_timer: 0,
        expire_timer_version: 2,
        inbox_position: position,
        avatar: None,
    }
}

fn group(title: &str) -> Group {
    Group {
        title: title.to_owned(),
        avatar: String::new(),
        disappearing_messages_timer: None,
        access_control: None,
        revision: 3,
        members: vec![],
        pending_members: vec![],
        requesting_members: vec![],
        invite_link_password: vec![1, 2, 3],
        description: Some("about".into()),
    }
}

#[test]
fn a_new_store_has_the_current_schema_and_a_second_open_does_not_redo_it() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("a.db");
    {
        let s = SignalStore::open(&path, OnNewIdentity::Trust)?;
        s.write(|tx| kv_set(tx, "k", b"v"))?;
    }
    let s = SignalStore::open(&path, OnNewIdentity::Trust)?;
    assert_eq!(s.read(|c| kv_get(c, "k"))?, Some(b"v".to_vec()));
    let version: i64 = s.read(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))?;
    assert_eq!(version, schema::KNOWN_VERSION);
    Ok(())
}

#[test]
fn a_store_from_a_newer_moho_is_refused_and_left_alone() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("b.db");
    {
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "user_version", schema::KNOWN_VERSION + 1)?;
    }
    let err = SignalStore::open(&path, OnNewIdentity::Trust).unwrap_err();
    assert!(matches!(err, SignalStoreError::NewerSchema { .. }), "{err}");
    Ok(())
}

#[cfg(unix)]
#[test]
fn the_store_and_its_folder_are_private_to_the_owner() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let folder = dir.path().join("signal");
    let path = folder.join("c.db");
    let s = SignalStore::open(&path, OnNewIdentity::Trust)?;
    s.write(|tx| kv_set(tx, "k", b"v"))?;
    assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&folder)?.permissions().mode() & 0o777, 0o700);
    // The write-ahead files SQLite makes beside it take the main file's mode.
    for suffix in ["-wal", "-shm"] {
        let side = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            assert_eq!(std::fs::metadata(&side)?.permissions().mode() & 0o077, 0, "{}", side.display());
        }
    }
    Ok(())
}

#[tokio::test]
async fn sessions_are_kept_per_identity_and_device() -> TestResult {
    let s = store();
    let mut aci_store = s.aci_protocol_store();
    let mut pni_store = s.pni_protocol_store();
    let uuid = Uuid::from_u128(1);
    let a: ServiceId = Aci::from(uuid).into();
    let p: ServiceId = Pni::from(uuid).into();
    let record = SessionRecord::new_fresh();

    for (id, device) in [(a, 1), (a, 2), (p, 1), (p, 3)] {
        aci_store.store_session(&address(&id, device), &record).await?;
    }
    // The PNI's tables are separate from the ACI's.
    assert!(pni_store.load_session(&address(&a, 2)).await?.is_none());
    pni_store.store_session(&address(&a, 2), &record).await?;
    assert!(pni_store.load_session(&address(&a, 2)).await?.is_some());

    assert_eq!(aci_store.get_sub_device_sessions(&a).await?, vec![DeviceId::try_from(2u32)?]);
    assert_eq!(aci_store.get_sub_device_sessions(&p).await?, vec![DeviceId::try_from(3u32)?]);
    assert_eq!(aci_store.delete_all_sessions(&a).await?, 2);
    assert!(aci_store.get_sub_device_sessions(&a).await?.is_empty());
    // Gone from the ACI's set, still in the PNI's.
    assert!(pni_store.load_session(&address(&a, 2)).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn a_session_stored_twice_is_one_session() -> TestResult {
    let s = store();
    let mut st = s.aci_protocol_store();
    let who = aci(5);
    st.store_session(&address(&who, 2), &SessionRecord::new_fresh()).await?;
    st.store_session(&address(&who, 2), &SessionRecord::new_fresh()).await?;
    assert_eq!(st.get_sub_device_sessions(&who).await?.len(), 1);
    st.delete_session(&address(&who, 2)).await?;
    assert!(st.load_session(&address(&who, 2)).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn identities_trust_on_first_use_and_a_changed_key_follows_the_setting() -> TestResult {
    let mut rng = rand09::rng();
    let who = address(&aci(7), 1);
    let first = *IdentityKeyPair::generate(&mut rng).identity_key();
    let second = *IdentityKeyPair::generate(&mut rng).identity_key();

    for (setting, trusts_a_changed_key) in [(OnNewIdentity::Trust, true), (OnNewIdentity::Reject, false)] {
        let s = SignalStore::open_in_memory(setting)?;
        let mut st = s.aci_protocol_store();
        // Never seen: trusted.
        assert!(st.is_trusted_identity(&who, &first, Direction::Receiving).await?);
        assert_eq!(st.save_identity(&who, &first).await?, IdentityChange::NewOrUnchanged);
        // The same key again is no change.
        assert_eq!(st.save_identity(&who, &first).await?, IdentityChange::NewOrUnchanged);
        assert!(st.is_trusted_identity(&who, &first, Direction::Sending).await?);
        // A different key is a change, and trusted only if the setting says so.
        assert_eq!(st.is_trusted_identity(&who, &second, Direction::Sending).await?, trusts_a_changed_key);
        assert_eq!(st.save_identity(&who, &second).await?, IdentityChange::ReplacedExisting);
        assert_eq!(st.get_identity(&who).await?, Some(second));
    }
    Ok(())
}

#[tokio::test]
async fn the_identity_key_pair_is_what_was_set() -> TestResult {
    let mut rng = rand09::rng();
    let s = store();
    let pair = IdentityKeyPair::generate(&mut rng);
    assert!(s.aci_protocol_store().get_identity_key_pair().await.is_err());
    s.set_aci_identity_key_pair(pair).await?;
    let back = s.aci_protocol_store().get_identity_key_pair().await?;
    assert_eq!(back.serialize(), pair.serialize());
    // The PNI has its own.
    assert!(s.pni_protocol_store().get_identity_key_pair().await.is_err());
    Ok(())
}

fn signed_pre_key(id: u32, signer: &KeyPair) -> SignedPreKeyRecord {
    let mut rng = rand09::rng();
    let pair = KeyPair::generate(&mut rng);
    let signature = signer.private_key.calculate_signature(&pair.public_key.serialize(), &mut rng).unwrap();
    SignedPreKeyRecord::new(SignedPreKeyId::from(id), Timestamp::from_epoch_millis(1_760_968_452_908), &pair, &signature)
}

fn kyber_pre_key(id: u32, signer: &KeyPair) -> KyberPreKeyRecord {
    KyberPreKeyRecord::generate(
        presage::libsignal_service::protocol::kem::KeyType::Kyber1024,
        KyberPreKeyId::from(id),
        &signer.private_key,
    )
    .unwrap()
}

#[tokio::test]
async fn pre_key_ids_and_counts_follow_what_is_stored() -> TestResult {
    let mut rng = rand09::rng();
    let signer = KeyPair::generate(&mut rng);
    let s = store();
    let mut st = s.aci_protocol_store();

    assert_eq!(st.next_signed_pre_key_id().await?, 1);
    assert_eq!(st.next_pq_pre_key_id().await?, 1);
    assert_eq!(st.signed_prekey_id().await?, None);
    assert_eq!(st.last_resort_kyber_prekey_id().await?, None);

    st.save_signed_pre_key(SignedPreKeyId::from(4), &signed_pre_key(4, &signer)).await?;
    st.save_kyber_pre_key(KyberPreKeyId::from(10), &kyber_pre_key(10, &signer)).await?;
    st.store_last_resort_kyber_pre_key(KyberPreKeyId::from(11), &kyber_pre_key(11, &signer)).await?;

    assert_eq!(st.next_signed_pre_key_id().await?, 5);
    assert_eq!(st.next_pq_pre_key_id().await?, 12);
    assert_eq!(st.signed_pre_keys_count().await?, 1);
    assert_eq!(st.signed_prekey_id().await?, Some(SignedPreKeyId::from(4)));
    // One of each kind, and asking for a kind counts that kind.
    assert_eq!(st.kyber_pre_keys_count(false).await?, 1);
    assert_eq!(st.kyber_pre_keys_count(true).await?, 1);
    assert_eq!(st.last_resort_kyber_prekey_id().await?, Some(KyberPreKeyId::from(11)));
    assert_eq!(st.load_last_resort_kyber_pre_keys().await?.len(), 1);

    // The PNI sees none of it.
    assert_eq!(s.pni_protocol_store().next_pq_pre_key_id().await?, 1);
    // A key that is not there is the protocol's "no such key", not a vague failure.
    assert!(matches!(
        st.get_kyber_pre_key(KyberPreKeyId::from(99)).await,
        Err(presage::libsignal_service::protocol::SignalProtocolError::InvalidKyberPreKeyId)
    ));
    assert!(matches!(
        st.get_signed_pre_key(SignedPreKeyId::from(99)).await,
        Err(presage::libsignal_service::protocol::SignalProtocolError::InvalidSignedPreKeyId)
    ));
    Ok(())
}

#[tokio::test]
async fn a_one_time_kyber_key_is_gone_once_used() -> TestResult {
    let mut rng = rand09::rng();
    let signer = KeyPair::generate(&mut rng);
    let s = store();
    let mut st = s.aci_protocol_store();
    let id = KyberPreKeyId::from(3);
    st.save_kyber_pre_key(id, &kyber_pre_key(3, &signer)).await?;
    assert!(st.get_kyber_pre_key(id).await.is_ok());
    st.mark_kyber_pre_key_used(id, SignedPreKeyId::from(1), &signer.public_key).await?;
    assert!(st.get_kyber_pre_key(id).await.is_err());
    Ok(())
}

#[tokio::test]
async fn a_last_resort_kyber_key_stays_but_refuses_the_same_handshake_twice() -> TestResult {
    let mut rng = rand09::rng();
    let signer = KeyPair::generate(&mut rng);
    let s = store();
    let mut st = s.aci_protocol_store();
    let id = KyberPreKeyId::from(3);
    let ec = SignedPreKeyId::from(1);
    st.save_signed_pre_key(ec, &signed_pre_key(1, &signer)).await?;
    st.store_last_resort_kyber_pre_key(id, &kyber_pre_key(3, &signer)).await?;

    st.mark_kyber_pre_key_used(id, ec, &signer.public_key).await?;
    assert!(st.get_kyber_pre_key(id).await.is_ok(), "a last-resort key is kept");
    assert!(matches!(
        st.mark_kyber_pre_key_used(id, ec, &signer.public_key).await,
        Err(presage::libsignal_service::protocol::SignalProtocolError::InvalidMessage(_, _))
    ));
    // A different base key is a different handshake.
    let other = KeyPair::generate(&mut rng);
    st.mark_kyber_pre_key_used(id, ec, &other.public_key).await?;
    Ok(())
}

#[tokio::test]
async fn messages_come_back_newest_first_within_the_range_asked() -> TestResult {
    let s = store();
    let thread = Thread::Contact(aci(2));
    for (ts, body) in [(100, "a"), (200, "b"), (300, "c"), (400, "d")] {
        s.save_message(&thread, text(aci(2), ts, body)).await?;
    }
    let all: Vec<_> = s.messages(&thread, ..).await?.collect::<Result<_>>()?;
    assert_eq!(all.iter().map(|m| body_of(m).unwrap()).collect::<Vec<_>>(), ["d", "c", "b", "a"]);
    let middle: Vec<_> = s.messages(&thread, 200..400).await?.collect::<Result<_>>()?;
    assert_eq!(middle.iter().map(|m| body_of(m).unwrap()).collect::<Vec<_>>(), ["c", "b"]);
    let from: Vec<_> = s.messages(&thread, 300..).await?.collect::<Result<_>>()?;
    assert_eq!(from.len(), 2);
    let upto: Vec<_> = s.messages(&thread, ..=200).await?.collect::<Result<_>>()?;
    assert_eq!(upto.len(), 2);
    // Another conversation has none of them.
    assert_eq!(s.messages(&Thread::Contact(aci(3)), ..).await?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn a_message_keeps_what_it_was_stored_with() -> TestResult {
    let s = store();
    let thread = Thread::Contact(aci(2));
    s.save_message(&thread, text(aci(2), 1_700_000_000_000, "hello")).await?;
    let back = s.message(&thread, 1_700_000_000_000).await?.expect("stored");
    assert_eq!(body_of(&back), Some("hello"));
    assert_eq!(back.metadata.sender, aci(2));
    assert_eq!(back.metadata.destination, aci(99));
    assert_eq!(back.metadata.client_timestamp.timestamp_millis(), 1_700_000_000_000);
    // The time the server stamped is kept, not replaced by the client's.
    assert_eq!(back.metadata.server_timestamp.timestamp_millis(), 1_700_000_000_007);
    assert!(back.metadata.needs_receipt && back.metadata.unidentified_sender && !back.metadata.was_plaintext);
    assert!(s.message(&thread, 5).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn a_message_can_be_deleted_and_a_conversation_cleared() -> TestResult {
    let mut s = store();
    let group_thread = Thread::Group([9; 32]);
    let contact_thread = Thread::Contact(aci(2));
    s.save_message(&group_thread, text(aci(2), 10, "g1")).await?;
    s.save_message(&group_thread, text(aci(3), 20, "g2")).await?;
    s.save_message(&contact_thread, text(aci(2), 10, "c1")).await?;

    assert!(s.delete_message(&group_thread, 10).await?);
    assert!(!s.delete_message(&group_thread, 10).await?);
    assert!(s.message(&contact_thread, 10).await?.is_some(), "the same timestamp elsewhere is another message");

    // Who sent what, when: the conversation it belongs to.
    assert_eq!(s.thread_for_sender_and_timestamp(&aci(3), 20).await?, Some(group_thread.clone()));
    assert_eq!(s.thread_for_sender_and_timestamp(&aci(2), 10).await?, Some(contact_thread.clone()));
    assert_eq!(s.thread_for_sender_and_timestamp(&aci(4), 10).await?, None);

    s.clear_thread(&group_thread).await?;
    assert_eq!(s.messages(&group_thread, ..).await?.count(), 0);
    assert_eq!(s.messages(&contact_thread, ..).await?.count(), 1);
    Ok(())
}

#[tokio::test]
async fn contacts_are_listed_in_inbox_order_and_keep_their_verification_through_a_resave() -> TestResult {
    let mut s = store();
    s.save_contact(&contact(2, "Zed", 1)).await?;
    s.save_contact(&contact(1, "Amy", 2)).await?;

    let mut verified_amy = contact(1, "Amy", 2);
    verified_amy.verified = Verified {
        destination_aci: Some(Uuid::from_u128(1).to_string()),
        destination_aci_binary: None,
        identity_key: Some(vec![5; 33]),
        state: Some(verified::State::Verified.into()),
        null_message: None,
    };
    s.save_contact(&verified_amy).await?;
    // Saved again with nothing said about verification (a contacts sync): what is known stays known.
    s.save_contact(&contact(1, "Amy B", 2)).await?;

    let listed: Vec<_> = s.contacts().await?.collect::<Result<_>>()?;
    assert_eq!(listed.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Zed", "Amy B"]);
    let amy = s.contact_by_id(&aci(1)).await?.expect("stored");
    assert_eq!(amy.verified.identity_key, Some(vec![5; 33]));
    assert_eq!(amy.verified.state, Some(verified::State::Verified.into()));
    assert_eq!(amy.verified.destination_aci, Some(Uuid::from_u128(1).to_string()));
    assert!(amy.verified.destination_aci_binary.is_some());
    assert!(s.contact_by_id(&aci(2)).await?.unwrap().verified.identity_key.is_none());
    assert_eq!(amy.profile_key, vec![1; 32]);
    assert!(s.contact_by_id(&aci(50)).await?.is_none());

    // The disappearing-messages timer lives on the contact, and an older version never overwrites a newer one.
    s.update_expire_timer(&Thread::Contact(aci(1)), 3600, 5).await?;
    s.update_expire_timer(&Thread::Contact(aci(1)), 60, 4).await?;
    assert_eq!(s.expire_timer(&Thread::Contact(aci(1))).await?, Some((3600, 5)));
    Ok(())
}

#[tokio::test]
async fn a_phone_number_is_kept_whole() -> TestResult {
    let mut s = store();
    let mut c = contact(1, "Amy", 1);
    c.phone_number = Some(presage::libsignal_service::prelude::phonenumber::parse(None, "+14155550123")?);
    s.save_contact(&c).await?;
    let back = s.contact_by_id(&aci(1)).await?.unwrap();
    assert_eq!(back.phone_number, c.phone_number);
    Ok(())
}

#[tokio::test]
async fn groups_are_saved_and_listed_and_leaving_is_forgotten_when_one_is_saved_again() -> TestResult {
    let mut s = store();
    let key = [4u8; 32];
    assert!(s.group(key).await?.is_none());
    s.save_group(key, group("Hikers")).await?;
    let back = s.group(key).await?.unwrap();
    assert_eq!((back.title.as_str(), back.revision, back.invite_link_password.as_slice()), ("Hikers", 3, &[1, 2, 3][..]));
    assert_eq!(back.description.as_deref(), Some("about"));

    s.save_group_avatar(key, &vec![9, 9]).await?;
    // Saving the group again (a newer revision) does not lose the picture kept for it.
    s.save_group(key, group("Hikers!")).await?;
    assert_eq!(s.group_avatar(key).await?, Some(vec![9, 9]));

    let listed: Vec<_> = s.groups().await?.collect::<Result<_>>()?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, key);

    assert!(!s.is_group_left(key).await?);
    s.mark_group_left(key).await?;
    s.mark_group_left(key).await?;
    assert!(s.is_group_left(key).await?);
    s.save_group(key, group("Hikers")).await?;
    assert!(!s.is_group_left(key).await?);

    // The timer of a group is its own, and zero turns it off.
    let thread = Thread::Group(key);
    s.update_expire_timer(&thread, 86_400, 1).await?;
    assert_eq!(s.expire_timer(&thread).await?, Some((86_400, 1)));
    s.update_expire_timer(&thread, 0, 1).await?;
    assert_eq!(s.expire_timer(&thread).await?, None);
    Ok(())
}

#[tokio::test]
async fn a_profile_is_found_by_its_key_and_dropped_when_the_key_changes() -> TestResult {
    use presage::libsignal_service::{profile_name::ProfileName, Profile};
    let mut s = store();
    let uuid = Uuid::from_u128(8);
    let key = ProfileKey::create([1; 32]);
    let other = ProfileKey::create([2; 32]);
    let profile = Profile {
        name: Some(ProfileName { given_name: "Ada".into(), family_name: Some("Lovelace".into()) }),
        about: Some("hi".into()),
        about_emoji: Some("🙂".into()),
        avatar: None,
        unrestricted_unidentified_access: true,
    };
    s.save_profile(uuid, key, profile).await?;
    s.save_profile_avatar(uuid, key, &vec![1, 2, 3]).await?;

    let back = s.profile(uuid, key).await?.unwrap();
    assert_eq!(back.name.as_ref().map(|n| n.given_name.as_str()), Some("Ada"));
    assert_eq!(back.name.and_then(|n| n.family_name).as_deref(), Some("Lovelace"));
    assert!(back.unrestricted_unidentified_access);
    assert_eq!(s.profile_avatar(uuid, key).await?, Some(vec![1, 2, 3]));
    assert_eq!(s.profile_key(&ServiceId::Aci(uuid.into())).await?.map(|k| k.bytes), Some(key.bytes));
    // The profile of one key is not the profile of another.
    assert!(s.profile(uuid, other).await?.is_none());

    // The same key again changes nothing; a new key means what was kept was made with a key that is gone.
    assert!(!s.upsert_profile_key(&uuid, key).await?);
    assert!(s.profile(uuid, key).await?.is_some());
    assert!(s.upsert_profile_key(&uuid, other).await?);
    assert!(s.profile(uuid, other).await?.is_none());
    assert!(s.profile_avatar(uuid, other).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn sticker_packs_are_installed_listed_and_removed() -> TestResult {
    use presage::store::{StickerPack, StickerPackManifest};
    let mut s = store();
    let pack = StickerPack {
        id: vec![1; 16],
        key: vec![2; 32],
        manifest: StickerPackManifest { title: "Cats".into(), author: "me".into(), cover: None, stickers: vec![] },
    };
    s.add_sticker_pack(&pack).await?;
    assert_eq!(s.sticker_pack(&pack.id).await?.unwrap().manifest.title, "Cats");
    assert_eq!(s.sticker_packs().await?.count(), 1);
    assert!(s.remove_sticker_pack(&pack.id).await?);
    assert!(!s.remove_sticker_pack(&pack.id).await?);
    assert!(s.sticker_pack(&pack.id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn unlinking_forgets_the_keys_and_sessions_and_keeps_what_was_said() -> TestResult {
    let mut rng = rand09::rng();
    let mut s = store();
    s.set_aci_identity_key_pair(IdentityKeyPair::generate(&mut rng)).await?;
    s.aci_protocol_store().store_session(&address(&aci(2), 2), &SessionRecord::new_fresh()).await?;
    s.save_message(&Thread::Contact(aci(2)), text(aci(2), 10, "kept")).await?;
    s.save_contact(&contact(2, "Zed", 1)).await?;

    s.clear_registration().await?;
    assert!(s.aci_protocol_store().get_identity_key_pair().await.is_err());
    assert!(s.aci_protocol_store().get_sub_device_sessions(&aci(2)).await?.is_empty());
    assert!(s.message(&Thread::Contact(aci(2)), 10).await?.is_some());
    assert!(s.contact_by_id(&aci(2)).await?.is_some());

    // Clearing the whole store leaves nothing.
    s.clear().await?;
    assert!(s.message(&Thread::Contact(aci(2)), 10).await?.is_none());
    assert!(s.contact_by_id(&aci(2)).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn a_clone_is_the_same_store() -> TestResult {
    let s = store();
    let other = s.clone();
    s.save_message(&Thread::Contact(aci(2)), text(aci(2), 1, "x")).await?;
    assert!(other.message(&Thread::Contact(aci(2)), 1).await?.is_some());
    Ok(())
}

#[test]
fn an_account_file_name_cannot_leave_the_folder() {
    assert!(path_for("../../etc/passwd").starts_with(folder()));
    assert_eq!(path_for("../x").file_name().unwrap(), "x.db");
    assert!(path_for_link("a/../b").starts_with(folder()));
}

#[test]
fn a_message_that_will_disappear_waits_to_be_read_then_runs_down_and_is_due() {
    use super::Expiry;
    let s = store();
    let row = |id: &str, deadline: Option<i64>| Expiry { buffer_id: "b".into(), msg_id: id.into(), thread_remote: "p:x".into(), sent_ms: 5, seconds: 60, deadline };
    s.expiry_add(&row("waiting", None)).unwrap();
    s.expiry_add(&row("sent", Some(1_000))).unwrap();
    // Noted twice, it keeps its first deadline.
    s.expiry_add(&row("sent", Some(9_999))).unwrap();
    assert_eq!(s.expiry_count().unwrap(), 2);
    // Nothing waiting is due, however late it is.
    assert_eq!(s.expiry_due(5_000).unwrap().iter().map(|e| e.msg_id.as_str()).collect::<Vec<_>>(), vec!["sent"]);
    assert_eq!(s.expiry_start("b", 2_000).unwrap(), 1);
    assert!(s.expiry_due(2_059).unwrap().iter().all(|e| e.msg_id != "waiting"));
    assert_eq!(s.expiry_due(2_060).unwrap().len(), 2);
    s.expiry_done("b", "sent").unwrap();
    s.expiry_done("b", "waiting").unwrap();
    assert_eq!(s.expiry_count().unwrap(), 0);
}

#[test]
fn a_store_made_before_disappearing_messages_gets_the_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(schema::V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let s = SignalStore::open(&path, OnNewIdentity::Trust).unwrap();
    assert_eq!(s.expiry_count().unwrap(), 0);
    let version: i64 = s.read(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?)).unwrap();
    assert_eq!(version, 2);
}
