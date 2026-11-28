//! An account's connection: the session checked, the conversation list read, and the event stream kept open.
//!
//! 1. `get_self_user_status` says whose session this is (and fails at once when nobody's is).
//! 2. `paginated_world` lists the conversations; `get_members` names the people in them; each becomes a buffer.
//! 3. The BrowserChannel is opened and kept open ([`super::channel`]), registering again when it expires or the server
//!    forgets it. What arrived while it was away is caught up from the conversation list: a conversation whose latest
//!    activity moved has its newest history read again.
//!
//! A signed-out session ends the account's connection with a word to sign in again; it is not retried, since every
//! retry would be refused the same way and a loop of refused sign-ins is what gets a session flagged.

use super::api::Api;
use super::channel::{Channel, End};
use super::session::{GcError, Session};
use super::world::{self, World};
use super::{messages, pb, Group, Handle};
use crate::accounts::GoogleChatAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
/// How often rotated cookies are written back to the account.
const SAVE_EVERY: Duration = Duration::from_secs(10 * 60);

enum Ending {
    SignedOut,
    Closed(String),
}

pub fn spawn(state: AppState, config: GoogleChatAccountConfig) {
    let account_id = config.account_id();
    crate::net::route::router().mark(&account_id, config.use_tor);
    state.runtime.reset_connection(&account_id);
    let task = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_with_retry(&state, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, task.abort_handle());
}

