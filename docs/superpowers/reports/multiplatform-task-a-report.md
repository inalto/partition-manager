# Multi-platform Task A report: portability audit, POSIX bench target, CI matrix

## What changed

- `Platform` trait: additive `os_name()` and `elevation_hint()` with host-derived defaults
  (`host_os_name()`, `host_elevation_hint()`, constants `ELEVATION_HINT_WINDOWS` /
  `ELEVATION_HINT_UNIX`). `WindowsPlatform` overrides both; `NullPlatform` reports `"none"`.
- `pm-platform/src/posix/` (`cfg(unix)`): `FileBenchTarget` / `BenchFile`. Linux reopens the
  scratch file with `O_DIRECT` (falls back to buffered if refused); macOS sets `F_NOCACHE` via
  `fcntl` (the only `unsafe`, one tiny commented function). The file is unlinked right after it
  is open, so it cannot outlive the process. `FileBenchTarget` is now exported on every OS with
  the same API, so `pm-analyze` (trait impl), `pm-cli` and `pm-gui` use it without `cfg`.
  `libc = "0.2"` added to workspace deps and `pm-platform` (`cfg(unix)`).
- `pm-cli`: access-denied hint rendered with the active platform's `elevation_hint()`
  (`CliError::render`; `Display` uses the host hint); `pm doctor` prints `os_name()`;
  `/dev/...` device arguments parse (as volumes) and resolve to a `VolumeId` (Task B maps them);
  `pm analyze bench` takes a folder on every OS (drive letters stay Windows-only).
- `pm-gui`: log folder per OS (`LOCALAPPDATA\PartitionManager`, `~/Library/Logs/PartitionManager`,
  `$XDG_STATE_HOME/partition-manager` or `~/.local/state/partition-manager`; pure
  `log_dir_for` with tests); status bar / banner wording via `platform.os_name()`
  ("Running as root" / "Not root: read-only" / "... with sudo"); `windows_subsystem` only on
  Windows; the benchmark window no longer stubs out non-Windows. `build.rs` already used
  `CARGO_CFG_TARGET_OS` (the TARGET), unchanged.
- Tests: `pm-analyze/tests/surface.rs` skips the slow-block test under `CI` and guards the
  `slow.is_empty()` assertion of the bad-block test (the bad-block assertions still run).
  Windows-path / drive-letter tests got `#[cfg(windows)]` (see Concerns).
- `.github/workflows/ci.yml`: matrix windows/ubuntu/macos, stable toolchain, rust-cache, apt
  deps on Ubuntu, `cargo build --workspace`, `cargo test --workspace --no-fail-fast`, `CI=true`.
- README: CI badge, Platform support table, Linux/macOS build section (apt line, the
  `RUSTUP_TOOLCHAIN=stable` override for a checkout that carries the Windows-pinned
  `rust-toolchain.toml`).

## Cross-checks (from Windows)

- `cargo check --workspace --target x86_64-unknown-linux-gnu`: pass, no warnings
- `cargo check --workspace --target aarch64-apple-darwin`: pass, no warnings
- `cargo check --workspace --target x86_64-apple-darwin`: pass, no warnings
- `cargo check --workspace --tests --target {linux, aarch64-apple}`: pass, no warnings

(The only Windows-ism that needed fixing for these to pass cleanly was a dead-code warning in
`pm-cli` from the old `cfg(not(windows))` bench stub; the rest already compiled.)

## Tests

Windows (`cargo test --workspace`): 643 passed, 0 failed, 3 ignored, zero warnings.

WSL Ubuntu 24.04 (`RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR=~/pm-target CI=true cargo test
--workspace --no-fail-fast`): 598 passed, 0 failed, 0 ignored, zero warnings, GUI tests included.
Per test binary `test result: ok` counts: pm_core 25; pm_fs 8 (+ bitmap 5, detect 6, exfat 15,
ext 9, fat 10, fat_corrupt 11, fat_undelete 2, ntfs 7); pm_gui 124; pm_image 17 (+ formats 20,
mapfile 7, rescue 20); pm_ops 4 (+ executor 31, executor_rescue 8, planner 20); pm_parttable 1
(+ convert 8, gpt 19, mbr 29, search 6); pm_platform 18 (+ file_platform 4, mock 7,
windows_live 0); pm_recover 3 (+ carve 24, files 5, partitions 9, session 5); pm_analyze
(bench 6, smart 3, space 4, surface 3); pm (cli binary) 51 (+ cli 7, cli_image 18, cli_ops 19).
`pm-analyze --test surface` also passes in WSL with `CI` unset.

