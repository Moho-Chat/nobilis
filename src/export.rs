//! A conversation written to disk as the folder of HTML it looks like here.
//!
//! ## Who does what
//!
//! The window renders and this writes. That split is not arbitrary: the HTML a
//! message becomes is produced by `src/renderer/src/lib/format.ts`, seven
//! hundred-odd lines covering markdown, BBCode, spoilers, code blocks, custom
//! emoji, nick colours and IRC control codes, and an export is supposed to be
//! what was on screen rather than a second opinion about it. Rewriting that
//! here would be two formatters that agree until they do not, and the one that
//! is wrong would be the one nobody looks at until months later.
//!
//! So the window drives the loop - it asks for messages, renders them, and
//! hands the HTML over a page at a time - and everything that is not
//! formatting lives here: the folder, the media, the pacing, the progress and
//! the ways to stop.
//!
//! ## Why it is a transfer
//!
//! An export appears in the downloads panel beside DCC files, because it is
//! the same kind of thing to the person waiting for it: a long job with a
//! size, a rate, and a way to call it off. Reusing the transfer row means the
//! panel, the progress, the rate, the persistence across a restart and the
//! cancel path all already exist and already agree with each other. The `kind`
//! on the row is what lets the panel say "export" where it would otherwise say
//! a filename.
//!
//! ## What lands on disk
//!
//! A folder, not a file, because the media comes too. `index.html` is written
//! last, wrapping the body that has been accumulating beside it - so a folder
//! with no `index.html` in it is an export that did not finish, which is a
//! more useful thing to find than a truncated page that looks complete.

use crate::state::AppState;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// How long to wait between fetching one piece of media and the next.
///
/// A conversation's worth of pictures is a burst of requests at one host, and
/// a host that decides moho is a scraper is a host that stops answering for
/// everybody - including the running client, since this shares its address.
/// Slower than it could be, on purpose: an export is something somebody
/// started and is content to wait for.
const MEDIA_PACE: std::time::Duration = std::time::Duration::from_millis(250);

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"), " (nobilis)"))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Turns anything into something safe to be a directory or file name.
///
/// A conversation is named by the network, and `#channel`, a Matrix room with
/// a slash in it and a Windows reserved name are all things somebody can be
/// sitting in. Nothing off the network decides where a file lands.
pub fn safe_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches(['-', '.']).to_string();
    let trimmed = if trimmed.is_empty() { "conversation".to_string() } else { trimmed };
    // Windows will not have these whatever the extension, and an export folder
    // is meant to survive being copied onto a memory stick.
    const RESERVED: [&str; 9] = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "LPT1", "LPT2", "LPT3"];
    let upper = trimmed.to_ascii_uppercase();
    let safe = if RESERVED.contains(&upper.as_str()) { format!("{trimmed}-") } else { trimmed };
    safe.chars().take(80).collect()
}

/// Where an export's pieces live while it is being written.
pub struct Paths {
    pub root: PathBuf,
    pub media: PathBuf,
    pub body: PathBuf,
    pub index: PathBuf,
}

pub fn paths(root: &Path) -> Paths {
    Paths {
        root: root.to_path_buf(),
        media: root.join("media"),
        // Accumulated separately from index.html so that an unfinished export
        // has no index.html at all, rather than half a page that looks whole.
        body: root.join(".body.html.part"),
        index: root.join("index.html"),
    }
}

