use crate::release::{parse_release, url_allowed, Release};
use crate::version::{Version, TARGET};
use pm_core::{Error, Result};
use std::io::Read;
use std::time::Duration;

/// Where the newest published release is described.
pub const LATEST_URL: &str = "https://api.github.com/repos/inalto/partition-manager/releases/latest";

/// Upper bound for the release JSON.
const MAX_RELEASE_JSON: u64 = 1024 * 1024;
/// Redirect hops followed by hand.
const MAX_REDIRECTS: usize = 5;

/// Source of bytes for the updater; the real one talks HTTPS, tests use a map.
pub trait Fetch: Send + Sync {
    /// Opens `url` and returns its body. Fails for URLs the updater must not
    /// request and for any non-2xx answer.
    fn get(&self, url: &str) -> Result<Box<dyn Read + Send>>;
}

/// Outcome of asking whether a newer release exists.
#[derive(Clone, Debug, PartialEq)]
pub enum Check {
    UpToDate,
    Newer(Release),
}

/// Reads all of `r`, failing when more than `limit` bytes arrive.
pub fn read_limited(r: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| Error::Invalid(format!("read failed: {e}")))?;
    if buf.len() as u64 > limit {
        return Err(Error::Invalid(format!("response larger than {limit} bytes")));
    }
    Ok(buf)
}

/// Fetches the latest release and compares it with `current`. A draft,
/// pre-release or unparsable tag counts as up to date; a failure to fetch or
/// to understand the answer is an error.
pub fn check(fetch: &dyn Fetch, current: Version) -> Result<Check> {
    let body = read_limited(fetch.get(&latest_url())?, MAX_RELEASE_JSON)?;
    let json =
        std::str::from_utf8(&body).map_err(|_| Error::Parse("release JSON is not UTF-8".into()))?;
    Ok(match parse_release(json, TARGET)? {
        Some(r) if r.version > current => Check::Newer(r),
        _ => Check::UpToDate,
    })
}

/// Debug builds only: `PM_UPDATE_URL` replaces `LATEST_URL`.
#[cfg(debug_assertions)]
fn latest_url() -> String {
    std::env::var("PM_UPDATE_URL").unwrap_or_else(|_| LATEST_URL.to_string())
}

#[cfg(not(debug_assertions))]
fn latest_url() -> String {
    LATEST_URL.to_string()
}

/// Debug builds may talk to a local test server; release builds never.
#[cfg(debug_assertions)]
fn allowed(url: &str) -> bool {
    url_allowed(url) || url.starts_with("http://127.0.0.1:") || url.starts_with("http://127.0.0.1/")
}

#[cfg(not(debug_assertions))]
fn allowed(url: &str) -> bool {
    url_allowed(url)
}

fn host_of(url: &str) -> &str {
    url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("?")
}

/// The real HTTPS client.
pub struct HttpFetch {
    agent: ureq::Agent,
}

impl HttpFetch {
    pub fn new() -> HttpFetch {
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .user_agent(concat!("partition-manager/", env!("CARGO_PKG_VERSION")))
            .build();
        HttpFetch { agent }
    }
}

impl Default for HttpFetch {
    fn default() -> Self {
        HttpFetch::new()
    }
}

impl Fetch for HttpFetch {
    fn get(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        let mut url = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            if !allowed(&url) {
                return Err(Error::Invalid(format!("refusing to request {}", host_of(&url))));
            }
            let resp =
                match self.agent.get(&url).set("Accept", "application/vnd.github+json").call() {
                    Ok(r) => r,
                    Err(ureq::Error::Status(code, _)) => {
                        return Err(Error::Invalid(format!("{code} from {}", host_of(&url))));
                    }
                    Err(e) => {
                        return Err(Error::Invalid(format!(
                            "request to {} failed: {e}",
                            host_of(&url)
                        )));
                    }
                };
            let status = resp.status();
            if (300..400).contains(&status) {
                let next = resp.header("Location").ok_or_else(|| {
                    Error::Invalid(format!("{status} from {} without Location", host_of(&url)))
                })?;
                url = next.to_string();
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(Error::Invalid(format!("{status} from {}", host_of(&url))));
            }
            return Ok(Box::new(resp.into_reader()));
        }
        Err(Error::Invalid(format!("too many redirects from {}", host_of(&url))))
    }
}

