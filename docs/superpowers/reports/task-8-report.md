# Task 8 report: pm-fs NTFS reader

**Status:** DONE. Commit `8fbd2ef feat(fs): NTFS reader` on `dev`.

## What was implemented (`crates/pm-fs/src/ntfs/`)
- `boot.rs`: `NtfsBoot::parse` reads the OEM id, bytes/sector @0x0B, sectors/cluster @0x0D, total_sectors @0x28, MFT LCN @0x30, MFTMirr LCN @0x38, clusters per MFT record @0x40 and per index block @0x44 (i8: >0 = clusters, <0 = 2^-v bytes), and the serial @0x48. It validates every field: power-of-two sizes, record size 512..=64 KiB, cluster size ≤ 2 MiB, MFT inside the volume.
- `runs.rs`: `pub fn decode_runs`. The length and offset fields are signed, offsets are relative to the previous allocated LCN, a sparse run does not move that base, and a 0 header or the end of the buffer ends the list. A bad header, a truncated run, a length ≤ 0 or a negative LCN gives `Error::Parse`.
- `mft.rs`:
  - `apply_fixups` uses a 512-byte stride. It checks the signature, requires `usa_count == len/512 + 1`, and bounds-checks the array. A mismatch gives `Parse("bad fixup")`.
  - `Record` exposes flags @0x16, sequence @0x10, base reference @0x20 and the first attribute @0x14.
  - Attribute headers are read with the resident and non-resident layouts from the brief. Every name, value and runlist slice is bounds-checked.
  - Also here: `FileName` (namespace preference Win32/Win32&DOS > POSIX > DOS), the `$STANDARD_INFORMATION` modified time (FILETIME → unix), and `$ATTRIBUTE_LIST` entries.
- `index.rs`: parses `$INDEX_ROOT` and INDX blocks (fixups, VCN check @0x10, INDEX_HEADER @0x18). The B-tree is walked in order, children before keys. A visited-VCN set catches loops and depth is capped at 64.
- `fs.rs`: `pub struct Ntfs` implementing `FileSystem`, plus `pub fn open_ntfs`.
  - **Bootstrap:** `$MFT` record 0 comes from the MFT LCN, falling back to `$MFTMirr`. If record 0 has an attribute list, the `$DATA` extents are pulled in one at a time in VCN order, so fragmented MFTs with attribute lists can bootstrap.
  - **Streams:** extents from the attribute list are merged by start VCN. Sizes come from the VCN-0 extent. Runs are looked up by binary search. Sparse runs and bytes past `initialized_size` read as zeros, and runs pointing past the device give `Parse`.
  - **`list()`:** reads `$I30` root and allocation (the allocation can come through an attribute list) and walks the index. It drops `.`, drops DOS aliases when a long name exists, and dedupes by (record, name). Each child record is read to get the `$STANDARD_INFORMATION` mtime and the real `$DATA` size, because sizes stored in the index go stale. If a record is stale or unreadable (wrong sequence, torn), the entry falls back to the index copy of `$FILE_NAME`.
  - **`read_file()`:** reads the unnamed `$DATA`. Non-resident data with compression flags gives `Unsupported("NTFS compressed")`; encrypted data gives `Unsupported("NTFS encrypted")`. Deleted records can still be read. The last stream is cached so `extract()` stays cheap.
  - **`used_map()`:** copies the `$Bitmap` bytes straight into `UsedMap` (both are LSB-first with unit = cluster, origin 0). Padding bits past the last cluster are masked off. It also marks the boot area (max(cluster, 8 KiB)), the `$MFT` and `$MFTMirr` runs, and the backup boot sector. Map length = `(total_sectors+1)*bps`, clipped to `dev.len()`.
  - **`deleted_entries()`:**
    - Reads the MFT in batches of 1024 records, falling back to one record at a time if a batch read fails.
    - Checks `cancel` and reports progress ("Scanning MFT", done/total) once per batch, plus a final report.
    - Emits records that have a FILE signature, the in-use bit clear, base reference 0 and a `$FILE_NAME`.
    - The path comes from the parent chain, cached per parent. Each ancestor must be an in-use directory with a matching sequence number; otherwise the path is `/<name>`.
- `lib.rs`: `pub mod ntfs; pub use ntfs::{open_ntfs, Ntfs};`
- `traits.rs`: added one line, the `FsKind::Ntfs` arm in `open()`. See the concerns section for why this is in `traits.rs` rather than `lib.rs`.

## Tests
`tests/ntfs.rs` contains exactly the 7 test names from the brief. It includes the builder via `#[path = "ntfs_builder.rs"] mod ntfs_builder;`. The builder also compiles as its own test target, which has 0 tests and no warnings.

