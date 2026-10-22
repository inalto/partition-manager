# Safety Model

Partition Manager is built so that no single bug or bypass in one front end can write to the
wrong disk. This page lists what protects you and where the gaps are. Read it before the
[[First Live Test]].

> Nothing has yet been run elevated against a real disk. These protections are tested on
> in-memory disks and image files only.

## What you see

- **Pending queue and Apply.** The GUI never writes when you pick an action: it queues an
  operation and shows the resulting layout. Nothing happens until you click **Apply**, which
  lists every destructive step and asks for confirmation. **Undo** and **Discard** remove
  queued operations. See [[GUI Guide]].
- **Dry run.** `Settings > Dry run` (or `partition-manager --dry-run`) makes Apply validate
  everything against the live disks and write nothing. The CLI has `--dry-run` as well.
- **The CLI needs `--yes`.** `pm create`, `format`, `resize`, `apply` and the other
  operation commands print the operation and validate it, and write nothing without
  `--yes`. There is no interactive prompt.
- **The direct-copy commands copy immediately.** `pm clone`, `restore`, `image`, `convert`
  and `rescue` start copying when run, after the refusals described below.
- **Elevation.** The release `partition-manager.exe` carries a `requireAdministrator`
  manifest and asks for UAC on launch. Debug builds do not.
- **Practice on image files.** `--disk-file PATH` swaps the platform for image files, so
  nothing physical is touched and no elevation is needed.

## The four layers

A destructive request passes four layers. Each one refuses on its own.

