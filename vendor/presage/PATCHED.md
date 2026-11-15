# Patched copy of presage

The `presage` crate from [whisperfish/presage](https://github.com/whisperfish/presage) at commit
`4b81740610c301cbd54d95f4dbd4cadd692af95e`, AGPL-3.0 like the original (`LICENSE.md`), wired in through `[patch]` in
nobilis's `Cargo.toml`. Upstream fetches a link preview for every message sent with a link in it, with an HTTP client of
its own: that went around an account's Tor or proxy route and ignored the person's "Generate link previews" setting.

What is changed, and nothing else:

- `src/manager/registered.rs`, `fetch_link_previews`: does nothing unless the thread's network setting (the patched
  `libsignal_service::push_service::network`) says link previews are allowed.
- `src/model/previews.rs`, `client_builder`: the preview client goes through the thread's proxy when it has one; its
  test module is removed.
- `src/manager/registered.rs`, `storage_service`: a public way to the account's storage service, which upstream
  reads only inside its group sync.
- `src/manager/registered.rs`, group management, which upstream does not have: `change_group` (any change, built by the
  caller from the group's operations, sent with the patched `PushService::patch_group`, then saved and sent to the
  members and to anybody it took out; leaving marks the group left), `create_group`, `add_member_action` and
  `profile_key_credential` (adding somebody directly), `group_link_info` and `join_group_by_link`.
  `create_group` sends the access rules Signal's clients make a group with (members edit and add, no joining by
  link): unset, the server answers HTTP 400.
  A change (and a join by link) leaves the actions' `group_id` empty: the server fills it in and refuses a change
  that sets it (HTTP 400).
  `change_group` also answers the change as the server signed it, so the caller can say it in its own conversation.
- `src/manager/registered.rs`: `upload_group_avatar` (encrypts and uploads a group's picture, answering the CDN key a
  change names it by) and `group_avatar_at` (a group's picture by that key, without the per-group copy that outlives a
  change).
- `src/model/groups.rs`: `Group` keeps `announcements_only` and `banned` (`BannedMember`), both `serde(default)`, so
  who may send and who is banned can be shown and changed; `ServiceIdType` is `Clone + Copy`.
- `src/manager/registered.rs`: `update_profile_with_picture` (a new profile picture, or none).
- `src/manager/registered.rs`, `src/errors.rs`: a group send that reached some members is saved and answered as
  `Error::PartialGroupSend` (who it missed, and why); one that reached nobody fails with the first error as before.
- `src/manager/registered.rs`: `send_story_to` (a story to My Story's or a custom list's people, not saved to a
  conversation).
- `src/manager/registered.rs`: `group_call_proof` (the group server's call token), and `send_to_people` (one content
  to a set of people, as a group send; `send_story_to` is it for a story).
- `src/manager/registered.rs`: `transfer_archive` and `download_transfer_archive` (the history a phone sends a new link).
- `src/manager/registered.rs`: `get_attachment_by_plaintext_hash` (a transferred-history file, checked by its plaintext's
  hash where the archive gives no encrypted digest).
- `src/manager/registered.rs`: `set_username` (Signal's candidates or a chosen number, reserved, proved, confirmed
  with a fresh link), `delete_username`, `reset_username_link`; `Error::UsernameTaken`.
- `src/errors.rs`: `Error::GroupChange`, a refusal in words.
- `src/manager/registered.rs`: the identified websocket asks for stories (`X-Signal-Receive-Stories: true`).
- `Cargo.toml`: reqwest's `socks` feature; the dev-dependencies are dropped (they named sibling crates of the
  upstream workspace that are not copied).

Drop this copy when upstream lets an application turn previews off and give their client a proxy.
