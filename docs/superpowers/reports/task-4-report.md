# Task 4 report: pm-parttable GPT, conversion, lost-partition search

Status: DONE (with concerns below). Commit: `5e8420c feat(parttable): GPT, conversion, lost-partition search` on `dev`.

## Implemented
- `src/gpt.rs`: `parse_gpt` (primary at LBA1; on any signature/size/CRC/MyLBA/entry-array failure falls back to the backup at the last LBA; both bad -> `Error::Parse` with both reasons), `write_gpt`, `GptHealth`, `gpt_health`, `gpt_repair_backup_from_primary`, `gpt_repair_primary_from_backup` (parse the good side, rewrite both via `write_gpt`), crate-private `parse_gpt_backup` / `has_valid_primary_header`.
  - Reading accepts up to 1024 entries of size 128..4096 (multiple of 8), keeps only non-empty slots; `Entry.number` = slot+1; names cut at first NUL; mixed-endian GUIDs via `Uuid::from_bytes_le`/`to_bytes_le`; usable range converted to exclusive byte end.
  - Writing: 128x128 canonical array; entries go to slot `number-1`; Invalid for number 0/>128/duplicate, missing GUID, MBR type, name >36 UTF-16 units, bootcode length not 0/440, wrong sector size, usable range outside the structures, device too small. Write order: backup entries, backup header, primary entries, primary header, protective MBR last, then flush. Protective MBR keeps bootcode + signature, CHS 000200/FFFFFF, size clamped to 0xFFFFFFFF.
