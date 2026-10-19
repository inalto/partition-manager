# Task 7 report: pm-fs (detection, FileSystem trait, UsedMap, FAT reader, FAT32 formatter)

Status: DONE. Commit d3c2db6 `feat(fs): detection, FAT parser, FAT32 formatter` (branch dev). Only `crates/pm-fs` and a minimal `Cargo.lock` change (the pm-fs package block only; the working-copy Cargo.lock still carries other agents' uncommitted pm-cli/pm-image entries, which I deliberately left unstaged).

## Implemented
- `src/util.rs`: LE/BE readers, `read_padded` (zero-extends short devices), label/UUID/volume-id formatters.
- `src/bitmap.rs`: `UsedMap { unit, bits, len }` with `new/set/is_used/used_bytes/ranges/all_used`. LSB-first bit order, `set` marks every touched unit and clips to `len`, `ranges()` is a lazy coalescing iterator (skips whole 0x00/0xFF bytes), last range clipped to `len`.
- `src/traits.rs`: `Entry`, `FileSystem` (with default `extract` via `read_file`, 64 KiB buffer), `open(Box<dyn BlockDevice>)` dispatching on `detect` (FAT12/16/32 supported; others `Error::Unsupported`).
- `src/detect.rs`: `FsProbe`, `detect`, `probe_for_search`. 68 KiB window, short devices zero-padded. Order: exFAT, NTFS, ReFS ("ReFS"@3 or "FSRS"@0x10), FAT (BPB sanity + cluster-count typing, shared `Bpb::parse`), ext2/3/4 (feature flags as specified), btrfs, XFS, HFS+, APFS, LUKS, swap, ISO9660, UDF. Fills total/label/cluster/uuid as specified; additionally (beyond the brief's minimum) total/cluster/label/uuid for btrfs, XFS, HFS+, APFS, ISO9660 and ReFS total/cluster from their superblocks. `used` is always `None`. exFAT uuid is "" per the brief.
- `src/fat/bpb.rs`: `Bpb::parse` with all brief sanity checks plus: FAT size > 0, overhead < total sectors, root entries nonzero for FAT12/16, root cluster >= 2 for FAT32; geometry helpers.
- `src/fat/dir.rs`: directory scanner: LFN assembly (sequence + checksum verified, otherwise falls back to 8.3), NT lowercase bits 0x08/0x10, 0x05 first-byte escape, skips label entries and "."/"..", stops at 0x00, 0xE5 entries returned separately as `"?" + rest of 8.3 name` lowercased, DOS date/time to unix (UTC; zero/invalid date gives `None`).
- `src/fat/mod.rs`: `FatFs` (also `FatFs::open(dev, base_offset)`): one-sector FAT cache, FAT12/16/32 entry decoding, chain walking bounded by the cluster count (`Error::Parse("FAT chain loop")`), a chain cursor so sequential `read_file` calls are O(1) per cluster, `list`, `read_file`, `used_map` (unit = cluster size; reserved + FATs + fixed root marked used, then every cluster whose FAT entry != 0; FAT read in 1 MiB-ish chunks), `deleted_entries` (iterative walk, visited set, per-directory progress, cancel check, unreadable subdirectories skipped rather than aborting). Volume label = root label entry if present, else BPB label.
- `src/fat/format.rs`: `FormatOpts`, `format_fat32` per the brief (EB 58 90, MSWIN4.1, 32 reserved, 2 FATs, FSInfo@1, backup boot at 6 (sectors 0-2 copied to 6-8), root cluster 2, media F8, ext sig 0x29, FAT[0..2], label entry + BPB label, volume id from opts or clock). All validation happens before the first write.

## Decisions beyond the brief
- Automatic cluster size: start from the size table, then halve while the cluster count is below 65525 (otherwise a 40 MiB volume with 4 KiB clusters would be detected as FAT16). Explicit `cluster_size` that is not a power of two, < sector size, > 128 sectors, or yields < 65525 / > 0x0FFFFFF5 clusters gives `Error::Invalid`.
- Label > 11 chars or containing non-ASCII/illegal FAT characters gives `Error::Invalid`; stored uppercase. Empty label writes "NO NAME    " in the BPB and no root label entry.
- Sector size other than 512/1024/2048/4096 gives `Invalid`; total sectors > u32::MAX gives `Invalid`.

## Tests (RED then GREEN)
- RED: after writing `tests/{detect,fat,bitmap}.rs` and `tests/common/mod.rs` against an empty `lib.rs`, `cargo test -p pm-fs` failed to compile (unresolved `pm_fs::format_fat32`, `FormatOpts`, `UsedMap`, ...).
- GREEN: `cargo test -p pm-fs`: 4 bitmap + 6 detect + 10 fat = 20 tests pass, 0 warnings (`cargo build` and `cargo test --no-run` both warning-free).
- Brief tests present: `detect_recognizes_all_signatures`, `format_fat32_then_detect_and_list_empty_root`, `fat32_write_entries_with_builder_then_list`, `fat32_used_map_counts_reserved_fats_and_root`, `fat_deleted_entries_lists_e5`, `fat16_small_image_parses`. Extras: FAT12 odd/even entries, chain loop, bad-BPB rejection, short device/offset detection, partition offset via `SliceDevice`, `probe_for_search`, formatter validation (small volume leaves device untouched, cluster-size errors, 0xAA data region preserved), cancellation, `open` on unknown/ext.
- The image builder (`tests/common/mod.rs`) writes dir entries, LFN sets (own checksum implementation) and FAT chains (incl. a non-contiguous 7->9->8 chain) directly; the reader is not used to build images.

## Concerns / notes
- `clippy` and `rustfmt` are not installed for the pinned toolchain, so neither was run.
- Formatter writes hidden sectors = 0 (the device does not know its partition start); fine for data volumes, not for boot volumes.
- `used_map` uses unit = cluster size aligned to filesystem start; when `data_start` is not cluster-aligned a cluster straddles two units and both are marked (conservative: never under-reports used data).
- Not tested: FAT32 formatting with non-512 sector sizes (code paths are generic), volumes > 4 GiB (tests use 40 MiB images).
- Other crates' state: no errors or warnings from other crates were observed in `cargo test -p pm-fs`.

## Fix round 1

Changes (all under `crates/pm-fs`):
1. Bounded directory reads: `FatFs::dir_chain` caps directory chains at `ceil(2 MiB / cluster size)` clusters and returns `Error::Parse("directory chain too long")` (a FAT directory holds at most 65 536 x 32-byte entries); the old unbounded `chain` was removed. `used_map` now builds its map with `len = min(bpb total_bytes, dev.len() - base)`.
2. `UsedMap.origin`: new `pub origin: u64` field (always `< unit`), `UsedMap::new` keeps origin 0, new `UsedMap::with_origin(len, unit, origin)` (origin reduced modulo unit). Unit 0 covers `[0, origin)`, unit i >= 1 covers `[origin + (i-1)*unit, origin + i*unit)`; `set`, `is_used`, `used_bytes`, `ranges` honour it and clip to `len` (implemented via a single `shift = (unit - origin) % unit`, so origin 0 is identical to before). FAT `used_map` uses `with_origin(len, cs, data_start % cs)`, so clusters map 1:1 onto units. Struct-literal construction of `UsedMap` now needs the extra field; `new`/`set` callers are unaffected.
3. Corrupt-input coverage (new `tests/fat_corrupt.rs`, 11 tests) plus `bitmap_origin_units_align` in `tests/bitmap.rs`; `tests/common` gained `fat1x_custom` (explicit geometry). The loose used-bytes bound in `fat32_used_map_counts_reserved_fats_and_root` is now `assert_eq!(used, data_off + 8 * cs)`.

New tests: `fat_dir_chain_too_long_is_parse_error` (self-pointing directory; deleted-entry scan skips it), `fat_used_map_clamps_to_device_len`, `fat_used_map_is_exact_when_data_region_is_unaligned` (FAT16, 2 KiB clusters, data start 50688 = 1536 mod 2048; used_bytes == reserved+FATs+root+5 clusters exactly, ranges and is_used boundaries), `bitmap_origin_units_align`, `fat_lfn_corruption_falls_back_to_short_name` (out-of-order, bad checksum in all/one piece, missing piece, seq 0, orphan set before an unrelated entry), `fat_orphan_lfn_before_terminator_is_ignored` (also an entry past the 0x00 terminator is not seen), `fat_kanji_escape_byte_0x05_means_0xe5`, `fat_directory_chain_outside_data_region_is_parse_error` (out of range / bad / reserved / free values and an out-of-volume start cluster), `fat32_entries_with_high_bits_set_are_masked`, `fat_eoc_and_bad_cluster_values_end_or_fail_chains_for_each_width` (FAT12/16/32: both EOC values end a directory chain cleanly; bad-cluster marker gives `Parse`; for a file with EOC/bad mid-chain the clusters before the break read, the rest is `Parse`), `format_fat32_with_4096_byte_sectors_round_trips` (280 MiB, detect + open + list + used_map), `format_fat32_on_a_5_gib_sparse_file_follows_the_size_table` (FileDevice via tempfile: cluster 4096, `fat_sectors*(bps/4) >= clusters+2`, and one sector fewer would not fit).

Interpretation note: "EOC and bad-cluster values stop a chain": EOC values end the chain; a bad-cluster marker is treated as corruption and returns `Error::Parse` (never silently truncates).

Evidence: mutation check - forcing origin 0 in the FAT `used_map` makes `fat_used_map_is_exact_when_data_region_is_unaligned` fail (left 63488 vs right 60928), i.e. the old over-marking is caught. Command `cargo test -p pm-fs` (no `src/ntfs` present yet at run time): bitmap 5/5, detect 6/6, fat 10/10, fat_corrupt 11/11 pass, 0 warnings, doc-tests 0.

## Follow-up round (deleted multi-cluster files)

Problem: a real FAT delete zeroes the chain, so `read_file` on a `deleted` entry failed. Fix (`src/fat/mod.rs`, documented in the module doc and the `read_file` doc comment): for `deleted == true` entries the chain is followed while intact; at the first cluster whose FAT entry is 0 the file is assumed to continue in the next contiguous clusters (for the rest of its `ceil(size / cluster_size)` clusters, and it stays contiguous from then on, so foreign chains are never followed). If the first cluster is free the whole file is the contiguous run starting there. Reaching an invalid cluster or the end of the volume gives `Error::Parse`; EOC/bad entries mid-chain of a deleted file still error as before; live entries are unchanged. The read cursor now also keys on `deleted` and remembers the contiguous state. Note: where the intact prefix is non-contiguous the run continues after the last intact cluster (not from the first cluster); identical for the common contiguous case.

Tests (`tests/fat_undelete.rs`): `fat32_deleted_multi_cluster_files_are_recoverable` (Windows-style delete of 3-cluster files: fully freed chain; only the first link intact; intact non-contiguous prefix 120 -> 125 then freed; each listed by `deleted_entries` with right size/id, `extract` returns the full original, partial reads across cluster boundaries and at the tail match) and `fat_deleted_heuristic_does_not_apply_to_live_files_and_is_bounded` (live file with a freed entry still errors; a deleted run leaving the volume and an invalid start cluster give `Parse`).

Command: `cargo test -p pm-fs --test fat --test fat_corrupt --test fat_undelete --test bitmap --test detect`
```
test result: ok. 5 passed; 0 failed   (bitmap)
test result: ok. 6 passed; 0 failed   (detect)
test result: ok. 10 passed; 0 failed  (fat)
test result: ok. 11 passed; 0 failed  (fat_corrupt)
test result: ok. 2 passed; 0 failed   (fat_undelete)
```
No warnings.
