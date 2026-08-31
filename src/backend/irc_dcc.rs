//! Receiving a file over DCC, which on IRC is how XDCC bots hand one over.
//!
//! Beside `irc.rs` the way `discord_voice.rs` sits beside `discord.rs`: it is
//! a second protocol that rides on the first, with its own sockets and its own
//! state, and folding it into the message loop would bury it.
//!
//! Everything in an offer is chosen by whoever sent it - the name, the size,
//! the address, the port - so the whole module is written against that. Three
//! rules carry it, and each is a test rather than a habit:
//!
//! 1. No string from the network reaches the filesystem. The name we write is
//!    derived here and joined onto our own configured directory, so traversal
//!    is not blocked so much as unrepresentable.
//! 2. Limits are enforced against the stream, never against the claim. The
//!    advertised size is a claim.
//! 3. The route is inherited from the connection that carried the offer. An
//!    account reached through Tor transfers through Tor, and a proxy that will
//!    not carry it is an error rather than a reason to dial direct.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crate::state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A DCC message we understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dcc {
    Send(DccSend),
    /// Recognised but not offered here - DCC CHAT and friends. Kept apart from
    /// "not DCC at all" so it can be answered with a reason rather than shown
    /// as a line of control characters.
    Unsupported(String),
}

/// An offer of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DccSend {
    /// What they called it, kept only to show and to log. Never used to build
    /// a path - `file_name` is.
    pub raw_name: String,
    /// What we would write, already made safe.
    pub file_name: String,
    pub addr: IpAddr,
    pub port: u16,
    pub size: u64,
    /// Present on a passive (reverse) offer, where the sender is asking *us*
    /// to listen. Refused - see `passive` below.
    pub token: Option<String>,
}

impl DccSend {
    /// The sender cannot accept an inbound connection and wants us to listen
    /// instead. Refused: it would mean opening a socket and publishing an
    /// address, which on a Tor-routed account cannot be done without undoing
    /// the tunnel, and which nothing here does yet in any case.
    pub fn passive(&self) -> bool {
        self.port == 0 || self.token.is_some()
    }
}

/// Pulls a DCC message out of a PRIVMSG body, if it is one.
///
/// The body still has its CTCP delimiters on: the `irc` crate answers VERSION
/// and PING itself and passes everything else through untouched, which is why
/// an offer arrives here looking like an ordinary message.
pub fn parse_dcc(body: &str) -> Option<Dcc> {
    let inner = body.strip_prefix('\u{1}')?;
    // The trailing delimiter is supposed to be there and often is not.
    let inner = inner.strip_suffix('\u{1}').unwrap_or(inner);
    let rest = inner.strip_prefix("DCC ").or_else(|| inner.strip_prefix("dcc "))?;
    let rest = rest.trim();

    let (verb, args) = rest.split_once(char::is_whitespace)?;
    if !verb.eq_ignore_ascii_case("SEND") {
        return Some(Dcc::Unsupported(verb.to_uppercase()));
    }
    parse_send(args).map(Dcc::Send)
}

/// `<name> <address> <port> <size> [token]`, where the name may be quoted.
///
/// Unquoted names containing spaces are genuinely ambiguous in this protocol
/// and always have been - mIRC quotes them for exactly that reason. The
/// numeric fields are taken from the end when the simple reading does not fit,
/// which recovers the common case. Getting it wrong costs a refused transfer
/// and nothing else: the name is made safe either way, and the address is
/// checked either way.
fn parse_send(args: &str) -> Option<DccSend> {
    let args = args.trim();
    let (raw_name, tail) = if let Some(rest) = args.strip_prefix('"') {
        let (name, rest) = rest.split_once('"')?;
        (name.to_string(), rest.trim().to_string())
    } else {
        let parts: Vec<&str> = args.split_whitespace().collect();
        // A name of one token followed by the three or four numbers.
        if parts.len() == 4 || parts.len() == 5 {
            (parts[0].to_string(), parts[1..].join(" "))
        } else {
            // Take the numbers off the end and let the name keep its spaces.
            let numeric_tail = parts.iter().rev().take_while(|t| t.chars().all(|c| c.is_ascii_digit())).count();
            let take = numeric_tail.min(4);
            if take < 3 || parts.len() <= take {
                return None;
            }
            let split = parts.len() - take;
            (parts[..split].join(" "), parts[split..].join(" "))
        }
    };

    let fields: Vec<&str> = tail.split_whitespace().collect();
    if fields.len() < 3 {
        return None;
    }
    let addr = parse_address(fields[0])?;
    let port: u16 = fields[1].parse().ok()?;
    let size: u64 = fields[2].parse().ok()?;
    let token = fields.get(3).map(|t| t.to_string());

    Some(DccSend {
        file_name: safe_file_name(&raw_name),
        raw_name,
        addr,
        port,
        size,
        token,
    })
}

/// The address field, which is conventionally a 32-bit number rather than
/// anything that looks like an address.
///
/// Newer clients send an IPv6 literal instead, and some send dotted quad, so
/// all three are read. Addresses that could only point back into this machine
/// or its own network segment are refused: the only thing this module ever
/// sends is an eight-byte acknowledgement, so the reach is small, but there is
/// no reason for somebody else's offer to name our loopback.
fn parse_address(field: &str) -> Option<IpAddr> {
    let addr = if let Ok(n) = field.parse::<u32>() {
        IpAddr::V4(Ipv4Addr::from(n))
    } else {
        field.parse::<IpAddr>().ok()?
    };
    if addr.is_loopback() || addr.is_unspecified() || addr.is_multicast() {
        return None;
    }
    if let IpAddr::V4(v4) = addr {
        // 169.254/16, and 255.255.255.255.
        if v4.is_link_local() || v4.is_broadcast() {
            return None;
        }
    }
    Some(addr)
}

