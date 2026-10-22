# Architecture

Partition Manager is a Cargo workspace of ten crates. Dependencies flow inward: the two
front ends (`pm-cli`, `pm-gui`) sit on top of the operation layer (`pm-ops`) and the
engines, and everything builds on `pm-core`.

## Crates

| Crate | Purpose |
|---|---|
| `pm-core` | `BlockDevice` trait, file/memory/slice devices, errors, progress and cancellation, size units, disk and partition domain model |
| `pm-parttable` | MBR and GPT read/plan/write, alignment, MBR/GPT conversion, lost-table search |
| `pm-platform` | `Platform` trait; Windows (`windows-sys`), Linux (sysfs, ioctls, `mkfs.*`) and macOS (`diskutil`, raw `rdisk` nodes) implementations with shared POSIX helpers; `FilePlatform` (`--disk-file`), `MockPlatform` (tests), `NullPlatform` (other OSes) |
| `pm-fs` | File-system detection; read-only NTFS, FAT, exFAT and ext parsers; used-block bitmaps; FAT32 formatter |
| `pm-image` | Raw, VHD, VMDK, qcow2 images (and read-only VHDX) as block devices; format conversion; ddrescue-style rescue engine and mapfile |
| `pm-recover` | Lost-partition finder, deleted-file recovery, signature carver, scan sessions |
| `pm-analyze` | Space analyzer, surface test, benchmark, SMART |
| `pm-ops` | Operation model, pending queue, planner/simulator, executor, clone/migrate sizing |
| `pm-cli` | The `pm` command line |
| `pm-gui` | The `partition-manager` desktop application (eframe/egui) |

## Dependency direction

Higher boxes depend on lower ones:

```text
 front ends   pm-cli (pm.exe)                    pm-gui (partition-manager.exe)
                 |                                   |
                 +----------------+------------------+
                                  v
 operations                    pm-ops   (model, queue, planner, executor)
                                  |
 engines      pm-analyze   pm-recover   pm-image   (also used directly by the front ends)
                 |            |            |
 OS + formats    +------> pm-platform   pm-parttable   pm-fs
                                  \           |          /
 foundation                        +-------> pm-core <---+
```

The exact direct dependencies:

| Crate | Depends on |
|---|---|
| `pm-core` | (nothing in the workspace) |
| `pm-parttable` | `pm-core` |
| `pm-fs` | `pm-core` |
| `pm-image` | `pm-core`, `pm-fs` |
| `pm-platform` | `pm-core`, `pm-fs`, `pm-parttable` |
| `pm-recover` | `pm-core`, `pm-fs`, `pm-parttable` |
| `pm-analyze` | `pm-core`, `pm-platform` |
| `pm-ops` | `pm-core`, `pm-fs`, `pm-image`, `pm-parttable`, `pm-platform` |
| `pm-cli` | `pm-core`, `pm-fs`, `pm-image`, `pm-parttable`, `pm-platform`, `pm-ops`, `pm-recover`, `pm-analyze` |
| `pm-gui` | the same set as `pm-cli` |

(`pm-image` is also a dev-dependency of `pm-recover` and `pm-analyze`, for tests only.)

Rules that keep the layers honest:

