# Task 13 report: pm-ops model, operations, planner and clone sizing

**Status:** DONE. Commit `bdf5105` on `dev`, titled "feat(ops): model, operations, planner". It touches only `crates/pm-ops/**` and `Cargo.lock`.

## What was implemented
- **`model.rs`**
  - `Model { disks, volumes, system_disk }` derives Clone, Debug, Default and serde.
  - Methods: `from_platform` (enumerate_disks, enumerate_volumes, system_disk), `disk`, `disk_mut`, `partition`, `partition_mut`, `is_system_disk`, `is_windows_partition` and `letter_in_use`.
  - `disk`/`disk_mut` look a disk up by `DiskInfo.index`, not by its position in the vector.
  - `is_windows_partition` is true for a partition on the system disk that has `flags.required`, `flags.boot` or a `C:` letter. EFI, MSR and recovery partitions are never the Windows partition.
  - `table_from_disk(&DiskInfo) -> Table`:
    - Keeps the disk GUID or MBR signature when it can be parsed.
    - Maps flags to attributes: GPT bits 0/60/62/63, MBR bit 0 = active.
    - A partition with no type becomes basic data on GPT or 0x07 on MBR.
    - For legacy layouts, GPT first-usable is lowered to the earliest partition start, but never below 34 sectors.
- **`operation.rs`**
  - `CloneMode`, `Sizing`, `WipeMethod`, `BootFix` and the `Operation` enum, with exactly the variants and fields in the brief. All have serde.
  - `MountPoint` has no serde derives in pm-platform, so the `at` fields use `#[serde(with = "MountPointDef")]` (a serde "remote" mirror type). pm-platform is unchanged.
  - `summary()` matches every example in the brief.
  - `is_destructive()` and `touches_disk()` are implemented.
  - Additive APIs beyond the brief:
    - `is_destructive_in(&Model)`: for ResizeMove, only a move or a shrink counts as destructive.
    - `target_disk()`: the disk the operation writes to.
- **`planner.rs`**: `Planner::simulate` and `simulate_all` (a `try_fold`). Every rule in the brief is implemented:
  - **System-disk guard:** "Disk N is the running system disk" for Initialize, WipeDisk, ConvertTable, and as the destination of CloneDisk, MigrateOs and RestoreImage. The guard runs before the image is opened.
  - **Windows partition guard:** delete, format, wipe, move, change letter, change type, align-move and unmount of `C:` are refused.
  - **CreatePartition:**
    - The start is rounded up and the length rounded down to 1 MiB, so the range stays inside the request; anything under 1 MiB is refused.
    - The range must be inside `free_ranges(MIB)`; the type must match the table family; GPT allows ≤128 partitions and MBR ≤4 primaries.
    - The drive letter must be unique across partitions and volumes.
    - Numbering: GPT gets max+1; MBR gets the lowest free primary slot 1..4.
    - A new GUID is assigned on GPT; `used` is `Some(0)` when a filesystem is given.
  - **Layout check:** after every layout change the result is re-checked with `table_from_disk(d).validate()` (overlap, bounds, sector alignment, MBR 32-bit limits), and the error is prefixed with the disk name.
  - **Other operations:**
    - Delete removes the partition and its volume, which frees its letter.
    - Format checks cluster size and label length.
    - ResizeMove checks used size, the 1 MiB minimum and the disk end.
    - SetActive is MBR only and clears the flag on the others.
    - SetLabel, SetLetter, SetType, Mount and Unmount check against `model.volumes`.
    - WipeDisk sets the table to None and removes the disk's volumes; WipePartition sets fs None and used 0.
    - ConvertTable uses `mbr_to_gpt`/`gpt_to_mbr` and keeps fs, label and mount points (matched by start). Align uses `align_table(MIB)`.
    - RebuildMbr changes nothing.
    - ClonePartition reports "Target smaller than used data" (Smart) or a length error (Sector).
    - CloneDisk and MigrateOs use `plan_disk_clone` (Sector mode counts the full length as used); fs, label and used carry over through `map`, and the destination's volumes are dropped.
    - Image* only checks that the parent folder exists.
    - RestoreImage opens the image with `pm_image::open(path, true)` to read its capacity:
      - Whole disk: the table becomes Unknown and partitions are cleared.
      - `dst_start` equal to an existing partition's start: the image must fit inside it.
      - Otherwise: a new partition is created in free space.
