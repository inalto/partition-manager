# Task 11 report: pm-image rescue copier (ddrescue semantics)

**Status:** DONE. Commit `8393197` on `dev`: `feat(image): ddrescue-style rescue engine with mapfile`.

## What was implemented

New module `crates/pm-image/src/rescue/`:

- `mapfile.rs`: `BlockStatus` (with `as_char`/`from_char`), `MapStats`, `MapFile { current_pos, current_status, pass, blocks }`.
  - `new`, `len`/`is_empty`, `parse`, `load`, `save` (atomic: write `<path>.tmp`, then rename), `save_with_comments`/`render` (extra `# ...` header lines), `mark`, `ranges`, `stats`, `merge_coalesce`, plus a crate-private `mark_many` (one linear pass for bulk used-map marking).
  - `to_string()` comes from `impl Display`. It writes the GNU 1.2x layout: the `# Mapfile. Created by pm (Partition Manager)` header, then the status line `0x%08x     c               pass`, then the `#      pos        size  status` header and the blocks as `0x%08x  0x%08x  c`, all lowercase hex.
  - `parse` accepts:
    - any number of comment lines (`#` starts a comment anywhere on a line), CRLF, trailing whitespace and blank lines;
    - a 2-field status line (an old-style file with no `current_pass`, which defaults to 1);
    - numbers written as `0x`/`0X` hex, leading-`0` octal or decimal (like strtoull with base 0);
    - current_status `F` or `G`, both read as `+`.
  - `parse` also normalises and rejects:
    - zero-size blocks are dropped;
    - gaps (including one before the first block) become `?`;
    - overlapping or unsorted blocks, bad status characters, overflowing `pos+size` and a missing status line are `Error::Parse`;
    - the result is coalesced.
  - `mark` uses a binary search, splices in at most 3 blocks and coalesces only the neighbours around the change. That keeps the invariant: blocks are non-empty, contiguous, cover [0, len) and are coalesced.
- `engine.rs`: `RescueOpts` (`Default`: 128 KiB block, 512-byte sector, 64 KiB skip, 0 retries), `RescueStats` (Serialize/Deserialize), `rescue`, `copy_simple`.
  - **Copy phase.** It runs 3 passes, as GNU ddrescue does. Pass 1 goes in the copy direction with skipping, pass 2 in the opposite direction with skipping, and pass 3 in the copy direction without skipping, so every byte is eventually tried.
    - Chunks are aligned to the `block_size` grid and clipped to the range.
    - A read error marks the chunk `*` and jumps ahead (or back) by `skip`, leaving the skipped bytes `?`. `skip` then doubles, up to `max(len/100, 64 KiB)` aligned to the sector (and never below the initial skip). It resets after a good read.
    - Reverse mode iterates the ranges from the end, reading chunks downwards.
  - **Trim** (unless `no_trim`): for each `*` range, it reads sector by sector from the front until the first error, which is marked `-`. Then it reads from the back down to just above the front error, until the first error (also marked `-`). The middle between the two becomes `/`. If the back loop finds no error, the middle is all `+`.
  - **Scrape** (unless `no_scrape`): every sector of every `/` range is read; a good read becomes `+`, a failure `-`.
  - **Retry**: up to `max_retries` passes over the `-` ranges, sector by sector. Pass 1 runs against the copy direction, and the direction alternates after that. The loop stops early when no `-` is left. `passes` is the number of passes run.
  - **Other behaviour:**
    - `abort_on_error`: the chunk is marked `*`, the mapfile is saved, and the source's `Error::Io` is returned.
    - Only `Error::Io` from the source counts as a read error. Any other source error, and any destination error, is fatal.
    - Phases with no work are not entered.
  - **Resume:** an existing mapfile is loaded and must cover exactly `src.len()`, otherwise `Error::Invalid`. An empty or whitespace-only file is treated as a fresh map. A file that does not parse returns `Error::Parse` and is never overwritten, so earlier rescue progress is not destroyed.
  - **Saving:** the mapfile is saved every 30 s (checked before each read), at every phase start, on any error or cancel (best effort, and the original error is returned), and at the end. Before each save the destination is flushed, so `+` never claims data that is not durable.
  - **Cancel:** checked before every read (each read is at most one 128 KiB block) and before every 1 MiB of zero-fill.
  - **Used map:** it becomes sector-widened used ranges; bytes past `used_map.len` count as used, and the complement is the unused areas.
    - Unused areas are marked `+` through `mark_many`.
    - With `zero_unused`, only the areas whose status changes are zeroed in `dst`, and they are zeroed *before* they are marked. A cancel during zero-fill therefore never saves `+` over areas that were not zeroed.
    - The mapfile gets the comment `# Note: used-map applied; unused areas are marked finished without being read (...)`.
  - **Progress:** the phases are "Copying", "Trimming", "Scraping" and "Retrying (pass N)", with `total` = len − unused and `done` = rescued − unused. `detail` = `errsize X, errors N, rate Y/s` (errsize = `*` + `/` + `-` bytes), refreshed at most every 250 ms and at once after an error. A final report is always sent, even when a finished mapfile means there was nothing to do.
  - `copy_simple` copies in 4 MiB chunks:
    - `used` offsets are relative to `src_off`;
    - unused areas are neither read nor written;
    - it aborts on the first error, checks cancel per chunk, reports phase "Copying" and returns the bytes copied.
