# Task 12 report: CLI imaging commands

Status: DONE (with concerns below). Commit: `d87a0fc feat(cli): clone, image, restore, convert, rescue, ls` (only `crates/pm-cli` and the two pm-cli lines of `Cargo.lock`; the concurrent `pm-ops` lock entry was left unstaged).

## What was implemented

New modules in `crates/pm-cli/src`:
- `cmd_clone.rs`: `pm clone`, plus the shared helpers: `SharedSource` (cloneable read-only handle on the source, writes rejected structurally), `check_copy` (sector-aligned `--dst-offset`, same-device/overlap refusal, destination-size check), `plan_used_map` (Auto = smart when `pm_fs::open` works, `--smart` that cannot be honoured is an error, `--sector`), `copy_device` (checks, then `pm_image::copy_simple`), `summary`.
- `cmd_image.rs`: `pm image` (format from `--format` else `ImageFormat::from_extension` else usage error; capacity = len rounded up to 512; refuses to overwrite the file it reads from before `create` truncates it), `pm restore` (sector copy, same checks as clone), `pm convert` (`pm_image::convert`, refuses in == out, missing input is exit 1; `--format` optional with extension fallback).
- `cmd_rescue.rs`: `pm rescue src dst mapfile [-b -K -r -R -n -N --dst-offset --used-map]`. `--used-map` sets `used_map` + `zero_unused = true`. Live status block on stderr (ipos, phase, rescued, errsize, errors, rate, ETA): ipos comes from a read-position-tracking source wrapper (`PosSource`), errsize/errors parsed from the engine's detail string. Three render modes: in-place ANSI block (terminal; ANSI enabled via kernel32 `SetConsoleMode` on Windows, falls back to `\r` line), `\r` line, or one line per phase when stderr is redirected. Final summary on stdout; exit 0 with bad sectors, errors only on fatal failures. `Error::Invalid` from the engine (e.g. mapfile of another size) maps to exit 1.
- `cmd_probe.rs`: `pm probe <device> [--offset]`, prints `FsProbe` fields or `no filesystem detected`.
- `cmd_ls.rs`: `pm ls` (type, size, `YYYY-MM-DD HH:MM:SS`, name; dirs first), `pm cat` (stdout, binary safe), `pm extract` (file or recursive dir; `/` extracts the root contents). Path lookup is exact then case-insensitive, `/` or `\` separated. Extraction treats image names as hostile: rejects separators, `.`/`..`, `:` and other Windows-special characters, trailing dot/space, prefixes reserved device names with `_`, caps depth at 64 and guards directory cycles via ancestor ids, honours the `CancelToken`.

Modified:
- `cli.rs`: new subcommands, `CopyModeArgs` (`--smart`/`--sector`, mutually exclusive), `RescueArgs`, `parse_format` (raw, vhd, vhd-fixed, vmdk, vmdk-flat, qcow2), sizes via `pm_core::parse_size`.
- `device_arg.rs`: `DeviceArg::Partition(Box<DeviceArg>, u32)` for `disk:N:P`, `file:PATH:P` and bare `PATH:P`; `DeviceArg::File` now opens through `pm_image::open`; `open_device_with_slice` returns `Opened { dev, identity, range }` (identity = disk index / volume id / canonical file; range = byte range covered, so partitions of one disk can be compared); `open_device` keeps its signature; `open_spec` = parse + open with NotFound -> exit 1. `parse_device` stays backward compatible (existing tests unchanged and passing).
- `main.rs`: dispatch, a `CancelToken` is created and threaded to every long command (no Ctrl-C handler yet, as agreed); `#[allow(dead_code)]` on `progress` removed.
- `Cargo.toml`: deps `pm-image`, `pm-fs`.

## Tests (RED / GREEN)

RED: tests written first in `tests/cli_image.rs`; first run, all 11 failed with `error: unrecognized subcommand 'clone'/'ls'` (build succeeded, so the failures were the missing commands, not compile errors).

