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

- **Only `pm-platform` talks to the operating system** (the `windows-sys` calls live in
  `pm-platform/src/windows/`, behind `#[cfg(windows)]`). Everything else works on the
  `BlockDevice` trait and plain data, so the engines are tested on files and in-memory
  buffers.
- **`pm-ops` never writes a disk without a `WriteSession`** from `pm-platform`, which
  encapsulates lock, dismount, offline, write, rescan.
- GUI and CLI share `pm-ops` and the engines entirely. The CLI is also the integration-test
  harness: most behaviour is exercised on image files without administrator rights.
- `pm-platform` has four implementations of the `Platform` trait: `WindowsPlatform`
  (real), `NullPlatform` (every call fails with `Unsupported`; what `default_platform()`
  returns on non-Windows targets), `MockPlatform` (in-memory disks for unit tests) and
  `FilePlatform` (image files presented as disks; this is what `--disk-file` uses).

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
| 1. Planner | `pm-ops/src/planner.rs` (`Planner::simulate`) | overlaps, misalignment, minimum sizes, an extended-partition container, a destination smaller than the used data, whole-disk destructive operations on the running system disk, and on every disk when the system disk cannot be determined |
| 2. Precheck | `pm-ops/src/executor.rs` (`precheck`), also run by `OpQueue::push` | operations the executor can never perform, so they are not even accepted into the queue: `SetLabel` (no platform API yet), resizing a recognised non-NTFS filesystem (ReFS may only grow), alignment that would shrink one, shrinking or aligning a partition whose filesystem is unknown, any write to a disk Windows reports as read-only or offline, a clone into a smaller target when the source is not an NTFS volume that can be shrunk online |
| 3. Executor pre-flight | `pm-ops/src/executor.rs` (`execute_queued`, `guard_system_disk`), `pm-ops/src/identity.rs` (`layout_matches`) | re-validates every operation against the live model just before running it: every disk it names must still match the queue's model on size, serial and disk id and every partition on start, length and GPT GUID (MBR logicals are renumbered in the simulation as on disk); the running system disk is refused unless `allow_system_disk` is set, and then only for resize, label, letter, mount, unmount and creating a partition in free space; an undeterminable system disk fails closed. Inside the session, a table-only shrink re-reads the partition head and refuses a filesystem signature |
| 4. Platform session | `pm-platform` (`WriteSession`, `windows/session.rs`) | writes only through a session that holds locked and dismounted volumes (a volume that cannot be locked aborts the operation); no kernel driver is used |

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