- `src/table.rs`: `read_table` GPT dispatch (protective MBR, or no 0x55AA with a valid header at LBA1), `write_table` -> `write_gpt`, GPT `validate` (<=128 entries, numbers 1..=128, no logicals, entries within `[gpt_first_usable, gpt_last_usable)`), GPT attribute flags in `to_disk_info` (bit0 required, 60 read_only, 62 hidden, 63 no_letter), `Table::empty(Gpt)` now: first usable = align_up(max(34 sectors, 1 MiB), 1 MiB), last usable = disk_len - 33 sectors rounded down to a sector, bootcode = 440 zero bytes (so write/read round trips compare equal).
- `src/convert.rs`: `mbr_to_gpt`, `gpt_to_mbr`, `align_table` per the lead's rules.
- `src/search.rs`: `Candidate`, `CandidateSource` (BootSector, BackupBootSector [documented as pm-recover's], GptBackupEntry), `search_lost`.
- Reviewer notes from Task 3: (a) `parse_mbr` now stops the EBR chain (keeps parsed logicals) when a link points past the device; (b) `write_mbr` returns `Invalid` for a bootcode that is not exactly 440 bytes (also rejects empty, since `Table::empty(Mbr)` always supplies 440).
- `Cargo.toml`: added `crc32fast`, updated description.

## Tests (RED -> GREEN)
- RED: with the three new test files written first, `cargo test -p pm-parttable --no-run` failed to compile (unresolved `parse_gpt`, `write_gpt`, `gpt_health`, `GptHealth`, `mbr_to_gpt`, `gpt_to_mbr`, `align_table`, `search_lost`, `CandidateSource`).
- GREEN: `cargo test -p pm-parttable`: 1 unit + 8 convert + 16 gpt + 28 mbr + 6 search = 59 passed, 0 failed, zero warnings. `cargo build --workspace`: zero warnings (pm-platform included, as of the run).
- Required names all present: gpt_roundtrip_512, gpt_roundtrip_4k_sectors, gpt_uses_backup_when_primary_corrupt, gpt_rejects_both_corrupt, gpt_protective_mbr_written, gpt_names_utf16, mbr_to_gpt_maps_types, gpt_to_mbr_rejects_five_entries, align_table_moves_starts, search_finds_fs_at_offset_with_injected_probe.
- GPT images are verified against independently hand-built bytes: header (signature at sector_size, revision, size 92, CRC via crc32fast over a header built field by field, MyLBA/AlternateLBA, usable LBAs, GUID, entries LBA, counts, array CRC), entry array at 2*sector_size, backup entries at last-16 KiB/sector, backup header in the last sector, for 512 and 4096 sectors. Extra tests: 3 TiB sparse device clamps protective MBR size, header CRC flip, slot/number handling, validate rules, flag mapping, health per side, repairs restore a byte-identical disk, GPT behind a wiped MBR, write input rejection and "nothing written on error", EBR link past device, short bootcode, search dedupe/progress/cancel/backup-GPT candidates.
- Existing tests changed in `tests/mbr.rs`: `protective_mbr...` now expects `Parse` (protective MBR with no GPT), `write_table_dispatch_errors` became `write_table_dispatch` (GPT now writes), and two Table::empty(Gpt) expectations changed from 34 to 33 tail sectors per the brief's decision.

## Files
Changed: `crates/pm-parttable/{Cargo.toml,src/lib.rs,src/table.rs,src/mbr.rs,tests/mbr.rs}`; new: `src/{gpt,convert,search}.rs`, `tests/{gpt,convert,search}.rs`; `Cargo.lock` (committed with only the crc32fast hunk; the working-tree lock still has the other agent's pm-platform changes uncommitted).

## Self-review / concerns
1. The brief says 128x128 entries = 32 KiB / 8 sectors at 4K; the correct size is 16 KiB (32 sectors at 512, 4 sectors at 4096). I implemented 16 KiB (standard); 4K entries are at byte 8192 as specified, first usable LBA >= 6.
2. `Table::empty(Gpt)` tail is 33 sectors regardless of sector size (as specified), which over-reserves on 4K disks (harmless).
3. `gpt_to_mbr` rejects disks > 2^41 bytes literally per the lead, even for 4K-sector disks where MBR could address more; the per-partition 32-bit LBA check is also applied.
4. `mbr_to_gpt` lowers `gpt_first_usable` to the 34-sector minimum when a legacy layout (e.g. first partition at sector 63) starts before 1 MiB, instead of failing; entries outside the range still fail. Mapped names longer than 36 units are truncated.
5. `align_table` leaves MBR extended containers unchanged (shrinking them could cut off logicals); deviation from "each entry", documented.
6. Search dedupe is greedy by confidence (ties: earlier start), which subsumes the "fully inside" and "partial overlap" rules. GPT backup entries are probed so fs/label from the probe replace Unknown. `BackupBootSector` is not produced (as instructed).
7. `read_table`: a protective MBR whose GPT is unreadable now yields `Error::Parse` rather than `Unsupported`. Hybrid MBRs are still treated as plain MBRs.
8. rustfmt/clippy are not installed in this toolchain, so formatting was done by hand and clippy was not run.

## Fix round 1
Changes (`crates/pm-parttable/src/gpt.rs`, `tests/gpt.rs`):
1. `read_header` now rejects a header revision other than 0x00010000 (after the CRC check) with a "GPT header revision ..." reason, surfaced as `Error::Parse` by `parse_gpt` (a bad primary falls back to the backup). Test `gpt_rejects_bad_revision`: patch the primary revision with a recomputed CRC, parse is rescued by the backup and `gpt_health().primary_ok` is false; patch the backup too, `Err(Parse)`. Mutation check: with the revision check disabled this test fails.
2. Both repair functions call `Table::validate` right after parsing and return `Error::Invalid("source GPT is readable but not writable: ...")` before writing anything. Documented on both repair functions and on `gpt_health` ("ok" means readable, not writable). Test `gpt_repair_refuses_invalid_source`: partition 2 patched to overlap partition 1 on both copies with array and header CRCs recomputed; `gpt_health` reports all ok, both repairs return the Invalid message, device bytes unchanged.
3. Test `gpt_corrupt_headers_never_panic`: header cases (num_entries u32::MAX and 0, entry_size u32::MAX and 100, entries LBA beyond the device and u64::MAX, header_size 4000 and 50, last usable beyond device, first usable > last usable) patched with recomputed CRCs. For each, patching only the primary is rescued by the backup (`read_table` ok), patching both gives `Err(Parse)` and `gpt_health` still returns Ok. Plus an entry with LastLBA < FirstLBA (array and header CRCs recomputed) on primary only, then on both.

Command: `cargo test -p pm-parttable`
Output: 1 unit + 8 convert + 19 gpt + 28 mbr + 6 search = 62 passed, 0 failed, zero warnings.