GREEN: `cargo test -p pm-cli` -> 27 unit + 7 existing integration + 12 `cli_image` tests pass, zero warnings (re-run after pm-image's concurrent commit 2598ac5, still green). Tests in `cli_image.rs`:
- brief names: `cli_image_and_restore_roundtrip_qcow2` (also checks `identify == Qcow2`, extension-chosen format, no-extension usage error that creates nothing), `cli_convert_vhd_to_vmdk` (vhd -> vmdk -> raw round-trip byte-identical, in==out refused with file intact, missing input exit 1), `cli_rescue_on_file_writes_mapfile` (mapfile parses with `MapFile::load`, all `Finished`, resume with `-R -n -N`), `cli_ls_fat32_image_lists_files` (40 MiB FAT32 via `format_fat32` + a minimal short-name FAT32 writer in the test, since pm-fs's `tests/common` is not importable; `ls`, `ls /sub` case-insensitive, `cat` of files incl. a multi-cluster one, error cases, `probe`), `cli_clone_smart_fat32_leaves_unused_zero` (destination pre-filled 0xAA; used ranges equal the source, tail stays 0xAA; `pm ls`/`cat` on the clone; `--sector` then gives an identical copy).
- extra: `cli_extract_file_and_directory`, `cli_probe_reports_no_filesystem` (and `ls` on it exits 2), `cli_clone_refuses_same_file_and_small_destination` (same file incl. alias path, too small, offset overflow, unaligned offset, `--smart --sector` conflict, `--smart` on non-fs fails; destinations verified untouched), `cli_partition_addressing_on_image_file` (GPT image, `file:img:1` ls/probe/clone, bad partition numbers exit 1, partition cloned onto its own disk image refused), `cli_rescue_refuses_unsafe_arguments` (too small, src as dst, mapfile == src/dst, missing mapfile dir, bad `-b`, wrong-size mapfile; src/dst untouched), `cli_rescue_with_used_map_zeroes_unused`, `cli_image_smart_fat32_to_qcow2_is_sparse_and_browsable` (qcow2 < 8 MiB for a 40 MiB volume, `cat` straight from the qcow2, restore byte-identical).
- unit tests: partition parsing, overlap rules, size/time formatting, name sanitising, status-block rendering/redraw, rescue option mapping, format parsing.

## Self-review

- No command writes to its own source: sources are opened `ReadOnly` and wrapped in `SharedSource`/`PosSource`, whose `write_at` returns an error. Same-device refusal is by canonical file path or disk index/volume id plus byte-range overlap (so `disk:0:1` -> `disk:0:2` is allowed, `disk:0:1` -> `disk:0` is not). `image`/`convert` refuse when the output is the existing source file, before `create` can truncate it. Rescue also refuses a mapfile that is the source or destination.
- Size checks precede every write: `check_copy` runs before `copy_simple`; `rescue` runs `check_copy` and the engine re-checks. Verified in tests by asserting refused destinations are still all zero.
- Not run live: the ANSI in-place block in a real terminal (only unit-tested for the byte sequence) and bad-sector behaviour through the CLI (no way to inject faults on files; engine covers it in pm-image).

## Concerns / decisions to review

1. Flag letters: the brief specifies `-n|--no-trim` and `-N|--no-scrape` and `-b` = block size. GNU ddrescue itself uses `-n` = no-scrape, `-N` = no-trim, and `-b` = sector size. I followed the brief verbatim; if true ddrescue compatibility is wanted, swap the two letters.
2. `pm image` and `pm convert` overwrite an existing output file silently (brief says "creates the image"; no `--force` added). Partial images are left in place on failure/cancel.
3. Sector-mode `image` into a sparse format (qcow2/VHD/VMDK) allocates zero chunks too, because `copy_simple` has no skip-zero option (`convert` does skip them). Smart mode is unaffected.
4. Volume vs disk aliasing (`D:` vs the `disk:N` it lives on) is not detected by the same-device check; only same disk index / same volume / same canonical file / overlapping partitions of one object. A flat-VMDK extent file referenced by a descriptor is likewise not recognised as "the same file".
5. The destination is opened read-write before the checks run (needed to know its size); on a real disk the platform layer may lock it even when the copy is then refused.
6. `--smart`/Auto uses the filesystem at offset 0 of the source only; a whole-disk source with a partition table falls back to sector mode (Auto) or errors (`--smart`), pointing the user at `disk:N:P`.
7. No confirmation prompt for writing to physical disks (not in the brief).

## Files

- Modified: `crates/pm-cli/Cargo.toml`, `src/cli.rs`, `src/device_arg.rs`, `src/main.rs`, `Cargo.lock` (two lines)
- Created: `crates/pm-cli/src/{cmd_clone,cmd_image,cmd_ls,cmd_probe,cmd_rescue}.rs`, `crates/pm-cli/tests/cli_image.rs`

## Fix round 1

Command: `cargo test -p pm-cli` (zero warnings)

```
test result: ok. 31 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s   (unit)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.54s    (tests/cli.rs)
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.45s   (tests/cli_image.rs)
```

1. System disk: `check_copy(platform, ..)` refuses a destination whose physical disk (`Opened::disk_index`, inherited by `disk:N:P` slices and resolved for volumes) is `platform.system_disk()`: "Disk N is the running system disk; refusing to write to it" (exit 1). Applies to clone, restore and rescue. Test `system_disk_is_never_a_destination` (MockPlatform + `set_system_disk`; disk, partition slice, restore, rescue; system disk bytes unchanged and no mapfile created; system disk still fine as a source).
2. One shared helper `cmd_clone::open_checked(platform, src, dst, offset)`: opens src RO, dst RO, runs `check_copy`, drops the probe, reopens dst RW and re-checks. `run` (clone), `run_restore` and `rescue::run` all use it; `copy_device` no longer opens or checks anything.
3. Volume/disk aliasing: `Opened` now carries `disk_extent: Option<(disk, start, end)>` (disks, partitions, and volumes resolved through `enumerate_volumes()` with a fallback to `enumerate_disks()` partitions' `volume`). `overlaps` compares whole extents of one physical disk (conservative: a copy never lands in a device that contains, or is contained by, the other). Test `volume_and_disk_aliasing_is_refused`: `D: -> disk:1`, `disk:1 -> D:`, `D: -> disk:1:2`, `D: -> disk:1:1`, `disk:1:1 -> D:` refused with nothing written; adjacent `F: -> disk:1:2` and `D: -> disk:2` succeed with correct bytes.
4. `pm rescue` short flags now match GNU ddrescue: `-b/--sector-size` (bytes, multiple of the device sector size), `-c/--cluster-size` (sectors per read; read size = c x b; default = 128 KiB worth), `-K/--skip-size`, `-r/--retries`, `-R/--reverse`, `-n/--no-scrape`, `-N/--no-trim`. Unit test `options_follow_the_flags`, integration tests updated (`-b 512 -c 128`, help lists every short/long pair, bad `-b`/`-c` values exit 1).
5. No FFI: the kernel32 `extern`/`unsafe` block is gone (`grep unsafe crates/pm-cli/src` is empty). The in-place ANSI block is used only with the new `--ansi` flag or when stderr is a terminal and `WT_SESSION` is set or `TERM` is set and not `dumb`; other terminals get the `\r` single line, redirected stderr one line per phase (`choose_mode`, unit-tested).
6. `pm image`: all refusals and the `--smart` used-map planning run before anything is created; the image is written to `<out>.partial` and renamed on success, and the partial is deleted on failure or cancel. Same for `pm convert`. A flat VMDK is handled as a unit (extent renamed to `<stem>-flat.vmdk` and the descriptor patched). New module `partial.rs`. Tests: `cli_image_failure_keeps_existing_output_and_leaves_no_partial` (failing `--smart` leaves an existing output byte-identical; success leaves no `.partial`; a failing rename removes the partial; same for convert) and `cli_flat_vmdk_roundtrip_and_extent_protection`.
7. Dependent files: the descriptor of a VMDK source is parsed (`RW|RDONLY|NOACCESS <n> FLAT "<name>"`, no public pm-image helper) and the canonical extent paths are part of the source's file set (`Opened::files`, `source_files()`); image/convert outputs (final, `.partial`, flat extents) and clone/restore destinations are refused when they hit any of them. Test `cli_flat_vmdk_roundtrip_and_extent_protection` (image/convert/clone/restore onto the extent, clone extent onto the descriptor, descriptor onto itself; extent bytes unchanged). The remaining limitation (hard links, junctions, files stored on the disk being copied) is documented in the `--help` of clone/image/restore/convert. VHD parents are not applicable: pm-image rejects differencing VHDs.

Note: the RO-then-RW reopen order in (2) is structural; there is no test observing open modes (MockPlatform does not record opens).
