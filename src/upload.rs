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
/// How far an upload has got, said out loud.
///
/// An upload to somebody else's host is the one part of sending a message
/// that can take a minute and has nothing to show for itself. The window
/// showed the message as sent-and-pending the whole time, which meant an
/// ordinary slow upload was indistinguishable from a message that had
/// vanished - and after ten seconds the window's own send timeout called it
/// failed and offered a retry, while the upload was still running.
///
/// So the upload says where it is. `Phase` rather than a percentage on
/// purpose: the file is handed to the HTTP client whole, and restructuring
/// that into a counted stream would mean rebuilding a request body whose
/// current shape was arrived at the hard way (see `http_client` on catbox and
/// HTTP/1.1). What can be said honestly is which of the three waits this is,
/// and how long it has been - which is what somebody looking at a spinner
/// actually wants to know.
#[derive(Clone)]
pub struct Progress {
    events: crate::events::EventBus,
    id: String,
}

/// The stages of an upload, in the order they happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Reading the file and checking it against the host's limits.
    Preparing,
    /// The request is out and the bytes are going.
    Sending,
    /// Everything is written and the host has not answered yet. Distinct from
    /// `Sending` because this is the wait that goes long on a slow host, and
    /// a spinner that has silently meant two different things for a minute is
    /// no better than no spinner.
    Waiting,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Preparing => "preparing",
            Phase::Sending => "sending",
            Phase::Waiting => "waiting",
        }
    }
}

impl Progress {
    pub fn new(events: crate::events::EventBus, id: &str) -> Progress {
        Progress { events, id: id.to_string() }
    }

    /// Says which stage this is, and how big the file turned out to be.
    ///
    /// `bytes` is zero until the file has been read, which is why it is sent
    /// with every phase rather than once at the start.
    pub fn at(&self, phase: Phase, bytes: usize, host: &str) {
        self.events.emit(
            "uploadProgress",
            serde_json::json!({
                "uploadId": self.id,
                "phase": phase.name(),
                "bytes": bytes,
                "host": host,
            }),
        );
    }

    /// How many of the file's bytes have been handed to the connection.
    ///
    /// Only sent by an upload that counts them - Discord's, which streams the
    /// files - so the window can draw a ring that fills. Everything else says
    /// its phase and stays a turning ring, because for them a percentage
    /// would be invented.
    pub fn sent(&self, sent: u64, total: u64, host: &str) {
        self.events.emit(
            "uploadProgress",
            serde_json::json!({
                "uploadId": self.id,
                "phase": "sending",
                "bytes": total,
                "sent": sent,
                "total": total,
                "host": host,
            }),
        );
    }

