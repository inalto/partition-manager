# Self-Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The desktop application notices a newer GitHub release at startup and, on request, downloads, verifies and installs it over the running copy.

**Architecture:** A new GUI-free crate `pm-update` does everything that touches the network or the install folder (release lookup, download, checksum, unpack, swap), behind a `Fetch` trait so tests never use the network. `pm-gui` gains a settings file, an `UpdateState` inside `AppState` driven by `Cmd`s like everything else, a notice bar and three dialogs.

**Tech Stack:** Rust workspace; `ureq` 2 (rustls), `sha2`, `zip` (deflate only), `flate2` + `tar`, `serde`/`serde_json` (already in the workspace); egui/eframe 0.29.

**Spec:** `docs/superpowers/specs/2026-11-15-self-update-design.md`

## Global Constraints

- Release source: `https://api.github.com/repos/inalto/partition-manager/releases/latest`; `User-Agent: partition-manager/<version>`; no other identifying data.
- Only `https` URLs whose host is `github.com`, `api.github.com` or ends in `.githubusercontent.com` are ever requested, including every redirect hop.
- Asset names: `partition-manager-<version>-<target>.zip` for `windows-x86_64`; `.tar.gz` for `linux-x86_64`, `macos-aarch64`, `macos-x86_64`. Archive top-level folder: `partition-manager-<version>-windows-x86_64/` (zip), `<os>-<arch>/` (tar.gz). Checksums: `SHA256SUMS.txt`, `sha256sum` format.
- Files taken from an archive, by exact name only: `partition-manager`, `pm` (`.exe` on Windows), `README.md`, `LICENSE`, `CHANGELOG.md`.
- Automatic check: at most once every 24 hours (86 400 s); never in debug builds or tests unless `PM_UPDATE_CURRENT` is set (debug builds only); failures are logged, never shown.
- Nothing is downloaded before the user presses Update; both executables are swapped or neither; nothing from an archive that failed its checksum is unpacked.
- `#![forbid(unsafe_code)]` in `pm-update`. No system libraries beyond what CI installs today.
- Local builds in this worktree use the GNU toolchain files already copied in (`.cargo/config.toml`, `rust-toolchain.toml`, both git-ignored) and `CARGO_TARGET_DIR=F:/Apps/PartitionManager/target`. Files are CRLF on Windows: keep the line endings a file already has.
- Commit messages follow the repository style (`feat(update): ...`) and end with `Co-Authored-By: Claude Fable 5.2 <noreply@anthropic.com>`.

## Review Focus

1. GitHub answers with something that is not a release (rate-limit 403/429 JSON, a captive-portal HTML page): a failed check, silent when automatic, a plain message when manual. Test in Task 2.
2. The download is cancelled or breaks halfway (disk full, connection lost): no `.pm-update/` is left behind and the installed files are untouched; Update can be pressed again. Test in Task 3.
3. A stale `partition-manager.old` or `.pm-update/` from an earlier update is still there when the next update runs: it is replaced, not an error. Test in Task 5.
4. The newest release has no archive for this platform, or no `SHA256SUMS.txt`: the notice still appears and offers "Open the download page" instead of Update. Tests in Tasks 1 and 7.
5. The clock moved backwards (`last_check` is in the future): the automatic check runs instead of waiting. Test in Task 6.

---

### Task 1: `pm-update` crate, versions and release parsing

**Files:**
- Create: `crates/pm-update/Cargo.toml`, `crates/pm-update/src/lib.rs`, `crates/pm-update/src/version.rs`, `crates/pm-update/src/release.rs`
- Modify: `Cargo.toml` (workspace dependencies: `ureq = "2"`, `sha2 = "0.10"`, `zip = { version = "2", default-features = false, features = ["deflate"] }`, `flate2 = "1"`, `tar = "0.4"`)

**Interfaces:**
- Produces:
  - `pub struct Version { pub major: u64, pub minor: u64, pub patch: u64 }` deriving `Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord`, with `Display` (`0.2.3`), `pub fn parse(s: &str) -> Option<Version>` (accepts `0.2.3` and `v0.2.3`, nothing else), `pub fn current() -> Version` (`CARGO_PKG_VERSION`; in debug builds `PM_UPDATE_CURRENT` overrides it when it parses).
  - `pub const TARGET: Option<&str>`: `Some("windows-x86_64" | "linux-x86_64" | "macos-aarch64" | "macos-x86_64")` by `cfg(target_os, target_arch)`, `None` elsewhere.
  - `pub struct Asset { pub name: String, pub url: String, pub size: u64 }`
  - `pub struct Release { pub version: Version, pub notes: String, pub page_url: String, pub archive: Option<Asset>, pub sums_url: Option<String> }` (`Clone, Debug, PartialEq`)
  - `pub fn parse_release(json: &str, target: Option<&str>) -> pm_core::Result<Option<Release>>`: `Ok(None)` for a draft, a pre-release, or a tag that is not a `Version`; `Err(Error::Parse)` when the JSON is not a release object (no `tag_name`).
  - `pub fn url_allowed(url: &str) -> bool`

