//! The newest released version of a project, from GitHub.
//!
//! So a frontend can say a newer version is out (moho does, in its mentions
//! inbox). Here rather than in a window because it is a request off this
//! machine like any other, and goes the way requests that belong to no
//! account go: through Tor or the proxy when anything is routed.
//!
//! Only a real release counts: published, not a pre-release, and tagged
//! `vMAJOR.MINOR.PATCH`. A draft, a release candidate and a moving tag such
//! as the retired rolling `latest` are not versions anybody should be told
//! to install.

use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub struct Release {
    /// "1.0.1", without the v.
    pub version: String,
    pub name: String,
    /// The release's page.
    pub url: String,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
}

/// MAJOR.MINOR.PATCH from a tag, if it is exactly one.
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v')?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// The newest real release among what GitHub listed.
pub fn newest(listed: &serde_json::Value) -> Option<Release> {
    listed
        .as_array()?
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(true) && !r["prerelease"].as_bool().unwrap_or(true))
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            let version = parse_version(tag)?;
            Some((
                version,
                Release {
                    version: tag.trim_start_matches('v').to_string(),
                    name: r["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(tag).to_string(),
                    url: r["html_url"].as_str()?.to_string(),
                    published_at: r["published_at"].as_str().unwrap_or_default().to_string(),
                },
            ))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, release)| release)
}

/// The newest real release of `repo` ("owner/name") on GitHub, or None when it
/// has none yet.
pub async fn latest(repo: &str) -> Result<Option<Release>> {
    let valid = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) && !name.contains('/') => {}
        _ => bail!("\"{repo}\" is not a GitHub repository"),
    }
    let router = crate::net::route::router();
    router.ready_for_general().await.context("starting Tor or reaching the proxy")?;
    let client = router.client_if("releases", router.general_routed(), |builder| {
        builder
            .user_agent(concat!("nobilis/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(20))
    });
    let response = client
        .get(format!("https://api.github.com/repos/{repo}/releases?per_page=30"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for releases")?;
    if !response.status().is_success() {
        bail!("GitHub answered {} for {repo}'s releases", response.status());
    }
    let listed: serde_json::Value = response.json().await.context("reading GitHub's list of releases")?;
    Ok(newest(&listed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_versions_parse() {
        assert_eq!(parse_version("v1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("v1.10.2"), Some((1, 10, 2)));
        assert_eq!(parse_version("1.0.0"), None);
        assert_eq!(parse_version("v1.0.0-rc.1"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("v1.0"), None);
        assert_eq!(parse_version("v1.0.0.1"), None);
    }

    #[test]
    fn the_newest_real_release_is_picked() {
        let listed = serde_json::json!([
            { "tag_name": "latest", "name": "Latest build", "draft": false, "prerelease": false, "html_url": "https://x/latest", "published_at": "2026-10-09" },
            { "tag_name": "v1.1.0-rc.1", "name": "moho 1.1.0-rc.1", "draft": false, "prerelease": true, "html_url": "https://x/rc", "published_at": "2026-10-08" },
            { "tag_name": "v1.2.0", "name": "moho 1.2.0", "draft": true, "prerelease": false, "html_url": "https://x/draft", "published_at": "" },
            { "tag_name": "v1.0.10", "name": "moho 1.0.10", "draft": false, "prerelease": false, "html_url": "https://x/1010", "published_at": "2026-10-07" },
            { "tag_name": "v1.0.9", "name": "moho 1.0.9", "draft": false, "prerelease": false, "html_url": "https://x/109", "published_at": "2026-10-06" }
        ]);
        let newest = newest(&listed).unwrap();
        // 1.0.10 is newer than 1.0.9 - compared as numbers, not as text.
        assert_eq!(newest.version, "1.0.10");
        assert_eq!(newest.url, "https://x/1010");
        assert_eq!(super::newest(&serde_json::json!([])), None);
    }
}