    /// The end, either way.
    ///
    /// Always sent, including when the upload failed, because the window has
    /// a row on screen waiting to be told - and a spinner with nothing coming
    /// is the failure this whole thing exists to remove.
    pub fn done(&self, error: Option<&str>) {
        self.events.emit(
            "uploadProgress",
            serde_json::json!({
                "uploadId": self.id,
                "phase": "done",
                "error": error,
            }),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Host {
    /// Permanent, 200MB, takes any file type.
    Catbox,
    /// Temporary, 1GB, takes any file type. Expires - which for a chat link is
    /// sometimes the point and sometimes a trap, so it says so.
    Litterbox,
    /// Images only, permanent.
    Postimg,
    /// Images only, permanent, 32MB - and a much wider list than postimg's,
    /// avif and heic among it.
    Ibb,
    /// Images only, 20MB. Not offered to Sneedchat, where an imgur link does
    /// not embed.
    Imgur,
}

impl Host {
    pub fn parse(name: &str) -> Option<Host> {
        match name.trim().to_ascii_lowercase().as_str() {
            "catbox" => Some(Host::Catbox),
            "litterbox" => Some(Host::Litterbox),
            "postimg" => Some(Host::Postimg),
            "ibb" | "imgbb" => Some(Host::Ibb),
            "imgur" => Some(Host::Imgur),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Host::Catbox => "catbox",
            Host::Litterbox => "litterbox",
            Host::Postimg => "postimg",
            Host::Ibb => "ibb",
            Host::Imgur => "imgur",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Host::Catbox => "catbox.moe (permanent, any file, 200MB)",
            Host::Litterbox => "litterbox.catbox.moe (expires, any file, 1GB)",
            Host::Postimg => "postimg.cc (permanent, images only)",
            Host::Ibb => "ibb.co (permanent, images only, avif and heic too, 32MB)",
            Host::Imgur => "imgur.com (images only, 20MB - removes unused anonymous uploads)",
        }
    }

    /// Whether this host will take something that is not an image.
    pub fn takes_any_file(self) -> bool {
        !matches!(self, Host::Postimg | Host::Ibb | Host::Imgur)
    }

    /// The file extensions this host accepts, or `None` for anything.
    ///
    /// postimg.cc's own list, from the table that also builds the multipart
    /// content type - so what a menu offers and what the site will actually
    /// take cannot drift apart. They did: "image" here once included avif,
    /// which postimg refuses, so an .avif was routed there and rejected on
    /// arrival.
    pub fn accepted_extensions(self) -> Option<&'static [(&'static str, &'static str)]> {
        match self {
            Host::Postimg => Some(crate::backend::sneedchat::POSTIMG_TYPES),
            Host::Ibb => Some(IBB_TYPES),
            Host::Imgur => Some(IMGUR_TYPES),
            _ => None,
        }
    }

    /// Whether this host will take this particular file.
    pub fn accepts(self, file_name: &str) -> bool {
        match self.accepted_extensions() {
            None => true,
            Some(table) => {
                let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                table.iter().any(|(e, _)| *e == ext)
            }
        }
    }

    pub fn max_bytes(self) -> usize {
        match self {
            Host::Catbox => 200 * 1024 * 1024,
            Host::Litterbox => 1024 * 1024 * 1024,
            Host::Postimg => 32 * 1024 * 1024,
            // Decimal, as the site states it: `max_filesize":32000000`.
            Host::Ibb => 32_000_000,
            // imgur's own anonymous image cap, from its upload page's script
            // (`R=20971520`).
            Host::Imgur => 20 * 1024 * 1024,
        }
    }

    /// The services this host is not offered to, by the protocol name an
    /// account carries.
    ///
    /// Sneedchat's forum embeds a picture from the hosts it trusts and shows
    /// anything else as a bare link, and imgur is not among them - so a
    /// picture sent there would arrive as a URL nobody sees the image of.
    pub fn not_for(self) -> &'static [&'static str] {
        match self {
            Host::Imgur => &["sneedchat"],
            _ => &[],
        }
    }

    /// Whether this host may be used for an upload posted to `service`.
    pub fn offered_to(self, service: &str) -> bool {
        !self.not_for().contains(&service)
    }
}

/// Every host a frontend can offer, so the list of choices lives in one place
/// rather than being spelled out again in each client that draws a menu.
pub fn hosts() -> Vec<serde_json::Value> {
    [Host::Catbox, Host::Litterbox, Host::Postimg, Host::Ibb, Host::Imgur]
        .iter()
        .map(|h| {
            serde_json::json!({
                "id": h.id(),
                "label": h.label(),
                "imagesOnly": !h.takes_any_file(),
                // What it will actually take, so a client can send a file
                // somewhere that will have it rather than somewhere that
                // will refuse it.
                "accepts": h.accepted_extensions().map(|t| t.iter().map(|(e, _)| *e).collect::<Vec<_>>()),
                "maxBytes": h.max_bytes(),
                // Services a menu should leave this host out of.
                "notFor": h.not_for(),
            })
        })
        .collect()
}

/// Uploads a file and returns the URL to link to, saying how it is going
/// where there is a window waiting on it.
///
/// `retention` applies only to the host that has any: it is passed through
/// rather than interpreted, so a value that host does not know is its own
/// error to report rather than something to be silently corrected here.
pub async fn upload_reporting(
    host: Host,
    path: &str,
    retention: Option<&str>,
    progress: Option<&Progress>,
) -> Result<String> {
    let result = upload_inner(host, path, retention, progress).await;
    if let Some(progress) = progress {
        progress.done(result.as_ref().err().map(|e| e.to_string()).as_deref());
    }
    result
}

async fn upload_inner(
    host: Host,
    path: &str,
    retention: Option<&str>,
    progress: Option<&Progress>,
) -> Result<String> {
    if let Some(progress) = progress {
        progress.at(Phase::Preparing, 0, host.id());
    }
    // Through Tor or the proxy when uploads are routed - started first, so
    // the upload neither waits on a cold Tor nor fails for want of one.
    crate::net::route::router().ready_for_general().await?;
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
    if !host.accepts(&file_name) {
        let taken = host
            .accepted_extensions()
            .map(|t| t.iter().map(|(e, _)| *e).collect::<Vec<_>>().join("/"))
            .unwrap_or_default();
        bail!("{} does not take \"{file_name}\" - it accepts {taken}", host.id());
    }

    if let Some(progress) = progress {
        progress.at(Phase::Sending, bytes.len(), host.id());
    }
    match host {
        Host::Catbox | Host::Litterbox => catbox_family(host, file_name, bytes, retention, progress).await,
        // The direct image link, not the page about it. Sneedchat wraps both
        // in BBCode because it renders markup; a caller reaching this has
        // none, so what it wants is the URL that ends in a file extension -
        // it is what makes a link unfurl into a picture at the far end.
        Host::Postimg => Ok(crate::backend::sneedchat::upload_to_postimg_reporting(path, progress).await?.direct),
        Host::Ibb => ibb(file_name, bytes, progress).await,
        Host::Imgur => imgur(file_name, bytes, progress).await,
    }
}

/// Whether a browser shows this file as a picture - what decides whether a
/// link to it is worth wrapping in a forum's `[img]`.
///
/// Wider than any one host's list and narrower than ibb's: heic and tiff are
/// images, and a browser draws neither.
pub fn is_web_picture(file_name: &str) -> bool {
    const SHOWN: &[&str] = &["png", "jpg", "jpeg", "jpe", "gif", "apng", "webp", "avif", "bmp", "svg", "ico"];
    let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    file_name.contains('.') && SHOWN.contains(&ext.as_str())
}

/// What imgur will take as an anonymous image upload.
///
/// From its upload page's own table (`H={jpeg:"image/jpeg",...}` beside the
/// accept list `.jpg,.jpeg,.png,.gif,.apng,.tiff,.tif,.bmp,.xcf,.webp`), less
/// xcf, which is GIMP's working format rather than a picture. No avif: imgur
/// does not take it. Video is left out too - imgur takes it, through a
/// different upload with processing afterwards, and that is not built here.
pub const IMGUR_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("apng", "image/apng"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
];

/// imgur's upload endpoint, the one its own site and every anonymous
/// uploader use.
const IMGUR_UPLOAD: &str = "https://api.imgur.com/3/image";
/// The client id imgur's own web app identifies itself with
/// (`apiClientId` in its page config). It names the application, not a
/// person: an upload made with it belongs to nobody, which is what anonymous
/// means here, and it is what the site sends for a visitor who is not signed
/// in.
const IMGUR_CLIENT_ID: &str = "d70305e7c3ac5c6";

/// Puts a picture on imgur, anonymously, and returns its direct link.
///
/// One request: the file as `image`, with `type=file`, identified by the
/// site's own client id. The answer's `data.link` is the file on i.imgur.com.
///
/// Anonymous imgur uploads are not permanent in the way catbox's are: since
/// 2023 imgur deletes content not tied to an account once it judges it
/// unused. The label says so, so the choice is made knowing it.
async fn imgur(file_name: String, bytes: Vec<u8>, progress: Option<&Progress>) -> Result<String> {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let content_type = IMGUR_TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t).unwrap_or("application/octet-stream");
    let size = bytes.len();
    let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name).mime_str(content_type)?;
    let form = reqwest::multipart::Form::new().text("type", "file").part("image", part);

    if let Some(progress) = progress {
        progress.at(Phase::Waiting, size, Host::Imgur.id());
    }
    let resp = http_client()
        .post(IMGUR_UPLOAD)
        .header(reqwest::header::AUTHORIZATION, format!("Client-ID {IMGUR_CLIENT_ID}"))
        .multipart(form)
        .send()
        .await
        .map_err(|e| anyhow!("couldn't reach imgur: {}", with_causes(&e)))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    imgur_link(status, &body)
}