/// Picks a folder inside `parent` that is not already taken.
///
/// A second export of the same conversation on the same day is an ordinary
/// thing to do, and silently writing into the first one would destroy it.
fn free_folder(parent: &Path, base: &str) -> PathBuf {
    let first = parent.join(base);
    if !first.exists() {
        return first;
    }
    for n in 2..1000 {
        let candidate = parent.join(format!("{base}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{base}-{}", crate::model::next_message_id()))
}

/// Starts one, and puts it in the downloads panel.
///
/// `parent` is where the window says downloads go - resolved there because
/// that is where the platform's own answer lives, and where somebody may have
/// chosen differently.
pub fn begin(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    title: &str,
    parent: &str,
) -> Result<serde_json::Value> {
    let parent = PathBuf::from(parent);
    if !parent.is_dir() {
        bail!("{} is not a folder", parent.display());
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d").to_string();
    let root = free_folder(&parent, &format!("{}-{stamp}", safe_name(title)));
    let p = paths(&root);
    std::fs::create_dir_all(&p.media).with_context(|| format!("making {}", p.media.display()))?;
    std::fs::write(&p.body, "").with_context(|| format!("making {}", p.body.display()))?;

    let id = format!("export-{}", crate::model::next_message_id());
    state.runtime.push_dcc_transfer(crate::runtime::DccTransfer {
        id: id.clone(),
        account_id: account_id.to_string(),
        outgoing: false,
        from: title.to_string(),
        file_name: root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        raw_name: buffer_id.to_string(),
        // Unknown until the range has been walked. The panel reads a zero size
        // as "no total yet" and shows what has been done instead of a
        // percentage of nothing.
        size: 0,
        received: 0,
        rate: 0,
        state: crate::runtime::DccState::Receiving,
        path: Some(root.to_string_lossy().into_owned()),
        error: None,
        kind: crate::runtime::TransferKind::Export,
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        offer: None,
        started_at: crate::backend::irc::dcc::now_seconds(),
    });
    if let Some(t) = state.runtime.dcc_transfer(&id) {
        crate::backend::irc::dcc::announce(state, &t);
    }
    Ok(serde_json::json!({ "id": id, "path": root.to_string_lossy() }))
}

/// Whether the window should keep going, and why not if it should stop.
///
/// Read rather than enforced: the loop lives in the window, so this is how it
/// is told. A cancelled export answers "stop" once and then has nothing more
/// to say - the folder is left where it is, since a half-finished export is
/// sometimes what somebody wanted when they pressed cancel.
pub fn status(state: &AppState, id: &str) -> Result<serde_json::Value> {
    let Some(t) = state.runtime.dcc_transfer(id) else { bail!("no such export") };
    Ok(serde_json::json!({
        "cancelled": t.cancel.load(Ordering::Relaxed),
        "paused": t.paused.load(Ordering::Relaxed),
        "done": matches!(t.state, crate::runtime::DccState::Done | crate::runtime::DccState::Failed),
    }))
}

/// Takes one page of rendered conversation.
///
/// `done` and `total` are what the window knows about its own progress - it is
/// the one walking the range, so it is the one that can say. Appending rather
/// than holding it all in memory because a year of a busy channel is not
/// something to keep twice.
pub fn append(state: &AppState, id: &str, html: &str, done: u64, total: u64) -> Result<()> {
    let Some(t) = state.runtime.dcc_transfer(id) else { bail!("no such export") };
    if t.cancel.load(Ordering::Relaxed) {
        bail!("export cancelled");
    }
    let p = paths(Path::new(t.path.as_deref().unwrap_or_default()));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&p.body).with_context(|| format!("writing {}", p.body.display()))?;
    f.write_all(html.as_bytes()).context("writing a page of the export")?;

    if let Some(t) = state.runtime.update_dcc(id, |t| {
        t.received = done;
        t.size = total;
    }) {
        crate::backend::irc::dcc::announce(state, &t);
    }
    Ok(())
}

/// Fetches one piece of media into the export's folder.
///
/// Answers with the path to write into the page - the local one where the
/// fetch worked, and the original URL where it did not. A picture that cannot
/// be had should leave a page that still reads, not a broken export.
pub async fn fetch_media(state: &AppState, id: &str, url: &str) -> Result<serde_json::Value> {
    let Some(t) = state.runtime.dcc_transfer(id) else { bail!("no such export") };
    if t.cancel.load(Ordering::Relaxed) {
        bail!("export cancelled");
    }
    let p = paths(Path::new(t.path.as_deref().unwrap_or_default()));

    // Named by what it is rather than by what it was called, so two files
    // called image.png from two hosts do not become one.
    let digest = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut h);
        h.finish()
    };
    let ext = url
        .rsplit('/')
        .next()
        .and_then(|tail| tail.split(['?', '#']).next())
        .and_then(|name| name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()))
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".to_string());
    let name = format!("{digest:016x}.{ext}");
    let dest = p.media.join(&name);
    let relative = format!("media/{name}");

    // Already fetched, because the same picture posted twice is one file.
    if dest.exists() {
        return Ok(serde_json::json!({ "path": relative, "cached": true }));
    }

    tokio::time::sleep(MEDIA_PACE).await;

    // Streamed to disk a chunk at a time rather than read into memory and
    // written out. There is no size limit here on purpose - a conversation's
    // attachments are the conversation, and an export that silently left the
    // big ones behind would be a worse lie than one that took a while - so the
    // whole-file-in-memory shape was not survivable: a 2GB video would have
    // meant 2GB of resident daemon.
    //
    // Written to a `.part` beside the destination and renamed only once the
    // body has ended. Without that, an export interrupted mid-file leaves a
    // truncated one at the real name, and the `dest.exists()` check above
    // would treat it as already fetched forever after - a broken picture that
    // never repairs itself.
    let part = dest.with_extension(format!("{ext}.part"));
    let mut response = match http_client().get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!("export: {url} answered {}", r.status());
            return Ok(serde_json::json!({ "path": url, "skipped": "could not be fetched" }));
        }
        Err(e) => {
            tracing::debug!("export: fetching {url}: {e}");
            return Ok(serde_json::json!({ "path": url, "skipped": "could not be fetched" }));
        }
    };

    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(&part).await.with_context(|| format!("writing {}", part.display()))?;
    let mut written: u64 = 0;
    loop {
        // Checked as it goes rather than only between files: with no cap, one
        // file can be the whole of a long download, and a cancel that only
        // took effect at the end of it would not feel like a cancel.
        if t.cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            bail!("export cancelled");
        }
        match response.chunk().await {
            Ok(Some(chunk)) => {
                file.write_all(&chunk).await.with_context(|| format!("writing {}", part.display()))?;
                written += chunk.len() as u64;
            }
            Ok(None) => break,
            Err(e) => {
                // A body that stopped early leaves nothing behind: half a
                // video under the name of a whole one is worse than a link.
                tracing::debug!("export: {url} stopped early after {written} bytes: {e}");
                drop(file);
                let _ = tokio::fs::remove_file(&part).await;
                return Ok(serde_json::json!({ "path": url, "skipped": "the download stopped early" }));
            }
        }
    }
    file.flush().await.ok();
    drop(file);
    tokio::fs::rename(&part, &dest).await.with_context(|| format!("renaming {}", part.display()))?;
    Ok(serde_json::json!({ "path": relative, "bytes": written }))
}