- [ ] **Step 1: Write the failing tests** (in the two modules)

```rust
#[test] fn version_parse_and_order() {
    assert_eq!(Version::parse("v0.2.3"), Some(Version { major: 0, minor: 2, patch: 3 }));
    assert_eq!(Version::parse("0.10.0").unwrap().to_string(), "0.10.0");
    assert!(Version::parse("0.10.0") > Version::parse("0.9.9"));
    for bad in ["", "v1", "1.2", "1.2.3-rc1", "1.2.3.4", "a.b.c", "v 1.2.3"] {
        assert_eq!(Version::parse(bad), None, "{bad}");
    }
}
#[test] fn release_picks_this_targets_archive() {
    // SAMPLE: tag_name v0.2.3, body "notes", html_url, draft false, prerelease false, assets:
    // the four archives + SHA256SUMS.txt, each with browser_download_url and size
    let r = parse_release(SAMPLE, Some("linux-x86_64")).unwrap().unwrap();
    assert_eq!(r.version.to_string(), "0.2.3");
    assert_eq!(r.archive.as_ref().unwrap().name, "partition-manager-0.2.3-linux-x86_64.tar.gz");
    assert!(r.sums_url.as_deref().unwrap().ends_with("/SHA256SUMS.txt"));
    assert_eq!(parse_release(SAMPLE, Some("windows-x86_64")).unwrap().unwrap().archive.unwrap().name,
               "partition-manager-0.2.3-windows-x86_64.zip");
}
#[test] fn release_without_this_target_or_sums_has_none() {
    assert!(parse_release(SAMPLE, None).unwrap().unwrap().archive.is_none());
    assert!(parse_release(SAMPLE, Some("freebsd-x86_64")).unwrap().unwrap().archive.is_none());
    assert!(parse_release(SAMPLE_NO_SUMS, Some("linux-x86_64")).unwrap().unwrap().sums_url.is_none());
}
#[test] fn drafts_prereleases_and_odd_tags_are_not_offered() {
    for j in [SAMPLE_DRAFT, SAMPLE_PRERELEASE, SAMPLE_TAG_NIGHTLY] {
        assert_eq!(parse_release(j, Some("linux-x86_64")).unwrap(), None);
    }
}
#[test] fn not_a_release_is_an_error() {
    assert!(parse_release(r#"{"message":"API rate limit exceeded"}"#, None).is_err());
    assert!(parse_release("<html>login</html>", None).is_err());
}
#[test] fn only_github_https_urls_are_allowed() {
    for ok in ["https://api.github.com/repos/x", "https://github.com/a/b/releases/download/v1/x.zip",
               "https://release-assets.githubusercontent.com/x", "https://objects.githubusercontent.com/x"] {
        assert!(url_allowed(ok), "{ok}");
    }
    for bad in ["http://github.com/x", "https://github.com.evil.example/x", "https://evilgithub.com/x",
                "https://example.com/github.com", "https://user@evil.example/github.com", "file:///etc/passwd", ""] {
        assert!(!url_allowed(bad), "{bad}");
    }
}
```

