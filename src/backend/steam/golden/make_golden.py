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
