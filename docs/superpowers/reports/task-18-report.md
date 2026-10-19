# Task 18 report: pm-gui shell

**Status:** DONE. Commit `27a9328` on `dev`: `feat(gui): main window, disk table, disk map, pending operations`.

## What was implemented

New crate `crates/pm-gui`: a lib (`pm_gui`) and a bin (`partition-manager`). Nothing outside `crates/pm-gui` changed except `Cargo.lock`, which picked up the new eframe dependency tree. The lockfile diff otherwise only moves existing entries.

| File | Contents |
| --- | --- |
| `src/main.rs` | Argument parsing; `--help` and argument errors. Release builds use `windows_subsystem = "windows"` and `AttachConsole(ATTACH_PARENT_PROCESS)` so `--help` still prints. |
| `src/args.rs` | Small hand-written parser for `--disk-file <P>` (repeatable, also `--disk-file=P`), `--dry-run`, `-h/--help`, plus `USAGE`. |
| `src/main_window.rs` | `PartitionApp`: the eframe `App` (panels, dim overlay while a modal is open, dialogs, toast, banner); `platform_for(args)` returns `FilePlatform` (already landed in `ce09499`) or `default_platform()`; `run()`. The window cannot be closed while Apply is running (`CancelClose` plus a toast). |
| `src/app.rs` | `AppState` (platform, `OpQueue`, `Selected`, theme, dry run, elevated, banner, the running job, `pending_action`, toast, modal `Dialog`, properties window, tool window) and the `Cmd` enum. `AppState::handle(cmd)` is the only place the queue is mutated (UI thread). Also: refresh and Apply jobs, `poll_job`, all-or-nothing `push_ops`, `rebase`, `system_disk_ops`, `destructive_flags`. |
| `src/jobs.rs` | `spawn_job(name, FnOnce(&dyn ProgressSink, &CancelToken) -> Result<JobOutput>) -> JobHandle { name, progress: Arc<Mutex<ProgressReport>>, cancel, result: Receiver<Result<JobOutput>>, started }`. `poll()` never blocks; a panicked worker becomes an `Err`. `JobOutput::{Model, Applied{report, refreshed}, Text}`. |
| `src/actions.rs` | The `Action` enum (24 actions), `GROUPS` (Wizards / Convert disk / Clean disk / Check disk / Manage partition), `Action::enabled(sel, model)` and `Action::effect(sel, model) -> Effect`. |
| `src/selection.rs` | `Selected { None, Disk, Partition, Free(disk, start, len) }`, accessors, and `revalidate` (after queue changes or refresh). |
| `src/items.rs` | `disk_items(disk)`: partitions plus unallocated gaps of at least 1 MiB, including free space inside MBR extended containers. Containers themselves are hidden. Shared by the table and the map. |
| `src/format_util.rs` | `partition_row(&PartitionInfo) -> [String; 7]`, `free_row`, `disk_header`, `format_rate`, `format_eta`, `COLUMNS`. |
| `src/theme.rs` | Light theme (MiniTool-like greys and blue accent) and dark theme; `MapColors` (primary `#3C78C8`, logical cyan, unallocated grey, EFI/MSR dark blue, recovery indigo, selected `#E5A800`); `mix`. |
| `src/views/` | `toolbar`, `sidebar` (with `pending` at the bottom), `disk_table` (egui_extras `TableBuilder`, rows from `queue.preview()`), `disk_map` (pure `segments()` plus painter drawing: label block, colored top strip, usage fill, gold 2 px selection), `legend` (legend, disk source, elevation status, refresh spinner), `context_menu` (enabled sidebar actions for the right-clicked item). |
| `src/dialogs/` | `confirm` (op list with destructive ops in red, red system-disk banner, "allow relaxed system-disk changes" checkbox, Cancel / Apply or Simulate), `progress` (phase, detail, bar, done/total, speed, time left, elapsed, Cancel), `properties` (all `DiskInfo` fields), and in `mod.rs`: error modal, wipe method chooser, mount letter chooser, unmount chooser, Apply report, tool placeholder windows. |
| `build.rs`, `partition-manager.manifest`, `partition-manager.rc` | Embeds `requireAdministrator`, PerMonitorV2 DPI awareness, Windows 10/11 compatibility, long paths and common controls v6, **in release builds only**. `PM_GUI_MANIFEST=1/0` overrides this. |
| `README-gui.md` | Usage, elevation behaviour (debug builds have no manifest so there is no UAC prompt during development; release `--disk-file` runs still prompt), layout, Apply flow, architecture. |