- [ ] **Step 2: Run** `cargo test -p pm-update`. Expected: compile errors (nothing defined).
- [ ] **Step 3: Implement** the crate (`pm-core`, `serde`, `serde_json` dependencies for now; `version.workspace = true`). The archive is the asset named exactly `partition-manager-<version>-<target>.zip` or `.tar.gz`. `url_allowed` parses scheme and host by hand (no URL crate): host is the text between `https://` and the first `/`, must contain no `@`, and any `:port` makes it not allowed.
- [ ] **Step 4: Run** `cargo test -p pm-update`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(update): pm-update crate with versions and release parsing`.

### Task 2: Fetching and the check

**Files:**
- Create: `crates/pm-update/src/fetch.rs`
- Modify: `crates/pm-update/src/lib.rs`, `crates/pm-update/Cargo.toml` (`ureq`)

**Interfaces:**
- Consumes: `Version`, `Release`, `parse_release`, `url_allowed`, `TARGET` (Task 1).
- Produces:
  - `pub trait Fetch: Send + Sync { fn get(&self, url: &str) -> pm_core::Result<Box<dyn std::io::Read + Send>>; }`
  - `pub struct HttpFetch;` with `pub fn new() -> HttpFetch`, implementing `Fetch`.
  - `pub const LATEST_URL: &str` (the Global Constraints URL).
  - `pub enum Check { UpToDate, Newer(Release) }` (`Clone, Debug, PartialEq`)
  - `pub fn check(fetch: &dyn Fetch, current: Version) -> pm_core::Result<Check>`
  - `pub fn read_limited(r: impl Read, limit: u64) -> pm_core::Result<Vec<u8>>` (error when more than `limit` bytes arrive).
  - Test helper, `#[cfg(test)] pub(crate) struct MapFetch`: URL → bytes map, records the URLs requested, `Error::NotFound` for the rest.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn check_reports_newer_equal_and_older() {
    let f = MapFetch::new().with(LATEST_URL, SAMPLE /* v0.2.3 */);
    assert!(matches!(check(&f, Version::parse("0.2.2").unwrap()), Ok(Check::Newer(r)) if r.version.to_string() == "0.2.3"));
    assert_eq!(check(&f, Version::parse("0.2.3").unwrap()).unwrap(), Check::UpToDate);
    assert_eq!(check(&f, Version::parse("0.3.0").unwrap()).unwrap(), Check::UpToDate); // no downgrade
}
#[test] fn check_fails_on_rate_limit_html_and_offline() {      // Review Focus 1
    let v = Version::parse("0.2.2").unwrap();
    assert!(check(&MapFetch::new().with(LATEST_URL, r#"{"message":"API rate limit exceeded"}"#), v).is_err());
    assert!(check(&MapFetch::new().with(LATEST_URL, "<html>sign in to the wifi</html>"), v).is_err());
    assert!(check(&MapFetch::new(), v).is_err());
}
#[test] fn a_draft_latest_is_up_to_date() { /* SAMPLE_DRAFT -> Check::UpToDate */ }
#[test] fn read_limited_stops_oversized_bodies() {
    assert_eq!(read_limited(&b"abc"[..], 3).unwrap(), b"abc");
    assert!(read_limited(&b"abcd"[..], 3).is_err());
}
#[test] fn http_fetch_refuses_urls_off_the_allow_list_without_connecting() {
    assert!(HttpFetch::new().get("https://example.com/x").is_err());
    assert!(HttpFetch::new().get("http://github.com/x").is_err());
}
```

- [ ] **Step 2: Run** `cargo test -p pm-update`. Expected: FAIL (undefined).
- [ ] **Step 3: Implement.** `check` reads at most 1 MiB of JSON. `HttpFetch`: a `ureq` agent with `redirects(0)`, 10 s connect and 30 s read timeouts, `User-Agent: partition-manager/<CARGO_PKG_VERSION>`, `Accept: application/vnd.github+json`; it follows up to 5 redirects by hand, checking `url_allowed` on every hop, and turns any non-2xx status into `Error::Invalid("<status> from <host>")`. In debug builds only, `PM_UPDATE_URL` replaces `LATEST_URL` and `http://127.0.0.1` URLs are allowed.
- [ ] **Step 4: Run** `cargo test -p pm-update`. Expected: PASS. Then `cargo build -p pm-update` to confirm `ureq`/rustls build with the GNU toolchain.
- [ ] **Step 5: Commit** `feat(update): release check over HTTPS behind a Fetch trait`.

### Task 3: Download and checksum

**Files:**
- Create: `crates/pm-update/src/download.rs`
- Modify: `crates/pm-update/src/lib.rs`, `crates/pm-update/Cargo.toml` (`sha2`; dev-dependency `tempfile`)

