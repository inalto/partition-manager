# Task 1 + Task 2 report (pm-core scaffold, device layer, domain model)

Status: DONE_WITH_CONCERNS (minor deviations listed below; all tests green, zero warnings)

## Commits (branch dev)
- 96e107c feat(core): workspace scaffold and block device layer
- 66fe529 feat(core): domain model

## What was implemented
Task 1: workspace `Cargo.toml` (members `crates/*`, all requested `[workspace.dependencies]`, release profile), `README.md`, `crates/pm-core` with `error.rs`, `device.rs` (BlockDevice + Box/&mut forwarding, MemDevice, FileDevice with seek_read/seek_write on Windows and read_at/write_at on unix, `with_len`, `with_sector_size`, SliceDevice, ReadOnly, copy_range), `progress.rs` (ProgressReport/Sink/NullProgress/FnProgress/CancelToken/ProgressTracker), `units.rs`. `rust-toolchain.toml` and `.cargo/config.toml` were left untouched (already present).
Task 2: `fs_kind.rs`, `ptype.rs` (incl. `gpt_types`), `model.rs`, with lib.rs re-exports.

## TDD evidence
- Task 1: tests-only module files + lib.rs written first; `cargo test -p pm-core` -> RED (unresolved imports `device::*`, `progress::*`, `units::*`, 80 compile errors). Implementation added -> 13/14 passed, `format_size_matches_screenshots` failed (see concerns), fixed -> 14 passed.
- Task 2: tests-only module files first -> RED (109 compile errors, `DiskInfo`/`PartitionInfo`/... not found). Implementation added -> GREEN.
- Final: `cargo test -p pm-core`: 23 passed, 0 failed; `cargo build` and test build both emit 0 warnings. No `unsafe` in pm-core (grep clean). clippy is not installed for this toolchain, so it was not run.

Tests: mem_device_roundtrip, read_past_end_is_error, slice_device_offsets_and_bounds, readonly_rejects_write, file_device_create_open_roundtrip, copy_range_copies_exact_bytes, boxed_and_ref_forwarding, format_size_matches_screenshots, parse_size_variants, align_helpers, cancel_token_check_errs, 3 progress tracker tests, ptype_names_for_known_guids, gpt_constants_have_standard_values, ptype_predicates, fs_kind display/mountable, free_ranges_finds_gaps, free_ranges_edge_cases, display_name_format, partition_helpers, serde_roundtrip_diskinfo.

## Deviations / decisions the controller should know
1. `format_size`: the brief's example `"1858.18 GB"` contradicts its own rule (1024 steps, TB unit) and the `"1.82 TB"` display_name example: 1858 GiB is 1.81 TB. I followed the rule. Test asserts `1015*MiB+660_000 -> "1015.63 MB"`, `1000 GiB+194M -> "1000.18 GB"`, `2_000_398_934_016 -> "1.82 TB"`. Also rounding that would print "1024.00 X" promotes to the next unit (GIB-1 -> "1.00 GB").
2. `PartFlags` gained `pub boot: bool` (as instructed). `status_text()` order: Active, System (flags.required or EFI ptype), Boot, joined " & ".
3. Added derives beyond the brief so the specified `#[derive(Default)]` structs compile and roundtrip tests can compare: `Default` on `TableKind` (Unknown), `DiskBus` (Unknown), `VolumeId`; `PartialEq, Eq` on `PartitionInfo`, `DiskInfo`, `VolumeInfo`.
4. Small extras not in the brief: `BlockDevice::is_empty` (default method), `FileDevice::with_sector_size`, `SliceDevice::into_inner`, `CancelToken::new`, `ProgressTracker::set_detail`/`done`.
5. `ProgressTracker`: no report at creation; first report happens on an `add` >= 100 ms after creation (or `finish`). Rate is computed from samples taken at report time over the last ~2 s; `eta_secs` is `Some(0)` when done >= total, `None` when rate is 0. `finish()` reports the actual `done` (does not force done = total).
6. `copy_range` rejects `buf_size == 0` with `Invalid`; caller should pick a sector-multiple buffer for raw devices (documented).
7. `FileDevice::write_at` on a read-only open returns `Invalid("read-only")`; reads/writes are bounds-checked against `len` (so raw handles need `with_len`).
8. `is_data()` definition (brief did not specify): GPT BASIC_DATA/LINUX_FS, MBR 0x01/04/06/07/0B/0C/0E/83. GPT tail uses 33 sectors as instructed (the model brief comment says 34; the controller note said 33).
9. Extra GPT/MBR display names beyond the required ones (Linux swap, Linux LVM, Apple HFS+/APFS, "GPT (BIOS boot)", MBR 0x82/0x8E/0xEE/0x27/etc.).
10. `Cargo.lock` is committed (binary workspace). Git warns about LF->CRLF on this machine; harmless.

## Self-review
Diff re-read; free_ranges handles unsorted/overlapping partitions (extended + logicals), zero-length partitions, tiny disks and 4Kn tail. `letter()` only matches `X:`/`X:\`/`X:/` roots, not folder mounts. Nothing else outstanding.

## Fix round 1
Review finding (Important): `align_up` overflowed for `v > u64::MAX - a`.
Changes (pm-core only):
- `units.rs`: `align_up` now uses `v.div_ceil(a).checked_mul(a).unwrap_or(u64::MAX)` (saturates, signature unchanged). New test `align_up_saturates_on_overflow` asserts `align_up(u64::MAX, 512) == u64::MAX`, `align_up(u64::MAX - 100, 512) == u64::MAX`, and that `u64::MAX - 511` (already a multiple's neighbour, no overflow) still rounds correctly.
- `device.rs`: `copy_range` validates `src_off + len <= src.len()` and `dst_off + len <= dst.len()` up front with `checked_add`, returning `Error::Invalid` before any write. New test `copy_range_validates_bounds_before_writing` covers small destination (dst stays all zero), oversized source range, and u64 overflow of either offset.
RED: both new tests failed before the fix (2 failed, 23 passed). GREEN: `cargo test -p pm-core` -> 25 passed, 0 failed, 0 warnings.