/// The direct link out of imgur's answer, or why there is none.
///
/// imgur's error is a string on most failures and an object with a
/// `message` on some, so it is read loosely.
fn imgur_link(status: reqwest::StatusCode, body: &str) -> Result<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let data = &parsed["data"];
    if let Some(link) = data["link"].as_str().filter(|l| l.starts_with("https://")) {
        return Ok(link.to_string());
    }
    let reason = data["error"]
        .as_str()
        .map(str::to_string)
        .or_else(|| data["error"]["message"].as_str().map(str::to_string));
    match reason {
        Some(reason) if !reason.is_empty() => bail!("imgur refused the upload: {reason}"),
        _ if !status.is_success() => bail!("imgur refused the upload: HTTP {status}"),
        _ => bail!("imgur did not return a link: {}", snippet(body)),
    }
}

/// What ibb.co will take, and what to send each as.
///
/// A subset of the site's own list (`"upload":{"image_types":[...]}` in its
/// upload page's config), which runs to seventy-odd formats including camera
/// raws and Photoshop files: these are the ones somebody would share in a
/// chat, and each has a content type that means something. avif is the one
/// that matters - postimg refuses it - with heic, the iPhone's own format,
/// close behind. The single source for both the multipart content type and
/// what `hosts()` publishes, as `POSTIMG_TYPES` is for postimg.
pub const IBB_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("jpe", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("jxl", "image/jxl"),
    ("bmp", "image/bmp"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("ico", "image/x-icon"),
    ("svg", "image/svg+xml"),
];

/// ibb.co's upload page, which hands out the token an upload needs.
const IBB_PAGE: &str = "https://imgbb.com/";
/// Where the page's own uploader posts (`PF.obj.config.json_api`).
const IBB_JSON: &str = "https://imgbb.com/json";

/// Puts a picture on ibb.co, anonymously, and returns its direct link.
///
/// Not the documented API, which wants a key and so an account: this is what
/// the site's own upload page does, read out of its script (`ibb.js`,
/// `CHV.fn.uploader`). Two requests, because the upload has to carry an
/// `auth_token` the page embeds (`PF.obj.config.auth_token="..."`), and the
/// token belongs to the PHP session the page opened - so the page's session
/// cookie goes back with the upload, or the token means nothing.
///
/// The form is the page's own, with its empty fields left out as the page
/// leaves them out: `type=file`, `action=upload`, a millisecond `timestamp`,
/// the token, and the file as `source`. No `expiration`, which is the page's
/// "Don't autodelete".
async fn ibb(file_name: String, bytes: Vec<u8>, progress: Option<&Progress>) -> Result<String> {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let content_type = IBB_TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t).unwrap_or("application/octet-stream");

    let page = http_client()
        .get(IBB_PAGE)
        .send()
        .await
        .map_err(|e| anyhow!("couldn't reach ibb.co: {}", with_causes(&e)))?;
    let cookies = session_cookies(page.headers());
    let html = page.text().await.unwrap_or_default();
    let token = ibb_auth_token(&html).ok_or_else(|| anyhow!("ibb.co's upload page has no upload token in it - the site may have changed"))?;

    let size = bytes.len();
    let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name).mime_str(content_type)?;
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let form = reqwest::multipart::Form::new()
        .text("type", "file")
        .text("action", "upload")
        .text("timestamp", timestamp.to_string())
        .text("auth_token", token)
        .part("source", part);

    if let Some(progress) = progress {
        progress.at(Phase::Waiting, size, Host::Ibb.id());
    }
    let resp = http_client()
        .post(IBB_JSON)
        .header(reqwest::header::COOKIE, cookies)
        .header(reqwest::header::ORIGIN, "https://imgbb.com")
        .header(reqwest::header::REFERER, IBB_PAGE)
        .multipart(form)
        .send()
        .await
        .map_err(|e| anyhow!("couldn't reach ibb.co: {}", with_causes(&e)))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    ibb_link(status, &body)
}

