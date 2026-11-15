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

/// Sends `body` to `thread`. The error is already in words. A group message that reached some members and not others
/// counts as sent, and who it missed (and why) is said in the group as a line.
async fn deliver(state: &AppState, account_id: &str, manager: &mut Library, thread: &Thread, body: impl Into<ContentBody>, at: u64) -> Result<(), String> {
    let body: ContentBody = body.into();
    // Passing news (typing) that missed somebody is not worth a line.
    let worth_saying = !matches!(body, ContentBody::TypingMessage(_));
    let sent = match thread {
        Thread::Contact(id) => manager.send_message(*id, body, at).await,
        Thread::Group(key) => {
            use presage::store::ContentsStore;
            let revision = manager.store().group(*key).await.ok().flatten().map(|g| g.revision).unwrap_or(0);
            manager.send_message_to_group(key, in_group(body, key, revision), at).await
        }
    };
    match sent {
        Err(PresageError::PartialGroupSend(_)) if !worth_saying => Ok(()),
        Err(PresageError::PartialGroupSend(failed)) => {
            tracing::info!("signal[{account_id}]: a group message missed {} member(s)", failed.len());
            let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
            let ctx = super::people::Ctx::new(state, account_id, manager.store().clone(), own);
            let buffer = super::people::buffer_for(&ctx, thread).await;
            let mut missed = Vec::new();
            for (who, error) in failed {
                let name = super::people::person(&ctx, &ids::uuid_of(&who)).await;
                missed.push(format!("{name} ({})", explain(&PresageError::MessageSenderError(Box::new(error))).to_lowercase()));
            }
            super::inbox::system(&ctx, &buffer, &format!("Not delivered to {}", missed.join(", ")), at + 1);
            Ok(())
        }
        other => other.map_err(|e| {
            tracing::info!("signal: a send failed: {e}");
            explain(&e)
        }),
    }
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
            Command::Fetch { pointer, plaintext_hash, done } => {
                let fetched = match &plaintext_hash {
                    Some(hash) => manager.get_attachment_by_plaintext_hash(&pointer, hash).await,
                    None => manager.get_attachment(&pointer).await,
                };
                let result = fetched.map_err(|e| {
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
                if let Err(e) = deliver(&state, &account_id, &mut manager, &thread, typing, at).await {
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
                            voice_note: f.voice.then_some(true),
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
            Command::Send { thread, body, ranges, quote, attachments, view_once, done } => {
                let at = now_ms();
                let is_view_once = view_once.then_some(true);
                let dm = DataMessage { body: Some(body).filter(|b| !b.is_empty()), body_ranges: ranges, quote, attachments, is_view_once, timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
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
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Delete { thread, target_ms, done } => {
                let at = now_ms();
                let dm = DataMessage { delete: Some(data_message::Delete { target_sent_timestamp: Some(target_ms) }), timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Storage => super::storage::refresh(&state, &handle, &manager).await,
            Command::SendSticker { thread, pack_id, sticker_id, done } => {
                use presage::store::ContentsStore;
                let at = now_ms();
                let result = async {
                    let pack = manager.store().sticker_pack(&pack_id).await.ok().flatten().ok_or("that sticker pack is not installed")?;
                    let sticker = pack.manifest.stickers.iter().find(|s| s.id == sticker_id).cloned().ok_or("that sticker is not in its pack")?;
                    let bytes = sticker.bytes.clone().ok_or("that sticker's picture has not been fetched")?;
                    let content_type = sticker.content_type.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| "image/webp".to_string());
                    let spec = presage::libsignal_service::sender::AttachmentSpec {
                        content_type,
                        length: bytes.len(),
                        file_name: None,
                        preview: None,
                        voice_note: None,
                        borderless: Some(true),
                        width: None,
                        height: None,
                        caption: None,
                        blur_hash: None,
                    };
                    let pointer = match manager.upload_attachments(vec![(spec, bytes)]).await {
                        Ok(mut results) => results.pop().ok_or("Signal did not take the sticker")?.map_err(|e| format!("Signal did not take the sticker: {e}"))?,
                        Err(e) => return Err(explain(&e)),
                    };
                    let message = data_message::Sticker { pack_id: Some(pack.id.clone()), pack_key: Some(pack.key.clone()), sticker_id: Some(sticker_id), data: Some(pointer), emoji: sticker.emoji.clone() };
                    let dm = DataMessage { sticker: Some(message), timestamp: Some(at), ..Default::default() };
                    deliver(&state, &account_id, &mut manager, &thread, dm, at).await
                }
                .await;
                finish(done, result.map_err(|e: String| e), at);
            }
            Command::StoryAnswer { thread, context, body, emoji, done } => {
                let at = now_ms();
                let reaction = emoji.map(|(emoji, remove)| data_message::Reaction {
                    emoji: Some(emoji),
                    remove: Some(remove),
                    target_author_aci: context.author_aci.clone(),
                    target_author_aci_binary: context.author_aci_binary.clone(),
                    target_sent_timestamp: context.sent_timestamp,
                });
                let dm = DataMessage { body, reaction, story_context: Some(context), timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Vote { thread, vote, done } => {
                let at = now_ms();
                let dm = DataMessage { poll_vote: Some(vote), timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::EndPoll { thread, target_ms, done } => {
                let at = now_ms();
                let end = data_message::PollTerminate { target_sent_timestamp: Some(target_ms) };
                let dm = DataMessage { poll_terminate: Some(end), timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Poll { thread, create, done } => {
                let at = now_ms();
                let dm = DataMessage { poll_create: Some(create), timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::SetProfile { given, family, about, emoji, picture, done } => {
                use presage::libsignal_service::profile_name::ProfileName;
                let name = ProfileName { given_name: given.trim().to_string(), family_name: Some(family.trim().to_string()).filter(|f| !f.is_empty()) };
                let text = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
                let mut result = match &picture {
                    None => manager.update_profile(name, text(about), text(emoji)).await,
                    Some(bytes) => manager.update_profile_with_picture(name, text(about), text(emoji), bytes.as_deref()).await,
                }
                .map_err(|e| explain(&e));
                if result.is_ok() {
                    // The account's own picture, as moho shows it, is the one just set.
                    let own_aci = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    match &picture {
                        Some(Some(bytes)) => {
                            let _ = super::media::keep_avatar(&account_id, &own_aci, bytes).await;
                        }
                        Some(None) => {
                            let _ = std::fs::remove_file(super::media::avatar_path(&account_id, &own_aci));
                        }
                        None => {}
                    }
                }
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
                    // Looked at: sends to them are no longer held.
                    super::profile::confirm(&store, &who);
                }
                let _ = done.send(result);
            }
            Command::EditGroup { key, edit, picture, done } => {
                // A new picture goes up to the CDN first; the change then names it by the key it was given.
                let edit = match (edit, picture) {
                    (super::manage::Edit::Picture(_), Some(bytes)) => match manager.upload_group_avatar(key, &bytes).await {
                        Ok(cdn_key) => super::manage::Edit::Picture(cdn_key),
                        Err(e) => {
                            let _ = done.send(Err(format!("the picture could not be put up: {}", explain(&e))));
                            return;
                        }
                    },
                    (edit, _) => edit,
                };
                let result = edit_group(&mut manager, key, edit).await;
                if let Ok(signed) = &result {
                    let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own);
                    super::inbox::rename_all(&ctx).await;
                    // Said here as the members' clients say it (seen on the first real account: the phone showed each
                    // change made from moho, and moho showed none).
                    super::groups::say_own_change(&ctx, key, signed).await;
                    super::groups::publish_members(&ctx, &key).await;
                    super::groups::show_avatar(&ctx, &manager, &key).await;
                }
                let _ = done.send(result.map(|_| ()));
            }
            Command::CreateGroup { title, members, done } => {
                let acis: Vec<presage::libsignal_service::protocol::Aci> = members.into_iter().map(Into::into).collect();
                let result = manager.create_group(title.trim(), &acis, 0, now_ms()).await.map_err(|e| explain(&e));
                if let Ok(key) = &result {
                    let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                    let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own);
                    let buffer = super::people::buffer_for(&ctx, &Thread::Group(*key)).await;
                    super::inbox::system(&ctx, &buffer, "You created the group", now_ms());
                    super::groups::publish_members(&ctx, key).await;
                    super::groups::show_avatar(&ctx, &manager, key).await;
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
                    super::groups::show_avatar(&ctx, &manager, key).await;
                }
                let _ = done.send(result);
            }
            Command::SetTimer { thread, seconds, done } => {
                let at = now_ms();
                use presage::store::ContentsStore;
                let version = manager.store().expire_timer(&thread).await.ok().flatten().map(|(_, v)| v).unwrap_or(1) + 1;
                let dm = DataMessage { flags: Some(2), expire_timer: Some(seconds), expire_timer_version: Some(version), timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
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
            Command::LookUpNumber { number, done } => {
                let result = match manager.discover_contacts_by_phone_number([number.as_str()]).await {
                    Ok(found) => number_found(found.into_iter().map(|(_, id)| id).collect()),
                    Err(PresageError::ServiceError(ServiceError::RateLimitExceeded { .. })) => Err("Signal asked for a pause: too many numbers looked up".to_string()),
                    Err(e) => {
                        tracing::info!("signal[{account_id}]: a number lookup failed: {e}");
                        Err(format!("Signal's number lookup did not answer: {}", explain(&e)))
                    }
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
            Command::PostStory { key, mut story, done } => {
                use presage::store::ContentsStore;
                let at = now_ms();
                let revision = manager.store().group(key).await.ok().flatten().map(|g| g.revision).unwrap_or(0);
                story.group = Some(presage::proto::GroupContextV2 { master_key: Some(key.to_vec()), revision: Some(revision), group_change: None });
                story.profile_key = Some(manager.registration_data().profile_key().get_bytes().to_vec());
                let result = manager.send_message_to_group(&key, ContentBody::StoryMessage(*story), at).await.map_err(|e| {
                    tracing::info!("signal[{account_id}]: a story was not posted: {e}");
                    explain(&e)
                });
                finish(done, result, at);
            }
            Command::PostStoryTo { people, list, mut story, done } => {
                let at = now_ms();
                story.profile_key = Some(manager.registration_data().profile_key().get_bytes().to_vec());
                let acis: Vec<presage::libsignal_service::protocol::Aci> = people.into_iter().map(Into::into).collect();
                let result = match manager.send_story_to(&acis, *story, at, &[list.to_string()]).await {
                    Ok(()) => Ok(()),
                    // Some missed it: still posted; who it missed is in the log (there is no conversation to say it in).
                    Err(PresageError::PartialGroupSend(failed)) => {
                        tracing::info!("signal[{account_id}]: a story missed {} of its audience", failed.len());
                        Ok(())
                    }
                    Err(e) => Err(explain(&e)),
                };
                finish(done, result, at);
            }
            Command::StoriesViewed { views, receipt } => {
                let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                let viewed = views.iter().map(|(sender, ts)| sync_message::Viewed { sender_aci: Some(sender.to_string()), timestamp: Some(*ts), sender_aci_binary: Some(sender.as_bytes().to_vec()) }).collect();
                if let Err(e) = manager.send_message(own, SyncMessage { viewed, ..Default::default() }, now_ms()).await {
                    tracing::debug!("signal[{account_id}]: the other devices were not told stories were viewed: {e}");
                }
                if receipt {
                    let mut by_author: std::collections::HashMap<uuid::Uuid, Vec<u64>> = std::collections::HashMap::new();
                    for (sender, ts) in views {
                        by_author.entry(sender).or_default().push(ts);
                    }
                    for (author, timestamps) in by_author {
                        let receipt = ReceiptMessage { r#type: Some(presage::proto::receipt_message::Type::Viewed as i32), timestamp: timestamps };
                        if let Err(e) = manager.send_message(ServiceId::Aci(author.into()), receipt, now_ms()).await {
                            tracing::debug!("signal[{account_id}]: a story view was not told to its author: {e}");
                        }
                    }
                }
            }
            Command::SharedChange { target, change, done } => {
                let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                let device = manager.registration_data().device_id.unwrap_or(0);
                let mut result = super::storage_write::write(&manager, own, device, &target, &change).await;
                if result.is_ok() {
                    // The other devices fetch the new manifest, as Signal's apps ask them to after writing it.
                    let fetch = sync_message::FetchLatest { r#type: Some(sync_message::fetch_latest::Type::StorageManifest as i32) };
                    let sync = SyncMessage { content: Some(sync_message::Content::FetchLatest(fetch)), ..Default::default() };
                    let me = ServiceId::Aci(manager.registration_data().service_ids.aci());
                    if let Err(e) = manager.send_message(me, sync, now_ms()).await {
                        tracing::info!("signal[{account_id}]: the other devices were not told the shared settings changed: {e}");
                    }
                    // And this one reads it back, so what it shows is what was written.
                    super::storage::refresh(&state, &handle, &manager).await;
                } else if let Err(e) = &result {
                    tracing::info!("signal[{account_id}]: a shared change was not made: {e}");
                    result = Err(e.clone());
                }
                let _ = done.send(result);
            }
            Command::Username { nickname, discriminator, reset_link, done } => {
                use super::storage_write::{Change, Target};
                let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                let device = manager.registration_data().device_id.unwrap_or(0);
                let kept = super::storage::current(&account_id, manager.store());
                let result: Result<(Option<String>, Change), String> = async {
                    if reset_link {
                        let name = kept.own_username.clone().ok_or("There is no username to make a link for")?;
                        let (entropy, handle) = manager.reset_username_link(&name).await.map_err(|e| explain(&e))?;
                        return Ok((Some(name), Change::UsernameLink(entropy, handle)));
                    }
                    match nickname {
                        None => {
                            manager.delete_username().await.map_err(|e| explain(&e))?;
                            Ok((None, Change::Username(None)))
                        }
                        Some(nick) => match manager.set_username(nick.trim(), discriminator.as_deref().map(str::trim).filter(|d| !d.is_empty())).await {
                            Ok((name, entropy, handle)) => Ok((Some(name.clone()), Change::Username(Some((name, entropy, handle))))),
                            Err(PresageError::UsernameTaken) => Err("That username is taken: try another number, or let Signal choose one".to_string()),
                            Err(PresageError::InvalidUsername(e)) => Err(format!("That is not a username Signal takes ({e}): 3 to 32 letters, digits or _, not starting with a digit")),
                            Err(e) => Err(explain(&e)),
                        },
                    }
                }
                .await;
                let result = match result {
                    Ok((name, change)) => match super::storage_write::write(&manager, own, device, &Target::Account, &change).await {
                        Ok(()) => {
                            let fetch = sync_message::FetchLatest { r#type: Some(sync_message::fetch_latest::Type::StorageManifest as i32) };
                            let sync = SyncMessage { content: Some(sync_message::Content::FetchLatest(fetch)), ..Default::default() };
                            let me = ServiceId::Aci(manager.registration_data().service_ids.aci());
                            let _ = manager.send_message(me, sync, now_ms()).await;
                            super::storage::refresh(&state, &handle, &manager).await;
                            Ok(name)
                        }
                        // Signal has it; only the shared copy lags, which the phone puts right when it next writes it.
                        Err(e) => {
                            tracing::info!("signal[{account_id}]: the username was changed but not written to the shared settings: {e}");
                            Ok(name)
                        }
                    },
                    Err(e) => Err(e),
                };
                let _ = done.send(result);
            }
            Command::CreateStoryList { name, people, done } => {
                let device = manager.registration_data().device_id.unwrap_or(0);
                let result = super::storage_write::create_list(&manager, device, &name, &people).await;
                if result.is_ok() {
                    let fetch = sync_message::FetchLatest { r#type: Some(sync_message::fetch_latest::Type::StorageManifest as i32) };
                    let sync = SyncMessage { content: Some(sync_message::Content::FetchLatest(fetch)), ..Default::default() };
                    let me = ServiceId::Aci(manager.registration_data().service_ids.aci());
                    let _ = manager.send_message(me, sync, now_ms()).await;
                    super::storage::refresh(&state, &handle, &manager).await;
                }
                let _ = done.send(result);
            }
            Command::StickerPack { id, key, install, done } => {
                let result = if install { manager.install_sticker_pack(&id, &key).await } else { manager.remove_sticker_pack(&id, &key).await };
                let _ = done.send(result.map_err(|e| format!("Signal did not {} the pack: {}", if install { "add" } else { "remove" }, explain(&e))));
            }
            Command::ImportHistory => {
                let Some(key) = handle.store.blob(super::history::KEY_BLOB).ok().flatten().and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok()) else { return };
                // Taken now, whatever comes of it: a history the phone did not send is not waited for on every connect.
                let _ = handle.store.set_blob(super::history::KEY_BLOB, &[]);
                let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
                let ctx = super::people::Ctx::new(&state, &account_id, manager.store().clone(), own);
                let note = super::people::buffer_for(&ctx, &Thread::Contact(ServiceId::Aci(own.into()))).await;
                super::inbox::system(&ctx, &note, "Waiting for your phone to send its message history", now_ms());
                // The phone uploads it after the link; it can take a while for a long history.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
                let archive = loop {
                    if tokio::time::Instant::now() > deadline {
                        break Err("The phone did not send its message history in time".to_string());
                    }
                    match manager.transfer_archive(25).await {
                        Ok(None) => continue,
                        Ok(Some(presage::libsignal_service::websocket::linking::TransferArchive::Ready { cdn, key: cdn_key })) => break Ok((cdn, cdn_key)),
                        Ok(Some(presage::libsignal_service::websocket::linking::TransferArchive::Failed { error })) => {
                            break Err(if error == "CONTINUE_WITHOUT_UPLOAD" { "The phone chose not to send its message history".to_string() } else { format!("The phone did not send its message history ({error})") })
                        }
                        Err(e) => break Err(format!("The message history could not be asked for: {}", explain(&e))),
                    }
                };
                let words = match archive {
                    Err(e) => e,
                    Ok((cdn, cdn_key)) => match manager.download_transfer_archive(cdn, &cdn_key).await {
                        Err(e) => format!("The message history could not be downloaded: {}", explain(&e)),
                        Ok(bytes) => match super::history::read(&bytes, &key, &own).await {
                            Err(e) => format!("The message history could not be read: {e}"),
                            Ok(frames) => {
                                let old = super::history::messages(&frames);
                                let brought = super::history::import(&ctx, &old).await;
                                tracing::info!("signal[{account_id}]: {brought} messages of history brought over");
                                format!("Your phone's message history was brought over: {brought} messages (pictures and files stay on the phone)")
                            }
                        },
                    },
                };
                super::inbox::system(&ctx, &note, &words, now_ms());
            }
            Command::SendContact { thread, card, done } => {
                let at = now_ms();
                let dm = DataMessage { contact: vec![*card], timestamp: Some(at), ..Default::default() };
                let result = deliver(&state, &account_id, &mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::CallEventSync { remote, call_id, at_ms, video, incoming, accepted } => {
                use presage::proto::sync_message::call_event::{Direction, Event, Type};
                let event = sync_message::CallEvent {
                    conversation_id: Some(remote.as_bytes().to_vec()),
                    call_id: Some(call_id),
                    timestamp: Some(at_ms),
                    r#type: Some(if video { Type::VideoCall } else { Type::AudioCall } as i32),
                    direction: Some(if incoming { Direction::Incoming } else { Direction::Outgoing } as i32),
                    event: Some(if accepted { Event::Accepted } else { Event::NotAccepted } as i32),
                };
                let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                let sync = SyncMessage { content: Some(sync_message::Content::CallEvent(event)), ..Default::default() };
                if let Err(e) = manager.send_message(own, sync, now_ms()).await {
                    tracing::debug!("signal[{account_id}]: the call log was not shared with the other devices: {e}");
                }
            }
            Command::ResetSession { who, done } => {
                use presage::libsignal_service::session_store::SessionStoreExt;
                use presage::store::Store;
                let at = now_ms();
                let id = ServiceId::Aci(who.into());
                let result = async {
                    manager.store().aci_protocol_store().delete_all_sessions(&id).await.map_err(|e| format!("the old session could not be forgotten: {e}"))?;
                    // An empty message: the library fetches their keys and starts a new session to send it.
                    let padding: Vec<u8> = (0..rand::random::<u8>() % 64 + 1).map(|_| rand::random()).collect();
                    manager.send_message(id, ContentBody::NullMessage(presage::proto::NullMessage { padding: Some(padding) }), at).await.map_err(|e| explain(&e))
                }
                .await;
                finish(done, result, at);
            }
            Command::CallLinkCredentials { start, end, done } => {
                let result = manager.call_link_auth_credentials(start, end).await.map_err(|e| format!("Signal did not give call link credentials: {}", explain(&e)));
                let _ = done.send(result);
            }
            Command::CreateCallLinkCredential { request, done } => {
                let result = manager.create_call_link_credential(&request).await.map_err(|e| format!("Signal did not let a call link be made: {}", explain(&e)));
                let _ = done.send(result);
            }
            Command::SaveCallLink { root_key, admin_passkey, done } => {
                let device = manager.registration_data().device_id.unwrap_or(0);
                let result = super::storage_write::create_call_link(&manager, device, &root_key, &admin_passkey).await;
                if result.is_ok() {
                    let fetch = sync_message::FetchLatest { r#type: Some(sync_message::fetch_latest::Type::StorageManifest as i32) };
                    let sync = SyncMessage { content: Some(sync_message::Content::FetchLatest(fetch)), ..Default::default() };
                    let me = ServiceId::Aci(manager.registration_data().service_ids.aci());
                    let _ = manager.send_message(me, sync, now_ms()).await;
                    super::storage::refresh(&state, &handle, &manager).await;
                }
                let _ = done.send(result);
            }
            Command::GroupCallProof { key, done } => {
                let result = manager.group_call_proof(key).await.map_err(|e| format!("the group server did not give a call token: {}", explain(&e)));
                let _ = done.send(result);
            }
            Command::CallMessageToPeople { people, call, done } => {
                let at = now_ms();
                let acis: Vec<presage::libsignal_service::protocol::Aci> = people.into_iter().map(Into::into).collect();
                let result = match manager.send_to_people(&acis, ContentBody::CallMessage(*call), at, &[]).await {
                    Ok(()) | Err(PresageError::PartialGroupSend(_)) => Ok(()),
                    Err(e) => Err(explain(&e)),
                };
                finish(done, result, at);
            }
            Command::CallMessage { to, call, done } => {
                let at = now_ms();
                let result = manager.send_message(ServiceId::Aci(to.into()), ContentBody::CallMessage(*call), at).await.map_err(|e| {
                    tracing::info!("signal[{account_id}]: a call message was not sent: {e}");
                    explain(&e)
                });
                finish(done, result, at);
            }
            Command::CallRelays { done } => {
                let result = manager.calling_relays().await.map_err(|e| explain(&e)).map(|relays| {
                    // RingRTC's ICE servers: the plain addresses, and those with IPs beside the name they are checked by.
                    let servers: Vec<serde_json::Value> = relays
                        .into_iter()
                        .flat_map(|r| {
                            let mut out = vec![serde_json::json!({ "username": r.username, "password": r.password, "urls": r.urls })];
                            if !r.urls_with_ips.is_empty() {
                                out.push(serde_json::json!({ "username": r.username, "password": r.password, "urls": r.urls_with_ips, "hostname": r.hostname }));
                            }
                            out
                        })
                        .collect();
                    serde_json::Value::Array(servers)
                });
                let _ = done.send(result);
            }
            Command::ViewOnceOpened { sender, sent_ms, receipt } => {
                let own = ServiceId::Aci(manager.registration_data().service_ids.aci());
                let open = sync_message::ViewOnceOpen { sender_aci: Some(sender.to_string()), timestamp: Some(sent_ms), sender_aci_binary: Some(sender.as_bytes().to_vec()) };
                let sync = SyncMessage { content: Some(sync_message::Content::ViewOnceOpen(open)), ..Default::default() };
                if let Err(e) = manager.send_message(own, sync, now_ms()).await {
                    tracing::debug!("signal[{account_id}]: the other devices were not told a view-once picture was opened: {e}");
                }
                if receipt {
                    let viewed = ReceiptMessage { r#type: Some(presage::proto::receipt_message::Type::Viewed as i32), timestamp: vec![sent_ms] };
                    if let Err(e) = manager.send_message(ServiceId::Aci(sender.into()), viewed, now_ms()).await {
                        tracing::debug!("signal[{account_id}]: a viewed receipt was not sent: {e}");
                    }
                }
            }
            Command::Edit { thread, target_ms, body, ranges, done } => {
                let at = now_ms();
                let dm = DataMessage { body: Some(body), body_ranges: ranges, timestamp: Some(at), ..Default::default() };
                let edit = EditMessage { target_sent_timestamp: Some(target_ms), data_message: Some(dm) };
                let result = deliver(&state, &account_id, &mut manager, &thread, edit, at).await;
                finish(done, result, at);
            }
        }
    });
}

/// What a number lookup found: an ACI is somebody to talk to. Only a PNI is an account that is on Signal but has chosen
/// not to be found by its number, which is said as such; nothing is nobody.
fn number_found(found: Vec<Option<ServiceId>>) -> Result<Option<uuid::Uuid>, String> {
    if let Some(aci) = found.iter().flatten().find_map(|id| match id {
        ServiceId::Aci(aci) => Some(uuid::Uuid::from(*aci)),
        _ => None,
    }) {
        return Ok(Some(aci));
    }
    if found.iter().flatten().any(|id| matches!(id, ServiceId::Pni(_))) {
        return Err("That number is on Signal, but its owner chose not to be found by it: ask them for their username".to_string());
    }
    Ok(None)
}

/// Makes one change to a group, after checking it is one the account may make.
async fn edit_group(manager: &mut Library, key: [u8; 32], edit: super::manage::Edit) -> Result<presage::proto::GroupChange, String> {
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
    result.map(|(_, signed)| signed).map_err(|e| explain(&e))
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
pub fn quote_of(state: &AppState, signal: &super::store::SignalStore, buffer_id: &str, id: &str) -> Option<data_message::Quote> {
    let (ts, author) = ids::parse_message_id(id)?;
    let held = state.store.get_message(buffer_id, id).ok().flatten();
    Some(data_message::Quote {
        // An edited message is quoted by its newest revision, as Signal's clients name it.
        id: Some(super::revisions::latest_ms(signal, id, ts)),
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
    fn a_number_lookup_tells_somebody_from_somebody_hidden_from_nobody() {
        use presage::libsignal_service::protocol::{Aci, Pni};
        let who = uuid::Uuid::from_u128(7);
        assert_eq!(number_found(vec![Some(ServiceId::Aci(Aci::from(who)))]), Ok(Some(who)));
        assert!(number_found(vec![Some(ServiceId::Pni(Pni::from(who)))]).unwrap_err().contains("chose not to be found"));
        assert_eq!(number_found(vec![None]), Ok(None));
    }

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

