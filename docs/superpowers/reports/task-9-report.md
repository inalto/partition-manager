# Task 9 report: pm-fs exFAT and ext2/3/4 readers

Status: DONE (with concerns below). Commit: `a911e4c feat(fs): exFAT and ext readers` on `dev`.

## What was implemented

crates/pm-fs only. New files are `src/exfat/{mod,boot,dir,fs}.rs`, `src/ext/{mod,super_block,inode,extent,dir,fs}.rs` and `tests/{exfat,exfat_builder,ext,ext_builder}.rs`. Edits to existing files: `src/lib.rs` (`pub mod exfat; pub mod ext;` only), `src/traits.rs` (the two dispatch arms), and `tests/fat.rs` (see concern 1).

### exFAT (`pm_fs::exfat::ExFat`)
- Boot sector: every field is validated at open with checked arithmetic.
  - sector and cluster shifts are range-checked;
  - the volume must fit in the device;
  - the FAT region must not overlap the heap;
  - `heap + cluster_count * cluster` must fit in the volume;
  - the root cluster must lie in the heap;
  - with two FATs, VolumeFlags bit 0 selects the active FAT.
- `Entry.id` layout (documented in `exfat/mod.rs`):
  - bits 0..32 = first cluster;
  - bit 32 = NoFatChain;
  - bits 33.. = cluster count of a NoFatChain stream (`ceil(data_length / cluster)`).
  - Directories report size 0, so a contiguous directory needs the count in its id.
- Entry sets: 0x85/0xC0/0xC1, plus 0x83 label and 0x81 bitmap. The 0x00 end marker stops the walk. Timestamps become unix seconds, with the 10 ms increment and UTC offset applied. Set checksums are deliberately not verified (recovery friendliness).
- In-use sets: a damaged set is a Parse error in `list` and is skipped in the deleted scan.
- Deleted sets (`0x05`/`0x40`/`0x41`): the look-ahead stops at the first in-use entry, so a reused slot is never misparsed.
- `read_file` follows contiguous runs (several clusters per read) or the FAT chain. A cursor caches the last chain position, so sequential reads of a fragmented file are not quadratic. Chain walks are bounded by `cluster_count`.
- Deleted-file recovery is best effort: a deleted chained stream whose first FAT entry is free is read as contiguous.
- `used_map`:
  - unit = cluster, `origin = heap_start % cluster`;
  - `[0, heap_start)` is marked, which covers the boot regions, backup boot and every FAT;
  - the root chain and bitmap clusters are marked;
  - the bitmap is streamed with run coalescing and clipped to `ClusterCount`;
  - multiple 0x81 entries (TexFAT) are OR-ed, which can only over-approximate;
  - a bitmap that is too short or points outside the heap is a Parse error (fail closed) rather than a silently incomplete map;
  - the bitmap itself is read via the FAT chain, or as contiguous when its first FAT entry is free.
- `deleted_entries`: BFS over all in-use directories with a visited set, cancel check and progress per directory. A damaged directory is skipped, not fatal.

### ext (`pm_fs::ext::Ext`)
- Superblock: validates block size shift, inode size (a power of two in 128..=block size), `bpg`/`ipg` against the one-block bitmaps, and the 64BIT `desc_size`.
- Further superblock checks:
  - fs size (including a 64-bit overflow) must fit in the device;
  - `first_data_block <= 1`;
  - the descriptor table must fit in the fs;
  - all inode tables together must fit in the fs. This also bounds the deleted-inode scan by the device length.
- Unsupported at open: BIGALLOC, META_BG, compression, journal device. Inline-data and encrypted files fail on read.
- Inodes: extent tree (depth <= 5, a node's depth must equal its parent's minus 1, so a self-referencing index fails; counts, magic and physical addresses are checked) and classic direct/single/double/triple indirect maps. Holes and uninitialized extents read as zeros, and hole lengths are computed so sparse reads are fast. Fast symlinks are read from `i_block`. A 64-bit size is used for regular files, with an extra-epoch mtime.
- Directories are read linearly; htree index blocks are skipped via inode 0 / `rec_len`. `rec_len` is validated (>= 8, multiple of 4, inside the block, name fits), so a corrupt directory is a Parse error and always progresses.
- Directories are capped at 256 MiB and at the device length.
- `used_map`:
  - unit = block, origin 0;
  - block 0 / superblock / GDT / reserved GDT are marked;
  - every group's bitmaps and inode table are marked (bad pointers are a Parse error);
  - the block bitmap is applied, clipped to `blocks_count`;
  - a `BLOCK_UNINIT` group (honoured only when GDT_CSUM/METADATA_CSUM is present, as in the kernel) contributes only its metadata plus a possible sb/GDT backup, over-approximated;
  - `needs_recovery` (journal not replayed, bitmaps possibly stale) returns an all-used map, which is safe for cloning.
- `deleted_entries`: scans every inode table, skipping `INODE_UNINIT` groups, inodes below `first_ino` and damaged descriptors. A match has `dtime != 0 && links == 0 && mode != 0`. It reports `inode-N`, path `/inode-N`, id N, and cancels/reports per group. This is documented in the `ext/mod.rs` module doc.

