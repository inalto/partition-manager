# Task 3 report: pm-parttable (MBR)

Status: DONE (commit e1ecd75 on dev, `feat(parttable): MBR parse/write`)

## Implemented
- `crates/pm-parttable`: `Cargo.toml` (pm-core path dep, serde, uuid; dev: serde_json, all via workspace), `src/lib.rs`, `src/bytes.rs` (rd_/wr_ u16/u32/u64), `src/table.rs`, `src/mbr.rs`, `tests/mbr.rs`.
- `Entry`, `Table` exactly as in the brief (plus `Entry::end()`). `Table::{empty, free_ranges, validate, next_number, to_disk_info, regenerate_ids}`, `read_table`, `write_table`.
- `mbr::{parse_mbr, write_mbr, is_protective_mbr, chs_from_lba}`; all also re-exported at the crate root with `Entry/Table/read_table/write_table`. `mbr::has_signature` is pub(crate).
- `read_table`: no 0x55AA -> `Table::empty(None)`; protective MBR -> `Err(Unsupported("GPT"))` with `// Task 4: dispatch to gpt::parse_gpt` markers (one at the top, one at the protective branch). `write_table`: Mbr -> write_mbr, Gpt -> Unsupported (`// Task 4` marker), None/Unknown -> Invalid.

## Decisions / interpretations
- Primary numbering is by slot (slot+1); `write_mbr` puts a primary at slot `number-1` when 1..=4 and free, else first free slot. Logicals are numbered 5.. in chain order on parse; on write they are sorted by start and their numbers are ignored.
- `next_number()` = lowest unused positive number.
- First EBR is always at the extended start (parser requirement); later EBRs are at `logical.start - 1 MiB`. Errors (`Invalid`) if any logical starts < ext.start + 1 MiB, or if a later EBR would overlap the previous logical. Link entry type reuses the extended container's type byte. Extended with no logicals gets a blank (signature-only) EBR at its start, so stale data cannot resurrect logicals.
- `write_mbr` read-modify-writes sector 0 (keeps reserved bytes 444..446 and anything past 512 on 4Kn), writes EBRs first, sector 0 last, then flush. It refuses (before any write) on invalid table, sector-size mismatch with the device, bootcode > 440, EBR beyond device.
- `validate` extras beyond the brief: MBR entries must have `PartitionType::Mbr(non-zero)`, must fit 32-bit LBA; GPT: no logicals, no overlaps; None/Unknown: no entries. It does NOT check GPT entries against gpt_first/last_usable and does not check the GPT ptype variant (left to Task 4).
- `free_ranges` splits at the extended boundary (a gap never straddles the container); gaps inside the extended are raw (ext.start..first logical etc., not reduced for the 1 MiB EBR reservation). `gpt_last_usable` is treated as an exclusive end in bytes. Kind None uses the MBR bounds.
- `parse_mbr` leniency: EBR chain stops silently on missing 0x55AA, empty link entry, repeated offset or 128 logicals; an EBR offset beyond the device is `Err(Parse)`. An empty logical slot in an EBR with a valid link is skipped but the chain continues. Entries with type 0 or len 0 are treated as empty (dropped on rewrite).
- `to_disk_info` maps only MBR active; GPT attribute bits are left for Task 4.

## Tests (crates/pm-parttable/tests/mbr.rs, 26 + 1 unit test in bytes.rs)
Brief's six: mbr_parse_four_primaries, mbr_parse_extended_chain_two_logicals, mbr_roundtrip_write_then_parse_equal, mbr_preserves_bootcode_and_signature, mbr_rejects_no_55aa, mbr_overlap_validate_fails. Plus: slot numbering, EBR loop guard, extended without EBR/logicals, 4Kn roundtrip, primaries-by-slot, CHS values/clamp, status+CHS+LBA bytes, protective detection (incl. hybrid), read_table/write_table dispatch, validate rules (10 cases), too-close logicals rejected with device untouched, free_ranges (MBR+extended, GPT), empty defaults, next_number, to_disk_info, regenerate_ids, serde roundtrip.
- RED: with `unimplemented!()` stubs, `cargo test -p pm-parttable`: 26 failed (only bytes unit test passed).
- GREEN: `test result: ok. 1 passed` (unit) and `ok. 26 passed` (tests/mbr.rs). Zero warnings in `cargo test -p pm-parttable` and `cargo build --workspace`.
- clippy/rustfmt are not installed for this toolchain (stable-x86_64-pc-windows-gnu), so neither was run.

## Files changed
crates/pm-parttable/{Cargo.toml, src/lib.rs, src/bytes.rs, src/table.rs, src/mbr.rs, tests/mbr.rs}, Cargo.lock (+pm-parttable package). Nothing in pm-core, .cargo or rust-toolchain touched.

## Concerns
- Stale EBRs from a previous layout (other than the one at the extended start) are not cleared on write; clearing them risks zeroing sectors that now belong to a partition.
- Gaps reported inside the extended partition do not subtract the 1 MiB EBR space, so a caller creating a logical at a raw gap start can hit the `write_mbr` Invalid error; the UI/ops layer should align logical starts to >= gap start + 1 MiB.
- Git warns about LF->CRLF on the new files (autocrlf), harmless.