**Builder (`tests/ntfs_builder.rs`, test-only).** It writes these structures byte by byte, applying fixups:
- the boot sector and its backup
- records 0..=15 as system files: `$MFT` with `$DATA` and `$BITMAP`, `$Volume` with label TESTVOL, root with `$INDEX_ROOT` plus a two-block `$INDEX_ALLOCATION` B-tree and `$BITMAP`, `$Bitmap`, `$BadClus` with a sparse named `$Bad` stream, and so on
- the test files:

| Record | File | What it exercises |
|---|---|---|
| 16 | hello.txt | resident, 20 bytes |
| 17 | big.bin | 3+2 clusters in two runs; the index stores a stale size of 0 |
| 18 / 19 / 27 | split.dat | `$ATTRIBUTE_LIST`; `$DATA` split into two extents in two extension records |
| 20 | gone.txt | deleted |
| 21 / 22 | docs/ and readme.txt | resident index |
| 23 | Document Long Name.txt | has a DOS alias |
| 24 | sparse.bin | sparse run; `initialized_size` < real size |
| 25 | comp.bin | compressed |
| 26 | docs/old file.txt | deleted, DOS name first |
| 28 | ghost | deleted extension record carrying a `$FILE_NAME` |
| 29 | enc.bin | encrypted |
| — | orphan | cluster used in `$Bitmap` that belongs to no file |

Option `mft_second_lcn` puts MFT records 8.. in a second run and fills the gap with 0xFF.

**RED:** `cargo test -p pm-fs --test ntfs` failed with `error[E0432]: unresolved import pm_fs::ntfs` before any implementation. The builder target compiled clean on its own.

**GREEN:** `cargo test -p pm-fs` gives 7 unit + 5 bitmap + 6 detect + 10 fat + 11 fat_corrupt + 7 ntfs tests passing, with 0 warnings.

**Mutation checks** (each mutation was applied on its own, the tests run, then the code restored):

| Mutation | Test that failed |
|---|---|
| ignore `initialized_size` | ntfs_reads_nonresident_file_across_runs |
| ignore `$ATTRIBUTE_LIST` | ntfs_attribute_list_followed, ntfs_fragmented_mft_runs |
| pre-order instead of in-order walk | ntfs_lists_root_and_reads_resident_file |
| treat the MFT as contiguous | ntfs_fragmented_mft_runs |
| no DOS dedupe | ntfs_lists_root_and_reads_resident_file |
| `used_map` ignores `$Bitmap` | ntfs_used_map_from_bitmap |
| take the first `$FILE_NAME` instead of the preferred one | ntfs_deleted_record_listed |

**Unit tests in `src/ntfs`:**
- boot sector sizes, including 0xF8 = 128 KiB clusters
- fixups restore sector ends and detect tears, a bad USA count and a USA out of range
- corrupt attribute lengths and value offsets give `Parse`
- B-tree in-order walk, self-loop detection, missing terminator
- signed run fields
- two never-panic fuzz loops, each pseudo-random: 20k runlists, and 2k records with plausible attribute headers

## Self-review
- **Offsets** match the brief for: boot sector fields, the record header, the attribute header (resident and non-resident), `$FILE_NAME`, attribute list entries, `$INDEX_ROOT`, INDEX_HEADER, index entries (sub-node VCN in the last 8 bytes) and INDX blocks.
- **No panics on corrupt input:**
  - Every slice goes through `sub()` or is guarded by a preceding length check: record ≥ 512 after fixups, attribute ≥ 16 / 0x18 / 0x40, list entry ≥ 0x1A, `$FILE_NAME` ≥ 0x42, index entry ≥ 0x10 / 0x18.
  - Arithmetic on disk values is checked: run LCN and VCN sums, cluster-to-byte conversions, index header offsets.
  - Allocations are bounded: record and index block size ≤ 64 KiB, attribute list ≤ 4 MiB, and a volume larger than the device is rejected at open, so a corrupt `total_sectors` cannot size a giant `UsedMap`.
  - Loops always terminate: attribute lengths are ≥ 16, the walk tracks visited VCNs and a depth cap, and parent-path resolution is capped at 64 levels.
- No `unsafe`. The public API uses bytes throughout. `deleted_entries` takes the progress sink and cancel token.

