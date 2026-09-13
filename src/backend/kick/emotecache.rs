//! Kick emotes, kept at the size they are actually drawn.
//!
//! Kick serves one variant of an emote and it is the big one: `fullsize` is
//! typically 500x500, and the obvious smaller spellings are a 403 (see
//! `api::emote_url`). A picker cell draws it at 26 pixels and a message draws
//! it smaller still, so every emote on screen is decoded at something like
//! four hundred times the area it occupies.
//!
//! That is worse than it sounds, because they are animated. A 500x500 emote of
//! 35 frames is 33MB of decoded frame buffers for one picture of a cat. A few
//! dozen of them on screen is most of a gigabyte, which is what issue #211
//! measured: the picker alone cost +311MB of renderer memory before its grid
//! was windowed, and +146MB after - the remainder being exactly this.
//!
//! So each emote is fetched once, shrunk to something proportionate, and kept.
//! A 500x500x35 emote becomes 64x64x35: 33MB of decoded frames becomes 0.55MB,
//! and the file on disk goes from 1024KB to 77KB. The animation is preserved,
//! because it is the point of the emote.
//!
//! Shrinking rather than asking for a smaller URL, because there is no smaller
//! URL to ask for. Discord is the comparison: its emoji have a size parameter,
//! so its path never needed any of this.

use anyhow::{bail, Context, Result};
use std::io::Cursor;
use std::path::PathBuf;

/// The longest edge a cached emote is stored at.
///
/// 64 rather than the 26 a cell actually draws: the same file is used by the
/// message list and the picker, it has to survive a HiDPI scale factor without
/// going soft, and at this size the saving is already two orders of magnitude.
pub const EMOTE_PX: u32 = 64;

/// Cap on the emote cache, swept like every other one here.
///
/// Generous next to what it holds - a shrunk emote is tens of kilobytes, so
/// this is thousands of them - because re-fetching means going back to Kick
/// for a megabyte to rebuild something that was 77KB.
pub const EMOTE_CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;

pub fn emote_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("kick-emotes")
}

pub async fn sweep_emote_cache() {
    crate::backend::sneedchat::sweep_cache_dir(&emote_cache_dir(), EMOTE_CACHE_MAX_BYTES, "kick emote").await;
}

/// Whether these bytes are a GIF, by its magic number.
///
/// The format decides the extension, and the extension has to be known before
/// the file is looked up rather than after it is fetched - so a cached emote is
/// looked for under both spellings. Kick serves GIF for everything seen so far;
/// the other branch exists so that something else arriving is a smaller
/// picture rather than a broken one.
fn is_gif(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
}

/// The size to draw into, longest edge `px`, aspect preserved, never zero.
fn fit(w: u32, h: u32, px: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (px, px);
    }
    if w <= px && h <= px {
        return (w, h);
    }
    if w >= h {
        (px, ((h as u64 * px as u64) / w as u64).max(1) as u32)
    } else {
        ((((w as u64 * px as u64) / h as u64).max(1) as u32), px)
    }
}

/// Shrink an animated GIF, keeping every frame and its delay.
///
/// `into_frames` hands back frames already composited onto the full canvas, so
/// each one can be resized on its own without tracking disposal methods - which
/// is the part of GIF that is easy to get subtly wrong.
fn shrink_gif(bytes: &[u8], px: u32) -> Result<Vec<u8>> {
    use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
    use image::AnimationDecoder;

    let decoder = GifDecoder::new(Cursor::new(bytes)).context("reading the emote")?;
    let frames = decoder.into_frames().collect_frames().context("decoding the emote's frames")?;
    if frames.is_empty() {
        bail!("emote had no frames");
    }

    let mut out = Vec::new();
    {
        // Speed is the quantiser's, not ours: at 1 a 35-frame emote takes long
        // enough to notice, and the difference at this size is not visible on a
        // 64-pixel square.
        let mut encoder = GifEncoder::new_with_speed(&mut out, 10);
        encoder.set_repeat(Repeat::Infinite).context("setting the emote to loop")?;
        for frame in frames {
            let delay = frame.delay();
            let buffer = frame.into_buffer();
            let (w, h) = fit(buffer.width(), buffer.height(), px);
            let small = image::imageops::resize(&buffer, w, h, image::imageops::FilterType::Lanczos3);
            encoder
                .encode_frame(image::Frame::from_parts(small, 0, 0, delay))
                .context("writing a shrunk frame")?;
        }
    }
    Ok(out)
}

/// Shrink a still picture, whatever format it arrived in, to a PNG.
fn shrink_still(bytes: &[u8], px: u32) -> Result<Vec<u8>> {
    let image = image::load_from_memory(bytes).context("reading the emote")?;
    let (w, h) = fit(image.width(), image.height(), px);
    let small = image.resize(w, h, image::imageops::FilterType::Lanczos3);
    let mut out = Vec::new();
    small.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).context("writing the shrunk emote")?;
    Ok(out)
}