- `faulty.rs` (`#[cfg(any(test, feature = "testing"))]`): `FaultyDevice::new(MemDevice, bad, transient)`.
  - Any read whose range touches a bad sector fails as a whole with `Error::Io{offset, len, "simulated bad sector"}`. Each read touching a transient sector uses up one of its failures.
  - Inspection helpers: `read_count()`, `reads()` (an `Arc<AtomicU64>`), `read_log()`, plus `write_count()`, so tests can prove the source is never written.
  - Writes pass through to the inner `MemDevice`.
- `mod.rs` re-exports everything, plus `pm_fs::UsedMap`. `lib.rs` adds `pub mod rescue;` and `pub use rescue::{copy_simple, rescue, BlockStatus, MapFile, MapStats, RescueOpts, RescueStats};`.
- `Cargo.toml`:
  - adds `pm-fs = { path = "../pm-fs" }` and `[features] testing = []`;
  - adds a self dev-dependency `pm-image = { path = ".", features = ["testing"] }`, so that plain `cargo test -p pm-image` integration tests see `FaultyDevice`;
  - the `Cargo.lock` change is only the matching 2 lines.

## Stats semantics (decided here)
- `rescued` is the number of finished bytes in the final map, minus the unused bytes skipped through the used map in this run. It includes bytes rescued by earlier runs of a resumed map.
- `bad_bytes` is every byte not rescued: `-`, plus any `?`, `*` or `/` left over (for example with `no_trim` or `no_scrape`). So `rescued + bad_bytes + unused == len`.
- `bad_areas` is the number of maximal unrescued runs.
- `MapStats.bad_bytes` and `MapStats.bad_areas` follow GNU: they count only `-`.

## Tests (TDD evidence)
- **RED, mapfile:** the stubbed `MapFile` (`unimplemented!()`) gave `test result: FAILED. 0 passed; 7 failed` for `--test mapfile`.
- **GREEN, mapfile:** after implementing it, `7 passed`.
- **RED, engine:** the stubbed engine gave `test result: FAILED. 0 passed; 13 failed` for `--test rescue`, every test panicking in the `engine.rs` stub.
- **GREEN, engine:** after implementing it, `13 passed`. One more test was then added, for 14 in total.
- **Final run:** `cargo test -p pm-image` gives lib 9, formats 18, mapfile 7, rescue 14 and doctests 0, all passing with **0 warnings**.
- `tests/mapfile.rs`:
  - the brief's tests: `mapfile_roundtrip_gnu_format` (the literal GNU 1.27 sample, verbatim) and `mapfile_stats`;
  - extra tests: `mapfile_parse_tolerates_crlf_trailing_spaces_and_missing_pass`, `mapfile_parse_rejects_garbage`, `mapfile_parse_fills_gaps_as_non_tried`, `mapfile_merge_coalesce` and `mapfile_save_load`.