**Actions implemented fully here:**
- Delete Partition, Set Active, Delete All Partitions (deletes highest number first, so logical partitions go before their container; pushed all-or-nothing).
- Align All Partitions, Rebuild MBR, Convert MBR to GPT, Convert GPT to MBR.
- Wipe Disk (method chooser), Mount (letter chooser: `platform.free_letters()` minus letters already used in the preview), Unmount (mount-point chooser).
- Disk Properties, and Explore (`explorer.exe <X:\>`, spawned without waiting).

**Wizard actions** (Migrate OS, Copy Disk/Partition, Partition Recovery, Surface Test, Create, Format, Resize/Move, Change Label/Letter/Type, Wipe Partition) set `pending_action = Some((action, selection))` and show the toast "… coming in the next step".

**Apply flow:**
1. A confirmation lists every op. The destructive flags are computed against the model each op will actually run on, and ops on the system disk are marked.
2. `pm_ops::execute` runs in a job with the user's dry-run setting and the `allow_system_disk` checkbox.
3. The progress dialog polls the job every 100 ms through `request_repaint_after`.
4. After a real run, `Model::from_platform` runs on the worker, followed by `queue.reset(model)` and a report dialog with the log and any failure.
5. A dry run keeps the queue. If re-enumeration fails, the queue is reset to an empty model and a banner asks the user to press Refresh.

**Refresh** (on start and from the toolbar) runs in a job. Errors show a non-blocking banner (Retry / Dismiss), with a hint when the process is not elevated, and the app still opens with an empty list. Pending operations that still validate on the new model are kept by `rebase`; the rest are dropped with a toast. This deviates from the brief's plain `queue.reset` for a manual Refresh; after Apply it is `reset`, as specified.

## Tests (TDD)

**RED:** the tests and API stubs (`todo!()`) were written first for `views/disk_map.rs::segments`, `jobs.rs` and `args.rs`. `cargo test -p pm-gui --lib` gave `10 passed; 10 failed`: every `disk_map`, `jobs` and `args` test panicked at `todo!()`, including `segments_sum_to_width`, `min_width_enforced`, `fake_job_reports_progress_and_completes`, `fake_job_cancels` and `panicking_job_reports_an_error`. The 10 that passed were `items`, `format_util` and `selection`, which had their implementation in place already.

**GREEN:** after the implementation, 20 of 20 passed. The final suite is **44 tests, 0 failed**, with zero warnings in `cargo build -p pm-gui`, `cargo test -p pm-gui` and `cargo build --release -p pm-gui`:
- **`disk_map`:** `segments_sum_to_width`, `min_width_enforced` (minimums pinned, the others scaled down keeping their ratio), `too_narrow_bar_splits_evenly`, `labels_usage_and_selection`, `empty_disk_gets_one_placeholder_segment`.
- **`jobs`:** a fake job reports progress while still running and then completes, delivering its result once; a fake job cancels; a panicking job turns into an error.
- **`format_util`:** rows for GPT data, EFI, MSR, MBR primary and logical, a raw partition, free space and the disk header; rate and ETA formatting.
- **`items`:** GPT layout, MBR container hidden with the free space inside it shown, uninitialized disk.
- **`actions`:** enable matrix for none, disk, partition and free selections, MBR Set Active rules, the extended container excluded, effects, and Delete All order (6, 5, 2, 1) accepted by the planner.
- **`app`** (MockPlatform with real tables written into `MemDevice`):
  - refresh on start; `NullPlatform` gives the banner and an empty list;
  - `push_ops` is all-or-nothing, plus undo and discard;
  - the selection falls back to the disk after a delete;
  - wizard actions are recorded in `pending_action`;
  - the wipe, unmount and properties popups open;
  - a full dry-run Apply goes through the job and keeps the queue;
  - `rebase` drops ops that no longer apply;
  - `system_disk_ops` and `destructive_flags`.
- **Others:** `theme`; `selection`; `disk_table` rows, including a queued delete showing as unallocated; `properties` fields; mount-point parsing.
- **`main_window`:**
  - `bundled_fonts_have_the_icons` checks that ✔ ⟲ ✖ ⟳ ⚙ ⚠ are in egui's bundled fonts;
  - `frames_render_headless` drives whole frames through the real `draw()` (all panels, the confirmation dialog, dark theme) without panicking.

## Manual runs

