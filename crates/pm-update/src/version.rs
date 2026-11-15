use std::fmt;

/// A `major.minor.patch` release version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parses `0.2.3` or `v0.2.3`; anything else (pre-release suffixes,
    /// missing or extra components, whitespace) is `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let mut next = || {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse::<u64>().ok()
        };
        let (major, minor, patch) = (next()?, next()?, next()?);
        if parts.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch })
    }

    /// The running version. In debug builds `PM_UPDATE_CURRENT` overrides it
    /// when it parses, so the update flow can be exercised.
    pub fn current() -> Version {
        #[cfg(debug_assertions)]
        if let Some(v) = std::env::var("PM_UPDATE_CURRENT").ok().and_then(|s| Version::parse(&s)) {
            return v;
        }
        Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is major.minor.patch")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The release-asset target name of this build, `None` on unsupported platforms.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub const TARGET: Option<&str> = Some("windows-x86_64");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const TARGET: Option<&str> = Some("linux-x86_64");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const TARGET: Option<&str> = Some("macos-aarch64");
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const TARGET: Option<&str> = Some("macos-x86_64");
#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
)))]
pub const TARGET: Option<&str> = None;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_order() {
        assert_eq!(Version::parse("v0.2.3"), Some(Version { major: 0, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("0.10.0").unwrap().to_string(), "0.10.0");
        assert!(Version::parse("0.10.0") > Version::parse("0.9.9"));
        for bad in ["", "v1", "1.2", "1.2.3-rc1", "1.2.3.4", "a.b.c", "v 1.2.3"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
    }
}
