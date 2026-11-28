//! The RPCs: binary protobuf POSTed to `/u/0/api/<name>`, answered in binary protobuf (or that in base64, which the
//! server sometimes chooses).
//!
//! A 401 is first taken to mean the XSRF token went stale: the page is asked for a fresh one and the request made once
//! more. Only a second refusal, or the page itself saying nobody is signed in, ends the session.
//!
//! Requests that make something - a message, a conversation - are not repeated after a server error, since the first
//! one may well have done it. A 429 is repeated after the wait it asks for, as it was refused rather than done.

use super::pb;
use super::session::{GcError, Session, BASE};
use base64::Engine;
use reqwest::header::{HeaderValue, CONTENT_TYPE, ORIGIN, REFERER};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The web client's API key: without it every RPC is refused.
const API_KEY: &str = "AIzaSyD7InnYR3VKdb4j2rMUEbTCIr2VyEazl6k";

/// The web client version the requests say they come from.
const CLIENT_VERSION: i64 = 2_440_378_181_258;

const ATTEMPTS: usize = 3;

pub struct Api {
    pub session: Session,
    counter: AtomicU64,
}

pub fn request_header() -> pb::RequestHeader {
    pb::RequestHeader {
        client_type: Some(pb::request_header::ClientType::Web as i32),
        client_version: Some(CLIENT_VERSION),
        client_feature_capabilities: Some(pb::ClientFeatureCapabilities {
            spam_room_invites_level: Some(pb::client_feature_capabilities::CapabilityLevel::FullySupported as i32),
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl Api {
    pub fn new(session: Session) -> Self {
        Api { session, counter: AtomicU64::new(0) }
    }

    /// One RPC. `idempotent` says whether a server error may be answered by asking again.
    pub async fn call<Req: prost::Message, Resp: prost::Message + Default>(&self, endpoint: &str, request: &Req, idempotent: bool) -> Result<Resp, GcError> {
        let token = self.session.ensure_xsrf().await?;
        match self.call_once(endpoint, request, &token, idempotent).await {
            Err(GcError::Status { status: 401, .. }) => {
                let fresh = self.session.refresh_xsrf(Some(&token)).await?;
                self.call_once(endpoint, request, &fresh, idempotent).await
            }
            other => other,
        }
    }

    async fn call_once<Req: prost::Message, Resp: prost::Message + Default>(&self, endpoint: &str, request: &Req, xsrf: &str, idempotent: bool) -> Result<Resp, GcError> {
        let body = request.encode_to_vec();
        let mut last = GcError::Network("no attempt made".into());
        for attempt in 0..ATTEMPTS {
            let count = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            let url = format!("{BASE}/api/{endpoint}?c={count}&rt=b&alt=proto&key={API_KEY}");
            let mut headers = self.session.headers(&url);
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/x-protobuf"));
            headers.insert("x-goog-encode-response-if-executable", HeaderValue::from_static("base64"));
            if let Ok(v) = HeaderValue::from_str(xsrf) {
                headers.insert("x-framework-xsrf-token", v);
            }
            let response = match self.session.client().post(&url).headers(headers).body(body.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = GcError::Network(crate::backend::mattermost::api::network_words(&e));
                    // Not sent at all, or no answer: a send may have happened, so only an idempotent call goes again.
                    if !idempotent && !e.is_connect() {
                        return Err(last);
                    }
                    tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                    continue;
                }
            };
            self.session.absorb(&response);
            let status = response.status().as_u16();
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let bytes = response.bytes().await.map_err(|e| GcError::Network(e.to_string()))?;
            if status == 200 {
                return decode_response(&bytes).map_err(|e| GcError::Network(format!("could not read the answer to {endpoint}: {e}")));
            }
            last = GcError::Status { status, body: String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).into_owned() };
            if status == 429 {
                tokio::time::sleep(Duration::from_secs(retry_after.unwrap_or(2).min(30))).await;
                continue;
            }
            if (500..600).contains(&status) && idempotent {
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                continue;
            }
            return Err(last);
        }
        Err(last)
    }

    pub async fn self_user_status(&self) -> Result<pb::GetSelfUserStatusResponse, GcError> {
        self.call("get_self_user_status", &pb::GetSelfUserStatusRequest { request_header: Some(request_header()) }, true).await
    }

    /// Profiles for a list of people.
    pub async fn members(&self, gaia_ids: &[String]) -> Result<pb::GetMembersResponse, GcError> {
        let request = pb::GetMembersRequest { request_header: Some(request_header()), member_ids: gaia_ids.iter().map(|id| super::member_id(id)).collect(), ..Default::default() };
        self.call("get_members", &request, true).await
    }

    /// Every conversation the account is in. Without a section request carrying a page size the server answers
    /// with an empty list.
    pub async fn paginated_world(&self) -> Result<pb::PaginatedWorldResponse, GcError> {
        let request = pb::PaginatedWorldRequest {
            request_header: Some(request_header()),
            fetch_from_user_spaces: Some(true),
            fetch_options: vec![pb::paginated_world_request::FetchOptions::ExcludeGroupLite as i32],
            world_section_requests: vec![pb::WorldSectionRequest { page_size: Some(999), ..Default::default() }],
            ..Default::default()
        };
        self.call("paginated_world", &request, true).await
    }

    /// The newest topics of a conversation, oldest last, with their replies.
    pub async fn list_topics(&self, group: &super::Group, topics: i32) -> Result<pb::ListTopicsResponse, GcError> {
        let request = pb::ListTopicsRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
            page_size_for_topics: Some(topics),
            page_size_for_replies: Some(30),
            ..Default::default()
        };
        self.call("list_topics", &request, true).await
    }

    /// Starts a topic: a message of its own, which in a direct message or a flat space is every message.
    pub async fn create_topic(&self, request: pb::CreateTopicRequest) -> Result<pb::CreateTopicResponse, GcError> {
        let request = pb::CreateTopicRequest { request_header: Some(request_header()), ..request };
        self.call("create_topic", &request, false).await
    }

    /// A reply inside a topic (a thread).
    pub async fn create_message(&self, request: pb::CreateMessageRequest) -> Result<pb::CreateMessageResponse, GcError> {
        let request = pb::CreateMessageRequest { request_header: Some(request_header()), ..request };
        self.call("create_message", &request, false).await
    }

    /// Marks a conversation read up to a time, in microseconds.
    pub async fn mark_read(&self, group: &super::Group, at_micros: i64) -> Result<pb::MarkGroupReadstateResponse, GcError> {
        let request = pb::MarkGroupReadstateRequest { request_header: Some(request_header()), id: Some(group.to_pb()), last_read_time: Some(at_micros) };
        self.call("mark_group_readstate", &request, true).await
    }

    /// Fetches one conversation (DM or space), including its members and details.
    pub async fn get_group(&self, group: &super::Group) -> Result<pb::GetGroupResponse, GcError> {
        let request = pb::GetGroupRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
            fetch_options: vec![
                pb::get_group_request::FetchOptions::Members as i32,
                pb::get_group_request::FetchOptions::IncludeDynamicGroupName as i32,
            ],
            // Without this, get_group 404s for DMs.
            include_invite_dms: Some(true),
            ..Default::default()
        };
        self.call("get_group", &request, true).await
    }

    /// Edits a message's text and annotations.
    pub async fn edit_message(&self, request: pb::EditMessageRequest) -> Result<pb::EditMessageResponse, GcError> {
        let request = pb::EditMessageRequest { request_header: Some(request_header()), ..request };
        self.call("edit_message", &request, true).await
    }

    /// Deletes a message.
    pub async fn delete_message(&self, message_id: pb::MessageId) -> Result<pb::DeleteMessageResponse, GcError> {
        let request = pb::DeleteMessageRequest { request_header: Some(request_header()), message_id: Some(message_id) };
        self.call("delete_message", &request, true).await
    }

    /// Adds or removes an emoji reaction on a message.
    pub async fn update_reaction(&self, message_id: pb::MessageId, emoji: &str, add: bool) -> Result<pb::UpdateReactionResponse, GcError> {
        let kind = if add {
            pb::update_reaction_request::ReactionUpdateType::Add
        } else {
            pb::update_reaction_request::ReactionUpdateType::Remove
        };
        let request = pb::UpdateReactionRequest {
            request_header: Some(request_header()),
            message_id: Some(message_id),
            emoji: Some(pb::Emoji { content: Some(pb::emoji::Content::Unicode(emoji.to_string())) }),
            r#type: Some(kind as i32),
        };
        self.call("update_reaction", &request, true).await
    }

    /// Sends a typing indicator (TYPING or STOPPED).
    pub async fn set_typing_state(&self, group: &super::Group, topic_id: Option<&str>, typing: bool) -> Result<pb::SetTypingStateResponse, GcError> {
        let context = match topic_id.filter(|t| !t.is_empty()) {
            Some(topic) => pb::TypingContext {
                context: Some(pb::typing_context::Context::TopicId(pb::TopicId {
                    group_id: Some(group.to_pb()),
                    topic_id: Some(topic.to_string()),
                })),
            },
            None => pb::TypingContext { context: Some(pb::typing_context::Context::GroupId(group.to_pb())) },
        };
        let state = if typing { pb::TypingState::Typing } else { pb::TypingState::Stopped };
        let request = pb::SetTypingStateRequest {
            request_header: Some(request_header()),
            state: Some(state as i32),
            context: Some(context),
        };
        self.call("set_typing_state", &request, true).await
    }

    /// Queries online/active/DND presence for one or more users.
    pub async fn get_user_presence(&self, gaia_ids: &[String]) -> Result<pb::GetUserPresenceResponse, GcError> {
        let request = pb::GetUserPresenceRequest {
            request_header: Some(request_header()),
            user_ids: gaia_ids.iter().map(|id| super::user_id(id)).collect(),
            include_active_until: Some(true),
            include_user_status: Some(true),
        };
        self.call("get_user_presence", &request, true).await
    }

    /// Pages through messages inside a topic (thread).
    pub async fn list_messages(&self, group: &super::Group, topic_id: &str, page_size: i32) -> Result<pb::ListMessagesResponse, GcError> {
        let request = pb::ListMessagesRequest {
            request_header: Some(request_header()),
            parent_id: Some(pb::MessageParentId {
                parent: Some(pb::message_parent_id::Parent::TopicId(pb::TopicId {
                    group_id: Some(group.to_pb()),
                    topic_id: Some(topic_id.to_string()),
                })),
            }),
            page_size: Some(page_size),
        };
        self.call("list_messages", &request, true).await
    }

    /// Replays missed user-level events since a revision.
    pub async fn catch_up_user(&self, from_revision: i64) -> Result<pb::CatchUpResponse, GcError> {
        let request = pb::CatchUpUserRequest {
            request_header: Some(request_header()),
            range: Some(pb::CatchUpRange { from_revision_timestamp: Some(from_revision), ..Default::default() }),
            page_size: Some(100),
            ..Default::default()
        };
        self.call("catch_up_user", &request, true).await
    }

    /// Replays missed events for one conversation since a revision.
    pub async fn catch_up_group(&self, group: &super::Group, from_revision: i64) -> Result<pb::CatchUpResponse, GcError> {
        let request = pb::CatchUpGroupRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
            range: Some(pb::CatchUpRange { from_revision_timestamp: Some(from_revision), ..Default::default() }),
            page_size: Some(100),
            ..Default::default()
        };
        self.call("catch_up_group", &request, true).await
    }

    /// Adds or invites members to a space (also used with own member_id to accept a space invite).
    pub async fn create_membership(&self, request: pb::CreateMembershipRequest) -> Result<pb::CreateMembershipResponse, GcError> {
        let request = pb::CreateMembershipRequest { request_header: Some(request_header()), ..request };
        self.call("create_membership", &request, true).await
    }

    /// Removes a member from a space (removing yourself leaves it or declines an invite).
    pub async fn remove_memberships(&self, group: &super::Group, gaia_id: &str) -> Result<pb::RemoveMembershipsResponse, GcError> {
        let request = pb::RemoveMembershipsRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
            member_ids: vec![super::member_id(gaia_id)],
            membership_state: Some(pb::MembershipState::MemberInvited as i32),
        };
        self.call("remove_memberships", &request, true).await
    }

    /// Creates or opens an existing DM by gaia user id or email address.
    pub async fn create_dm(&self, request: pb::CreateDmRequest) -> Result<pb::CreateDmResponse, GcError> {
        let request = pb::CreateDmRequest { request_header: Some(request_header()), ..request };
        self.call("create_dm", &request, false).await
    }

    /// Starts (or finds) a direct message the way the web app does: by email and, when known, id.
    pub async fn create_dm_extended(&self, email: Option<&str>, gaia: Option<&str>) -> Result<pb::CreateDmExtendedResponse, GcError> {
        let request = pb::CreateDmExtendedRequest {
            request_header: Some(request_header()),
            invitees: vec![pb::DmInvitee { user_id: gaia.map(super::user_id), email: email.map(str::to_string), kind: Some(1) }],
            unknown5: vec![1],
            unknown8: Some(0),
            unknown9: Some(9),
        };
        self.call("create_dm_extended", &request, false).await
    }

    /// Mutes or unmutes one conversation (setting_type: 4 for DM, 5 for Space).
    pub async fn update_group_notification_settings(&self, group: &super::Group, muted: bool) -> Result<pb::UpdateGroupNotificationSettingsResponse, GcError> {
        let setting_type = if group.is_dm() { 4 } else { 5 };
        let state = if muted {
            pb::group_notification_settings_update::mute::State::Muted
        } else {
            pb::group_notification_settings_update::mute::State::Unmuted
        };
        let request = pb::UpdateGroupNotificationSettingsRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
            update: Some(pb::GroupNotificationSettingsUpdate {
                setting_type: Some(setting_type),
                mute: Some(pb::group_notification_settings_update::Mute { state: Some(state as i32) }),
            }),
        };
        self.call("update_group_notification_settings", &request, true).await
    }

    /// Hides or unhides a conversation in the user's chat list (how a DM is put away).
    pub async fn hide_group(&self, group: &super::Group, hide: bool) -> Result<pb::HideGroupResponse, GcError> {
        let request = pb::HideGroupRequest {
            request_header: Some(request_header()),
            id: Some(group.to_pb()),
            hide: Some(hide),
        };
        self.call("hide_group", &request, true).await
    }

    /// Updates a space's name or details (spaces only).
    pub async fn update_group(&self, request: pb::UpdateGroupRequest) -> Result<pb::UpdateGroupResponse, GcError> {
        let request = pb::UpdateGroupRequest { request_header: Some(request_header()), ..request };
        self.call("update_group", &request, true).await
    }

    /// Creates a new Space.
    pub async fn create_group(&self, name: &str, local_id: &str) -> Result<pb::CreateGroupResponse, GcError> {
        let request = pb::CreateGroupRequest {
            request_header: Some(request_header()),
            local_id: Some(local_id.to_string()),
            creation_info: Some(pb::create_group_request::CreationInfo::Space(pb::SpaceCreationInfo {
                name: Some(name.to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        self.call("create_group", &request, false).await
    }

    /// Sets Do Not Disturb on or off (with an optional duration in seconds when turning DND on).
    pub async fn set_dnd_duration(&self, dnd: bool, duration_secs: Option<i64>) -> Result<pb::SetDndDurationResponse, GcError> {
        let (state, expiry) = if dnd {
            let dur_usec = duration_secs.unwrap_or(8 * 3600).max(60) * 1_000_000;
            (None, Some(pb::set_dnd_duration_request::DndExpiry::NewDndDurationUsec(dur_usec)))
        } else {
            (Some(pb::set_dnd_duration_request::State::Available as i32), None)
        };
        let request = pb::SetDndDurationRequest {
            request_header: Some(request_header()),
            current_dnd_state: state,
            dnd_expiry: expiry,
        };
        self.call("set_dnd_duration", &request, true).await
    }

    /// Sets or clears the account's custom status text and optional emoji.
    pub async fn set_custom_status(&self, text: &str, emoji: Option<&str>, duration_secs: Option<i64>) -> Result<pb::SetCustomStatusResponse, GcError> {
        let clearing = text.trim().is_empty() && emoji.is_none_or(|e| e.trim().is_empty());
        let custom_status = if clearing {
            Some(pb::CustomStatus::default())
        } else {
            let emoji_str = emoji.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
            Some(pb::CustomStatus {
                status_text: Some(text.trim().to_string()),
                status_emoji: emoji_str.clone(),
                emoji: emoji_str.map(|u| pb::Emoji { content: Some(pb::emoji::Content::Unicode(u)) }),
                ..Default::default()
            })
        };
        let timing = if clearing {
            Some(pb::set_custom_status_request::CustomStatusTiming::CustomStatusRemainingDurationUsec(0))
        } else {
            let dur = duration_secs.unwrap_or(24 * 3600).max(60) * 1_000_000;
            Some(pb::set_custom_status_request::CustomStatusTiming::CustomStatusRemainingDurationUsec(dur))
        };
        let request = pb::SetCustomStatusRequest {
            request_header: Some(request_header()),
            custom_status,
            custom_status_timing: timing,
        };
        self.call("set_custom_status", &request, true).await
    }

    /// Turns presence sharing on (Active) or off (Invisible / Away).
    pub async fn set_presence_shared(&self, shared: bool) -> Result<pb::SetPresenceSharedResponse, GcError> {
        let request = pb::SetPresenceSharedRequest {
            request_header: Some(request_header()),
            presence_shared: Some(shared),
        };
        self.call("set_presence_shared", &request, true).await
    }

    /// Blocks or unblocks a user (by gaia ID) or a conversation, optionally reporting abuse.
    pub async fn block_entity(&self, entity: pb::block_entity_request::Entity, blocked: bool, reported: bool) -> Result<pb::BlockEntityResponse, GcError> {
        let request = pb::BlockEntityRequest {
            request_header: Some(request_header()),
            entity: Some(entity),
            blocked: Some(blocked),
            reported: reported.then_some(true),
        };
        self.call("block_entity", &request, true).await
    }

    /// Creates a Google Meet video call annotation for a DM or Space.
    pub async fn create_video_call(&self, group: &super::Group) -> Result<pb::CreateVideoCallResponse, GcError> {
        let request = pb::CreateVideoCallRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
        };
        self.call("create_video_call", &request, false).await
    }

    /// Lists user IDs blocked by this account.
    pub async fn list_blocked_users(&self) -> Result<pb::ListBlockedUsersResponse, GcError> {
        let request = pb::ListBlockedUsersRequest { request_header: Some(request_header()) };
        self.call("list_blocked_users", &request, true).await
    }

    /// Queries slash commands offered by bots/apps installed in a conversation.
    pub async fn autocomplete_slash_commands(&self, group: &super::Group, query: &str) -> Result<pb::AutocompleteSlashCommandsResponse, GcError> {
        let request = pb::AutocompleteSlashCommandsRequest {
            request_header: Some(request_header()),
            query: Some(query.to_string()),
            group_id: Some(group.to_pb()),
            max_num_results: Some(25),
            restrict_to_bots_in_group: Some(true),
            include_dialogs: Some(false),
            ..Default::default()
        };
        self.call("autocomplete_slash_commands", &request, true).await
    }

    /// Toggles History On (`PERMANENT = 1`) or History Off (`EPHEMERAL_ONE_DAY = 2`) for a DM or Space.
    pub async fn update_group_retention_settings(&self, group: &super::Group, history_on: bool) -> Result<pb::UpdateGroupRetentionSettingsResponse, GcError> {
        let state = if history_on {
            pb::retention_settings::RetentionState::Permanent as i32
        } else {
            pb::retention_settings::RetentionState::EphemeralOneDay as i32
        };
        let local_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<u64>().to_le_bytes());
        let request = pb::UpdateGroupRetentionSettingsRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
            retention_settings: Some(pb::RetentionSettings { state: Some(state), expiry_timestamp: None }),
            local_id: Some(local_id),
        };
        self.call("update_group_retention_settings", &request, true).await
    }

    /// Promotes or demotes a member's role in a Space (`3` = Member, `4` = Owner, `6` = Manager).
    pub async fn update_membership_role(&self, group: &super::Group, gaia_id: &str, role: i32) -> Result<pb::UpdateMembershipRoleResponse, GcError> {
        let request = pb::UpdateMembershipRoleRequest {
            request_header: Some(request_header()),
            updates: vec![pb::MembershipRoleUpdate {
                role: Some(role),
                member_id: Some(super::user_id(gaia_id)),
                group_id: Some(group.to_pb()),
            }],
        };
        self.call("update_membership_role", &request, true).await
    }

    /// Creates a custom sidebar section (`create_roster_section`).
    pub async fn create_roster_section(&self, display_name: &str, sort_order: i32) -> Result<pb::CreateRosterSectionResponse, GcError> {
        let request = pb::CreateRosterSectionRequest {
            request_header: Some(request_header()),
            section: Some(pb::RosterSection {
                section_id: Some(pb::RosterSectionId {
                    r#type: Some(3),
                    label_id: Some(pb::GroupLabelId { r#type: Some(4), id: Some("1".into()) }),
                    id: Some("1".into()),
                }),
                sort_order: Some(sort_order),
                display_name: Some(display_name.to_string()),
            }),
        };
        self.call("create_roster_section", &request, true).await
    }

    /// Deletes a custom sidebar section by its section ID (`delete_roster_section`).
    pub async fn delete_roster_section(&self, section_id: &str) -> Result<pb::DeleteRosterSectionResponse, GcError> {
        let request = pb::DeleteRosterSectionRequest {
            request_header: Some(request_header()),
            section_id: Some(pb::RosterSectionId {
                r#type: Some(3),
                label_id: Some(pb::GroupLabelId { r#type: Some(4), id: Some(section_id.to_string()) }),
                id: Some(section_id.to_string()),
            }),
        };
        self.call("delete_roster_section", &request, true).await
    }

    /// Moves a DM or Space into a custom sidebar section (`apply_group_label`).
    pub async fn apply_group_label(&self, group: &super::Group, section_id: &str) -> Result<pb::ApplyGroupLabelResponse, GcError> {
        let request = pb::ApplyGroupLabelRequest {
            request_header: Some(request_header()),
            label_id: Some(pb::GroupLabelId { r#type: Some(4), id: Some(section_id.to_string()) }),
            group_id: Some(group.to_pb()),
        };
        self.call("apply_group_label", &request, true).await
    }

    /// Calls a `chat.google.com/_/DynamiteWebUi/data/batchexecute` RPC and returns the parsed `wrb.fr` payload JSON.
    pub async fn batchexecute(&self, rpc_id: &str, payload_json: &str, source_path: &str) -> Result<serde_json::Value, GcError> {
        let tokens = self.session.ensure_batch_tokens().await?;
        let reqid = (rand::random::<u32>() % 900_000 + 100_000).to_string();
        let mut params = vec![
            ("rpcids", rpc_id.to_string()),
            ("source-path", source_path.to_string()),
            ("bl", tokens.bl.clone()),
            ("hl", "en".to_string()),
            ("soc-app", "1".to_string()),
            ("soc-platform", "1".to_string()),
            ("soc-device", "1".to_string()),
            ("_reqid", reqid),
            ("rt", "c".to_string()),
        ];
        if !tokens.fsid.is_empty() {
            params.push(("f.sid", tokens.fsid.clone()));
        }
        let url = reqwest::Url::parse_with_params("https://chat.google.com/_/DynamiteWebUi/data/batchexecute", &params)
            .map_err(|e| GcError::Network(e.to_string()))?;
        let f_req = serde_json::json!([[[rpc_id, payload_json, serde_json::Value::Null, "generic"]]]).to_string();
        let mut headers = self.session.headers(url.as_str());
        headers.insert(ORIGIN, HeaderValue::from_static("https://chat.google.com"));
        headers.insert(REFERER, HeaderValue::from_static("https://chat.google.com/"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded;charset=utf-8"));
        let response = self
            .session
            .client()
            .post(url)
            .headers(headers)
            .form(&[("f.req", f_req.as_str()), ("at", tokens.at.as_str())])
            .send()
            .await
            .map_err(|e| GcError::Network(crate::backend::mattermost::api::network_words(&e)))?;
        self.session.absorb(&response);
        let status = response.status().as_u16();
        let text = response.text().await.map_err(|e| GcError::Network(e.to_string()))?;
        if status == 401 {
            return Err(GcError::SignedOut);
        }
        if status != 200 {
            return Err(GcError::Status { status, body: text });
        }
        parse_batchexecute_response(&text, rpc_id)
    }

    /// Server-side message search (`batchexecute` `SBNmJb`).
    pub async fn search_messages(&self, query: &str, has_link: bool) -> Result<serde_json::Value, GcError> {
        let uuid = format!(
            "{:08X}-{:04X}-4{:03X}-8{:03X}-{:012X}",
            rand::random::<u32>(),
            rand::random::<u16>(),
            rand::random::<u16>() & 0x0fff,
            rand::random::<u16>() & 0x0fff,
            rand::random::<u64>() & 0xffff_ffff_ffff
        );
        let has_link_flag = if has_link { 1 } else { 0 };
        let payload = serde_json::json!([
            null,
            null,
            null,
            query,
            null,
            uuid,
            [
                [],
                null,
                null,
                null,
                uuid,
                null,
                has_link_flag,
                [[[[[[1]]]]]],
                [[[1]]]
            ],
            null,
            [3],
            [175],
            0,
            "",
            1
        ])
        .to_string();
        self.batchexecute("SBNmJb", &payload, "/app/search").await
    }

    /// Browse & discover Spaces (`batchexecute` `jfcZG`).
    pub async fn browse_spaces(&self, query: &str, unjoined_only: bool) -> Result<serde_json::Value, GcError> {
        let filter = if unjoined_only {
            serde_json::json!([[[[3, 2], [2, 0]]], 0])
        } else {
            serde_json::json!([[], 0])
        };
        let payload = serde_json::json!([null, null, null, 1, query, "", null, filter, 0]).to_string();
        self.batchexecute("jfcZG", &payload, "/app/browse").await
    }

    /// Pins or unpins a DM or Space (`batchexecute` `XZhsdc`).
    pub async fn set_pinned(&self, group: &super::Group, pinned: bool) -> Result<bool, GcError> {
        let (resource, raw_id, kind) = match group {
            super::Group::Space(id) => (format!("space/{id}"), id.as_str(), 2),
            super::Group::Dm(id) => (format!("dm/{id}"), id.as_str(), 1),
        };
        let payload = serde_json::json!([[resource, raw_id, kind], if pinned { 1 } else { 0 }]).to_string();
        let val = self.batchexecute("XZhsdc", &payload, "/app/home").await?;
        Ok(val.get(1).and_then(|v| v.as_bool()).unwrap_or(pinned))
    }
}

/// Extracts the inner JSON payload of the matching `["wrb.fr", rpc_id, "<json>", ...]` envelope from a
/// Google `batchexecute` (`rt=c`) response.
pub fn parse_batchexecute_response(body: &str, rpc_id: &str) -> Result<serde_json::Value, GcError> {
    let stripped = body.trim_start_matches(")]}'").trim();
    for line in stripped.lines() {
        let line = line.trim();
        if !line.starts_with('[') {
            continue;
        }
        let Ok(arr) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(entries) = arr.as_array() else { continue };
        for entry in entries {
            let Some(cols) = entry.as_array() else { continue };
            if cols.first().and_then(|v| v.as_str()) == Some("wrb.fr") && cols.get(1).and_then(|v| v.as_str()) == Some(rpc_id) {
                if let Some(inner_str) = cols.get(2).and_then(|v| v.as_str()) {
                    return serde_json::from_str(inner_str).map_err(|e| GcError::Network(format!("bad batchexecute JSON for {rpc_id}: {e}")));
                }
                return Ok(serde_json::Value::Null);
            }
        }
    }
    Err(GcError::Network(format!("Google Chat batchexecute response did not include {rpc_id}")))
}

/// The answer as protobuf, or as base64 of it.
fn decode_response<M: prost::Message + Default>(bytes: &[u8]) -> anyhow::Result<M> {
    if let Ok(message) = M::decode(bytes) {
        return Ok(message);
    }
    let text = std::str::from_utf8(bytes)?.trim();
    let decoded = base64::engine::general_purpose::STANDARD.decode(text)?;
    Ok(M::decode(decoded.as_slice())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_batchexecute_wrb_envelope() {
        let body = ")]}'\n\n198\n[[\"wrb.fr\",\"XZhsdc\",\"[[\\\"space/AAQArMBdIrY\\\",\\\"AAQArMBdIrY\\\",2],true]\",null,null,null,\"generic\"]]\n25\n[[\"e\",4,null,null,234]]\n";
        let val = parse_batchexecute_response(body, "XZhsdc").unwrap();
        assert_eq!(val[1], serde_json::json!(true));
    }
}