**Interfaces:**
- Consumes: `Fetch`, `Release`, `Asset`, `read_limited`, `MapFetch`; `pm_core::{ProgressSink, ProgressReport, CancelToken, Error}`.
- Produces:
  - `pub fn sum_for(sums: &str, name: &str) -> Option<String>` (lower-case hex; accepts `<hex>  name` and `<hex> *name`).
  - `pub fn sha256_hex(path: &Path) -> pm_core::Result<String>`
  - `pub fn download(fetch: &dyn Fetch, release: &Release, staging: &Path, progress: &dyn ProgressSink, cancel: &CancelToken) -> pm_core::Result<PathBuf>`: the verified archive inside `staging` (which must exist). Errors: `Error::Unsupported` when `archive` or `sums_url` is `None`; `Error::Cancelled`; `Error::Invalid` naming the cause otherwise. On any error it removes the partial archive.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn sum_lines_in_both_sha256sum_styles() {
    let s = "aa11  a.zip\nBB22 *b.tar.gz\n";
    assert_eq!(sum_for(s, "a.zip").as_deref(), Some("aa11"));
    assert_eq!(sum_for(s, "b.tar.gz").as_deref(), Some("bb22"));
    assert_eq!(sum_for(s, "c.zip"), None);
    assert_eq!(sum_for(s, "zip"), None);          // whole name, not a suffix
}
#[test] fn download_verifies_and_reports_progress() {
    // fixture(): a Release whose archive is 300 KiB of bytes served by MapFetch with a matching SHA256SUMS.txt
    let (f, rel, bytes) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let seen = RecordingProgress::default();
    let p = download(&f, &rel, dir.path(), &seen, &CancelToken::new()).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
    let last = seen.last();
    assert_eq!((last.done, last.total), (bytes.len() as u64, bytes.len() as u64));
    assert_eq!(f.requested()[0], rel.sums_url.clone().unwrap(), "checksums first");
}
#[test] fn wrong_checksum_missing_line_and_wrong_size_fail_and_leave_nothing() {
    for broken in [fixture_bad_sum(), fixture_no_line(), fixture_longer_than_announced(), fixture_shorter_than_announced()] {
        let dir = tempfile::tempdir().unwrap();
        assert!(download(&broken.0, &broken.1, dir.path(), &NullProgress, &CancelToken::new()).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
#[test] fn cancel_and_broken_stream_leave_nothing() {      // Review Focus 2
    let (f, rel, _) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let c = CancelToken::new(); c.cancel();
    assert!(matches!(download(&f, &rel, dir.path(), &NullProgress, &c), Err(Error::Cancelled)));
    assert!(download(&f.failing_after(1000), &rel, dir.path(), &NullProgress, &CancelToken::new()).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
#[test] fn no_archive_or_no_sums_is_unsupported() { /* Error::Unsupported, nothing requested */ }
```

- [ ] **Step 2: Run** `cargo test -p pm-update download`. Expected: FAIL.
- [ ] **Step 3: Implement.** `SHA256SUMS.txt` is read with a 64 KiB limit. The archive is streamed in 64 KiB chunks to `staging/<asset name>`, checking `cancel` and reporting `ProgressReport { phase: "Downloading", done, total: asset.size, .. }` per chunk; more bytes than `asset.size` or fewer at end of stream is an error. The hash is computed while streaming.
- [ ] **Step 4: Run** `cargo test -p pm-update`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(update): download with SHA-256 verification`.

### Task 4: Unpacking

**Files:**
- Create: `crates/pm-update/src/unpack.rs`
- Modify: `crates/pm-update/src/lib.rs`, `crates/pm-update/Cargo.toml` (`zip`, `flate2`, `tar`)

**Interfaces:**
- Produces:
  - `pub fn exe_name(base: &str, windows: bool) -> String` (`"pm"` → `"pm.exe"` when `windows`)
  - `pub const EXECUTABLES: [&str; 2] = ["partition-manager", "pm"];` `pub const DOCUMENTS: [&str; 3] = ["README.md", "LICENSE", "CHANGELOG.md"];`
  - `pub fn unpack(archive: &Path, into: &Path) -> pm_core::Result<Vec<String>>`: extracts the wanted files flat into `into` and returns their names. Format by extension (`.zip`, `.tar.gz`); the wanted executable names are those of the archive's platform (`.exe` for `.zip`). `Error::Invalid` when `partition-manager` is not in the archive.

- [ ] **Step 1: Write the failing tests** (fixtures are built in the tests with the `zip` and `tar` writers)

```rust
#[test] fn zip_and_targz_yield_the_five_files_flat() {
    for (archive, exes) in [(zip_fixture("partition-manager-0.2.3-windows-x86_64"), ["partition-manager.exe", "pm.exe"]),
                            (targz_fixture("linux-x86_64"), ["partition-manager", "pm"])] {
        let out = tempfile::tempdir().unwrap();
        let mut names = unpack(&archive, out.path()).unwrap(); names.sort();
        assert_eq!(names.len(), 5);
        for e in exes { assert_eq!(std::fs::read(out.path().join(e)).unwrap(), format!("bin:{e}").as_bytes()); }
    }
}
#[test] fn extra_nested_and_escaping_entries_are_ignored() {
    // top/evil.exe, top/sub/pm, ../pm, /abs/pm, top/../../pm, a second top-level folder with pm, a symlink named pm
    let out = tempfile::tempdir().unwrap();
    let names = unpack(&hostile_targz(), out.path()).unwrap();
    assert_eq!(names, ["partition-manager"]);
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1);
    assert!(!out.path().parent().unwrap().join("pm").exists());
}
#[test] fn archive_without_the_gui_executable_is_an_error() { /* only pm + docs -> Err */ }
#[test] fn truncated_or_garbage_archives_are_errors() { /* half a zip, half a tar.gz, 1 KiB of zeros named .zip */ }
#[cfg(unix)] #[test] fn executables_are_executable_documents_are_not() { /* mode & 0o111 */ }
```

- [ ] **Step 2: Run** `cargo test -p pm-update unpack`. Expected: FAIL.
- [ ] **Step 3: Implement.** An entry is taken only if it is a regular file whose path is exactly `<top>/<wanted name>`, where `<top>` is the first path component of the first entry and contains no separator, `.` or `..`. Each file is capped at 512 MiB. Unix executables get mode `0o755`.
- [ ] **Step 4: Run** `cargo test -p pm-update`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(update): unpack release archives by exact name`.

### Task 5: Installing over the running copy

**Files:**
- Create: `crates/pm-update/src/install.rs`
- Modify: `crates/pm-update/src/lib.rs`

**Interfaces:**
- Consumes: `download`, `unpack`, `EXECUTABLES`, `DOCUMENTS`, `exe_name`, `Fetch`, `Release`.
- Produces:
  - `pub const STAGING: &str = ".pm-update";`
  - `pub fn install_dir() -> pm_core::Result<PathBuf>` (folder of `std::env::current_exe()` after `canonicalize`; on Windows the `\\?\` prefix is stripped so the path reads normally in messages).
  - `pub enum InstallKind { Portable, Unavailable(String) }` and `pub fn install_kind(dir: &Path) -> InstallKind`: `Unavailable` with a one-line reason when `TARGET` is `None` or a probe file cannot be created in `dir`.
  - `pub fn swap(staged: &Path, dir: &Path) -> pm_core::Result<()>`
  - `pub fn clean_up(dir: &Path)` (removes `*.old` siblings of the executables and `dir/.pm-update`, ignoring errors).
  - `pub fn update(fetch: &dyn Fetch, release: &Release, dir: &Path, progress: &dyn ProgressSink, cancel: &CancelToken) -> pm_core::Result<()>`: staging folder, download, unpack, swap; the staging folder is removed on success and on every failure.
  - `pub fn restart(dir: &Path, args: &[std::ffi::OsString]) -> pm_core::Result<()>`: starts `dir/partition-manager[.exe]` with `args`, detached.

- [ ] **Step 1: Write the failing tests** (all on temporary folders; file contents are short marker strings)

```rust
#[test] fn swap_replaces_only_what_is_installed() {
    // installed: partition-manager(.exe) + README.md ; staged: all five
    swap(staged.path(), dir.path()).unwrap();
    assert_eq!(read(dir, EXE_GUI), "new gui");
    assert_eq!(read(dir, "README.md"), "new readme");
    assert!(!dir.path().join(EXE_PM).exists() && !dir.path().join("LICENSE").exists());
    assert_eq!(read(dir, &format!("{EXE_GUI}.old")), "old gui");
}
#[test] fn swap_replaces_both_executables_when_both_are_installed() { /* both new, both .old kept */ }
#[test] fn a_failing_second_executable_rolls_the_first_back() {
    // staged pm is missing although pm is installed -> Err; installed gui and pm unchanged, no .old left
}
#[test] fn stale_old_files_and_staging_are_replaced() {      // Review Focus 3
    // pre-create partition-manager.old ("ancient") and .pm-update/junk, then run update() with fixtures
    assert_eq!(read(dir, &format!("{EXE_GUI}.old")), "old gui");
    assert!(!dir.path().join(STAGING).exists());
}
#[test] fn update_end_to_end_with_fixture_release() { /* MapFetch + archive fixture for this platform -> new files in place */ }
#[test] fn failed_update_leaves_the_install_untouched_and_no_staging() { /* bad checksum */ }
#[test] fn clean_up_removes_old_files_and_staging() { /* and does not touch other files */ }
#[test] fn install_kind_reports_an_unwritable_folder() { /* a path that does not exist -> Unavailable */ }
```

- [ ] **Step 2: Run** `cargo test -p pm-update install`. Expected: FAIL.
- [ ] **Step 3: Implement.** Swap order: for each installed executable (GUI first), remove a stale `<name>.old`, rename `<name>` → `<name>.old`, rename staged → `<name>`; on failure undo the completed renames in reverse and return the error. Then copy the documents that are installed, ignoring errors. `update` removes a stale staging folder before creating a fresh one.
- [ ] **Step 4: Run** `cargo test -p pm-update`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(update): install over the running copy with rollback`.