- `cargo run -p pm-gui -- --help` prints the usage and exits 0. `--bogus` prints an error plus the usage and exits 2.
- **The window opened and stayed up.** Each run was checked with a capture of only the app window (PrintWindow) and then killed:
  - `--disk-file <64 MiB MBR image>`: the table shows `Disk 0 (test-mbr.img, MBR, 64.00 MB)` with an Active primary, a primary and a `(Unallocated)` row. The map draws the proportional segments and the status bar shows "Disk files: test-mbr.img".
  - Debug build with no flags, unelevated: the real disks are listed (3 disks, EFI/MSR/Windows/WinRE, LaCie, exFAT MBR) and the status bar shows "Not elevated: read-only" in red.
- The release exe contains exactly one manifest resource, 24/1, with `requireAdministrator`; checked with `windres -i partition-manager.exe -O rc`. I did not launch the release exe, since that would raise a UAC prompt.

## Self-review

- **No blocking calls on the UI thread.**
  - Enumeration (`Model::from_platform`) and `execute` run only inside jobs, and the post-Apply enumeration also runs on the worker.
  - The UI calls only these cheap platform functions: `is_elevated()` once at start (token query) and `free_letters()` when the mount popup opens (`GetLogicalDrives`).
  - `Planner::simulate` runs on the UI thread through `push`, `rebase` and `destructive_flags`, but it works on the in-memory model.
- **The queue is mutated only in `AppState::handle` / `push_ops` / `poll_job`**, all on the UI thread. Views and dialogs get `&AppState` and emit `Cmd`s. Toolbar and sidebar actions are disabled while a job runs, and the main window is disabled (and dimmed) while a modal or Apply is active.
- **Safety:**
  - The destructive flag comes from `is_destructive_in` on the model each op will run on.
  - System-disk ops are flagged against the base model's `effective_system_disk`. The relax checkbox maps to `ExecOpts::allow_system_disk`, which the executor limits to the non-destructive family.
  - The window cannot be closed mid-Apply.
- **The view/state split is ready for Tasks 19 and 20:** a wizard can be one more `Dialog` variant (or its own window state) that emits `Cmd::Push`. `pending_action` carries `(Action, Selected)`. `spawn_job` and `JobOutput` are generic, with `Text` available and more variants easy to add.

## Concerns / notes

1. **The GNU manifest relies on a MinGW internal.** MinGW gcc always links its own `default-manifest.o`, an `asInvoker` manifest. Linking ours next to it gave ld ".rsrc merge failure: multiple non-default manifests" and left **both** manifests in the exe. `build.rs` now compiles ours into `OUT_DIR/manifest/default-manifest.o` with windres and passes `-B<dir>/` so gcc picks it instead. This depends on that gcc specs detail (`%:if-exists(default-manifest.o%s)`). The MSVC path uses `embed_resource::compile_for` and is untested here because there is no MSVC toolchain.
2. **Refresh keeps pending ops that still validate** instead of plain `queue.reset`; see the Refresh paragraph above.
3. **Planner calls on the UI thread** are fine for the ops queued in this task. Once Tasks 19 and 20 queue image ops (`RestoreImage` reads the image file in `simulate`), `rebase`/`push` will do file I/O on the UI thread; consider moving those into a job.
4. **In `--disk-file` mode the status bar says "Running as administrator"** because that is what `FilePlatform::is_elevated` reports. It is accurate enough, but it could say "image files" instead.
5. **Explore launches `explorer.exe` from the GUI crate** rather than through `Platform`. It is a shell action with no disk I/O.
6. **clippy is not installed** for the toolchain, so it was not run.
7. **Disk-level actions are enabled only for a `Selected::Disk` selection** (MiniTool-style). Disk Properties and Surface Test are also offered for a selected partition.
8. **Long disk names are clipped** in the table's header row, which uses the first column (initial width 300 px, resizable).

## Fix round 1

Two Important review findings are fixed. The edits to shared files were kept to targeted replacements, because Task 19 is editing pm-gui at the same time. Only my hunks are committed: Task 19's uncommitted `pub mod wizards;` line in `lib.rs` was left out of the staged change.

### 1. No `unsafe` and no OS calls in pm-gui

