# Rescue-mode VM image export — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Export a disk or partition to a VM image (raw / VHD / VMDK / qcow2) even when the source has unreadable sectors: after a configurable number of failures a bad block is padded with zeros, the copy continues with the next blocks, a GNU-ddrescue mapfile records what was rescued, and an interrupted export can be resumed.

**Architecture:** The image step in `pm-ops` already copies through `pm_image::rescue::rescue` with `abort_on_error = true`. This plan adds a `RescueSettings` value to the image operations (serde, optional), makes the executor run the full ddrescue phases when it is present (copy with skip-ahead → trim → scrape → N retry passes; bad sectors zero-filled), keeps the `.partial` image plus `<image>.map` for resume, and exposes it in the CLI (`pm image --rescue …`, the `image-disk` / `image-partition` op forms, `pm rescue … --format` creating the destination) and in the GUI Create Image wizard ("Damaged disk" page) and report dialog.

**Spec:** `docs/superpowers/specs/2026-10-19-partition-manager-design.md` §2.5 (rescue copier), §2.8 (operations), §2.10 (wizards).

## Global Constraints

- Same as the main plan: bytes everywhere; no `unsafe`/OS APIs outside pm-platform; progress + cancel; the source is never written; nothing writes a disk outside a write session (image files are not disks).
- `RescueSettings` is plain serde data in `pm-ops` (no `pm_image::RescueOpts` in the public `Operation`, which must stay serializable and `PartialEq`).
- Defaults mirror the CLI `pm rescue` defaults: cluster 128 KiB, skip 64 KiB doubling, 0 retries unless set, trim and scrape on.
- A run with bad sectors is a **success with a warning** (ddrescue exit 0): `ExecReport.failed` stays `None`, the log carries `rescued X, errsize Y, errors N, mapfile <path>`, and the GUI report highlights it.

## Review Focus

1. A source that errors on every read must still terminate and produce an image of the right size (all zeros) with a mapfile of `-` blocks. Test in Task 1.
2. Resume: cancelling mid-copy keeps `.partial` + mapfile; re-queuing the same operation continues (read count of already-rescued ranges is 0) and finishes with the rename. Test in Task 1.
3. Sparse formats: zero-filled bad blocks must not allocate clusters needlessly (VHD/qcow2 unallocated reads zero anyway) — write zeros only where the engine writes; document. Test in Task 1 (`qcow2` size after a fully-bad source stays small).
4. The GUI "Damaged disk" page must not be reachable for Restore Image (read side only) and must explain that bad blocks become zeros in the image. Task 3.
5. `pm rescue src dst.vhd --format vhd` must refuse to overwrite an existing destination unless a matching mapfile exists (resume). Task 2.

---

### Task 1: pm-ops `RescueSettings` + executor rescue mode (+ mock faulty disks)

**Files:**
- Modify: `crates/pm-ops/src/operation.rs` (new `pub struct RescueSettings { pub retries: u32, pub skip_size: Option<u64>, pub cluster_size: Option<u32>, pub no_trim: bool, pub no_scrape: bool, pub reverse: bool, pub mapfile: Option<PathBuf> }` with `Default`, serde, `PartialEq`; `ImageDisk`/`ImagePartition` gain `#[serde(default)] rescue: Option<RescueSettings>`; `summary()` appends ", rescue mode (N retries)"), `crates/pm-ops/src/steps/image.rs` (when `rescue` is `Some`: `abort_on_error = false`, `mapfile = Some(settings.mapfile.clone().unwrap_or(<path>.map))`, retries/skip/trim/scrape/reverse mapped; if `<path>.partial` and the mapfile both exist, reopen the partial with `pm_image::open(.., false)` instead of creating; on `Cancelled`/error keep both files when a mapfile exists; on success rename and keep the mapfile next to the image; log the stats line), `crates/pm-ops/src/executor.rs` (precheck: mapfile directory must exist; `ExecReport.rescue_stats` already collected), `crates/pm-platform/src/mock.rs` (additive `MockPlatform::add_device(index: u32, dev: Box<dyn BlockDevice + Send>, meta: DiskInfo)` so tests can register a `FaultyDevice`), `crates/pm-ops/Cargo.toml` (dev-dep `pm-image` with `features = ["testing"]`).
- Test: `crates/pm-ops/tests/executor_rescue.rs`.

