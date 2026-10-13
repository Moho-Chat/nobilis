#!/usr/bin/env python3
"""Makes golden.txt: every message in proto.rs, as Valve's own definitions
encode it.

proto.rs is written by hand, and a wrong field number or type in a hand-written
message is silent: the field just never arrives. This asks the real
definitions (the .proto files extracted from the official client) to encode a
fully populated message of each kind, and the tests in proto.rs decode those
bytes with the hand-written structs and require the very same bytes back. A
field the struct has wrong is dropped or fails to decode, and the bytes differ.

Run it when proto.rs gains a message or Valve changes one:

    python3 make_golden.py /path/to/steam/decomp/protos > golden.txt

It needs `protoc` and the Python `protobuf` package. It also checks that every
field of each struct in proto.rs is filled in here, so adding a field to the
Rust and forgetting it here fails loudly instead of leaving it untested.
"""
import importlib
import os
import re
import subprocess
import sys
import tempfile

PROTO_DIR = sys.argv[1] if len(sys.argv) > 1 else "/1tb/git/steam/decomp/protos"
HERE = os.path.dirname(os.path.abspath(__file__))
FILES = [
    "steammessages_base.proto",
    "steammessages_clientserver_login.proto",
    "steammessages_clientserver_friends.proto",
    "steammessages_friendmessages.steamclient.proto",
    "steammessages_auth.steamclient.proto",
    "steammessages_player.steamclient.proto",
    "steammessages_clientserver_2.proto",
    "steammessages_parental.steamclient.proto",
    "steammessages_chat.steamclient.proto",
    "steammessages_parental_objects.proto",
    # What those import.
    "steammessages_unified_base.steamclient.proto",
    "enums.proto",
]

out = tempfile.mkdtemp()
subprocess.run(["protoc", "-I", PROTO_DIR, f"--python_out={out}", *FILES], check=True, capture_output=True)
sys.path.insert(0, out)

base = importlib.import_module("steammessages_base_pb2")
login = importlib.import_module("steammessages_clientserver_login_pb2")
friends = importlib.import_module("steammessages_clientserver_friends_pb2")
fmsg = importlib.import_module("steammessages_friendmessages.steamclient_pb2")
auth = importlib.import_module("steammessages_auth.steamclient_pb2")
player = importlib.import_module("steammessages_player.steamclient_pb2")
cs2 = importlib.import_module("steammessages_clientserver_2_pb2")
parental = importlib.import_module("steammessages_parental.steamclient_pb2")
parental_objects = importlib.import_module("steammessages_parental_objects_pb2")
chat = importlib.import_module("steammessages_chat.steamclient_pb2")

ID = 76561197960287930  # an account's 64-bit id
ID2 = 76561197960265729

