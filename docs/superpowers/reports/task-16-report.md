# Task 16 report: pm-recover

Status: DONE_WITH_CONCERNS. Commit: f173060 `feat(recover): partition finder, deleted files, carver, sessions` (crates/pm-recover + Cargo.lock only).

## Implemented
- `partitions.rs`: `find_partitions` (quick = surviving backup-GPT entries minus those the table already has, plus a probe at every 1 MiB step inside `Table::free_ranges(MIB)`, skipping past a found filesystem; full = `search_lost` + `probe_for_search`, step 1 MiB). Post-processing: drop exact matches of existing entries (same start, len within 1 MiB), drop candidates not starting in free space, cap `len` to the gap and subtract 20 confidence, sort by start. `apply_candidates` (type mapping per brief, name = label truncated to 36 UTF-16 units, fresh GUID on GPT, lowest free number in start order, Invalid on overlap with existing/other chosen/extended container, no room, or failed `validate()`).
- `files.rs`: `deleted_files`, plus `deleted_files_at(fs, fs_offset, ..)` (the generic `deleted_files` cannot know where the fs lives, so it records 0); integrity 100 (empty) / 80 / 40 / 10 / 0 (directory) by reading the first and last `min(4 KiB, size, cluster)` bytes; used-map penalty skipped and documented. `restore_file` (sanitised name, leading `?` stripped, reserved device names prefixed, `_N` suffix on collision via `create_new`, partial file removed on failure, dest dir created).
- `carve/signatures.rs`: 30 signatures; `Signature` gained `secondary: Option<(usize,&[u8])>` and `rule: LengthRule {Footer, MaxOnly, Riff, Mp4Atoms, ZipEocd}`.
- `carve/scanner.rs`: 4 MiB chunks, overlap = longest header check + 16, first-byte lookup grouped by header offset, sector-aligned (absolute) starts, any sector >= 1. Results sorted by offset. Same-type hits inside a *complete* hit are suppressed (nested zip, thumbnails); different types all kept. `extract_carved` -> `dest/<offset:012x>.<ext>` (ext sanitised to alphanumerics, else `bin`).
- `session.rs`: `ScanSession::{new, save (tmp + rename), load, matches}`, `DeviceIdentity::from_disk`, ISO-8601 UTC `created` (no chrono).

## Tests (cargo test -p pm-recover: 3 unit + 13 carve + 5 files + 9 partitions + 4 session, all green, zero warnings)
Brief names present: `find_partitions_after_table_wiped`, `deleted_fat_file_restored_contents_match`, `carve_finds_jpeg_png_pdf_zip_in_synthetic_disk`, `carve_truncates_at_max_len`, `session_roundtrip_json`; plus quick/backup, gap cap, existing-entry, apply (MBR/GPT types, overlaps), RIFF/MP4/ZIP-lookalike, any-offset + chunk straddle, cancel, read-only device, etc.
RED/GREEN: tests were written against `todo!()` stubs first, but I did not capture a clean all-red run. Real failures seen and fixed on the way to green: wrong expected ISO date in a unit test; FAT deleted names lose their first character (`?one.txt`, not `?gone.txt`) so my test expectations were wrong; gap-cap test exposed a real bug (an existing-entry backup-GPT candidate shadowed an overlapping lost candidate in dedupe) fixed by filtering existing-entry matches before dedupe.
Deviation: the image is 100 MiB, not 64 MiB, because `format_fat32` needs >= 33 MiB per volume. Candidate confidence for boot-sector hits is 70 (not 80 from the brief text; per your instruction >= 70).

## Self-review
- No writes: only `read_at` on the device (tests run carve/find through `ReadOnly`). Output files are written only to caller-supplied dirs.
- Bounds: every parsed length is clamped to `min(max_len, device remainder)`; RIFF size, MP4 box sizes (incl. 64-bit), ZIP comment lengths all checked; MP4 walk capped at 1M boxes; extension from sessions sanitised; extract range validated against device length.
- Cancel: per 4 MiB chunk in carve, per 1 MiB block in footer/EOCD searches, per probe in find_partitions, per entry in deleted_files, per 1024 MP4 boxes.

