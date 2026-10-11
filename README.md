# nobilis

A chat daemon. It speaks IRC, Discord, Matrix, Kick and Sneedchat (SneedChat, the chat built
into Kiwi Farms), and translates all of them into one unified JSON model — accounts, buffers, messages
— served over a Unix socket.

Frontends talk to that model and never to a protocol. Two exist today: [moho](https://github.com/Moho-Chat/moho),
an Electron desktop client, and dms-chat, a Quickshell/QML shell plugin.

## Why a daemon

Connections, Tor circuits and Matrix sync are expensive to establish and unpleasant to lose.
Keeping them in a long-lived process means a frontend can close, restart, crash or be replaced
mid-session without dropping off IRC or resyncing Matrix from scratch. It also means more than
one frontend can attach at once — the socket accepts multiple clients, each with its own
subscriptions.

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

### Notes for frontend authors

- An ordinary message arrives with `kind` `"chat"` from IRC, Discord, Kick and Sneedchat, but
  `"message"` from Matrix. Treat both as "something a person said". Every other kind is a log
  line or an event - a join, a topic change, a Kick raid or redeemed reward - and which of them
  to draw is the frontend's choice; nobilis always records them. A kind a frontend does not know
  is best drawn as a log line.
- A message can carry `embeds`: what a service sent with it (a Discord embed, a Matrix link
  preview) and what nobilis worked out for it - a YouTube link's title and channel, a GIF's
  address, and for a Kick event the amount, the recipients and the colours. They are added
  after the message, as a `messageUpdated` with `edited: false`.
- Media that nobilis fetched on the client's behalf (Sneedchat avatars and attachments, Matrix
  media, shrunk Kick emotes, Discord thumbnails and stickers, the Discord login QR) is handed over
  as a local `file://` path or filesystem path, not a remote URL. This assumes the frontend runs
  on the same machine.
- `listSneedchatSmilies` returns a bare filename per smiley, resolved against
  `resources/sneedchat-smilies/` in this repository. A frontend that renders them needs its own
  copy of that directory.

## Protocol backends

IRC, Discord, Matrix, Kick and Sneedchat, each one module under `src/backend/`. Any account on
any of them can be routed through Tor or a SOCKS5 proxy. What each one implements is in moho's
[ARCHITECTURE.md](https://github.com/Moho-Chat/moho/blob/master/ARCHITECTURE.md).

## On-disk state

| Path | Contents |
|---|---|
| `~/.config/nobilis/accounts.toml` | account config and credentials |
| `~/.config/nobilis/scrollback.db` | SQLite scrollback |
| `~/.config/nobilis/matrix-crypto/` | Matrix E2EE device keys and Olm sessions |
| `~/.config/nobilis/voice.toml` | microphone, speakers, volumes and voice processing |
| `~/.config/nobilis/highlights.toml`, `ignores.toml` | what counts as a mention, and who is ignored |
| `~/.config/nobilis/dcc.toml`, `irc-sts.toml` | DCC settings, and IRC networks that have pinned TLS |
| `~/.config/nobilis/tor-state/`, `tor-cache/` | the embedded Tor client's state |
| `~/.cache/nobilis/` | re-derivable media caches |
| `$XDG_RUNTIME_DIR/nobilis/nobilis.sock` | the control socket |

An `flock` on the data directory keeps a second instance from racing the first; a second process
notices and exits rather than competing for the socket.

State from when this daemon was called `chatd` and lived in the moho repository (`~/.config/moho`,
`~/.cache/moho`) is moved across automatically on first run.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
