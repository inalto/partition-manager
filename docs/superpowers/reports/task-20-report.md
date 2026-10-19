# Task 20 report: clone / migrate / image / rescue / recovery wizards and tool windows

Status: DONE. Commit `9f61a09` on `dev` (on top of Task 19's fix round `82676c1`).

## What was implemented

### Shared plumbing (`src/tools/mod.rs`)
- `Task<T>`: a typed `jobs::spawn_job` job. Its result comes back through the new
  `JobOutput::Any(Box<dyn Any + Send>)` variant (the one-line change to `jobs.rs`).
  **Dropping a `Task` cancels it**, so closing a window always cancels its job.
- `ToolWindow` trait (`kind`, `modal`, `writing`, `show -> WindowOutcome {Open, Close, Refresh}`)
  and `ToolEnv` (platform, the **live** model `queue.base()`, the pending ops). Tools act on
  the real disks, not on the preview.
- `write_problem(live, pending, disk)`: refuses the running system disk, fails closed when it
  is unknown (like the executor), and refuses disks with pending operations.
- `Device` (disk or partition) helpers, `open_range` (worker side: `open_disk` + `SliceDevice`),
  `device_combo`, `progress_panel`, `wizard_window` (the step frame of the job wizards: numbered
  pages left, page right, Back / Next / Close footer) and `tool_frame`.

### Operation wizards (Wizard framework, queue an `Operation`)
- **Copy Disk** (`wizards/copy_disk.rs`): source → target (excludes source and system disk,
  model/size table, red "all data erased") → options (Fit / Proportional / Exact with
  explanations, Smart / Sector, "Align partitions to 1 MB" always on, boot fix defaulting to
  UEFI for an EFI source, BIOS for MBR) → preview (source bar and planned target bar from
  `Planner::simulate`, which runs `plan_disk_clone`; `shrinks_source` lines in orange) →
  summary → `CloneDisk`. Pickers / options / preview are shared with Migrate OS.
- **Copy Partition**: source partition → free ranges (all initialized disks) able to hold the
  used data → size slider (min = data to copy, max = range) with a live preview → mode →
  `ClonePartition`.
- **Migrate OS**: "How to migrate?" A (all partitions) / B (only system partitions) → target →
  options (sizing names per the brief, 1 MB, the "use GPT" note for MBR source + target > 2 TB,
  boot fix default UEFI / All) → preview + shrink warning + "set the new disk as first boot
  device" → `MigrateOs { only_system_parts }`.
- **Create Image**: source (disk/partition, from the selection) → path (full path, folder must
  exist; check cached per text change) + format combo (Raw, VHD dyn/fixed, VMDK sparse/flat,
  QCOW2) with `fix_extension` → mode → `ImageDisk` / `ImagePartition`.
- **Restore Image**: path identified with `pm_image::identify` + capacity (read once when the
  text field loses focus) → whole disk or free range that fits (never the system disk) with a
  red warning → `RestoreImage`.
- Dispatch: `wizards::clone_for_action` / `CLONE_ACTIONS` (additive; Task 19's `for_action`
  and its test untouched). Image wizards open from the toolbar "Backup / Image" menu.

### Job wizards (modal windows; run immediately only after an explicit confirmation)
- **Rescue Copy** (`wizards/rescue.rs`): source → destination (disk / partition / image file;
  same-device overlap refusal, size check, system-disk + pending-ops refusal) → mapfile
  (default `<dest>.map`; an existing parseable mapfile shows a resume notice) → options (sector,
  cluster, skip, retries, reverse, no-trim, no-scrape, filesystem-aware skip of unused space)
  → run page: confirmation + "Start rescue"; live status (ipos, `format_rescue_status`,
  progress, Cancel); final stats + a mapfile bar coloured by block status. Mirrors
  `pm rescue`: source opened read-only, disk/partition destinations through
  `platform.begin_write` (+ `finish`, `rescan`), the system disk re-checked on the worker
  (fail closed), image files created as raw when missing. A disk write triggers a refresh.
- **Partition Recovery**: disk table + bar → Quick / Full (and GPT/MBR to create when the disk
  has no table) → scan job → results (checkboxes default ≥ 70 % confidence, "overlaps"
  flags, preview bar of `apply_candidates`) → red confirmation → write job:
  system-disk re-check, **re-reads the table in the write session and refuses if it changed
  since the scan**, `commit_table`, `finish`, `rescan` → refresh.
- **Data Recovery**: device cards ("Logical drives" with fs and free-of-total bar, "Hard
  disks", "Open saved session") → Deleted files (only FAT/NTFS/exFAT/ext volumes) or Deep scan
  with category checkboxes → scan job → results (virtualised `TableBuilder`: name, size, type,
  integrity/complete, path/offset; search, type filter, check shown/none) → destination
  (refused on the source drive's letters, like the CLI) → restore job → "Restored N, M failed"
  + errors; "Save session" (results and summary pages) writes a `ScanSession` on a job. Saved
  carve sessions are made disk-relative; a loaded session is matched to a live disk with
  `ScanSession::matches` (results viewable but not restorable when the disk is absent).

### Tool windows (non-modal)
- **Disk Benchmark**: drive combo / folder, transfer size from/to (4 KB..1 MB), total length
  (100 MB..1 GB), queue, threads, Sequential/Random, cooldown; Start/Stop; the job benchmarks
  one size at a time so the painter bar chart (read blue / write orange, value labels, axis)
  and the table fill in live.
- **Space Analyzer**: drive combo + path + Scan; tabs Tree view (lazy collapsible tree, size,
  %, files), File view (largest files), Folder view (top 100 folders, computed on the worker);
  treemap (`tools/treemap.rs::squarify`), hover tooltip with path and size, click drills
  down, Up button, "Show in treemap" from the tree.
- **Surface Test**: device + block size; Start/Stop; grid bucketed to ≤ 10 000 cells, green as
  `done` advances, red/orange from the final `SurfaceReport`; counters.
- **SMART**: disk combo, `platform.smart` on a job, `smart_summary` rows with coloured health
  dots, raw attribute table, "not available" for `None`.

### App integration (surgical)
- `app.rs`: `Tool` gained CreateImage, RestoreImage, RescueCopy, SurfaceTest, Smart plus
  `IMAGE_MENU` / `MORE`; `tool: Option<Tool>` → `windows: Vec<Box<dyn ToolWindow>>`;
  `Cmd::CloseTool` removed; `open_tool`, `tool_env`; `modal()` includes modal windows;
  `open_wizard` tries `clone_for_action` and opens the Partition Recovery / Surface Test
  windows for those sidebar actions.
- `main_window.rs`: draws the windows (taken out of the state for the frame), `Refresh`
  outcome → `Cmd::Refresh`; closing the app is refused while a window is writing.
- `views/toolbar.rs`: "Backup / Image" is a menu (Create / Restore Image, Rescue Copy) and a
  "Disk tools" menu (Surface Test, SMART). `dialogs/mod.rs`: the placeholder `tool_window`
  removed. `Cargo.toml`: pm-parttable (now a normal dep), pm-fs, pm-image, pm-recover,
  pm-analyze; dev-dep tempfile.

## Tests (TDD evidence)
- RED: `squarify` stubbed to `Vec::new()` → `squarify_areas_proportional` and
  `squarify_degenerate_inputs` failed ("left: 0, right: 7"). GREEN after the implementation.
- RED: `format_rescue_status` stubbed to `String::new()` → `rescue_status_text_formatting`
  failed (line count 0 ≠ 4). GREEN after the implementation.
- Op builders: `build_clone_disk_op_from_filled_state`, `build_clone_partition_op_from_filled_state`,
  `build_migrate_op_from_filled_state`, `build_image_ops_from_filled_state`,
  `build_restore_ops_from_filled_state` (each also checks `check_ops` and `wizard(state).build_ops()`).
- Pure checks: `destination_must_not_be_on_the_source` (Data Recovery), `rescue_destination_checks`,
  `rescue_options_and_map_bar`, `checked_by_confidence_and_overlaps_flagged`,
  `extension_follows_the_format`, `image_path_validation`, `grid_buckets_and_colours_blocks`,
  `transfer_size_ranges_and_options`, `folders_and_treemap_items`, `devices_and_write_guard`,
  `task_returns_typed_results_and_cancels_on_drop`, `gpt_note_for_large_targets_of_mbr_sources`.
- Integration on image files through `FilePlatform`: `rescue_copies_an_image_file_through_a_job`,
  `scan_and_apply_on_an_image` (lost FAT32 found by a quick scan, table written, a second write
  refused as "changed since the scan"), `scan_and_restore_deleted_files_on_an_image` (PDF carved
  from a FAT32 volume and restored).
- Headless rendering of every page of every new wizard / window (both themes),
  `clone_wizards_and_tool_windows_open` in app.rs, and the main-window frame test with windows open.
- Final: `cargo test -p pm-gui` → 112 passed, 0 failed, 0 warnings; `cargo build -p pm-gui`
  clean. The app was started on two `--disk-file` GPT images, opened ("Partition Manager"
  window, no stderr output) and closed. (`cargo clippy` is not installed for this toolchain.)

## Self-review
- No result is applied silently: op wizards only queue; rescue / partition-recovery write /
  file restore start only from an explicit button on a confirmation page (red text for writes).
- No disk I/O on the UI thread: every open/read/write/scan/benchmark/SMART/session load+save
  runs in a `Task`. Remaining small UI-thread file-system calls: `is_dir` / `exists` path
  checks and the mapfile / image header probe, each cached and recomputed only when the text
  changes.
- Every job is cancellable (Stop / Cancel buttons; closing a window drops and cancels its
  task). Writing windows cannot be closed (and the app cannot exit) while writing; the
  partition-table write itself is short and not interruptible.

## Concerns
- The framework's summary page (Task 19 `Wizard::summary_page`) runs `check_ops` every frame;
  for `RestoreImage` the planner opens the image file each frame, and for `ImageDisk` it does
  an `is_dir` — small synchronous I/O on the UI thread that I could not change without editing
  `wizard.rs`. Caching `checked_ops` per state change in the framework would fix it.
- Carve sessions saved by the CLI on a *partition* store partition-relative offsets; the GUI
  restores carved files of a loaded session from the whole matched disk (sessions saved by the
  GUI are converted to disk-relative). A session format field for the scanned range would make
  this exact.
- `platform.smart()` is a single blocking call: Cancel/close cannot interrupt it mid-call (the
  worker just finishes and its result is dropped).
- The Migrate OS "use GPT for the target" item is an informational note: `Operation::MigrateOs`
  has no table-kind field, so the copy keeps the source's table type.
- The rescue live status derives errsize / errors by parsing the engine's progress detail
  string (as the CLI does); the coloured mapfile bar is shown from the final mapfile, not live.
- Commit trailer uses `Claude Fable 5.1` as instructed by the task (the session reminder named
  a different model).

## Fix round 1

1. **Rescue re-checks the layout on the worker** (`wizards/rescue.rs`). The plan now carries
   the source and destination as disk + partition number together with the ranges the user
   confirmed, plus the pending operations. `run_rescue` re-reads the layout (`Model::from_platform`)
   and `recheck` refuses ("the layout of Disk N changed since the rescue was set up …; nothing
   was written") when the source or destination moved, changed size or vanished. It then re-runs
   `device_dest_problem` (overlap, size, system disk fail-closed, pending operations) and the path
   checks on the fresh layout before `begin_write`. A partition source is read from exactly the
   re-checked range; there is no whole-disk fallback anywhere, and `start` refuses when the UI
   model has no range for it.
   Test `a_layout_change_before_the_run_is_refused` uses a MockPlatform and stale UI models where
   the source moved, the source is gone, the destination partition changed size, or the partition
   is unknown. Each run is refused with the message, and `write_sessions_log()` stays empty.
2. **"New run…" goes back to the confirmation.** The results-page button now returns to the
   confirmation page and its checks instead of starting a write. Back also clears the error.
   Navigation moved into `RescueWizard::navigate`; test `new_run_goes_back_to_the_confirmation`.
3. **Rescue files are kept off the source and destination disks.** `rescue_path_problem` refuses a
   destination image file or a mapfile on any drive letter of the source disk, and a mapfile on
   the destination disk. It runs on the Destination, Mapfile and confirmation pages and again on
   the worker. The default mapfile is `<file>.map` next to an image destination, or
   `%USERPROFILE%\rescue-diskN[-pM].map` for device destinations (empty when there is no profile
   folder); it never uses `current_dir()`. Test `files_on_the_source_or_destination_disk_are_refused`.
   The shared helpers `tools::path_letter` and `tools::on_letters_problem` are also used by Data
   Recovery.
4. **Image wizards refuse files on the source or target disk.**
   - Create Image refuses an image path on a drive of the device being imaged: the partition's
     own letters, or every letter of a disk source. The check runs in the file step and in
     `build`. Imaging partition 1 into partition 2 of the same disk stays allowed, because only
     the source is read.
   - Restore Image never offers the disk that holds the image file. It names that disk ("not
     offered: it holds the image file"), and `build` refuses it with a clear message.
   - Tests: `image_file_on_the_source_is_refused`, `image_on_the_target_disk_is_refused`.
5. **`ScanSession` records the scanned region** (`pm-recover/src/session.rs`).
   - New fields: `offset: u64` and `scanned_len: u64`, both `#[serde(default)]`, where 0 / 0 means
     the whole device. Also `with_region()`, and `region(dev_len)`, which bounds-checks the region
     against the current device length.
   - pm-cli (`cmd_recover.rs`): every saved session (partitions, files, carve) records the region
     of the opened device (`session_of`). `open-session` prints the region ("Scanned region: 8.00 MB
     at offset 4194304 (offsets below are relative to it)").
   - pm-gui: sessions are saved with the region and keep region-relative offsets; the earlier
     disk-relative conversion is gone. A loaded session is located with `session_origin`, which
     falls back for older region-less files (deleted files: their `fs_offset` partition; carved:
     the whole disk). The load job opens the device and bounds-checks the region (`s.region(len)`).
     The restore reads exactly that region through `open_range`, whose `SliceDevice` bounds-checks
     again.
   - Data Recovery now restores from the origin recorded at scan time rather than re-resolving
     the device later.
   - Tests:
     - pm-recover `session_region_roundtrip_and_legacy_files`: JSON round trip, and a legacy file
       without the fields loads as 0 / 0 (whole device). `region` refuses a device that is too small.
     - pm-cli `partition_carve_session_records_its_region`: a carve of `img:1` records (4 MiB, 8 MiB),
       and the stored partition-relative offset extracts the right PDF through a slice of the image.
     - pm-gui `scan_and_restore_deleted_files_on_an_image`: a partition carve session is saved,
       loaded, located and restored, giving the same bytes.
     - pm-gui `rows_filter_and_categories`: the region, the legacy fallback, and a region that no
       longer fits.

Command: `cargo test -p pm-gui -p pm-recover -p pm-cli` (zero warnings)

```
test result: ok. 47 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (pm-cli unit)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (pm-cli tests/cli.rs)
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (pm-cli tests/cli_image.rs)
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (pm-cli tests/cli_ops.rs)
test result: ok. 117 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out  (pm-gui lib)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (pm-gui main)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (pm-recover lib)
test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (pm-recover tests/carve.rs)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (pm-recover tests/files.rs)
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (pm-recover tests/partitions.rs)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (pm-recover tests/session.rs)
```

Concern 2 of the first round (carve sessions without a region) is resolved by item 5.

## Fix round 2

- **Relative mapfile paths are refused.** The new `mapfile_path_problem` requires an absolute
  mapfile path ("… enter an absolute path"). A relative path would resolve against the
  process's working folder, which the drive-letter checks cannot see. The rule is applied in
  the wizard (`mapfile_problem`) and again on the worker (`recheck`).
- **The worker re-checks again right before writing.** `run_rescue` now runs `recheck` a second
  time after the optional used-map scan, on a freshly read layout, just before `begin_write` or
  the image file is opened. This covers the time the scan takes.
- **Test** `mapfile_must_be_an_absolute_path`: in the pure function, the wizard and the worker
  `recheck`, a relative path is refused and an absolute path on a drive of neither disk is
  accepted.
- **Documentation.**
  - The `tools::path_letter` and `tools::on_letters_problem` doc comments state that the
    letter-based checks do not cover folder mount points, junctions, UNC or `\?\Volume{…}`
    paths, and that relative paths have no letter.
  - README "Known limitations" (only that list was edited) adds the same drive-letter
    limitation. It also notes that old region-less scan sessions load as whole-device sessions,
    so a partition carve session from the old CLI restores the wrong bytes: delete it and
    rescan.

Command: `cargo test -p pm-gui` (zero warnings)

```
test result: ok. 118 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (pm-gui lib)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out     (pm-gui main)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out     (doc-tests)
```