pm-platform gains two new files:
- **`console.rs`:** `pm_platform::console::attach_parent_console() -> bool`. On Windows it calls `AttachConsole(ATTACH_PARENT_PROCESS)` inside a `// SAFETY:`-commented `unsafe` block; on other OSes it is a no-op returning `false`. The `Win32_System_Console` feature was added to pm-platform's windows-sys features. Another agent's commit f22ee6b, which committed the whole `crates/pm-platform/Cargo.toml`, already carries that line.
- **`shell.rs`:** `pm_platform::shell::open_in_file_manager(&Path) -> Result<()>`, plus `file_manager_command()`. It runs `explorer.exe` on Windows, `open` on macOS and `xdg-open` elsewhere, without waiting; a missing path gives `NotFound` and a failed spawn gives `Error::Platform`.

Both modules are registered in pm-platform's `lib.rs`.

In pm-gui:
- `main.rs` calls `attach_parent_console()`.
- `app.rs::explore` calls `open_in_file_manager`.
- `windows-sys` is removed from pm-gui's `Cargo.toml` (and its edge from `Cargo.lock`).
- `#![forbid(unsafe_code)]` is now in both `main.rs` and `lib.rs`. `grep` finds no `unsafe` and no `std::process::Command` left in `crates/pm-gui/src`.

### 2. Nothing can change the queue under the Apply confirmation

- **Banner:** its Retry / Dismiss buttons are now drawn inside `add_enabled_ui(enabled, ..)`, so they are disabled while a modal is open.
- **`Cmd::Refresh`** returns early while any dialog is open, so a refresh (and its `rebase`) can no longer start under the confirmation.
- **Confirmation flags are no longer frozen:** `Dialog::Confirm` is now just `{ allow_system }`. `confirm::show` recomputes `destructive_flags(base, ops)` and `system_disk_ops()` from the live queue every frame; this is an in-memory simulation of a short list.
- **`Cmd::ConfirmApply`** only proceeds when the open dialog is `Dialog::Confirm`; otherwise it does nothing.
- **Failed Apply:** any Apply `Err`, including the "job stopped unexpectedly (panic)" path, shows the error and, unless it was a dry run, sets `reset_on_refresh` and starts a refresh. On that refresh the queue is reset onto the re-read disks instead of rebased, so an operation that was already written is never queued again.

### Tests added

- **`app::tests::confirm_apply_without_the_dialog_does_nothing`** (the negative test): `ConfirmApply` with no dialog, or with an Error dialog open, starts no job and leaves the queue and the dialog unchanged.
- **`app::tests::refresh_is_refused_while_a_dialog_is_open`:** Refresh does nothing under the confirmation and works after it closes; the rebased queue is kept.
- **`app::tests::failed_apply_refreshes_and_drops_the_queue`:** a panicking Apply worker leads to the "Apply failed" error, an automatic refresh, an empty queue and the disks re-read.
- **`shell::tests::missing_path_is_not_found_and_nothing_is_started`** and **`command_per_os`** in pm-platform.
- **`console::tests::attach_is_harmless_when_already_attached_or_absent`** in pm-platform.

### Commands

`cargo test -p pm-gui -p pm-platform`:
```
test result: ok. 50 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (pm_gui lib)
test result: ok. 0 passed; 0 failed; 0 ignored; ...                          (partition-manager bin)
test result: ok. 51 passed; 0 failed; 0 ignored; ...                          (pm_platform lib)
test result: ok. 4 passed; 0 failed; 0 ignored; ...                           (tests/file_platform.rs)
test result: ok. 5 passed; 0 failed; 0 ignored; ...                           (tests/mock.rs)
test result: ok. 0 passed; 0 failed; 3 ignored; ...                           (tests/windows_live.rs)
test result: ok. 0 passed; ... (doc-tests pm_gui, pm_platform)
```

Notes on these numbers:
- The pm_gui count of 50 includes 3 tests from Task 19's uncommitted `src/wizards/` work, which is in the shared working tree. My crate has 47: the 44 from before plus the 3 above.
- `cargo build -p pm-gui` has zero warnings from my files. The working tree currently shows 2 `unused variable` warnings in Task 19's uncommitted `src/wizards/resize_bar.rs`, which is not mine and not committed by me.
- `cargo run -p pm-gui -- --help` still prints the usage.

**The staged tree was checked on its own.** I exported the index with `git checkout-index` to a scratch directory, which leaves out Task 19's uncommitted files, and ran `cargo test -p pm-gui -p pm-platform` there with **zero warnings**:
```
test result: ok. 47 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (pm_gui lib)
test result: ok. 51 passed; 0 failed; 0 ignored; ...                          (pm_platform lib)
test result: ok. 4 passed; ... / 5 passed; ... / 0 passed; 3 ignored; ...      (pm_platform integration)
```