- `tests/rescue.rs`:
  - the brief's tests: `rescue_clean_source_copies_all`, `rescue_bad_sector_isolated_to_one_sector`, `rescue_permanent_bad_sector_terminates`, `rescue_transient_error_recovered_on_retry`, `rescue_resume_from_mapfile_skips_finished`, `rescue_reverse_direction`, `rescue_used_map_skips_unused_and_zero_fills` and `copy_simple_aborts_on_error`;
  - extra tests: `rescue_scrape_marks_middle_sectors`, `rescue_no_trim_leaves_non_trimmed`, `rescue_abort_on_error_saves_mapfile`, `rescue_cancel_saves_consistent_mapfile` (cancel before the first read, and mid-copy through a wrapper that cancels during its 5th read, then resume), `rescue_rejects_small_destination` and `rescue_odd_length_tail_sector`.
- Unit tests: `faulty_fails_whole_reads_touching_bad_sectors`, `mark_many_splits_and_reports_changes` and `used_ranges_widen_to_sectors_and_cover_tail`.
- Notable assertions:
  - **Isolated bad sector:** exactly one `-` block of 0x200, and the map is exactly `+ / - / +`. Exactly 2 reads touch the bad sector (one copy read, one trim read).
  - **Permanent bad sectors:** the last 6 reads, which belong to the retry passes, follow the expected alternating order.
  - **Resume:** the second run makes 0 reads and leaves `dst` byte-identical. A partly finished map is read only past 2 MiB. A mapfile of the wrong size gives `Invalid`.
  - **Used map:** no read touches an unused area, including a bad sector placed there. Unused bytes come out zero when `zero_unused` is set and keep their `0xEE` otherwise.
  - **Source never written:** `src.write_count() == 0` everywhere it is checked.

## Self-review
- **Phase transitions:** copy runs only if `?` exists; trim only if `*` exists and `!no_trim`; scrape only if `/` exists and `!no_scrape`; retry runs N passes only while `-` exists. Each `start_phase` sets `current_status` (`?`, `*`, `/`, `-`) and `pass`, finishes the previous tracker and saves. At the end the status is set to `+`, and the run sends a final report, flushes and saves.
- **Termination:** every loop works on a snapshot of the ranges and moves strictly forward or backward. The pass-3 copy cannot skip. Retry is bounded by `max_retries`.
- **Mapfile invariant:** it holds after every change:
  - `mark` (splice plus local coalesce) and `mark_many` (a linear rebuild) preserve contiguity and coalescing;
  - `parse` fills gaps and coalesces;
  - the tests assert the invariant after rescue, abort and cancel.
- **No source writes:** `src` is used only through `len()` and `read_at` in `engine.rs` (checked by grep), and there is no `flush` on the source.
- **Fixed in review:** the used-map zero-fill originally marked areas `+` before zeroing them, so a cancel in between could save stale areas as finished. It now zeroes first and swaps in the marked map afterwards.

## Concerns and omissions
1. **`min_read_rate` is implemented but not tested.** It needs at least 5 s of wall-clock history, and `FaultyDevice` has no delay injection. I also changed the brief's semantics on purpose: the slowly read block is kept, written and marked `+`, and only the skip-ahead is triggered (as GNU does), rather than marking the block `*`. Throwing away data that was read correctly only means reading it again later.
2. **The copy phase is GNU's 3-pass scheme, not a single sweep.** Without it, bytes skipped in pass 1 would stay `?` forever, because trim and scrape only look at `*` and `/`.
3. **A mapfile that exists but does not parse is an error,** not a silent fresh start. This protects earlier rescue progress. An empty file is treated as fresh.
4. **No `rustfmt` or `clippy` in the installed toolchain** (`stable-x86_64-pc-windows-gnu`), so formatting was not machine-checked. Some test lines are over 100 columns.
5. **The self dev-dependency makes Cargo build the crate's lib twice for tests.** That is normal for this pattern, and no conflicting types showed up because the integration tests only use `pm_image::*`.
6. **A transient `formats` test failure appeared once** (`qcow2_autoclear_features_cleared_on_write`) while the concurrent Task 10 fix (commit fe71659) was still uncommitted. It passed on the next run and has nothing to do with this task.
7. **`RescueStats.rescued` subtracts unused bytes only when `used_map` is passed in the current run.** A resume without the used map counts the unused areas as rescued.

