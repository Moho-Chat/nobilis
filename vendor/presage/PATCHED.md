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
- `src/errors.rs`: `Error::GroupChange`, a refusal in words.
- `src/manager/registered.rs`: the identified websocket asks for stories (`X-Signal-Receive-Stories: true`).
- `Cargo.toml`: reqwest's `socks` feature; the dev-dependencies are dropped (they named sibling crates of the
  upstream workspace that are not copied).

Drop this copy when upstream lets an application turn previews off and give their client a proxy.