/// The longest name we will write, in characters.
///
/// Comfortably inside every filesystem's own limit while leaving room for the
/// `.part` suffix and a " (2)" from deduplication.
const MAX_NAME_CHARS: usize = 120;

/// Windows keeps these reserved whatever extension follows, and opening one
/// talks to a device instead of a file.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A name safe to join onto the download directory, from one that is not.
///
/// Written to be dull rather than clever, because it is the single thing
/// standing between a stranger's string and the filesystem. It always returns
/// a name: there is no error path to forget to handle, and no input that
/// produces something with a directory in it.
///
/// Deliberately stricter than a Unix-only version would need to be. A name
/// that is harmless here can be a device on Windows, and a trailing dot or
/// space is quietly dropped there - so `evil.exe.` and `evil.exe` are the same
/// file on one platform and not on the other, which is exactly the kind of
/// difference that survives a review.
pub fn safe_file_name(raw: &str) -> String {
    // Both separators regardless of platform: the name came off the network,
    // not off this filesystem, so it may use either.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    // Drops a drive letter, and an alternate data stream with it.
    let base = base.rsplit(':').next().unwrap_or("");

    let cleaned: String = base
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || (c as u32) < 0x20 || c == '\u{7f}' {
                '_'
            } else {
                c
            }
        })
        .collect();

    // Leading dots would hide it; trailing dots and spaces are dropped by
    // Windows itself, so a name is trimmed here rather than being renamed
    // out from under us later.
    let trimmed = cleaned.trim_start_matches('.').trim_end_matches(['.', ' ']);
    let capped = cap_length(trimmed);

    let stem = capped.split('.').next().unwrap_or("");
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return format!("_{capped}");
    }
    if capped.is_empty() {
        return "download".to_string();
    }
    capped
}

/// Shortens a long name while keeping what it is a file of.
fn cap_length(name: &str) -> String {
    if name.chars().count() <= MAX_NAME_CHARS {
        return name.to_string();
    }
    // Only a short trailing run counts as an extension; a name with a dot
    // three quarters of the way through has not got one.
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty() && e.chars().count() <= 10 && !e.contains(' '))
        .unwrap_or("");
    let room = MAX_NAME_CHARS - ext.chars().count() - if ext.is_empty() { 0 } else { 1 };
    let stem: String = name.chars().take(room).collect();
    if ext.is_empty() {
        stem
    } else {
        format!("{stem}.{ext}")
    }
}

/// Where a transfer is written while it is still running.
///
/// Alongside the eventual file rather than in a temp directory, so the rename
/// at the end cannot cross a filesystem, and so an abandoned one is obvious.
pub fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    target.with_file_name(name)
}

/// A path in `dir` that nothing is using yet, suffixing "(2)", "(3)"...
///
/// Receiving the same file twice should leave two of them, the way a browser's
/// download does, rather than quietly replacing the first.
///
/// A name is only free if its part file is free too. Two transfers of the same
/// name starting together would otherwise agree on a target that neither had
/// created yet, and then write into one another's part file.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let free = |p: &Path| !p.exists() && !part_path(p).exists();
    let target = dir.join(name);
    if free(&target) {
        return target;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    for n in 2..1000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if free(&candidate) {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}{ext}", std::process::id()))
}

/// Confirms a target really does sit directly inside the download directory.
///
/// Belt and braces over the naming rules above rather than the primary
/// defence: if any of this is ever wrong, it should fail here instead of
/// writing somewhere unintended. Compares the resolved directory rather than
/// the strings, so a symlinked download folder still works while `..` in a
/// name that somehow survived would not.
pub fn assert_inside(dir: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().context("target has no parent directory")?;
    let dir = dir.canonicalize().with_context(|| format!("resolving {}", dir.display()))?;
    let parent = parent.canonicalize().with_context(|| format!("resolving {}", parent.display()))?;
    if dir != parent {
        bail!("refusing to write outside the download folder");
    }
    Ok(())
}

// --- preferences ---------------------------------------------------------

/// What receiving is allowed to do, at ~/.config/nobilis/dcc.toml.
///
/// Lives with the daemon rather than the frontend because the daemon is what
/// writes the file. A download folder held in the renderer would mean a
/// network-driven write aimed by whichever window happened to ask, and the
/// point of keeping it here is that there is only one answer and it is not
/// reachable from a message.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
// Matching VoicePrefs beside it: these go to a frontend as they are, so they
// are named the way the frontend names things rather than being translated at
// every boundary.
#[serde(rename_all = "camelCase")]
pub struct DccPrefs {
    /// None means the platform's own downloads folder, which is what somebody
    /// who has never chosen wants.
    #[serde(default)]
    pub directory: Option<String>,
    /// Refuse anything claiming to be bigger, and stop anything that turns out
    /// to be. Zero means no limit, which is a choice somebody can make.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_max_transfers")]
    pub max_transfers: usize,
    /// Bytes a second, across all transfers at once. Zero means as fast as it
    /// comes, which is the right default: somebody who needs a limit knows
    /// they need one, and somebody who does not should not be throttled by a
    /// guess about their connection.
    #[serde(default)]
    pub max_rate: u64,
    /// Take offers without asking. Off, and worth keeping off: an offer is a
    /// stranger putting a file on your disk.
    #[serde(default)]
    pub auto_accept: bool,
}