- **`sizing.rs`**: `TargetLayout`, `plan_disk_clone`, and `windows_partition`, which is exported for the executor.
  - **Windows partition:** flagged required/boot, otherwise the one at `C:\`, otherwise the largest NTFS data partition.
  - **Exact:** same starts and lengths.
  - **Proportional:** EFI, MSR, recovery and other non-data partitions keep their length. Data partitions are scaled to fill the remaining space and rounded down to 1 MiB. Each gets at least its minimum; any overshoot is taken back from the partitions with the most room above their minimum.
  - **Fit:** each data partition gets `max(min, min(src_len, remaining slack))`. Any leftover goes to the largest data partition.
  - **Layout:** partitions are placed in source order from first-usable, 1 MiB aligned, and numbered 1..n.
  - **MBR logicals:** the extended container is rebuilt, with a 1 MiB slot for each EBR, and logicals are numbered from 5.
  - **IDs and checks:** IDs are regenerated, MBR bit 0 comes from `flags.active`, and the result is checked with `validate()`. A target that is too small gives "Target smaller than used data: partition N of Disk M needs at least …".

## Deviations / interpretations (please check)
1. **Minimum size for data partitions:** it is `align_up(max(used, min(used×1.1, src_len)), 1 MiB)`, at least 1 MiB. The 10% margin is capped at the source length, so a Sector clone (used = len) or a nearly full partition never needs more than its source size. With the test data (25 GiB used of 60 GiB) this is exactly 27.5 GiB.
2. **ResizeMove summary:** it shows `start – end`, read from the example "65.00 MB – 2.73 TB".
3. **`is_destructive()` for ResizeMove:** returns true without a model. `is_destructive_in(&Model)` gives the precise answer (the brief's signature takes no model).
4. **`touches_disk` for Mount/Unmount:** returns false, because they address a volume, not a disk.
5. **InitializeDisk:** refused when the disk still has partitions, and refused for MBR when the disk has more than 2^32 sectors.
6. **CreatePartition summary with a letter:** a create that sets a letter appends ` (E:)`. The brief's example has no letter and matches exactly.
7. **RestoreImage with `dst_start`:** handled as above (fit inside the existing partition at that start, or create a new partition in free space). The brief only specified the whole-disk case.

## Tests (crates/pm-ops/tests/planner.rs)
The 9 tests named in the brief: `simulate_create_in_free_space_assigns_number`, `simulate_create_overlap_rejected`, `simulate_delete_then_create_reuses_space`, `plan_clone_rejects_small_target`, `planner_guards_system_disk`, `plan_disk_clone_proportional_keeps_efi_msr`, `plan_disk_clone_fit_uses_used_sizes`, `simulate_all_chains_models`, `operation_summary_text`.

Two extra tests:
- `table_from_disk_round_trips`
- `model_from_platform_and_serde` (MockPlatform)

They use the fixture described in the task:
- Disk 0: GPT, 100 GiB, with EFI, MSR, Windows (C:, required, 25 GiB used) and WinRE.
- Empty disks of 50, 200 and 20 GiB.
- `system_disk = Some(0)`.

Restore tests use a real 4 MiB raw image made with `pm_image::create` in a tempdir.

- **RED:** `cargo test -p pm-ops` with only lib.rs and the tests failed to compile with `error[E0583]: file not found for module model/operation/planner/sizing`.
- **GREEN:** `test result: ok. 11 passed; 0 failed`.
- **Mutation check:** turning off `guard_system_disk` made `planner_guards_system_disk ... FAILED`; the code was then restored.
- **Builds:** `cargo build -p pm-ops` and `cargo test -p pm-ops` produce 0 warnings; `cargo doc -p pm-ops --no-deps` also produces 0 warnings.
- **Not run:** clippy and rustfmt are not installed for this toolchain. Lines were wrapped to about 100 columns by hand; a few string literals are longer.

## Files
- Created: `crates/pm-ops/Cargo.toml`, `src/{lib,model,operation,planner,sizing}.rs`, `tests/planner.rs`
- Modified: `Cargo.lock` (new pm-ops package entry only)

## Self-review
- **Error messages:** every `Error::Invalid` names the disk and partition (or the volume, for Mount/Unmount). Errors from validate, convert and `pm_image` are wrapped with "Disk N: …" or "Cannot convert/clone Disk N …".
- **No panics on user data:** no `unwrap`/`expect` on user data in `src/`. `new_number` returns Invalid when no slot is free.
- **Arithmetic:** size math uses `checked_add` for range ends, `saturating_*` for sums and differences, and u128 for proportional scaling. Every plain subtraction is guarded by a comparison just before it.
- **No unsafe code and no OS APIs.** The planner reads the file system only through `pm_image::open` and `Path::is_dir`, both called for in the brief.

## Concerns
- **pm-image dependency:** pm-ops depends on pm-image, and through it on pm-fs. Concurrent uncommitted edits in pm-fs could break pm-ops builds for other agents for a while.
- **Windows-partition detection:** it relies on `flags.required`/`flags.boot`/`C:`. Task 14's executor and the platform's enumeration should agree on which flag marks the Windows partition.
- **Partition order:** `DiskInfo.partitions` is kept sorted by start after create, clone and resize.

## Fix round 1 (commit `8e1cc2e`)

| # | Fix | Where |
|---|-----|-------|
| 1 | `Table::validate` now has the same EBR rule as `write_mbr`. The first logical must start at least 1 MiB after the extended container's start. Every later logical must start at least 1 MiB after the end of the logical before it. The rule names the logical and says "room for its EBR". With this, the planner's `validate_layout` and the writer agree. | `pm-parttable/src/table.rs` |
| 1b | Errors from an Exact clone now add a hint: "use Fit or Proportional sizing, which lay the partitions out on a 1 MB grid with room for MBR EBRs". This covers both `place_exact` errors and `validate` errors. | `sizing.rs` (`with_exact_hint`) |
| 2 | ResizeMove refuses to shrink when `used == None` and `fs.is_some()`. The message says "used space is unknown; mount or check the volume first". A raw partition (`fs` None) may still shrink. | `planner.rs` `resize_move` |
| 3 | New `Model::effective_system_disk()`. It returns `system_disk` if set; otherwise the disk with a partition at `C:\`; otherwise the disk of a volume mounted at `C:\`. `is_system_disk` uses it. When it returns None, `guard_system_disk` refuses on **every** disk with "Disk N: cannot determine the running system disk; refusing destructive operation". | `model.rs`, `planner.rs` |
| 4 | `AlignPartitions` is now destructive. | `operation.rs` |
| 5 | When a partition moves (`new_start != start`): the new start must be MiB-aligned and ≥ 1 MiB. The end must be MiB-aligned or equal the end of the usable area. An in-place resize keeps its legacy start. RestoreImage into free space: the start is aligned up to 1 MiB, and an image smaller than 1 MiB is Invalid. | `planner.rs` |
| a | Windows detection is now one function, `model::windows_partition`, also re-exported as `pm_ops::windows_partition` and `sizing::windows_partition`. Order: mounted at `C:\`, then `flags.boot`, then `flags.required`, then the largest NTFS data partition. `Model::is_windows_partition` is now `is_system_disk(disk) && windows_partition(disk) == Some(p.number)`. | `model.rs` |
| b | The Unmount message had a run of literal spaces in it; this is fixed. Three new messages had the same problem during this round and are fixed too. `invalid_msg` in the tests now asserts that no message contains a double space, so every tested message is checked. | `planner.rs`, `sizing.rs`, tests |
| c | The `table_from_disk` doc comment now says it is **model-only** and lossy: the boot code is zeroed, only 4 GPT attribute bits survive, and the bounds are rebuilt rather than read. It must never be written to a disk. | `model.rs` |

### New and updated tests

**pm-ops `tests/planner.rs`**
- `mbr_logicals_need_ebr_room` uses a new MBR fixture, Disk 4: one primary, an extended container and 2 logicals, laid out with either 1 MiB or legacy 63-sector gaps. It checks:
  - moving a logical into the EBR gap is Invalid;
  - growing a logical in place over the next logical's EBR slot is Invalid;
  - an aligned move that keeps the gap is OK;
  - an Exact clone of the legacy layout is Invalid with "Fit or Proportional" in the message, both through `plan_disk_clone` and through the planner;
  - Proportional and Fit rebuild the container with 1 MiB EBR slots, logicals numbered 5 and 6.
- `resize_refuses_shrink_with_unknown_used`: shrink is refused, growing is OK, and shrinking a raw partition is OK.
- `system_disk_fallbacks` covers both fallbacks (a partition at `C:\`, then a volume at `C:\`) and the unknown case. When unknown, wipe, initialize, convert, clone, migrate and restore are all refused, while create still works.
- `move_and_restore_are_mib_aligned` checks:
  - an unaligned start, or an unaligned end, on a move is refused;
  - a move ending at the unaligned GPT usable end is OK;
  - an in-place resize with a legacy start is OK;
  - a restore start of 1 MiB + 4 KiB is aligned to 2 MiB;
  - a 512 KiB image is Invalid.
- `windows_partition_detection_order` adds an OEM NTFS partition with the required bit, placed before Windows in the partition vector, so each rule (C:, boot, required, largest NTFS) can be told apart. The guards protect the right partition and the OEM partition can be deleted. Sizing with `only_system` drops the OEM partition.
- `operation_summary_text` now expects `AlignPartitions.is_destructive()` to be true.

**pm-parttable `tests/mbr.rs`**
- New test `validate_requires_ebr_room`: exact 1 MiB gaps pass and can be written; a legacy gap fails and names "logical partition 5" and "EBR"; a second logical that is too close fails, with the entries in unsorted order.
- `write_mbr_rejects_logical_too_close_to_extended_start` used to assert `validate().unwrap()` on the legacy layout. It now expects `validate` to fail on it.

### RED and GREEN

**RED**
- pm-parttable, with HEAD's `table.rs` and the new tests: `validate_requires_ebr_room ... FAILED` and `write_mbr_rejects_logical_too_close_to_extended_start ... FAILED`, giving `test result: FAILED. 27 passed; 2 failed`.
- pm-ops, with HEAD's `src/` and the new tests (the `effective_system_disk` asserts stubbed out so it compiles), giving `test result: FAILED. 10 passed; 6 failed`. The failing tests:
  - `operation_summary_text`
  - `windows_partition_detection_order`
  - `system_disk_fallbacks`
  - `move_and_restore_are_mib_aligned`
  - `resize_refuses_shrink_with_unknown_used`
  - `mbr_logicals_need_ebr_room`

**GREEN**

`cargo test -p pm-ops` (unit tests, `tests/planner.rs`, doc-tests in that order):
```
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

`cargo test -p pm-parttable` (unit tests, then convert, gpt, mbr, search, doc-tests):
```
test result: ok. 1 passed; 0 failed; ...
test result: ok. 8 passed; 0 failed; ...
test result: ok. 19 passed; 0 failed; ...
test result: ok. 29 passed; 0 failed; ...
test result: ok. 6 passed; 0 failed; ...
test result: ok. 0 passed; 0 failed; ...
```

pm-platform and pm-cli also pass with the stricter `validate` (`cargo test -p pm-parttable -p pm-platform -p pm-cli`, every result is ok). `cargo build -p pm-ops` and `cargo doc -p pm-ops --no-deps` give 0 warnings.

### Notes
- **Planner refuses all layout changes on legacy MBR disks.** If a disk already has legacy logicals (63-sector EBR gaps), every layout-changing operation on it (create, resize, clone into it) now fails `validate_layout`. This is consistent with the writer, which already could not rewrite such a table, but it means the planner will not edit legacy extended layouts at all.
- **Windows partition when no flag or C: is present.** `windows_partition` falls back to the largest NTFS partition. On the system disk with no `C:`, boot or required marker, that partition is therefore guarded. This is the conservative choice.
