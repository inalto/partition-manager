# Rescue-mode VM image export — implementation report

Plan: `docs/superpowers/plans/2026-10-20-rescue-image-export.md` (3 tasks). Branch `main`,
one commit per task. Final run: `cargo test --workspace --no-fail-fast`, exit 0, 633 passed,
0 failed, 3 ignored (the `windows_live` tests, as before), zero warnings (test and non-test
builds). Clippy is not installed for the pinned GNU toolchain and was not run.

## Task 1 — pm-ops `RescueSettings` + executor rescue mode (+ mock faulty disks)

Commit `6aabf5f feat(ops): rescue-mode image export with resumable mapfile` (plus a follow-up
note/test in the Task 3 commit, see below).

What changed:

- `pm_ops::RescueSettings { retries, skip_size, cluster_size, no_trim, no_scrape, reverse,
  mapfile }`: `Default`, serde (`#[serde(default)]`, so partial JSON fills in defaults),
  `PartialEq`/`Eq`. `cluster_size` is in sectors, like ddrescue's `-c` / `pm rescue -c`.
  `ImageDisk` / `ImagePartition` gain `#[serde(default)] rescue: Option<RescueSettings>`
  (old operation files still load). `summary()` adds `, rescue mode (N retries)` inside the
  parentheses: `Image Disk 1 → x.vhd (VHD (dynamic), sector by sector, rescue mode (3 retries))`.
- `steps/image.rs`: without `rescue` the step is unchanged (abort on first error, partial
  removed). With it, `pm_ops::export_rescue_image` (public, also used by the CLI):
  - mapfile = `settings.mapfile` or `<image>.map` (`pm_ops::mapfile_path`);
  - both `<image>.partial` and the mapfile exist → resume: the partial is reopened read-write
    (raw through `FileDevice` so content signatures never pick a format; other formats must
    `identify` as the requested one), capacity must match, else a clear refusal telling the
    user to delete the partial + mapfile or pick another file;
  - mapfile without a partial (e.g. the one kept next to a finished image) → it starts over
    (fresh map written, note logged) instead of skipping data that is not in the new image;
  - engine: `abort_on_error = false`, retries / skip / cluster×sector / trim / scrape /
    reverse mapped, `zero_unused = false` — unreadable and unused areas are never written,
    so every format reads them as zeros and sparse formats allocate nothing;
  - cancel / error with a mapfile on disk → both files kept, note
    `kept … and …; resume by running the same operation again`; without a mapfile the
    partial is discarded;
  - success → rename (mapfile stays beside the image), note
    `rescued X, errsize Y, errors N, mapfile P` (`errors` = unrescued areas, as ddrescue
    counts them), `ExecReport.failed` stays `None`, stats pushed to `rescue_stats`.
- `executor.rs` `precheck`: rescue-mode images refuse a mapfile whose folder does not exist,
  a mapfile equal to the image (or its `.partial`), and `cluster_size == 0`.
- `pm-image`: `RescueStats` gained `#[serde(default)] mapfile: Option<PathBuf>` (set from
  `RescueOpts::mapfile`) so the GUI report can show it.
- `pm-platform` `MockPlatform::add_device(index, Box<dyn BlockDevice + Send>, DiskInfo)`:
  additive (`add_disk` unchanged); disks are now an internal `Store::{Mem, Dev}`;
  `disk_bytes` zero-fills sectors a device cannot read.
- `pm-ops` dev-dependency `pm-image` with `features = ["testing"]` (FaultyDevice).
- Existing literals updated with `rescue: None` (pm-ops tests, pm-cli `cmd_ops`, pm-gui
  `image_create`).