# Rust struct name -> (the real message class, the values to give it).
# Every value is something other than the field's default, or it would not be
# written to the wire and would prove nothing.
CASES = {
    "Header": (base.CMsgProtoBufHeader, dict(
        steamid=ID, client_sessionid=-1234, routing_appid=730, jobid_source=17, jobid_target=18,
        target_job_name="FriendMessagesClient.IncomingMessage#1", eresult=1, error_message="no", realm=1)),
    "Multi": (base.CMsgMulti, dict(size_unzipped=4096, message_body=b"\x1f\x8b\x08\x00zipped")),
    "ClientHello": (login.CMsgClientHello, dict(protocol_version=65581)),
    "ClientHeartBeat": (login.CMsgClientHeartBeat, dict(send_reply=True)),
    "ClientLogon": (login.CMsgClientLogon, dict(
        protocol_version=65581, cell_id=9, client_package_version=1771, client_language="english",
        client_os_type=203, should_remember_password=True, obfuscated_private_ip=dict(v4=0xdeadbeef),
        qos_level=2, machine_id=b"\x01\x02\x03machine", ui_mode=4, chat_mode=2, account_name="someone",
        steamguard_dont_remember_computer=True, machine_name="HOST", client_instance_id=2**40 + 5,
        supports_rate_limit_response=True, access_token="eyJ.refresh.token", gaming_device_type=1)),
    "ClientLogonResponse": (login.CMsgClientLogonResponse, dict(
        eresult=1, heartbeat_seconds=27, rtime32_server_time=1790000000, account_flags=3, cell_id=11,
        eresult_extended=5, vanity_url="someone", client_supplied_steamid=ID, ip_country_code="GB",
        client_instance_id=2**41 + 7, token_id=2**50 + 9, family_group_id=2**33)),
    "ClientLogOff": (login.CMsgClientLogOff, dict()),
    "ClientLoggedOff": (login.CMsgClientLoggedOff, dict(eresult=6)),
    "ClientAccountInfo": (login.CMsgClientAccountInfo, dict(
        persona_name="Some One", ip_country="GB", count_authed_computers=3, account_flags=8, two_factor_state=1)),
    "FriendsListEntry": (friends.CMsgClientFriendsList.Friend, dict(ulfriendid=ID, efriendrelationship=3)),
    "ClientFriendsList": (friends.CMsgClientFriendsList, dict(
        bincremental=True, friends=[dict(ulfriendid=ID, efriendrelationship=3), dict(ulfriendid=ID2, efriendrelationship=2)],
        max_friend_count=250, active_friend_count=2, friends_limit_hit=True)),
    "KeyValue": (friends.CMsgClientPersonaState.Friend.KV, dict(key="status", value="In a match")),
    "PersonaFriend": (friends.CMsgClientPersonaState.Friend, dict(
        friendid=ID, persona_state=3, game_played_app_id=730, persona_state_flags=512, persona_set_by_user=True,
        player_name="Some One", avatar_hash=bytes(range(20)), last_logoff=1700000000, last_logon=1700000100,
        last_seen_online=1700000200, game_name="Counter-Strike 2", gameid=730,
        rich_presence=[dict(key="steam_display", value="#Playing"), dict(key="status", value="Dust II")],
        on_steam_deck=True, gaming_device_type=2)),
    "ClientPersonaState": (friends.CMsgClientPersonaState, dict(
        status_flags=1106, friends=[dict(friendid=ID, persona_state=1, player_name="A"), dict(friendid=ID2, persona_state=7, player_name="B")])),
    "ClientRequestFriendData": (friends.CMsgClientRequestFriendData, dict(persona_state_requested=1106, friends=[ID, ID2])),
    "ClientChangeStatus": (friends.CMsgClientChangeStatus, dict(
        persona_state=7, player_name="Some One", is_auto_generated_name=True, high_priority=True,
        persona_set_by_user=True, persona_state_flags=1, need_persona_response=True, is_client_idle=True)),
    "GetCommunityPreferencesRequest": (player.CPlayer_GetCommunityPreferences_Request, dict()),
    "CommunityPreferences": (player.CPlayer_CommunityPreferences, dict(
        timestamp_updated=1790000000, parenthesize_nicknames=True, text_filter_setting=2, text_filter_ignore_friends=False, text_filter_words_revision=7)),
    "GetCommunityPreferencesResponse": (player.CPlayer_GetCommunityPreferences_Response, dict(preferences=dict(text_filter_setting=1, text_filter_ignore_friends=True))),
    "GetTextFilterWordsRequest": (player.CPlayer_GetTextFilterWords_Request, dict()),
    "TextFilterWords": (player.CPlayer_TextFilterWords, dict(text_filter_custom_banned_words=["a", "b"], text_filter_custom_clean_words=["c"], text_filter_words_revision=3)),
    "GetTextFilterWordsResponse": (player.CPlayer_GetTextFilterWords_Response, dict(words=dict(text_filter_custom_banned_words=["x"]))),
    "GetPrivacySettingsRequest": (player.CPlayer_GetPrivacySettings_Request, dict()),
    "PrivacySettings": (player.CPrivacySettings, dict(
        privacy_state=3, privacy_state_inventory=2, privacy_state_gifts=1, privacy_state_ownedgames=3, privacy_state_playtime=2, privacy_state_friendslist=1)),
    "GetPrivacySettingsResponse": (player.CPlayer_GetPrivacySettings_Response, dict(privacy_settings=dict(privacy_state=2))),
    "GetParentalSettingsRequest": (parental.CParental_GetParentalSettings_Request, dict(steamid=ID)),
    "ParentalSettings": (parental_objects.ParentalSettings, dict(steamid=ID, is_enabled=True, enabled_features=15, temporary_enabled_features=3)),
    "GetParentalSettingsResponse": (parental.CParental_GetParentalSettings_Response, dict(settings=dict(is_enabled=True, enabled_features=8))),
    "EnumerateTokensRequest": (auth.CAuthentication_RefreshToken_Enumerate_Request, dict(include_revoked=True)),
    "TokenUsageEvent": (auth.CAuthentication_RefreshToken_Enumerate_Response.TokenUsageEvent, dict(time=1790000001, country="GB", state="ENG", city="London")),
    "RefreshTokenDescription": (auth.CAuthentication_RefreshToken_Enumerate_Response.RefreshTokenDescription, dict(
        token_id=2**40 + 3, token_description="moho on host", time_updated=1790000002, platform_type=3, logged_in=True,
        first_seen=dict(time=1, country="GB"), last_seen=dict(time=2, city="London"))),
    "EnumerateTokensResponse": (auth.CAuthentication_RefreshToken_Enumerate_Response, dict(
        refresh_tokens=[dict(token_id=5, token_description="phone", platform_type=3)], requesting_token=5)),
    "RevokeRefreshTokenRequest": (auth.CAuthentication_RefreshToken_Revoke_Request, dict(token_id=2**40 + 3, steamid=ID)),
    "RevokeRefreshTokenResponse": (auth.CAuthentication_RefreshToken_Revoke_Response, dict()),
    "ChatRoomState": (chat.CChatRoomState, dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, members_in_voice=[7, 9], time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)),
    "ChatRoomMember": (chat.CChatRoomMember, dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])),
    "ChatRole": (chat.CChatRole, dict(role_id=8589934593, name='s2', ordinal=10)),
    "ChatRoomGroupHeaderState": (chat.CChatRoomGroupHeaderState, dict(chat_group_id=8589934593, chat_name='s2', clanid=20, accountid_owner=21, tagline='s15', avatar_sha=b'\x01\x02\x03', default_role_id=8589934609, roles=[dict(role_id=8589934593, name='s2', ordinal=10)], role_actions=[dict(role_id=8589934593, can_chat=True)], avatar_ugc_url='s25', disabled=True)),
    "ChatRoomGroupState": (chat.CChatRoomGroupState, dict(header_state=dict(chat_group_id=8589934593, chat_name='s2', clanid=20, accountid_owner=21, tagline='s15', avatar_sha=b'\x01\x02\x03', default_role_id=8589934609, roles=[dict(role_id=8589934593, name='s2', ordinal=10)], role_actions=[dict(role_id=8589934593, can_chat=True)], avatar_ugc_url='s25', disabled=True), members=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])], default_chat_id=8589934596, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], kicked=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])])),
    "UserChatRoomState": (chat.CUserChatRoomState, dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)),
    "UserChatRoomGroupState": (chat.CUserChatRoomGroupState, dict(chat_group_id=8589934593, time_joined=9, user_chat_room_state=[dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)], desktop_notification_level=3, mobile_notification_level=2, time_last_group_ack=13, unread_indicator_muted=True, direct_messages_allowed=True)),
    "ChatGroupSummary": (chat.CChatRoom_GetChatRoomGroupSummary_Response, dict(chat_group_id=8589934593, chat_group_name='s2', active_member_count=10, active_voice_member_count=11, default_chat_id=8589934597, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], clanid=14, chat_group_tagline='s8', accountid_owner=16, top_members=[17, 19], chat_group_avatar_sha=b'\x01\x02\x03', rank=20, default_role_id=8589934605, role_ids=[8589934606, 8589934607], appid=24, avatar_ugc_url='s21', disabled=True)),
    "ChatRoomSummaryPair": (chat.CChatRoomSummaryPair, dict(user_chat_group_state=dict(chat_group_id=8589934593, time_joined=9, user_chat_room_state=[dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)], desktop_notification_level=3, mobile_notification_level=2, time_last_group_ack=13, unread_indicator_muted=True, direct_messages_allowed=True), group_summary=dict(chat_group_id=8589934593, chat_group_name='s2', active_member_count=10, active_voice_member_count=11, default_chat_id=8589934597, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], clanid=14, chat_group_tagline='s8', accountid_owner=16, top_members=[17, 19], chat_group_avatar_sha=b'\x01\x02\x03', rank=20, default_role_id=8589934605, role_ids=[8589934606, 8589934607], appid=24, avatar_ugc_url='s21', disabled=True))),
    "GetMyChatRoomGroupsRequest": (chat.CChatRoom_GetMyChatRoomGroups_Request, dict()),
    "GetMyChatRoomGroupsResponse": (chat.CChatRoom_GetMyChatRoomGroups_Response, dict(chat_room_groups=[dict(user_chat_group_state=dict(chat_group_id=8589934593, time_joined=9, user_chat_room_state=[dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)], desktop_notification_level=3, mobile_notification_level=2, time_last_group_ack=13, unread_indicator_muted=True, direct_messages_allowed=True), group_summary=dict(chat_group_id=8589934593, chat_group_name='s2', active_member_count=10, active_voice_member_count=11, default_chat_id=8589934597, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], clanid=14, chat_group_tagline='s8', accountid_owner=16, top_members=[17, 19], chat_group_avatar_sha=b'\x01\x02\x03', rank=20, default_role_id=8589934605, role_ids=[8589934606, 8589934607], appid=24, avatar_ugc_url='s21', disabled=True))])),
    "GetChatRoomGroupStateRequest": (chat.CChatRoom_GetChatRoomGroupState_Request, dict(chat_group_id=8589934593)),
    "GetChatRoomGroupStateResponse": (chat.CChatRoom_GetChatRoomGroupState_Response, dict(state=dict(header_state=dict(chat_group_id=8589934593, chat_name='s2', clanid=20, accountid_owner=21, tagline='s15', avatar_sha=b'\x01\x02\x03', default_role_id=8589934609, roles=[dict(role_id=8589934593, name='s2', ordinal=10)], role_actions=[dict(role_id=8589934593, can_chat=True)], avatar_ugc_url='s25', disabled=True), members=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])], default_chat_id=8589934596, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], kicked=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])]))),
    "SetSessionActiveGroupsRequest": (chat.CChatRoom_SetSessionActiveChatRoomGroups_Request, dict(chat_group_ids=[8589934593, 8589934594], chat_groups_data_requested=[8589934594, 8589934595], virtualize_members_threshold=6)),
    "SetSessionActiveGroupsResponse": (chat.CChatRoom_SetSessionActiveChatRoomGroups_Response, dict(chat_group_notices=[dict(chat_group_id=8589934593, member_notices=[dict(accountid=7, notice_type=1)])], chat_states=[dict(header_state=dict(chat_group_id=8589934593, chat_name='s2', clanid=20, accountid_owner=21, tagline='s15', avatar_sha=b'\x01\x02\x03', default_role_id=8589934609, roles=[dict(role_id=8589934593, name='s2', ordinal=10)], role_actions=[dict(role_id=8589934593, can_chat=True)], avatar_ugc_url='s25', disabled=True), members=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])], default_chat_id=8589934596, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], kicked=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])])], virtualize_members_chat_group_ids=[8589934594, 8589934595])),
    "ChatSendRequest": (chat.CChatRoom_SendChatMessage_Request, dict(chat_group_id=8589934593, chat_id=8589934594, message='text', echo_to_sender=True)),
    "ChatSendResponse": (chat.CChatRoom_SendChatMessage_Response, dict(modified_message='s1', server_timestamp=9, ordinal=10, message_without_bb_code='s4')),
    "ChatHistoryRequest": (chat.CChatRoom_GetMessageHistory_Request, dict(chat_group_id=8589934593, chat_id=8589934594, last_time=10, last_ordinal=11, start_time=12, start_ordinal=13, max_count=14)),
    "ServerMessage": (chat.ServerMessage, dict(message=2, string_param='s2', accountid_param=10)),
    "ChatMessageReaction": (chat.CChatRoom_GetMessageHistory_Response.ChatMessage.MessageReaction, dict(reaction_type=1, reaction='s2', num_reactors=10, has_user_reacted=True)),
    "ChatHistoryMessage": (chat.CChatRoom_GetMessageHistory_Response.ChatMessage, dict(sender=8, server_timestamp=9, message='text', ordinal=11, server_message=dict(message=2, string_param='s2', accountid_param=10), deleted=True, reactions=[dict(reaction_type=1, reaction='s2', num_reactors=10, has_user_reacted=True)])),
    "ChatHistoryResponse": (chat.CChatRoom_GetMessageHistory_Response, dict(messages=[dict(sender=8, server_timestamp=9, message='text', ordinal=11, server_message=dict(message=2, string_param='s2', accountid_param=10), deleted=True, reactions=[dict(reaction_type=1, reaction='s2', num_reactors=10, has_user_reacted=True)])], more_available=True)),
    "ChatMentions": (chat.CChatMentions, dict(mention_all=True, mention_here=True, mention_accountids=[10, 12])),
    "ChatIncoming": (chat.CChatRoom_IncomingChatMessage_Notification, dict(chat_group_id=8589934593, chat_id=8589934594, steamid_sender=ID, message='text', timestamp=12, mentions=dict(mention_all=True, mention_here=True, mention_accountids=[10, 12]), ordinal=14, server_message=dict(message=2, string_param='s2', accountid_param=10), message_no_bbcode='s9', chat_name='s10', notification_key='s11')),
    "ChatAck": (chat.CChatRoom_AckChatMessage_Notification, dict(chat_group_id=8589934593, chat_id=8589934594, timestamp=10)),
    "ChatModifiedMessage": (chat.CChatRoom_ChatMessageModified_Notification.ChatMessage, dict(server_timestamp=8, ordinal=9, deleted=True)),
    "ChatModified": (chat.CChatRoom_ChatMessageModified_Notification, dict(chat_group_id=8589934593, chat_id=8589934594, messages=[dict(server_timestamp=8, ordinal=9, deleted=True)])),
    "ChatMemberChange": (chat.CChatRoom_MemberStateChange_Notification, dict(chat_group_id=8589934593, member=dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600]), change=1)),
    "ChatHeaderChange": (chat.CChatRoom_ChatRoomHeaderState_Notification, dict(header_state=dict(chat_group_id=8589934593, chat_name='s2', clanid=20, accountid_owner=21, tagline='s15', avatar_sha=b'\x01\x02\x03', default_role_id=8589934609, roles=[dict(role_id=8589934593, name='s2', ordinal=10)], role_actions=[dict(role_id=8589934593, can_chat=True)], avatar_ugc_url='s25', disabled=True))),
    "ChatRoomsChange": (chat.CChatRoom_ChatRoomGroupRoomsChange_Notification, dict(chat_group_id=8589934593, default_chat_id=8589934594, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)])),
    "ChatUserStateChange": (chat.ChatRoomClient_NotifyChatGroupUserStateChanged_Notification, dict(chat_group_id=8589934593, user_chat_group_state=dict(chat_group_id=8589934593, time_joined=9, user_chat_room_state=[dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)], desktop_notification_level=3, mobile_notification_level=2, time_last_group_ack=13, unread_indicator_muted=True, direct_messages_allowed=True), group_summary=dict(chat_group_id=8589934593, chat_group_name='s2', active_member_count=10, active_voice_member_count=11, default_chat_id=8589934597, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], clanid=14, chat_group_tagline='s8', accountid_owner=16, top_members=[17, 19], chat_group_avatar_sha=b'\x01\x02\x03', rank=20, default_role_id=8589934605, role_ids=[8589934606, 8589934607], appid=24, avatar_ugc_url='s21', disabled=True), user_action=2)),
    "CreateChatGroupRequest": (chat.CChatRoom_CreateChatRoomGroup_Request, dict(steamid_partner=ID, steamid_invited=ID, name='s3', steamid_invitees=[ID, ID2])),
    "CreateChatGroupResponse": (chat.CChatRoom_CreateChatRoomGroup_Response, dict(chat_group_id=8589934593, state=dict(header_state=dict(chat_group_id=8589934593, chat_name='s2', clanid=20, accountid_owner=21, tagline='s15', avatar_sha=b'\x01\x02\x03', default_role_id=8589934609, roles=[dict(role_id=8589934593, name='s2', ordinal=10)], role_actions=[dict(role_id=8589934593, can_chat=True)], avatar_ugc_url='s25', disabled=True), members=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])], default_chat_id=8589934596, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], kicked=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])]), user_chat_state=dict(chat_group_id=8589934593, time_joined=9, user_chat_room_state=[dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)], desktop_notification_level=3, mobile_notification_level=2, time_last_group_ack=13, unread_indicator_muted=True, direct_messages_allowed=True))),
    "JoinChatGroupRequest": (chat.CChatRoom_JoinChatRoomGroup_Request, dict(chat_group_id=8589934593, invite_code='s2', chat_id=8589934595)),
    "JoinChatGroupResponse": (chat.CChatRoom_JoinChatRoomGroup_Response, dict(state=dict(header_state=dict(chat_group_id=8589934593, chat_name='s2', clanid=20, accountid_owner=21, tagline='s15', avatar_sha=b'\x01\x02\x03', default_role_id=8589934609, roles=[dict(role_id=8589934593, name='s2', ordinal=10)], role_actions=[dict(role_id=8589934593, can_chat=True)], avatar_ugc_url='s25', disabled=True), members=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])], default_chat_id=8589934596, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], kicked=[dict(accountid=8, state=2, rank=20, time_kick_expire=13, role_ids=[8589934599, 8589934600])]), user_chat_state=dict(chat_group_id=8589934593, time_joined=9, user_chat_room_state=[dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)], desktop_notification_level=3, mobile_notification_level=2, time_last_group_ack=13, unread_indicator_muted=True, direct_messages_allowed=True), join_chat_id=8589934596, time_expire=12)),
    "LeaveChatGroupRequest": (chat.CChatRoom_LeaveChatRoomGroup_Request, dict(chat_group_id=8589934593)),
    "LeaveChatGroupResponse": (chat.CChatRoom_LeaveChatRoomGroup_Response, dict()),
    "CreateChatRoomRequest": (chat.CChatRoom_CreateChatRoom_Request, dict(chat_group_id=8589934593, name='s2', allow_voice=True)),
    "CreateChatRoomResponse": (chat.CChatRoom_CreateChatRoom_Response, dict(chat_room=dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15))),
    "DeleteChatRoomRequest": (chat.CChatRoom_DeleteChatRoom_Request, dict(chat_group_id=8589934593, chat_id=8589934594)),
    "DeleteChatRoomResponse": (chat.CChatRoom_DeleteChatRoom_Response, dict()),
    "RenameChatRoomRequest": (chat.CChatRoom_RenameChatRoom_Request, dict(chat_group_id=8589934593, chat_id=8589934594, name='s3')),
    "RenameChatRoomResponse": (chat.CChatRoom_RenameChatRoom_Response, dict()),
    "RenameChatGroupRequest": (chat.CChatRoom_RenameChatRoomGroup_Request, dict(chat_group_id=8589934593, name='s2')),
    "RenameChatGroupResponse": (chat.CChatRoom_RenameChatRoomGroup_Response, dict()),
    "CreateInviteLinkRequest": (chat.CChatRoom_CreateInviteLink_Request, dict(chat_group_id=8589934593, seconds_valid=9, chat_id=8589934595)),
    "CreateInviteLinkResponse": (chat.CChatRoom_CreateInviteLink_Response, dict(invite_code='s1', seconds_valid=9)),
    "GetInviteLinksRequest": (chat.CChatRoom_GetInviteLinksForGroup_Request, dict(chat_group_id=8589934593)),
    "InviteLinkInfo": (chat.CChatRoom_GetInviteLinksForGroup_Response.LinkInfo, dict(invite_code='s1', steamid_creator=ID, time_expires=10, chat_id=8589934596)),
    "GetInviteLinksResponse": (chat.CChatRoom_GetInviteLinksForGroup_Response, dict(invite_links=[dict(invite_code='s1', steamid_creator=ID, time_expires=10, chat_id=8589934596)])),
    "GetInviteLinkInfoRequest": (chat.CChatRoom_GetInviteLinkInfo_Request, dict(invite_code='s1')),
    "GetInviteLinkInfoResponse": (chat.CChatRoom_GetInviteLinkInfo_Response, dict(steamid_sender=ID, time_expires=11, chat_id=8589934598, group_summary=dict(chat_group_id=8589934593, chat_group_name='s2', active_member_count=10, active_voice_member_count=11, default_chat_id=8589934597, chat_rooms=[dict(chat_id=8589934593, chat_name='s2', voice_allowed=True, time_last_message=12, sort_order=13, last_message='s7', accountid_last_message=15)], clanid=14, chat_group_tagline='s8', accountid_owner=16, top_members=[17, 19], chat_group_avatar_sha=b'\x01\x02\x03', rank=20, default_role_id=8589934605, role_ids=[8589934606, 8589934607], appid=24, avatar_ugc_url='s21', disabled=True), user_chat_group_state=dict(chat_group_id=8589934593, time_joined=9, user_chat_room_state=[dict(chat_id=8589934593, time_joined=9, time_last_ack=10, desktop_notification_level=3, mobile_notification_level=2, time_last_mention=13, unread_indicator_muted=True, time_first_unread=15)], desktop_notification_level=3, mobile_notification_level=2, time_last_group_ack=13, unread_indicator_muted=True, direct_messages_allowed=True), time_kick_expire=17, banned=True)),
    "DeleteInviteLinkRequest": (chat.CChatRoom_DeleteInviteLink_Request, dict(chat_group_id=8589934593, invite_code='s2')),
    "DeleteInviteLinkResponse": (chat.CChatRoom_DeleteInviteLink_Response, dict()),
    "KickChatUserRequest": (chat.CChatRoom_KickUser_Request, dict(chat_group_id=8589934593, steamid=ID, expiration=6)),
    "KickChatUserResponse": (chat.CChatRoom_KickUser_Response, dict()),
    "BanChatUserRequest": (chat.CChatRoom_SetUserBanState_Request, dict(chat_group_id=8589934593, steamid=ID, ban_state=True)),
    "BanChatUserResponse": (chat.CChatRoom_SetUserBanState_Response, dict()),
    "MuteChatUserRequest": (chat.CChatRoom_MuteUser_Request, dict(chat_group_id=8589934593, steamid=ID, expiration=6)),
    "MuteChatUserResponse": (chat.CChatRoom_MuteUser_Response, dict()),
    "DeleteChatMessage": (chat.CChatRoom_DeleteChatMessages_Request.Message, dict(server_timestamp=8, ordinal=9)),
    "DeleteChatMessagesRequest": (chat.CChatRoom_DeleteChatMessages_Request, dict(chat_group_id=8589934593, chat_id=8589934594, messages=[dict(server_timestamp=8, ordinal=9)])),
    "DeleteChatMessagesResponse": (chat.CChatRoom_DeleteChatMessages_Response, dict()),
    "ChatReactionRequest": (chat.CChatRoom_UpdateMessageReaction_Request, dict(chat_group_id=8589934593, chat_id=8589934594, server_timestamp=10, ordinal=11, reaction_type=1, reaction='s6', is_add=True)),
    "ChatReactionResponse": (chat.CChatRoom_UpdateMessageReaction_Response, dict(num_reactors=10)),
    "ChatReactionNotice": (chat.CChatRoom_MessageReaction_Notification, dict(chat_group_id=8589934593, chat_id=8589934594, server_timestamp=10, ordinal=11, reactor=ID, reaction_type=1, reaction='s7', is_add=True)),
    "InviteFriendToChatGroupRequest": (chat.CChatRoom_InviteFriendToChatRoomGroup_Request, dict(chat_group_id=8589934593, steamid=ID, chat_id=8589934595, skip_friendsui_check=True)),
    "InviteFriendToChatGroupResponse": (chat.CChatRoom_InviteFriendToChatRoomGroup_Response, dict()),
    "ChatRoleActions": (chat.CChatRoleActions, dict(role_id=8589934593, can_create_rename_delete_channel=True, can_kick=True, can_ban=True, can_invite=True, can_change_tagline_avatar_name=True, can_chat=True, can_view_history=True, can_change_group_roles=True, can_change_user_roles=True, can_mention_all=True, can_set_watching_broadcast=True)),
    "SetTaglineRequest": (chat.CChatRoom_SetChatRoomGroupTagline_Request, dict(chat_group_id=8589934593, tagline='s2')),
    "SetTaglineResponse": (chat.CChatRoom_SetChatRoomGroupTagline_Response, dict()),
    "UserChatGroupPreferences": (chat.CChatRoom_SetUserChatGroupPreferences_Request.ChatGroupPreferences, dict(desktop_notification_level=3, mobile_notification_level=2, unread_indicator_muted=True, direct_messages_allowed=True)),
    "SetUserPreferencesRequest": (chat.CChatRoom_SetUserChatGroupPreferences_Request, dict(chat_group_id=8589934593, chat_group_preferences=dict(desktop_notification_level=3, mobile_notification_level=2, unread_indicator_muted=True, direct_messages_allowed=True))),
    "SetUserPreferencesResponse": (chat.CChatRoom_SetUserChatGroupPreferences_Response, dict()),
    "CreateRoleRequest": (chat.CChatRoom_CreateRole_Request, dict(chat_group_id=8589934593, name='s2')),
    "CreateRoleResponse": (chat.CChatRoom_CreateRole_Response, dict(actions=dict(role_id=8589934593, can_kick=True))),
    "RenameRoleRequest": (chat.CChatRoom_RenameRole_Request, dict(chat_group_id=8589934593, role_id=8589934594, name='s3')),
    "RenameRoleResponse": (chat.CChatRoom_RenameRole_Response, dict()),
    "DeleteRoleRequest": (chat.CChatRoom_DeleteRole_Request, dict(chat_group_id=8589934593, role_id=8589934594)),
    "DeleteRoleResponse": (chat.CChatRoom_DeleteRole_Response, dict()),
    "GetRoleActionsRequest": (chat.CChatRoom_GetRoleActions_Request, dict(chat_group_id=8589934593, role_id=8589934594)),
    "GetRoleActionsResponse": (chat.CChatRoom_GetRoleActions_Response, dict(actions=[dict(role_id=8589934593, can_chat=True)])),
    "ReplaceRoleActionsRequest": (chat.CChatRoom_ReplaceRoleActions_Request, dict(chat_group_id=8589934593, role_id=8589934594, actions=dict(role_id=8589934594, can_chat=True, can_ban=True))),
    "ReplaceRoleActionsResponse": (chat.CChatRoom_ReplaceRoleActions_Response, dict()),
    "AddRoleToUserRequest": (chat.CChatRoom_AddRoleToUser_Request, dict(chat_group_id=8589934593, role_id=8589934595, steamid=ID)),
    "AddRoleToUserResponse": (chat.CChatRoom_AddRoleToUser_Response, dict()),
    "DeleteRoleFromUserRequest": (chat.CChatRoom_DeleteRoleFromUser_Request, dict(chat_group_id=8589934593, role_id=8589934595, steamid=ID)),
    "DeleteRoleFromUserResponse": (chat.CChatRoom_DeleteRoleFromUser_Response, dict()),
    "GetBanListRequest": (chat.CChatRoom_GetBanList_Request, dict(chat_group_id=8589934593)),
    "BanInfo": (chat.CChatRoom_GetBanList_Response.BanInfo, dict(accountid=7, accountid_actor=8, time_banned=9, ban_reason='s4')),
    "GetBanListResponse": (chat.CChatRoom_GetBanList_Response, dict(bans=[dict(accountid=7, accountid_actor=8, time_banned=9, ban_reason='s4')])),
    "SetAvatarRequest": (chat.CChatRoom_SetChatRoomGroupAvatar_Request, dict(chat_group_id=8589934593, avatar_sha=b'\x01\x02\x03')),
    "SetAvatarResponse": (chat.CChatRoom_SetChatRoomGroupAvatar_Response, dict()),
    "ChatRoomNoticeNotification": (chat.CChatRoom_ChatRoomNotice_Notification, dict(chat_group_id=8589934593, steamid_sender=ID, notice_type=1, active=True)),
    "DismissChatRoomNotice": (chat.CChatRoom_DismissChatRoomNotice_Notification, dict(chat_group_id=8589934593, steamid_sender=ID, notice_type=1)),
    "ChatRoomMemberNotice": (chat.CChatRoomMemberNotice, dict(accountid=7, notice_type=1)),
    "ChatRoomGroupMemberNotices": (chat.CChatRoomGroupMemberNotices, dict(chat_group_id=8589934593, member_notices=[dict(accountid=7, notice_type=1)])),
    "FriendGroup": (friends.CMsgClientFriendsGroupsList.FriendGroup, dict(nGroupID=7, strGroupName="Raiders")),
    "FriendGroupMembership": (friends.CMsgClientFriendsGroupsList.FriendGroupsMembership, dict(ulSteamID=ID, nGroupID=7)),
    "ClientFriendsGroupsList": (friends.CMsgClientFriendsGroupsList, dict(
        bremoval=True, bincremental=True, friendGroups=[dict(nGroupID=7, strGroupName="Raiders"), dict(nGroupID=8, strGroupName="Work")],
        memberships=[dict(ulSteamID=ID, nGroupID=7), dict(ulSteamID=ID2, nGroupID=8)])),
    "ClientCreateFriendsGroup": (friends.CMsgClientCreateFriendsGroup, dict(steamid=ID, groupname="Raiders", steamid_friends=[ID, ID2])),
    "ClientCreateFriendsGroupResponse": (friends.CMsgClientCreateFriendsGroupResponse, dict(eresult=1, groupid=7)),
    "ClientDeleteFriendsGroup": (friends.CMsgClientDeleteFriendsGroup, dict(steamid=ID, groupid=7)),
    "ClientDeleteFriendsGroupResponse": (friends.CMsgClientDeleteFriendsGroupResponse, dict(eresult=1)),
    "ClientManageFriendsGroup": (friends.CMsgClientManageFriendsGroup, dict(groupid=7, groupname="Raiders", steamid_friends_added=[ID], steamid_friends_removed=[ID2])),
    "ClientManageFriendsGroupResponse": (friends.CMsgClientManageFriendsGroupResponse, dict(eresult=1)),
    "ClientAddFriendToGroup": (friends.CMsgClientAddFriendToGroup, dict(groupid=7, steamiduser=ID)),
    "ClientAddFriendToGroupResponse": (friends.CMsgClientAddFriendToGroupResponse, dict(eresult=1)),
    "ClientRemoveFriendFromGroup": (friends.CMsgClientRemoveFriendFromGroup, dict(groupid=7, steamiduser=ID)),
    "ClientRemoveFriendFromGroupResponse": (friends.CMsgClientRemoveFriendFromGroupResponse, dict(eresult=1)),
    "ClientAddFriend": (friends.CMsgClientAddFriend, dict(steamid_to_add=ID, accountname_or_email_to_add="someone")),
    "ClientAddFriendResponse": (friends.CMsgClientAddFriendResponse, dict(eresult=29, steam_id_added=ID2, persona_name_added="Some One")),
    "ClientRemoveFriend": (friends.CMsgClientRemoveFriend, dict(friendid=ID)),
    "ClientFriendMsg": (friends.CMsgClientFriendMsg, dict(
        steamid=ID, chat_entry_type=2, message=b"typing", rtime32_server_timestamp=1790000001, echo_to_sender=True)),
    "ClientFriendMsgIncoming": (friends.CMsgClientFriendMsgIncoming, dict(
        steamid_from=ID, chat_entry_type=1, from_limited_account=True, message=b"hello", rtime32_server_timestamp=1790000002)),
    "FriendMessagesSendRequest": (fmsg.CFriendMessages_SendMessage_Request, dict(
        steamid=ID, chat_entry_type=1, message="hi [b]there[/b]", contains_bbcode=True, echo_to_sender=True,
        low_priority=True, client_message_id="abc-123")),
    "FriendMessagesSendResponse": (fmsg.CFriendMessages_SendMessage_Response, dict(
        modified_message="hi [b]there[/b]", server_timestamp=1790000003, ordinal=2, message_without_bb_code="hi there")),
    "FriendMessagesIncoming": (fmsg.CFriendMessages_IncomingMessage_Notification, dict(
        steamid_friend=ID, chat_entry_type=1, from_limited_account=True, message="yo", rtime32_server_timestamp=1790000004,
        ordinal=3, local_echo=True, message_no_bbcode="yo", low_priority=True)),
    "GetRecentMessagesRequest": (fmsg.CFriendMessages_GetRecentMessages_Request, dict(
        steamid1=ID, steamid2=ID2, count=50, most_recent_conversation=True, rtime32_start_time=1700000000,
        bbcode_format=True, start_ordinal=3, time_last=1790000000, ordinal_last=4)),
    "PastReaction": (fmsg.CFriendMessages_GetRecentMessages_Response.FriendMessage.MessageReaction, dict(reaction_type=1, reaction=":steamhappy:", reactors=[22202, 100])),
    "PastMessage": (fmsg.CFriendMessages_GetRecentMessages_Response.FriendMessage, dict(
        accountid=22202, timestamp=1790000005, message="from the past", ordinal=2,
        reactions=[dict(reaction_type=1, reaction=":steamhappy:", reactors=[100])])),
    "GetRecentMessagesResponse": (fmsg.CFriendMessages_GetRecentMessages_Response, dict(
        messages=[dict(accountid=22202, timestamp=1790000005, message="a", ordinal=1), dict(accountid=100, timestamp=1790000006, message="b", ordinal=0)],
        more_available=True)),
    "GetActiveSessionsRequest": (fmsg.CFriendsMessages_GetActiveMessageSessions_Request, dict(lastmessage_since=1790000000, only_sessions_with_messages=True)),
    "MessageSession": (fmsg.CFriendsMessages_GetActiveMessageSessions_Response.FriendMessageSession, dict(
        accountid_friend=100, last_message=1790000007, last_view=1790000003, unread_message_count=4, notices=[1])),
    "GetActiveSessionsResponse": (fmsg.CFriendsMessages_GetActiveMessageSessions_Response, dict(
        message_sessions=[dict(accountid_friend=100, last_message=1790000007, last_view=1790000003, unread_message_count=4)],
        timestamp=1790000008)),
    "AckMessage": (fmsg.CFriendMessages_AckMessage_Notification, dict(steamid_partner=ID, timestamp=1790000009)),
    "GetEmoticonListRequest": (player.CPlayer_GetEmoticonList_Request, dict()),
    "EmoticonInfo": (player.CPlayer_GetEmoticonList_Response.Emoticon, dict(
        name="steamhappy", count=3, time_last_used=1790000010, use_count=9, time_received=1700000000, appid=730)),
    "GetEmoticonListResponse": (player.CPlayer_GetEmoticonList_Response, dict(
        emoticons=[dict(name="steamhappy", count=1), dict(name="steamsad", count=2, use_count=4)])),
    "UpdateMessageReactionRequest": (fmsg.CFriendMessages_UpdateMessageReaction_Request, dict(
        steamid=ID, server_timestamp=1790000011, ordinal=2, reaction_type=1, reaction="steamhappy", is_add=True)),
    "UpdateMessageReactionResponse": (fmsg.CFriendMessages_UpdateMessageReaction_Response, dict(reactors=[22202, 100])),
    "MessageReactionNotice": (fmsg.CFriendMessages_MessageReaction_Notification, dict(
        steamid_friend=ID, server_timestamp=1790000012, ordinal=1, reactor=ID2, reaction_type=2, reaction="CatSurprise", is_add=True)),
    "SessionNoticeNotification": (fmsg.CFriendMessages_SessionNotice_Notification, dict(steamid_friend=ID, notice_type=1, active=True)),
    "DismissSessionNotice": (fmsg.CFriendMessages_DismissSessionNotice_Notification, dict(steamid_friend=ID2, notice_type=1)),
    "FriendProfileInfoRequest": (friends.CMsgClientFriendProfileInfo, dict(steamid_friend=ID)),
    "FriendProfileInfoResponse": (friends.CMsgClientFriendProfileInfoResponse, dict(
        eresult=1, steamid_friend=ID2, time_created=1300000000, real_name="Some One", city_name="Bellevue",
        state_name="WA", country_name="United States", headline="Hello", summary="About me")),
    "FriendsSteamLevelsRequest": (cs2.CMsgClientFSGetFriendsSteamLevels, dict(accountids=[22202, 100])),
    "FriendLevel": (cs2.CMsgClientFSGetFriendsSteamLevelsResponse.Friend, dict(accountid=22202, level=42)),
    "FriendsSteamLevelsResponse": (cs2.CMsgClientFSGetFriendsSteamLevelsResponse, dict(friends=[dict(accountid=22202, level=42), dict(accountid=100, level=7)])),
    "GetPerFriendPreferencesRequest": (player.CPlayer_GetPerFriendPreferences_Request, dict()),
    "PerFriendPreferences": (player.PerFriendPreferences, dict(
        accountid=22202, nickname="Buddy", notifications_showingame=1, notifications_showonline=2, notifications_showmessages=2,
        sounds_showingame=1, sounds_showonline=2, sounds_showmessages=1, notifications_sendmobile=2)),
    "GetPerFriendPreferencesResponse": (player.CPlayer_GetPerFriendPreferences_Response, dict(preferences=[dict(accountid=22202, nickname="Buddy", notifications_showmessages=2)])),
    "SetPerFriendPreferencesRequest": (player.CPlayer_SetPerFriendPreferences_Request, dict(preferences=dict(accountid=22202, nickname="Pal"))),
    "SetPerFriendPreferencesResponse": (player.CPlayer_SetPerFriendPreferences_Response, dict()),
    "PerFriendPreferencesChanged": (player.CPlayer_PerFriendPreferencesChanged_Notification, dict(accountid=22202, preferences=dict(accountid=22202, nickname="Pal", notifications_showmessages=1))),
    "FriendNicknameChanged": (player.CPlayer_FriendNicknameChanged_Notification, dict(accountid=22202, nickname="Pal", is_echo_to_self=True)),
    "ClientGetEmoticonList": (friends.CMsgClientGetEmoticonList, dict()),
    "EmoticonOwned": (friends.CMsgClientEmoticonList.Emoticon, dict(name="steamhappy", count=3, time_last_used=1790000010, use_count=9, time_received=1700000000, appid=730)),
    "StickerOwned": (friends.CMsgClientEmoticonList.Sticker, dict(name="CatSurprise", count=2, time_received=1700000001, appid=440, time_last_used=1790000011, use_count=5)),
    "EffectOwned": (friends.CMsgClientEmoticonList.Effect, dict(name="Fireworks", count=1, time_received=1700000002, infinite_use=True, appid=570)),
    "ClientEmoticonList": (friends.CMsgClientEmoticonList, dict(
        emoticons=[dict(name="steamhappy", count=1)], stickers=[dict(name="CatSurprise", count=2)], effects=[dict(name="Fireworks", infinite_use=True)])),
    "IgnoreFriendRequest": (player.CPlayer_IgnoreFriend_Request, dict(steamid=ID, unignore=True)),
    "IgnoreFriendResponse": (player.CPlayer_IgnoreFriend_Response, dict(friend_relationship=5)),
    "GetPasswordRsaPublicKeyRequest": (auth.CAuthentication_GetPasswordRSAPublicKey_Request, dict(account_name="someone")),
    "GetPasswordRsaPublicKeyResponse": (auth.CAuthentication_GetPasswordRSAPublicKey_Response, dict(
        publickey_mod="C0FFEE" * 8, publickey_exp="010001", timestamp=1790000005000)),
    "DeviceDetails": (auth.CAuthentication_DeviceDetails, dict(
        device_friendly_name="moho", platform_type=3, os_type=203, gaming_device_type=1, client_count=2,
        machine_id=b"\x09\x08\x07", app_type=2)),
    "AllowedConfirmation": (auth.CAuthentication_AllowedConfirmation, dict(confirmation_type=3, associated_message="a***@example.com")),
    "BeginAuthViaCredentialsRequest": (auth.CAuthentication_BeginAuthSessionViaCredentials_Request, dict(
        device_friendly_name="moho", account_name="someone", encrypted_password="Zm9v", encryption_timestamp=1790000005000,
        remember_login=True, platform_type=3, persistence=0, website_id="Client",
        device_details=dict(device_friendly_name="moho", platform_type=3), guard_data="guard", language=1, qos_level=3)),
    "BeginAuthViaCredentialsResponse": (auth.CAuthentication_BeginAuthSessionViaCredentials_Response, dict(
        client_id=2**50 + 3, request_id=b"\x0a\x0b\x0c", interval=5.0,
        allowed_confirmations=[dict(confirmation_type=3), dict(confirmation_type=4, associated_message="x")],
        steamid=ID, weak_token="weak", agreement_session_url="https://x", extended_error_message="more")),
    "BeginAuthViaQrRequest": (auth.CAuthentication_BeginAuthSessionViaQR_Request, dict(
        device_friendly_name="moho", platform_type=3, device_details=dict(device_friendly_name="moho"), website_id="Client")),
    "BeginAuthViaQrResponse": (auth.CAuthentication_BeginAuthSessionViaQR_Response, dict(
        client_id=2**50 + 4, challenge_url="https://s.team/q/1/2", request_id=b"\x01\x02", interval=2.5,
        allowed_confirmations=[dict(confirmation_type=2)], version=1)),
    "PollAuthSessionRequest": (auth.CAuthentication_PollAuthSessionStatus_Request, dict(
        client_id=2**50 + 3, request_id=b"\x0a\x0b\x0c", token_to_revoke=2**40)),
    "PollAuthSessionResponse": (auth.CAuthentication_PollAuthSessionStatus_Response, dict(
        new_client_id=2**50 + 5, new_challenge_url="https://s.team/q/new", refresh_token="refresh", access_token="access",
        had_remote_interaction=True, account_name="someone", new_guard_data="newguard", agreement_session_url="https://a")),
    "UpdateAuthWithGuardCodeRequest": (auth.CAuthentication_UpdateAuthSessionWithSteamGuardCode_Request, dict(
        client_id=2**50 + 3, steamid=ID, code="AB12C", code_type=3)),
    "UpdateAuthWithGuardCodeResponse": (auth.CAuthentication_UpdateAuthSessionWithSteamGuardCode_Response, dict(agreement_session_url="https://a")),
    "GenerateAccessTokenRequest": (auth.CAuthentication_AccessToken_GenerateForApp_Request, dict(
        refresh_token="refresh", steamid=ID, renewal_type=1)),
    "GenerateAccessTokenResponse": (auth.CAuthentication_AccessToken_GenerateForApp_Response, dict(access_token="access", refresh_token="refresh2")),
    "IpAddress": (base.CMsgIPAddress, dict(v6=bytes(range(16)))),
}