async fn run_with_retry(state: &AppState, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        let Some(config) = state.accounts.get_googlechat(account_id) else { return };
        let started = Instant::now();
        let detail = match run_once(state, &config, account_id).await {
            Ok(Ending::SignedOut) => return signed_out(state, account_id),
            Err(e) if e.is_signed_out() => return signed_out(state, account_id),
            Ok(Ending::Closed(why)) => why,
            Err(e) => {
                tracing::warn!("googlechat[{account_id}]: {e}");
                e.to_string()
            }
        };
        state.runtime.clear_googlechat_handle(account_id);
        if started.elapsed() > Duration::from_secs(120) {
            delay = RECONNECT_INITIAL_DELAY;
        }
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
        tracing::info!("googlechat[{account_id}]: disconnected ({detail}); reconnecting in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

fn signed_out(state: &AppState, account_id: &str) {
    tracing::info!("googlechat[{account_id}]: signed out");
    state.runtime.clear_googlechat_handle(account_id);
    state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some("Google has signed this session out. Sign in to Google Chat again from Accounts."));
}

/// Saves cookies Google rotated, so the next start begins from the session as it now is.
fn save_cookies(state: &AppState, handle: &Handle) {
    if let Some(jar) = handle.api.session.take_rotated() {
        if let Err(e) = state.accounts.mutate_googlechat(&handle.account_id, |a| a.cookies = jar) {
            tracing::warn!("googlechat[{}]: could not save the session's cookies: {e:#}", handle.account_id);
        }
    }
}

async fn run_once(state: &AppState, config: &GoogleChatAccountConfig, account_id: &str) -> Result<Ending, GcError> {
    let router = crate::net::route::router();
    if router.routed(account_id) {
        router.ready(|msg| state.runtime.report_progress(state, account_id, msg)).await.map_err(|e| GcError::Network(format!("starting Tor or reaching the SOCKS5 proxy: {e:#}")))?;
    }
    let api = Arc::new(Api::new(Session::new(account_id, config.cookies.clone(), &config.user_agent)));
    let own_id = api.self_user_status().await?.user_status.and_then(|s| s.user_id).map(|u| u.id().to_string()).filter(|s| !s.is_empty()).ok_or_else(|| GcError::Network("Google Chat did not say whose session this is".into()))?;
    if !config.user_id.is_empty() && own_id != config.user_id {
        // The browser's first account is somebody else now. Not this account's session any more.
        tracing::warn!("googlechat[{account_id}]: the session belongs to another Google account");
        return Ok(Ending::SignedOut);
    }

    let handle = Handle { account_id: account_id.to_string(), api: api.clone(), world: Arc::new(Mutex::new(World { own_id: own_id.clone(), ..Default::default() })) };
    world::learn_people(&handle, vec![own_id.clone()]).await;
    let world_response = api.paginated_world().await?;
    let unknown = {
        let mut world = handle.world();
        if let Some(me) = world.people.get(&own_id).cloned() {
            world.own_name = me.name;
            world.own_email = me.email;
        }
        world::absorb_world(&mut world, &world_response);
        world.unknown_people()
    };
    world::learn_people(&handle, unknown).await;

    let (own_name, own_email) = {
        let world = handle.world();
        (world.own_name.clone(), world.own_email.clone())
    };
    if !own_name.is_empty() {
        state.runtime.set_own_identity(account_id, &own_name);
    }
    if (!own_name.is_empty() && own_name != config.name) || (!own_email.is_empty() && own_email != config.email) {
        let _ = state.accounts.mutate_googlechat(account_id, |a| {
            if !own_name.is_empty() {
                a.name = own_name.clone();
            }
            if !own_email.is_empty() {
                a.email = own_email.clone();
            }
        });
        state.events.emit("accountsChanged", serde_json::json!({}));
    }
    world::register_rail(state, &handle);
    let groups: Vec<Group> = handle.world().conversations.keys().cloned().collect();
    for group in &groups {
        world::register(state, &handle, group, false);
    }
    state.runtime.set_googlechat_handle(account_id, handle.clone());
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
    tracing::info!("googlechat[{account_id}]: connected ({} conversations)", groups.len());
    save_cookies(state, &handle);

    // Events are handled in order on a task of their own, so the stream is read while a name is being looked up.
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<pb::Event>();
    let worker = tokio::spawn({
        let state = state.clone();
        let handle = handle.clone();
        async move {
            let mut save = tokio::time::interval(SAVE_EVERY);
            save.tick().await;
            // Messages sent with history off leave scrollback when the server deletes them; the first sweep takes
            // whatever fell due while moho was not running.
            let mut expire = tokio::time::interval(super::retention::SWEEP_EVERY);
            loop {
                tokio::select! {
                    event = events_rx.recv() => match event {
                        Some(event) => handle_event(&state, &handle, event).await,
                        None => break,
                    },
                    _ = save.tick() => save_cookies(&state, &handle),
                    _ = expire.tick() => { super::retention::sweep(&state, &handle.account_id); }
                }
            }
        }
    });
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _worker = AbortOnDrop(worker.abort_handle());

    let mut first = true;
    let mut quick_ends = 0u32;
    loop {
        if !first {
            catch_up(state, &handle).await?;
        }
        first = false;
        let started = Instant::now();
        let tx = events_tx.clone();
        let mut on_event = move |event: pb::Event| {
            let _ = tx.send(event);
        };
        let end = Channel::new(&api.session).run(&mut on_event, &mut || {}).await;
        save_cookies(state, &handle);
        // A channel that ends again straight away, again and again, is not going to work by registering faster.
        if started.elapsed() < Duration::from_secs(60) {
            quick_ends += 1;
        } else {
            quick_ends = 0;
        }
        match end {
            End::SignedOut => return Ok(Ending::SignedOut),
            End::Failed(why) => return Ok(Ending::Closed(why)),
            End::Expired | End::UnknownSid if quick_ends < 3 => {
                tracing::debug!("googlechat[{account_id}]: event stream ended ({end:?}); registering again");
                tokio::time::sleep(Duration::from_secs(1 << quick_ends)).await;
            }
            End::Expired | End::UnknownSid => return Ok(Ending::Closed("the event stream kept closing".into())),
        }
    }
}

/// After the stream was away: the conversation list read again, new conversations shown, and the newest history read
/// for every conversation that moved meanwhile.
async fn catch_up(state: &AppState, handle: &Handle) -> Result<(), GcError> {
    let response = handle.api.paginated_world().await?;
    let (moved, unknown) = {
        let mut world = handle.world();
        let before: std::collections::HashMap<Group, i64> = world.conversations.iter().map(|(g, c)| (g.clone(), c.sort_time)).collect();
        world::absorb_world(&mut world, &response);
        let moved: Vec<Group> = world.conversations.iter().filter(|(g, c)| before.get(*g).is_none_or(|t| c.sort_time > *t)).map(|(g, _)| g.clone()).collect();
        (moved, world.unknown_people())
    };
    world::learn_people(handle, unknown).await;
    for group in moved {
        let buffer = world::register(state, handle, &group, true);
        if let Err(e) = messages::history(state, handle, &buffer, false).await {
            tracing::debug!("googlechat[{}]: catching up: {e:#}", handle.account_id);
        }
    }
    Ok(())
}

async fn handle_event(state: &AppState, handle: &Handle, event: pb::Event) {
    use pb::event::event_body::Type;
    let kind = event.r#type();
    let group = Group::from_pb(event.group_id.as_ref());
    let Some(body) = event.body.and_then(|b| b.r#type) else { return };
    match body {
        Type::MessagePosted(posted) => {
            let (Some(group), Some(message)) = (group, posted.message) else { return };
            if kind == pb::event::EventType::MessageUpdated {
                messages::update_live(state, handle, &group, &message).await;
                return;
            }
            if handle_system_message(state, handle, &group, &message).await {
                return;
            }
            messages::record(state, handle, &group, &message).await;
        }
        Type::MessageDeleted(deleted) => {
            let Some(group) = group else { return };
            let Some(msg_id) = deleted.message_id.as_ref().map(|m| m.message_id().to_string()).filter(|s| !s.is_empty()) else { return };
            if let Some(buffer) = world::buffer_of(state, &handle.account_id, &group) {
                state.runtime.delete_message(state, &buffer.id, &msg_id);
            }
        }
        Type::MessageReaction(reaction) => {
            let Some(group) = group else { return };
            let Some(msg_id) = reaction.message_id.as_ref().map(|m| m.message_id().to_string()).filter(|s| !s.is_empty()) else { return };
            let add = match reaction.r#type() {
                pb::message_reaction_event::ReactionEventType::Add => true,
                pb::message_reaction_event::ReactionEventType::Remove => false,
            };
            let emoji = match reaction.emoji.as_ref().and_then(|e| e.content.as_ref()) {
                Some(pb::emoji::Content::Unicode(u)) if !u.is_empty() => u.replace('\u{fe0f}', ""),
                _ => return,
            };
            let sender = reaction.user_id.as_ref().map(|u| u.id().to_string()).unwrap_or_default();
            let is_me = {
                let mut world = handle.world();
                if !sender.is_empty() {
                    let list = world.reactors.entry((msg_id.clone(), emoji.clone())).or_default();
                    if add {
                        if !list.contains(&sender) {
                            list.push(sender.clone());
                        }
                    } else {
                        list.retain(|u| u != &sender);
                    }
                }
                sender == world.own_id
            };
            if let Some(buffer) = world::buffer_of(state, &handle.account_id, &group) {
                state.runtime.update_reaction(state, &buffer.id, &msg_id, &emoji, is_me, add);
            }
        }
        Type::TypingStateChanged(typing) => {
            // Unlike other events, TypingStateChanged carries the group inside its TypingContext (outer group_id is empty).
            let typing_group = typing.context.as_ref().and_then(|c| c.context.as_ref()).and_then(|ctx| match ctx {
                pb::typing_context::Context::GroupId(gid) => Group::from_pb(Some(gid)),
                pb::typing_context::Context::TopicId(tid) => Group::from_pb(tid.group_id.as_ref()),
            });
            let Some(group) = typing_group.or(group) else { return };
            let sender = typing.user_id.as_ref().map(|u| u.id().to_string()).unwrap_or_default();
            let (own_id, known) = {
                let w = handle.world();
                (w.own_id.clone(), sender.is_empty() || w.people.contains_key(&sender))
            };
            if sender.is_empty() || sender == own_id {
                return;
            }
            if !known {
                world::learn_people(handle, vec![sender.clone()]).await;
            }
            let Some(buffer) = world::buffer_of(state, &handle.account_id, &group) else { return };
            let name = handle.world().name_of(&sender);
            if typing.state() == pb::TypingState::Typing {
                state.events.emit("typing", serde_json::json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": [name], "expiresInMs": 6000 }));
            } else {
                state.events.emit("typing", serde_json::json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": [], "expiresInMs": 0 }));
            }
        }
        Type::ReadReceiptChanged(changed) => {
            let Some(group) = group else { return };
            let Some(buffer) = world::buffer_of(state, &handle.account_id, &group) else { return };
            let receipts = changed.read_receipt_set.as_ref().map(|s| s.read_receipts.as_slice()).unwrap_or(&[]);
            if receipts.is_empty() {
                return;
            }
            let own_id = handle.world().own_id.clone();
            let mut readers: Vec<serde_json::Value> = Vec::new();
            for rr in receipts {
                let uid = rr.user.as_ref().and_then(|u| u.user_id.as_ref()).map(|u| u.id().to_string()).unwrap_or_default();
                if uid.is_empty() || uid == own_id {
                    continue;
                }
                let name = handle.world().name_of(&uid);
                let avatar = handle.world().people.get(&uid).and_then(|p| p.avatar.clone());
                readers.push(serde_json::json!({
                    "userId": uid,
                    "nick": name,
                    "avatarUrl": avatar,
                    "readAt": super::micros_to_secs(rr.read_time_micros()),
                }));
            }
            if !readers.is_empty() {
                state.events.emit("readReceipts", serde_json::json!({ "accountId": handle.account_id, "bufferId": buffer.id, "readers": readers }));
            }
        }
        Type::GroupViewed(_) => {
            if let Some(group) = group {
                if let Some(buffer) = world::buffer_of(state, &handle.account_id, &group) {
                    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
                }
            }
        }
        Type::GroupReadStateUpdatedEvent(updated) => {
            if let Some(group) = group.or_else(|| Group::from_pb(updated.group_id.as_ref())) {
                if let Some(buffer) = world::buffer_of(state, &handle.account_id, &group) {
                    if updated.mark_as_unread_time() > 0 {
                        state.runtime.set_buffer_unread(state, &buffer.id, 1, 0);
                    } else if updated.most_recent_read_time() >= updated.last_head_message_create_time() {
                        state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
                    }
                }
            }
        }
        Type::MembershipChanged(changed) => {
            let Some(m) = changed.new_membership else { return };
            let uid = m
                .id
                .as_ref()
                .and_then(|id| id.member_id.as_ref())
                .and_then(|mid| mid.id.as_ref())
                .map(|pb::member_id::Id::UserId(u)| u.id().to_string())
                .unwrap_or_default();
            let grp = m.id.as_ref().and_then(|id| Group::from_pb(id.group_id.as_ref())).or(group);
            let Some(Group::Space(space_id)) = grp else { return };
            if uid != handle.world().own_id {
                return;
            }
            match m.membership_state() {
                pb::MembershipState::MemberInvited => {
                    if m.invite_category() == pb::InviteCategory::SpamInvite {
                        return;
                    }
                    let mut world = handle.world();
                    if !world.pending_invites.contains(&space_id) {
                        world.pending_invites.push(space_id);
                    }
                }
                pb::MembershipState::MemberJoined => {
                    handle.world().pending_invites.retain(|id| id != &space_id);
                    world::register(state, handle, &Group::Space(space_id), false);
                }
                pb::MembershipState::MemberNotAMember => {
                    let mut world = handle.world();
                    world.pending_invites.retain(|id| id != &space_id);
                    world.conversations.remove(&Group::Space(space_id.clone()));
                }
                _ => {}
            }
        }
        Type::UserStatusUpdated(updated) => {
            let Some(us) = updated.user_status else { return };
            let Some(uid) = us.user_id.as_ref().map(|u| u.id().to_string()).filter(|s| !s.is_empty()) else { return };
            let dnd = us.dnd_settings.as_ref().is_some_and(|d| d.dnd_state() == pb::dnd_settings::DndStateState::Dnd);
            let custom = us.custom_status.as_ref().and_then(|cs| {
                let emoji = cs
                    .emoji
                    .as_ref()
                    .and_then(|e| match &e.content {
                        Some(pb::emoji::Content::Unicode(u)) => Some(u.as_str()),
                        _ => None,
                    })
                    .unwrap_or(cs.status_emoji());
                let combined = format!("{emoji} {}", cs.status_text()).trim().to_string();
                (!combined.is_empty()).then_some(combined)
            });
            let mut world = handle.world();
            let person = world.people.entry(uid).or_default();
            if dnd {
                person.status = "dnd".to_string();
            } else if person.status == "dnd" {
                person.status = "online".to_string();
            }
            person.custom_status = custom;
        }
        _ => {}
    }
}

/// Handles `SYSTEM_MESSAGE` events carrying a single `ROOM_UPDATED` or `MEMBERSHIP_CHANGED` annotation.
/// Returns `true` if handled as a state update so it isn't duplicated as a plain message.
async fn handle_system_message(state: &AppState, handle: &Handle, group: &Group, message: &pb::Message) -> bool {
    if message.message_type() != pb::message::MessageType::SystemMessage || message.annotations.len() != 1 {
        return false;
    }
    let ann = &message.annotations[0];
    match &ann.metadata {
        Some(pb::annotation::Metadata::RoomUpdated(update)) => {
            let mut handled = false;
            if let Some(new_name) = update.rename_metadata.as_ref().and_then(|r| r.new_name.as_deref()).filter(|s| !s.is_empty()) {
                if let Some(conv) = handle.world().conversations.get_mut(group) {
                    conv.name = Some(new_name.to_string());
                }
                world::register(state, handle, group, false);
                handled = true;
            } else if let Some(details) = update.group_details_metadata.as_ref() {
                let desc = details.new_group_details.as_ref().map(|d| d.description().to_string()).unwrap_or_default();
                if let Some(conv) = handle.world().conversations.get_mut(group) {
                    conv.description = Some(desc.clone());
                }
                if let Some(buffer) = world::buffer_of(state, &handle.account_id, group) {
                    state.runtime.set_buffer_topic(state, &buffer.id, &desc);
                }
                handled = true;
            }
            handled
        }
        Some(pb::annotation::Metadata::MembershipChanged(change)) => {
            use pb::membership_changed_metadata::Type as McType;
            let affected: Vec<String> = change
                .affected_members
                .iter()
                .filter_map(|m| m.id.as_ref().map(|pb::member_id::Id::UserId(u)| u.id().to_string()))
                .filter(|s| !s.is_empty())
                .collect();
            let unknown: Vec<String> = {
                let mut world = handle.world();
                if let Some(conv) = world.conversations.get_mut(group) {
                    match change.r#type() {
                        McType::Joined | McType::Added | McType::BotAdded => {
                            for id in &affected {
                                if !conv.members.contains(id) {
                                    conv.members.push(id.clone());
                                }
                            }
                        }
                        McType::Left | McType::Removed | McType::BotRemoved | McType::KickedDueToOtrConflict => {
                            conv.members.retain(|id| !affected.contains(id));
                        }
                        _ => {}
                    }
                }
                affected.iter().filter(|id| !world.people.contains_key(*id)).cloned().collect()
            };
            if !unknown.is_empty() {
                world::learn_people(handle, unknown).await;
            }
            if let Some(buffer) = world::buffer_of(state, &handle.account_id, group) {
                let names: Vec<String> = {
                    let world = handle.world();
                    affected.iter().map(|id| world.name_of(id)).collect()
                };
                if !names.is_empty() {
                    let verb = match change.r#type() {
                        McType::Invited => "was invited",
                        McType::Joined => "joined",
                        McType::Added | McType::BotAdded => "was added",
                        McType::Left => "left",
                        McType::Removed | McType::BotRemoved | McType::KickedDueToOtrConflict => "was removed",
                        _ => return true,
                    };
                    let text = format!("{} {verb}", names.join(", "));
                    let sent_at = super::micros_to_secs(message.create_time());
                    state.runtime.record_message_at(
                        state,
                        &handle.account_id,
                        &buffer.name,
                        &buffer.kind,
                        "",
                        &text,
                        false,
                        "system",
                        None,
                        message.id.as_ref().map(|m| m.message_id().to_string()),
                        false,
                        None,
                        Vec::new(),
                        Vec::new(),
                        None,
                        (sent_at > 0).then_some(sent_at),
                        None,
                        None,
                    );
                }
            }
            true
        }
        _ => false,
    }
}
