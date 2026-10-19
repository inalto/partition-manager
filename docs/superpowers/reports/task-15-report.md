# Task 15 report: pm-cli apply / operation subcommands / recover / analyze, FilePlatform

Status: DONE (with the concerns below)

Commits (branch `dev`):
- ce09499 `feat(cli): apply and operation subcommands; file-backed platform`
- c1d3637 `feat(cli): recover and analyze commands`

Each commit builds and passes its tests on its own (the first was verified from an exported index in a separate target dir).

## What was implemented

pm-platform (additive only): `src/file_platform.rs` (`FilePlatform`, `NO_SYSTEM_DISK`), `pub mod` + re-export in `lib.rs`, dev-dependency `tempfile`.
- `FilePlatform::new(Vec<PathBuf>) -> Result<Self>`: `Invalid` for an empty list or a non-file, `NotFound` for a missing file. Disk i = file i; `open_disk` -> `pm_core::FileDevice` (raw only); `enumerate_disks` re-reads the table every call (model = file name, bus Virtual, 512-byte sectors, size = file length; a damaged table shows the disk without partitions); `enumerate_volumes` empty; `begin_write` -> session over a `FileDevice` whose `commit_table` = `write_table` + flush; mount/unmount/format/shrink/extend/run_tool -> `Unsupported("not available on image files")`; smart -> `Ok(None)`; `is_elevated` true; `free_letters` E F G; `set_offline`/`rescan` no-ops.
- `system_disk()` returns `Some(u32::MAX)` (`NO_SYSTEM_DISK`). Verified: `Model::effective_system_disk`, `Planner::guard_system_disk` and the executor guard compare indices only, so a known-but-matching-nothing value lets everything through. Documented in the module docs. `pm doctor` prints "none (image files)" for it.

pm-cli:
- Global `--disk-file PATH` (repeatable, `global = true`, so it may follow the subcommand): platform = `FilePlatform`; a bad path is exit 1.
- `cmd_apply.rs`: `pm apply FILE [--dry-run] [--yes] [--allow-system-disk]`. File = JSON array of `Operation` or a single operation object. Flow: print numbered summaries -> `Model::from_platform` -> `Planner::simulate_all` (error => exit 1 with the message) -> executor dry run (applies the system-disk guard and `precheck`; refusal => exit 1) -> `--dry-run`: print `would:` lines, stop -> no `--yes`: print `Re-run with --yes to apply`, exit 0 (no prompt) -> real `execute` with `StderrProgress`, log lines on stdout, failure => exit 2 (`CliError::Failed`, new variant).
- On `--disk-file`, operations that need a volume layer (create with --fs/--letter, format, set-letter, set-label, mount/unmount, clone/migrate with a boot fix) are refused up front with exit 1 "not available on image files" (otherwise the executor would wait 5 s in `locate_volume` and then fail after the table was already written).
- `cmd_ops.rs`: `init-disk --gpt|--mbr`, `create`, `delete`, `format`, `resize`, `set-active`, `set-label`, `set-letter` (letter or `none`), `set-type`, `wipe-disk`, `wipe-partition`, `convert-table`, `align`, `rebuild-mbr`, `clone-disk`, `migrate-os`, `image-disk`, `image-partition`, `restore-image`; all with `--dry-run/--yes/--allow-system-disk/--emit-json` (emit prints the single operation object; `apply` reads it back). Disk arg = `N` or `disk:N`. `--type` maps by the disk's table kind (GPT GUIDs / MBR ids; basic on MBR follows --fs; MSR on MBR is a usage error).
- `cmd_recover.rs`: `recover partitions DEVICE [--quick] [--apply [--yes]] [--min-confidence N] [--select 1,3] [--table gpt|mbr] [--allow-system-disk] [--session F]`; `recover files DEVICE[:P] [--dest D] [--min-integrity N] [--session F]`; `recover carve DEVICE [--dest D] [--types a,b] [--sector N] [--session F] [--extract]`; `recover open-session FILE`. `--apply` adds candidates to the existing table (or a new empty GPT/MBR when the disk has none; an unreadable table is never replaced); written via `begin_write`/`commit_table`/`finish` for `disk:N`, directly with `write_table` for image files; the system disk is refused without `--allow-system-disk`. Without `--yes` it prints what it would add and `Re-run with --yes to apply`. `--dest` on the drive being recovered from is refused (drive-letter check).
- `cmd_analyze.rs`: `analyze space PATH [--threads N] [--top N] [--json]`; `analyze surface DEVICE [--block SIZE]` (counts, 1-char-per-1% map `. S X`, bad offsets); `analyze bench TARGET [--sizes] [--total] [--queue] [--threads] [--random] [--cooldown]` (Windows: `pm_platform::FileBenchTarget`; elsewhere exit 2 Unsupported); `analyze smart DISK [--json]` (`smart_summary` rows with `[ OK ]/[WARN]/[BAD ]` plus the raw attribute table; "No SMART data" / `null` when the platform has none).
- Small edits to existing files: `cli.rs` (new commands, `size_arg` pub, `CliError::Failed`), `main.rs` (platform selection, dispatch), `cmd_image.rs` (`pick_format` pub(crate)), `cmd_doctor.rs`.

