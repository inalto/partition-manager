# Task 6 report: pm-cli foundation

Status: DONE (one cosmetic concern below). Commit: 7c6e19e `feat(cli): list/table/mount/unmount/doctor` on `dev`.

## Implemented (crates/pm-cli)
- `Cargo.toml` (bin `pm`), `src/main.rs` (dispatch, env_logger, exit codes), `src/cli.rs` (clap derive, `CliError`, exit-code mapping, access-denied hint, `--letter/--folder` ArgGroup, `parse_letter`), `src/device_arg.rs` (`DeviceArg`, `parse_device`, `resolve_volume`, `open_device`; File arm isolated for the later pm-image hookup), `src/cmd_list.rs`, `src/cmd_table.rs`, `src/cmd_mount.rs`, `src/cmd_doctor.rs`, `src/progress.rs` (`StderrProgress`, `format_line`, throttled `frame()`), `tests/cli.rs`.
- Commands take `&dyn Platform` + `&mut dyn Write`, so every renderer is unit-tested against `MockPlatform`/in-memory data.
- Exit codes: 0 ok; 1 usage (clap errors mapped from clap's default 2 to 1; `Error::Invalid/NotFound` raised while resolving arguments); 2 platform/runtime, printed `error: <msg>`; access denied (Platform code 5 / "access denied" text / PermissionDenied io) appends "(hint: run this command from an elevated prompt)".
- `pm table`: Start and Length printed as exact bytes (Length also human-readable), so scripts/tests can match offsets; flags column decoded (GPT required/read-only/hidden/no-letter, MBR active); free ranges listed.
- `pm list`: columns aligned across all disks; Unallocated gaps >= 1 MiB interleaved in start order; "Used"/"Unused" blank when unknown.
- `pm doctor`: never fails on enumeration errors (prints "enumeration failed: ...").
- `progress.rs` is `#[allow(dead_code)]` in main.rs (not used by this task's commands).

## Tests
RED: with an empty `main() {}` stub and tests/cli.rs written first, `cargo test -p pm-cli` -> 5 integration tests failed (exit code 0 instead of 1, no output, ...).
GREEN: `cargo test -p pm-cli` -> 17 unit tests + 5 integration tests pass, zero warnings from pm-cli.
- `cli_table_on_image_file_prints_gpt` (64 MiB image, 2 GPT entries, text contains "GPT", both start offsets and names; `--json` parses to a `Table` with 2 entries; `file:` prefix also works)
- `cli_list_json_runs`: observed on this (non-elevated) machine: exit 0 with valid JSON (real disk data; enumeration works unelevated because pm-platform uses query-only handles). The exit 2 + "elevated" branch is also accepted and covered by unit test `exit_codes_and_hint`.
- `cli_parse_device_variants` (unit, device_arg.rs), plus doctor, usage-error (exit 1), missing-file (exit 1) integration tests; mount/unmount, list, table, doctor, progress unit tests.

## Output (non-elevated)
```
> pm doctor
pm 0.1.0
Elevated:     no
Platform:     windows
Toolchain:    stable-x86_64-pc-windows-gnu (x86_64 windows)
Disks:        3
  Disk 0 (Fanxiang S880E 2TB, GPT, 1.82 TB) - 4 partitions
  Disk 1 (LaCie Rugged USB-C, GPT, 4.55 TB) - 2 partitions
  Disk 2 (Netac Mo bileDataStar, MBR, 931.51 GB) - 1 partition
System disk:  0
Free letters: E: H: I: J: K: L: M: N: O: P: Q: R: S: T: U: V: W: X: Z:

> pm list
Disk 0 (Fanxiang S880E 2TB, GPT, 1.82 TB)
Partition    Capacity    Used       Unused     File system  Type                        Status
-----------  ----------  ---------  ---------  -----------  --------------------------  ------
D:SYSTEM     1015.63 MB  38.37 MB   977.26 MB  FAT32        GPT (EFI system partition)  System
*:           16.00 MB                          Other        GPT (Microsoft reserved)    None
C:Windows    1.81 TB     1.05 TB    781.95 GB  NTFS         GPT (basic data)            None
*:WinRE_DRV  3.82 GB     871.02 MB  2.97 GB    NTFS         GPT (recovery)              None

Disk 1 (LaCie Rugged USB-C, GPT, 4.55 TB)
...(2 partitions)
Disk 2 (Netac Mo bileDataStar, MBR, 931.51 GB)
...(1 partition)

> pm table disk:0   (exit 2)
error: platform error 5: open \.\PhysicalDrive0: Access denied - run as administrator (hint: run this command from an elevated prompt)
```

## Files changed
Only `crates/pm-cli/**` and `Cargo.lock` (the lock diff also contains entries added by other agents' crates, e.g. pm-fs/pm-image deps, because the lock was already modified in the shared tree).

## Self-review / concerns
- A `cargo run` printed a dead-code warning `system_directory` in pm-platform (another agent's in-progress change); not from pm-cli.
- `pm table <volume>` of a letter opens the volume device and reads a table; a filesystem volume yields `None` kind - harmless.
- Mount/unmount were only exercised against `MockPlatform` (no real mount on this machine, by design).
- `parse_device` treats `D:` / `D:\` always as a volume, even if a directory of that name exists.
- clippy is not installed for this toolchain, so only rustc warnings were checked.

## Fix round 1
Review finding: "no such device" gave inconsistent exit codes (1 vs 2). Fixed so every unknown disk / drive letter / missing file (bare path and `file:` form) exits 1; access denied and I/O errors stay exit 2.
- `device_arg.rs`: `parse_device` checks `file:` paths exist (`Error::NotFound`); `open_device` turns an open failure into `Error::NotFound` when the disk index / volume id is not in `enumerate_disks` / `enumerate_volumes` (so the OS's own error code for a missing disk no longer decides the exit code); the `\?\Volume{..}` trailing-backslash normalisation moved into a shared helper used by both `parse_device` and `resolve_volume` (so mount/unmount get it too).
- `cmd_table.rs`: `NotFound` from opening the device maps to `CliError::Usage`; other errors stay Runtime.
- New tests: `cli_unknown_volume_exits_1` (`table disk:99999`; and, with a letter chosen from `pm list --json` that no partition uses, `table`, `mount`, `unmount` -> 1), `cli_missing_file_exits_1_for_both_forms`; unit tests for NotFound on disk/volume/file and for `resolve_volume` normalisation.
- `cargo test -p pm-cli`: 17 unit + 7 integration pass, no warnings.
- Manual: `pm table disk:99` -> `error: no disk with index 99` exit 1; `pm table Q:` -> `error: no volume is mounted at Q:` exit 1; `pm table file:nope.img` -> exit 1; `pm table disk:0` unelevated still exit 2 with the elevation hint.