## Concerns
1. pm-fs `FatFs::read_file` follows the FAT chain, and a real FAT delete frees the chain, so multi-cluster deleted FAT files cannot be restored (integrity reports 40; restore returns an error). Only single-cluster files, or files whose chain survived, restore. A contiguous-run fallback belongs in pm-fs (out of scope here).
2. Full scan uses `search_lost`, which dedupes by confidence internally: a surviving backup-GPT entry (90) can shadow an overlapping boot-sector hit (70). Quick mode filters existing-table entries first and does not have this problem.
3. Partitions starting below 1 MiB (e.g. legacy LBA 63) are not searched; logical partitions inside an MBR extended container are not recreated (apply returns Invalid); `Candidate.len` for boot-sector hits is the filesystem length, not necessarily the original partition length.
4. Carver limits: JPEG/GIF/PDF end at the first footer (EXIF thumbnails, incremental PDF updates end early); BMP/7z/RAR/GZIP/MP3/OLE2 etc. are MaxOnly (always incomplete, up to max_len); a ZIP with no consistent EOCD scans up to 2 GiB before falling back to the first EOCD seen.
5. `pm_fs::Entry.id` is treated as opaque. CRLF warnings from git autocrlf only.

## Fix round 1

1. Structure-aware file ends (new `LengthRule::JpegMarkers`, `PdfLastEof`, `GifBlocks`; `complete` is now only true when the end was actually derived):
   - JPEG: marker-segment walk from SOI, skipping segments by length (so an EXIF thumbnail inside APP1 is skipped), then entropy-coded data after SOS scanned for the next real marker (FF 00, RSTn, FF fill are data); top-level EOI ends the file. Broken structure falls back to the first FF D9 with `complete = false`; a structure that is fine up to the cap is `(cap, false)`.
   - PDF: length = the LAST `%%EOF` in the window, with one deviation from the request: the window stops at the next `%PDF-` header, otherwise back-to-back PDFs would be merged into the first one. No `%%EOF` gives an incomplete file up to that window end.
   - GIF: header, logical screen, global color table, then image descriptors/extensions with sub-blocks up to the `3B` trailer. Broken structure falls back to the first `00 3B`, incomplete.
2. Read errors: `carve` now returns `CarveReport { files, unreadable, bytes_scanned }` (`SessionKind::Carved(CarveReport)`). A failing chunk read is retried in 64 KiB pieces, then per sector; failed sectors are zero-filled and listed (sorted, merged). A read error while measuring a file makes that file incomplete and records the range. `pm-image` (feature `testing`) is a dev-dependency for `FaultyDevice`.
3. Block-boundary tests: PNG IEND, ZIP EOCD and JPEG entropy markers placed on both sides of the 1 MiB `find_forward` / `scan_entropy` block boundary; ZIP larger than two blocks with exact length.
Also: pm-fs now (commit c6cb5a3, by another agent) reads deleted FAT files as a contiguous run, which made my "freed chain is unrecoverable" fixtures wrong; tests updated (LOST.BIN now restores and has integrity 80; new EDGE.BIN, whose run leaves the volume, covers integrity 40 and the restore-failure path). Concern 1 above is therefore resolved.

New tests (carve.rs): `jpeg_ends_at_the_real_eoi_not_at_an_embedded_thumbnail`, `jpeg_with_broken_structure_falls_back_to_the_first_footer_as_incomplete`, `pdf_ends_at_the_last_eof_before_the_next_pdf`, `gif_ends_at_the_trailer_even_when_lzw_data_contains_the_footer_bytes`, `png_footer_straddling_the_search_block_boundary_is_found`, `zip_eocd_straddling_the_search_block_boundary_is_found`, `zip_larger_than_several_search_blocks_has_the_exact_length`, `jpeg_entropy_data_longer_than_a_block_is_walked`, `carve_survives_a_bad_sector_and_reports_it`, `adjacent_bad_sectors_are_merged_and_cover_chunk_overlaps`, `a_bad_sector_inside_a_file_makes_it_incomplete_instead_of_aborting`. `different_types_may_overlap_but_nested_same_type_is_suppressed` now uses a JPEG with an embedded thumbnail.
RED check: with the JPEG/GIF/PDF rules temporarily switched back to plain `Footer`, 5 tests fail (pdf last-EOF, jpeg thumbnail, gif trailer, jpeg broken-structure incomplete, nested suppression); restored afterwards.

Command: `cargo test -p pm-recover` (zero warnings)
```
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.91s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
