# Task 17 report: pm-analyze + Windows BenchTarget / smart()

Status: DONE (CLI bullet skipped as instructed).
Commits: `8851990 feat(analyze): space, surface, bench, SMART`, `c24d9f3 feat(platform): bench target and SMART`.
Note: the first commit alone does not compile on Windows (pm-analyze's `#[cfg(windows)] impl BenchTarget for FileBenchTarget` and one test need the second commit); HEAD is fine.

## Implemented
pm-analyze (`crates/pm-analyze`): `space.rs` (`scan_space`, `Node`, `SpaceReport`), `surface.rs` (`surface_test`, `SurfaceReport`, `DEFAULT_BLOCK`), `bench.rs` (`benchmark`, `BenchOpts` + `Default`, `BenchMode`, `BenchPoint`, `BenchTarget`), `smart.rs` (`smart_summary`, `Health`).
pm-platform: `windows/bench.rs` (`FileBenchTarget`, `BenchFile`), `windows/smart.rs` (`smart()`, `parse_nvme_smart_log`, `parse_ata_smart`, both re-exported), `MockPlatform::set_smart`, live ignored test `live_smart_disk0`. No new `unsafe` anywhere (std `OpenOptionsExt::custom_flags`/`FileExt`, existing `ioctl::ioctl` wrapper), no new pm-platform deps.

## Tests (RED then GREEN)
RED: before any source existed `cargo test -p pm-analyze` failed to compile (missing `FileBenchTarget`, `pm_analyze` items). GREEN: `cargo test -p pm-analyze -p pm-platform` all pass, zero warnings (clippy is not installed for this toolchain).
- pm-analyze: space 5 (`scan_space_sizes_and_counts`, top-100, progress/cancel, missing root, junction not followed - ran, mklink worked), surface 3 (`surface_reports_bad_block_from_faulty_device` uses `pm_image::rescue::FaultyDevice`, slow-block, cancel/invalid), bench 5 (`benchmark_runs_on_mem_device_returns_points`, rounding+phases, validation/cancel, defaults, real unbuffered temp file), smart 2 (`smart_summary_flags_reallocated_sectors`, thresholds).
- pm-platform: 48 lib (new: 7 smart parser/layout/encoding tests incl. layout checks vs windows-sys, 3 bench file tests) + mock `mock_smart_returns_configured_data_per_disk`.

## Live SMART (disk 0, NOT elevated)
Works unelevated: NVMe query via `StorageDeviceProtocolSpecificProperty` on a query-only/read handle returned data: healthy=true, 30 C, 13623 power-on hours, percent used 10, available spare 100 (threshold 10), 69 power cycles, 24 unsafe shutdowns, 0 media errors. ATA path is covered by unit tests only (no ATA drive here; ATA SMART needs admin).

## Decisions / deviations (please review)
1. **Trait location**: `BenchTarget` lives in pm-analyze (per brief); pm-platform cannot depend on pm-analyze (cycle: analyze needs `SmartData`). So pm-platform exposes `FileBenchTarget::open_for_bench` as an inherent method and pm-analyze implements the trait for it (`#[cfg(windows)]`).
2. **NVMe attr ids** = byte offset of the field in the log page (0, 3, 32, 48, 112, 128, 144, 160), not all 0xFF, so ids are unique and cannot collide with ATA ids (5/187/197/198/199) used by `smart_summary`.
3. `healthy` for ATA is `None` when thresholds could not be read. `smart_summary` shows unknown health as `Warn` ("Unknown"), not Ok.
4. Surface `slow` = > 10x median of good blocks AND >= 1 ms (floor so RAM-speed jitter is not flagged). Bad blocks are excluded from median/slow. `ms_per_block` is truncated ms.
5. Benchmark: BlockDevice is synchronous, so effective concurrency = `threads`; `queue_depth` = requests issued back to back before cancel/progress check. `total_len` is split across workers (each worker gets its own device of ~total_len/threads, so disk use is total_len, not threads x total_len). Sizes rounded up to 4096; workers = min(threads, ops). Write timing includes flush (BenchFile flush is a no-op: WRITE_THROUGH).
6. `BenchFile` copies through its own 4096-aligned buffer (NO_BUFFERING needs aligned memory too), RMW for unaligned requests. Known: writes past the valid-data length on NTFS zero-fill, which can slightly depress first-write numbers.
7. `scan_space`: reparse points/symlinks are skipped entirely (not counted), including OneDrive-style placeholders; `dirs` = descendant directory count; tree built iteratively (no deep recursion). Root itself may be a junction (explicitly requested).
8. `Cargo.lock`: committed only the pm-analyze entry (pm-recover's entry from another agent is left unstaged).

## Files
Created: `crates/pm-analyze/{Cargo.toml,src/{lib,space,surface,bench,smart}.rs,tests/{space,surface,bench,smart}.rs}`, `crates/pm-platform/src/windows/{bench,smart}.rs`.
Modified: `crates/pm-platform/src/{lib.rs,mock.rs,windows/mod.rs}`, `crates/pm-platform/tests/{mock.rs,windows_live.rs}`, `Cargo.lock` (pm-analyze entry).

## Self-review / concerns
- No real ATA hardware to validate `SMART_RCV_DRIVE_DATA` layout beyond windows-sys offset checks; command bytes follow the standard SMART register values.
- Skipped: CLI (`pm analyze`, `pm recover`), per instruction.
- Benchmark default 200 MiB x 4 sizes x (w+r) on a real volume not run end-to-end (only a 512 KiB real-file run in tests).

## Fix round 1
Review finding (Important): random-mode write timing was biased because a freshly `set_len` file has valid data length 0 and NTFS zero-fills lazily inside the timed write pass.
- Fix (in `benchmark`, so it covers every target): new `warm_up` writes each worker device sequentially in 1 MiB chunks (untimed, then flush) before the first timed pass of each size; phase "Benchmark {size} warm-up"; not counted in IOPS/MB/s. Documented in `benchmark` docs and in `windows/bench.rs` module docs.
- New test `warm_up_writes_each_device_once_and_is_not_timed_or_counted` (counting `MemDevice`, each warm-up write sleeps 100 ms): asserts warm-up bytes == 256 KiB, counted transfers == 128, and write IOPS > 5000 (would be <= 640 if warm-up were timed). Existing counting test now counts only transfers <= 8 KiB.
- Minor: `smart_summary` skips the attribute row named "Power-on hours" (or id 9) when `power_on_hours` is set; test `smart_summary_does_not_repeat_power_on_hours`.
- Minor: `smart()` now retries each query per access mask (`query_with`: open, query, next mask on failure) instead of stopping at the first successful open; live NVMe SMART on disk 0 still returns data unelevated.
- Files touched: `crates/pm-analyze/src/{bench,smart}.rs`, `crates/pm-analyze/tests/{bench,smart}.rs`, `crates/pm-platform/src/windows/{bench,smart}.rs` (smart files are required by the two minors).
- Command: `cargo test -p pm-analyze -p pm-platform` (zero warnings). Results:
```
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 48 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.36s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