/// The upload token ibb.co's page embeds in its config.
fn ibb_auth_token(html: &str) -> Option<String> {
    let start = html.find("auth_token=\"")? + "auth_token=\"".len();
    let token = &html[start..start + html[start..].find('"')?];
    (!token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric())).then(|| token.to_string())
}

/// Every cookie a response set, as a `Cookie` header sends them back.
fn session_cookies(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .map(str::trim)
        .filter(|pair| pair.contains('='))
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(serde::Deserialize)]
struct IbbReply {
    #[serde(default)]
    image: Option<IbbImage>,
    #[serde(default)]
    error: Option<IbbError>,
}

#[derive(serde::Deserialize)]
struct IbbImage {
    /// The file itself, on i.ibb.co - the link that unfurls into a picture.
    url: String,
}

#[derive(serde::Deserialize)]
struct IbbError {
    #[serde(default)]
    message: String,
}

/// The direct link out of ibb.co's answer, or why there is none.
///
/// Of the several links the answer carries - the page (`url_viewer`), a
/// medium and a thumbnail rendition, the delete link - `image.url` is the
/// original file, which is what a chat that unfurls a bare URL wants.
fn ibb_link(status: reqwest::StatusCode, body: &str) -> Result<String> {
    match serde_json::from_str::<IbbReply>(body) {
        Ok(IbbReply { image: Some(image), .. }) if image.url.starts_with("https://") => Ok(image.url),
        Ok(IbbReply { error: Some(error), .. }) if !error.message.is_empty() => bail!("ibb.co refused the upload: {}", error.message),
        _ if !status.is_success() => bail!("ibb.co refused the upload: HTTP {status}"),
        _ => bail!("ibb.co did not return a link: {}", snippet(body)),
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
fn http_client() -> reqwest::Client {
    let router = crate::net::route::router();
    router.client_if("upload", router.general_routed(), |builder| {
        builder.user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"), " (nobilis)"))
    })
}

/// catbox.moe and its temporary sibling share one API: a multipart form with a
/// `reqtype`, and the finished URL as the whole of the response body.
async fn catbox_family(
    host: Host,
    file_name: String,
    bytes: Vec<u8>,
    retention: Option<&str>,
    progress: Option<&Progress>,
) -> Result<String> {
    let url = match host {
        Host::Litterbox => "https://litterbox.catbox.moe/resources/internals/api.php",
        _ => "https://catbox.moe/user/api.php",
    };
    let size = bytes.len();
    let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
    let mut form = reqwest::multipart::Form::new().text("reqtype", "fileupload").part("fileToUpload", part);
    if host == Host::Litterbox {
        form = form.text("time", retention.unwrap_or("72h").to_string());
    }

    // Everything past here is one await with nothing observable inside it, so
    // this is the last honest thing that can be said until the host answers.
    if let Some(progress) = progress {
        progress.at(Phase::Waiting, size, host.id());
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

    /// postimg takes images and nothing else, and a client's menu has to
    /// know that - it is the difference between not offering it for a video
    /// and having the upload refused after the fact.
    #[test]
    fn the_listing_says_what_each_host_will_take() {
        let listed = hosts();
        let postimg = listed.iter().find(|h| h["id"] == "postimg").expect("postimg listed");
        assert_eq!(postimg["imagesOnly"], true);
        let catbox = listed.iter().find(|h| h["id"] == "catbox").expect("catbox listed");
        assert_eq!(catbox["imagesOnly"], false);
        let ibb = listed.iter().find(|h| h["id"] == "ibb").expect("ibb listed");
        assert_eq!(ibb["imagesOnly"], true);
        assert_eq!(listed.len(), 5);
        // imgur is the one host a service is kept from: Sneedchat does not
        // embed it.
        let imgur = listed.iter().find(|h| h["id"] == "imgur").expect("imgur listed");
        assert_eq!(imgur["notFor"], serde_json::json!(["sneedchat"]));
        assert_eq!(catbox["notFor"], serde_json::json!([]));
    }

    #[test]
    fn host_names_round_trip() {
        for h in [Host::Catbox, Host::Litterbox, Host::Postimg, Host::Ibb, Host::Imgur] {
            assert_eq!(Host::parse(h.id()), Some(h));
        }
        assert_eq!(Host::parse("nowhere"), None);
    }

    /// avif is the case that started this: it is an image by any ordinary
    /// reading, and postimg does not take it. What a menu offers and what
    /// the site accepts have to be the same answer.
    #[test]
    fn a_host_accepts_exactly_what_it_says_it_does() {
        assert!(Host::Postimg.accepts("cat.png"));
        assert!(Host::Postimg.accepts("cat.JPEG"));
        assert!(!Host::Postimg.accepts("cat.avif"));
        assert!(!Host::Postimg.accepts("clip.mp4"));
        assert!(!Host::Postimg.accepts("noextension"));

        // The general hosts take whatever they are given.
        assert!(Host::Catbox.accepts("cat.avif"));
        assert!(Host::Catbox.accepts("clip.mp4"));
        assert_eq!(Host::Catbox.accepted_extensions(), None);

        // And a client is told, so it can route round a refusal.
        let listed = hosts();
        let postimg = listed.iter().find(|h| h["id"] == "postimg").unwrap();
        let accepts: Vec<&str> = postimg["accepts"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(accepts.contains(&"png"));
        assert!(!accepts.contains(&"avif"));
        assert!(listed.iter().find(|h| h["id"] == "catbox").unwrap()["accepts"].is_null());
    }

    /// The images-only host has to refuse a video before it is uploaded, not
    /// after: the point of saying so is to send the person to another host.
    #[tokio::test]
    async fn an_images_only_host_refuses_a_video_without_uploading_it() {
        let dir = std::env::temp_dir().join(format!("nobilis-upload-{}", crate::model::next_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        std::fs::write(&file, b"not really a video").unwrap();
        let err = upload_reporting(Host::Postimg, file.to_str().unwrap(), None, None).await.unwrap_err().to_string();
        assert!(err.contains("does not take"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An upload that never starts still says it is over.
    ///
    /// The whole point of the progress events is a spinner that ends. A
    /// refusal before any network work - the wrong file type, a file over the
    /// host's limit - is exactly the case where it would be easiest to return
    /// early and leave the window turning forever, so it is the one pinned
    /// down here.
    #[tokio::test]
    async fn a_refused_file_still_reports_that_it_finished() {
        let dir = std::env::temp_dir().join(format!("nobilis-progress-{}", crate::model::next_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        std::fs::write(&file, b"not really a video").unwrap();

        let bus = crate::events::EventBus::new();
        let mut rx = bus.subscribe();
        let progress = Progress::new(bus, "upload-1");
        let err = upload_reporting(Host::Postimg, file.to_str().unwrap(), None, Some(&progress))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not take"), "got {err}");

        let mut phases = Vec::new();
        while let Ok(event) = rx.try_recv() {
            assert_eq!(event.data["uploadId"], "upload-1");
            phases.push(event.data["phase"].as_str().unwrap_or_default().to_string());
        }
        assert_eq!(phases.first().map(String::as_str), Some("preparing"));
        assert_eq!(phases.last().map(String::as_str), Some("done"), "phases were {phases:?}");
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

    /// ibb.co is the answer to avif: the format postimg refuses, it takes.
    #[test]
    fn ibb_takes_what_postimg_refuses_and_still_only_images() {
        assert!(Host::Ibb.accepts("cat.avif"));
        assert!(Host::Ibb.accepts("IMG_0001.HEIC"));
        assert!(Host::Ibb.accepts("cat.png"));
        assert!(!Host::Ibb.accepts("clip.mp4"));
        assert!(!Host::Ibb.accepts("notes.txt"));
        assert_eq!(Host::Ibb.max_bytes(), 32_000_000);
        let listed = hosts();
        let accepts = listed.iter().find(|h| h["id"] == "ibb").unwrap()["accepts"].as_array().unwrap().clone();
        assert!(accepts.iter().any(|v| v == "avif"));
    }

    /// The token as the page writes it into its config.
    #[test]
    fn the_upload_token_is_read_off_the_page() {
        let html = r#"PF.obj.config.json_api="https://imgbb.com/json";
PF.obj.config.auth_token="32dc939d0777df0367b9d127ee79b39aa27525fb";"#;
        assert_eq!(ibb_auth_token(html).as_deref(), Some("32dc939d0777df0367b9d127ee79b39aa27525fb"));
        assert_eq!(ibb_auth_token("<html>no config</html>"), None);
        assert_eq!(ibb_auth_token(r#"auth_token="";"#), None);
    }

    #[test]
    fn the_session_cookie_goes_back_without_its_attributes() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(reqwest::header::SET_COOKIE, "PHPSESSID=abc123; path=/; secure; HttpOnly".parse().unwrap());
        headers.append(reqwest::header::SET_COOKIE, "lang=en; expires=Thu, 01 Jan 2099 00:00:00 GMT".parse().unwrap());
        assert_eq!(session_cookies(&headers), "PHPSESSID=abc123; lang=en");
    }

    /// The direct file, not the page, the medium rendition or the delete link.
    #[test]
    fn the_direct_link_is_taken_from_the_answer() {
        let body = r#"{"status_code":200,"success":{"message":"image uploaded","code":200},"image":{"name":"cat","url":"https://i.ibb.co/abc123/cat.png","url_viewer":"https://ibb.co/abc123","display_url":"https://i.ibb.co/xyz/cat.png","delete_url":"https://ibb.co/abc123/deadbeef"},"status_txt":"OK"}"#;
        assert_eq!(ibb_link(reqwest::StatusCode::OK, body).unwrap(), "https://i.ibb.co/abc123/cat.png");
        let refused = r#"{"status_code":400,"error":{"message":"Invalid content type","code":311},"status_txt":"Bad Request"}"#;
        let err = ibb_link(reqwest::StatusCode::BAD_REQUEST, refused).unwrap_err().to_string();
        assert!(err.contains("Invalid content type"), "got {err}");
        let err = ibb_link(reqwest::StatusCode::OK, "<html>maintenance</html>").unwrap_err().to_string();
        assert!(err.contains("did not return a link"), "got {err}");
    }

    #[test]
    fn imgur_is_kept_from_sneedchat_and_takes_no_avif() {
        assert!(!Host::Imgur.offered_to("sneedchat"));
        assert!(Host::Imgur.offered_to("irc"));
        assert!(Host::Postimg.offered_to("sneedchat"));
        assert!(Host::Imgur.accepts("cat.png"));
        assert!(!Host::Imgur.accepts("cat.avif"));
        assert!(!Host::Imgur.accepts("clip.mp4"));
    }

    #[test]
    fn imgur_answers_are_read_for_the_file_or_the_reason() {
        let ok = r#"{"data":{"id":"AbC123x","deletehash":"zz","link":"https://i.imgur.com/AbC123x.png","type":"image/png"},"success":true,"status":200}"#;
        assert_eq!(imgur_link(reqwest::StatusCode::OK, ok).unwrap(), "https://i.imgur.com/AbC123x.png");
        let refused = r#"{"data":{"error":"File type invalid (1)","request":"/3/image","method":"POST"},"success":false,"status":400}"#;
        assert!(imgur_link(reqwest::StatusCode::BAD_REQUEST, refused).unwrap_err().to_string().contains("File type invalid"));
        let nested = r#"{"data":{"error":{"code":1003,"message":"File type invalid (1)","type":"ImgurException"}},"success":false,"status":400}"#;
        assert!(imgur_link(reqwest::StatusCode::BAD_REQUEST, nested).unwrap_err().to_string().contains("File type invalid"));
        let err = imgur_link(reqwest::StatusCode::TOO_MANY_REQUESTS, "").unwrap_err().to_string();
        assert!(err.contains("429"), "got {err}");
    }

    #[test]
    fn a_web_picture_is_one_a_browser_draws() {
        assert!(is_web_picture("cat.avif"));
        assert!(is_web_picture("cat.PNG"));
        assert!(!is_web_picture("IMG_0001.heic"));
        assert!(!is_web_picture("clip.mp4"));
        assert!(!is_web_picture("png"));
    }

    /// An empty file is a mistake worth catching here rather than sending a
    /// zero-byte upload and linking to nothing.
    #[tokio::test]
    async fn an_empty_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("nobilis-upload-{}", crate::model::next_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("empty.png");
        std::fs::write(&file, b"").unwrap();
        let err = upload_reporting(Host::Catbox, file.to_str().unwrap(), None, None).await.unwrap_err().to_string();
        assert!(err.contains("empty"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