## Tests

`cargo test -p pm-cli -p pm-platform`: pm-cli 43 unit + 7 (cli.rs) + 14 (cli_image.rs) + 16 (cli_ops.rs); pm-platform 48 unit + 3 (file_platform.rs) + 5 (mock.rs), 3 live tests ignored as before. Zero compiler warnings (clippy is not installed for this toolchain, so it was not run).

RED/GREEN: the `file_platform_*` tests were written first and failed to compile (`FilePlatform` unresolved) before the implementation; green afterwards. The CLI tests in `tests/cli_ops.rs` (+ `tests/common/mod.rs`) were also written before the code, but I did not run them against a stub, so there is no separate red run for them (the crate did not compile without the new modules). Test-side bugs found on the first green run: the deleted-file name check (FAT shows `?one.bin`) and a hand-rolled cluster skip in the recover-files test that corrupted the SUB directory (replaced by writing the deleted file inside the same builder).

Required names present: `file_platform_enumerates_image` (pm-platform/tests/file_platform.rs), `cli_apply_json_creates_partitions_on_image`, `cli_clone_disk_image_to_image_proportional`, `cli_apply_dry_run_leaves_image_unchanged`, `cli_recover_partitions_on_wiped_image`, `cli_recover_carve_finds_embedded_jpeg`, `cli_analyze_space_on_temp_tree`, `cli_analyze_surface_on_image`. Extra: apply with hand-written JSON / single object, planning error exits 1 and writes nothing, subcommand round trip incl. `--emit-json`, volume ops refused on images, clone dry-run + self-clone, image/restore round trip, recover through `--disk-file` + `disk:0`, recover files list+restore, analyze smart/bench.

## Self-review

- No write without `--yes`: apply and all op subcommands print `Re-run with --yes to apply` and exit 0 (tests assert the image bytes are unchanged); `recover partitions --apply` likewise. `recover files/carve --dest` write only host files, never to the source drive.
- Dry run writes nothing: only `Model::from_platform`, `Planner` and `execute(dry_run: true)` run; bytes compared before/after in tests; `--dry-run` wins over `--yes`.
- Real-platform behaviour unchanged: `FilePlatform` is only constructed when `--disk-file` is given; `check_copy`, `device_arg`, planner and executor guards are untouched; the system disk is still refused by the planner and the executor guard; `recover partitions --apply` adds its own system-disk refusal.
- No `unsafe`, no OS API in pm-cli (the bench uses the `pm_platform` target under `cfg(windows)`).

## Concerns / deviations

