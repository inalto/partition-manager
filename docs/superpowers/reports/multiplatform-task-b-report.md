# Multi-platform Task B report: Linux platform and shared POSIX helpers

## Commits

- `3545f7f feat(platform): shared POSIX helpers` (committed early for Task C)
- `d36fc10 feat(platform): Linux implementation`
- `f711ad3 fix(platform): Linux write sessions cooperate with udev` (a race the live test
  found after the first Linux commit, see "udev" below)

Task C was editing `lib.rs`, `Cargo.toml` and `aligned.rs` in the same working tree while I worked. I
committed only my own hunks: my version of `lib.rs` was staged as a separate blob
(`git hash-object` + `update-index`). Every commit was verified in a separate `git worktree`
holding exactly the committed content (WSL, Windows and the macOS check). The last verification
ran on `951c3c8` (Task C's macOS commit included) plus the udev fix.

## What changed

### `pm-platform/src/posix/` (shared with macOS)
- `fd_device.rs`: `FdDevice` owns an `OwnedFd`. `open(path, read_only)` takes its length from
  `fstat` and uses 512-byte sectors. `open_with(path, read_only, extra_flags, geometry)` gets
  `(len, sector)` from a closure the caller supplies. It uses `O_CLOEXEC`, `pread64`/`pwrite64`
  loops (plain `pread`/`pwrite` on macOS) that retry EINTR, and turns a 0-byte read into
  `UnexpectedEof`, reported as `Error::io(offset, len)`. Accesses past the end are refused,
  `flush` = `fsync`, and `try_lock_exclusive`/`unlock` wrap `flock`. Each `unsafe` call sits
  in a small safe fn with a `// SAFETY:` comment.
- `cmd.rs`: `find_tool` looks on `PATH`, then `/sbin`, `/usr/sbin`, `/bin`, `/usr/bin` and
  `/usr/local/{sbin,bin}`. `output` returns the output whatever the status. `run` turns a
  non-zero exit into `Error::Platform{code: exit, msg: "<tool> failed: <stderr>"}`. A missing
  tool gives `Error::Unsupported("<tool> is not installed")`. No shell is involved: stdin is
  `/dev/null` and `LC_ALL=C` is set.
- `smartctl.rs`: `parse(json) -> Option<SmartData>` for `smartctl -j -a`. It reads the NVMe
  health log (with the same attribute ids/names as the Windows parser), the ATA
  `ata_smart_attributes.table[]` and `smart_status.passed`. Temperature and power-on hours
  come from the top-level summary when present, otherwise from attributes 194/190 and 9.
  Fixtures: `tests/fixtures/smartctl-{nvme,ata}.json`.
- `statvfs.rs`: `usage(path) -> (total, free)` (`f_blocks`/`f_bfree` × `f_frsize`).
- `mod.rs`: `euid()`, `is_root()`, and `os_error(e, ctx)` (errno becomes the code; EACCES/EPERM
  say "access denied" so the CLI hint matches).
- `serde_json` added to pm-platform under `cfg(unix)` (libc was already there from Task A).

### `pm-platform/src/linux/`
- `sysfs.rs`: pure `parse_block_dir(root)`.
  - Read per disk: size, ro, removable, logical/physical block size, `discard_max_bytes`
    (sets `trim`), model (vendor + model with the "ATA" vendor dropped, the MMC `name`, or
    "Loop device (<backing file>)"), and serial (`device/serial`).
  - Partitions: start and size are always in 512-byte units, and the partition node becomes
    the volume (the 1 KiB MBR-extended stub gets no volume).
  - Skipped: `ram`, `zram`, `dm-`, `md`, `sr`, `fd`, `nbd`, `zd`, eMMC boot/RPMB areas,
    `nvmeXcYnZ` multipath paths, `hidden`, size-0 devices, and loops without `backing_file`.
  - Bus comes from the canonical sysfs path (nvme/usb/ata/mmc/virtio/vmbus/virtual).
  - Disks are sorted in natural order with loop devices last, and `index` is the position.
- `mounts.rs`: pure `parse_proc_mounts` (octal unescape, so `\040` becomes a space),
  `parse_proc_swaps`, `longest_mount`, the fs ↔ mount-type tables, and `read_mounts()`
  (`/proc/self/mounts` with device symlinks canonicalized).
- `ioctl.rs`: `BLKGETSIZE64`, `BLKSSZGET`, `BLKPBSZGET`, `BLKROGET`, `BLKRRPART` (`_IOC`
  encoding checked against libc's constants), plus `umount2`, `mount`, `major_minor`, and a
  `geometry` closure that also drops the stale page cache (`posix_fadvise(DONTNEED)`).
- `session.rs`:
  - `begin_write` first refuses `Busy` for holders (LVM/RAID/crypt) or an active swap on the
    disk.
  - It then unmounts the disk's filesystems, deepest first (`unmount_all`). On the first
    failure it re-mounts everything already unmounted (`remount_all`, in reverse order) and
    returns `Busy` naming the busy mount and what was rolled back.
  - Next comes `udevadm settle`, then `open(O_RDWR|O_EXCL)` and `flock(LOCK_EX|LOCK_NB)`, both
    retried for about 2 s. If they fail, it re-mounts and returns `Busy`. `O_EXCL` goes beyond
    the spec: the kernel refuses while anything still mounts or claims the disk, for example a
    mount in another namespace that `/proc/self/mounts` does not show.
  - `commit_table` = `write_table` + `fsync` + `BLKRRPART`. On EBUSY it retries 3 × 300 ms,
    then tries `partprobe`. EINVAL on a loop device without `-P` is only logged.
  - `finish`/`Drop`: fsync, hand the lock to a read-only descriptor, close the RW one, re-read
    the table, release the lock, then `udevadm settle`.
  - Filesystems are not re-mounted after a successful session (documented).
  - A `Mounter` trait (`SystemMounter`: `umount2`/`mount(2)` with `mount` command fallback)
    makes the rollback unit-testable.
- `mount.rs`: folder mount points only (letters give `Unsupported`). The type comes from
  `pm_fs::detect`. It tries `mount(2)` with each type (`ntfs3` → `ntfs-3g` → `ntfs`, `vfat`,
  `exfat`, ext*, xfs, btrfs, hfsplus, iso9660, udf), then the `mount -t` command, then
  `mount` without a type. Unmount checks `/proc/mounts` (`NotFound` otherwise), uses
  `umount2`, and falls back to `umount`. `ready_node` waits up to 3 s for a partition node.
- `format.rs`: pure `mkfs_command`:
  - `mkfs.vfat -F 12|16|32 -n L [-s spc]`
  - `mkfs.exfat -L L [-c bytes]` (exfatprogs uses `-L`, not `-n`)
  - `mkfs.ntfs [-Q] -L L [-c] -s <sector> -p <start sector>` (the hidden-sectors field)
  - `mkfs.ext2|3|4 -F -L L [-b]`, `mkfs.xfs -f -L L [-b size=]`, `mkfs.btrfs -f -L L`
  - Unusable cluster sizes are ignored with a log line. Mounted volumes are refused with
    `Busy`.
- `resize.rs`: NTFS uses `ntfsresize --force --no-progress-bar -s <bytes>`. ext uses
  `e2fsck -f -y` (exit 0/1 accepted), then `resize2fs <dev> <N>s`. Other filesystems are
  `Unsupported`, and mounted volumes are refused with `Busy`.
- `smart.rs`: `smartctl -j -a /dev/<disk>`. Returns `Ok(None)` when smartctl is missing or no
  data comes back.
- `system_disk.rs`: `system_disk_from(mounts, disks)` takes the last `/` entry. A plain disk or
  partition node gives its disk index. `/dev/mapper/*`, `/dev/dm-*`, `/dev/md*`, `overlay`,
  `rootfs`, ZFS, `/dev/root` and anything unknown give `None`.
- `mod.rs`: `LinuxPlatform`.
  - Enumeration: sysfs first, then, if the disk node is readable, `read_table`. Partitions then
    come from the table (table-slot numbers, types, GUIDs, names, flags), with the volume
    matched to the kernel's node by start. `pm_fs::detect` fills fs and label.
  - `/proc/mounts` supplies mount points and the fs when still unknown. `statvfs` supplies
    `used`, and `flags.boot` is set for `/`.
  - A filesystem on the whole disk (WSL's disks, "superfloppy" sticks) becomes one
    pseudo-partition at offset 0 whose volume is the disk node. Otherwise such a disk looks
    "Uninitialized" and all unallocated, and the planner refuses to initialize a disk that has
    partitions.
  - `enumerate_volumes` = every partition node. `open_volume` accepts block-device `/dev`
    nodes only.
  - Simple methods: `rescan` = `BLKRRPART` (EBUSY/EINVAL logged); `set_offline` is a logged
    no-op; `is_elevated` = euid 0; `free_letters` = `[]`; `run_tool` gives `Unsupported`.
    `os_name` is `"linux"` and the hint is "run with sudo (root)". `system_disk` is recomputed
    on each call, never cached, because indexes are positions.
- `lib.rs`: `default_platform()` returns `LinuxPlatform` on Linux. Also added
  `canonical_existing(path)` (resolves the deepest existing ancestor and keeps the missing
  tail), which macOS now uses too.

### `Platform::disk_of_path` (additive, default `None`)
- Linux: `st_dev` → `/sys/dev/block/M:m` → node. Fallback: the longest `/proc/mounts`
  mount-point prefix gives the device. The node is then mapped to `(disk index, partition
  number)`.
- Windows: the longest volume mount-point prefix (pure `volume_of_path`, unit-tested) gives the
  volume. Its extent gives the disk, and the new `disk::disk_info(index)` gives the partition
  by start. A live unit test confirms `%SystemRoot%` is on `system_disk()`.
- Mock: `set_disk_of_path(path, disk, part)`; the deepest registered ancestor wins.
- CLI/GUI callers were not changed (Task D).

## Tests

### WSL (Ubuntu 24.04, `RUSTUP_TOOLCHAIN=stable CI=true`), on the verification worktree at HEAD content
`cargo test --workspace --no-fail-fast`: **657 passed, 0 failed, 1 ignored**, zero warnings.

`cargo test -p pm-platform`:
```
unittests src/lib.rs      test result: ok. 57 passed; 0 failed; 0 ignored
tests/file_platform.rs    test result: ok. 4 passed; 0 failed; 0 ignored
tests/linux_fixtures.rs   test result: ok. 7 passed; 0 failed; 0 ignored
tests/linux_live.rs       test result: ok. 0 passed; 0 failed; 1 ignored
tests/macos_fixtures.rs   test result: ok. 8 passed; 0 failed; 0 ignored
tests/mock.rs             test result: ok. 8 passed; 0 failed; 0 ignored
```
Fixture tests (`linux_fixtures.rs`):
- sda: SATA, 512-byte sectors, 2 partitions.
- nvme0n1, 4Kn: p1 `start` 256 → byte 131072 (Review Focus 3).
- mmcblk0: removable SD.
- Skipped as intended: loop0 without a backing file, dm-0, ram0, mmcblk0boot0, sr0.
- loop1 is listed last.
- `/proc/mounts` with `\040`, `\011` and `\134` escapes.
- `system_disk`: `/dev/nvme0n1p2` gives the nvme index, a whole-disk `/dev/sda` works, and the
  last `/` entry wins. LVM, dm, md, overlay, rootfs, ZFS and `/dev/root` all give `None`
  (Review Focus 4).

Rollback unit tests (`linux::session`):
- Second partition busy: the first is re-mounted and the call returns `Busy` (Review Focus 1).
- First partition busy: nothing is unmounted.
- A re-mount that fails is reported.
- A real `begin_write` on a regular-file "disk" whose flock is held re-mounts what it unmounted.
- Swap or holders refuse before anything is unmounted.

Live (`sudo -E env "PATH=$PATH" ... cargo test -p pm-platform --test linux_live -- --ignored --test-threads=1`):
```
test loop_device_end_to_end ... attached /tmp/.tmpQjnILW/pm-live.img as /dev/loop0
begin_write refused as expected: /dev/loop0p2 on /tmp/.tmpFs2CrJ cannot be unmounted (Device or resource busy (os error 16)); the 1 filesystem(s) already unmounted were mounted again
detached /dev/loop0
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.33s
```
What the live test covers:
1. A 64 MiB file is attached with `losetup -fP --show`; enumeration finds it (Virtual, 64 MiB,
   table None).
2. `open_disk` works.
3. `begin_write` + `commit_table` write a GPT with two partitions, then `finish`.
4. `/dev/loop0p{1,2}` appear in sysfs, and enumeration shows the GPT, numbers, names and
   volumes.
5. `format()` creates FAT32 and FAT16 with `mkfs.vfat`.
6. `mount()` into temp dirs; a file is written.
7. `disk_of_path` returns `(loop index, Some(1))`, including for a file that does not exist
   yet.
8. Enumeration shows fs, label, mount point and used; `enumerate_volumes` agrees.
9. Format of a mounted volume returns `Busy`.
10. **Live Review Focus 1:** a file is held open on p2, and `begin_write` returns `Busy` with p1
    re-mounted and both still mounted.
11. `unmount()` ×2, then `unmount` again returns `NotFound`.
12. `rescan` works, and `smart` returns `None` for a loop device.
13. An idle `begin_write` reads the FAT32 boot sector; `losetup -d` runs in a drop guard.

After the udev fix, all 13 root runs passed: 10 × `cargo test -p pm-platform --
--include-ignored` and 3 × live only. Before the fix 1 run in about 10 failed (see udev below).

CLI smoke (WSL, root):
- `pm list` shows WSL's 4 virtual disks with their whole-disk ext4/swap as one partition each,
  the 1 TB root disk flagged Boot, and a loop device with GPT, FAT16 "SMOKE" and ext4
  "EXTSMOKE" (mounted, used 24 KB).
- `pm table /dev/loop0` prints the GPT.
- `pm doctor` shows `Platform: linux`, `Elevated: yes`, 5 disks, `System disk: 3` (sdd = `/`).
- `pm --disk-file img list` is unchanged.
- Unprivileged `pm list` shows sysfs-only disks marked "[partition table not readable]".

### Windows (`cargo test -p pm-platform`, verification worktree at HEAD content)
```
unittests src\lib.rs     test result: ok. 62 passed; 0 failed; 0 ignored
tests\file_platform.rs   test result: ok. 4 passed; 0 failed; 0 ignored
tests\macos_fixtures.rs  test result: ok. 8 passed; 0 failed; 0 ignored
tests\mock.rs            test result: ok. 8 passed; 0 failed; 0 ignored
tests\windows_live.rs    test result: ok. 0 passed; 0 failed; 3 ignored
```
(`linux_fixtures`, `linux_live` and `macos_live` compile to 0 tests on Windows.) Zero
warnings.

### macOS cross-check (from Windows)
`cargo check --workspace --tests --target aarch64-apple-darwin`: Finished, no warnings.
`cargo check --workspace --target x86_64-apple-darwin`: Finished, no warnings.
`cargo check --workspace --tests --target x86_64-unknown-linux-gnu`: Finished, no warnings
(run on `d36fc10`).

## udev
The first live runs failed about 1 in 10 times: a partition node was briefly missing right
after `finish`. Closing a writable descriptor on a disk makes `systemd-udevd` (running in this
WSL) issue its own `BLKRRPART`, which removes and re-adds every partition node.

Fix (`f711ad3`):
- Before the RW descriptor is closed, the flock moves to a read-only descriptor. udev's
  re-read takes `LOCK_EX|LOCK_NB`, so it skips and only sends change events.
- The table is re-read once, by us, and `udevadm settle` runs after the lock is released.
  Settling while still holding the lock waited 10 s: udev's workers wait for that lock.
- `begin_write` settles after unmounting and retries `O_EXCL`/`flock` for about 2 s, because
  udev briefly holds the lock while probing.
- `format`, `mount` and `resize` wait up to 3 s for the node.

## Concerns
1. **Disk indexes are positions.** Loop devices are sorted last, so attaching an image does not
   renumber real disks. A hot-plugged USB disk can still shift `sdX` order between
   `enumerate_disks` and `begin_write(index)`. The executor should re-check the disk's
   identity (path, size, serial) before writing; that belongs in a follow-up or Task D.
2. **Without root, the view comes from sysfs only.** The table shows as `Unknown`, with no
   partition types and no fs for unmounted partitions. The udev database
   (`/run/udev/data/b*`) could fill this in later.
3. Whole-disk filesystems appear as pseudo-partition 1 at offset 0 with `TableKind::None`. The
   CLI renders them as `*:`, and their wording is Task D. `WipeDisk` stays possible as an
   explicit action.
4. `CliError::is_access_denied` treats `code == 5` as access denied. On Linux, 5 is EIO.
   `os_error` adds "access denied" to the message for EACCES/EPERM so the hint still shows, but
   an EIO would also get the hint. Task D should make the check OS-aware.
5. `pm table /dev/loop0` works, but `device_arg::volume_extent` does not map a whole-disk
   `/dev` node to its disk index, so the source/destination overlap checks cannot recognise
   `/dev/sdb` as `disk:N` (Task D). `disk_of_path` is now available for the Task A concern
   about mount-point-based destination guards.
6. The rollback re-mount uses the original fstype and `ro` only, not the other original
   options (`uid=`, `fmask=`, ...). FUSE (`fuseblk`/ntfs-3g) is re-mounted through the `mount`
   command without options.
7. Not exercised live:
   - `mkfs.ntfs`/`exfat`/`ext4`/`xfs`, `ntfsresize`, `e2fsck`/`resize2fs`: only command-line
     unit tests. The tools exist in WSL; only `mkfs.vfat` ran through the platform.
   - `ntfs3` mounting: this WSL kernel lists only `vfat` among the filesystems the live test
     would use.
   - `smartctl` on a real drive: WSL disks are virtual.
8. Live-test deviation: the brief asked for one 32 MiB FAT32 partition. A 32 MiB FAT32 has
   fewer than 65525 clusters, which `mkfs.vfat` warns about and `pm-fs` correctly rejects (the
   fs then comes only from `/proc/mounts`, with no label). The test therefore uses p1 = 48 MiB
   FAT32 with 512-byte clusters (which also exercises `-s`) plus p2 = 8 MiB FAT16 for the
   rollback check.
9. The `_IOC` direction bits for powerpc/mips/sparc are encoded by hand. They are only
   verified on x86_64, where a test compares them with libc's `BLKSSZGET`/`BLKPBSZGET`.
10. After a successful session, filesystems the session unmounted stay unmounted (by design,
    documented in `session.rs`).
