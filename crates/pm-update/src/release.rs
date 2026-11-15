use crate::version::Version;
use pm_core::{Error, Result};
use serde::Deserialize;

/// The GitHub repository the releases come from, as a literal for `concat!`.
macro_rules! repo {
    () => {
        "inalto/partition-manager"
    };
}
pub(crate) use repo;

/// Every archive and checksum file is downloaded from below this address.
pub const DOWNLOAD_PREFIX: &str = concat!("https://github.com/", repo!(), "/releases/download/");

/// The page of the newest release: where to send the user when a release names no
/// usable page of its own.
pub const RELEASES_PAGE: &str = concat!("https://github.com/", repo!(), "/releases/latest");

/// `url` when it may be opened (see [`url_allowed`]), else [`RELEASES_PAGE`]: a release
/// page that is missing, empty or off the allow-list (from the API or a hand-edited
/// settings file) still leads somewhere useful.
pub fn release_page(url: &str) -> String {
    if url_allowed(url) {
        url.to_string()
    } else {
        RELEASES_PAGE.to_string()
    }
}

/// Whether `url` is one of this repository's release downloads (exactly below
/// [`DOWNLOAD_PREFIX`], also once `.` and `..` segments are resolved). Debug builds also
/// take a local test server (`http://127.0.0.1`).
fn is_release_download(url: &str) -> bool {
    #[cfg(debug_assertions)]
    if crate::fetch::is_loopback(url) {
        return true;
    }
    url_allowed(url)
        && url.starts_with(DOWNLOAD_PREFIX)
        && url::Url::parse(url).is_ok_and(|u| u.as_str().starts_with(DOWNLOAD_PREFIX))
}

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
            .filter(|a| is_release_download(&a.browser_download_url))
            .map(|a| Asset { name: a.name.clone(), url: a.browser_download_url.clone(), size: a.size })
    });
    let sums_url = raw
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.txt")
        .filter(|a| is_release_download(&a.browser_download_url))
        .map(|a| a.browser_download_url.clone());
    Ok(Some(Release {
        version,
        notes: raw.body.unwrap_or_default(),
        page_url: release_page(raw.html_url.as_deref().unwrap_or_default()),
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
    fn downloads_must_be_this_repositorys_release_downloads() {
        let ok = parse_release(SAMPLE, Some("linux-x86_64")).unwrap().unwrap();
        assert!(ok.archive.is_some() && ok.sums_url.is_some());
        let ours = "https://github.com/inalto/partition-manager/releases/download/";
        for other in [
            "https://github.com/evil/partition-manager/releases/download/",
            "https://github.com/inalto/partition-manager-fork/releases/download/",
            "https://objects.githubusercontent.com/inalto/partition-manager/releases/download/",
            "http://github.com/inalto/partition-manager/releases/download/",
            "https://github.com/inalto/partition-manager/releases/download/../../../evil/x/releases/download/",
            "https://github.com/inalto/partition-manager/releases/download/%2e%2e/%2e%2e/%2e%2e/evil/x/",
            "http://127.0.0.1.evil.com/",
        ] {
            let r = parse_release(&SAMPLE.replace(ours, other), Some("linux-x86_64")).unwrap().unwrap();
            assert!(r.archive.is_none() && r.sums_url.is_none(), "{other}");
            assert_eq!(r.version.to_string(), "0.2.3", "the release itself is still found");
        }
    }

    #[test]
    fn an_unusable_page_url_becomes_the_releases_page() {
        let page = "https://github.com/inalto/partition-manager/releases/tag/v0.2.3";
        assert_eq!(release_page(page), page);
        assert_eq!(RELEASES_PAGE, "https://github.com/inalto/partition-manager/releases/latest");
        for bad in ["", "   ", "http://github.com/x", "https://example.com/", "file:///etc/passwd"] {
            assert_eq!(release_page(bad), RELEASES_PAGE, "{bad:?}");
        }
        // a release without (or with an odd) html_url
        let none = SAMPLE.replace(&format!(r#""html_url": "{page}","#), "");
        assert_eq!(parse_release(&none, None).unwrap().unwrap().page_url, RELEASES_PAGE);
        let odd = SAMPLE.replace(page, "https://example.com/x");
        assert_eq!(parse_release(&odd, None).unwrap().unwrap().page_url, RELEASES_PAGE);
        assert_eq!(parse_release(SAMPLE, None).unwrap().unwrap().page_url, page);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_builds_also_take_downloads_from_a_local_server() {
        let ours = "https://github.com/inalto/partition-manager/releases/download/";
        let json = SAMPLE.replace(ours, "http://127.0.0.1:8080/dl/");
        let r = parse_release(&json, Some("linux-x86_64")).unwrap().unwrap();
        assert_eq!(
            r.archive.unwrap().url,
            "http://127.0.0.1:8080/dl/v0.2.3/partition-manager-0.2.3-linux-x86_64.tar.gz"
        );
        assert_eq!(r.sums_url.unwrap(), "http://127.0.0.1:8080/dl/v0.2.3/SHA256SUMS.txt");
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
