use crate::model::Account;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// IRC account shape - mirrors daemon/nobilis/actions.h's ChatdAccountOptions
/// field-for-field (minus a couple of fields, like `autodetect_utf8`, that
/// have no meaningful Rust-side equivalent since this stack is UTF-8 only).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct IrcAccountConfig {
    pub nick: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_true")]
    pub ssl: bool,
    #[serde(default)]
    pub sasl: bool,
    #[serde(default)]
    pub sasl_user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub realname: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub quit_message: Option<String>,
    #[serde(default)]
    pub allow_plaintext_sasl: bool,
    #[serde(default)]
    pub autojoin: String,
    #[serde(default)]
    pub nickserv_password: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Tunnel this connection through a SOCKS5 proxy - disabled by default.
    /// Unlike Sneedchat's embedded-Arti transport, the `irc` crate's proxy
    /// support only knows how to dial an *external* SOCKS5 proxy (see
    /// backend/irc.rs's connection setup and net/tor.rs's doc comment on
    /// why an in-process Arti client can't be handed to it directly) - so
    /// this requires a real Tor daemon or Tor Browser already running and
    /// listening at `tor_proxy` (defaults to the standard system Tor port).
    #[serde(default)]
    pub use_tor: bool,
    #[serde(default)]
    pub tor_proxy: Option<String>,
}

fn default_true() -> bool {
    true
}

impl IrcAccountConfig {
    pub fn account_id(&self) -> String {
        format!("{}@{}", self.nick, self.host)
    }
}

/// Discord account shape - unlike IRC/XMPP this has no user-chosen
/// identifier at creation time (see backend/discord.rs's QR remote-auth
/// flow): `user_id`/`username` are learned from Discord's own API only
/// after a successful login, and `token` is the raw user token that flow
/// produces (used as-is in the `Authorization` header - Discord user
/// tokens, unlike bot tokens, take no prefix).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DiscordAccountConfig {
    pub user_id: String,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub token: String,
    /// Captured once at login from /users/@me's `avatar` hash (see
    /// backend/discord.rs's run_qr_login) - not refreshed on later
    /// reconnects, same precedent as `username` above.
    #[serde(default)]
    pub avatar_url: Option<String>,
}

impl DiscordAccountConfig {
    pub fn account_id(&self) -> String {
        format!("discord:{}", self.user_id)
    }
}

/// One configured room to maintain a permanent connection to - see
/// backend/sockchat/mod.rs, which opens one websocket per room rather than
/// switching a single connection between them (the earlier v1 approach).
/// `name` is the room's real, human-chosen name (e.g. "general") - the
/// server has no endpoint this backend uses to look names up on its own,
/// so it's supplied at account-creation time, same spirit as an IRC
/// account's autojoin list being user-provided rather than discovered.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SockChatRoom {
    pub id: u32,
    pub name: String,
}

/// Sneedchat (SockChat) account shape. Unlike Discord's token, the daemon
/// needs the raw username/password on hand indefinitely (not just an opaque
/// session token) because the XenForo session cookie this backend logs in
/// with can expire and has to be re-derived by logging in again - see
/// backend/sockchat/auth.rs's `Session::refresh`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SockChatAccountConfig {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub totp_secret: Option<String>,
    #[serde(default = "default_sockchat_host")]
    pub host: String,
    #[serde(default = "default_tor_mode")]
    pub tor_mode: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default = "default_sockchat_rooms")]
    pub rooms: Vec<SockChatRoom>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub user_id: Option<u32>,
}

fn default_sockchat_host() -> String {
    crate::backend::sockchat::DEFAULT_ONION.to_string()
}

fn default_tor_mode() -> String {
    "embedded".to_string()
}

fn default_sockchat_rooms() -> Vec<SockChatRoom> {
    vec![SockChatRoom { id: 1, name: "general".to_string() }]
}

impl SockChatAccountConfig {
    pub fn account_id(&self) -> String {
        format!("sockchat:{}", self.username)
    }
}

