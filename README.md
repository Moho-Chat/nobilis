# nobilis

A chat daemon. It speaks IRC, Discord, Sneedchat (SneedChat, the Tor-only XenForo chat feature)
and Matrix, and translates all of them into one unified JSON model — accounts, buffers, messages
— served over a Unix socket.

Frontends talk to that model and never to a protocol. Two exist today: [moho](https://git.salastil.com/Salastil/moho),
an Electron desktop client, and dms-chat, a Quickshell/QML shell plugin.

## Why a daemon

Connections, Tor circuits and Matrix sync are expensive to establish and unpleasant to lose.
Keeping them in a long-lived process means a frontend can close, restart, crash or be replaced
mid-session without dropping off IRC or resyncing Matrix from scratch. It also means more than
one frontend can attach at once — the socket accepts multiple clients, each with its own
subscriptions.

```
src/backend/      one module per protocol (irc, discord, sneedchat, matrix)
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

Core methods: `listAccounts`, `listBuffers`, `listProtocols`, `addAccount`, `removeAccount`,
`setAccountConnected`, `joinBuffer`, `partBuffer`, `sendMessage`, `editMessage`, `deleteMessage`,
`toggleReaction`, `subscribe`/`unsubscribe`, `getBacklog`. Each protocol also has its own
account-creation method (`addAccount` for IRC, `addDiscordAccount`, `addSneedChatAccount`,
`addMatrixAccount`).

Push events: `message`, `messageUpdated`, `messageDeleted`, `reactionsChanged`, `presenceChange`,
`bufferListChange`, `connectionState`, `notification`, plus per-protocol login-flow events
(`discordLoginQr`/`discordLoginScanned`/`discordLoginResult`, `sneedChatLoginStatus`/
`sneedChatLoginResult`, `matrixLoginStatus`/`matrixLoginResult`) and Matrix verification events
(`matrixVerificationStatus`/`matrixVerificationEmoji`/`matrixVerificationResult`).

`message`, `presenceChange`, `messageUpdated`, `messageDeleted` and `reactionsChanged` are only
delivered to clients that called `subscribe` for that buffer; everything else broadcasts.

### Notes for frontend authors

- An ordinary message arrives with `kind` `"chat"` from IRC, Discord and Sneedchat, but
  `"message"` from Matrix. Treat both as "something a person said"; every other kind
  (`system`, `join`, `part`, `nick`, `topic`, `matrixJoin`, `matrixInvite`, `matrixKick`,
  `matrixQuit`) is a log line, and which of those to render is the frontend's choice — nobilis
  always records them.
- Media that nobilis fetched on the client's behalf (Tor-routed Sneedchat avatars and
  attachments, Matrix media, the Discord login QR) is handed over as a local `file://` path or
  filesystem path, not a remote URL. This assumes the frontend runs on the same machine.
- `listSneedchatSmilies` returns a bare filename per smiley, resolved against `resources/sneedchat-smilies/`
  in this repository. A frontend that renders them needs its own copy of that directory.

## Protocol backends

- **IRC** — TLS with SASL PLAIN, NickServ auto-identify, autojoin, optional SOCKS5 proxying.
- **Discord** — the official cross-device QR login, a real-time gateway client, and message
  edit/delete/reaction/reply sync.
- **Sneedchat (SneedChat)** — the Tor-only chat built into Kiwi Farms. Runs over an embedded Tor
  client (or an external SOCKS5 proxy), solves the site's proof-of-work anti-bot gate, and holds
  one persistent websocket per configured room behind a single login. Avatars and attachments are
  fetched through the same Tor session and cached locally, since a frontend has no route to a
  `.onion` host of its own.
- **Matrix** — Client-Server API with full end-to-end encryption (vodozemac-backed Olm/Megolm via
  `matrix-sdk-crypto`), SAS device verification, server-side key backup, and room moderation.
- **XMPP/Slack** — not implemented.

## On-disk state

| Path | Contents |
|---|---|
| `~/.config/nobilis/accounts.toml` | account config and credentials |
| `~/.config/nobilis/scrollback.db` | SQLite scrollback |
| `~/.config/nobilis/matrix-crypto/` | Matrix E2EE device keys and Olm sessions |
| `~/.cache/nobilis/` | re-derivable media caches |
| `$XDG_RUNTIME_DIR/nobilis/nobilis.sock` | the control socket |

An `flock` on the data directory keeps a second instance from racing the first; a second process
notices and exits rather than competing for the socket.

State from when this daemon was called `chatd` and lived in the moho repository (`~/.config/moho`,
`~/.cache/moho`) is moved across automatically on first run.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
