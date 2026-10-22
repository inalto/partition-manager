# Multi-platform Task C report: macOS platform

## What changed

- `crates/pm-platform/src/macos/` (new). Everything that does not need macOS compiles on all
  OSes, so the fixtures run on Windows, Linux and the macOS CI job alike:
  - `diskutil.rs`: `DiskutilRunner` trait (`run(&[&str]) -> Result<Vec<u8>>`), pure
    `parse_list` (→ `Vec<ListedDisk>`, alias `WholeDisk`; synthesized APFS container disks
    skipped: `APFSPhysicalStores` present or the APFS container scheme as `Content`) and
    `parse_info` (→ `DeviceInfo`: identifier, size, `DeviceBlockSize`, media name, bus
    protocol, removable = `Removable || RemovableMedia || Ejectable`, writable, whole disk,
    parent whole disk, mount point, volume name, `FilesystemType`/`FilesystemName`, total /
    free, content, `DiskUUID`, internal, solid state, APFS physical stores). Helpers:
    `parse_identifier` (`/dev/rdisk3s2` → `(3, Some(2))`), `volume_identifier`, `raw_node`,
    `bus_of` (USB→Usb, SATA→Sata, PCI-Express/Apple Fabric/NVMe→Nvme, Disk Image/Virtual
    Interface→Virtual, SD→Sd, SAS/SCSI/FC→Scsi, else Unknown), `scheme_of`, `fs_of` (apfs,
    hfs, msdos with FAT12/16/32 from `FilesystemName`, exfat, ntfs, udf, cd9660; `Apple_APFS` /
    `Apple_HFS` content as fallback), `ptype_of` (slice `Content` → GPT GUID / MBR id).
  - `enumerate.rs`: `enumerate(runner, opener, system_disk) -> Enumerated { disks, volumes }`.
    `diskutil list -plist`, then `diskutil info -plist` for every physical whole disk and
    slice. `DiskInfo.index` is the diskutil number (`disk:N` = `diskN`), `path` `/dev/rdiskN`,
    model = `MediaName`, serial empty, sector sizes = `DeviceBlockSize`, flags {removable,
    read_only = !Writable, system, trim = SolidState}. When the opener can read `/dev/rdiskN`
    (root) the table comes from `pm_parttable::read_table` and slices are matched to entries
    by number + size (then size alone); otherwise slices are laid end to end from Apple's first
    GPT offset (LBA 40 / LBA 6 on 4Kn, sector 1 for MBR), `table = Unknown`, ptype / GUID /
    name from diskutil. Each slice gets fs, label, mount point, `used = TotalSize - FreeSpace`
    when mounted, and `volume = /dev/diskNsM`; readable disks also get `msdos` refined from
    the boot sector and unknown filesystems probed (`detect_filesystems`).
  - `system_disk.rs`: `system_disk(runner)` = `diskutil info -plist /` → first APFS physical
    store (`disk0s2` → 0), else `ParentWholeDisk`; `disk_of_path(runner, path)` tries the
    path and each ancestor with `diskutil info -plist` (it only resolves mount points) →
    `(N, Some(M))`, APFS volumes mapped to their physical store slice.
  - `mount.rs` / `format.rs`: `diskutil mount -mountPoint <dir> diskNsM`, `diskutil unmount
    diskNsM` (letters → Unsupported), `diskutil eraseVolume FAT32|FAT16|FAT12|ExFAT|JHFS+|APFS
    <label> diskNsM` (empty label → `UNTITLED` / `Untitled`; NTFS → `Unsupported("NTFS
    formatting needs third-party tools on macOS")`; cluster / quick ignored).
  - `ioctl.rs`: BSD `_IOR` / `_IO` encoders and `DKIOCGETBLOCKSIZE` (0x40046418),
    `DKIOCGETBLOCKCOUNT` (0x40086419), `DKIOCSYNCHRONIZECACHE` (0x20006416) as constants
    (unit-tested everywhere); the `ioctl` calls are macOS-only, each `unsafe` in a small
    function with a `// SAFETY:` comment.
  - `device.rs`: `AlignedDevice<D: BlockDevice>` (OS-neutral): every read/write goes through
    `aligned::read_unaligned` / `write_unaligned`, so the inner device only ever sees
    block-aligned offsets and lengths (Review Focus 2). macOS: `open_raw(path, mode)` =
    `FdDevice::open_with(path, ro, 0, geometry)` (length = block count × block size) wrapped in
    `AlignedDevice`; `open_disk` → `/dev/rdiskN`, `open_volume` → `/dev/rdiskNsM` (accepts
    `/dev/diskNsM`, `/dev/rdiskNsM`, `diskNsM`).
  - `session.rs` (macOS): `begin_write` = `diskutil unmountDisk diskN` (failure →
    `Error::Busy(diskutil's message)`), then `/dev/rdiskN` read-write; `commit_table` =
    `write_table` + fsync + `DKIOCSYNCHRONIZECACHE` (best effort); `finish` = sync + close (the
    kernel re-reads the map), no remount (documented); `Drop` flushes and closes.
  - `smart.rs` (macOS): `smartctl -j -a /dev/diskN` through `posix::smartctl::parse`; `None`
    when smartmontools is missing (`brew install smartmontools`).
  - `mod.rs`: `MacPlatform` (macOS; `new()` / `with_runner(Box<dyn DiskutilRunner>)`) and
    `SystemDiskutil` (`posix::cmd::run("diskutil", ..)`). `rescan` / `set_offline` → Ok no-op,
    `shrink_volume` / `extend_volume` / `run_tool` → Unsupported, `free_letters` → `[]`,
    `is_elevated` = `posix::is_root()`, `os_name` = "macos", `elevation_hint` =
    `ELEVATION_HINT_UNIX`, `disk_of_path` = `crate::canonical_existing` + the ancestor walk.
