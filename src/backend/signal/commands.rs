//! What the daemon asks of an account's thread, run beside the receive loop: each command gets its own local task and
//! a clone of the library's manager, so a slow send does not hold up receiving.

use super::ids;
use super::store::SignalStore;
use super::{Command, Handle};
use crate::state::AppState;
use presage::{
    libsignal_service::{content::ContentBody, prelude::ServiceError, protocol::ServiceId},
    manager::Registered,
    proto::{data_message, sync_message, typing_message, DataMessage, EditMessage, ReceiptMessage, SyncMessage, TypingMessage},
    store::Thread,
    Error as PresageError, Manager,
};

type Library = Manager<SignalStore, Registered>;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What a failed send means, in words a person can use.
pub fn explain(error: &PresageError<super::store::SignalStoreError>) -> String {
    use presage::libsignal_service::prelude::MessageSenderError as Sender;
    match error {
        PresageError::MessageSenderError(inner) => match inner.as_ref() {
            Sender::NotFound { .. } => "That person is not on Signal any more".to_string(),
            Sender::UntrustedIdentity { .. } => "Their safety number has changed. Check it with them in Signal before sending".to_string(),
            Sender::ServiceError(ServiceError::RateLimitExceeded { .. }) => "Signal asked for a pause: too many messages too quickly".to_string(),
            Sender::ServiceError(ServiceError::Unauthorized) => "Signal has unlinked this device".to_string(),
            other => format!("{other}"),
        },
        PresageError::UnknownGroup => "That group is not known to this account".to_string(),
        PresageError::ServiceError(ServiceError::RateLimitExceeded { .. }) => "Signal asked for a pause: too many messages too quickly".to_string(),
        other => format!("{other}"),
    }
}

/// Puts the group's context on a message for a group: its master key and the revision the account knows. Without it
/// the members' clients take the message for a one-to-one one (the library leaves this to its caller).
pub fn in_group(body: ContentBody, key: &[u8; 32], revision: u32) -> ContentBody {
    let context = || Some(presage::proto::GroupContextV2 { master_key: Some(key.to_vec()), revision: Some(revision), group_change: None });
    match body {
        ContentBody::DataMessage(mut dm) => {
            if dm.group_v2.is_none() {
                dm.group_v2 = context();
            }
            ContentBody::DataMessage(dm)
        }
        ContentBody::EditMessage(mut edit) => {
            if let Some(dm) = edit.data_message.as_mut() {
                if dm.group_v2.is_none() {
                    dm.group_v2 = context();
                }
            }
            ContentBody::EditMessage(edit)
        }
        other => other,
    }
}

/// Sends `body` to `thread`. The error is already in words.
async fn deliver(manager: &mut Library, thread: &Thread, body: impl Into<ContentBody>, at: u64) -> Result<(), String> {
    let sent = match thread {
        Thread::Contact(id) => manager.send_message(*id, body, at).await,
        Thread::Group(key) => {
            use presage::store::ContentsStore;
            let revision = manager.store().group(*key).await.ok().flatten().map(|g| g.revision).unwrap_or(0);
            manager.send_message_to_group(key, in_group(body.into(), key, revision), at).await
        }
    };
    sent.map_err(|e| {
        tracing::info!("signal: a send failed: {e}");
        explain(&e)
    })
}

fn finish(done: super::Done, result: Result<(), String>, at: u64) {
    let _ = done.send(result.map(|()| at));
}