#[cfg(test)]
pub(crate) struct MapFetch {
    files: std::collections::HashMap<String, Vec<u8>>,
    requested: std::sync::Mutex<Vec<String>>,
    /// When set, every stream fails with an I/O error after this many bytes.
    fail_after: Option<usize>,
}

/// Yields the first `n` bytes of `data`, then an I/O error.
#[cfg(test)]
struct BreaksAfter {
    data: std::io::Cursor<Vec<u8>>,
    left: usize,
}

#[cfg(test)]
impl Read for BreaksAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "stream broke"));
        }
        let max = buf.len().min(self.left);
        let n = self.data.read(&mut buf[..max])?;
        self.left -= n;
        Ok(n)
    }
}

#[cfg(test)]
impl MapFetch {
    pub(crate) fn new() -> MapFetch {
        MapFetch { files: Default::default(), requested: Default::default(), fail_after: None }
    }

    pub(crate) fn with(mut self, url: &str, bytes: impl AsRef<[u8]>) -> MapFetch {
        self.files.insert(url.to_string(), bytes.as_ref().to_vec());
        self
    }

    /// A copy of this fetcher whose streams fail after `n` bytes.
    pub(crate) fn failing_after(&self, n: usize) -> MapFetch {
        MapFetch {
            files: self.files.clone(),
            requested: Default::default(),
            fail_after: Some(n),
        }
    }

    /// URLs asked for so far, in order.
    pub(crate) fn requested(&self) -> Vec<String> {
        self.requested.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Fetch for MapFetch {
    fn get(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        self.requested.lock().unwrap().push(url.to_string());
        match self.files.get(url) {
            Some(b) => {
                let data = std::io::Cursor::new(b.clone());
                Ok(match self.fail_after {
                    // Only large bodies break: the small checksum file stays readable.
                    Some(left) if b.len() > left => Box::new(BreaksAfter { data, left }),
                    _ => Box::new(data),
                })
            }
            None => Err(Error::NotFound(url.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::*;

    #[test]
    fn check_reports_newer_equal_and_older() {
        let f = MapFetch::new().with(LATEST_URL, SAMPLE);
        assert!(
            matches!(check(&f, Version::parse("0.2.2").unwrap()), Ok(Check::Newer(r)) if r.version.to_string() == "0.2.3")
        );
        assert_eq!(check(&f, Version::parse("0.2.3").unwrap()).unwrap(), Check::UpToDate);
        assert_eq!(check(&f, Version::parse("0.3.0").unwrap()).unwrap(), Check::UpToDate);
        assert_eq!(f.requested(), vec![LATEST_URL; 3]);
    }

    #[test]
    fn check_fails_on_rate_limit_html_and_offline() {
        let v = Version::parse("0.2.2").unwrap();
        let limited = MapFetch::new().with(LATEST_URL, r#"{"message":"API rate limit exceeded"}"#);
        assert!(check(&limited, v).is_err());
        let html = MapFetch::new().with(LATEST_URL, "<html>sign in to the wifi</html>");
        assert!(check(&html, v).is_err());
        assert!(check(&MapFetch::new(), v).is_err());
    }

    #[test]
    fn a_draft_latest_is_up_to_date() {
        let f = MapFetch::new().with(LATEST_URL, SAMPLE_DRAFT);
        assert_eq!(check(&f, Version::parse("0.1.0").unwrap()).unwrap(), Check::UpToDate);
    }

    #[test]
    fn read_limited_stops_oversized_bodies() {
        assert_eq!(read_limited(&b"abc"[..], 3).unwrap(), b"abc");
        assert!(read_limited(&b"abcd"[..], 3).is_err());
    }

    #[test]
    fn http_fetch_refuses_urls_off_the_allow_list_without_connecting() {
        assert!(HttpFetch::new().get("https://example.com/x").is_err());
        assert!(HttpFetch::new().get("http://github.com/x").is_err());
    }
}