**Interfaces (Produces):** `pm_ops::RescueSettings`; `Operation::ImageDisk { disk, path, format, mode, rescue: Option<RescueSettings> }` and the same on `ImagePartition`. All existing constructors/tests updated (`rescue: None`).

- [ ] Tests first: `image_in_rescue_mode_pads_bad_sectors_and_writes_mapfile` (FaultyDevice-backed mock disk with two bad sectors → image bytes equal the source except zeros at the bad sectors, mapfile has two `-` blocks of 512 B, `ExecReport.failed == None`, log mentions "errors 2"); `image_without_rescue_still_aborts_on_error` (first error → `failed` set, no image left, no mapfile); `image_rescue_resume_continues_partial` (cancel after the first chunk via a progress sink that cancels, then re-run the same op: finished, image complete, the second run read nothing already marked `+`); `image_rescue_all_bad_source_terminates` (every read fails, retries 2 → terminates, image is all zeros, qcow2 file stays < 1 MiB); `operation_rescue_settings_serde_roundtrip_and_default`.
- [ ] Implement; `cargo test -p pm-ops -p pm-platform` green, zero warnings.
- [ ] Commit `feat(ops): rescue-mode image export with resumable mapfile`.

### Task 2: CLI

**Files:**
- Modify: `crates/pm-cli/src/cli.rs` (a reusable `RescueModeArgs { --rescue, -r/--retries, -K/--skip-size, -c/--cluster-size, --no-trim, --no-scrape, --reverse, --mapfile }` flattened into `image`, `image-disk`, `image-partition`), `crates/pm-cli/src/cmd_image.rs` (`pm image … --rescue` builds `RescueSettings` and runs through the same op path as `image-disk`; without `--rescue` unchanged), `crates/pm-cli/src/cmd_ops.rs` (the two op forms), `crates/pm-cli/src/cmd_rescue.rs` (`pm rescue <src> <dst> [--format F]`: when `dst` does not exist and is not a device, create the image with `pm_image::create` (capacity = source length rounded to 512, format from `--format` or the extension) before the copy; refuse an existing image destination unless the given mapfile exists and parses (resume)), `docs/CLI.md` regenerated, README "Rescue a failing disk into a VM image" example.
- Test: `crates/pm-cli/tests/cli_image.rs`: `cli_image_rescue_on_file_writes_mapfile_and_image` (raw source → `--rescue --format qcow2`; mapfile all `+`; image identical), `cli_rescue_creates_destination_image_with_format` (`pm rescue src.img out.vhd --format vhd mapfile` creates a VHD whose capacity equals the source and whose contents match), `cli_rescue_refuses_existing_image_without_mapfile`.
- [ ] Commit `feat(cli): --rescue for image export; pm rescue creates image destinations`.

### Task 3: GUI

**Files:**
- Modify: `crates/pm-gui/src/wizards/image_create.rs` (new page "Damaged disk": checkbox "Continue on read errors (ddrescue mode)" with the explanation "Unreadable blocks are retried, then written as zeros; a mapfile next to the image records them so the export can be resumed", retries (0–10), skip size combo, trim/scrape/reverse toggles, mapfile path (default `<image>.map`, must be absolute, parent must exist, not on the source device via the existing letter check); summary rows; `build()` sets `rescue`), `crates/pm-gui/src/dialogs/report.rs` or wherever `ExecReport` is shown (show `rescue_stats` per image: rescued / errsize / errors, in orange when errors > 0, with the mapfile path), `crates/pm-gui/src/wizards/rescue.rs` (destination "image file" branch may create the file via `pm_image::create` when it does not exist, with a format combo — reuse `fix_extension`).
- Test: op-builder test `build_image_op_with_rescue_settings`; `damaged_page_requires_absolute_mapfile_off_source`; report-dialog formatting test for the stats line.
- [ ] Commit `feat(gui): damaged-disk mode in Create Image; rescue stats in the report`.
