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
- `Cargo.toml`: reqwest's `socks` feature; the dev-dependencies are dropped (they named sibling crates of the
  upstream workspace that are not copied).

Drop this copy when upstream lets an application turn previews off and give their client a proxy.
