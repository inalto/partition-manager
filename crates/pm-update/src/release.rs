use crate::version::Version;
use pm_core::{Error, Result};
use serde::Deserialize;

/// A downloadable release asset.
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// A published release offered for update.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    pub notes: String,
    pub page_url: String,
    /// The archive for the requested target, if the release has one.
    pub archive: Option<Asset>,
    /// URL of `SHA256SUMS.txt`, if the release has one.
    pub sums_url: Option<String>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

/// Parses a GitHub "latest release" response. `Ok(None)` for a draft, a
/// pre-release or a tag that is not a `Version`; `Err(Error::Parse)` when the
/// JSON is not a release object.
pub fn parse_release(json: &str, target: Option<&str>) -> Result<Option<Release>> {
    let raw: RawRelease =
        serde_json::from_str(json).map_err(|e| Error::Parse(format!("release JSON: {e}")))?;
    if raw.draft || raw.prerelease {
        return Ok(None);
    }
    let Some(version) = Version::parse(&raw.tag_name) else {
        return Ok(None);
    };
    let archive = target.and_then(|t| {
        let ext = if t.starts_with("windows-") { "zip" } else { "tar.gz" };
        let want = format!("partition-manager-{version}-{t}.{ext}");
        raw.assets
            .iter()
            .find(|a| a.name == want)
            .map(|a| Asset { name: a.name.clone(), url: a.browser_download_url.clone(), size: a.size })
    });
    let sums_url =
        raw.assets.iter().find(|a| a.name == "SHA256SUMS.txt").map(|a| a.browser_download_url.clone());
    Ok(Some(Release {
        version,
        notes: raw.body.unwrap_or_default(),
        page_url: raw.html_url.unwrap_or_default(),
        archive,
        sums_url,
    }))
}

/// Whether `url` may be requested: `https` on `github.com`, `api.github.com`
/// or a `*.githubusercontent.com` host, no userinfo, no port. The URL is
/// parsed with the same WHATWG parser ureq uses, so the host checked is the
/// host connected to; control characters, spaces and backslashes are refused
/// outright.
pub fn url_allowed(url: &str) -> bool {
    if url.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return false;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
    {
        return false;
    }
    // The parser drops a default port (`:443`); an explicit port is refused anyway.
    let authority = url.split_once("://").map_or("", |(_, r)| r);
    if authority.split(['/', '?', '#']).next().is_some_and(|a| a.contains(':')) {
        return false;
    }
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            host == "github.com"
                || host == "api.github.com"
                || host.strip_suffix(".githubusercontent.com").is_some_and(|p| !p.is_empty())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[test]
    fn release_picks_this_targets_archive() {
        let r = parse_release(SAMPLE, Some("linux-x86_64")).unwrap().unwrap();
        assert_eq!(r.version.to_string(), "0.2.3");
        assert_eq!(r.archive.as_ref().unwrap().name, "partition-manager-0.2.3-linux-x86_64.tar.gz");
        assert!(r.sums_url.as_deref().unwrap().ends_with("/SHA256SUMS.txt"));
        assert_eq!(
            parse_release(SAMPLE, Some("windows-x86_64")).unwrap().unwrap().archive.unwrap().name,
            "partition-manager-0.2.3-windows-x86_64.zip"
        );
    }

    #[test]
    fn release_without_this_target_or_sums_has_none() {
        assert!(parse_release(SAMPLE, None).unwrap().unwrap().archive.is_none());
        assert!(parse_release(SAMPLE, Some("freebsd-x86_64")).unwrap().unwrap().archive.is_none());
        assert!(parse_release(SAMPLE_NO_SUMS, Some("linux-x86_64")).unwrap().unwrap().sums_url.is_none());
    }

    #[test]
    fn drafts_prereleases_and_odd_tags_are_not_offered() {
        for j in [SAMPLE_DRAFT, SAMPLE_PRERELEASE, SAMPLE_TAG_NIGHTLY] {
            assert_eq!(parse_release(j, Some("linux-x86_64")).unwrap(), None);
        }
    }

    #[test]
    fn not_a_release_is_an_error() {
        assert!(parse_release(r#"{"message":"API rate limit exceeded"}"#, None).is_err());
        assert!(parse_release("<html>login</html>", None).is_err());
    }

    #[test]
    fn only_github_https_urls_are_allowed() {
        for ok in [
            "https://api.github.com/repos/x",
            "https://github.com/a/b/releases/download/v1/x.zip",
            "https://release-assets.githubusercontent.com/x",
            "https://objects.githubusercontent.com/x",
        ] {
            assert!(url_allowed(ok), "{ok}");
        }
        for bad in [
            "http://github.com/x",
            "https://github.com.evil.example/x",
            "https://evilgithub.com/x",
            "https://example.com/github.com",
            "https://user@evil.example/github.com",
            "https://evil.com?.githubusercontent.com",
            "https://evil.com#.githubusercontent.com",
            "https://evil.com\\.githubusercontent.com",
            "https://evil.com\\@github.com/x",
            "https://github.com\t.evil.com/x",
            "https://evil.com\n.githubusercontent.com/x",
            "https://github.com./x",
            "https://github.com:443/x",
            "https://:pw@github.com/x",
            " https://github.com/x",
            "file:///etc/passwd",
            "",
        ] {
            assert!(!url_allowed(bad), "{bad}");
        }
    }
}