### Task 6: Settings file

**Files:**
- Create: `crates/pm-gui/src/settings.rs`
- Modify: `crates/pm-gui/src/lib.rs`, `crates/pm-gui/Cargo.toml` (`serde`, `serde_json`, `pm-update`)

**Interfaces:**
- Produces:
  - `pub struct Found { pub version: String, pub page_url: String }`
  - `pub struct Settings { pub check_updates: bool, pub last_check: u64, pub skipped: Option<String>, pub found: Option<Found> }` (serde, `#[serde(default)]`, `Default` has `check_updates: true`).
  - `pub fn settings_dir() -> Option<PathBuf>` and `pub fn settings_dir_for(os: crate::logging::LogOs, app_data: Option<&OsStr>, xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf>`: `%APPDATA%\PartitionManager`; `$XDG_CONFIG_HOME/partition-manager` (absolute only) else `~/.config/partition-manager`; `~/Library/Application Support/PartitionManager`.
  - `pub fn load(path: &Path) -> Settings` (defaults when missing or unreadable), `pub fn save(&self, path: &Path) -> std::io::Result<()>` (creates the folder).
  - `pub fn check_due(&self, now: u64) -> bool`: `check_updates && (now < last_check || now - last_check >= 86_400)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn defaults_round_trip_and_unknown_fields() {
    assert!(Settings::default().check_updates);
    // save then load gives the same value; a file with an extra unknown field loads; "{" , "" and "[]" give defaults
}
#[test] fn folders_per_os() { /* the three paths above, relative XDG_CONFIG_HOME ignored, None without HOME */ }
#[test] fn check_is_due_after_a_day_never_when_off_and_when_the_clock_went_back() {   // Review Focus 5
    let s = Settings { last_check: 1_000_000, ..Default::default() };
    assert!(!s.check_due(1_000_000 + 86_399));
    assert!(s.check_due(1_000_000 + 86_400));
    assert!(s.check_due(999_999));
    assert!(!Settings { check_updates: false, ..s }.check_due(u64::MAX));
}
```

