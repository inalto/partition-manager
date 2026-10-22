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
| 1. Planner | `pm-ops/src/planner.rs` | Overlaps, misalignment, minimum sizes, an extended-partition container, a destination smaller than the used data, whole-disk destructive operations on the running system disk, and every disk when the system disk cannot be determined |
| 2. Precheck | `pm-ops/src/executor.rs`, also run when an operation is queued | Operations the executor can never perform, so they are not even accepted into the queue: set label (no platform API yet), resizing a recognized non-NTFS file system (ReFS may only grow), alignment that would shrink one, shrinking or aligning a partition whose file system is unknown, any write to a disk Windows reports as read-only or offline, a clone into a smaller target when the source is not an NTFS volume that can be shrunk online |
| 3. Executor pre-flight | `pm-ops/src/executor.rs`, `pm-ops/src/identity.rs` | Re-validates every operation against the live disks just before running it: each disk it names must still match the queue on size, serial and disk id, and each partition on start, length and GPT GUID. The running system disk is refused unless explicitly allowed, and then only for resize, label, letter, mount, unmount and creating a partition in free space. A system disk that cannot be determined fails closed. Inside the session, a table-only shrink re-reads the partition head and refuses a file-system signature |
| 4. Platform session | `pm-platform` (`WriteSession`) | Writes only through a session that holds locked and dismounted volumes; a volume that cannot be locked aborts the operation before anything is written. There is no kernel driver |

More on where each layer lives: [[Architecture]].

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

- writing onto the source;
- writing onto a partition or volume that overlaps the source;
- writing onto the running system disk (failing closed when it cannot be determined);
- volumes whose place on a disk is unknown;
- writing onto the extent file of a VMDK descriptor.

Images are written to `FILE.partial` and renamed when complete, so a failed run leaves an
existing file untouched.

Limits of these checks: hard links, junctions, storage aliases, a file stored on the disk
being copied and a differencing image's parent are not detected. In the GUI, the checks
that an image file, a rescue mapfile or a recovery destination is "not on the source or
target disk" look at drive letters only; a path that reaches the same volume through a
folder mount point, a junction, a UNC share or a `\\?\Volume{...}` name is not recognized.

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