1. Table conversion is `pm convert-table <disk> --to gpt|mbr`, not `convert`, because `pm convert` (image format conversion) already exists.
2. Ctrl-C is still not wired to the `CancelToken` (pre-existing in main.rs).
3. Disk selectors of the operation commands are indices (`N` / `disk:N`); with `--disk-file` the index is the position of the file. Only raw images work as `--disk-file` (FilePlatform has no pm-image layer; other formats would be read as raw bytes).
4. `Cargo.lock` is shared with the pm-gui agent's uncommitted changes; I committed only the lock edits for pm-cli (pm-ops, pm-recover, pm-analyze) and pm-platform (tempfile) on top of HEAD's lock and left the rest of the working-tree lock alone.
5. `analyze surface` exits 0 even when bad blocks are found (it is a report); say if a non-zero exit is wanted.
6. `SetLabel` is refused by pm-ops itself (no platform support yet); the CLI exposes the command anyway.

## Fix round 1

Command: `cargo test -p pm-cli -p pm-platform`

1. `recover partitions --apply` on `disk:N` now goes through `write_disk_table` (cmd_recover.rs): `Model::from_platform(...).effective_system_disk()`; refused when the disk is the system disk or when the system disk is unknown (same two messages as the executor, `--allow-system-disk` overrides exactly like the executor's relaxed family, which includes CreatePartition); after `finish` it calls `platform.rescan(disk)`. Unit test with `MockPlatform` (existing `calls()` log, no mock change needed): unknown system disk -> refused, no session; disk is the system disk -> refused; other disk is the system disk -> written, session finished, `rescan 1` logged; override -> written.
2. `FilePlatform::enumerate_disks` now fills `fs` and `label` per partition with `pm_fs::detect` (`pm-fs` added to pm-platform's Cargo.toml; no cycle), `used` stays `None`. Planner/precheck then refuse shrinks, and `precheck` refuses any length change of a non-NTFS volume. `check_supported_on_images` (cmd_apply.rs) now also refuses `align` on images and a length change of an NTFS/ReFS partition (it would need the volume layer). Tests: `cli_image_filesystems_are_protected_from_resize_and_align` (list shows FAT32/PMTEST; resize to 20M and 60M refused with exit 1 and the image bytes unchanged; align refused; a same-length move is allowed and the files follow) and `file_platform_detects_filesystems_without_fill_level`. Deviation from the ruling: a FAT *grow* is also refused, because pm-ops' `check_resize` rejects every length change of a non-NTFS filesystem (the filesystem would not follow); growing a partition without a filesystem stays allowed (covered in `cli_ops_subcommands_build_and_run`). Side effect: cloning an image holding an NTFS partition onto a larger target now waits 5 s in `locate_volume` and logs "could not extend the filesystem" (the clone itself succeeds).
3. New `disk_files.rs` (used by `main.rs`): `--disk-file` files must be raw (`pm_image::identify == Raw` and no container extension) else exit 1 with "only raw images can be used as --disk-file ... convert with `pm convert <file> <out>.img --format raw`"; duplicate paths (canonicalised) are refused. Tests: unit tests in disk_files.rs and `cli_disk_file_must_be_a_raw_image_and_unique` (fixed VHD made with `pm_image::create`, left unchanged).

Result lines:
```
test result: ok. 46 passed; 0 failed   (pm-cli unit)
test result: ok. 7 passed; 0 failed    (cli.rs)
test result: ok. 14 passed; 0 failed   (cli_image.rs)
test result: ok. 18 passed; 0 failed   (cli_ops.rs)
test result: ok. 51 passed; 0 failed   (pm-platform unit)
test result: ok. 4 passed; 0 failed    (file_platform.rs)
test result: ok. 5 passed; 0 failed    (mock.rs)
test result: ok. 0 passed; 0 failed; 3 ignored (windows_live.rs)
```
Zero compiler warnings.