def fill(msg, values):
    for name, value in values.items():
        field = msg.DESCRIPTOR.fields_by_name[name]
        if field.message_type is not None and field.is_repeated:
            for item in value:
                fill(getattr(msg, name).add(), item)
        elif field.message_type is not None:
            fill(getattr(msg, name), value)
        elif field.is_repeated:
            getattr(msg, name).extend(value)
        else:
            setattr(msg, name, value)
    return msg


def rust_fields():
    """struct name -> its field names, read out of proto.rs."""
    text = open(os.path.join(HERE, "..", "proto.rs")).read()
    structs = {}
    for chunk in text.split("pub struct ")[1:]:
        name = chunk.split(" ", 1)[0]
        # An empty message is written `{}` on one line; the rest end at a
        # closing brace at the start of a line.
        body = "" if chunk.startswith(f"{name} {{}}") else chunk.split("\n}", 1)[0]
        structs[name] = set(re.findall(r"pub (\w+):", body))
    return structs


rust = rust_fields()
problems = []
for name, (cls, values) in CASES.items():
    # The oneof in IpAddress is one Rust field standing for two real ones.
    if name == "IpAddress":
        continue
    if name not in rust:
        problems.append(f"{name}: no such struct in proto.rs")
        continue
    if set(values) != rust[name]:
        problems.append(f"{name}: filled {sorted(set(values) ^ rust[name])} differently from the struct")
missing = sorted(set(rust) - set(CASES))
if missing:
    problems.append(f"structs with no case here: {missing}")
if problems:
    sys.exit("\n".join(problems))

for name, (cls, values) in CASES.items():
    print(name, fill(cls(), values).SerializeToString().hex())