/// Seals it: wraps the accumulated body in a page and writes `index.html`.
pub fn finish(state: &AppState, id: &str, title: &str, subtitle: &str) -> Result<serde_json::Value> {
    let Some(t) = state.runtime.dcc_transfer(id) else { bail!("no such export") };
    let p = paths(Path::new(t.path.as_deref().unwrap_or_default()));
    let body = std::fs::read_to_string(&p.body).unwrap_or_default();
    let page = page(title, subtitle, &body);
    std::fs::write(&p.index, page).with_context(|| format!("writing {}", p.index.display()))?;
    let _ = std::fs::remove_file(&p.body);

    if let Some(t) = state.runtime.update_dcc(id, |t| {
        t.state = crate::runtime::DccState::Done;
        t.path = Some(p.index.to_string_lossy().into_owned());
    }) {
        crate::backend::irc::dcc::announce(state, &t);
    }
    Ok(serde_json::json!({ "path": p.index.to_string_lossy() }))
}

/// Gives up on one, with a reason somebody can act on.
pub fn fail(state: &AppState, id: &str, why: &str) {
    if let Some(t) = state.runtime.update_dcc(id, |t| {
        t.state = crate::runtime::DccState::Failed;
        t.error = Some(why.to_string());
    }) {
        crate::backend::irc::dcc::announce(state, &t);
    }
}

/// Puts one down or picks it back up.
pub fn set_paused(state: &AppState, id: &str, paused: bool) -> Result<()> {
    let Some(t) = state.runtime.dcc_transfer(id) else { bail!("no such export") };
    t.paused.store(paused, Ordering::Relaxed);
    if let Some(t) = state.runtime.update_dcc(id, |t| {
        t.state = if paused { crate::runtime::DccState::Paused } else { crate::runtime::DccState::Receiving };
    }) {
        crate::backend::irc::dcc::announce(state, &t);
    }
    Ok(())
}