## Concerns and deviations
1. **Sectors-per-cluster ≥ 0x80.** I implemented 2^(256−v) *sectors* (the cluster size is that many sectors × bytes per sector), not 2^(256−v) *bytes* as the context note suggested. This is the real on-disk rule (ntfs-3g does `1 << (256 - spc)` sectors), and Task 7's `detect.rs` uses the same rule. Under the "bytes" reading, 0xF4 would mean 4 KiB clusters, when it actually encodes 2 MiB clusters on 512-byte sectors.
2. **`open()` dispatch is in `traits.rs`, not `lib.rs`.** I added the one-line `FsKind::Ntfs` arm there. The file was clean in the working tree when I committed, and the line is covered by `ntfs_lists_root_and_reads_resident_file`, which calls `pm_fs::open`. If Task 7's reviewer would rather own that line, revert it; nothing else depends on it.
3. **`decode_runs` is `pub`, not `pub(crate)`.** The integration test `decode_runs_examples` in `tests/ntfs.rs` needs to reach it, and `pub(crate)` would hide it from integration tests.
4. **`used_map().len` vs `total()`.** The map length is `(total_sectors+1)*bps` clipped to `dev.len()`, while `total()` returns `total_sectors*bps`, the same value `detect` reports. Clone code should use `map.len` for the copy extent.
5. **Untested `$MFT` bootstrap path.** Bootstrapping an `$MFT` whose own `$DATA` sits behind an attribute list is implemented but not covered by an integration test. The brief's layout keeps records 0–7 in fragment 1, which leaves no free record there to hold the extension. The generic attribute-list path is covered through split.dat.
6. **Things deliberately left out:**
   - Compressed data is reported `Unsupported`, as the brief asks.
   - The `$I30` `$BITMAP` is ignored; the walk only follows live tree pointers.
   - Record numbers @0x2C are not cross-checked.
   - A `deleted_entries` batch is 1024 × record size, so 1–4 MiB for normal record sizes.
7. clippy is not installed for the `stable-x86_64-pc-windows-gnu` toolchain, so it was not run.

## Fix round 1 (commit `ce57f3b`)
Only these files were touched: `src/ntfs/fs.rs`, `tests/ntfs.rs` and `tests/ntfs_builder.rs`, each staged individually.

1. **Short `$Bitmap` under-marked clusters.**
   - `used_map` now copies only the bytes that lie inside both the real and the initialized size of `$Bitmap` (clipped to `ceil(total_clusters/8)`).
   - Every cluster the bitmap does not describe is marked USED; the function does not return an error.
   - Test (in `ntfs_used_map_from_bitmap`): two builder variants, one with `bitmap_real = 256` and one with `bitmap_init = 300`. Each asserts that every cluster ≥ covered×8 is used and that the covered clusters match the bitmap exactly.
2. **Unbounded MFT record count.**
   - `record_count()` is now `min(real, bytes mapped by the $MFT runs, volume bytes) / record size`.
   - Opening a volume whose `$MFT` real size exceeds its allocated size now gives `Error::Parse`. To support this check, `Stream::NonResident` now carries `alloc`.
   - Tests (in `ntfs_deleted_record_listed`):
     - A variant with `$MFT` real and allocated sizes both set to 2^60. `deleted_entries` returns the same two entries with progress total 32. A progress-sink guard cancels the scan if the total ever exceeds 32, so the test fails fast instead of hanging; before the fix it failed with `Cancelled`.
     - A variant with real size > allocated size: `open_ntfs` gives `Err(Parse)`.
3. **`end_vcn` ignored when merging extents.**
   - `Stream::from_extents` now requires the extents to tile the VCN space. Each extent must start at the previous extent's `end+1`, and its runlist must cover exactly `start_vcn..=end_vcn`. The wrapping arithmetic accepts the empty-attribute form `end_vcn = -1`.
   - A gap, an overlap, an inverted range, or a runlist that is too short or too long gives `Error::Parse`. Gaps are no longer filled with sparse holes. Sparse runs inside one extent still work.
   - Builder change: split.dat now has three extents (records 19, 27 and 30; `SPLIT_SIZE` = 10000). A variant `split_drop_middle_extent` leaves the middle extent out of the attribute list. I used the middle rather than the last extent because a missing tail already failed at read time with "VCN not mapped"; only a missing middle extent read back as silent zeros.
   - Tests:
     - `ntfs_attribute_list_followed`: with the middle extent dropped, `read_file` at offsets 0, 4096 and 8192 all give `Err(Parse)`, and sparse.bin still reads correctly from the same image.
     - The full read and the read across all three extents still pass.
     - New unit test `ntfs::fs::tests::extents_must_tile_their_vcn_ranges`.

**RED** (after the tests were written, before the fixes): `cargo test -p pm-fs --test ntfs` gave `FAILED. 4 passed; 3 failed`:
- `ntfs_attribute_list_followed` failed with "offset 0": silent success on the damaged file.
- `ntfs_deleted_record_listed` failed with "bounded scan: Cancelled".
- `ntfs_used_map_from_bitmap` failed with "cluster 2048, $Bitmap covers 256 bytes: left false".

**GREEN.** A plain `cargo test -p pm-fs` currently fails to compile, but only because of another agent's untracked, in-progress targets: `tests/exfat.rs` gives "unresolved import pm_fs::exfat" and `tests/ext_builder.rs` gives "[u8; 60]: Default". So I ran every committed pm-fs target explicitly:
`cargo test -p pm-fs --lib --test ntfs --test ntfs_builder --test fat --test fat_corrupt --test bitmap --test detect`
```
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s   (lib)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (bitmap)
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s   (detect)
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s  (fat)
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s  (fat_corrupt)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s   (ntfs)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s   (ntfs_builder)
```
There are 0 warnings.