fn default_max_bytes() -> u64 {
    4 * 1024 * 1024 * 1024
}

fn default_max_transfers() -> usize {
    3
}

impl Default for DccPrefs {
    fn default() -> Self {
        Self {
            directory: None,
            max_bytes: default_max_bytes(),
            max_transfers: default_max_transfers(),
            max_rate: 0,
            auto_accept: false,
        }
    }
}

impl DccPrefs {
    /// Where files land, resolved.
    pub fn download_dir(&self) -> PathBuf {
        resolve_download_dir(self.directory.as_deref(), dirs::download_dir(), dirs::home_dir())
    }
}

/// The download folder, in the order the answer should be looked for.
///
/// Whatever the platform says first: `XDG_DOWNLOAD_DIR` from user-dirs on
/// Linux, `FOLDERID_Downloads` on Windows. Somewhere of one's own only when
/// somebody has actually named one.
///
/// The last resort is the interesting part. On Linux `dirs` reports nothing at
/// all when user-dirs has no `XDG_DOWNLOAD_DIR` line, which is an ordinary
/// state for a machine that has never run a desktop's first-run setup - and
/// this one is such a machine. Falling through to the home directory there
/// drops downloaded files loose in it. The XDG user-dirs spec gives
/// `$HOME/Downloads` as the default for that entry when it is unset, so that
/// is what is used, which is also what Chromium does for the same question
/// and so what the rest of moho already resolves to.
fn resolve_download_dir(configured: Option<&str>, platform: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    configured
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or(platform)
        .or_else(|| home.map(|h| h.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod download_dir_tests {
    use super::*;

    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    #[test]
    fn somewhere_chosen_wins() {
        assert_eq!(resolve_download_dir(Some("/srv/files"), p("/home/a/Downloads"), p("/home/a")), PathBuf::from("/srv/files"));
    }

    #[test]
    fn otherwise_the_platform_answers() {
        assert_eq!(resolve_download_dir(None, p("/home/a/Downloads"), p("/home/a")), PathBuf::from("/home/a/Downloads"));
        // An empty setting is not a choice; it is the absence of one.
        assert_eq!(resolve_download_dir(Some(""), p("/home/a/Downloads"), p("/home/a")), PathBuf::from("/home/a/Downloads"));
    }

    #[test]
    fn a_machine_with_no_user_dirs_still_uses_a_downloads_folder() {
        // The case this exists for: no XDG_DOWNLOAD_DIR, so `dirs` says
        // nothing. Files must not end up loose in the home directory.
        assert_eq!(resolve_download_dir(None, None, p("/home/a")), PathBuf::from("/home/a/Downloads"));
    }
}

pub struct DccPrefsStore {
    path: PathBuf,
    inner: std::sync::Mutex<DccPrefs>,
}

impl DccPrefsStore {
    pub fn open(path: PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            // Defaults are a working configuration, and a daemon that refused
            // to start over a bad preferences file would be worse than one
            // that asks before every transfer.
            .unwrap_or_default();
        Self { path, inner: std::sync::Mutex::new(inner) }
    }

    pub fn get(&self) -> DccPrefs {
        self.inner.lock().unwrap().clone()
    }

    pub fn update(&self, edit: impl FnOnce(&mut DccPrefs)) -> DccPrefs {
        let mut prefs = self.inner.lock().unwrap();
        edit(&mut prefs);
        let out = prefs.clone();
        if let Ok(text) = toml::to_string_pretty(&out) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = self.path.with_extension("toml.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
        out
    }
}

// --- the transfer --------------------------------------------------------

/// How much to read at once. Large enough not to syscall per packet, small
/// enough that cancelling is felt immediately.
const CHUNK: usize = 64 * 1024;

/// How often to report progress.
///
/// Timed rather than counted in bytes: a byte interval reports constantly on
/// a fast transfer and almost never on a slow one, which is backwards - the
/// slow one is the one somebody is watching to see whether it is moving at
/// all. It also makes the rate below a measurement over a known interval
/// rather than over however long the last half megabyte happened to take.
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_millis(400);

/// What a transfer tells the world as it runs.
pub struct Progress {
    pub received: u64,
    pub total: u64,
    /// Bytes a second, over the last interval rather than the whole transfer:
    /// an average since the start keeps showing a healthy rate for a transfer
    /// that stalled a minute ago.
    pub rate: u64,
}

/// Holds a transfer to a set number of bytes a second.
///
/// Averaged over a window that restarts rather than over the whole transfer,
/// so a slow start cannot bank credit and then be spent as a burst - which is
/// the failure people notice, because the burst is what saturates the line
/// they set the limit to protect.
struct RateLimiter {
    rate: u64,
    window_start: std::time::Instant,
    window_bytes: u64,
}

impl RateLimiter {
    fn new(rate: u64) -> Self {
        Self { rate, window_start: std::time::Instant::now(), window_bytes: 0 }
    }

    /// How much to ask for at once.
    ///
    /// Small enough under a limit that the waits between reads stay short:
    /// one 64KB read at 32KB/s would be a two second sleep, and a cancel
    /// pressed during it would appear to do nothing.
    fn read_size(&self) -> usize {
        if self.rate == 0 {
            return CHUNK;
        }
        (self.rate / 8).clamp(4096, CHUNK as u64) as usize
    }

    async fn take(&mut self, n: u64) {
        if self.rate == 0 {
            return;
        }
        self.window_bytes += n;
        let owed = std::time::Duration::from_secs_f64(self.window_bytes as f64 / self.rate as f64);
        let spent = self.window_start.elapsed();
        if owed > spent {
            tokio::time::sleep(owed - spent).await;
        }
        if self.window_start.elapsed() >= std::time::Duration::from_secs(1) {
            self.window_start = std::time::Instant::now();
            self.window_bytes = 0;
        }
    }
}

/// Receives one offered file.
///
/// The route comes in as a `Transport` rather than being decided here, which
/// is what makes an account reached through Tor transfer through Tor: it is
/// the same transport the connection that carried this offer is using, and a
/// proxy that will not carry the transfer produces an error rather than a
/// direct connection.
///
/// Written to a `.part` file and renamed only once it is whole, so an
/// interrupted transfer never leaves something that looks like a finished
/// file. The part file is removed on every failure path.
pub async fn receive(
    offer: &DccSend,
    transport: &crate::net::tor::Transport,
    dir: &Path,
    max_bytes: u64,
    max_rate: u64,
    cancel: &std::sync::atomic::AtomicBool,
    mut on_progress: impl FnMut(Progress),
) -> Result<PathBuf> {
    if offer.passive() {
        bail!("this is a reverse (passive) offer, which moho does not accept");
    }
    if max_bytes > 0 && offer.size > max_bytes {
        bail!("offered file is {} bytes, over the {max_bytes} byte limit", offer.size);
    }

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;

    let target = unique_path(dir, &offer.file_name);
    // Checked before anything is opened, and again on the finished file.
    assert_inside(dir, &target)?;
    let part = part_path(&target);

    let result = stream_to(offer, transport, &part, max_rate, cancel, &mut on_progress).await;
    match result {
        Ok(()) => {
            tokio::fs::rename(&part, &target)
                .await
                .with_context(|| format!("moving {} into place", part.display()))?;
            assert_inside(dir, &target)?;
            Ok(target)
        }
        Err(e) => {
            // Nothing half-written is left behind to be mistaken for the file.
            let _ = tokio::fs::remove_file(&part).await;
            Err(e)
        }
    }
}

async fn stream_to(
    offer: &DccSend,
    transport: &crate::net::tor::Transport,
    part: &Path,
    max_rate: u64,
    cancel: &std::sync::atomic::AtomicBool,
    on_progress: &mut impl FnMut(Progress),
) -> Result<()> {
    // No context added here: Transport::connect already names the address and
    // the route it tried, so wrapping it printed the address twice in one
    // sentence - which is what a failed transfer showed on the downloads
    // screen, in a line too long to fit because half of it was a repeat.
    let mut stream = transport.connect(&offer.addr.to_string(), offer.port, false).await?;

    // create_new, so if the check above raced with another transfer this is
    // an error rather than two writers sharing a file.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;

    let mut limiter = RateLimiter::new(max_rate);
    let mut buf = vec![0u8; CHUNK];
    let mut received: u64 = 0;
    // Where the last progress report was taken from, which is what makes the
    // rate a measurement over an interval rather than a running average.
    let mut mark = (std::time::Instant::now(), 0u64);
    // Acknowledgements are advisory, and a sender that has stopped reading
    // them must not be written to again: on a socket the far end has closed,
    // writing provokes a reset, and a reset throws away whatever this side
    // had received but not yet read. That is a lost file, caused by being
    // polite about a byte count nobody was listening for.
    let mut acknowledge = true;

    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("cancelled");
        }
        let want = limiter.read_size();
        let n = stream.read(&mut buf[..want]).await.context("reading from the sender")?;
        if n == 0 {
            break;
        }
        received += n as u64;
        // The size was a claim; this is the check. A sender that keeps going
        // past what it offered is not going to be allowed to fill the disk.
        if received > offer.size {
            bail!("sender sent more than the {} bytes it offered", offer.size);
        }
        file.write_all(&buf[..n]).await.context("writing to disk")?;

        // The acknowledgement every DCC sender expects: how much has arrived
        // so far, big-endian. Some stall without it. Truncated to 32 bits the
        // way every other client does it, which is all the field has room for.
        if acknowledge {
            let ack = (received as u32).to_be_bytes();
            if stream.write_all(&ack).await.is_err() {
                tracing::debug!("dcc: sender is not reading acknowledgements; not sending more");
                acknowledge = false;
            }
        }

        let since = mark.0.elapsed();
        if since >= PROGRESS_EVERY || received == offer.size {
            let moved = received - mark.1;
            let rate = if since.as_secs_f64() > 0.0 { (moved as f64 / since.as_secs_f64()) as u64 } else { 0 };
            mark = (std::time::Instant::now(), received);
            on_progress(Progress { received, total: offer.size, rate });
        }

        // After the accounting, so a limited transfer still reports what it
        // moved before it waits.
        limiter.take(n as u64).await;

        // Done when what was offered has arrived, rather than when the sender
        // gets around to closing. Waiting for the close would hang against a
        // sender that politely waits for us first, and there is nothing left
        // to read in any case - anything more would be over the offer.
        if received == offer.size {
            break;
        }
    }

    file.flush().await.context("flushing to disk")?;
    if received < offer.size {
        bail!("connection ended after {received} of {} bytes", offer.size);
    }
    Ok(())
}

// --- wiring it to a connection -------------------------------------------

/// The route a transfer for this account must take.
///
/// Built from the same account config the IRC connection itself was built
/// from, which is the whole point: a server reached through a proxy has its
/// transfers reached through the same proxy, and there is no second setting
/// that could disagree with the first.
pub fn transport_for(config: &crate::accounts::IrcAccountConfig) -> crate::net::tor::Transport {
    if !config.use_tor {
        return crate::net::tor::Transport::Direct;
    }
    let host = config
        .tor_proxy
        .as_deref()
        .and_then(|p| p.split(':').next())
        .filter(|s| !s.is_empty())
        .unwrap_or("127.0.0.1")
        .to_string();
    let port = config
        .tor_proxy
        .as_deref()
        .and_then(|p| p.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or(9050);
    crate::net::tor::Transport::Socks { host, port }
}

/// Handles a DCC message that arrived on a connection.
///
/// Everything that ends in a refusal says so in the conversation it arrived
/// in. An offer that silently disappeared would be indistinguishable from one
/// that never came, and somebody waiting on a file they asked a bot for would
/// have nothing to go on.
pub async fn incoming(state: &AppState, account_id: &str, from: &str, buffer: &str, kind: &str, dcc: Dcc) {
    let offer = match dcc {
        Dcc::Unsupported(verb) => {
            note(state, account_id, buffer, kind, &format!("{from} offered a DCC {verb}, which moho does not do."));
            return;
        }
        Dcc::Send(offer) => offer,
    };

    let prefs = state.dcc_prefs.get();

    if offer.passive() {
        note(
            state,
            account_id,
            buffer,
            kind,
            &format!(
                "{from} offered \"{}\" as a reverse (passive) transfer, which moho does not accept - it would mean listening for a connection rather than making one. Ask them to send it the ordinary way.",
                offer.raw_name
            ),
        );
        return;
    }
    if prefs.max_bytes > 0 && offer.size > prefs.max_bytes {
        note(
            state,
            account_id,
            buffer,
            kind,
            &format!(
                "{from} offered \"{}\" at {}, over the {} limit in settings.",
                offer.raw_name,
                human_size(offer.size),
                human_size(prefs.max_bytes)
            ),
        );
        return;
    }
    if state.runtime.dcc_active_count() >= prefs.max_transfers {
        note(
            state,
            account_id,
            buffer,
            kind,
            &format!(
                "{from} offered \"{}\", but {} transfers are already going.",
                offer.raw_name, prefs.max_transfers
            ),
        );
        return;
    }

    let transfer = crate::runtime::DccTransfer {
        id: format!("dcc-{}", crate::model::next_message_id()),
        account_id: account_id.to_string(),
        from: from.to_string(),
        file_name: offer.file_name.clone(),
        raw_name: offer.raw_name.clone(),
        size: offer.size,
        received: 0,
        rate: 0,
        state: crate::runtime::DccState::Offered,
        path: None,
        error: None,
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        offer: Some(offer),
    };
    let Some(transfer) = state.runtime.add_dcc_offer(transfer) else {
        note(state, account_id, buffer, kind, &format!("{from} is offering more files than moho will queue up."));
        return;
    };

    announce(state, &transfer);

    if prefs.auto_accept {
        tracing::info!("dcc: accepting \"{}\" from {from} automatically", transfer.file_name);
        accept(state, &transfer.id);
    }
}

/// Starts receiving an offer that has been accepted.
///
/// Spawned rather than awaited: a transfer runs for as long as it runs, and
/// the message loop that accepted it has other messages to read.
pub fn accept(state: &AppState, id: &str) {
    let Some(transfer) = state.runtime.dcc_transfer(id) else { return };
    if transfer.state != crate::runtime::DccState::Offered {
        return;
    }
    let Some(offer) = transfer.offer.clone() else {
        fail(state, id, "the offer is no longer available");
        return;
    };
    // The route the connection that carried this offer is actually using.
    // Absent means that connection has gone, and a transfer belonging to a
    // connection that no longer exists should not quietly start on a new one.
    let Some(transport) = state.runtime.irc_transport(&transfer.account_id) else {
        fail(state, id, "that connection is no longer up");
        return;
    };
    let prefs = state.dcc_prefs.get();
    let dir = prefs.download_dir();
    let state = state.clone();
    let id = id.to_string();

    tokio::spawn(async move {
        if let Some(t) = state.runtime.update_dcc(&id, |t| t.state = crate::runtime::DccState::Receiving) {
            announce(&state, &t);
        }
        tracing::info!(
            "dcc: receiving \"{}\" from {} over {}",
            offer.file_name,
            transport.describe(),
            dir.display()
        );

        let cancel = state.runtime.dcc_transfer(&id).map(|t| t.cancel).unwrap_or_default();
        let progress_state = state.clone();
        let progress_id = id.clone();
        let result = receive(&offer, &transport, &dir, prefs.max_bytes, prefs.max_rate, &cancel, |p| {
            if let Some(t) = progress_state.runtime.update_dcc(&progress_id, |t| {
                t.received = p.received;
                t.rate = p.rate;
            }) {
                announce(&progress_state, &t);
            }
        })
        .await;

        match result {
            Ok(path) => {
                tracing::info!("dcc: saved {}", path.display());
                if let Some(t) = state.runtime.update_dcc(&id, |t| {
                    t.state = crate::runtime::DccState::Done;
                    t.received = t.size;
                    t.rate = 0;
                    t.path = Some(path.display().to_string());
                }) {
                    announce(&state, &t);
                }
            }
            Err(e) => {
                tracing::warn!("dcc: {:#}", e);
                fail(&state, &id, &format!("{e:#}"));
            }
        }
        // The address and port are of no further use, and a settled transfer
        // should not still be carrying somewhere to connect to.
        state.runtime.update_dcc(&id, |t| t.offer = None);
    });
}

/// Turns an offer down, or stops one already running.
pub fn cancel(state: &AppState, id: &str, why: &str) {
    let Some(transfer) = state.runtime.dcc_transfer(id) else { return };
    transfer.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    // A transfer already running ends through its own error path, which is
    // what removes the part file; only an unanswered offer is closed here.
    if transfer.state == crate::runtime::DccState::Offered {
        if let Some(t) = state.runtime.update_dcc(id, |t| {
            t.state = crate::runtime::DccState::Declined;
            t.error = Some(why.to_string());
            t.offer = None;
        }) {
            announce(state, &t);
        }
    }
}

fn fail(state: &AppState, id: &str, why: &str) {
    if let Some(t) = state.runtime.update_dcc(id, |t| {
        t.state = crate::runtime::DccState::Failed;
        t.error = Some(why.to_string());
        t.rate = 0;
    }) {
        announce(state, &t);
    }
}

/// Tells every window what a transfer looks like now.
pub fn announce(state: &AppState, t: &crate::runtime::DccTransfer) {
    state.events.emit("dccTransfer", transfer_json(t));
}

pub fn transfer_json(t: &crate::runtime::DccTransfer) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "accountId": t.account_id,
        "from": t.from,
        "fileName": t.file_name,
        "rawName": t.raw_name,
        "size": t.size,
        "received": t.received,
        "rate": t.rate,
        "state": t.state.as_str(),
        "path": t.path,
        "error": t.error,
    })
}

