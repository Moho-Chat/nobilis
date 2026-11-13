# Patched copy of libsignal-service-rs

A copy of [libsignal-service-rs](https://github.com/whisperfish/libsignal-service-rs) at commit `7a7526741dfd9ad4c4894717f22651eb0506fb7e`
(the commit presage `4b81740` pins), AGPL-3.0 like the original (`LICENSE.md`), wired in through `[patch]` in nobilis's `Cargo.toml`.

What is changed, and nothing else:

- `src/push_service/mod.rs`: `push_service::network`, a per-thread setting for the proxy and the user agent that every
  `PushService::new` on that thread reads (and whether presage, patched beside it, may fetch link previews). The library builds one `reqwest` client per `PushService` and carries REST,
  the attachment CDNs and both websockets over it, but presage constructs them where no proxy can be handed in.
- `Cargo.toml`: reqwest's `socks` feature.

Drop this copy when upstream takes a way to set the client's proxy and user agent.
