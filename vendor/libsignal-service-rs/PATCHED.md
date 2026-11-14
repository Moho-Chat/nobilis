# Patched copy of libsignal-service-rs

A copy of [libsignal-service-rs](https://github.com/whisperfish/libsignal-service-rs) at commit `7a7526741dfd9ad4c4894717f22651eb0506fb7e`
(the commit presage `4b81740` pins), AGPL-3.0 like the original (`LICENSE.md`), wired in through `[patch]` in nobilis's `Cargo.toml`.

What is changed, and nothing else:

- `src/push_service/mod.rs`: `push_service::network`, a per-thread setting for the proxy and the user agent that every
  `PushService::new` on that thread reads (and whether presage, patched beside it, may fetch link previews). The library builds one `reqwest` client per `PushService` and carries REST,
  the attachment CDNs and both websockets over it, but presage constructs them where no proxy can be handed in.
- `src/push_service/mod.rs`: `patch_group`, `create_group` and `group_join_info` (`/v2/groups/`, `/v1/groups/join/`),
  the group server's requests for changing, making and looking at a group through its link.
- `src/websocket/profile.rs`: `retrieve_profile_key_credential`, a person's expiring profile key credential.
- `src/groups_v2/operations.rs`: a few encryption helpers made public (`encrypt_aci`, `encrypt_service_id`,
  `encrypt_profile_key`, `decrypt_title`, `decrypt_description_text`, `decrypt_disappearing_messages_timer`).
- `Cargo.toml`: reqwest's `socks` feature.
- `src/account_manager.rs`, `update_device_name`: the name is encrypted to the account's own identity key (from the
  identity key pair), as Signal's clients do; upstream looked the account up among the identities it had seen, which
  a freshly linked device has not yet, so renaming it failed.
- Group pictures: `GroupOperations::encrypt_avatar` (the blob members decrypt with `decrypt_avatar`), and
  `PushService::upload_group_avatar` (`GET /v1/groups/avatar/form` from the group server, then CDN 0 through
  `upload_to_cdn0`, whose upload attributes are now crate-visible so the form can be made into them).
- Stories: a `StoryMessage` is sent with `?story=true` (`OutgoingPushMessages::story`), and its sent copy is synced to
  the account's other devices with `story_message` and a `StoryMessageRecipient` per member (upstream syncs only data
  and edit messages).

Drop this copy when upstream takes a way to set the client's proxy and user agent.