/// The document the pages are poured into.
///
/// Deliberately one file with the styling inside it. An export is something
/// somebody keeps, mails to a lawyer, or opens in five years - a folder whose
/// page depends on a stylesheet that has to be found beside it is a folder one
/// careless copy away from being unreadable. The media is separate because it
/// has to be; nothing else is.
fn page(title: &str, subtitle: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  :root {{
    color-scheme: light dark;
    --bg: #ffffff; --fg: #1a1a1a; --dim: #6a6a6a; --line: #e3e3e3;
    --quote: #f4f4f5; --code: #f0f0f1;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{ --bg: #16171a; --fg: #e6e6e6; --dim: #9a9a9a; --line: #2c2e33;
             --quote: #1e2024; --code: #1e2024; }}
  }}
  body {{ margin: 0; background: var(--bg); color: var(--fg);
          font: 14px/1.5 system-ui, -apple-system, Segoe UI, sans-serif; }}
  header {{ padding: 20px 24px; border-bottom: 1px solid var(--line); }}
  h1 {{ margin: 0 0 4px; font-size: 18px; }}
  .sub {{ color: var(--dim); font-size: 13px; }}
  main {{ padding: 16px 24px 48px; }}
  .msg {{ display: flex; gap: 10px; padding: 3px 0; }}
  .msg time {{ color: var(--dim); font-variant-numeric: tabular-nums;
               flex: none; font-size: 12px; padding-top: 2px; }}
  .msg .who {{ font-weight: 600; flex: none; }}
  .msg .what {{ min-width: 0; overflow-wrap: anywhere; }}
  .msg.system {{ color: var(--dim); font-style: italic; }}
  blockquote {{ margin: 4px 0; padding: 4px 10px; background: var(--quote);
                border-left: 3px solid var(--line); }}
  pre, code {{ background: var(--code); border-radius: 4px; }}
  pre {{ padding: 8px 10px; overflow-x: auto; }}
  code {{ padding: 1px 4px; }}
  img, video {{ max-width: min(100%, 480px); height: auto; border-radius: 6px; }}
  a {{ color: inherit; }}
</style>
</head>
<body>
<header><h1>{title}</h1><div class="sub">{subtitle}</div></header>
<main>
{body}
</main>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing off the network decides where a file lands.
    #[test]
    fn a_conversations_name_is_made_safe_to_be_a_folder() {
        assert_eq!(safe_name("#general"), "general");
        assert_eq!(safe_name("!abc:matrix.org"), "abc-matrix.org");
        assert_eq!(safe_name("../../etc/passwd"), "etc-passwd");
        assert_eq!(safe_name("a/b\\c"), "a-b-c");
    }

    /// A name that is nothing but punctuation still has to be something.
    #[test]
    fn a_name_that_survives_as_nothing_gets_a_name() {
        assert_eq!(safe_name("###"), "conversation");
        assert_eq!(safe_name(""), "conversation");
        assert_eq!(safe_name("..."), "conversation");
    }

    /// Windows will not have these, and an export is meant to survive being
    /// copied onto a memory stick.
    #[test]
    fn a_reserved_windows_name_is_not_used_bare() {
        assert_eq!(safe_name("CON"), "CON-");
        assert_eq!(safe_name("nul"), "nul-");
        assert_eq!(safe_name("console"), "console");
    }

    #[test]
    fn a_very_long_name_is_cut_to_something_a_filesystem_will_take() {
        assert_eq!(safe_name(&"a".repeat(300)).len(), 80);
    }

    /// An unfinished export has no index.html, so what is found on disk says
    /// whether it finished.
    #[test]
    fn the_page_is_written_somewhere_the_body_is_not() {
        let p = paths(Path::new("/tmp/x"));
        assert_ne!(p.body, p.index);
        assert!(p.index.ends_with("index.html"));
        assert!(p.media.ends_with("media"));
    }

    #[test]
    fn the_page_carries_its_own_styling() {
        let html = page("#general", "1 March - 2 April", "<div class=\"msg\"></div>");
        assert!(html.contains("<style>"));
        assert!(html.contains("#general"));
        assert!(html.contains("1 March - 2 April"));
        assert!(html.contains("class=\"msg\""));
        // Nothing to fetch for the page itself to render.
        assert!(!html.contains("<link"));
    }
}
