# Multi-platform (Linux, macOS) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Partition Manager builds, tests and runs on Linux and macOS with `Platform` implementations for both, a CI matrix on all three OSes, and docs/wiki updated.

**Architecture:** see the spec. Shared POSIX helpers in `pm-platform/src/posix/`, OS modules in `linux/` and `macos/`, `default_platform()` dispatching by `cfg(target_os)`. Parsers over sysfs/procfs text and `diskutil` plists are pure and fixture-tested; live tests are `#[ignore]`d.

**Tech Stack:** `libc` 0.2 (ioctl, umount2, mount, flock, statvfs, geteuid, fcntl), `plist` 1 (macOS), `serde_json` (smartctl JSON), existing workspace crates.

**Spec:** `docs/superpowers/specs/2026-10-22-multi-platform-design.md`

## Global Constraints

- Only `pm-platform` may use `unsafe`/OS APIs; every `unsafe` block wrapped in a small safe function with a `// SAFETY:` comment; `errno` read immediately after the failing call.
- `Platform` trait changes are additive: `os_name()`, `elevation_hint()`; Windows behaviour unchanged; the Windows test suite stays green (verify on this machine).
- Linux builds/tests run in WSL2 (`wsl -d NVIDIA-SDKM-Ubuntu-24.04`, rustup in `~/.cargo`); run `cargo test -p <crate>` there, and `sudo -E $HOME/.cargo/bin/cargo test -p pm-platform -- --ignored` for loop-device tests. macOS: `cargo check --target aarch64-apple-darwin` (and x86_64) from Windows must pass for the whole workspace.
- No new OS-specific code in pm-cli / pm-gui / pm-analyze beyond `cfg` on display strings and paths; everything else goes through `Platform`.
- Commit per task; messages end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.

## Review Focus

1. Linux `begin_write` must never leave a filesystem unmounted when the session fails before writing: mounts are only removed when every unmount succeeds, otherwise everything already unmounted is re-mounted (Task B test with two mounted loop partitions where the second is busy).
2. macOS raw device I/O is sector-aligned: an unaligned `read_at`/`write_at` must go through `AlignedDevice`, never straight to `pread` (Task C test with the strict fake from `aligned.rs`).
3. sysfs partition start/size are in 512-byte units regardless of the logical sector size (Task B parser test with a 4Kn fixture).
4. `system_disk()` on Linux with `/` on LVM/dm must return `None` (so the planner fails closed), not a wrong index (Task B).
5. CI must not run `#[ignore]`d live tests and must skip the two wall-clock surface tests under `CI=true` (Task A).

---

### Task A: portability audit, cross-check targets, CI matrix

**Files:** `crates/pm-platform/src/{lib.rs,traits.rs,mock.rs,file_platform.rs}` (add `os_name`/`elevation_hint` with defaults for Null/Mock/File), `crates/pm-gui/build.rs` (manifest only on `cfg!(target_os = "windows")` of the TARGET, via `CARGO_CFG_TARGET_OS`), `crates/pm-gui/src/{main.rs,logging.rs,app.rs,views/legend.rs}` (log dir per OS, "root" wording via `elevation_hint`, no `windows_subsystem` on other OSes), `crates/pm-cli/src/{main.rs,cli.rs,device_arg.rs,cmd_doctor.rs}` (`/dev/...` device syntax, hint wording, doctor prints `os_name` + tools), `crates/pm-analyze/src/bench.rs` + `crates/pm-platform/src/posix/bench.rs` (POSIX file bench target; `FileBenchTarget` selected per OS inside pm-platform), `crates/pm-analyze/tests/surface.rs` (skip the two timing tests when `CI` is set), `.github/workflows/ci.yml` (matrix windows/ubuntu/macos; `cargo build --workspace`, `cargo test --workspace --no-fail-fast`; ubuntu installs the GTK/X11 dev packages eframe needs; `CI=true`), `README.md` (platform support table).
- [ ] `cargo check --workspace --target x86_64-unknown-linux-gnu` and `--target aarch64-apple-darwin` pass from Windows (NullPlatform on those targets at this stage).
- [ ] In WSL: `cargo test --workspace` passes (GUI tests included; if eframe needs system libs, document the apt line).
- [ ] Windows suite still green. Commit `feat(platform): portability audit, POSIX bench target, CI matrix`.