- [ ] **Step 2: Run** `cargo test -p pm-gui settings`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p pm-gui settings`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(gui): settings file for the update check`.

### Task 7: Update state in the application

**Files:**
- Create: `crates/pm-gui/src/update.rs`
- Modify: `crates/pm-gui/src/app.rs` (state, `Cmd`, `Dialog`, `handle`, `poll_job`, `modal`), `crates/pm-gui/src/tools/mod.rs` (`ToolWindow::running`), the seven `impl ToolWindow` files (`tools/benchmark.rs`, `tools/smart.rs`, `tools/space_analyzer.rs`, `tools/surface_test.rs`, `wizards/data_recovery.rs`, `wizards/partition_recovery.rs`, `wizards/rescue.rs`), `crates/pm-gui/src/lib.rs`, `crates/pm-platform/src/shell.rs`

**Interfaces:**
- Consumes: `pm_update::{Version, Release, Check, Fetch, check, update, install_kind, InstallKind, url_allowed}`, `Settings`, `Found`, `tools::Task`.
- Produces:
  - `ToolWindow::running(&self) -> bool`, a required method (no default): a worker of this window has not finished. Every implementer answers from its `Task` fields.
  - `AppState::anything_running(&self) -> bool`: `self.job.is_some() || self.windows.iter().any(|w| w.running())`.
  - `pub struct Notice { pub version: Version, pub page_url: String }`
  - `pub struct UpdateState` with public fields `settings: Settings`, `notice: Option<Notice>`, `hidden: bool`, `current: Version`, `can_install: bool`, `downloading: Option<Task<Version>>`, and constructors `UpdateState::new(fetch: Arc<dyn Fetch>, settings_path: Option<PathBuf>, install_dir: Option<PathBuf>, current: Version) -> UpdateState` (loads the settings, shows the remembered `found` if still newer and not skipped, drops it otherwise) and `UpdateState::disabled()` (no path, no folder, a `Fetch` that always fails; used by `AppState::new`).
  - `AppState.update: UpdateState`, `AppState.quit: bool`, `AppState::with_update(self, u: UpdateState) -> AppState`.
  - `UpdateState::start_check(&mut self, manual: bool)` and `AppState::start_auto_check(&mut self, now: u64)` (starts only when `settings.check_due(now)`).
  - `Cmd`: `CheckUpdates`, `SetCheckUpdates(bool)`, `UpdateWhatsNew`, `UpdateAsk`, `UpdateStart`, `UpdateCancel`, `UpdateSkip`, `UpdateHide`, `UpdateRestart`, `OpenUrl(String)`.
  - `Dialog`: `WhatsNew { release: Release }`, `UpdateConfirm { release: Release }`, `Updated { version: Version }`.
  - `pm_platform::shell::open_url(url: &str) -> pm_core::Result<()>` (same launcher as `open_in_file_manager`; refuses anything not starting with `https://`).

**Behaviour to implement (each line has a test below):**
- A finished check that is `Newer` and not skipped sets `notice`, stores `found` and `last_check` and saves; `UpToDate` clears `found`; a failure changes nothing and, when automatic, shows nothing. A manual check ignores `skipped`, and toasts "You have the latest version (x.y.z)" or opens `Dialog::Error` with the reason.
- `UpdateWhatsNew` / `UpdateAsk` open their dialog from the release found in this session, or run a check first when only the remembered version is known.
- `UpdateAsk` and `UpdateRestart` do nothing but toast "Wait for the running work to finish first" while `anything_running()`.
- `UpdateStart` spawns the download/install `Task<Version>`; while it runs `modal()` is true and `anything_running()` is true. Success opens `Dialog::Updated` and clears the notice and `found`; failure opens `Dialog::Error` with the reason and the release page URL; `UpdateCancel` cancels it and shows nothing.
- `UpdateSkip` stores the version in `skipped`, clears the notice, saves. `UpdateHide` sets `hidden`. `SetCheckUpdates` saves.
- `UpdateRestart` calls `pm_update::restart` with the arguments this process was started with (`std::env::args_os().skip(1)`) and sets `quit`; an error opens `Dialog::Error`.
- `can_install` is false when there is no install folder, `install_kind` is `Unavailable`, or the release has no archive or no checksums.