pub fn run(state: &AppState, handle: &Handle, manager: &Library, command: Command) {
    let account_id = handle.account_id.clone();
    let handle = handle.clone();
    let state = state.clone();
    let mut manager = manager.clone();
    tokio::task::spawn_local(async move {
        match command {
            Command::RefreshContacts => {
                if let Err(e) = manager.request_contacts().await {
                    tracing::warn!("signal[{account_id}]: asking the phone for its contacts: {e}");
                }
            }
            Command::Devices { done } => {
                let this = manager.registration_data().device_id.unwrap_or(0);
                let result = manager.devices().await.map_err(|e| explain(&e)).map(|list| {
                    list.into_iter()
                        .map(|d| super::DeviceRow {
                            id: u8::from(d.id),
                            name: d.name.unwrap_or_default(),
                            created_at: d.created_at.timestamp(),
                            last_seen: d.last_seen.timestamp(),
                            is_this: u32::from(u8::from(d.id)) == this,
                        })
                        .collect()
                });
                let _ = done.send(result);
            }
            Command::Rename { name, done } => {
                let result = manager.update_device_name(&name).await.map_err(|e| explain(&e));
                let _ = done.send(result);
            }
            Command::Profile { who } => {
                use presage::{libsignal_service::protocol::Aci, store::ContentsStore};
                let id = ServiceId::Aci(Aci::from(who));
                let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                // The account's own key is part of its registration; everybody else's is what their messages brought.
                let key = if who == own { Some(manager.registration_data().profile_key()) } else { manager.store().profile_key(&id).await.ok().flatten() };
                let Some(key) = key else { return };
                match manager.retrieve_profile_by_uuid(Aci::from(who), key).await {
                    Ok(profile) => {
                        if profile.avatar.is_some() {
                            if let Ok(Some(bytes)) = manager.retrieve_profile_avatar_by_uuid(who, key).await {
                                if let Err(e) = super::media::keep_avatar(&account_id, &who, &bytes).await {
                                    tracing::debug!("signal[{account_id}]: a picture was not kept: {e}");
                                }
                            }
                        }
                        let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own);
                        super::inbox::profile_arrived(&ctx, &who).await;
                    }
                    Err(e) => tracing::debug!("signal[{account_id}]: a profile was not fetched: {e}"),
                }
            }
            Command::Fetch { pointer, done } => {
                let result = manager.get_attachment(&pointer).await.map_err(|e| {
                    tracing::info!("signal[{account_id}]: an attachment was not fetched: {e}");
                    format!("Signal did not give the file: {e}")
                });
                let _ = done.send(result);
            }
            Command::Receipt { to, kind, timestamps } => {
                let at = now_ms();
                let receipt = ReceiptMessage { r#type: Some(kind as i32), timestamp: timestamps };
                if let Err(e) = manager.send_message(to, receipt, at).await {
                    tracing::debug!("signal[{account_id}]: a receipt was not sent: {e}");
                }
            }
            Command::ReadSync { reads } => {
                let at = now_ms();
                let read = reads
                    .into_iter()
                    .map(|(sender, timestamp)| sync_message::Read { sender_aci: Some(sender.to_string()), timestamp: Some(timestamp), sender_aci_binary: Some(sender.as_bytes().to_vec()) })
                    .collect();
                let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                let sync = SyncMessage { read, ..Default::default() };
                if let Err(e) = manager.send_message(own, sync, at).await {
                    tracing::debug!("signal[{account_id}]: the read state was not shared with the other devices: {e}");
                }
            }
            Command::Typing { thread, started } => {
                let at = now_ms();
                let action = if started { typing_message::Action::Started } else { typing_message::Action::Stopped };
                let group_id = match &thread {
                    Thread::Group(key) => Some(group_identifier(key)),
                    Thread::Contact(_) => None,
                };
                let typing = TypingMessage { timestamp: Some(at), action: Some(action as i32), group_id };
                // Typing is passing news: a failure to say it is not worth more than a line in the log.
                if let Err(e) = deliver(&mut manager, &thread, typing, at).await {
                    tracing::debug!("signal[{account_id}]: typing was not sent: {e}");
                }
            }
            Command::AskPhone(kind) => {
                let at = now_ms();
                let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                let ask = SyncMessage { content: Some(sync_message::Content::Request(sync_message::Request { r#type: Some(kind as i32) })), ..Default::default() };
                if let Err(e) = manager.send_message(own, ask, at).await {
                    tracing::debug!("signal[{account_id}]: the phone was not asked: {e}");
                }
            }
            Command::Upload { files, done } => {
                let specs = files
                    .into_iter()
                    .map(|f| {
                        let spec = presage::libsignal_service::sender::AttachmentSpec {
                            content_type: f.content_type,
                            length: f.bytes.len(),
                            file_name: Some(f.name),
                            preview: None,
                            voice_note: None,
                            borderless: None,
                            width: f.width,
                            height: f.height,
                            caption: None,
                            blur_hash: None,
                        };
                        (spec, f.bytes)
                    })
                    .collect();
                let result = match manager.upload_attachments(specs).await {
                    Ok(results) => results.into_iter().collect::<Result<Vec<_>, _>>().map_err(|e| format!("Signal did not take the file: {e}")),
                    Err(e) => Err(explain(&e)),
                };
                if let Err(e) = &result {
                    tracing::info!("signal[{account_id}]: an upload failed: {e}");
                }
                let _ = done.send(result);
            }
            Command::Send { thread, body, ranges, quote, attachments, done } => {
                let at = now_ms();
                let dm = DataMessage { body: Some(body).filter(|b| !b.is_empty()), body_ranges: ranges, quote, attachments, timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::React { thread, target_ms, target_author, emoji, remove, done } => {
                let at = now_ms();
                let reaction = data_message::Reaction {
                    emoji: Some(emoji),
                    remove: Some(remove),
                    target_author_aci: Some(target_author.to_string()),
                    target_author_aci_binary: Some(target_author.as_bytes().to_vec()),
                    target_sent_timestamp: Some(target_ms),
                };
                let dm = DataMessage { reaction: Some(reaction), timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Delete { thread, target_ms, done } => {
                let at = now_ms();
                let dm = DataMessage { delete: Some(data_message::Delete { target_sent_timestamp: Some(target_ms) }), timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Storage => super::storage::refresh(&state, &handle, &manager).await,
            Command::Vote { thread, vote, done } => {
                let at = now_ms();
                let dm = DataMessage { poll_vote: Some(vote), timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::SetProfile { given, family, about, emoji, done } => {
                use presage::libsignal_service::profile_name::ProfileName;
                let name = ProfileName { given_name: given.trim().to_string(), family_name: Some(family.trim().to_string()).filter(|f| !f.is_empty()) };
                let text = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
                let mut result = manager.update_profile(name, text(about), text(emoji)).await.map_err(|e| explain(&e));
                if result.is_ok() {
                    // The account's other devices fetch the profile again, as Signal's clients ask them to.
                    let fetch = sync_message::FetchLatest { r#type: Some(sync_message::fetch_latest::Type::LocalProfile as i32) };
                    let sync = SyncMessage { content: Some(sync_message::Content::FetchLatest(fetch)), ..Default::default() };
                    let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                    result = manager.send_message(own, sync, now_ms()).await.map_err(|e| explain(&e));
                    let own_aci = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own_aci);
                    super::inbox::profile_arrived(&ctx, &own_aci).await;
                }
                let _ = done.send(result);
            }
            Command::Block { thread, blocked, done } => {
                let remote = super::ids::remote_id(&thread);
                let store = manager.store().clone();
                let mut kept = (*super::storage::current(&account_id, &store)).clone();
                kept.conversations.entry(remote.clone()).or_default().blocked = blocked;
                let list = super::storage::blocked_list(&kept);
                let sync = SyncMessage { content: Some(sync_message::Content::Blocked(list)), ..Default::default() };
                let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                let result = manager.send_message(own, sync, now_ms()).await.map_err(|e| explain(&e));
                if result.is_ok() {
                    super::storage::keep(&account_id, &store, kept);
                    // A blocked request is answered by the blocking.
                    if blocked {
                        let own_aci = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                        let ctx = super::people::Ctx::new(&state, &account_id, store.clone(), own_aci);
                        if super::requests::is_held(&store, &remote) {
                            super::requests::apply(&ctx, &thread, super::requests::Answer::Block).await;
                        }
                    }
                }
                let _ = done.send(result);
            }
            Command::Verify { who, verified, done } => {
                let store = manager.store().clone();
                let result = match store.identity_of(&who.to_string()) {
                    Ok(Some(key)) => {
                        use presage::proto::verified::State;
                        let state_value = if verified { State::Verified } else { State::Default };
                        let padding: Vec<u8> = (0..rand::random::<u8>() % 64 + 1).map(|_| rand::random()).collect();
                        let message = presage::proto::Verified {
                            destination_aci: Some(who.to_string()),
                            destination_aci_binary: Some(who.as_bytes().to_vec()),
                            identity_key: Some(key.serialize().to_vec()),
                            state: Some(state_value as i32),
                            null_message: Some(padding),
                        };
                        let sync = SyncMessage { content: Some(sync_message::Content::Verified(message)), ..Default::default() };
                        let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                        manager.send_message(own, sync, now_ms()).await.map_err(|e| explain(&e))
                    }
                    Ok(None) => Err("Their key is not known yet: exchange a message with them first".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                if result.is_ok() {
                    let _ = store.set_blob(&format!("verified:{who}"), if verified { b"1" } else { b"0" });
                }
                let _ = done.send(result);
            }
            Command::EditGroup { key, edit, done } => {
                let result = edit_group(&mut manager, key, edit).await;
                if result.is_ok() {
                    let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own);
                    super::inbox::rename_all(&ctx).await;
                    super::groups::publish_members(&ctx, &key).await;
                }
                let _ = done.send(result);
            }
            Command::CreateGroup { title, members, done } => {
                let acis: Vec<presage::libsignal_service::protocol::Aci> = members.into_iter().map(Into::into).collect();
                let result = manager.create_group(title.trim(), &acis, 0, now_ms()).await.map_err(|e| explain(&e));
                if let Ok(key) = &result {
                    let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own);
                    super::people::buffer_for(&ctx, &Thread::Group(*key)).await;
                    super::groups::publish_members(&ctx, key).await;
                }
                let _ = done.send(result);
            }
            Command::LinkInfo { link, done } => {
                let result = match super::manage::parse_link(&link) {
                    None => Err("That is not a Signal group link".to_string()),
                    Some((key, password)) => manager.group_link_info(key, &password).await.map(|(title, count, approval, _)| (title, count, approval)).map_err(|e| link_error(&e)),
                };
                let _ = done.send(result);
            }
            Command::JoinByLink { link, done } => {
                let result = match super::manage::parse_link(&link) {
                    None => Err("That is not a Signal group link".to_string()),
                    Some((key, password)) => manager.join_group_by_link(key, &password, now_ms()).await.map(|joined| (key, joined)).map_err(|e| link_error(&e)),
                };
                if let Ok((key, true)) = &result {
                    let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own);
                    super::people::buffer_for(&ctx, &Thread::Group(*key)).await;
                    super::groups::publish_members(&ctx, key).await;
                }
                let _ = done.send(result);
            }
            Command::SetTimer { thread, seconds, done } => {
                let at = now_ms();
                use presage::store::ContentsStore;
                let version = manager.store().expire_timer(&thread).await.ok().flatten().map(|(_, v)| v).unwrap_or(1) + 1;
                let dm = DataMessage { flags: Some(2), expire_timer: Some(seconds), expire_timer_version: Some(version), timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                if result.is_ok() {
                    let mut store = manager.store().clone();
                    let _ = store.update_expire_timer(&thread, seconds, version).await;
                }
                finish(done, result, at);
            }
            Command::LookUpUsername { name, done } => {
                let result = match manager.lookup_username(&name).await {
                    Ok(found) => Ok(found.map(uuid::Uuid::from)),
                    // A name that cannot be a username at all is said as such, not as a network failure.
                    Err(PresageError::InvalidUsername(_)) => Err(format!("{name} is not a Signal username: one is a name, a dot and a number, like name.42")),
                    Err(e) => Err(explain(&e)),
                };
                let _ = done.send(result);
            }
            Command::AnswerRequest { thread, answer, done } => {
                let at = now_ms();
                let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                let response = match &thread {
                    Thread::Contact(id) => {
                        let who = super::ids::uuid_of(id);
                        sync_message::MessageRequestResponse {
                            thread_aci: Some(who.to_string()),
                            thread_aci_binary: Some(who.as_bytes().to_vec()),
                            group_id: None,
                            r#type: Some(answer.response_type() as i32),
                        }
                    }
                    Thread::Group(key) => sync_message::MessageRequestResponse { thread_aci: None, thread_aci_binary: None, group_id: Some(group_identifier(key)), r#type: Some(answer.response_type() as i32) },
                };
                let sync = SyncMessage { content: Some(sync_message::Content::MessageRequestResponse(response)), ..Default::default() };
                let mut result = manager.send_message(own, sync, at).await.map_err(|e| explain(&e));
                // Accepting a person shares the account's profile with them, as Signal's clients do: the library puts the
                // profile key on the message.
                if result.is_ok() && answer == super::requests::Answer::Accept {
                    if let Thread::Contact(id) = &thread {
                        let dm = DataMessage { flags: Some(4), timestamp: Some(at + 1), ..Default::default() };
                        result = manager.send_message(*id, dm, at + 1).await.map_err(|e| explain(&e));
                    }
                }
                if result.is_ok() {
                    let own_aci = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own_aci);
                    super::requests::apply(&ctx, &thread, answer).await;
                }
                let _ = done.send(result);
            }
            Command::Edit { thread, target_ms, body, ranges, done } => {
                let at = now_ms();
                let dm = DataMessage { body: Some(body), body_ranges: ranges, timestamp: Some(at), ..Default::default() };
                let edit = EditMessage { target_sent_timestamp: Some(target_ms), data_message: Some(dm) };
                let result = deliver(&mut manager, &thread, edit, at).await;
                finish(done, result, at);
            }
        }
    });
}

/// Makes one change to a group, after checking it is one the account may make.
async fn edit_group(manager: &mut Library, key: [u8; 32], edit: super::manage::Edit) -> Result<(), String> {
    use presage::store::ContentsStore;
    let own = manager.registration_data().service_ids.aci();
    let group = manager.store().group(key).await.ok().flatten().ok_or("That group is not known to this account")?;
    super::manage::allowed(&group, &own, &edit)?;
    // Adding: each person straight in when their credential can be had, invited otherwise.
    let mut edit = edit;
    let mut added = Vec::new();
    if let super::manage::Edit::Add(people) = &mut edit {
        let operations = presage::libsignal_service::groups_v2::GroupOperations::new(presage::libsignal_service::zkgroup::groups::GroupSecretParams::derive_from_master_key(
            presage::libsignal_service::zkgroup::groups::GroupMasterKey::new(key),
        ));
        for (who, direct) in people.iter_mut() {
            if group.is_member(&(*who).into()) {
                return Err("That person is already in the group".to_string());
            }
            match manager.add_member_action((*who).into(), &operations).await {
                Ok(Some(action)) => {
                    added.push(action);
                    *direct = true;
                }
                Ok(None) => *direct = false,
                Err(e) => {
                    tracing::info!("signal: no credential for somebody being added ({e}); inviting instead");
                    *direct = false;
                }
            }
        }
    }
    let result = manager.change_group(key, |operations, group| super::manage::actions(operations, group, &own, &edit, added), now_ms()).await;
    result.map(|_| ()).map_err(|e| explain(&e))
}

/// Words for a group link that did not work.
fn link_error(error: &PresageError<super::store::SignalStoreError>) -> String {
    match error {
        PresageError::ServiceError(ServiceError::Unauthorized) | PresageError::ServiceError(ServiceError::NotFoundError) => "That group link does not work any more: it was reset or turned off".to_string(),
        other => explain(other),
    }
}

/// The identifier Signal's servers know a group by, which is not its master key: typing notices name the group this way.
pub fn group_identifier(master_key: &[u8; 32]) -> Vec<u8> {
    use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
    GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key)).get_group_identifier().to_vec()
}

/// The service id of a person's thread, for the places that want one.
pub fn recipient(thread: &Thread) -> Option<ServiceId> {
    match thread {
        Thread::Contact(id) => Some(*id),
        Thread::Group(_) => None,
    }
}

/// The quote a reply to message `id` carries: who said it and what, as the daemon holds it.
pub fn quote_of(state: &AppState, buffer_id: &str, id: &str) -> Option<data_message::Quote> {
    let (ts, author) = ids::parse_message_id(id)?;
    let held = state.store.get_message(buffer_id, id).ok().flatten();
    Some(data_message::Quote {
        id: Some(ts),
        author_aci: Some(author.to_string()),
        author_aci_binary: Some(author.as_bytes().to_vec()),
        text: held.map(|m| m.body),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_for_a_group_carries_the_group_and_an_edit_does_too() {
        let key = [3u8; 32];
        let ContentBody::DataMessage(dm) = in_group(ContentBody::DataMessage(DataMessage { body: Some("hi".into()), ..Default::default() }), &key, 7) else { panic!() };
        let context = dm.group_v2.unwrap();
        assert_eq!((context.master_key.unwrap(), context.revision), (key.to_vec(), Some(7)));
        let edit = EditMessage { target_sent_timestamp: Some(1), data_message: Some(DataMessage::default()) };
        let ContentBody::EditMessage(edit) = in_group(ContentBody::EditMessage(edit), &key, 7) else { panic!() };
        assert!(edit.data_message.unwrap().group_v2.is_some());
        // A typing notice names its group its own way, and is left alone.
        let typing = TypingMessage { timestamp: Some(1), action: Some(0), group_id: Some(vec![1]) };
        assert!(matches!(in_group(ContentBody::TypingMessage(typing.clone()), &key, 7), ContentBody::TypingMessage(t) if t == typing));
    }
}