Tests (`crates/pm-ops/tests/executor_rescue.rs`):
`image_in_rescue_mode_pads_bad_sectors_and_writes_mapfile` (two bad sectors on a
FaultyDevice-backed mock disk → VHD equals the source with zeros there, mapfile has exactly
two 512-byte `-` blocks, `failed == None`, log line has `errors 2` and the mapfile path; then
the same op again starts over and yields the same image),
`image_without_rescue_still_aborts_on_error`, `image_rescue_resume_continues_partial`
(cancelled by a progress sink after the first reported chunk; partial + mapfile kept; second
run's engine reads never touch a range the first run marked `+`; finished image correct),
`image_rescue_all_bad_source_terminates` (every sector bad, 2 retries → success, image all
zeros with the source's size, qcow2 file < 1 MiB, mapfile one `-` range),
`operation_rescue_settings_serde_roundtrip_and_default`, plus
`precheck_refuses_a_mapfile_in_a_missing_folder`; pm-platform
`mock_add_device_backs_a_disk_with_any_device`.

RED: before implementation the test target did not compile —
`unresolved import pm_image::rescue::FaultyDevice`, `unresolved import pm_ops::RescueSettings`,
`no method named add_device found for struct MockPlatform`,
`variant Operation::ImageDisk has no field named rescue`, `no field mapfile on type &RescueStats`.
First GREEN run: 5/6 passed; the serde test's `ends_with(", rescue mode (3 retries)")` failed
because the suffix is rendered inside the parentheses — the assertion was adjusted to the
chosen wording. Then `executor_rescue: 6 passed`.

## Task 2 — CLI

Commit `7c2bac9 feat(cli): --rescue for image export; pm rescue creates image destinations`.

- `cli.rs`: `RescueModeArgs { --rescue, -r/--retries, -K/--skip-size, -c/--cluster-size,
  --no-trim, --no-scrape, --reverse, --mapfile }`, every option `requires = "rescue"`,
  flattened into `image`, `image-disk`, `image-partition`; `settings()` builds the
  `RescueSettings`. `pm rescue` gains `--format` (long only; the GNU short flags
  -b -c -K -r -R -n -N are untouched).
- `pm image … --rescue`: a `disk:N` / `disk:N:P` source builds `ImageDisk` /
  `ImagePartition` and runs it through `run_ops` (validated, printed, executed without
  `--yes`, as `pm image` always copies immediately). Other sources (image files, volumes)
  call the same `pm_ops::export_rescue_image` directly, after the existing source-overwrite
  refusal (now also covering the mapfile). Without `--rescue`, `pm image` is unchanged.
- `pm rescue SRC DST MAPFILE [--format F]`:
  - `DST` a path that does not exist (bare or `file:`) → created with `pm_image::create`,
    capacity = source length rounded up to 512, format from `--format` or the extension; all
    refusals first (mapfile already present without its destination, missing folders,
    `--dst-offset`, an existing flat-VMDK extent, mapfile = destination, bad flags), and the
    new image is removed again if the run fails before any mapfile exists;
  - `--format` with an existing image → only a resume: the mapfile must exist and parse and
    the image must identify as that format, else `'…' already exists; refusing to overwrite it`;
  - `--format` with a device destination (`disk:`, letter, volume, file partition) → usage error;
  - without `--format`, an existing destination behaves exactly as before.
- `docs/CLI.md` regenerated with `scripts/gen-cli-docs.sh`; README: rescue-into-VM-image
  examples in the CLI block and the feature bullet; ARCHITECTURE: one paragraph on rescue
  mode's partial/mapfile handling.

Tests (`crates/pm-cli/tests/cli_image.rs`): `cli_image_rescue_on_file_writes_mapfile_and_image`
(raw file → `--rescue --format qcow2 -r 1 -K 128K -c 64`: image identical, mapfile all `+`;
`--disk-file … image disk:0 --rescue --mapfile` runs the operation; rescue flags without
`--rescue` exit 1; `image-disk --rescue -r 3 --no-trim --emit-json` carries the settings),
`cli_rescue_creates_destination_image_with_format` (VHD of the source's capacity with equal
contents; qcow2 from the extension; `disk:7 --format` refused),
`cli_rescue_refuses_existing_image_without_mapfile` (no mapfile / unparsable mapfile → exit 1,
file untouched; a real resume succeeds; resume as another format refused; leftover mapfile
without its destination refused).

RED: all three failed — `error: unexpected argument '--rescue' found` and
`error: unexpected argument '--format' found`. GREEN: `cli_image: 17 passed`.

## Task 3 — GUI

Commit `feat(gui): damaged-disk mode in Create Image; rescue stats in the report` (this
report is part of it).

- `wizards/image_create.rs`: new page "Damaged disk" (Create Image only): checkbox "Continue
  on read errors (ddrescue mode)" with the explanation that unreadable blocks are retried, then
  written as zeros and that a mapfile next to the image allows resuming; retries 0–10, skip
  size combo (64 KB … 16 MB), trim / scrape / reverse toggles, mapfile field (default
  `<image>.map`, follows the image path until edited). `mapfile_problem`: absolute, not on a
  drive of the source (`on_letters_problem`), folder exists, not the image itself. Summary rows
  (read errors / skip size / phases / mapfile). `build()` sets `rescue` and refuses what the
  page refuses. No `set_min_height` added; the page lives in the existing bounded scroll area.
- `dialogs/mod.rs`: `rescue_lines(&[RescueStats])` → `Image N: rescued X, errsize Y,
  errors E, mapfile P`; shown in the Apply report, orange (`tools::WARN`) with a warning icon
  when errors > 0.
- `wizards/rescue.rs`: the "Image file" destination has a "Format of a new image" combo
  (`image_create::FORMATS`); changing it fixes the extension (`fix_extension`), and a missing
  file is created in that format (was always raw). The confirmation says "(new <format> image)".
- Follow-up to Task 1 included here: the "mapfile starts over" log note and its assertion in
  `image_in_rescue_mode_pads_bad_sectors_and_writes_mapfile`.

Tests: `build_image_op_with_rescue_settings`, `damaged_page_requires_absolute_mapfile_off_source`
(also renders the page with rescue on in a 1000×600 viewport and checks the window stays
inside it, and checks Restore Image has no such page),
`report_rescue_lines_show_stats_mapfile_and_warn_on_errors`,
`rescue_creates_a_new_destination_image_in_the_chosen_format`.

RED: the first two did not compile before the implementation (`cannot find struct …
RescueSettings`, `no field rescue on type ImageCreateState`, `cannot find function
damaged_summary`). The report-line and rescue-wizard-format tests were written together with
their (small) implementations and were not observed failing first. GREEN: `pm_gui lib:
127 passed` (123 before).

## Final `test result:` lines (`cargo test --workspace --no-fail-fast`)

```text
pm-analyze  lib            ok. 0 passed; 0 failed; 0 ignored
pm-analyze  bench          ok. 6 passed; 0 failed; 0 ignored
pm-analyze  smart          ok. 3 passed; 0 failed; 0 ignored
pm-analyze  space          ok. 5 passed; 0 failed; 0 ignored
pm-analyze  surface        ok. 3 passed; 0 failed; 0 ignored
pm-cli      bin pm         ok. 51 passed; 0 failed; 0 ignored
pm-cli      cli            ok. 7 passed; 0 failed; 0 ignored
pm-cli      cli_image      ok. 17 passed; 0 failed; 0 ignored
pm-cli      cli_ops        ok. 19 passed; 0 failed; 0 ignored
pm-core     lib            ok. 25 passed; 0 failed; 0 ignored
pm-fs       lib            ok. 8 passed; 0 failed; 0 ignored
pm-fs       bitmap         ok. 5 passed; 0 failed; 0 ignored
pm-fs       detect         ok. 6 passed; 0 failed; 0 ignored
pm-fs       exfat          ok. 15 passed; 0 failed; 0 ignored
pm-fs       exfat_builder  ok. 0 passed; 0 failed; 0 ignored
pm-fs       ext            ok. 9 passed; 0 failed; 0 ignored
pm-fs       ext_builder    ok. 0 passed; 0 failed; 0 ignored
pm-fs       fat            ok. 10 passed; 0 failed; 0 ignored
pm-fs       fat_corrupt    ok. 11 passed; 0 failed; 0 ignored
pm-fs       fat_undelete   ok. 2 passed; 0 failed; 0 ignored
pm-fs       ntfs           ok. 7 passed; 0 failed; 0 ignored
pm-fs       ntfs_builder   ok. 0 passed; 0 failed; 0 ignored
pm-gui      lib            ok. 127 passed; 0 failed; 0 ignored
pm-gui      bin            ok. 0 passed; 0 failed; 0 ignored
pm-image    lib            ok. 17 passed; 0 failed; 0 ignored
pm-image    formats        ok. 20 passed; 0 failed; 0 ignored
pm-image    mapfile        ok. 7 passed; 0 failed; 0 ignored
pm-image    rescue         ok. 18 passed; 0 failed; 0 ignored
pm-ops      lib            ok. 4 passed; 0 failed; 0 ignored
pm-ops      executor       ok. 31 passed; 0 failed; 0 ignored
pm-ops      executor_rescue ok. 6 passed; 0 failed; 0 ignored
pm-ops      planner        ok. 20 passed; 0 failed; 0 ignored
pm-parttable lib           ok. 1 passed; 0 failed; 0 ignored
pm-parttable convert       ok. 8 passed; 0 failed; 0 ignored
pm-parttable gpt           ok. 19 passed; 0 failed; 0 ignored
pm-parttable mbr           ok. 29 passed; 0 failed; 0 ignored
pm-parttable search        ok. 6 passed; 0 failed; 0 ignored
pm-platform lib            ok. 54 passed; 0 failed; 0 ignored
pm-platform file_platform  ok. 4 passed; 0 failed; 0 ignored
pm-platform mock           ok. 7 passed; 0 failed; 0 ignored
pm-platform windows_live   ok. 0 passed; 0 failed; 3 ignored
pm-recover  lib            ok. 3 passed; 0 failed; 0 ignored
pm-recover  carve          ok. 24 passed; 0 failed; 0 ignored
pm-recover  files          ok. 5 passed; 0 failed; 0 ignored
pm-recover  partitions     ok. 9 passed; 0 failed; 0 ignored
pm-recover  session        ok. 5 passed; 0 failed; 0 ignored
doc-tests (9 crates)       ok. 0 passed; 0 failed; 0 ignored
```

## Concerns / decisions to review

1. **"errors" means unrescued areas** (`RescueStats::bad_areas`), not failed reads: one bad
   sector is read several times (copy, trim, retries), so `read_errors` > 2 for two bad
   sectors. The log line, CLI output and GUI report all use the area count.
2. **`pm rescue` refusal is scoped to `--format`.** Without `--format`, writing into an
   existing image/raw file without a mapfile still works (existing tests and ddrescue
   semantics). A path that does not exist is created with or without `--format` (format then
   from the extension). The created destination is written in place (no `.partial`), because
   resuming relies on destination + mapfile.
3. **`pm image --rescue` has two entry paths**: `disk:N[:P]` → the operation (planner,
   system-disk guard, executor); any other source → `pm_ops::export_rescue_image` directly
   (same export code, no model). A drive-letter source is not mapped to its disk partition.
4. `RescueStats` (pm-image) gained a `mapfile` field — a small public-API addition outside the
   plan's file list, needed to show the mapfile per image in the GUI report.
5. The resume test uses a slow source wrapper (150 ms per large read) so the throttled
   progress tracker (100 ms) reports the first chunk before cancelling, and arms its read log
   on the step's first progress report so the executor's own layout reads are not counted.
6. The GUI page exposes no cluster-size field (engine default 128 KiB); `RescueSettings`
   supports it for the CLI and JSON.
7. Not verified in the running GUI app (unit tests and an offscreen egui render only).
