# Self-update: design

Date: 2026-11-15. Status: awaiting approval.

## Intent

When a newer release of Partition Manager exists on GitHub, the desktop application says so
and offers to download it and install it over the running copy, so that nobody has to fetch
and unpack an archive by hand and users do not stay on old versions without knowing.

Decided by the owner: the check runs by itself at startup; the download and install are
built into the application (no system tools, no ready-made self-update library). An
installer is wanted afterwards as a separate project (see "What comes next").

## Goals

- At startup the GUI checks, in the background, whether a newer release exists (at most once
  every 24 hours) and shows a notice when there is one.
- From the notice the user can read what is new, update, skip that version, or dismiss.
- Update downloads the archive of this platform, verifies it, replaces `partition-manager`
  and `pm` where they are installed, and offers to restart.
- The check can be turned off, and can be run by hand.
- Works on the four released targets: windows-x86_64, linux-x86_64, macos-aarch64,
  macos-x86_64.

## Non-goals

- Signed releases or signature verification (a later step; see Trust).
- An update check or an `update` command in the `pm` command line (it is replaced along with
  the GUI, nothing more).
- Background or silent installation, delta updates, release channels, pre-releases,
  downgrades.
- Persisting other settings (theme, dry run) in the new settings file.
- The installer itself.

## Anti-goals

- Nothing is downloaded or replaced before the user presses Update.
- No update while a job is running; the user is warned that pending operations are dropped
  by the restart.
- The installed copy is never left half replaced: both executables are swapped, or neither.
- Nothing from an archive that failed its checksum is ever unpacked or run.
- A failed or offline automatic check is silent (log only): it never blocks or delays
  startup and never shows an error.

## Constraints (how things are today)

- The application has no network code, no saved settings and no installer.
- Releases are produced by `.github/workflows/release.yml` on `v*` tags, on
  `github.com/inalto/partition-manager`. Assets:
  - `partition-manager-<version>-windows-x86_64.zip`, top-level folder
    `partition-manager-<version>-windows-x86_64/` with `pm.exe`, `partition-manager.exe`,
    `README.md`, `LICENSE`, `CHANGELOG.md`;
  - `partition-manager-<version>-<os>-<arch>.tar.gz` for `linux-x86_64`, `macos-aarch64`,
    `macos-x86_64`, top-level folder `<os>-<arch>/` with `pm`, `partition-manager` and the
    same documents;
  - `SHA256SUMS.txt` (`sha256sum` format) covering every archive.
  The release body is the CHANGELOG section of that version.
- The GUI runs as administrator on Windows (manifest) and normally as root on Linux/macOS.
  A running `.exe` cannot be overwritten on Windows but can be renamed.
- GUI work runs on worker threads through `jobs.rs`; the UI polls once per frame. There is
  one job slot (`AppState::busy()`), and tool windows and wizards run their own workers
  through `Task::spawn` outside it, some of them writing (rescue copy, writing a partition
  table, restoring files); `main_window.rs` already asks the windows whether one is writing
  before closing.
- Builds: MSVC or GNU (WinLibs) on Windows, stock toolchains on Linux/macOS, an x86_64
  macOS cross-build. New dependencies must build on all of them without system libraries
  beyond what CI already installs.

## Approaches

1. **Built into the application (chosen).** An HTTPS client, archive unpacking and SHA-256
   as Rust dependencies; the application does the whole update. Same behaviour on the three
   systems, testable against a fake release source. Cost: the first network dependencies,
   binaries a few MB larger.
2. **System tools** (`curl`, `tar`). No new dependencies, but it runs whatever is first on
   the PATH with administrator rights and fails where the tools are missing. Rejected for a
   disk tool.
3. **A self-update library.** Less code, but a much heavier network stack and a
   one-binary-per-release model that does not fit two executables plus a checksum file.
   Rejected.

The owner chose 1.

## Design

### A new crate, `pm-update` (no GUI code)

- **Version**: `major.minor.patch` parsed from the tag (`v0.2.3`) and from
  `CARGO_PKG_VERSION`; ordered numerically. A tag that does not parse, a pre-release or a
  draft is never offered.
- **Release lookup**: `GET https://api.github.com/repos/inalto/partition-manager/releases/latest`
  with a generic `User-Agent` (`partition-manager/<version>`) and no other identifying data.
  From the JSON: tag, body (the notes), page URL, and the assets. The asset for this build
  is picked by the target suffix compiled in with `cfg` (`windows-x86_64`, `linux-x86_64`,
  `macos-aarch64`, `macos-x86_64`); on any other target no asset matches.
- **Result of a check**: up to date, or a newer release with its notes, page URL and
  (when present) the archive and `SHA256SUMS.txt` URLs.
- **Download**: `SHA256SUMS.txt` first, then the archive, streamed into a staging folder
  created inside the install folder (`.pm-update/`, same volume, so the swap is a rename),
  with progress and cancellation through the existing `ProgressSink` / `CancelToken`. The
  size announced by the API bounds the download.
- **Verify**: SHA-256 of the archive must equal its line in `SHA256SUMS.txt`; otherwise the
  staging folder is removed and the update fails with a clear message.
- **Unpack**: only the two executables and the three documents are taken from the archive's
  top-level folder, by exact name; anything else, and any path that would leave the staging
  folder, is ignored. Executable bits are set on Unix.
