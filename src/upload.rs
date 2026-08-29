use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

/// Putting a file somewhere it can be linked to.
///
/// Several protocols have no idea of an attachment at all - IRC carries text
/// and nothing else, and Sneedchat's own uploader is images-only - so sharing
/// a picture on them means uploading it somewhere and sending the link. That
/// was already true for Sneedchat, which had postimg.cc wired directly into
/// its send path; this generalises it, because the same need is IRC's and the
/// choice of where to put somebody's files is theirs rather than ours.
///
/// Anonymous hosts only. Nothing here holds an account or a key: a client that
/// silently signed uploads into an account somebody forgot they had would be a
/// worse thing than a broken link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Host {
    /// Permanent, 200MB, takes any file type.
    Catbox,
    /// Temporary, 1GB, takes any file type. Expires - which for a chat link is
    /// sometimes the point and sometimes a trap, so it says so.
    Litterbox,
    /// Images only, permanent.
    Postimg,
}

impl Host {
    pub fn parse(name: &str) -> Option<Host> {
        match name.trim().to_ascii_lowercase().as_str() {
            "catbox" => Some(Host::Catbox),
            "litterbox" => Some(Host::Litterbox),
            "postimg" => Some(Host::Postimg),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Host::Catbox => "catbox",
            Host::Litterbox => "litterbox",
            Host::Postimg => "postimg",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Host::Catbox => "catbox.moe (permanent, any file, 200MB)",
            Host::Litterbox => "litterbox.catbox.moe (expires, any file, 1GB)",
            Host::Postimg => "postimg.cc (permanent, images only)",
        }
    }

    /// Whether this host will take something that is not an image.
    pub fn takes_any_file(self) -> bool {
        !matches!(self, Host::Postimg)
    }

    pub fn max_bytes(self) -> usize {
        match self {
            Host::Catbox => 200 * 1024 * 1024,
            Host::Litterbox => 1024 * 1024 * 1024,
            Host::Postimg => 32 * 1024 * 1024,
        }
    }
}

impl Host {
    /// The one service this host can be reached from, if it is not general.
    ///
    /// postimg.cc is posted through Sneedchat's own transport, with the
    /// headers and the second page-scrape that path carries - the shared
    /// uploader here has no way to reach it. A client offering it for IRC
    /// would be offering a choice that fails at send time, which is why this
    /// is answered here rather than guessed at by each menu.
    pub fn only_for(self) -> Option<&'static str> {
        match self {
            Host::Postimg => Some("sockchat"),
            _ => None,
        }
    }
}

/// Every host a frontend can offer, so the list of choices lives in one place
/// rather than being spelled out again in each client that draws a menu.
pub fn hosts() -> Vec<serde_json::Value> {
    [Host::Catbox, Host::Litterbox, Host::Postimg]
        .iter()
        .map(|h| {
            serde_json::json!({
                "id": h.id(),
                "label": h.label(),
                "imagesOnly": !h.takes_any_file(),
                "onlyFor": h.only_for(),
                "maxBytes": h.max_bytes(),
            })
        })
        .collect()
}

fn is_image(file_name: &str) -> bool {
    matches!(
        file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif"
    )
}

/// Uploads a file and returns the URL to link to.
///
/// `retention` applies only to the host that has any: it is passed through
/// rather than interpreted, so a value that host does not know is its own
/// error to report rather than something to be silently corrected here.
pub async fn upload(host: Host, path: &str, retention: Option<&str>) -> Result<String> {
    let bytes = tokio::fs::read(path).await.with_context(|| format!("reading {path}"))?;
    if bytes.is_empty() {
        bail!("that file is empty");
    }
    if bytes.len() > host.max_bytes() {
        bail!("that file is over {}'s {}MB limit", host.id(), host.max_bytes() / 1024 / 1024);
    }
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload")
        .to_string();
    if !host.takes_any_file() && !is_image(&file_name) {
        bail!("{} only takes images - choose another host for \"{file_name}\"", host.id());
    }

    match host {
        Host::Catbox | Host::Litterbox => catbox_family(host, file_name, bytes, retention).await,
        Host::Postimg => Err(anyhow!(
            "postimg.cc uploads go through the Sneedchat transport; pick catbox or litterbox here"
        )),
    }
}

/// The client these hosts are talked to over.
///
/// HTTP/2 deliberately, and it is not a preference: catbox.moe's HTTP/1.1
/// path is broken. Over 1.1 it answers a completed upload with a chunked 500
/// and then drops the TLS connection without a close_notify, which rustls
/// correctly reports as a truncated stream - so the reply is never read at
/// all and every attachment sent to IRC failed. The same request over HTTP/2
/// gets a clean 200 with the link in it. Negotiated by ALPN, so a host that
/// only speaks 1.1 still works.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"), " (nobilis)"))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// catbox.moe and its temporary sibling share one API: a multipart form with a
/// `reqtype`, and the finished URL as the whole of the response body.
async fn catbox_family(host: Host, file_name: String, bytes: Vec<u8>, retention: Option<&str>) -> Result<String> {
    let url = match host {
        Host::Litterbox => "https://litterbox.catbox.moe/resources/internals/api.php",
        _ => "https://catbox.moe/user/api.php",
    };
    let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
    let mut form = reqwest::multipart::Form::new().text("reqtype", "fileupload").part("fileToUpload", part);
    if host == Host::Litterbox {
        form = form.text("time", retention.unwrap_or("72h").to_string());
    }

    let resp = http_client()
        .post(url)
        .multipart(form)
        .send()
        .await
        // The cause, not just the intent: "uploading to catbox" on its own
        // leaves somebody staring at a failure that could equally be a dead
        // network, a proxy, or the host being down. reqwest keeps the actual
        // reason in the error's source chain rather than its Display, so
        // without walking it every transport failure reads identically.
        .map_err(|e| anyhow!("couldn't reach {}: {}", host.id(), with_causes(&e)))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    link_from_response(host, status, &body)
}

