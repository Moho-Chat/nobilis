# nobilis

A chat daemon. It speaks IRC, Discord, Matrix, Kick and Sneedchat (SneedChat, the chat built
into Kiwi Farms), and translates all of them into one unified JSON model — accounts, buffers, messages
— served over a Unix socket.

Frontends talk to that model and never to a protocol. Two exist today: [moho](https://github.com/Moho-Chat/moho),
an Electron desktop client, and dms-chat, a Quickshell/QML shell plugin.


```
src/backend/      one module per protocol (irc, discord, sneedchat, matrix, kick)
src/rpc/          the Unix-socket JSON-RPC server
src/runtime.rs    protocol-agnostic connection/buffer/message state
src/store.rs      SQLite-backed scrollback persistence
src/accounts.rs   account and credential persistence
resources/        assets the protocols need (Sneedchat's smiley table)
```

## Building

```bash
cargo build --release
```

Takes no required arguments. `--data-dir` and `--socket-path` override the defaults below.

## Wire protocol

Newline-delimited JSON over a Unix socket at `$XDG_RUNTIME_DIR/nobilis/nobilis.sock`:

```
request:  {"id": 1, "method": "sendMessage", "params": {...}}
response: {"id": 1, "result": {...}}  or  {"id": 1, "error": "..."}
push:     {"event": "message", "data": {...}}
```

The calls and events are whatever `src/rpc/methods.rs` matches on and the runtime emits - that
is the one list that cannot go stale, and moho's
[ARCHITECTURE.md](https://github.com/Moho-Chat/moho/blob/master/ARCHITECTURE.md) describes the
shape of it and what each backend implements. The ones every service shares are `listAccounts`,
`listBuffers`, `sendMessage`, `editMessage`, `deleteMessage`, `toggleReaction`, `subscribe` and
`getBacklog`; everything a service has that the others do not is a call of its own.

`message`, `presenceChange`, `messageUpdated`, `messageDeleted` and `reactionsChanged` are only
delivered to clients that called `subscribe` for that buffer; everything else broadcasts.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