- `lib.rs`: `pub mod macos;` (all OSes), `pub use macos::MacPlatform` and
  `default_platform()` → `MacPlatform::new()` on macOS; `NullPlatform` now only for other OSes.
- `aligned.rs`: the "misaligned I/O" error code that triggers the bounce fallback is 87
  (`ERROR_INVALID_PARAMETER`) on Windows and `EINVAL` (22) elsewhere, so a macOS raw node that
  rejects an unaligned buffer address also falls back to the bounce buffer. One line.
- `plist = "1"` in `[workspace.dependencies]` and as a plain pm-platform dependency (pure Rust:
  plist 1.10, quick-xml, time, base64 in Cargo.lock).
- Fixtures `crates/pm-platform/tests/fixtures/macos/`: `list.plist` (disk0 internal 4Kn
  PCI-Express GPT: EFI + Apple_APFS; disk1 synthesized APFS container with two volumes; disk2
  USB MBR stick with a `Windows_FAT_32` slice at `/Volumes/STICK`; disk3 USB GPT disk with EFI
  + unmounted NTFS `Microsoft Basic Data`), `info-disk0`, `-disk0s1`, `-disk0s2`, `-disk2`,
  `-disk2s1`, `-disk3`, `-disk3s1`, `-disk3s2`, `info-root` (`/` = APFS `disk1s5` snapshot,
  physical store `disk0s2`). Written by hand in Apple's XML plist format with diskutil's key
  names (not captured on a Mac).
- Tests: `tests/macos_fixtures.rs` (8, all OSes): list parsing, info field mapping,
  enumeration without device access (disks 0, 2, 3; bus / size / removable / system / trim /
  approximate starts / fs / labels / mount points / used / volume ids / volumes / exact
  diskutil calls), enumeration with a readable disk3 (sparse device with a real GPT → `Gpt`,
  exact starts), failure handling, `system_disk() == Some(0)`, `disk_of_path("/Volumes/STICK")
  == Some((2, Some(1)))` (also for a path inside it and for a path on `/` → `(0, Some(2))`),
  mount/unmount/format argument vectors and refusals, and the aligned-I/O test (a strict
  raw-node fake that fails unaligned I/O with `EINVAL`: direct access fails, through
  `AlignedDevice` every request is block-aligned, neighbours preserved, a GPT round trip on a
  4Kn fake works, read-only enforced). Unit tests in `diskutil.rs` and `ioctl.rs`.
  `tests/macos_live.rs` (`cfg(target_os = "macos")`, `#[ignore]`): `hdiutil create -size 64m
  -layout NONE`, `hdiutil attach -nomount`, enumerate, GPT through a session, re-read, `diskutil`
  sees `diskNs1`, `hdiutil detach` in a drop guard; procedure documented in the file header.

## Cross-checks (from Windows)