## Tests (all in `cargo test -p pm-fs`)
`tests/exfat.rs` (10): `exfat_lists_and_reads`, `exfat_used_map_from_bitmap` (the brief's names), plus:
- `exfat_open_dispatch`
- `exfat_used_map_unaligned_heap` (heap off the cluster grid, origin 2048)
- `exfat_used_map_full_bitmap`
- `exfat_used_map_bitmap_spans_clusters` (512-byte clusters, 8-cluster bitmap, chained and contiguous)
- `exfat_used_map_short_bitmap_is_error`
- `exfat_deleted_entries_listed` (also best-effort recovery and cancel)
- `exfat_dir_cycle_is_bounded`
- `exfat_corrupt_never_panics`

`tests/ext.rs` (8): `ext4_extent_file_read`, `ext2_indirect_blocks_read`, `ext_used_map_from_group_bitmaps`, `ext_deleted_inodes_listed` (the brief's names), plus:
- `ext_used_map_marks_everything_when_journal_needs_replay`
- `ext_open_rejects_unsupported_features`
- `ext_inline_data_and_encrypted_files_are_unsupported`
- `ext_corrupt_never_panics`

Coverage detail:
- `ext_used_map_from_group_bitmaps` compares the map block by block against the builder's set. It runs 1 KiB and 4 KiB blocks, 32- and 64-byte descriptors, ext2 and ext4, and a BLOCK_UNINIT group whose on-disk bitmap is garbage 0xFF.
- Corrupt tests: zero/huge shifts, FAT offset beyond device, huge counts, extent depth 100 and 5, a self-referencing index block, bad extent magic and entry counts, `rec_len` 0 / small / unaligned / past the block, descriptor pointers outside the device, a 4 GiB directory, group-count overflow, 64-bit block-count overflow, truncated devices, plus seeded random byte corruption of every metadata area (150 exFAT and 400 ext images through open/list/read/used_map/deleted_entries).

### RED/GREEN evidence
- Builders and tests were written first. RED: with the module declarations absent, compilation failed with `error[E0433]: cannot find 'exfat' in the crate root` and the same for `ext` (the new tests cannot even resolve `pm_fs::exfat::ExFat`).
- GREEN: `cargo test -p pm-fs` passes with zero warnings. exfat 10/10, ext 8/8, fat 10, fat_corrupt 11, ntfs 7, detect 6, bitmap 5, unit 8.
- Mutation sanity check: disabling the BLOCK_UNINIT branch makes `ext_used_map_from_group_bitmaps` fail, so the tests do bite.
- Limitation of the RED evidence: I wrote the implementation before running the tests against it, so there is no run showing individual assertions failing in an earlier stub. The RED is a compile failure.

## Concerns / things the reviewer should know
1. I edited `crates/pm-fs/tests/fat.rs` (Task 7's file), 5 lines. `open_rejects_unknown_and_unsupported` used an ext superblock as its "recognised but unsupported" example. With ext now dispatched it returns Parse instead of Unsupported, which broke that test. I switched the example to an XFS magic. If the Task 7 owner is also editing this file, expect a trivial merge.
2. The brief says INLINE_DATA is superblock incompat `0x1000`; in the kernel it is `0x8000` (0x1000 is DIRDATA). I did not gate on the superblock flag. Inline data is detected per inode via `i_flags & 0x10000000`, as the brief specifies.
3. `open` requires the volume/filesystem size to be <= the device length (like NTFS). A truncated image of a larger fs is rejected with Parse instead of being partially readable. FAT's reader is more lenient.
4. exFAT `Entry.size` is `DataLength`, not `ValidDataLength`; bytes past VDL are read from storage, not zeroed. Directory entries report size 0.
5. ext4 `needs_recovery` returns an all-used map (a full copy), by design. The alternative of erroring was rejected, to avoid blocking clones of dirty volumes.
6. Not supported, by design: META_BG, BIGALLOC (both fail at open with Unsupported, never a wrong map), inline data, encryption, and ext journal replay. exFAT set checksums are not verified.
7. `cargo clippy` is not installed for this toolchain, so only rustc warnings were checked (none).
8. Windows git warns that LF becomes CRLF on the new files, which is the existing repo behaviour.

## Self-review checklist
- Bounds: every on-disk index is checked. exFAT entry sets and ext dir records use explicit length checks. Extent and indirect pointers are checked against `blocks_count`. Cluster numbers go through `cluster_offset`. Inode numbers and tables are range-checked.
- Allocations: bounded by device length (descriptor table, directory reads, bitmap streamed in <= 1 MiB pieces, inode-table scan in <= 1 MiB chunks).
- Loops: bounded. FAT chains by `cluster_count`, extent trees by the depth limit plus strict depth decrement, indirect maps by 3 levels, dir records advance >= 8 bytes, and every read loop advances >= 1 byte. Cancel is checked per directory (exFAT) and per group (ext).
- Used map completeness: exFAT marks the whole pre-heap region plus bitmap-set clusters, with bitmaps/root explicitly marked. ext marks the metadata areas plus bitmaps, and over-approximates UNINIT groups and unreplayed journals. Missing or short bitmaps and bad metadata pointers are errors, never silent omissions.

## Fix round 1 (commit 9e9c1c4)

Changes (src/exfat/*, src/ext/*, tests/exfat.rs, tests/ext.rs only):
1. `ext/super_block.rs`: `groups * ipg * inode_size` now uses chained `checked_mul(...).ok_or_else(Parse)`. New test `ext_huge_geometry_is_a_parse_error_not_an_overflow` (sparse 600 GiB `FileDevice`, 64 KiB blocks, bpg = 1, ipg = 524288, inode size 32768) asserts `Ext::open` and `pm_fs::open` return an error (Parse) without panicking. Note: at 600 GiB the product is about 1.7e17 and does not actually overflow u64 (that needs a device of tens of TiB), so the test exercises the large-geometry "tables > total" guard; the overflow path itself is covered by the checked arithmetic.
2. `exfat/fs.rs` `read_file`: a NoFatChain run is validated once (`first >= 2` and `first + clusters <= cluster_count + 2`, checked add), else `Error::Parse`, before any read. New test `exfat_contiguous_run_must_stay_in_the_heap` (heap at sector 260, which leaves 2 KiB of volume slack where the old code would have read out-of-heap bytes successfully) covers a run past the heap end and a first cluster below 2.
3. ValidDataLength: `FileSet.valid_len` (stream @8, clamped to data_len). `Entry` has no spare field and `id` is full, so `ExFat` keeps a side map `first cluster -> valid_len` (filled by `list` and `deleted_entries` for files with valid < data; a deleted entry never overrides a live one). `read_file` reads only `[0, valid_len)` from disk and zero-fills the rest. New test `exfat_valid_data_length_zero_fills_the_tail` covers a contiguous file, a FAT-chained file and a deleted file, each with stale 0xAA beyond valid_len on disk, plus reads straddling and wholly past valid_len.

Command: `cargo test -p pm-fs` (zero warnings). Output:

```
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s   (unit)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (bitmap)
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s   (detect)
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s  (exfat)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (exfat_builder)
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.60s   (ext)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (ext_builder)
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s  (fat)
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s  (fat_corrupt)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s   (ntfs)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (ntfs_builder)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (doc-tests)
```

## Fix round 2

Changes (src/exfat/*, tests/exfat.rs, tests/ext.rs):
- A. The ValidDataLength side map is keyed by `(first cluster, deleted)`. Every non-empty file stores an explicit entry (live: always overwritten, never removed as a marker; deleted: the first listed wins). `read_file` looks up `(first, e.deleted)`. The module doc (`exfat/mod.rs`) states that VDL is only known for entries obtained from the same `ExFat` instance; entries built by hand read up to `size`.
- B. One helper, `Loc::check_run(cluster_count)` (`first >= 2` and checked `first + clusters <= cluster_count + 2`), is used by every NoFatChain consumer: `read_file`, `cluster_at`, and `chain()` (directory reads and the allocation bitmap in `used_map`). `ExFatBoot::cluster_offset` already refused clusters outside the heap with Parse (from round 0), so it stays as the second line of defence.
- Test-comment fixes (frag.bin valid-length comment; `FirstCluster` at stream offset +20) and `pm_fs::open` in `ext_huge_geometry_is_a_parse_error_not_an_overflow` now asserts `Some(Error::Parse(_))`.

New tests: `exfat_live_file_unaffected_by_deleted_entry_sharing_first_cluster` (both VDL arrangements, both call orders), `exfat_directory_run_must_stay_in_the_heap`, `exfat_bitmap_run_must_stay_in_the_heap`, and `exfat_contiguous_run_must_stay_in_the_heap` extended (heap-ends-the-volume case added).

RED (new tests run before the implementation):
```
test exfat_bitmap_run_must_stay_in_the_heap ... ok
test exfat_directory_run_must_stay_in_the_heap ... ok
test exfat_contiguous_run_must_stay_in_the_heap ... ok
test exfat_live_file_unaffected_by_deleted_entry_sharing_first_cluster ... FAILED
thread 'exfat_live_file_unaffected_by_deleted_entry_sharing_first_cluster' panicked at crates\pm-fs\tests\exfat.rs:528:13:
assertion `left == right` failed: live 6000/40 deleted_first false   (live hello.txt zero-filled from the deleted file's VDL)
test result: FAILED. 14 passed; 1 failed
```
Honest note: the three heap-bound tests already passed before the refactor, because `cluster_at` checked each cluster individually (the run was never read out of the heap for directories and the bitmap). They are regression guards for the now-centralised check, so only the collision test was RED.

GREEN: `cargo test -p pm-fs`, zero warnings:
```
test result: ok. 8 passed  (unit)
test result: ok. 5 passed  (bitmap)
test result: ok. 6 passed  (detect)
test result: ok. 15 passed (exfat)
test result: ok. 9 passed  (ext)
test result: ok. 10 passed (fat)
test result: ok. 11 passed (fat_corrupt)
test result: ok. 7 passed  (ntfs)
(+ another crate-owner test binary with 2 passed, and the empty builder/doc-test binaries)
```