/// Matrix account shape. Unlike Discord's token (opaque, never expires
/// until revoked) the daemon keeps the raw password on hand too, for
/// unattended re-login if `access_token` is ever rejected (expired session,
/// or a fresh daemon that never got to call /login yet). `device_id` MUST
/// be threaded back through every re-login - see backend/matrix/auth.rs's
/// module doc for why a fresh one silently orphans all E2EE state.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MatrixAccountConfig {
    pub homeserver_url: String,
    pub user_id: String,
    pub password: String,
    pub access_token: String,
    pub device_id: String,
    /// Last successful `/sync` cursor - throttled-persisted (see
    /// Runtime::maybe_persist_matrix_next_batch) rather than on every
    /// sync response, since resuming from a slightly stale cursor after an
    /// unclean restart just replays a few already-seen events (harmless -
    /// the same class of tolerance message-id dedup already needs), while
    /// persisting on every sync would mean a full accounts.toml rewrite
    /// every few seconds on a busy account.
    #[serde(default)]
    pub next_batch: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

impl MatrixAccountConfig {
    pub fn account_id(&self) -> String {
        format!("matrix:{}", self.user_id)
    }
}

/// Kick account shape.
///
/// The token is optional, which is the unusual part and is deliberate: Kick's
/// chat is public, so an account with no credential is a working reader of
/// every channel it is pointed at. Signing in adds sending and the
/// subscription check, and nothing else - see backend/kick's module doc.
///
/// `channels` holds handles (the part after kick.com/), not ids. A streamer's
/// numeric ids are Kick's business and are looked up fresh on every connect;
/// the handle is what somebody typed and what they would recognise if they
/// ever opened this file.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct KickAccountConfig {
    /// Who this is on Kick, or a placeholder for a signed-out reader.
    pub username: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    /// Whether this account's follows have been read into `channels` yet.
    ///
    /// Once, not on every connect, and that is the whole point of the flag: a
    /// follow list seeded repeatedly would undo closing a channel, which is
    /// the one thing closing it is supposed to mean. After the first sync the
    /// list belongs to whoever is using it.
    #[serde(default)]
    pub followed_synced: bool,
}