### Task B: Linux platform

**Files:** `crates/pm-platform/src/posix/{mod.rs,fd_device.rs,cmd.rs,smartctl.rs,statvfs.rs}`, `crates/pm-platform/src/linux/{mod.rs,sysfs.rs,mounts.rs,ioctl.rs,session.rs,mount.rs,format.rs,resize.rs,smart.rs,system_disk.rs}`, `crates/pm-platform/Cargo.toml` (`libc`), tests `crates/pm-platform/tests/linux_fixtures.rs` (sysfs tree + `/proc/mounts` samples incl. 4Kn, nvme, mmcblk, a dm root), `crates/pm-platform/tests/linux_live.rs` (`#[ignore]`, loop device).
**Interfaces:** `pub struct LinuxPlatform; impl Platform`; pure `linux::sysfs::parse_block_dir(root: &Path) -> Vec<DiskInfo>` and `linux::mounts::parse_proc_mounts(text) -> Vec<MountEntry>` for fixtures; `posix::FdDevice::open(path, read_only) -> Result<FdDevice>` (`BlockDevice` via `pread64`/`pwrite64`, `fsync` on flush).
- [ ] Fixture tests first; implement; live loop-device test in WSL under sudo: create a 64 MiB file, `losetup -fP --show`, enumerate shows it, `begin_write` + `commit_table` of a GPT with a FAT32 partition, `BLKRRPART` → partition node appears, `mkfs.vfat` through `format()`, `mount()` into a temp dir, `unmount()`, `losetup -d`.
- [ ] CLI smoke in WSL: `pm list`, `pm table /dev/loop0`, `pm --disk-file …` unchanged. Commit `feat(platform): Linux implementation`.

### Task C: macOS platform

**Files:** `crates/pm-platform/src/macos/{mod.rs,diskutil.rs,device.rs,session.rs,mount.rs,format.rs,smart.rs,system_disk.rs}`, `crates/pm-platform/Cargo.toml` (`plist`), tests `crates/pm-platform/tests/macos_fixtures.rs` (captured `diskutil list -plist` / `info -plist` samples for an internal APFS disk, an external USB FAT32 stick, a GPT disk with several slices), `crates/pm-platform/tests/macos_live.rs` (`#[ignore]`; documented manual procedure with `hdiutil attach -nomount`).
**Interfaces:** `pub struct MacPlatform; impl Platform`; pure `macos::diskutil::parse_list(plist_bytes) -> Vec<WholeDisk>` and `parse_info(plist_bytes) -> DeviceInfo`; raw devices through `posix::FdDevice` wrapped in `aligned::AlignedDevice` with the block size from `DKIOCGETBLOCKSIZE`.
- [ ] Fixture tests first (plists written by hand following Apple's `diskutil` keys: `AllDisksAndPartitions`, `DeviceIdentifier`, `Size`, `Content`, `MountPoint`, `VolumeName`, `WholeDisk`, `IORegistryEntryName`, `BusProtocol`, `Removable`, `Writable`, `FilesystemType`, `DeviceBlockSize`, `TotalSize`, `FreeSpace`); implement; `cargo check --target aarch64-apple-darwin` and `x86_64-apple-darwin` pass; the CI macOS job runs the unit tests. Commit `feat(platform): macOS implementation`.

### Task D: apps, docs, wiki

**Files:** `crates/pm-gui` (hide letter choosers when `free_letters()` is empty; Migrate OS boot-fix option disabled with a note on non-Windows; status bar wording), `crates/pm-cli` (doctor tools list per OS; `pm mount --folder` only on non-Windows), `README.md` (Platform support section, Linux/macOS build + run with `sudo`, first-live-test notes for loop devices / `hdiutil`), `docs/FIRST-LIVE-TEST.md` (Linux/macOS variants), `docs/wiki/{Getting-Started,Safety-Model,Known-Limitations,Roadmap,Architecture}.md` and the wiki repo push, `scripts/build-release.sh` (Linux/macOS release build into `dist/`).
- [ ] Commit `docs: Linux and macOS support`, push `main` and the wiki.