No extra apt packages were needed beyond what is installed there. The apt line recorded in the
README and CI: `pkg-config libgtk-3-dev libxkbcommon-dev libwayland-dev libxcb-render0-dev
libxcb-shape0-dev libxcb-xfixes0-dev` (plus `build-essential`, already present).

## Concerns

1. **Drive-letter guards have no POSIX equivalent yet.** The "destination/image/mapfile must not
   be on the source disk" checks in `pm-cli/src/cmd_recover.rs` (`dest_letter`/`check_dest`)
   and the GUI wizards (`data_recovery`, `image_create`, `image_restore`, `rescue`) are based on
   Windows drive letters (`Path` prefix `Disk(..)`, `partition.letter()`), so on Linux/macOS they
   are silent no-ops. I gated their 7 Windows-path tests with `#[cfg(windows)]` and a comment.
   Task B/D should add a mount-point-based equivalent (path under a mount point of the source
   volumes) before enabling Linux/macOS writes in the apps, and un-gate those tests.
2. The GUI benchmark/space-analyzer drive chooser (`drive_roots`) still lists Windows letters
   only; it is Task D (mount-point list).
3. `cli_unknown_volume_exits_1` returns early when the platform layer is "not supported"
   (NullPlatform on Linux/macOS until Tasks B/C); remove that escape once they land.
4. User-facing text elsewhere still mentions Windows terms (e.g. "e.g. D:\Rescue\disk1.map" in
   the mapfile message). Task D.
5. POSIX bench target: unlinking on open means the temp file cannot be observed in the folder
   while a run is in progress (Windows keeps a visible `FILE_FLAG_DELETE_ON_CLOSE` file).
   Direct I/O behaviour on real block devices was only exercised on WSL's ext4/9p mounts.
6. The macOS code path (`F_NOCACHE`) is compile-checked only; CI's macOS job will exercise it.

## Fix round 1

Review finding: `#[cfg(windows)]` had been applied to whole tests that are mostly portable.
Now only the drive-letter assertions are guarded with `if cfg!(windows)`; the portable parts
(relative / empty / "full path" / "does not exist" / "cannot be the image" / flat-extent and
`.partial` rules / `build()` agreement / `recheck`) run on every OS, using `std::env::temp_dir()`
for OS-neutral absolute paths. Tests changed:

- `pm-gui` `image_create::damaged_page_requires_absolute_mapfile_off_source` (only the `e:\maps`
  line is guarded)
- `pm-gui` `rescue::mapfile_must_be_an_absolute_path` (absolute temp path added; `D:\` guarded)
- `pm-gui` `data_recovery::destination_must_not_be_on_the_source` (absolute temp folder added;
  letter lines guarded)
- `pm-cli` `cmd_recover::destination_on_the_source_drive_is_refused` (letter lines guarded; the
  relative-destination check runs everywhere)

The three wholly letter-dependent tests stay `#[cfg(windows)]` (`image_create::
image_file_on_the_source_is_refused`, `image_restore::image_on_the_target_disk_is_refused`,
`rescue::files_on_the_source_or_destination_disk_are_refused`). Added
`// TODO(Task B/C): remove once Linux/macOS platforms land` next to the NullPlatform early
return in `cli_unknown_volume_exits_1`.

`cargo test -p pm-gui -p pm-cli`, zero warnings on both.

Windows:
```
test result: ok. 52 passed; 0 failed; 0 ignored   (pm bin)
test result: ok. 7 passed; 0 failed; 0 ignored    (cli)
test result: ok. 18 passed; 0 failed; 0 ignored   (cli_image)
test result: ok. 19 passed; 0 failed; 0 ignored   (cli_ops)
test result: ok. 130 passed; 0 failed; 0 ignored  (pm_gui lib)
```
WSL (`RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR=~/pm-target-check CI=true`), run on a clean copy of
`HEAD` plus these five files because another agent's in-progress `pm-platform` work (missing
`posix/*` modules) did not compile in the shared tree at that moment:
```
test result: ok. 52 passed; 0 failed; 0 ignored   (pm bin)
test result: ok. 7 passed; 0 failed; 0 ignored    (cli)
test result: ok. 18 passed; 0 failed; 0 ignored   (cli_image)
test result: ok. 19 passed; 0 failed; 0 ignored   (cli_ops)
test result: ok. 127 passed; 0 failed; 0 ignored  (pm_gui lib; was 124, +3 restored)
```
One unexplained `pm-gui --lib` failure appeared once on Windows while the other agent was
building in the same target dir; 7 further runs (3 of `--lib`, 4 of both crates) were all green.