- [ ] **Step 1: Write the failing tests** in `update.rs` (a `MapFetch`-like fake defined in this module's tests, serving sample JSON and an archive fixture for the host platform; `mock_state()` moves to `test_util.rs` if it is not reachable from here)

```rust
#[test] fn newer_release_shows_the_notice_and_is_remembered() {
    let (mut s, path) = state_with(fake_release("0.9.0"), "0.2.2");
    s.update.start_check(false); wait_update(&mut s);
    assert_eq!(s.update.notice.as_ref().unwrap().version.to_string(), "0.9.0");
    assert!(s.dialog.is_none() && s.toast.is_none());
    let saved = settings::load(&path);
    assert_eq!(saved.found.unwrap().version, "0.9.0");
    assert!(saved.last_check > 0);
    // a second start that does not check still shows it
    let again = UpdateState::new(never_fetch(), Some(path), None, v("0.2.2"));
    assert!(again.notice.is_some());
}
#[test] fn equal_skipped_and_failed_checks_show_nothing() {
    // same version -> no notice, found cleared; skipped "0.9.0" -> no notice; fetch error -> no notice,
    // no dialog, no toast, last_check unchanged (0)
}
#[test] fn manual_check_reports_latest_or_the_error_and_ignores_skip() {
    // up to date -> toast contains "latest version"; error -> Dialog::Error; skipped version -> notice shown
}
#[test] fn skip_hide_and_the_checkbox_are_saved() {
    // UpdateSkip -> notice None, settings.skipped == Some("0.9.0") on disk; UpdateHide -> hidden;
    // SetCheckUpdates(false) -> on disk, and start_auto_check(u64::MAX / 2) starts nothing
}
#[test] fn update_and_restart_wait_for_running_work() {
    // with s.job set (start_refresh) and, separately, with a window whose running() is true:
    // UpdateAsk -> no dialog, toast; UpdateRestart -> quit stays false
}
#[test] fn update_installs_and_offers_restart() {
    // install_dir = tempdir holding an old partition-manager; UpdateAsk -> Dialog::UpdateConfirm;
    // UpdateStart -> modal() and anything_running(); wait -> Dialog::Updated { 0.9.0 }, file replaced,
    // notice None, settings.found None
}
#[test] fn failed_install_reports_and_changes_nothing() { /* bad checksum fixture -> Dialog::Error containing the page URL; old file intact */ }
#[test] fn no_archive_or_unwritable_folder_cannot_install() {      // Review Focus 4
    // release without this platform's asset, release without SHA256SUMS, install_dir None: notice shown, can_install false
}
#[test] fn open_url_refuses_non_https() { assert!(pm_platform::shell::open_url("file:///x").is_err()); }
```

- [ ] **Step 2: Run** `cargo test -p pm-gui update`. Expected: FAIL.
- [ ] **Step 3: Implement.** Checks and the install run on `tools::Task` workers owned by `UpdateState` (not the job slot); `AppState::poll_job` also polls them. `OpenUrl` calls `open_url` only for URLs passing `pm_update::url_allowed`, and toasts the URL when the launcher fails.
- [ ] **Step 4: Run** `cargo test -p pm-gui`. Expected: PASS (all existing tests too).
- [ ] **Step 5: Commit** `feat(gui): update state, commands and gating`.

### Task 8: What the user sees, and startup

**Files:**
- Create: `crates/pm-gui/src/dialogs/update.rs`
- Modify: `crates/pm-gui/src/main_window.rs`, `crates/pm-gui/src/dialogs/mod.rs`, `crates/pm-gui/src/views/toolbar.rs`, `crates/pm-gui/src/args.rs`, `crates/pm-gui/src/main.rs`

**Interfaces:**
- Consumes: everything Task 7 produces; `tools::progress_panel`, `dialogs::progress::activity_bar`.
- Produces: `Args.no_update_check: bool` (`--no-update-check`); `pub fn notice_text(n: &Notice, current: Version) -> String`; `pub fn confirm_lines(release: &Release, dir: &Path, pending: usize, open_windows: usize) -> Vec<String>`.

**Exact copy:**
- Notice: `Partition Manager {new} is available (you have {current}).` Buttons: `What's new`, `Update` (or `Open the download page` when `!can_install`), `Skip this version`, and a small `x` with hover text `Hide until the next start`. Neutral colours (`ui.visuals().faint_bg_color` fill, normal text), drawn in the central panel above the error banner, inside the same enabled scope; not drawn when `hidden`.
- What's new window: title `What's new in {version}`, the notes in a scroll area, `Open the release page`, `Close`.
- Confirmation: title `Update to {version}`; lines `Partition Manager {version} will be downloaded from GitHub and installed in {dir}.`; when `pending > 0` or `open_windows > 0`: `The {pending} pending operations and the open windows are dropped when the application restarts.` (drop the part that is zero; singular `operation` for 1). Buttons `Update`, `Cancel`.
- Download window (modal): title `Updating to {version}`; `activity_bar` while `total == 0`, otherwise `progress_panel` with byte counts; `Cancel`.
- Done: `Updated to {version}. Restart Partition Manager to use it.` Buttons `Restart now`, `Later`.
- Settings menu, after the dry-run checkbox: checkbox `Check for updates at startup`, button `Check for updates now` (closes the menu).
- Usage text gains `  --no-update-check   Do not look for a newer release at startup`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn notice_and_confirmation_copy() {
    assert_eq!(notice_text(&notice("0.2.3"), v("0.2.2")), "Partition Manager 0.2.3 is available (you have 0.2.2).");
    let l = confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 0, 0);
    assert_eq!(l.len(), 1); assert!(l[0].contains("/opt/pm"));
    assert!(confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 1, 0)[1].starts_with("The 1 pending operation is dropped"));
    assert!(confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 3, 2)[1].starts_with("The 3 pending operations and the open windows are dropped"));
    assert!(confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 0, 1)[1].starts_with("The open windows are dropped"));
}
#[test] fn no_update_check_flag_parses() { assert!(args::parse(["--no-update-check"]).unwrap().no_update_check); }
```

Extend the existing headless frame test in `main_window.rs` (or add one beside it) so that a frame is drawn with a notice set, with each of the three new dialogs open, and with a download running, without panicking.

- [ ] **Step 2: Run** `cargo test -p pm-gui`. Expected: FAIL.
- [ ] **Step 3: Implement.** `main_window::run` builds `UpdateState::new(Arc::new(HttpFetch::new()), settings_dir().map(|d| d.join("settings.json")), install_dir().ok(), Version::current())`, calls `pm_update::clean_up` on the install folder, and calls `start_auto_check(now)` unless `args.no_update_check`, or the build is a debug build without `PM_UPDATE_CURRENT`. `draw` requests a repaint every 100 ms while an update worker runs, and sends `ViewportCommand::Close` when `state.quit` is set. Closing the window while the install is running is refused like Apply ("The update is being installed: wait for it to finish or cancel it first").
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(gui): update notice, dialogs, settings entries and startup check`.

### Task 9: Documents and the live check

**Files:**
- Modify: `README.md`, `CHANGELOG.md` (an `## [Unreleased]` section above 0.2.2), `docs/wiki/Getting-Started.md`

- [ ] **Step 1: README "Updates" section** after "Downloads": what is contacted (`api.github.com` and GitHub's download hosts, a generic user agent, nothing else), when (at startup, at most once a day), what Update does (download, SHA-256 check against `SHA256SUMS.txt`, replaces the files already in the folder, restart), how to turn it off (Settings, `--no-update-check`), that releases are not signed, and that `pm` has no check of its own. Two lines in the wiki page pointing at it. CHANGELOG "Added" entry.
- [ ] **Step 2: Whole-workspace check.** Run `cargo test --workspace` and `cargo build --release -p pm-gui`. Expected: all tests pass, release build succeeds; note the size of `partition-manager.exe` before and after in the commit message.
- [ ] **Step 3: Live test on Windows** (the only test that uses the network; never on a real physical disk: start with `--disk-file` on a scratch image). Copy a debug build of `partition-manager.exe` and `pm.exe` into an empty folder and start it from an elevated terminal (the released build it restarts into requires administrator rights, and a non-elevated process cannot start it) with `PM_UPDATE_CURRENT=0.2.0`: the notice names the real latest release; What's new shows its notes; Update installs it; Restart now starts the released build; the `.old` files are gone after that start; `pm --version` in the folder prints the new version. Record the result in `docs/superpowers/reports/sdd-ledger.md`. If the test boxes are reachable, repeat on Linux and macOS and record that too; if not, say so in the ledger.
- [ ] **Step 4: Commit** `docs: updates section, changelog, live self-update check`.
- [ ] **Step 5: Open a pull request** so CI builds the new dependencies on Windows (MSVC), Linux and macOS before anything is merged, and run the Release workflow by hand on the branch (`workflow_dispatch`, which builds without publishing) to confirm the four release targets, including the macOS x86_64 cross-build, still build.