- `cargo check -p pm-platform --tests --target aarch64-apple-darwin`: pass, no warnings
- `cargo check -p pm-platform --tests --target x86_64-apple-darwin`: pass, no warnings
- `cargo check --workspace --target aarch64-apple-darwin` and `--target x86_64-apple-darwin`:
  pass, no warnings; `cargo check --workspace --tests --target aarch64-apple-darwin`: pass.
- `cargo clippy -p pm-platform --tests --target aarch64-apple-darwin`: nothing in `macos/`.

## Tests (Windows, `cargo test -p pm-platform`)

Run with the Linux agent's uncommitted Task B work in the same tree.

```
Running unittests src\lib.rs        test result: ok. 62 passed; 0 failed; 0 ignored
Running tests\file_platform.rs      test result: ok. 4 passed; 0 failed; 0 ignored
Running tests\linux_fixtures.rs     test result: ok. 0 passed; 0 failed; 0 ignored
Running tests\linux_live.rs         test result: ok. 0 passed; 0 failed; 0 ignored
Running tests\macos_fixtures.rs     test result: ok. 8 passed; 0 failed; 0 ignored
Running tests\macos_live.rs         test result: ok. 0 passed; 0 failed; 0 ignored
Running tests\mock.rs               test result: ok. 8 passed; 0 failed; 0 ignored
Running tests\windows_live.rs       test result: ok. 0 passed; 0 failed; 3 ignored
Doc-tests pm_platform               test result: ok. 0 passed; 0 failed; 0 ignored
```

No warnings.

## Not verified without a Mac

- Everything behind `cfg(target_os = "macos")` is compile-checked only: the ioctls on a real
  `/dev/rdiskN` (request numbers checked against the `<sys/disk.h>` values only),
  `FdDevice` + `AlignedDevice` on a raw node, the write session (`unmountDisk`, the kernel
  re-reading the map on close, Disk Arbitration auto-mounting new volumes afterwards),
  `diskutil mount/unmount/eraseVolume` behaviour, `smartctl` on macOS, the live test.
- The fixtures follow diskutil's key names from documentation/memory, not a capture. Details
  to check against `diskutil list -plist` / `info -plist` on a real Mac: `BusProtocol` strings
  on Apple Silicon (`Apple Fabric`), `Ejectable` / `RemovableMedia` on USB sticks and disks,
  `FreeSpace` presence on non-APFS volumes, the `APFSPhysicalStores` entry key in `info`
  (`APFSPhysicalStore`; `DeviceIdentifier` is accepted too), whether `diskutil info` accepts a
  non-mount-point path (the ancestor walk works either way).
- The macOS CI job will run `macos_fixtures` and the unit tests; the live test needs `sudo`
  on a Mac.

## Concerns

1. Without root, partition starts are approximate and the disk reports `TableKind::Unknown`.
   Nothing in the GUI/CLI marks such a disk read-only explicitly yet; the planner sees an
   Unknown table (Task D should show "run with sudo for exact offsets" and keep it read-only).
2. The APFS physical store slice carries no mount points (nothing is attached from the
   synthesized container, per the ruling), so the boot disk's APFS partition looks unmounted.
   Protection relies on `flags.system` on disk 0 and on `diskutil unmountDisk` failing on the
   boot disk (→ Busy). A later improvement: attach the container volumes' mount points to the
   store slice.
3. Deviation: every slice gets `volume = /dev/diskNsM`, mounted or not (as on Linux), because
   mount and format need a volume id for an unmounted partition.
4. `begin_write` does not refuse the system disk itself (same as Windows); `diskutil
   eraseVolume` also changes the partition type, and `format` ignores cluster size / quick /
   progress.
5. Only synthesized APFS containers are filtered; CoreStorage logical volumes and other
   virtual disks would be listed (disk images are listed on purpose, bus `Virtual`).
6. `disk_of_path` spawns one `diskutil` per ancestor until a mount point answers; a `statfs`
   (`f_mntonname`) shortcut would make it one call.
7. Task C builds on Task B's `Platform::disk_of_path` (traits.rs) and
   `crate::canonical_existing` (lib.rs): it was committed as 068d6ae after d36fc10 (Linux), and the checks
   and tests above were re-run on top of it. Using `canonical_existing` on macOS also removes
   its dead-code warning on the macOS targets.