- **Install folder**: the folder of the running executable, after resolving symlinks.
- **Swap** (designer's call): only files already present in the install folder are
  replaced, nothing is added. The running `partition-manager` is always replaced; `pm` only
  if it is there; `README.md`, `LICENSE`, `CHANGELOG.md` only if they are there (so a copy
  placed alone in a shared `bin` folder gets no extra files). Each executable is renamed to
  `<name>.old` and the new one renamed into place. If any step fails, the files already
  moved are moved back and the update fails. Documents are replaced afterwards, best
  effort. The staging folder is removed.
- **Clean-up**: at startup, `*.old` files and a left-over `.pm-update/` next to the
  executable are removed (best effort; on Windows the old executable can only go once the
  old process has exited).
- **Network access behind a trait**, so tests run against an in-memory source. Only
  `https` URLs on `github.com`, `api.github.com` and `*.githubusercontent.com` (where
  GitHub redirects asset downloads) are followed.
- **Install kind**: a single place decides how this copy is updated. Today it always
  answers "portable: swap the files in place" or, when the install folder is not writable,
  "cannot update here". The installer project adds a third answer.

### Settings file

A small JSON file in the per-user configuration folder (`%APPDATA%\PartitionManager\` on
Windows, `$XDG_CONFIG_HOME/partition-manager/` or `~/.config/partition-manager/` on Linux,
`~/Library/Application Support/PartitionManager/` on macOS): check at startup (default on),
time of the last successful check, the version the user chose to skip, and the newest
release found so far (version and page URL). A missing or unreadable file means the
defaults; a failed write is logged and otherwise ignored.

Designer's calls: the release found is remembered so that the notice returns at every start
until the user updates or skips it, not only on the one start per day that checks; it is
dropped once the running version is equal or newer. Notes and download URLs are not stored:
What's new and Update fetch the release again. A failed or offline check does not record
its time, so the next start tries again.

### In the application

- **Startup**: when checking is on, the last check is older than 24 hours, and the
  application was not started with `--no-update-check`, `main.rs` starts the check on its
  own thread. It does not use the job slot, so the application is not "busy" meanwhile.
  Debug builds and tests never check by themselves.
- **Notice**: when a newer, not skipped version is found, a neutral notice bar above the
  disk table (where the error banner goes, but not styled as an error):

  ```
  Partition Manager 0.2.3 is available (you have 0.2.2).   [What's new] [Update] [Skip this version] [x]
  ```

  - What's new: a window with the release notes and a link to the release page.
  - Update: a confirmation naming the version and the install folder, with a warning when
    operations are pending or tool windows or wizards are open ("the N pending operations
    and the open windows are dropped when the application restarts"). Then the progress
    window (activity bar until the size is known, then percentage, Cancel), then "Updated
    to 0.2.3" with Restart now / Later.
  - Designer's call: Update and Restart now are both unavailable while anything is
    running: the job slot, or a worker of any tool window or wizard, writing or not. One
    question ("is anything running?") answers both, and it must cover the `Task::spawn`
    workers, not only `busy()`.
  - When this copy cannot be updated in place (folder not writable, unknown target), Update
    is replaced by "Open the download page".
  - Skip this version: remembered in the settings; the notice returns only for a later one.
  - x: hides the notice until the next start.
- **Restart**: the new `partition-manager` is started with the original arguments and the
  current process exits.
- **Settings menu**: a "Check for updates at startup" checkbox and "Check for updates
  now". The manual check ignores the 24-hour limit and the skipped version, and reports
  "You have the latest version" or the error.
- **Failures** of a download or install are shown in a dialog with the reason and the link
  to the release page; the installed copy is unchanged.

### Trust

Downloads come only from the project's GitHub releases over HTTPS and must match the
checksum published in the same release. This protects against a corrupted or truncated
download and against anything outside GitHub; it does not protect against a compromised
GitHub account or repository, which is the same trust a manual download has today. Signed
releases are the answer to that and are left for later.

### Documentation

README: a short "Updates" section (what is contacted, when, how to turn it off). CHANGELOG
entry. No change to the release workflow is needed.

### Testing

- `pm-update`: version ordering; picking the asset from sample API JSON; checksum match and
  mismatch; unpacking zip and tar.gz fixtures built in the tests, including an archive with
  extra and path-escaping entries; swap, rollback on a failing second file, and clean-up, on
  temporary folders; the URL allow-list.
- GUI state tests: the notice appears for a newer version and not for a skipped or equal
  one, and again on a start that does not check; Skip and the settings checkbox are saved;
  Update and Restart are unavailable while the job slot or a tool-window worker is running;
  the fallback when the folder is not writable.
- By hand, once, on Windows: a build with a lowered version updates itself to the real
  latest release and restarts. Linux and macOS on the test boxes if available.

## Left to the builder

- The exact crates and versions (a small blocking HTTPS client with a pure-Rust TLS stack
  such as `ureq` with `rustls`, `sha2`, `zip` with deflate only, `flate2` + `tar`), provided
  they build on every CI target and with the GNU toolchain on Windows.
- A debug-build-only way to point the check at another URL or pretend an older version, for
  the manual test. Nothing of the kind in release builds.
- Module layout inside `pm-update` and the GUI, names, and the wording of messages beyond
  those given above.

## What comes next (not in this design)

- **Installer** (requested by the owner, after this): once it exists, a copy that was
  installed by it must update by downloading and running the new installer instead of
  swapping files; the "install kind" decision above is where that plugs in.
- The tools group in the side panel (separate, small).
- Signed releases.
