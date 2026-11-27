//! RPC method dispatcher for all Rocket.Chat-specific daemon methods (Rounds 0–9).

use super::{calls, discovery, e2ee, extras, login, media, members, moderation, profile, rooms, search, send, threads};
use crate::state::AppState;
use serde_json::{json, Value};

fn p_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

fn p_bool(params: &Value, key: &str, default: bool) -> bool {
    params.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn p_usize(params: &Value, key: &str, default: usize) -> usize {
    params.get(key).and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(default)
}

fn p_str_vec(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn wrap_result(res: anyhow::Result<Value>) -> (Option<Value>, Option<String>) {
    match res {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    }
}

/// Returns `Some((result, error))` if `method` is a Rocket.Chat RPC, or `None` otherwise.
pub async fn dispatch(
    state: &AppState,
    method: &str,
    params: &Value,
) -> Option<(Option<Value>, Option<String>)> {
    let out = match method {
        // Round 0 / 1: Server discovery & Sign-in
        "rocketchatDiscover" => {
            let Some(url) = p_str(params, "serverUrl") else {
                return Some((None, Some("rocketchatDiscover requires \"serverUrl\"".into())));
            };
            let route_key = p_str(params, "routeKey").unwrap_or("rocketchat:discover");
            if p_bool(params, "useTor", false) {
                let router = crate::net::route::router();
                router.mark(route_key, true);
                if let Err(e) = router.ready(|_| {}).await {
                    return Some((None, Some(format!("proxy/Tor not ready: {e:#}"))));
                }
            }
            match discovery::discover(route_key, url).await {
                Ok(info) => (Some(serde_json::to_value(info).unwrap_or(Value::Null)), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "addRocketChatAccount" => {
            let (Some(url), Some(user), Some(password)) = (
                p_str(params, "serverUrl"),
                p_str(params, "user"),
                p_str(params, "password"),
            ) else {
                return Some((None, Some("addRocketChatAccount requires serverUrl, user, and password".into())));
            };
            let code = p_str(params, "code");
            let use_tor = p_bool(params, "useTor", false);
            let route_key = "rocketchat:login";
            let router = crate::net::route::router();
            router.mark(route_key, use_tor);
            if use_tor || router.tunnel_all() {
                if let Err(e) = router.ready(|_| {}).await {
                    return Some((None, Some(format!("proxy/Tor not ready: {e:#}"))));
                }
            }
            match login::login_password(route_key, url, user, password, code, use_tor).await {
                Ok(login::LoginOutcome::TotpRequired { method }) => (
                    Some(json!({ "totpRequired": true, "method": method })),
                    None,
                ),
                Ok(login::LoginOutcome::SignedIn(mut cfg)) => {
                    if let Some(pass) = p_str(params, "e2eePassphrase") {
                        cfg.e2ee_passphrase = Some(pass.to_string());
                    }
                    match state.accounts.add_rocketchat(cfg) {
                        Ok(saved) => {
                            let id = saved.account_id();
                            super::spawn(state.clone(), saved);
                            state.events.emit("accountsChanged", json!({}));
                            (Some(json!({ "ok": true, "accountId": id })), None)
                        }
                        Err(e) => (None, Some(format!("{e:#}"))),
                    }
                }
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "addRocketChatAccountToken" => {
            let (Some(url), Some(token)) = (p_str(params, "serverUrl"), p_str(params, "authToken").or_else(|| p_str(params, "token"))) else {
                return Some((None, Some("addRocketChatAccountToken requires serverUrl and authToken".into())));
            };
            let user_id = p_str(params, "userId").unwrap_or("");
            let made_here = p_bool(params, "madeHere", false);
            let use_tor = p_bool(params, "useTor", false);
            let route_key = "rocketchat:login";
            let router = crate::net::route::router();
            router.mark(route_key, use_tor);
            if use_tor || router.tunnel_all() {
                if let Err(e) = router.ready(|_| {}).await {
                    return Some((None, Some(format!("proxy/Tor not ready: {e:#}"))));
                }
            }
            match login::login_token(route_key, url, user_id, token, made_here, use_tor).await {
                Ok(mut cfg) => {
                    if let Some(pass) = p_str(params, "e2eePassphrase") {
                        cfg.e2ee_passphrase = Some(pass.to_string());
                    }
                    match state.accounts.add_rocketchat(cfg) {
                        Ok(saved) => {
                            let id = saved.account_id();
                            super::spawn(state.clone(), saved);
                            state.events.emit("accountsChanged", json!({}));
                            (Some(json!({ "ok": true, "accountId": id })), None)
                        }
                        Err(e) => (None, Some(format!("{e:#}"))),
                    }
                }
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Round 1 / 2: Rooms, Directory, DMs, Teams, Discussions
        "rocketchatDirectory" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(
                rooms::directory(
                    &h,
                    p_str(params, "type").unwrap_or("channels"),
                    p_str(params, "query").unwrap_or(""),
                    p_usize(params, "count", 30),
                )
                .await,
            )
        }

        "rocketchatCreateDm" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            let usernames = p_str_vec(params, "usernames");
            if usernames.is_empty() {
                return Some((None, Some("rocketchatCreateDm requires usernames[]".into())));
            }
            wrap_result(rooms::create_dm(state, &h, &usernames).await)
        }

        "rocketchatJoinChannel" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatJoinChannel requires accountId and roomId".into())));
            };
            wrap_result(rooms::join_channel(state, &h, rid, p_str(params, "joinCode")).await)
        }

        "rocketchatLeaveRoom" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatLeaveRoom requires accountId and roomId".into())));
            };
            wrap_result(rooms::leave_room(&h, rid, p_str(params, "teamId")).await)
        }

        "rocketchatCreateRoom" => {
            let (Some(h), Some(name)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "name"),
            ) else {
                return Some((None, Some("rocketchatCreateRoom requires accountId and name".into())));
            };
            wrap_result(
                rooms::create_channel_or_group(
                    state,
                    &h,
                    name,
                    p_bool(params, "private", false),
                    &p_str_vec(params, "members"),
                    p_bool(params, "readOnly", false),
                    p_bool(params, "encrypted", false),
                )
                .await,
            )
        }

        "rocketchatCreateTeam" => {
            let (Some(h), Some(name)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "name"),
            ) else {
                return Some((None, Some("rocketchatCreateTeam requires accountId and name".into())));
            };
            wrap_result(
                rooms::create_team(state, &h, name, p_bool(params, "private", false), &p_str_vec(params, "members")).await,
            )
        }

        "rocketchatListTeams" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(rooms::list_teams(&h).await)
        }

        "rocketchatCreateDiscussion" => {
            let (Some(h), Some(prid), Some(name)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "parentRoomId"),
                p_str(params, "name"),
            ) else {
                return Some((None, Some("rocketchatCreateDiscussion requires accountId, parentRoomId, name".into())));
            };
            wrap_result(
                rooms::create_discussion(state, &h, prid, name, p_str(params, "reply"), &p_str_vec(params, "users")).await,
            )
        }

        "rocketchatListDiscussions" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatListDiscussions requires accountId and roomId".into())));
            };
            wrap_result(rooms::list_discussions(&h, rid).await)
        }

        "rocketchatRoomInfo" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatRoomInfo requires accountId and roomId".into())));
            };
            wrap_result(rooms::refresh_room_info(state, &h, rid).await)
        }

        // Round 3: Threads, Pins, Stars, Search, Files, Read Receipts
        "rocketchatThreadMessages" => {
            let (Some(h), Some(tmid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "tmid"),
            ) else {
                return Some((None, Some("rocketchatThreadMessages requires accountId and tmid".into())));
            };
            wrap_result(threads::get_thread_messages(&h, tmid, p_usize(params, "count", 50)).await)
        }

        "rocketchatThreadsList" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatThreadsList requires accountId and roomId".into())));
            };
            wrap_result(threads::get_threads_list(&h, rid, p_usize(params, "count", 50)).await)
        }

        "rocketchatFollowThread" => {
            let (Some(h), Some(mid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "messageId"),
            ) else {
                return Some((None, Some("rocketchatFollowThread requires accountId and messageId".into())));
            };
            wrap_result(threads::set_following(&h, mid, p_bool(params, "follow", true)).await)
        }

        "rocketchatPinMessage" => {
            let (Some(h), Some(mid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "messageId"),
            ) else {
                return Some((None, Some("rocketchatPinMessage requires accountId and messageId".into())));
            };
            wrap_result(send::set_pinned(&h, mid, p_bool(params, "pin", true)).await)
        }

        "rocketchatStarMessage" => {
            let (Some(h), Some(mid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "messageId"),
            ) else {
                return Some((None, Some("rocketchatStarMessage requires accountId and messageId".into())));
            };
            wrap_result(send::set_starred(&h, mid, p_bool(params, "star", true)).await)
        }

        "rocketchatSearchRoom" => {
            let (Some(h), Some(buf_id), Some(rid), Some(q)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "bufferId"),
                p_str(params, "roomId"),
                p_str(params, "query"),
            ) else {
                return Some((None, Some("rocketchatSearchRoom requires accountId, bufferId, roomId, query".into())));
            };
            wrap_result(search::search_room(state, &h, buf_id, rid, q, p_usize(params, "count", 30)).await)
        }

        "rocketchatPinnedMessages" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatPinnedMessages requires accountId and roomId".into())));
            };
            wrap_result(search::pinned_messages(&h, rid, p_usize(params, "count", 50)).await)
        }

        "rocketchatStarredMessages" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatStarredMessages requires accountId and roomId".into())));
            };
            wrap_result(search::starred_messages(&h, rid, p_usize(params, "count", 50)).await)
        }

        "rocketchatRoomFiles" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatRoomFiles requires accountId and roomId".into())));
            };
            wrap_result(search::room_files(&h, rid, p_usize(params, "count", 50)).await)
        }

        "rocketchatReadReceipts" => {
            let (Some(h), Some(mid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "messageId"),
            ) else {
                return Some((None, Some("rocketchatReadReceipts requires accountId and messageId".into())));
            };
            wrap_result(search::read_receipts(&h, mid).await)
        }

        "rocketchatUploadFile" => {
            let (Some(h), Some(rid), Some(file_path)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "filePath"),
            ) else {
                return Some((None, Some("rocketchatUploadFile requires accountId, roomId, filePath".into())));
            };
            let path = std::path::Path::new(file_path);
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let mime = p_str(params, "mimeType").unwrap_or("application/octet-stream");
            match tokio::fs::read(path).await {
                Ok(bytes) => wrap_result(
                    media::upload_file(&h, rid, filename, mime, bytes, p_str(params, "description"), p_str(params, "tmid")).await,
                ),
                Err(e) => (None, Some(format!("reading file: {e}"))),
            }
        }

        // Round 4: Presence, Profiles, Preferences, Favorites, Hide, Ignores, Mark Unread
        "rocketchatUserInfo" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(profile::user_info(&h, p_str(params, "userId"), p_str(params, "username")).await)
        }

        "rocketchatUpdateProfile" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(
                profile::update_own_profile(
                    &h,
                    p_str(params, "name"),
                    p_str(params, "username"),
                    p_str(params, "email"),
                    p_str(params, "statusText"),
                    p_str(params, "bio"),
                    p_str(params, "nickname"),
                )
                .await,
            )
        }

        "rocketchatSetAvatar" => {
            let (Some(h), Some(url)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "avatarUrl"),
            ) else {
                return Some((None, Some("rocketchatSetAvatar requires accountId and avatarUrl".into())));
            };
            wrap_result(profile::set_avatar_url(&h, url).await)
        }

        "rocketchatFavoriteRoom" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatFavoriteRoom requires accountId and roomId".into())));
            };
            wrap_result(rooms::set_favorite(&h, rid, p_bool(params, "favorite", true)).await)
        }

        "rocketchatHideRoom" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatHideRoom requires accountId and roomId".into())));
            };
            wrap_result(rooms::hide_room(&h, rid).await)
        }

        "rocketchatSaveNotification" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatSaveNotification requires accountId and roomId".into())));
            };
            wrap_result(
                profile::save_room_notification(&h, rid, params.get("notifications").cloned().unwrap_or(json!({}))).await,
            )
        }

        "rocketchatGetPreferences" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(profile::get_preferences(&h).await)
        }

        "rocketchatSetPreferences" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(profile::set_preferences(&h, params.get("data").cloned().unwrap_or(json!({}))).await)
        }

        "rocketchatIgnoreUser" => {
            let (Some(h), Some(rid), Some(uid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "userId"),
            ) else {
                return Some((None, Some("rocketchatIgnoreUser requires accountId, roomId, userId".into())));
            };
            wrap_result(members::ignore_user(&h, rid, uid, p_bool(params, "ignore", true)).await)
        }

        "rocketchatMarkUnread" => {
            let Some(buf_id) = p_str(params, "bufferId") else {
                return Some((None, Some("rocketchatMarkUnread requires bufferId".into())));
            };
            let Some(buf) = state.runtime.get_buffer(buf_id) else {
                return Some((None, Some("no such buffer".into())));
            };
            let Some(h) = state.runtime.rocketchat_handle(&buf.account_id) else {
                return Some((None, Some("account not connected".into())));
            };
            match send::mark_unread(state, &h, &buf, p_str(params, "messageId")).await {
                Ok(()) => (Some(json!({ "ok": true })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Round 5: E2EE & Security
        "rocketchatE2eeUnlock" => {
            let (Some(acc_id), Some(pass)) = (p_str(params, "accountId"), p_str(params, "passphrase")) else {
                return Some((None, Some("rocketchatE2eeUnlock requires accountId and passphrase".into())));
            };
            let Some(h) = state.runtime.rocketchat_handle(acc_id) else {
                return Some((None, Some("account not connected".into())));
            };
            match e2ee::unlock_with_passphrase(&h, pass).await {
                Ok(v) => {
                    if p_bool(params, "remember", true) {
                        let _ = state.accounts.mutate_rocketchat(acc_id, |a| {
                            a.e2ee_passphrase = Some(pass.to_string());
                        });
                    }
                    (Some(v), None)
                }
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "rocketchatE2eeReset" => {
            let Some(acc_id) = p_str(params, "accountId") else {
                return Some((None, Some("rocketchatE2eeReset requires accountId".into())));
            };
            let Some(h) = state.runtime.rocketchat_handle(acc_id) else {
                return Some((None, Some("account not connected".into())));
            };
            let res = e2ee::reset_own_key(&h).await;
            if res.is_ok() {
                let _ = state.accounts.mutate_rocketchat(acc_id, |a| {
                    a.e2ee_passphrase = None;
                });
            }
            wrap_result(res)
        }

        "rocketchatE2eeDistributeKeys" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatE2eeDistributeKeys requires accountId and roomId".into())));
            };
            match e2ee::distribute_room_keys(&h, rid).await {
                Ok(n) => (Some(json!({ "distributed": n })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "rocketchatLogoutOtherClients" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            match login::logout_other_clients(&h.api).await {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Round 6: Video Conferences & WebRTC Signaling
        "rocketchatVideoConfCapabilities" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(calls::capabilities(&h).await)
        }

        "rocketchatVideoConfStart" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatVideoConfStart requires accountId and roomId".into())));
            };
            wrap_result(calls::start(&h, rid, p_str(params, "title"), p_bool(params, "allowRinging", true)).await)
        }

        "rocketchatVideoConfJoin" => {
            let (Some(h), Some(cid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "callId"),
            ) else {
                return Some((None, Some("rocketchatVideoConfJoin requires accountId and callId".into())));
            };
            wrap_result(calls::join(&h, cid, p_bool(params, "mic", true), p_bool(params, "cam", false)).await)
        }

        "rocketchatVideoConfCancel" => {
            let (Some(h), Some(cid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "callId"),
            ) else {
                return Some((None, Some("rocketchatVideoConfCancel requires accountId and callId".into())));
            };
            wrap_result(calls::cancel(&h, cid).await)
        }

        "rocketchatVideoConfInfo" => {
            let (Some(h), Some(cid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "callId"),
            ) else {
                return Some((None, Some("rocketchatVideoConfInfo requires accountId and callId".into())));
            };
            wrap_result(calls::info(&h, cid).await)
        }

        "rocketchatWebrtcSignal" => {
            let (Some(h), Some(target), Some(sig_type)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "targetUserId"),
                p_str(params, "signalType"),
            ) else {
                return Some((None, Some("rocketchatWebrtcSignal requires accountId, targetUserId, signalType".into())));
            };
            match calls::send_webrtc_signal(&h, target, sig_type, params.get("payload").cloned().unwrap_or(json!({}))) {
                Ok(()) => (Some(json!({ "ok": true })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Round 7: Moderation, Roles & Room Administration
        "rocketchatSaveRoomSettings" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatSaveRoomSettings requires accountId and roomId".into())));
            };
            wrap_result(rooms::save_room_settings(&h, rid, params.get("settings").cloned().unwrap_or(json!({}))).await)
        }

        "rocketchatArchiveRoom" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatArchiveRoom requires accountId and roomId".into())));
            };
            wrap_result(rooms::set_archived(&h, rid, p_bool(params, "archived", true)).await)
        }

        "rocketchatDeleteRoom" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatDeleteRoom requires accountId and roomId".into())));
            };
            wrap_result(rooms::delete_room(&h, rid).await)
        }

        "rocketchatCleanHistory" => {
            let (Some(h), Some(rid), Some(latest), Some(oldest)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "latest"),
                p_str(params, "oldest"),
            ) else {
                return Some((None, Some("rocketchatCleanHistory requires accountId, roomId, latest, oldest".into())));
            };
            wrap_result(
                rooms::clean_history(
                    &h,
                    rid,
                    latest,
                    oldest,
                    p_bool(params, "inclusive", false),
                    p_bool(params, "excludePinned", true),
                    p_bool(params, "filesOnly", false),
                )
                .await,
            )
        }

        "rocketchatRoomMembers" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatRoomMembers requires accountId and roomId".into())));
            };
            wrap_result(members::list_members(&h, rid, p_str(params, "teamId"), p_usize(params, "count", 100)).await)
        }

        "rocketchatInviteMember" => {
            let (Some(h), Some(rid), Some(uid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "userId"),
            ) else {
                return Some((None, Some("rocketchatInviteMember requires accountId, roomId, userId".into())));
            };
            wrap_result(members::invite_member(&h, rid, uid, p_str(params, "teamId")).await)
        }

        "rocketchatKickMember" => {
            let (Some(h), Some(rid), Some(uid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "userId"),
            ) else {
                return Some((None, Some("rocketchatKickMember requires accountId, roomId, userId".into())));
            };
            wrap_result(members::kick_member(&h, rid, uid, p_str(params, "teamId")).await)
        }

        "rocketchatMuteMember" => {
            let (Some(h), Some(rid), Some(uname)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "username"),
            ) else {
                return Some((None, Some("rocketchatMuteMember requires accountId, roomId, username".into())));
            };
            wrap_result(members::set_muted(&h, rid, uname, p_bool(params, "muted", true)).await)
        }

        "rocketchatSetRole" => {
            let (Some(h), Some(rid), Some(uid), Some(role)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "userId"),
                p_str(params, "role"),
            ) else {
                return Some((None, Some("rocketchatSetRole requires accountId, roomId, userId, role".into())));
            };
            wrap_result(members::set_role(&h, rid, uid, role, p_bool(params, "add", true), p_str(params, "teamId")).await)
        }

        "rocketchatCreateInvite" => {
            let (Some(h), Some(rid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
            ) else {
                return Some((None, Some("rocketchatCreateInvite requires accountId and roomId".into())));
            };
            wrap_result(
                rooms::create_invite(&h, rid, p_usize(params, "days", 1) as u32, p_usize(params, "maxUses", 0) as u32)
                    .await,
            )
        }

        "rocketchatListInvites" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(rooms::list_invites(&h).await)
        }

        "rocketchatRemoveInvite" => {
            let (Some(h), Some(inv_id)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "inviteId"),
            ) else {
                return Some((None, Some("rocketchatRemoveInvite requires accountId and inviteId".into())));
            };
            wrap_result(rooms::remove_invite(&h, inv_id).await)
        }

        "rocketchatUseInvite" => {
            let (Some(h), Some(token)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "token"),
            ) else {
                return Some((None, Some("rocketchatUseInvite requires accountId and token".into())));
            };
            wrap_result(rooms::use_invite_token(&h, token).await)
        }

        "rocketchatReportMessage" => {
            let (Some(h), Some(mid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "messageId"),
            ) else {
                return Some((None, Some("rocketchatReportMessage requires accountId and messageId".into())));
            };
            wrap_result(moderation::report_message(&h, mid, p_str(params, "description").unwrap_or("")).await)
        }

        "rocketchatModerationReports" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(moderation::list_reports_by_users(&h, p_usize(params, "count", 50)).await)
        }

        "rocketchatDismissReports" => {
            let (Some(h), Some(uid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "userId"),
            ) else {
                return Some((None, Some("rocketchatDismissReports requires accountId and userId".into())));
            };
            wrap_result(moderation::dismiss_reports(&h, uid).await)
        }

        "rocketchatDeleteReportedMessages" => {
            let (Some(h), Some(uid)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "userId"),
            ) else {
                return Some((None, Some("rocketchatDeleteReportedMessages requires accountId and userId".into())));
            };
            wrap_result(moderation::delete_reported_messages(&h, uid).await)
        }

        // Round 8 & 9: Slash commands, Omnichannel, Account settings
        "rocketchatListCommands" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(extras::list_commands(&h).await)
        }

        "rocketchatRunCommand" => {
            let (Some(h), Some(rid), Some(cmd)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "command"),
            ) else {
                return Some((None, Some("rocketchatRunCommand requires accountId, roomId, command".into())));
            };
            wrap_result(
                extras::run_command(&h, rid, cmd, p_str(params, "params").unwrap_or(""), p_str(params, "tmid")).await,
            )
        }

        "rocketchatPreviewCommand" => {
            let (Some(h), Some(rid), Some(cmd)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "command"),
            ) else {
                return Some((None, Some("rocketchatPreviewCommand requires accountId, roomId, command".into())));
            };
            wrap_result(extras::preview_command(&h, rid, cmd, p_str(params, "params").unwrap_or("")).await)
        }

        "rocketchatLivechatRooms" => {
            let Some(h) = p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)) else {
                return Some((None, Some("account not connected".into())));
            };
            wrap_result(extras::list_livechat_rooms(&h, p_bool(params, "openOnly", true)).await)
        }

        "rocketchatCloseLivechat" => {
            let (Some(h), Some(rid), Some(token)) = (
                p_str(params, "accountId").and_then(|a| state.runtime.rocketchat_handle(a)),
                p_str(params, "roomId"),
                p_str(params, "token"),
            ) else {
                return Some((None, Some("rocketchatCloseLivechat requires accountId, roomId, token".into())));
            };
            wrap_result(extras::close_livechat_room(&h, rid, token, p_str(params, "comment")).await)
        }

        "rocketchatAccountSettings" => {
            let Some(acc_id) = p_str(params, "accountId") else {
                return Some((None, Some("rocketchatAccountSettings requires accountId".into())));
            };
            let Some(cfg) = state.accounts.get_rocketchat(acc_id) else {
                return Some((None, Some("no such Rocket.Chat account".into())));
            };
            let (e2ee_unlocked, server_info, is_livechat_agent) = match state.runtime.rocketchat_handle(acc_id) {
                Some(h) => {
                    let unlocked = h.e2ee.lock().unwrap_or_else(|e| e.into_inner()).unlocked;
                    let agent = h.world.lock().unwrap_or_else(|e| e.into_inner()).is_livechat_agent();
                    (unlocked, Some((*h.server).clone()), agent)
                }
                None => (false, None, false),
            };
            (
                Some(json!({
                    "accountId": acc_id,
                    "serverUrl": cfg.server_url,
                    "username": cfg.username,
                    "userId": cfg.user_id,
                    "defaultStatus": cfg.default_status.unwrap_or_else(|| "online".into()),
                    "omnichannelEnabled": cfg.omnichannel_enabled,
                    "isLivechatAgent": is_livechat_agent,
                    "hasE2eePassphrase": cfg.e2ee_passphrase.as_deref().is_some_and(|s| !s.is_empty()),
                    "e2eeUnlocked": e2ee_unlocked,
                    "useTor": cfg.use_tor,
                    "server": server_info,
                })),
                None,
            )
        }

        "rocketchatUpdateAccountSettings" => {
            let Some(acc_id) = p_str(params, "accountId") else {
                return Some((None, Some("rocketchatUpdateAccountSettings requires accountId".into())));
            };
            let default_status = p_str(params, "defaultStatus").map(str::to_string);
            let omnichannel = params.get("omnichannelEnabled").and_then(|v| v.as_bool());
            match state.accounts.mutate_rocketchat(acc_id, |a| {
                if let Some(st) = default_status.clone() {
                    a.default_status = Some(st);
                }
                if let Some(oc) = omnichannel {
                    a.omnichannel_enabled = oc;
                }
            }) {
                Ok(true) => {
                    if let (Some(st), Some(h)) = (default_status.as_deref(), state.runtime.rocketchat_handle(acc_id)) {
                        let _ = profile::set_status(&h, st, None).await;
                    }
                    state.events.emit("accountsChanged", json!({}));
                    (Some(json!({ "ok": true })), None)
                }
                Ok(false) => (None, Some("no such Rocket.Chat account".into())),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        _ => return None,
    };
    Some(out)
}
