# pm-gui — Partition Manager desktop application

`cargo run -p pm-gui` builds and starts `partition-manager` (eframe/egui).

```
partition-manager [--disk-file <PATH>]... [--dry-run] [-h | --help]
```

| Option | Meaning |
| --- | --- |
| `--disk-file <PATH>` | Show raw image file `PATH` as a disk instead of the physical disks (repeatable, disk `i` is the `i`-th file). Uses `pm_platform::FilePlatform`; no administrator rights are needed to read or write the images. |
| `--dry-run` | Start with *Settings → Dry run* enabled: Apply validates every operation against the disks and writes nothing. |
| `-h`, `--help` | Print usage and exit. |

## Elevation (UAC manifest)

`build.rs` embeds `partition-manager.manifest` (`requestedExecutionLevel level="requireAdministrator"`,
per-monitor-v2 DPI awareness, Windows 10/11 compatibility, common controls 6) through
`embed-resource` — **in release builds only**:

* `cargo build --release -p pm-gui`: the executable always starts elevated; Windows shows the UAC
  prompt on launch. This also applies to `--disk-file` runs: a manifest cannot be bypassed per launch.
* `cargo run -p pm-gui` (debug): no manifest, no UAC prompt. Started from an unelevated shell the
  app opens with an empty disk list, a banner explaining that the disks could not be read, and
  "Not elevated: read-only" in the status bar. Use `--disk-file` to work on image files, or start the
  debug build from an elevated terminal.
* `PM_GUI_MANIFEST=1` forces the manifest into a debug build, `PM_GUI_MANIFEST=0` leaves it out of a
  release build.

With MSVC the manifest is embedded through `embed-resource`. With the GNU toolchain MinGW's gcc
always links its own `default-manifest.o` (`asInvoker`); linking a second manifest makes ld fail
to merge them and leaves both in the file. `build.rs` therefore compiles ours with `windres` (found
next to the configured linker, i.e. the portable MinGW `gcc.exe`) into an object named
`default-manifest.o` and passes `-B<that dir>/` to gcc, so ours replaces the default one. Check with
`windres -i target/release/partition-manager.exe -O rc`: exactly one `Type: 24 / Name: 1` resource
with `requireAdministrator`.

Release builds use the Windows GUI subsystem (no console window); `--help` and argument errors are
printed to the console of the parent process when there is one.

## Layout

* **Toolbar**: Apply / Undo / Discard (disabled while the queue is empty or a job runs), the tool
  windows (Data Recovery, Partition Recovery, Disk Benchmark, Space Analyzer, Backup / Image —
  placeholders until their step), Refresh, Settings (dark theme, dry run).
* **Sidebar**: Wizards, Convert disk, Clean disk, Check disk, Manage partition. Each action is enabled
  by the selection (`actions::Action::enabled`): disk actions need a disk, partition actions a
  partition, Create needs unallocated space. Below: *Pending operations: N* with every summary
  (destructive ones in red).
* **Table**: Partition | Capacity | Used | Unused | File system | Type | Status, grouped by disk, built
  from the queue's *preview* model so queued operations show immediately; unallocated gaps of at
  least 1 MiB are `(Unallocated)` rows; MBR extended containers are hidden behind their logical
  partitions (free space inside them is shown).
* **Disk map**: one bar per disk with segments proportional to size (minimum 40 px, the others
  scaled down to make room) and a usage fill; click selects, right-click opens the same actions
  as the sidebar.
* **Status bar**: color legend, disk source, elevation status.

## Apply

Apply opens a confirmation listing every operation (red = destructive in the model it will run on)
with a red banner when the running system disk is involved; a checkbox there relaxes the
executor's guard for the non-destructive family (resize, letter, label, mount) only. The run
happens on a worker thread (`jobs::spawn_job`) with a progress dialog (phase, bytes, speed, time
left, Cancel); afterwards the disks are re-read on the worker and the execution log is shown.
A dry run keeps the queue.

## Architecture

* `app.rs` — `AppState` (queue, selection, dialogs, the running job) and `Cmd`; the queue is only
  mutated in `AppState::handle`, on the UI thread.
* `views/*`, `dialogs/*` — draw from `&AppState` and push `Cmd`s.
* `actions.rs`, `items.rs`, `format_util.rs`, `views/disk_map.rs::segments`, `selection.rs` — pure,
  unit-tested logic.
* `jobs.rs` — worker threads with a shared `ProgressReport`, a `CancelToken` and a result channel,
  polled every frame (`request_repaint_after(100 ms)` while busy).
* Actions that need a wizard (Create, Format, Resize/Move, Copy, Migrate, …) are recorded in
  `AppState::pending_action` with a "coming in the next step" toast.