/// Reads the outcome out of what the host actually sent back.
///
/// The body is the finished URL and nothing else, and that - not the status
/// line - is what says whether the upload worked. catbox.moe answers a
/// completed upload with HTTP 500 while storing and serving the file
/// perfectly well, so checking the status first refuses uploads that in fact
/// succeeded: the file is sitting on the host and the link is in our hand,
/// and we throw both away. Both of these hosts also serve error pages with a
/// 200, so neither half is trustworthy alone.
///
/// So: believe a link wherever one came back, and fall back to the status
/// only to explain a reply that carried none.
fn link_from_response(host: Host, status: reqwest::StatusCode, body: &str) -> Result<String> {
    let link = body.trim();
    if link.starts_with("https://") {
        return Ok(link.to_string());
    }
    if !status.is_success() {
        bail!("{} refused the upload: HTTP {status}", host.id());
    }
    bail!("{} did not return a link: {}", host.id(), snippet(link));
}

/// An error together with everything underneath it.
///
/// The interesting half of a failed request - the DNS answer, the refused
/// connection, the certificate - is never in the top error's own message.
fn with_causes(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cause = err.source();
    while let Some(c) = cause {
        out.push_str(&format!(": {c}"));
        cause = c.source();
    }
    out
}

/// The first part of a reply, for an error message.
///
/// Cut on a character boundary rather than a byte one: these are error pages
/// rather than the URL we expected, and one containing any non-ASCII - a
/// typographic quote in a maintenance notice is enough - would panic the
/// daemon on a plain byte slice.
fn snippet(text: &str) -> String {
    const LIMIT: usize = 200;
    match text.char_indices().nth(LIMIT) {
        None => text.to_string(),
        Some((end, _)) => format!("{}...", &text[..end]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A menu that offered postimg for IRC would be offering a choice that
    /// fails at send time - the shared uploader cannot reach it, only
    /// Sneedchat's own transport can.
    #[test]
    fn a_host_says_which_service_it_belongs_to() {
        assert_eq!(Host::Postimg.only_for(), Some("sockchat"));
        assert_eq!(Host::Catbox.only_for(), None);
        assert_eq!(Host::Litterbox.only_for(), None);

        // And it reaches a client, since that is the point of saying it.
        let listed = hosts();
        let postimg = listed.iter().find(|h| h["id"] == "postimg").expect("postimg listed");
        assert_eq!(postimg["onlyFor"], "sockchat");
        assert_eq!(postimg["imagesOnly"], true);
        let catbox = listed.iter().find(|h| h["id"] == "catbox").expect("catbox listed");
        assert!(catbox["onlyFor"].is_null());
    }

    #[test]
    fn host_names_round_trip() {
        for h in [Host::Catbox, Host::Litterbox, Host::Postimg] {
            assert_eq!(Host::parse(h.id()), Some(h));
        }
        assert_eq!(Host::parse("nowhere"), None);
    }

    /// The images-only host has to refuse a video before it is uploaded, not
    /// after: the point of saying so is to send the person to another host.
    #[tokio::test]
    async fn an_images_only_host_refuses_a_video_without_uploading_it() {
        let dir = std::env::temp_dir().join(format!("nobilis-upload-{}", crate::model::next_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        std::fs::write(&file, b"not really a video").unwrap();
        let err = upload(Host::Postimg, file.to_str().unwrap(), None).await.unwrap_err().to_string();
        assert!(err.contains("only takes images"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one that broke sending files on IRC: catbox.moe returns the
    /// finished URL with a 500 on it, and the upload really did work.
    #[test]
    fn a_link_is_believed_even_under_a_failing_status() {
        let link = link_from_response(
            Host::Catbox,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "https://files.catbox.moe/n8zh4g.png\n",
        )
        .unwrap();
        assert_eq!(link, "https://files.catbox.moe/n8zh4g.png");
    }

    /// The other half: these hosts serve error pages with a 200, so a reply
    /// that is not a link is still a failure however cheerful its status.
    #[test]
    fn a_reply_that_is_not_a_link_still_fails() {
        let err = link_from_response(Host::Catbox, reqwest::StatusCode::OK, "<html>go away</html>")
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not return a link"), "got {err}");

        let err = link_from_response(Host::Litterbox, reqwest::StatusCode::BAD_GATEWAY, "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("HTTP 502"), "got {err}");
    }

    /// A long error page full of non-ASCII must not take the daemon with it.
    #[test]
    fn a_long_reply_is_cut_on_a_character_boundary() {
        let page = "\u{201c}down for maintenance\u{201d} ".repeat(40);
        let err = link_from_response(Host::Catbox, reqwest::StatusCode::OK, &page).unwrap_err().to_string();
        assert!(err.ends_with("..."), "got {err}");
        assert!(snippet(&page).chars().count() <= 203);
        // Short replies are shown whole, with nothing appended.
        assert_eq!(snippet("nope"), "nope");
    }

    /// An empty file is a mistake worth catching here rather than sending a
    /// zero-byte upload and linking to nothing.
    #[tokio::test]
    async fn an_empty_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("nobilis-upload-{}", crate::model::next_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("empty.png");
        std::fs::write(&file, b"").unwrap();
        let err = upload(Host::Catbox, file.to_str().unwrap(), None).await.unwrap_err().to_string();
        assert!(err.contains("empty"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
