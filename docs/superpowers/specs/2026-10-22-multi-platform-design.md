# Multi-platform support (Linux, macOS) — Design Spec

Date: 2026-10-22
Status: approved for implementation (user asked for plan + autonomous execution)

## 1. Intent

Make Partition Manager build, test and run on Linux and macOS in addition to Windows, with the
same engines, CLI and GUI, by implementing the `Platform` trait for each OS. Windows stays
the reference platform; Linux and macOS get a working v1 of enumeration, raw I/O, write
sessions, mount/unmount, format/resize through the OS's own tools, SMART through `smartctl`,
and elevation detection. Boot-fix for Windows installations stays Windows-only.

### What the user said

"Can we make this multi platform adding linux and macos? Plan how to and execute."

### What already exists

- Only `pm-platform` touches the OS. Every engine (tables, filesystems, images, rescue,
  recovery, analysis, operations) works on `BlockDevice` and plain data and is already
  OS-neutral.
- `default_platform()` returns `WindowsPlatform` on Windows and a `NullPlatform` stub
  elsewhere; `FilePlatform` (image files as disks) and `MockPlatform` are OS-neutral.
- The GUI is eframe/egui, which runs natively on all three OSes. The CLI is clap.
- A few Windows-only bits leak outside the platform crate: the GUI manifest `build.rs`, the
  console attach, the benchmark file target, `windows_subsystem`, and OS-specific log and
  temp paths.

### Assumptions made (flag if wrong)

- Linux access goes through sysfs/procfs and raw `/dev` nodes with `libc` ioctls; no udev
  library dependency (serial numbers come from sysfs where exposed, otherwise empty).
- macOS access goes through `diskutil … -plist` (parsed with the `plist` crate) and raw
  `/dev/rdiskN` character devices; no IOKit FFI in v1.
- Filesystem *creation* and *resizing* on Linux/macOS shell out to the OS tools
  (`mkfs.*`, `resize2fs`, `ntfsresize`, `diskutil eraseVolume`/`newfs_*`); absent tools give a
  clear `Unsupported` error naming the tool.
- Boot repair (`bcdboot`/`bootsect`) has no Linux/macOS equivalent in v1: `run_tool` returns
  `Unsupported`, Migrate OS warns that the clone will not be made bootable from a non-Windows host.
- Writing to a disk requires root (`sudo`); the apps detect `geteuid() == 0` and otherwise
  run read-only, exactly like the unelevated Windows mode.
- Verification: Linux is built and tested in WSL2 (Ubuntu 24.04) including live tests on loop
  devices; macOS is cross-checked with `cargo check --target aarch64-apple-darwin` locally and
  built + unit-tested on a GitHub Actions `macos-latest` runner. No macOS live-disk test is
  possible from this machine.

## 2. Architecture

```
pm-platform/src/
  lib.rs            default_platform(): cfg dispatch Windows / Linux / macOS / Null
  traits.rs         Platform, WriteSession, MountPoint (unchanged API)
  aligned.rs        sector-aligned bounce buffer (shared; macOS rdisk needs it too)
  posix/            shared unix helpers: FdDevice (pread/pwrite on a RawFd), euid, run_cmd,
                    smartctl JSON parsing, statvfs usage, tool lookup on PATH
  linux/            sysfs enumeration, /proc/mounts, ioctls (BLKGETSIZE64, BLKSSZGET,
                    BLKPBSZGET, BLKRRPART, BLKROGET), umount2/mount, mkfs/resize tools,
                    write session (unmount all partitions → flock → write → BLKRRPART)
  macos/            diskutil list/info -plist, /dev/rdiskN, write session (diskutil
                    unmountDisk → write → close triggers re-read), mount/unmount/erase via
                    diskutil, newfs_* tools
```

Platform trait additions (small, keep Windows semantics):

- `fn os_name(&self) -> &'static str` ("windows" / "linux" / "macos" / "none").
- `fn elevation_hint(&self) -> &'static str` ("run from an elevated prompt" / "run with sudo").
- `MountPoint::Letter` is accepted only on Windows; Linux/macOS use `MountPoint::Folder`
  (`free_letters()` returns an empty list, the GUI hides the letter chooser).

Per-OS mapping of `Platform` methods:

| Method | Linux | macOS |
|---|---|---|
| enumerate_disks | `/sys/block/{sd*,nvme*n*,vd*,mmcblk*,loop*}`: size, sector sizes, removable, ro, model/vendor/serial from `device/`, bus from the sysfs device path; partitions from `/sys/block/X/X1/{start,size}` + `pm_parttable::read_table` when readable; fs/label via `pm_fs::detect`; mount points + fs from `/proc/mounts`; used/free via `statvfs` | `diskutil list -plist` for whole disks and slices; `diskutil info -plist <dev>` for size, media name, protocol (bus), removable, writable, mount point, fs, label; APFS containers are shown as the physical store's partitions, synthesized disks are skipped |
| open_disk / open_volume | `/dev/sdX` or `/dev/sdX1` via `FdDevice` (O_RDONLY / O_RDWR, `O_CLOEXEC`), length from `BLKGETSIZE64`, sector from `BLKSSZGET` | `/dev/rdiskN` / `/dev/rdiskNsM` via `FdDevice` behind `AlignedDevice` (raw devices require sector-aligned I/O), length from `DKIOCGETBLOCKCOUNT*DKIOCGETBLOCKSIZE` |
| begin_write | `umount2` every mounted partition of the disk (Busy if any fails) → open RW → `flock(LOCK_EX|LOCK_NB)` (Busy if held) → session; `commit_table` = `write_table` + `BLKRRPART` (retry `partprobe` if EBUSY); `finish` = fsync, unlock, close, `BLKRRPART` | `diskutil unmountDisk <diskN>` (Busy on failure) → open `/dev/rdiskN` RW → session; `commit_table` = `write_table`; `finish` = close (the kernel re-reads the table), then `diskutil mountDisk` is NOT run (user decides) |
| mount / unmount | `mount(2)` with the type from `pm_fs::detect` (fallback: `mount` command), `umount2` | `diskutil mount -mountPoint <dir> <dev>` / `diskutil unmount <dev>` |
| format | `mkfs.vfat`, `mkfs.exfat`, `mkfs.ntfs -Q`, `mkfs.ext4`, `mkfs.xfs` by `FsKind` | `newfs_msdos`, `newfs_exfat`, `newfs_hfs`, `newfs_apfs`? (v1: FAT32/exFAT/HFS+ via `diskutil eraseVolume`) |
| shrink/extend | NTFS: `ntfsresize -s <bytes> <dev>` (unmounted); ext: `e2fsck -f` + `resize2fs <dev> <size>`; others Unsupported | Unsupported in v1 (APFS/HFS resizing needs `diskutil resizeVolume`, which also moves the partition — out of scope) |
| smart | `smartctl -j -a /dev/X` → NVMe/ATA fields; `None` when smartctl is absent | same |
| is_elevated | `geteuid() == 0` | same |
| system_disk | device of `/` from `/proc/mounts` (resolve LVM/dm → Unknown) → parent block device | `diskutil info -plist /` → parent whole disk (APFS: the physical store's disk) |
| run_tool | Unsupported (no bcdboot/bootsect) | Unsupported |
| free_letters | `[]` | `[]` |
| bench target | temp file opened `O_DIRECT` (fallback without) | temp file with `F_NOCACHE` |

Elsewhere:

- `pm-gui`: `build.rs` embeds the manifest only on Windows; the log file goes to
  `$XDG_STATE_HOME/partition-manager/` (Linux) or `~/Library/Logs/PartitionManager/` (macOS);
  the status bar says "Running as root" / "Not root: read-only"; letter-based UI hidden when
  `free_letters()` is empty; "Open in file manager" uses `xdg-open` / `open`.
- `pm-cli`: the access-denied hint uses `elevation_hint()`; device syntax accepts `/dev/...`
  paths and `disk:N`; `pm doctor` prints the OS, root status and the external tools found.
- `pm-analyze`: the benchmark target is chosen per OS in `pm-platform`.
- CI: `.github/workflows/ci.yml` runs `cargo test --workspace` on `windows-latest`,
  `ubuntu-latest` and `macos-latest`; two timing-sensitive surface tests are skipped under
  `CI=true`.

## 3. Testing

- Unit tests per OS module with recorded fixtures: sysfs trees and `/proc/mounts` samples
  (Linux), `diskutil … -plist` captures (macOS), `smartctl -j` JSON samples; parsers are pure
  functions over those.
- WSL2 live tests (`#[ignore]`, run with `sudo -E cargo test -- --ignored`): a loop device over
  a temp file (`losetup -P`) — enumerate, read the table, write a GPT through a session,
  `BLKRRPART` shows the partitions, mount/unmount a FAT32 partition in a temp dir, format via
  `mkfs.vfat` if present.
- macOS: `cargo check --target aarch64-apple-darwin` locally; `cargo test` on the GitHub
  macOS runner (unit tests + plist fixtures). Live macOS tests are `#[ignore]`d with a
  documented manual procedure (a `hdiutil attach -nomount` disk image).
- The Windows suite must stay green and unchanged in behaviour.

## 4. Out of scope (v1)

Boot repair on Linux/macOS, LVM/dm/mdraid and APFS-container editing, resizing on macOS,
udev-based hot-plug notifications, packaging (deb/rpm/dmg), code signing/notarization,
Wayland-vs-X11 tuning beyond what eframe does by default.