## Fix round 1 (commit 2598ac5)

1. **(Important) Progress for zero-filling unused areas.** `apply_used_map` now runs as its own progress phase, "Zeroing unused".
   - `total` is the number of bytes actually being zeroed: only areas whose status changes, so a resumed run never zeroes the same area twice.
   - It writes in chunks of at most 4 MiB (`ZERO_CHUNK` = 4 MiB), calls `tracker.add(n)` per chunk, checks cancel per chunk, and calls `finish()` at the end.
   - Areas are still zeroed *before* the map marks them `+`.
   - Test `rescue_zero_unused_reports_progress`: a "Zeroing unused" report exists, its last report has `done == total == unused bytes`, it comes before "Copying", and the last "Copying" report covers only the used bytes (done == total == 1 MiB).
2. **(Important) Durable mapfile saves.** `save_with_comments` now uses `File::create(<path>.tmp)`, then `write_all`, `sync_all`, `drop`, and finally `rename` over the path.
   - In `rescue()`, an existing mapfile that is not UTF-8 or does not parse (for example the NUL-filled file a power loss can leave) returns `Error::Parse("mapfile <path> is unreadable (<why>); delete or move the mapfile to start over")`. The file is not touched and nothing is read.
   - Test `rescue_corrupt_mapfile_is_reported_not_overwritten` covers a 300-byte NUL file and invalid UTF-8. It checks the path and the hint in the message, that the read count is 0, and that the file is unchanged.
   - `mapfile_save_load` now also checks that saving a second time replaces the file and that only `x.map` remains in the directory (no `*.tmp`). The fsync itself cannot be observed in a test.
3. **(Minor) Device sector size.** The effective sector is `max(opts.sector, src.sector_size())`. `FaultyDevice::sector_size()` now reports the inner device's sector size; bad offsets still have 512-byte granularity.
   - Test `rescue_uses_device_sector_size_4kn` uses an inner `MemDevice::with_sector_size(4096)` and a bad sector at 1 MiB+512, with `opts.sector` left at 512.
   - Result: `bad_bytes == 4096`, and every read smaller than 64 KiB is a 4096-byte read at a 4096-aligned offset.
4. **(Minor) Block cap.** `block = (min(block_size, 64 MiB) / sector * sector).max(sector)` (`MAX_BLOCK`). The read buffer is sized `min(block, len)`.
   - Test `rescue_block_size_clamped`: `block_size = usize::MAX` on the 4 MiB source makes a single read `(0, 4 MiB)`.
   - Before the fix, this test failed with a `capacity overflow` panic.

**RED evidence.** I stashed the fixes by reverting `src/rescue/` to 8393197 and ran `cargo test -p pm-image --test rescue --test mapfile`. Result: `test result: FAILED. 14 passed; 4 failed` in rescue, with the 4 new tests failing (including the `capacity overflow` panic); mapfile gave `test result: ok. 7 passed`. The tmp-file check also passes on the old code, because the rename already removed the temp file.

**GREEN** (`cargo test -p pm-image`, 0 warnings):
```
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s   (lib)
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.14s   (formats)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s    (mapfile)
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.53s   (rescue)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s    (doctests)
```
Only my 5 rescue files were staged. The working-tree `Cargo.lock` change belongs to another agent and was left unstaged.
