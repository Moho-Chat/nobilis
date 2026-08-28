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

    let resp = reqwest::Client::new()
        .post(url)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("uploading to {}", host.id()))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{} refused the upload: HTTP {status}", host.id());
    }
    let link = body.trim();
    // The body is the URL, and only the URL. Anything else is an error page
    // served with a 200, which these hosts do.
    if !link.starts_with("https://") {
        bail!("{} did not return a link: {}", host.id(), &link[..link.len().min(200)]);
    }
    Ok(link.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
