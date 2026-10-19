# Task 14 report: pm-ops queue and executor

**Status:** DONE. Commit `81444f1 feat(ops): queue and executor` on `dev`.

## What was implemented

- `src/queue.rs`: `OpQueue { base, ops, preview }` with `new`, `push` (runs `Planner::simulate` on the preview; a rejected op leaves the queue unchanged), `undo` (pops, then recomputes the preview from the base; if an earlier op no longer validates, the queue is cut back to the valid prefix so the preview always matches `ops`), `discard`, `preview`, `ops`, `len`, `is_empty` (added for clippy), `base`, `reset(base)` (new base, pending ops dropped).
- `src/executor.rs`: `StepEvent`, `ExecOpts { dry_run, allow_system_disk }` (Clone/Copy/Debug/Default), `ExecReport` (Clone/Debug/Default/Serialize/Deserialize), `execute`. For each op it:
  1. checks cancel;
  2. re-reads the live state (`Model::from_platform`);
  3. re-checks the system disk on the live model;
  4. re-runs `Planner::simulate` on the live model;
  5. runs the steps;
  6. rescans.

  It stops at the first failure with `failed = Some((i, msg))` and `completed` = the number of ops done. Op failures go into the report and are never returned as `Err`.
- **Dry run:** validates the whole chain against the live state (each op on the model the previous ones produce, so "create then format it" works). It logs `[i/n] would: <summary>` and writes nothing: no session, no rescan, no platform calls. In a dry run, `completed` counts the ops that validated.
- **System-disk rule, re-checked at execution:** an op whose written disk is the effective system disk (or any disk when the system disk can't be determined) is refused. "Written disk" means `target_disk()`, or the volume's disk for Mount/Unmount. `allow_system_disk` lets only this relaxed family through: ResizeMove, SetLabel, SetLetter, Mount, Unmount, **CreatePartition**. Initialize, Wipe, Clone, Restore, Format, Delete, SetType, SetActive, Convert, Align and RebuildMbr are never relaxed.
- **Progress:** each step logs one line `[i/n] <summary>: <step>` and gets a `StepSink`. The sink immediately sends a "started" report, then forwards every engine report with `phase = "[i/n] <summary>: <step>"` and `detail = "<engine phase>: <engine detail>"`.
- `src/steps/mod.rs`:
  - `Ctx` holds the live model and the simulated model. `new_partition()` takes the planner's number, aligned start/len, type and GUID by diffing the two.
  - `Session` guard with `with_session`: the session is always finished. On success and error paths `finish` is explicit and its error is reported (the op's own error wins). On a panic `Drop` finishes it.
  - `locate_volume`: polls `enumerate_volumes` every 250 ms for up to 5 s, and honours cancel.
  - `fs_facts`: the used map and fs size via `pm_fs::open`; `None` on any error.
  - `move_data`: a same-device copy that is overlap-safe (backwards when moving right).
- **`steps/table_ops.rs`:**
  - Initialize uses `Table::empty` + commit.
  - Create, Delete, SetActive and SetType read the device table inside the session, edit it with the planner's numbers and commit.
  - Create then formats (after finish and rescan, locating the volume) and assigns the letter.
  - ConvertTable uses `mbr_to_gpt` / `gpt_to_mbr` on the device table. For GPT→MBR, the primary and backup GPT are zeroed **after** the MBR is committed (LBA 0 is kept), so the disk is never left without a table.
  - Align: NTFS/ReFS partitions that shrink are shrunk via `shrink_volume` first; other filesystems that would shrink get `Unsupported`. Inside the session, every entry is checked against the device first, then each moved partition's data is copied, then the table is committed.
  - RebuildMbr: see concerns.
- **`steps/format.rs`:**
  - FormatPartition calls `platform.format` on the partition's volume (no session needed).
  - SetLetter unmounts the existing letters, then mounts the new one.
  - Mount and Unmount pass straight through to the platform.
- **`steps/resize.rs`:**
  - NTFS/ReFS length change: shrink calls `shrink_volume` first, then the table; grow writes the table first, then `extend_volume`.
  - A partition without a filesystem changes in the table only.
  - Other filesystems get `Unsupported("resize of <fs> is not supported …")`.
  - Moves copy the data inside the session, then write the table. A move that keeps the length is allowed for any filesystem.
- **`steps/wipe.rs`:**
  - Methods: Zero; Random (xorshift64 seeded from a v4 UUID); Dod3 (0x00, 0xFF, random).
  - Writes 4 MiB chunks with a `ProgressTracker` and checks cancel per chunk.
  - WipeDisk then zeroes LBA 0..34 and the last 33 sectors. `commit_table` cannot express "no table" because `write_table` refuses kind None, so the layout is re-read with `rescan` instead.
- **`steps/clone.rs`:**
  - `copy_partition`: Smart uses the `pm_fs` used map and falls back to a full copy if it can't be read. Sector copies everything. It uses `copy_simple` from a read-only `SliceDevice` into `session.device()`.
  - A target smaller than its source is accepted only if the source filesystem's size and used data fit it. Otherwise `Unsupported`, because there is no filesystem shrink during a copy.
  - After the clone, NTFS/ReFS copies with a larger target are grown with `extend_volume`. This is non-fatal: a failure is only logged.
  - ClonePartition checks the target range is still free on the device.
- **`steps/migrate.rs`** (CloneDisk / MigrateOs):
  1. Calls `plan_disk_clone` (the same call the planner makes, Smart for migrate) and applies `regenerate_ids`. The source's MBR boot code is kept.
  2. In one session on the destination: commit an empty table of the source kind, copy each mapped partition, commit the final table.
  3. Rescan, then grow NTFS copies.
  4. Boot fix: temporary letters from `free_letters()` for W (the cloned Windows partition) and S (EFI for UEFI; the active partition or W for BIOS; EFI, else active, else W for ALL). Then `bcdboot W:\Windows /s S: /f UEFI|BIOS|ALL`, plus `bootsect /nt60 W: /mbr` for BIOS and ALL.
  5. The temporary letters are always unmounted. A boot-fix failure adds the log line `boot fix failed: …` and does not fail the op.
- **`steps/image.rs`:**
  - ImageDisk / ImagePartition use `pm_image::create` with capacity rounded up to 512 and `sparse: true`, writing to `<path>.partial` and renaming on success. A flat VMDK's extent is handled like the CLI's `Partial`. On failure or cancel the partial files are deleted.
  - Reads go through `pm_image::rescue` with `abort_on_error: true` (plain clone semantics) so `ExecReport.rescue_stats` gets one entry per image.
  - Smart partition images use the fs used map. Smart disk images use a disk-wide 64 KiB-unit map: everything outside partitions plus each partition's used map (all of it when unreadable).
  - RestoreImage opens the image read-only. A whole-disk restore copies to offset 0. A restore to a partition copies into the partition at `dst_start`; a restore into free space adds the planner's new partition entry after copying (after checking the range is still free). Then rescan.
- `src/lib.rs`: new modules and re-exports. I also rewrote one doc sentence that had become false ("Nothing in this crate touches a device").
- `Cargo.toml`: added `pm-fs` as a dependency (for used maps).
- `crates/pm-platform/src/mock.rs` (additive only): `MockSession::finish` records into a shared log, exposed as `MockPlatform::finished_sessions_log()`. `calls()` is unchanged.

## Tests (`crates/pm-ops/tests/executor.rs`, 15 tests on MockPlatform)

These are the brief's 6 test names plus 9 extra tests.

- `execute_create_partition_writes_table_in_session`:
  - exactly one session, on disk 1, and it is finished;
  - the entry is at 1 MiB / 32 MiB with the right type and name;
  - the `format vol-new NTFS label=Data quick=true` call comes after `rescan 1`;
  - letter F is mounted.
- `execute_order_and_stop_on_first_failure`: format is failure-injected.
  - `completed == 1`, `failed == (1, "…mock failure…")`;
  - op 2's table write happened, op 3 never ran (no `[3/3]` in the log);
  - all sessions are finished.
- `execute_dry_run_touches_nothing`: Create, Format, Delete, WipeDisk in a dry run.
  - `disk_bytes` is identical before and after;
  - no sessions, no calls, no mounts;
  - 4 `would:` lines;
  - an invalid op still fails validation.
- `execute_clone_disk_copies_used_clusters_only`: a real FAT32 (`format_fat32` plus a minimal directory-entry writer, two files) on a 0x55 disk is cloned to a raw 0xAA disk.
  - Smart: every used range is identical, a mid-volume unused cluster is still 0xAA, `pm_fs` lists the same files with the same contents, and the disk and partition GUIDs are fresh.
  - Sector: the whole partition is identical.
- `execute_migrate_regenerates_disk_guid_and_calls_bcdboot`: GPT source with EFI, MSR, Windows (FAT32, `required` attribute, `C:\` volume) and WinRE.
  - The disk GUID and all partition GUIDs differ from the source.
  - The attributes are kept and the files are identical.
  - `run_tool BcdBoot ["E:\\Windows", "/s", "F:", "/f", "UEFI"]` is called, and no BootSect for UEFI.
  - The temporary letters are unmounted afterwards.
  - Second case: bcdboot is failure-injected. The op still succeeds, the log has `boot fix failed`, and the letters are unmounted.
- `queue_push_undo_discard_preview`: push, push rejected, undo, discard, reset.
- Extra tests:
  - `execute_refuses_system_disk_at_execution_time`: the queue is valid on a stale model, then the platform reports disk 1 as the system disk. The executor refuses with no session and no byte changed. With `allow_system_disk` the wipe is still refused and the create is allowed.
  - `execute_wipe_partition_and_disk`
  - `execute_wipe_cancel_still_finishes_session`
  - `execute_resize_ntfs_shrinks_volume_first_and_grows_table_first`: failure injection proves the ordering; FAT32 resize gets `Unsupported`.
  - `execute_move_copies_data_overlap_safe`: right overlap, left overlap, and a disjoint move.
  - `execute_align_moves_data_before_table`: legacy start at sector 63.
  - `execute_image_and_restore_partition`: the partial file is renamed, `rescue_stats` is populated, and the restored filesystem is identical.
  - `execute_mount_unmount_and_letters`: includes SetLabel → `unsupported`.
  - `execute_table_ops_initialize_convert_active_type_delete`: includes GPT↔MBR conversion with the GPT areas cleared, and RebuildMbr.

**RED:** `cargo test -p pm-ops --test executor` initially failed with `error[E0432]: unresolved imports pm_ops::execute, pm_ops::ExecOpts, pm_ops::ExecReport, pm_ops::OpQueue`.

**GREEN:** `cargo test -p pm-ops` gives 15 passed (executor) + 16 passed (planner), 0 warnings. `cargo build --workspace` gives 0 warnings. `cargo test -p pm-platform` is green.

**Mutation checks (temporary, reverted):**
- Making Smart ignore the used map fails the clone test.
- Bypassing the system-disk guard fails the refusal test.
- Skipping the table-area zeroing fails the wipe test.
- `mem::forget`-ing the session instead of finishing it fails 11 of the 14 executor tests (14 was the count at the time; the table-ops test came later).

## Self-review

- **Every session is finished on every path.** `with_session` always calls `finish`, and `Session::drop` finishes on a panic or early drop. The tests assert `write_sessions_log() == finished_sessions_log()`, including the failure and cancel paths.
- **Nothing writes outside a session.** `write_at` and `copy_simple`/`move_data` destinations are always `session.device()`. Source disks are opened `OpenMode::ReadOnly` only. Image files are the only other write target. Format, shrink, extend and mount go through the platform.
- **Dry run writes nothing.** Verified by comparing `disk_bytes` and checking that the session and call logs are empty.
- **The system disk is re-checked at execution** from the live `Model::from_platform` (which prefers `platform.system_disk()`, else the C: fallback). The planner is also re-run on the live model.

## Concerns / stubs

1. **SetLabel** returns `Error::Unsupported("set label requires platform support (planned)")`. `Platform` has no label API, and the trait was frozen for this task.
2. **`allow_system_disk` relaxed family includes CreatePartition** (creating in free space is non-destructive and is the natural follow-up to shrinking C:). If you want the strict reading (resize/letter/label/mount only), drop it from `executor::relaxed`.
3. **RebuildMbr** rewrites the table read from the disk, keeping its boot code as read. Empty boot code stays empty and a log note is added, because no Windows MBR boot code is embedded. It needs a parseable table.
4. **Clone/migrate into a smaller partition** is refused unless the source filesystem already fits, because shrinking a filesystem during a copy is not supported. Fit/Proportional migrations from a big HDD to a small SSD therefore fail at execution when the source NTFS is larger than its planned target. The planner (Task 13) accepts these; a source shrink step, or an NTFS resize, is needed to support them.
5. **Live-volume consistency:** migrating, cloning or imaging from the running system disk copies a mounted, changing volume (no VSS snapshot). The result may be inconsistent.
6. **Moving partitions:**
   - `move_data` is not cancellable once copying starts (only before); a half-moved partition is unrecoverable.
   - Moving a bootable NTFS partition does not update the boot sector's hidden-sectors field.
   - The brief said `copy_simple`, but it cannot alias source and destination on one device, so a same-device chunked copy was written instead.
7. **Volume lookup** after create, format or clone relies on the OS exposing a volume at the partition start within 5 s. On MockPlatform the tests pre-register those volumes. A missing volume costs a 5 s wait before the error.
8. **Image ops use `pm_image::rescue`** (clone mode, aborts on the first read error) rather than `copy_simple`, so that `rescue_stats` is meaningful. The brief's test note said "unused clusters zero on target"; per the dispatch context the target is pre-filled with 0xAA, and the test asserts unused clusters stay 0xAA.
9. **MockPlatform volumes are static**, so create/clone tests pre-add the volume the OS would expose.

## Files

- `crates/pm-ops/Cargo.toml`, `crates/pm-ops/src/lib.rs`
- `crates/pm-ops/src/executor.rs`, `crates/pm-ops/src/queue.rs`
- `crates/pm-ops/src/steps/{mod,table_ops,format,resize,wipe,clone,migrate,image}.rs`
- `crates/pm-ops/tests/executor.rs`
- `crates/pm-platform/src/mock.rs` (additive `finished_sessions_log`)

## Fix round 1

**Critical 1: Align is now consistent on cancel or error.** `steps/table_ops.rs`:
- Every decision is taken before anything is written (`align_changes`, shared with `precheck`).
- NTFS/ReFS filesystems that shrink are shrunk next.
- Then, inside the session, each partition gets "move data → update its entry → `commit_table`", so the table on disk always matches the data. Each new range lies inside the old one, so every intermediate table is valid.
- Cancel is checked between partitions only; a single move is not cancellable once started (documented).

**2: Source shrink for clone/migrate into a smaller target.** `steps/clone.rs::prepare_copies` is a pre-flight run before the destination session:
1. **Decide (read-only).** For each source larger than its target whose filesystem size (from `pm_fs`, else the partition length) exceeds the target:
   - an NTFS/ReFS source with a volume id is shrunk;
   - anything else is refused: `partition N (fs) on Disk d must be shrunk first; only NTFS/ReFS volumes can be shrunk online`.
2. **Log.** The line `source volume(s) will be shrunk: …` is added to the op's log.
3. **Shrink.** `shrink_volume(src_vol, planned_len)` runs as the step `shrinking source volume X: from A to B`. This is allowed on the system disk.
4. **Re-read used maps.** They are read only after the shrinks, because shrinking moves clusters.

`Operation::shrinks_source(&Model) -> Vec<String>` (an inherent impl in `executor.rs`) lists the planned shrinks for the confirmation dialog. It uses the planner's own numbers: the `plan_disk_clone` layout for disk clones, the simulated new partition for a partition clone.

**3: Refusals write nothing.** All fit/shrink/refuse decisions moved into that pre-flight. `copy_partition` now just copies a prepared `CopyPlan`.
- `clone_disk` runs the pre-flight before the destination session, so before the table is cleared.
- `clone_partition` does the same.

**4: `pub fn precheck(model, op)` in `executor.rs`, re-exported.** It refuses:
- SetLabel;
- a length change on a filesystem other than NTFS/ReFS (shared `resize::check_resize`);
- an Align that would shrink such a partition;
- a clone/migrate whose smaller target has no shrinkable (NTFS/ReFS with volume) source. This is conservative: the model does not know filesystem sizes.

It is called from `execute` (real run and dry run, after the system-disk guard and before `Planner::simulate`) and from `OpQueue::push` (before simulate).

**5: Stale GPT cleared under a new MBR.**
- `initialize` (MBR) calls `zero_table_areas(dev, true)` after the commit.
- `clone_disk` with an MBR layout does the same right after committing the empty MBR, before the copies (a copy may legitimately cover the last sectors).
- `convert` already did this.

**6: Move+grow re-locates the volume.** After the rescan, `resize_move` looks the volume up with `locate_volume(disk, new_start)` before `extend_volume`. A shrink still uses the pre-move volume.

**New tests** in `tests/executor.rs`:
- `execute_align_cancel_keeps_table_consistent`: two legacy partitions; a cancel fired from the first move's progress leaves the table at (1 MiB, 7 MiB) + the untouched partition 2, matching the data; a second run completes the alignment.
- `execute_clone_to_smaller_disk_shrinks_ntfs_source_first`:
  - `shrinks_source` returns 1 line;
  - `calls()` contains `shrink src-ntfs <target len>`;
  - the log has the "will be shrunk" and "shrinking source volume" lines;
  - the data is copied;
  - with shrink failure-injected: no session on the target and its bytes are unchanged.
- `execute_clone_refusal_writes_nothing`: a FAT32 source too large for the target is refused with "must be shrunk first", with no session, unchanged target bytes and no calls; `queue.push` also refuses it.
- `precheck_rejects_what_the_executor_refuses`:
  - SetLabel, a FAT32 resize and a FAT32 align are refused by `precheck` and by `OpQueue::push`;
  - Delete still passes;
  - a dry run of the FAT32 resize sets `failed`.
- `execute_mbr_over_gpt_clears_stale_gpt`: InitializeDisk MBR on a GPT disk, and CloneDisk of an MBR source onto a former GPT disk; LBA1 and the last sector are zero afterwards.
- `execute_move_and_grow_extends_the_relocated_volume`: the mock exposes a different volume id at the new offset; `extend v1-moved …` is called.

**RED.** First run, with `precheck`/`shrinks_source` stubbed: `test result: FAILED. 15 passed; 6 failed`. All six new tests failed at their key assertions.

**GREEN.** `cargo test -p pm-ops`:
```
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.88s
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
`cargo build --workspace`: 0 warnings.

**Notes:**
- `precheck` can refuse a non-NTFS clone that would actually fit, when the filesystem is smaller than its partition, because the model has no filesystem sizes. The executor's own pre-flight is exact.
- Concern 4 of the original report (shrinking clones) is resolved for NTFS/ReFS sources.

## Fix round 2

**(a) Message spacing.** Two messages had lost a `\` line continuation, leaving a run of spaces in the middle of the text. Both are fixed:
- `Operation::shrinks_source` ("…will be shrunk from {} to {}");
- `table_ops::check_shrink` ("…aligning it would shrink it by {} bytes…").

The executor tests now have a `spaced()` helper, the same check as the planner tests' `invalid_msg`: it fails on any run of two spaces. It is applied to:
- the `shrinks_source` line;
- the align / resize / SetLabel `precheck` errors;
- the clone refusal;
- the new ReFS messages.

Mutation check: re-inserting the extra spaces into the `shrinks_source` string makes `execute_clone_to_smaller_disk_shrinks_ntfs_source_first` fail with "broken message spacing". The original was restored and confirmed byte-identical. A grep finds no other run of spaces in any pm-ops string literal.

**(b) Only NTFS can be shrunk online.** Windows' FSCTL_SHRINK_VOLUME does not support ReFS.
- `clone::shrinkable` is now NTFS with a volume. ReFS sources larger than their target are refused, by `precheck`/`OpQueue::push` and by the pre-flight, with "partition N (ReFS) on Disk d must be shrunk first; only NTFS volumes can be shrunk online".
- `shrinks_source` lists NTFS sources only.
- `resize::check_resize`: NTFS can shrink and grow; ReFS can only grow (via `extend_volume`). A ReFS shrink is refused with "shrinking ReFS is not supported (…); only NTFS volumes can be shrunk online".
- `check_shrink` (used by align): only NTFS shrinks via the platform. ReFS and other filesystems are refused, with "only NTFS volumes can be shrunk online" appended.
- The docs are updated to match.

**Unreachable SetLabel arm.** The `SetLabel => precheck(..)` arm in `run_op` is gone. The match on `Operation` must stay exhaustive, so the arm now returns the same `set_label_unsupported()` error that `precheck` uses, with a comment noting that `precheck` refuses SetLabel before any step runs.

**Tests:**
- New: `refs_can_grow_but_cannot_be_shrunk_online`.
  - A ReFS shrink is refused by `precheck` and by `execute`, with no calls and no session.
  - A ReFS grow passes `precheck` and calls `extend r1 <len>`.
  - A ReFS clone source larger than its target: `shrinks_source` is empty; the run is refused with "(ReFS) … only NTFS volumes can be shrunk online"; no calls, no session, target bytes unchanged.
- Updated:
  - the refusal and align tests assert the "only NTFS volumes can be shrunk online" text;
  - the `shrinks_source` test asserts "will be shrunk from".

**GREEN.** `cargo test -p pm-ops`:
```
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.12s
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```
`cargo build --workspace`: 0 warnings.