/// A line in the conversation the offer arrived in.
fn note(state: &AppState, account_id: &str, buffer: &str, kind: &str, text: &str) {
    state.runtime.record_message(
        state,
        account_id,
        buffer,
        kind,
        "",
        text,
        false,
        "system",
        None,
        None,
        false,
        None,
        Vec::new(),
        Vec::new(),
        None,
    );
}

/// Sizes as a person reads them.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    /// The whole point of the module, so it gets the widest table.
    #[test]
    fn a_name_can_never_carry_a_directory() {
        for raw in [
            "../../etc/passwd",
            "..\\..\\Windows\\System32\\drivers\\etc\\hosts",
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "C:evil.txt",
            "....//....//evil",
            "dir/sub/file.txt",
        ] {
            let safe = safe_file_name(raw);
            assert!(!safe.contains('/'), "{raw:?} -> {safe:?}");
            assert!(!safe.contains('\\'), "{raw:?} -> {safe:?}");
            assert!(!safe.contains(':'), "{raw:?} -> {safe:?}");
            assert_ne!(safe, "..", "{raw:?}");
            assert_ne!(safe, ".", "{raw:?}");
            assert!(!safe.is_empty(), "{raw:?}");
        }
    }

    #[test]
    fn the_ordinary_case_is_left_alone() {
        assert_eq!(safe_file_name("Some.Show.S01E01.mkv"), "Some.Show.S01E01.mkv");
        assert_eq!(safe_file_name("a file with spaces.txt"), "a file with spaces.txt");
        // Not everyone names files in English.
        assert_eq!(safe_file_name("日本語のファイル.zip"), "日本語のファイル.zip");
    }

    #[test]
    fn nothing_is_hidden_and_nothing_trails() {
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name("..."), "download");
        // Windows drops these itself, so they are dropped here where it can
        // still be seen happening.
        assert_eq!(safe_file_name("evil.exe."), "evil.exe");
        assert_eq!(safe_file_name("evil.exe "), "evil.exe");
        assert_eq!(safe_file_name("report.txt   "), "report.txt");
    }

    #[test]
    fn windows_devices_are_not_openable_by_accident() {
        // Reserved with any extension, which is the part that surprises people.
        for raw in ["CON", "con.txt", "NUL", "COM1", "com9.log", "LPT1.tar.gz", "AUX", "PRN"] {
            let safe = safe_file_name(raw);
            let stem = safe.split('.').next().unwrap();
            assert!(
                !RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)),
                "{raw:?} -> {safe:?} still names a device"
            );
        }
        // A name that merely starts with one is fine and must not be mangled.
        assert_eq!(safe_file_name("console.log"), "console.log");
        assert_eq!(safe_file_name("com10.txt"), "com10.txt");
    }

    #[test]
    fn control_characters_cannot_reach_a_terminal_or_a_filesystem() {
        let safe = safe_file_name("evil\u{0}\u{1b}[2Jname\n.txt");
        assert!(!safe.chars().any(|c| (c as u32) < 0x20), "{safe:?}");
        assert!(!safe.contains('\u{0}'), "{safe:?}");
    }

    #[test]
    fn an_absurd_name_is_shortened_but_still_says_what_it_is() {
        let raw = format!("{}.mkv", "a".repeat(500));
        let safe = safe_file_name(&raw);
        assert!(safe.chars().count() <= MAX_NAME_CHARS, "{}", safe.chars().count());
        assert!(safe.ends_with(".mkv"), "{safe:?}");
    }

    #[test]
    fn a_long_name_without_an_extension_is_still_shortened() {
        let safe = safe_file_name(&"b".repeat(400));
        assert!(safe.chars().count() <= MAX_NAME_CHARS);
    }

    #[test]
    fn shortening_does_not_split_a_character_in_half() {
        // Multi-byte throughout, so a byte-wise truncation would panic.
        let safe = safe_file_name(&"é".repeat(400));
        assert!(safe.chars().count() <= MAX_NAME_CHARS);
    }

    #[test]
    fn nothing_ever_comes_back_empty() {
        for raw in ["", "   ", "...", "/", "\\", "//", ".", ".."] {
            assert!(!safe_file_name(raw).is_empty(), "{raw:?}");
        }
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// A sender that hands over exactly these bytes and then closes.
    ///
    /// The only `TcpListener` in this module, and deliberately: receiving
    /// never listens, so testing it needs something that does.
    async fn serve(bytes: Vec<u8>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(&bytes).await;
                // Closes the write half so the receiver sees the end of the
                // data, while the read half stays open to absorb
                // acknowledgements until the receiver goes away. Dropping the
                // socket outright instead resets the connection and discards
                // whatever the receiver had not yet read, which is a real way
                // to lose a file and is how this test first failed.
                let _ = sock.shutdown().await;
                let mut sink = [0u8; 64];
                while matches!(sock.read(&mut sink).await, Ok(n) if n > 0) {}
            }
        });
        port
    }

    fn offer_of(port: u16, name: &str, size: u64) -> DccSend {
        DccSend {
            raw_name: name.to_string(),
            file_name: safe_file_name(name),
            addr: "127.0.0.1".parse().unwrap(),
            port,
            size,
            token: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nobilis-dcc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run(dir: &Path, offer: &DccSend, max: u64) -> Result<PathBuf> {
        let cancel = AtomicBool::new(false);
        receive(offer, &crate::net::tor::Transport::Direct, dir, max, 0, &cancel, |_| {}).await
    }

    #[tokio::test]
    async fn a_whole_file_arrives_and_is_renamed_into_place() {
        let dir = temp_dir("whole");
        let body = vec![7u8; 200_000];
        let port = serve(body.clone()).await;
        let path = run(&dir, &offer_of(port, "thing.bin", body.len() as u64), 0).await.expect("should arrive");

        assert_eq!(path, dir.join("thing.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), body);
        // Nothing half-finished is left beside it.
        assert!(!part_path(&path).exists(), "the part file should be gone");
    }

    #[tokio::test]
    async fn a_sender_that_overruns_its_offer_is_cut_off() {
        let dir = temp_dir("overrun");
        // Offers 10 bytes, sends 100_000. The size was a claim; this is what
        // stops the claim being the limit.
        let port = serve(vec![1u8; 100_000]).await;
        let err = run(&dir, &offer_of(port, "liar.bin", 10), 0).await.unwrap_err();

        assert!(format!("{err:#}").contains("more than"), "{err:#}");
        assert!(!dir.join("liar.bin").exists(), "nothing should have been kept");
        assert!(!part_path(&dir.join("liar.bin")).exists(), "no part file should survive");
    }

    #[tokio::test]
    async fn a_transfer_that_stops_early_leaves_nothing_behind() {
        let dir = temp_dir("short");
        let port = serve(vec![2u8; 50]).await;
        let err = run(&dir, &offer_of(port, "short.bin", 5_000), 0).await.unwrap_err();

        assert!(format!("{err:#}").contains("ended after"), "{err:#}");
        // The important half: a partial file must not be left looking whole.
        assert!(!dir.join("short.bin").exists());
        assert!(!part_path(&dir.join("short.bin")).exists());
    }

    #[tokio::test]
    async fn too_big_is_refused_without_connecting() {
        let dir = temp_dir("toobig");
        // Port 1 would refuse the connection, so reaching the size check is
        // the only way this can pass.
        let err = run(&dir, &offer_of(1, "huge.bin", 10_000), 1_000).await.unwrap_err();
        assert!(format!("{err:#}").contains("over the"), "{err:#}");
    }

    #[tokio::test]
    async fn a_reverse_offer_is_refused() {
        let dir = temp_dir("passive");
        let mut offer = offer_of(0, "f.bin", 10);
        offer.token = Some("123".into());
        let err = run(&dir, &offer, 0).await.unwrap_err();
        assert!(format!("{err:#}").contains("reverse"), "{err:#}");
    }

    #[tokio::test]
    async fn a_rate_limit_actually_slows_it_down() {
        let dir = temp_dir("rate");
        // 40KB at 20KB/s is about two seconds; unlimited it is instant over
        // loopback. Asserting only the floor, because a machine under load
        // can always be slower and a test that fails for that is worthless.
        let body = vec![4u8; 40 * 1024];
        let port = serve(body.clone()).await;
        let cancel = AtomicBool::new(false);

        let began = std::time::Instant::now();
        let offer = offer_of(port, "slow.bin", body.len() as u64);
        receive(&offer, &crate::net::tor::Transport::Direct, &dir, 0, 20 * 1024, &cancel, |_| {})
            .await
            .expect("should still arrive");
        let took = began.elapsed();

        assert!(took >= std::time::Duration::from_millis(900), "took {took:?}, so the limit did nothing");
        assert_eq!(std::fs::read(dir.join("slow.bin")).unwrap().len(), body.len());
    }

    #[tokio::test]
    async fn progress_reports_a_rate() {
        let dir = temp_dir("progress");
        let body = vec![5u8; 60 * 1024];
        let port = serve(body.clone()).await;
        let cancel = AtomicBool::new(false);
        let seen = std::sync::Mutex::new(Vec::new());

        let offer = offer_of(port, "measured.bin", body.len() as u64);
        receive(&offer, &crate::net::tor::Transport::Direct, &dir, 0, 16 * 1024, &cancel, |p| {
            seen.lock().unwrap().push((p.received, p.rate));
        })
        .await
        .unwrap();

        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "a transfer should report progress at least once");
        // The last report is the whole file, which is what a finished bar
        // needs in order to reach the end.
        assert_eq!(seen.last().unwrap().0, body.len() as u64);
        assert!(seen.iter().any(|(_, rate)| *rate > 0), "no report carried a rate: {seen:?}");
    }

    #[tokio::test]
    async fn the_same_file_twice_leaves_two_of_them() {
        let dir = temp_dir("twice");
        let body = vec![3u8; 64];

        let port = serve(body.clone()).await;
        let first = run(&dir, &offer_of(port, "dup.bin", 64), 0).await.unwrap();
        let port = serve(body.clone()).await;
        let second = run(&dir, &offer_of(port, "dup.bin", 64), 0).await.unwrap();

        assert_eq!(first, dir.join("dup.bin"));
        assert_eq!(second, dir.join("dup (2).bin"), "the first must not be replaced");
        assert!(first.exists() && second.exists());
    }

    #[tokio::test]
    async fn a_hostile_name_lands_in_the_download_folder_and_nowhere_else() {
        let dir = temp_dir("traversal");
        let body = vec![9u8; 32];
        let port = serve(body).await;
        let path = run(&dir, &offer_of(port, "../../../../tmp/escaped.bin", 32), 0).await.unwrap();

        assert_eq!(path.parent().unwrap(), dir, "it must sit directly in the download folder");
        assert_eq!(path.file_name().unwrap(), "escaped.bin");
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn send(body: &str) -> DccSend {
        match parse_dcc(body) {
            Some(Dcc::Send(s)) => s,
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    #[test]
    fn an_ordinary_offer() {
        let s = send("\u{1}DCC SEND file.zip 3232235777 5000 1234\u{1}");
        assert_eq!(s.file_name, "file.zip");
        assert_eq!(s.addr, "192.168.1.1".parse::<IpAddr>().unwrap());
        assert_eq!(s.port, 5000);
        assert_eq!(s.size, 1234);
        assert!(!s.passive());
    }

    #[test]
    fn a_quoted_name_keeps_its_spaces() {
        let s = send("\u{1}DCC SEND \"my great file.mkv\" 3232235777 5000 99\u{1}");
        assert_eq!(s.file_name, "my great file.mkv");
    }

    #[test]
    fn an_unquoted_name_with_spaces_is_still_recovered() {
        let s = send("\u{1}DCC SEND my great file.mkv 3232235777 5000 99\u{1}");
        assert_eq!(s.file_name, "my great file.mkv");
    }

    #[test]
    fn a_dotted_quad_and_an_ipv6_literal_both_read() {
        assert_eq!(send("\u{1}DCC SEND f 203.0.113.5 1 2\u{1}").addr, "203.0.113.5".parse::<IpAddr>().unwrap());
        assert_eq!(send("\u{1}DCC SEND f 2001:db8::1 1 2\u{1}").addr, "2001:db8::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn a_missing_trailing_delimiter_is_tolerated() {
        // Real clients omit it often enough that refusing would lose files.
        assert_eq!(send("\u{1}DCC SEND f.bin 3232235777 5000 1").file_name, "f.bin");
    }

    #[test]
    fn a_passive_offer_is_recognised_as_one() {
        let s = send("\u{1}DCC SEND f.bin 3232235777 0 100 812345\u{1}");
        assert!(s.passive(), "port 0 with a token is a reverse offer");
    }

    #[test]
    fn an_address_pointing_back_at_us_is_refused() {
        for body in [
            "\u{1}DCC SEND f 2130706433 5000 1\u{1}", // 127.0.0.1
            "\u{1}DCC SEND f 127.0.0.1 5000 1\u{1}",
            "\u{1}DCC SEND f 0 5000 1\u{1}",
            "\u{1}DCC SEND f 2851995649 5000 1\u{1}", // 169.254.0.1
            "\u{1}DCC SEND f ::1 5000 1\u{1}",
        ] {
            assert!(parse_dcc(body).is_none(), "{body:?} should not parse into an offer");
        }
    }

    #[test]
    fn a_name_carrying_a_path_is_safe_by_the_time_it_is_an_offer() {
        let s = send("\u{1}DCC SEND ../../.ssh/authorized_keys 3232235777 5000 1\u{1}");
        assert_eq!(s.file_name, "authorized_keys");
        // The original is kept, but only to show.
        assert_eq!(s.raw_name, "../../.ssh/authorized_keys");
    }

    #[test]
    fn other_dcc_verbs_are_named_rather_than_shown_as_control_codes() {
        assert_eq!(parse_dcc("\u{1}DCC CHAT chat 1 2\u{1}"), Some(Dcc::Unsupported("CHAT".into())));
        assert_eq!(parse_dcc("\u{1}DCC RESUME f 5000 100\u{1}"), Some(Dcc::Unsupported("RESUME".into())));
    }

    #[test]
    fn things_that_are_not_dcc_are_left_alone() {
        for body in [
            "hello",
            "\u{1}ACTION waves\u{1}",
            "\u{1}VERSION\u{1}",
            "\u{1}DCC\u{1}",
            "\u{1}DCC SEND\u{1}",
            "\u{1}DCC SEND onlyname\u{1}",
            "\u{1}DCC SEND f notanaddress 5000 1\u{1}",
            "\u{1}DCC SEND f 3232235777 notaport 1\u{1}",
            "\u{1}DCC SEND f 3232235777 5000 notasize\u{1}",
            "\u{1}DCC SEND f 3232235777 99999 1\u{1}",
        ] {
            assert!(
                !matches!(parse_dcc(body), Some(Dcc::Send(_))),
                "{body:?} must not be read as an offer"
            );
        }
    }
}
