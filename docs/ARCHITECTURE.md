# Architecture

Partition Manager is a Cargo workspace of ten crates. Dependencies flow inward: the two
front ends (`pm-cli`, `pm-gui`) sit on top of the operation layer (`pm-ops`) and the
engines, and everything builds on `pm-core`. The full design rationale is in
[`superpowers/specs/2026-10-19-partition-manager-design.md`](superpowers/specs/2026-10-19-partition-manager-design.md).

## Crate dependency graph

The diagram shows the layering (higher boxes depend on lower ones); the table below lists
the exact direct dependencies.

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

Edges not drawn: several crates also use `pm-fs` and `pm-parttable` where the table below
says so.

The exact direct edges (from the crate manifests):

| Crate | Depends on |
|---|---|
| `pm-core` | (nothing in the workspace) |
| `pm-parttable` | `pm-core` |
| `pm-fs` | `pm-core` |
| `pm-image` | `pm-core`, `pm-fs` |
| `pm-platform` | `pm-core`, `pm-fs`, `pm-parttable` |
| `pm-recover` | `pm-core`, `pm-fs`, `pm-parttable` (`pm-image` in dev-dependencies only) |
| `pm-analyze` | `pm-core`, `pm-platform` (`pm-image` in dev-dependencies only) |
| `pm-ops` | `pm-core`, `pm-fs`, `pm-image`, `pm-parttable`, `pm-platform` |
| `pm-cli` | `pm-core`, `pm-fs`, `pm-image`, `pm-parttable`, `pm-platform`, `pm-ops`, `pm-recover`, `pm-analyze` |
| `pm-gui` | the same set as `pm-cli` |

Crate rules:

- **Only `pm-platform` talks to the operating system** (`pm-platform/src/windows/`,
  `linux/`, `macos/` and the shared `posix/`, behind `cfg`; see
  [The platform layer per OS](#the-platform-layer-per-os)). Everything else works on the
  `BlockDevice` trait and plain data, so the engines are tested on files and in-memory
  buffers.
- **`pm-ops` never writes a disk without a `WriteSession`** from `pm-platform`, which
  encapsulates lock, dismount, offline, write, rescan.
- GUI and CLI share `pm-ops` and the engines entirely. The CLI is also the integration-test
  harness: most behaviour is exercised on image files without administrator rights.
- `pm-platform` has six implementations of the `Platform` trait: `WindowsPlatform`,
  `LinuxPlatform` and `MacPlatform` (real, chosen by `default_platform()` per OS),
  `NullPlatform` (every call fails with `Unsupported`; other OSes), `MockPlatform` (in-memory
  disks for unit tests) and `FilePlatform` (image files presented as disks; this is what
  `--disk-file` uses).

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

## Data flow: cloning a disk

This is the flow of `Operation::CloneDisk` (`pm clone-disk`, or the Copy Disk wizard in the
GUI). Migrate OS is the same flow with a different partition selection and an optional boot
fix.

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

## Where the safety checks live

A destructive request passes four layers. Each one refuses on its own, so a bug or a bypass
in one front end does not defeat the others.

| Layer | Where | What it refuses |
|---|---|---|
| 1. Planner | `pm-ops/src/planner.rs` (`Planner::simulate`) | table edits on a disk whose table is missing or unreadable (`TableKind::Unknown`, e.g. Linux/macOS without root), reading or wiping a partition whose exact range is unknown (only MBR/GPT partitions and a table-less disk's whole-disk filesystem qualify), overlaps, misalignment, minimum sizes, an extended-partition container, a destination smaller than the used data, whole-disk destructive operations on the running system disk, and on every disk when the system disk cannot be determined |
| 2. Precheck | `pm-ops/src/executor.rs` (`precheck`), also run by `OpQueue::push` | operations the executor can never perform, so they are not even accepted into the queue: `SetLabel` (no platform API yet), resizing a recognised non-NTFS filesystem (ReFS may only grow), alignment that would shrink one, shrinking or aligning a partition whose filesystem is unknown, any write to a disk Windows reports as read-only or offline, a clone into a smaller target when the source is not an NTFS volume that can be shrunk online |
| 3. Executor pre-flight | `pm-ops/src/executor.rs` (`execute_queued`, `guard_system_disk`), `pm-ops/src/identity.rs` (`layout_matches`) | re-validates every operation against the live model just before running it: every disk it names must still match the queue's model on size, serial and disk id and every partition on start, length and GPT GUID (MBR logicals are renumbered in the simulation as on disk); the running system disk is refused unless `allow_system_disk` is set, and then only for resize, label, letter, mount, unmount and creating a partition in free space; an undeterminable system disk fails closed. Inside the session, a table-only shrink re-reads the partition head and refuses a filesystem signature |
| 4. Platform session | `pm-platform` (`WriteSession`; `windows/session.rs`, `linux/session.rs`, `macos/session.rs`) | writes only through a session that holds locked and dismounted volumes (a volume that cannot be locked aborts the operation); no kernel driver is used. Linux: unmount every filesystem of the disk (refusing bind mounts, btrfs subvolumes and covered mounts it could not restore, re-mounting with the original options if another one is busy, and again when the session ends except partitions it wrote into), `O_EXCL` + `flock`, and a check that the disk is still the one enumerated (`BLKGETDISKSEQ`). macOS: physical whole disks only, I/O in whole sectors through the raw node, `diskutil unmountDisk` (re-mounted on failure) |

On top of these:

- **Pending queue and Apply.** The GUI and `pm apply` never write when an operation is
  merely requested. The GUI queues operations and shows the simulated result; **Apply**
  shows a confirmation listing every destructive step. The CLI operation commands write
  only with `--yes`, and `--dry-run` runs the whole validation path without writing.
- **Direct-copy commands** (`pm clone`, `restore`, `image`, `convert`, `rescue`) refuse the
  source as destination, overlapping targets, the running system disk (failing closed when it
  cannot be determined), volumes whose place on a disk is unknown, and the extent file of a
  VMDK descriptor; they write disks, partitions and volumes through a `WriteSession` and
  images to `FILE.partial` before renaming. In rescue mode (`pm image --rescue`, the
  `ImageDisk` / `ImagePartition` operations with `RescueSettings`) a failed or cancelled
  export keeps `FILE.partial` and its ddrescue mapfile so the same command resumes; `pm rescue`
  writes a destination image it creates in place, and refuses an existing one with `--format`
  unless its mapfile is there to resume.
- **`--disk-file`** swaps the platform for `FilePlatform`, so practice runs touch only
  image files and need no elevation.
- **Elevation.** Release builds of `partition-manager.exe` embed a `requireAdministrator`
  manifest; debug builds do not, so development does not raise a UAC prompt.