/// The shrunk bytes and the extension they should be stored under.
pub fn shrink(bytes: &[u8], px: u32) -> Result<(Vec<u8>, &'static str)> {
    if is_gif(bytes) {
        // A GIF that will not decode as an animation is still a picture, so
        // fall through rather than give up on it.
        match shrink_gif(bytes, px) {
            Ok(out) => return Ok((out, "gif")),
            Err(e) => tracing::debug!("kick: emote would not shrink as an animation ({e}), trying it as a still"),
        }
    }
    Ok((shrink_still(bytes, px)?, "png"))
}

/// A local, small copy of one emote, fetching and shrinking it if this is the
/// first time it has been asked for.
///
/// `None` rather than an error on every failure path: a missing emote should
/// leave the caller free to fall back to Kick's own URL, which still draws the
/// right picture - just an expensive one.
pub async fn local_copy(http: &reqwest::Client, id: &str) -> Option<String> {
    let dir = emote_cache_dir();
    for ext in ["gif", "png"] {
        let path = dir.join(format!("{id}.{ext}"));
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Some(format!("file://{}", path.display()));
        }
    }

    let url = super::api::emote_url(id);
    let fetch = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let res = http.get(&url).send().await?;
        let status = res.status();
        let bytes = res.bytes().await?;
        Ok::<_, reqwest::Error>((status, bytes))
    })
    .await;
    let (status, bytes) = match fetch {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::debug!("kick: fetching emote {id} failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::debug!("kick: fetching emote {id} timed out");
            return None;
        }
    };
    if !status.is_success() || bytes.is_empty() {
        tracing::debug!("kick: fetching emote {id} returned HTTP {status}");
        return None;
    }

    // Off the runtime: resizing thirty-five frames is the one genuinely
    // CPU-bound thing this daemon does, and doing it inline would stall
    // whichever worker thread picked it up for a tenth of a second per emote.
    let raw = bytes.to_vec();
    let shrunk = tokio::task::spawn_blocking(move || shrink(&raw, EMOTE_PX)).await;
    let (small, ext) = match shrunk {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::debug!("kick: shrinking emote {id} failed: {e}");
            return None;
        }
        Err(e) => {
            tracing::debug!("kick: shrinking emote {id} panicked: {e}");
            return None;
        }
    };

    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return None;
    }
    // Written aside and renamed, so a reader never finds a half-written emote:
    // the name appearing is the whole file appearing.
    let path = dir.join(format!("{id}.{ext}"));
    let part = dir.join(format!("{id}.{ext}.part"));
    if tokio::fs::write(&part, &small).await.is_err() {
        return None;
    }
    if tokio::fs::rename(&part, &path).await.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
        return None;
    }
    Some(format!("file://{}", path.display()))
}

/// One client for every emote fetch, built once.
///
/// Kick filters on user agent (see `api::client`), so this borrows that
/// builder rather than reaching for a default one - a plain `reqwest/0.12`
/// gets a 403 and the emote silently stays big.
fn http() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| match super::api::client() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("kick: no HTTP client for emotes: {e}");
                None
            }
        })
        .as_ref()
}

/// How many emotes to fetch at once.
///
/// A handful rather than all of them: a picker opening on a cold cache asks
/// for a screenful at once, and answering that with seventy simultaneous
/// requests to one host is how a client gets rate-limited.
const FETCH_AT_ONCE: usize = 6;

/// Everything already shrunk and on disk, by emote id.
///
/// Read from the directory rather than from a table in memory, so it survives
/// a restart without anything having to be rebuilt or remembered.
pub async fn cached_map() -> std::collections::BTreeMap<String, String> {
    let dir = emote_cache_dir();
    let mut out = std::collections::BTreeMap::new();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else { return out };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        // A half-written file is not a cached emote yet.
        if name.ends_with(".part") {
            continue;
        }
        let Some((id, _ext)) = name.rsplit_once('.') else { continue };
        out.insert(id.to_string(), format!("file://{}", path.display()));
    }
    out
}

