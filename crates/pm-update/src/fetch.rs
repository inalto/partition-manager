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

/// Debug builds may talk to a local test server (`http://127.0.0.1`, any port, no
/// userinfo); release builds never.
#[cfg(debug_assertions)]
fn allowed(url: &str) -> bool {
    url_allowed(url) || is_loopback(url)
}

#[cfg(debug_assertions)]
fn is_loopback(url: &str) -> bool {
    if url.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return false;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "http"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.host() == Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
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
mod tests {
    use super::*;
    use crate::testing::*;

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

    #[cfg(debug_assertions)]
    #[test]
    fn the_debug_loopback_exception_is_only_127_0_0_1() {
        assert!(allowed("http://127.0.0.1:8080/x"));
        assert!(allowed("http://127.0.0.1/x"));
        for bad in [
            "http://127.0.0.1:1@evil.com/",
            "http://127.0.0.1@evil.com/",
            "http://user:pw@127.0.0.1/x",
            "http://127.0.0.1.evil.com/x",
            "http://localhost:8080/x",
            "ftp://127.0.0.1/x",
        ] {
            assert!(!allowed(bad), "{bad}");
        }
    }

    /// Redirect handling of the real client, against a loopback server (debug builds admit
    /// `http://127.0.0.1`). No external network is used.
    #[cfg(debug_assertions)]
    mod redirects {
        use super::*;
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        /// A loopback HTTP/1.1 server answering each request with `answer(base, path)`
        /// (status line and extra headers, body) until stopped; counts the connections.
        struct Server {
            base: String,
            connections: Arc<AtomicUsize>,
            stop: Arc<AtomicBool>,
            thread: Option<std::thread::JoinHandle<()>>,
        }

        impl Server {
            fn start(answer: fn(&str, &str) -> (String, String)) -> Server {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                listener.set_nonblocking(true).unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let (connections, stop) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
                let (count, done, b) = (connections.clone(), stop.clone(), base.clone());
                let thread = std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(20);
                    while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                        let Ok((mut stream, _)) = listener.accept() else {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        };
                        count.fetch_add(1, Ordering::SeqCst);
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let mut req = Vec::new();
                        let mut byte = [0u8; 1];
                        while !req.ends_with(b"\r\n\r\n") && matches!(stream.read(&mut byte), Ok(1)) {
                            req.push(byte[0]);
                        }
                        let req = String::from_utf8_lossy(&req);
                        let path = req.split(' ').nth(1).unwrap_or("/").to_string();
                        let (head, body) = answer(&b, &path);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                });
                Server { base, connections, stop, thread: Some(thread) }
            }

            /// Stops the server and returns how many connections it accepted.
            fn finish(mut self) -> usize {
                self.stop.store(true, Ordering::SeqCst);
                self.thread.take().unwrap().join().unwrap();
                self.connections.load(Ordering::SeqCst)
            }
        }

        fn get(server: &Server, path: &str) -> Result<String> {
            let mut body = String::new();
            HttpFetch::new().get(&format!("{}{path}", server.base))?.read_to_string(&mut body).unwrap();
            Ok(body)
        }

        #[test]
        fn a_redirect_off_the_allow_list_is_refused_before_connecting() {
            let server = Server::start(|_, _| ("302 Found\r\nLocation: https://example.com/x".into(), String::new()));
            let e = get(&server, "/start").unwrap_err();
            assert!(e.to_string().contains("refusing to request example.com"), "{e}");
            assert_eq!(server.finish(), 1, "no second connection");
        }

        #[test]
        fn a_redirect_without_location_fails() {
            let server = Server::start(|_, _| ("302 Found".into(), String::new()));
            let e = get(&server, "/start").unwrap_err();
            assert!(e.to_string().contains("without Location"), "{e}");
            assert_eq!(server.finish(), 1);
        }

        #[test]
        fn a_redirect_loop_stops_after_the_hop_limit() {
            let server = Server::start(|base, _| (format!("302 Found\r\nLocation: {base}/again"), String::new()));
            let e = get(&server, "/start").unwrap_err();
            assert!(e.to_string().contains("too many redirects"), "{e}");
            assert_eq!(server.finish(), 1 + MAX_REDIRECTS);
        }

        #[test]
        fn an_allowed_redirect_is_followed_to_the_body() {
            let server = Server::start(|base, path| match path {
                "/start" => (format!("302 Found\r\nLocation: {base}/final"), String::new()),
                "/final" => ("200 OK".into(), "the body".into()),
                _ => ("500 Internal Server Error".into(), String::new()),
            });
            assert_eq!(get(&server, "/start").unwrap(), "the body");
            assert_eq!(server.finish(), 2);
        }

        #[test]
        fn a_final_error_status_is_reported() {
            let server = Server::start(|_, _| ("404 Not Found".into(), "nothing here".into()));
            match get(&server, "/start") {
                Err(Error::Invalid(m)) => assert!(m.contains("404"), "{m}"),
                other => panic!("unexpected {other:?}"),
            }
            assert_eq!(server.finish(), 1);
        }
    }
}