| Layer | Where | What it refuses |
|---|---|---|
| 1. Planner | `pm-ops/src/planner.rs` | Table edits (delete, resize, move, create) on a disk whose table is missing or could not be read; wiping, imaging or cloning a partition whose exact byte range is unknown (an unreadable table on Linux/macOS without root, where macOS offsets are approximate; a table-less disk's whole-disk file system qualifies), overlaps, misalignment, minimum sizes, an extended-partition container, a destination smaller than the used data, whole-disk destructive operations on the running system disk, and every disk when the system disk cannot be determined |
| 2. Precheck | `pm-ops/src/executor.rs`, also run when an operation is queued | Operations the executor can never perform, so they are not even accepted into the queue: set label (no platform API yet), resizing a recognized non-NTFS file system (ReFS may only grow), alignment that would shrink one, shrinking or aligning a partition whose file system is unknown, any write to a disk Windows reports as read-only or offline, a clone into a smaller target when the source is not an NTFS volume that can be shrunk online |
| 3. Executor pre-flight | `pm-ops/src/executor.rs`, `pm-ops/src/identity.rs` | Re-validates every operation against the live disks just before running it: each disk it names must still match the queue on size, serial and disk id, and each partition on start, length and GPT GUID. The running system disk is refused unless explicitly allowed, and then only for resize, label, letter, mount, unmount and creating a partition in free space. A system disk that cannot be determined fails closed. Inside the session, a table-only shrink re-reads the partition head and refuses a file-system signature |
| 4. Platform session | `pm-platform` (`WriteSession`) | Writes only through a session that holds locked and dismounted volumes; a volume that cannot be locked aborts the operation before anything is written. There is no kernel driver. See [Linux and macOS sessions](#linux-and-macos-sessions) |

More on where each layer lives: [[Architecture]].

## Linux and macOS sessions

**Linux** (`pm-platform/src/linux/session.rs`):

- A disk with holders (LVM, RAID, dm-crypt) or active swap is refused before anything is
  unmounted.
- Every filesystem of the disk is unmounted, deepest first, after reading
  `/proc/self/mountinfo`: bind mounts, btrfs subvolumes and mounts covered by another mount
  are refused, because they could not be restored exactly. If one filesystem is busy,
  everything already unmounted is mounted again with its original options and the session
  fails with "busy".
- The disk is opened `O_RDWR | O_EXCL` (the kernel refuses while anything still claims it,
  even a mount in another namespace) and locked with `flock`; the session then checks that
  the node is still the disk that was enumerated (`BLKGETDISKSEQ`, or the size on older
  kernels), so a hot-plug between listing and writing cannot redirect the write.
- After writing, the table is re-read (`BLKRRPART`, then `partprobe`) while udev is kept off
  the disk. When the session ends the filesystems are mounted again: all of them if nothing
  was written, otherwise those whose partition was not written into and still has the same
  start and length.

**macOS** (`pm-platform/src/macos/session.rs`):

- Only physical whole disks are accepted (not APFS containers or other synthesized disks).
- `diskutil unmountDisk` first; on failure the session fails with "busy" and what was
  unmounted is mounted again. After a session the volumes are not mounted again by
  Partition Manager (Disk Arbitration may mount new ones).
- I/O goes through the raw `/dev/rdiskN` node in whole sectors (`AlignedDevice`), so an
  unaligned request never reaches the device.
- The system disk is determined from `/`; on Fusion / CoreStorage or multi-store roots it is
  reported unknown, so the planner protects every disk (fail closed), as on Linux with `/`
  on LVM.

## System-disk rules

- The running system disk is **refused for destructive operations** (delete, format, wipe,
  convert, clone onto it, and so on) at the planner and the executor pre-flight; the
  direct-copy commands add their own check.
- **Resize, label, letter, mount and creating a partition in free space** on it need an
  explicit `--allow-system-disk` (CLI) or the GUI's checkbox. Destructive operations stay
  refused even then.
- **If the system disk cannot be determined, every disk is treated as protected.**
- **"Device busy" on the system disk.** The operations above are currently allowed by the
  checks but still stop with "device busy": every write session locks all volumes of the
  disk, and C: cannot be locked while Windows runs from it. Nothing is written. A lock-free
  path for these edits is planned (see [[Roadmap]]).

## Same disks as queued

Before each operation the disks are read again. Every disk and partition the operation names
must still be the one it was queued against (size, serial number and disk id; partition
position and GUID). Otherwise the run stops with "disk layout changed since the operations
were queued; refresh and queue again".

## Same-device and overlap rules

`pm clone`, `restore`, `image`, `convert` and `rescue` refuse:

- an image file, `.partial` or mapfile on the disk or partition being imaged or rescued, and
  a rescue mapfile on the destination disk (decided with `Platform::disk_of_path`, see
  below);

- writing onto the source;
- writing onto a partition or volume that overlaps the source;
- writing onto the running system disk (failing closed when it cannot be determined);
- volumes whose place on a disk is unknown;
- writing onto the extent file of a VMDK descriptor.

Images are written to `FILE.partial` and renamed when complete, so a failed run leaves an
existing file untouched.

Device arguments are normalized first: a whole-disk node (`/dev/sdb`, `/dev/disk2` or
`/dev/rdisk2`) is the same disk as its `disk:N`, and a macOS raw slice (`/dev/rdisk2s1`) the
same volume as `/dev/disk2s1`, so these checks see through the different names.

**"Not on the source disk" checks** (image files, rescue mapfiles and `pm recover files`
destinations in the CLI; the same files in the Create Image, Restore Image, Rescue Copy and
Data Recovery wizards) ask the platform which disk and partition a path is on:
the volume mount point and its extents on Windows (drive letters and folder mount points
alike), the filesystem's device on Linux (an LVM, dm-crypt or md volume is placed on the disk
all its member partitions are on), `diskutil info` on macOS. When the platform cannot tell
(a RAM disk, a network share on Linux/macOS, a volume spanning several disks) and the device
has a mounted filesystem or an LVM / RAID member partition, the path is refused with "Cannot
tell which disk ..."; a device with neither cannot hold the path. Windows UNC shares and
mapped network drives (which resolve to a UNC path) are accepted.

Limits of these checks: hard links, junctions, storage aliases, a file stored on the disk
being copied and a differencing image's parent are not detected.

## Data safety notes

- **Lock and dismount.** Writes happen inside a session that locks and dismounts every
  volume on the disk and takes a non-system disk offline. Any open handle on a target volume
  (Explorer windows, antivirus, the indexer) blocks the session; the operation stops before
  anything is written. If a run was interrupted, bring the disk back with
  `Set-Disk N -IsOffline $false`.
- **Data remanence.** Smart copies skip unused areas, so old data stays in the unused areas
  of the destination. Wipe the destination first if that matters (`pm wipe-disk`, method
  `zero`, `random` or `dod3`).
- **A move cannot be cancelled once copying starts.** Use a UPS or a charged battery.
- **No VSS snapshot.** Nothing takes a Volume Shadow Copy snapshot of a live system volume.
  Imaging or cloning a mounted, in-use volume (for example Migrate OS on a running C:) can
  capture an inconsistent file system: close applications, expect `chkdsk` on the first boot
  of the copy, and test the copy with the source detached. Image the system disk from
  another OS or from a disk that is not in use if you can.
- **Migrated copies get new ids.** Windows remembers which volume is `C:` by disk and
  partition id (`HKLM\SYSTEM\MountedDevices`). A copy made by Migrate OS or Copy Disk gets
  new ids unless you ask to keep them, so its first boot may map `C:` to the old disk or
  give the system volume another letter. Boot the copy with the source detached, or use
  *Keep the disk and partition ids* (`pm migrate-os --keep-ids`), which requires detaching
  the source before the first boot because Windows takes a disk with a duplicate id offline.
  Keeping ids needs the same table kind as the source. Neither route has been verified
  live.