/// Local copies for these emotes, fetching and shrinking whatever is missing.
///
/// An empty list means "just tell me what you already have", which is how a
/// client fills in its map at startup: from then on it can rewrite an emote's
/// URL without waiting for anything, and the big original is never requested.
pub async fn resolve(ids: &[String]) -> std::collections::BTreeMap<String, String> {
    let have = cached_map().await;
    if ids.is_empty() {
        return have;
    }

    let mut out = std::collections::BTreeMap::new();
    let mut wanted = Vec::new();
    for id in ids {
        match have.get(id) {
            Some(url) => {
                out.insert(id.clone(), url.clone());
            }
            None => wanted.push(id.clone()),
        }
    }

    let Some(client) = http() else { return out };
    for chunk in wanted.chunks(FETCH_AT_ONCE) {
        let mut set = tokio::task::JoinSet::new();
        for id in chunk {
            let id = id.clone();
            let client = client.clone();
            set.spawn(async move { (id.clone(), local_copy(&client, &id).await) });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((id, Some(url))) = joined {
                out.insert(id, url);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-frame animated GIF, built here rather than checked in, so the
    /// test says what it is testing.
    fn animated_gif(w: u32, h: u32, frames: usize) -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for i in 0..frames {
                let shade = (i * 60) as u8;
                let buf = image::RgbaImage::from_pixel(w, h, image::Rgba([shade, 40, 200, 255]));
                enc.encode_frame(image::Frame::from_parts(buf, 0, 0, image::Delay::from_numer_denom_ms(100, 1)))
                    .unwrap();
            }
        }
        out
    }

    #[test]
    fn an_animated_emote_keeps_every_frame_and_gets_smaller() {
        use image::AnimationDecoder;
        let big = animated_gif(500, 500, 4);
        let (small, ext) = shrink(&big, 64).expect("should shrink");
        assert_eq!(ext, "gif", "an animated emote stays animated");

        let decoded = image::codecs::gif::GifDecoder::new(Cursor::new(&small)).unwrap();
        let frames = decoded.into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 4, "every frame survives the shrink");
        assert_eq!(frames[0].buffer().width(), 64);
        assert_eq!(frames[0].buffer().height(), 64);
        assert!(small.len() < big.len(), "shrinking should make it smaller");
    }

    #[test]
    fn a_still_picture_comes_back_as_a_png() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(300, 150, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let (small, ext) = shrink(&png, 64).expect("should shrink");
        assert_eq!(ext, "png");
        let out = image::load_from_memory(&small).unwrap();
        assert_eq!((out.width(), out.height()), (64, 32), "aspect ratio is kept");
    }

    #[test]
    fn fit_keeps_the_aspect_ratio_and_never_returns_zero() {
        assert_eq!(fit(500, 500, 64), (64, 64));
        assert_eq!(fit(224, 128, 64), (64, 36));
        assert_eq!(fit(128, 224, 64), (36, 64));
        // Already small enough is left alone rather than blown up.
        assert_eq!(fit(20, 10, 64), (20, 10));
        // A sliver must not round down to a zero-width image, which no
        // encoder will accept.
        assert_eq!(fit(4000, 1, 64), (64, 1));
    }

    /// The synthetic GIFs above are flat colour, which quantises perfectly and
    /// so flatters the encoder. This one runs a real emote through, and is
    /// ignored because it needs a file that is not in the tree:
    ///
    /// ```text
    /// curl -o /tmp/e.gif https://files.kick.com/emotes/5769661/fullsize
    /// MOHO_TEST_EMOTE=/tmp/e.gif cargo test shrinks_a_real_kick_emote -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn shrinks_a_real_kick_emote() {
        use image::AnimationDecoder;
        let Ok(path) = std::env::var("MOHO_TEST_EMOTE") else {
            panic!("set MOHO_TEST_EMOTE to a downloaded emote");
        };
        let big = std::fs::read(&path).expect("reading the emote");
        let before = image::codecs::gif::GifDecoder::new(Cursor::new(&big))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        let started = std::time::Instant::now();
        let (small, ext) = shrink(&big, EMOTE_PX).expect("should shrink");
        let took = started.elapsed();
        let after = image::codecs::gif::GifDecoder::new(Cursor::new(&small))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();

        let (bw, bh) = (before[0].buffer().width(), before[0].buffer().height());
        let (aw, ah) = (after[0].buffer().width(), after[0].buffer().height());
        println!(
            "  {bw}x{bh} {}f {} KB  ->  {aw}x{ah} {}f {} KB (.{ext})  in {} ms",
            before.len(),
            big.len() / 1024,
            after.len(),
            small.len() / 1024,
            took.as_millis()
        );
        println!(
            "  decoded frame buffers: {:.1} MB  ->  {:.2} MB",
            (bw as f64 * bh as f64 * 4.0 * before.len() as f64) / 1048576.0,
            (aw as f64 * ah as f64 * 4.0 * after.len() as f64) / 1048576.0
        );
        assert_eq!(after.len(), before.len(), "every frame survives");
        assert!(aw <= EMOTE_PX && ah <= EMOTE_PX);
        assert!(small.len() < big.len() / 4, "should be much smaller, not slightly");
    }

    #[test]
    fn a_gif_is_recognised_by_its_magic_number() {
        assert!(is_gif(b"GIF89a\x00\x00"));
        assert!(is_gif(b"GIF87a\x00\x00"));
        assert!(!is_gif(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_gif(b""));
    }
}