- **Only `pm-platform` talks to the operating system.** The Windows, Linux and macOS code
  lives behind `cfg` in `windows/`, `linux/`, `macos/` and the shared `posix/` (see
  [The platform layer per OS](#the-platform-layer-per-os)). Everything else works on the
  `BlockDevice` trait and plain data, so the engines are tested on files and in-memory
  buffers.
- **`pm-ops` never writes a disk without a `WriteSession`** from `pm-platform`, which
  encapsulates lock, dismount, offline, write and rescan.
- **GUI and CLI share `pm-ops` and the engines entirely.** The CLI is also the
  integration-test harness: most behavior is exercised on image files without administrator
  rights.
- **Six `Platform` implementations:** `WindowsPlatform`, `LinuxPlatform` and `MacPlatform`
  (real, chosen per OS by `default_platform()`), `NullPlatform` (every call fails with
  `Unsupported`; other OSes), `MockPlatform` (in-memory disks for unit tests) and
  `FilePlatform` (image files presented as disks; what `--disk-file` uses).

## The platform layer per OS

`pm_platform::default_platform()` picks the implementation by `cfg(target_os)`:
`WindowsPlatform`, `LinuxPlatform` or `MacPlatform`, and `NullPlatform` on any other OS.

```text
pm-platform/src/
  traits.rs     Platform, WriteSession, MountPoint; os_name(), elevation_hint(),
                disk_of_path(), canonical_volume_id() (additive, with defaults)
  aligned.rs    sector-aligned bounce buffer (Windows volumes, macOS raw nodes)
  windows/      windows-sys: IOCTLs, volume locks, FormatEx, bcdboot/bootsect
  posix/        (unix) FdDevice (pread/pwrite on an owned fd, flock), tool lookup and
                runner (system directories first, no shell, LC_ALL=C), smartctl JSON,
                statvfs, euid, the POSIX benchmark file
  linux/        sysfs enumeration, /proc mounts and mountinfo, BLK* ioctls, mount(2)/umount2,
                write sessions, mkfs.* / ntfsresize / resize2fs, system disk
  macos/        diskutil -plist parsing and enumeration, /dev/rdiskN behind AlignedDevice,
                write sessions, diskutil mount/unmount/eraseVolume, system disk
                (compiled on every OS so its fixtures run everywhere; the ioctls and the
                platform itself are macOS-only)
```

| `Platform` method | Linux | macOS |
|---|---|---|
| `enumerate_disks` | `/sys/block/*`: size, sector sizes, removable, read-only, model, serial, bus; partitions from `/sys/block/X/X1/{start,size}` (512-byte units), refined by `pm_parttable::read_table` when the node is readable (root); fs and label by signature; mount points from `/proc/self/mounts`; used/free via `statvfs` | `diskutil list -plist` + `diskutil info -plist` per disk and slice; APFS containers shown as their physical store; synthesized disks skipped; without root the table is `Unknown` and slice starts approximate |
| `open_disk` / `open_volume` | `/dev/sdX`, `/dev/sdXN` through `FdDevice` (`O_CLOEXEC`), length `BLKGETSIZE64`, sector `BLKSSZGET` | `/dev/rdiskN`, `/dev/rdiskNsM` through `FdDevice` wrapped in `AlignedDevice` (raw nodes need whole sectors) |
| `begin_write` | refuse holders (LVM/RAID/crypt) and swap; unmount every filesystem of the disk deepest first, re-mounting what was unmounted if one is busy; `udevadm settle`; open `O_RDWR|O_EXCL` + `flock`; check the disk is still the one enumerated (`BLKGETDISKSEQ` or size); `commit_table` = write + `BLKRRPART` (`partprobe` fallback) | `diskutil unmountDisk`, then `/dev/rdiskN` read-write; `commit_table` = write + `DKIOCSYNCHRONIZECACHE`; the kernel re-reads the map on close |
| `mount` / `unmount` | `mount(2)` with the type detected from the signature (fallback: `mount` command), `umount2` | `diskutil mount -mountPoint` / `diskutil unmount` |
| `format` | `mkfs.vfat`, `mkfs.exfat`, `mkfs.ntfs`, `mkfs.ext2/3/4`, `mkfs.xfs`, `mkfs.btrfs` | `diskutil eraseVolume` FAT32/FAT16/FAT12/ExFAT/JHFS+/APFS; NTFS `Unsupported` |
| `shrink_volume` / `extend_volume` | NTFS: `ntfsresize`; ext: `e2fsck -f` + `resize2fs`; others `Unsupported` | `Unsupported` |
| `smart` | `smartctl -j -a` (`None` without smartmontools) | same |
| `is_elevated` / `elevation_hint` | `geteuid() == 0` / "run with sudo (root)" | same |
| `system_disk` | the device of `/`; LVM, dm, md, overlay, ZFS and `/dev/root` give `None` (fail closed) | the physical store of `/`; Fusion / CoreStorage / multi-store roots give `None` |
| `disk_of_path` | `st_dev` -> `/sys/dev/block/M:m` (fallback: longest `/proc/mounts` prefix) | `diskutil info -plist` on the path and its ancestors |
| `free_letters` / `run_tool` | `[]` / `Unsupported` (no boot repair) | same |

The front ends contain no OS-specific code beyond `cfg` on display strings and paths: drive
letters are offered only when `free_letters()` is non-empty, boot repair only when
`os_name()` is `"windows"`, and every "is this file on the source disk" check goes through
`disk_of_path` (`pm_ops::location`).

## Data flow of a clone

This is the flow of `pm clone-disk` or the Copy Disk wizard. Migrate OS is the same flow with
a different partition selection and an optional boot fix.

```text
  GUI wizard / pm clone-disk
        |  collects src disk, dst disk, sizing (exact | fit | proportional), mode (smart | sector)
        v
  Operation::CloneDisk { .. }
        |
        v
  OpQueue::push  --> precheck (static refusals)   --> Planner::simulate(model, op)
        |                                                   |
        |      builds the destination Table, checks every   |
        |      destination partition >= used bytes of the   |
        |      source, rejects the system disk as target    v
        |                                             new Model  --> disk map preview / `pm` output
        v
  user clicks Apply  (or `pm ... --yes`)
        |
        v
  Executor::execute(platform, ops, opts, progress, cancel)
        |   for each operation, on a fresh live Model:
        |     0. layout_matches(expected, live)   the disks / partitions it names are still
        |                                         the ones it was queued against
        |     1. guard_system_disk(live, op)      refuse the running system disk
        |     2. precheck(live, op)               static refusals again, on live state
        |     3. Planner::simulate(live, op)      validate against what is really attached
        v
  platform.begin_write(dst)                       WriteSession: lock + dismount volumes,
        |                                         take the disk offline, open read-write
        v
  write an empty partition table (commit_table)
        |
        v
  for each partition:
        |   open the source as a SliceDevice over the source disk
        |   pm-fs: used_map() -> Bitmap  (smart mode; unknown filesystem => every sector)
        |   pm-image::rescue::copy_simple / rescue engine with the bitmap
        |   progress + cancel on every chunk
        v
  write the final table, regenerate GPT GUIDs / MBR signature  (commit_table)
        |
        v
  session.finish(): bring the disk online, rescan (IOCTL_DISK_UPDATE_PROPERTIES),
        unlock volumes;  optional boot fix via bcdboot / bootsect (run_tool)
        |
        v
  ExecReport { completed, failed, log, rescue_stats }
```

If an operation fails, the executor stops at the first failure and reports which step failed
and what had been written; later operations are not run.

## Where each safety check lives

| Layer | Where | What it refuses |
|---|---|---|
| 1. Planner | `pm-ops/src/planner.rs` (`Planner::simulate`) | Table edits on a disk whose table is missing or unreadable, wipes / images / clones of a partition whose exact range is unknown (Linux/macOS without root), overlaps, misalignment, minimum sizes, extended-partition container, destination smaller than used data, whole-disk destructive operations on the running system disk (and on every disk if the system disk is unknown) |
| 2. Precheck | `pm-ops/src/executor.rs` (`precheck`), also run by `OpQueue::push` | Operations the executor can never perform, so they never enter the queue |
| 3. Executor pre-flight | `pm-ops/src/executor.rs` (`execute_queued`, `guard_system_disk`), `pm-ops/src/identity.rs` (`layout_matches`) | Re-validation against the live disks; identity of every disk and partition; the system disk |
| 4. Platform session | `pm-platform` (`WriteSession`; `windows/`, `linux/`, `macos/session.rs`) | Any write without locked and dismounted volumes; no kernel driver. Linux: mountinfo-based refusal of bind / subvolume / covered mounts, re-mount on failure and after the session (except partitions written into), `O_EXCL` + `flock`, disk identity re-checked (`diskseq`). macOS: physical whole disks only, sector-aligned raw I/O, fail-closed system disk on Fusion / CoreStorage |

The direct-copy commands (`pm clone`, `restore`, `image`, `convert`, `rescue`) add their own
checks (source as destination, overlapping targets, system disk, unknown volume placement,
VMDK extent files) and write disks through a `WriteSession` and images to `FILE.partial`
before renaming. In rescue mode, a failed or cancelled export keeps `FILE.partial` and its
mapfile so the same command resumes. The full text of what each layer refuses is on the
[[Safety Model]] page.

## Further reading

- Design spec:
  [`docs/superpowers/specs/2026-10-19-partition-manager-design.md`](https://github.com/inalto/partition-manager/blob/main/docs/superpowers/specs/2026-10-19-partition-manager-design.md)
- Repository architecture document:
  [`docs/ARCHITECTURE.md`](https://github.com/inalto/partition-manager/blob/main/docs/ARCHITECTURE.md)
- Image layouts:
  [`crates/pm-image/README-formats.md`](https://github.com/inalto/partition-manager/blob/main/crates/pm-image/README-formats.md)
- Pages: [[Imaging and VM Formats]], [[Rescue Mode]], [[Roadmap]]
