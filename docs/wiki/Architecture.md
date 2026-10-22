# Architecture

Partition Manager is a Cargo workspace of ten crates. Dependencies flow inward: the two
front ends (`pm-cli`, `pm-gui`) sit on top of the operation layer (`pm-ops`) and the
engines, and everything builds on `pm-core`.

## Crates

| Crate | Purpose |
|---|---|
| `pm-core` | `BlockDevice` trait, file/memory/slice devices, errors, progress and cancellation, size units, disk and partition domain model |
| `pm-parttable` | MBR and GPT read/plan/write, alignment, MBR/GPT conversion, lost-table search |
| `pm-platform` | `Platform` trait; Windows implementation (`windows-sys`), `NullPlatform` stub, `FilePlatform` (`--disk-file`), `MockPlatform` (tests) |
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

- **Only `pm-platform` talks to the operating system.** The `windows-sys` calls live behind
  `#[cfg(windows)]`. Everything else works on the `BlockDevice` trait and plain data, so the
  engines are tested on files and in-memory buffers.
- **`pm-ops` never writes a disk without a `WriteSession`** from `pm-platform`, which
  encapsulates lock, dismount, offline, write and rescan.
- **GUI and CLI share `pm-ops` and the engines entirely.** The CLI is also the
  integration-test harness: most behavior is exercised on image files without administrator
  rights.
- **Four `Platform` implementations:** `WindowsPlatform` (real), `NullPlatform` (every call
  fails with `Unsupported`; what non-Windows targets get), `MockPlatform` (in-memory disks
  for unit tests) and `FilePlatform` (image files presented as disks; what `--disk-file`
  uses).

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
| 1. Planner | `pm-ops/src/planner.rs` (`Planner::simulate`) | Overlaps, misalignment, minimum sizes, extended-partition container, destination smaller than used data, whole-disk destructive operations on the running system disk (and on every disk if the system disk is unknown) |
| 2. Precheck | `pm-ops/src/executor.rs` (`precheck`), also run by `OpQueue::push` | Operations the executor can never perform, so they never enter the queue |
| 3. Executor pre-flight | `pm-ops/src/executor.rs` (`execute_queued`, `guard_system_disk`), `pm-ops/src/identity.rs` (`layout_matches`) | Re-validation against the live disks; identity of every disk and partition; the system disk |
| 4. Platform session | `pm-platform` (`WriteSession`, `windows/session.rs`) | Any write without locked and dismounted volumes; no kernel driver |

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