impl KickAccountConfig {
    pub fn account_id(&self) -> String {
        format!("kick:{}", self.username)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct AccountsFile {
    #[serde(default, rename = "account")]
    irc: Vec<IrcAccountConfig>,
    #[serde(default, rename = "discord_account")]
    discord: Vec<DiscordAccountConfig>,
    #[serde(default, rename = "sockchat_account")]
    sockchat: Vec<SockChatAccountConfig>,
    #[serde(default, rename = "matrix_account")]
    matrix: Vec<MatrixAccountConfig>,
    #[serde(default, rename = "kick_account")]
    kick: Vec<KickAccountConfig>,
}

/// Account/credential persistence at ~/.config/nobilis/accounts.toml,
/// replacing libpurple's accounts.xml. Each protocol gets its own variant
/// (see project plan) rather than a shared flat struct - IRC and Discord
/// so far.
pub struct AccountStore {
    path: PathBuf,
    irc: Mutex<HashMap<String, IrcAccountConfig>>,
    discord: Mutex<HashMap<String, DiscordAccountConfig>>,
    sockchat: Mutex<HashMap<String, SockChatAccountConfig>>,
    matrix: Mutex<HashMap<String, MatrixAccountConfig>>,
    kick: Mutex<HashMap<String, KickAccountConfig>>,
}

impl AccountStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        let (irc, discord, sockchat, matrix, kick) = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let file: AccountsFile = toml::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            let irc = file.irc.into_iter().map(|a| (a.account_id(), a)).collect();
            let discord = file.discord.into_iter().map(|a| (a.account_id(), a)).collect();
            let sockchat = file.sockchat.into_iter().map(|a| (a.account_id(), a)).collect();
            let matrix = file.matrix.into_iter().map(|a| (a.account_id(), a)).collect();
            let kick = file.kick.into_iter().map(|a| (a.account_id(), a)).collect();
            (irc, discord, sockchat, matrix, kick)
        } else {
            (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new())
        };
        Ok(Self { path, irc: Mutex::new(irc), discord: Mutex::new(discord), sockchat: Mutex::new(sockchat), matrix: Mutex::new(matrix), kick: Mutex::new(kick) })
    }

    /// Callers already hold whichever map they just mutated - the others
    /// are locked fresh here (different Mutexes, so no deadlock) just to
    /// snapshot them for the combined write.
    fn persist(
        &self,
        irc: &HashMap<String, IrcAccountConfig>,
        discord: &HashMap<String, DiscordAccountConfig>,
        sockchat: &HashMap<String, SockChatAccountConfig>,
        matrix: &HashMap<String, MatrixAccountConfig>,
        kick: &HashMap<String, KickAccountConfig>,
    ) -> Result<()> {
        let file = AccountsFile {
            irc: irc.values().cloned().collect(),
            discord: discord.values().cloned().collect(),
            sockchat: sockchat.values().cloned().collect(),
            matrix: matrix.values().cloned().collect(),
            kick: kick.values().cloned().collect(),
        };
        let text = toml::to_string_pretty(&file)?;
        // Atomic write-temp-then-rename, same spirit as libpurple's periodic
        // accounts.xml flush - a crash mid-write can't corrupt the file.
        let tmp_path = self.path.with_extension("toml.tmp");
        {
            // Private from the moment it exists rather than tightened after -
            // this file holds Discord tokens and IRC passwords, and a window
            // in which it was readable is a window that counts.
            let mut f = crate::secure::create_private_file(&tmp_path)?;
            f.write_all(text.as_bytes())?;
        }
        std::fs::rename(&tmp_path, &self.path)?;
        // The rename carries the temporary file's permissions on Unix, but
        // say so explicitly: an accounts.toml that predates this code, or one
        // restored from a backup, would otherwise keep whatever it had.
        crate::secure::restrict_file_to_owner(&self.path)?;
        Ok(())
    }

    pub fn get_irc(&self, account_id: &str) -> Option<IrcAccountConfig> {
        self.irc.lock().unwrap().get(account_id).cloned()
    }

    pub fn all_irc(&self) -> Vec<IrcAccountConfig> {
        self.irc.lock().unwrap().values().cloned().collect()
    }

    /// Returns None if an account with this nick@host already exists
    /// (matches nobilis_account_create()'s dedup behavior).
    pub fn add_irc(&self, config: IrcAccountConfig) -> Result<Option<IrcAccountConfig>> {
        let id = config.account_id();
        let mut irc = self.irc.lock().unwrap();
        if irc.contains_key(&id) {
            return Ok(None);
        }
        irc.insert(id, config.clone());
        self.persist(&irc, &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
        Ok(Some(config))
    }

    pub fn get_discord(&self, account_id: &str) -> Option<DiscordAccountConfig> {
        self.discord.lock().unwrap().get(account_id).cloned()
    }

    pub fn all_discord(&self) -> Vec<DiscordAccountConfig> {
        self.discord.lock().unwrap().values().cloned().collect()
    }

    /// Unlike add_irc, this upserts rather than rejecting an existing
    /// user_id - re-running the QR login flow for an account you already
    /// added is a legitimate way to refresh a stale/revoked token, not a
    /// duplicate-creation mistake (there's no equivalent "did you mean to
    /// reconnect instead" ambiguity IRC's nick@host dedup guards against).
    pub fn add_discord(&self, config: DiscordAccountConfig) -> Result<DiscordAccountConfig> {
        let id = config.account_id();
        let mut discord = self.discord.lock().unwrap();
        discord.insert(id, config.clone());
        self.persist(&self.irc.lock().unwrap(), &discord, &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
        Ok(config)
    }

    pub fn get_sockchat(&self, account_id: &str) -> Option<SockChatAccountConfig> {
        self.sockchat.lock().unwrap().get(account_id).cloned()
    }

    pub fn all_sockchat(&self) -> Vec<SockChatAccountConfig> {
        self.sockchat.lock().unwrap().values().cloned().collect()
    }

    /// Upserts like add_discord - re-adding the same username is how a user
    /// updates a changed password/TOTP secret, not a duplicate mistake.
    pub fn add_sockchat(&self, config: SockChatAccountConfig) -> Result<SockChatAccountConfig> {
        let id = config.account_id();
        let mut sockchat = self.sockchat.lock().unwrap();
        sockchat.insert(id, config.clone());
        self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &sockchat, &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
        Ok(config)
    }

    pub fn get_matrix(&self, account_id: &str) -> Option<MatrixAccountConfig> {
        self.matrix.lock().unwrap().get(account_id).cloned()
    }

    pub fn all_matrix(&self) -> Vec<MatrixAccountConfig> {
        self.matrix.lock().unwrap().values().cloned().collect()
    }

    /// Upserts like add_discord/add_sockchat - re-running addMatrixAccount
    /// for an already-added user_id is how a user refreshes a changed
    /// password, not a duplicate-creation mistake.
    pub fn add_matrix(&self, config: MatrixAccountConfig) -> Result<MatrixAccountConfig> {
        let id = config.account_id();
        let mut matrix = self.matrix.lock().unwrap();
        matrix.insert(id, config.clone());
        self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &matrix, &self.kick.lock().unwrap())?;
        Ok(config)
    }

    pub fn get_kick(&self, account_id: &str) -> Option<KickAccountConfig> {
        self.kick.lock().unwrap().get(account_id).cloned()
    }

    pub fn all_kick(&self) -> Vec<KickAccountConfig> {
        self.kick.lock().unwrap().values().cloned().collect()
    }

    /// Upserts, like the others: signing in to Kick again with the same
    /// username is how a rejected token gets replaced.
    pub fn add_kick(&self, config: KickAccountConfig) -> Result<KickAccountConfig> {
        let id = config.account_id();
        let mut kick = self.kick.lock().unwrap();
        kick.insert(id, config.clone());
        self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &kick)?;
        Ok(config)
    }

    /// Records that this account's follows have been read in, so they are not
    /// read in again - see `KickAccountConfig::followed_synced`.
    pub fn mark_kick_follows_synced(&self, account_id: &str) -> Result<bool> {
        let mut kick = self.kick.lock().unwrap();
        let Some(config) = kick.get_mut(account_id) else { return Ok(false) };
        if config.followed_synced {
            return Ok(false);
        }
        config.followed_synced = true;
        self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &kick)?;
        Ok(true)
    }

    /// Remembers which streamers this account watches.
    ///
    /// Persisted rather than rebuilt from open buffers, for the same reason
    /// an IRC autojoin list is: a channel you went and found should still be
    /// there tomorrow, and nothing else in the daemon knows you wanted it.
    pub fn set_kick_channels(&self, account_id: &str, channels: Vec<String>) -> Result<bool> {
        let mut kick = self.kick.lock().unwrap();
        let Some(config) = kick.get_mut(account_id) else { return Ok(false) };
        if config.channels == channels {
            return Ok(false);
        }
        config.channels = channels;
        self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &kick)?;
        Ok(true)
    }

    /// Called after a re-login (expired/rejected access_token) refreshes
    /// the session - device_id should normally be unchanged (the whole
    /// point of passing it back into backend/matrix/auth.rs::login is
    /// getting the *same* one back), but is still updated here rather than
    /// assumed, in case a homeserver ever behaves unexpectedly.
    pub fn set_matrix_session(&self, account_id: &str, access_token: &str, device_id: &str) -> Result<bool> {
        let mut matrix = self.matrix.lock().unwrap();
        match matrix.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                a.access_token = access_token.to_string();
                a.device_id = device_id.to_string();
                self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &matrix, &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }

    /// See MatrixAccountConfig::next_batch's doc comment on why callers
    /// throttle how often this actually gets invoked.
    pub fn set_matrix_next_batch(&self, account_id: &str, next_batch: &str) -> Result<bool> {
        let mut matrix = self.matrix.lock().unwrap();
        match matrix.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                a.next_batch = Some(next_batch.to_string());
                self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &matrix, &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }

    /// Replaces the account's configured room list - takes effect on the
    /// next reconnect (see backend/sockchat::spawn, called after this by
    /// the RPC handler so it applies immediately rather than waiting for a
    /// daemon restart).
    pub fn set_sockchat_rooms(&self, account_id: &str, rooms: Vec<SockChatRoom>) -> Result<bool> {
        let mut sockchat = self.sockchat.lock().unwrap();
        match sockchat.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                a.rooms = rooms;
                self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &sockchat, &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }

    /// Updates the account's transport mode (embedded Tor vs an external
    /// SOCKS5 proxy) - takes effect on the next reconnect, same as
    /// set_sockchat_rooms (the RPC handler re-spawns the account
    /// immediately after this succeeds, rather than waiting for the user
    /// to notice and reconnect manually).
    pub fn set_sockchat_tor_config(&self, account_id: &str, tor_mode: String, proxy: Option<String>) -> Result<bool> {
        let mut sockchat = self.sockchat.lock().unwrap();
        match sockchat.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                a.tor_mode = tor_mode;
                a.proxy = proxy;
                self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &sockchat, &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }

    /// Called after a successful login once the `xf_user` cookie reveals the
    /// account's numeric id - best-effort, not required for the backend to
    /// function.
    pub fn set_sockchat_user_id(&self, account_id: &str, user_id: u32) -> Result<bool> {
        let mut sockchat = self.sockchat.lock().unwrap();
        match sockchat.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                a.user_id = Some(user_id);
                self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &sockchat, &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }

    pub fn remove(&self, account_id: &str) -> Result<bool> {
        {
            let mut irc = self.irc.lock().unwrap();
            if irc.remove(account_id).is_some() {
                self.persist(&irc, &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                return Ok(true);
            }
        }
        {
            let mut discord = self.discord.lock().unwrap();
            if discord.remove(account_id).is_some() {
                self.persist(&self.irc.lock().unwrap(), &discord, &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                return Ok(true);
            }
        }
        {
            let mut sockchat = self.sockchat.lock().unwrap();
            if sockchat.remove(account_id).is_some() {
                self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &sockchat, &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                return Ok(true);
            }
        }
        {
            let mut matrix = self.matrix.lock().unwrap();
            if matrix.remove(account_id).is_some() {
                self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &matrix, &self.kick.lock().unwrap())?;
                return Ok(true);
            }
        }
        let mut kick = self.kick.lock().unwrap();
        if kick.remove(account_id).is_some() {
            self.persist(&self.irc.lock().unwrap(), &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &kick)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn set_autojoin(&self, account_id: &str, channels_csv: &str) -> Result<bool> {
        self.mutate(account_id, |a| a.autojoin = channels_csv.to_string())
    }

    pub fn set_nickserv_password(&self, account_id: &str, password: &str) -> Result<bool> {
        self.mutate(account_id, |a| {
            a.nickserv_password = if password.is_empty() { None } else { Some(password.to_string()) }
        })
    }

    /// Toggles routing this IRC connection through a SOCKS5 proxy - see
    /// IrcAccountConfig::use_tor's doc comment for why this is an external
    /// proxy rather than the embedded Arti client Sneedchat uses. Empty
    /// `proxy` means "leave whatever's stored untouched" (same convention
    /// as set_sasl's password handling), so re-toggling on/off doesn't
    /// clobber a previously-entered address.
    pub fn set_irc_use_tor(&self, account_id: &str, use_tor: bool, proxy: &str) -> Result<bool> {
        self.mutate(account_id, |a| {
            a.use_tor = use_tor;
            if !proxy.is_empty() {
                a.tor_proxy = Some(proxy.to_string());
            }
        })
    }

    /// Opportunistic refresh, called on every gateway READY (see
    /// backend/discord.rs's run_gateway) rather than only at initial QR
    /// login - accounts added before this feature existed have no
    /// avatar_url yet, and a user's real Discord avatar can change over
    /// time anyway. Idempotent/cheap enough to just always persist rather
    /// than diffing first.
    pub fn set_discord_avatar_url(&self, account_id: &str, url: &str) -> Result<bool> {
        let mut discord = self.discord.lock().unwrap();
        match discord.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                a.avatar_url = Some(url.to_string());
                self.persist(&self.irc.lock().unwrap(), &discord, &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }

    /// Purely a local rename - never touches Discord's API (there's no
    /// "display name" concept to push there anyway; Discord's real
    /// global_name/username still drives what other users see). Tries the
    /// IRC store first, falling back to Discord, since `mutate` only
    /// knows about IrcAccountConfig - this was the bug behind "Display
    /// Name doesn't work for Discord": every call silently hit the IRC-only
    /// path and returned `false` for any discord: account id without
    /// touching discord_account_to_json's own display_name field at all.
    pub fn set_display_name(&self, account_id: &str, name: &str) -> Result<bool> {
        if self.mutate(account_id, |a| {
            a.display_name = if name.is_empty() { None } else { Some(name.to_string()) }
        })? {
            return Ok(true);
        }
        let mut discord = self.discord.lock().unwrap();
        match discord.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                a.display_name = if name.is_empty() { None } else { Some(name.to_string()) };
                self.persist(&self.irc.lock().unwrap(), &discord, &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }

    /// Empty password means "leave whatever's stored untouched" - same
    /// convention nobilis_set_account_sasl() uses (the frontend never reads
    /// the password back, see hasPassword/hasNickservPassword instead).
    pub fn set_sasl(
        &self,
        account_id: &str,
        enabled: bool,
        sasl_user: &str,
        password: &str,
        allow_plaintext: bool,
    ) -> Result<bool> {
        self.mutate(account_id, |a| {
            a.sasl = enabled;
            a.sasl_user = if sasl_user.is_empty() { None } else { Some(sasl_user.to_string()) };
            a.allow_plaintext_sasl = allow_plaintext;
            if !password.is_empty() {
                a.password = Some(password.to_string());
            }
        })
    }

    fn mutate(&self, account_id: &str, f: impl FnOnce(&mut IrcAccountConfig)) -> Result<bool> {
        let mut irc = self.irc.lock().unwrap();
        match irc.get_mut(account_id) {
            None => Ok(false),
            Some(a) => {
                f(a);
                self.persist(&irc, &self.discord.lock().unwrap(), &self.sockchat.lock().unwrap(), &self.matrix.lock().unwrap(), &self.kick.lock().unwrap())?;
                Ok(true)
            }
        }
    }
}

/// `state` ("connected"/"connecting"/"disconnected") comes from the
/// runtime's live connection tracking, not from AccountStore, which only
/// knows the persisted config - see runtime.rs.
pub fn irc_account_to_json(a: &IrcAccountConfig, state: &str) -> Account {
    let id = a.account_id();
    Account {
        status: "online".to_string(),
        display_name: a.display_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| id.clone()),
        id,
        service: "irc".to_string(),
        state: state.to_string(),
        autojoin: a.autojoin.clone(),
        has_nickserv_password: a.nickserv_password.is_some(),
        sasl_enabled: a.sasl,
        sasl_username: a.sasl_user.clone().unwrap_or_default(),
        allow_plaintext_sasl: a.allow_plaintext_sasl,
        ssl: a.ssl,
        has_password: a.password.as_deref().is_some_and(|p| !p.is_empty()),
        avatar_url: None,
        sockchat_rooms: Vec::new(),
        tor_mode: None,
        tor_proxy: a.tor_proxy.clone(),
        use_tor: a.use_tor,
        has_key_backup: false,
    }
}

/// Discord has none of IRC's autojoin/SASL/NickServ concepts - those
/// fields are just left at their zero values so the shared Account shape
/// (which the frontend renders generically) stays valid.
pub fn discord_account_to_json(a: &DiscordAccountConfig, state: &str) -> Account {
    Account {
        id: a.account_id(),
        service: "discord".to_string(),
        status: "online".to_string(),
        display_name: a.display_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| a.username.clone()),
        state: state.to_string(),
        autojoin: String::new(),
        has_nickserv_password: false,
        sasl_enabled: false,
        sasl_username: String::new(),
        allow_plaintext_sasl: false,
        ssl: true,
        has_password: !a.token.is_empty(),
        avatar_url: a.avatar_url.clone(),
        sockchat_rooms: Vec::new(),
        tor_mode: None,
        tor_proxy: None,
        use_tor: false,
        has_key_backup: false,
    }
}

/// Sneedchat has none of IRC's autojoin/SASL/NickServ concepts either -
/// same zero-value convention as discord_account_to_json.
pub fn sockchat_account_to_json(a: &SockChatAccountConfig, state: &str) -> Account {
    Account {
        id: a.account_id(),
        service: "sockchat".to_string(),
        status: "online".to_string(),
        display_name: a.display_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| a.username.clone()),
        state: state.to_string(),
        autojoin: String::new(),
        has_nickserv_password: false,
        sasl_enabled: false,
        sasl_username: String::new(),
        allow_plaintext_sasl: false,
        ssl: true,
        has_password: !a.password.is_empty(),
        avatar_url: None,
        sockchat_rooms: a.rooms.iter().map(|r| crate::model::SockChatRoomInfo { id: r.id, name: r.name.clone() }).collect(),
        tor_mode: Some(a.tor_mode.clone()),
        tor_proxy: a.proxy.clone(),
        use_tor: false,
        has_key_backup: false,
    }
}

/// Kick, like the others, has none of IRC's autojoin/SASL/NickServ concepts.
///
/// `has_password` is the one field carrying real information here, and it is
/// the answer to a question a Kick account genuinely has both answers to:
/// whether this one is signed in. A signed-out account is not broken or
/// half-configured - it reads every channel it watches - so the client shows
/// it as an ordinary connected account and only sending says otherwise.
pub fn kick_account_to_json(a: &KickAccountConfig, state: &str) -> Account {
    Account {
        id: a.account_id(),
        service: "kick".to_string(),
        status: "online".to_string(),
        display_name: a.username.clone(),
        state: state.to_string(),
        // The watched channels are deliberately not reported as `autojoin`.
        // That field is IRC's comma-separated string with IRC's meaning, and
        // the client offers an edit box for it; a Kick channel list is added
        // to by watching a streamer and taken from by closing the buffer.
        autojoin: String::new(),
        has_nickserv_password: false,
        sasl_enabled: false,
        sasl_username: String::new(),
        allow_plaintext_sasl: false,
        ssl: true,
        has_password: a.token.as_deref().is_some_and(|t| !t.is_empty()),
        avatar_url: None,
        sockchat_rooms: Vec::new(),
        tor_mode: None,
        tor_proxy: None,
        use_tor: false,
        has_key_backup: false,
    }
}

/// Matrix has none of IRC's autojoin/SASL/NickServ concepts either - same
/// zero-value convention as discord_account_to_json/sockchat_account_to_json.
/// `has_key_backup` comes from the caller (Runtime::list_accounts) rather
/// than being derivable from `a` alone - it reflects live in-memory
/// BackupMachine state (see runtime.rs's matrix_backup_enabled), not
/// anything persisted in MatrixAccountConfig.
pub fn matrix_account_to_json(a: &MatrixAccountConfig, state: &str, has_key_backup: bool) -> Account {
    Account {
        id: a.account_id(),
        service: "matrix".to_string(),
        status: "online".to_string(),
        display_name: a.display_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| a.user_id.clone()),
        state: state.to_string(),
        autojoin: String::new(),
        has_nickserv_password: false,
        sasl_enabled: false,
        sasl_username: String::new(),
        allow_plaintext_sasl: false,
        ssl: true,
        has_password: !a.password.is_empty(),
        avatar_url: None,
        sockchat_rooms: Vec::new(),
        tor_mode: None,
        tor_proxy: None,
        use_tor: false,
        has_key_backup,
    }
}
