# Task 21 report: packaging, docs, platform stub, VHDX reader

Status: DONE (one concern, see below)

## Commits (branch `dev`)

- `e688738` feat(image): read-only VHDX opener
- `b9c5a44` chore: packaging and docs
- `bbf355d` chore: keep shell scripts LF on checkout (adds `.gitattributes` with `*.sh text eol=lf`)

## What was done

### VHDX (`crates/pm-image/src/vhdx.rs`)
- File identifier ("vhdxfile"), two headers at 64/128 KiB (signature "head", local CRC-32C, highest
  sequence among valid headers wins), region table at 192 KiB ("regi", CRC-32C verified), BAT and
  metadata regions by GUID, metadata table ("metadata") with File Parameters, Virtual Disk Size and
  Logical Sector Size, BAT with chunk-ratio sector-bitmap skipping.
- `BlockDevice` read-only: unallocated/zero/unmapped blocks read as zeros; FULLY_PRESENT reads from
  `entry & !0xFFFFF`; PARTIALLY_PRESENT or HasParent -> `Unsupported`; writes and `open(.., false)`
  -> `Invalid("VHDX is read-only")`. Sector size is the logical sector size from metadata.
- Hooked in: `ImageFormat::Vhdx` (`display() = "VHDX (read-only)"`, `from_extension("vhdx")`,
  `extension() = "vhdx"`), `sniff` ("vhdxfile" at 0), `open`, `create` -> `Unsupported`.
  No exhaustive matches elsewhere in the workspace broke.
- Tests: `vhdx_read_fixture_header`, `crc32c_known_vector` (0xE3069283), plus
  `guid_on_disk_order`, `vhdx_ignores_newer_header_with_bad_checksum`, `vhdx_rejects_corrupt_and_differencing`.
- `crates/pm-image/README-formats.md` updated (table row, "Not supported", VHDX section).
- Not done on purpose: log replay (documented as a limitation), writing (TODO in README).

### Platform stub
`NullPlatform` and `default_platform()` in `crates/pm-platform/src/lib.rs` were already clean:
`default_platform()` returns `WindowsPlatform` under `#[cfg(windows)]` and `NullPlatform` under
`#[cfg(not(windows))]`; the `windows` module, its re-exports and the `windows-sys` dependency are all
`cfg(windows)`-gated; `console.rs`, `pm-core/device.rs`, `pm-analyze` and `pm-cli/cmd_analyze.rs`
have matching `cfg(windows)` / `cfg(not(windows))` pairs. No code change was needed. The Linux
target was not compiled (not installed). The README documents the Linux build as a stub.

### Docs and scripts
- `README.md`: all requested sections (What it is + screenshot, Features, Safety model, Build routes
  A/B with the exact `.cargo/config.toml` paths, Run (GUI + CLI examples), Try it on an image file,
  Verifying images with qemu-img / Hyper-V, Known limitations, Project layout, Testing, License, Credits).
- `docs/CLI.md`: generated from the clap `--help` of every command by `scripts/gen-cli-docs.sh`
  (run it again after CLI changes; uses `target/debug/pm.exe`).
- `docs/ARCHITECTURE.md`: layering diagram, exact dependency table from the manifests, clone-disk data
  flow, safety-check layers (planner, precheck, executor pre-flight, platform session) with file locations.
- `scripts/build-release.ps1`: `cargo build --release`, copies to `dist/`, prints sizes,
  `pm.exe --version`, checks the manifest string and fails if missing.
- License: no LICENSE file exists; README says MIT per `Cargo.toml` and notes the missing file.

## Self-review of README commands
Every documented command and flag was checked against the actual `--help` output, and these were run
against an image file with the built binary: `init-disk --gpt --yes`, `create ... --name/--type efi --yes`,
`list`, `table`, `probe`, `image` (to .vhd), `convert` (to .qcow2), `clone` (absolute Windows paths),
`restore`, `rescue` (destination must already exist; README says so), `create --emit-json` then
`apply --dry-run`, `image-disk --dry-run`, `recover partitions --quick --session`, `recover open-session`,
`recover carve`, `analyze space`, `analyze surface`, `doctor`. Corrections found this way: `format`/`create --fs`/
`clone-disk` are "not available on image files" (README states it and the walkthrough creates unformatted
partitions); `fsutil createnew` is not sparse (comment fixed); `clone`/`restore`/`image`/`convert`/`rescue`
have no `--yes` (they copy immediately; only the operation commands and `apply` need `--yes`).

## Verification
`cargo test --workspace --no-fail-fast`: no warnings, no failures. Per-binary `test result:` lines (in run order):
```
pm-analyze lib: ok. 0 passed
pm-analyze bench: ok. 6 passed | smart: ok. 3 passed | space: ok. 5 passed | surface: ok. 3 passed
pm-cli unit: ok. 47 passed | cli: ok. 7 passed | cli_image: ok. 14 passed | cli_ops: ok. 18 passed
pm-core: ok. 25 passed
pm-fs unit: ok. 8 passed | bitmap: 5 | detect: 6 | exfat: 15 | exfat_builder: 0 | ext: 9 | ext_builder: 0
        fat: 10 | fat_corrupt: 11 | fat_undelete: 2 | ntfs: 7 | ntfs_builder: 0   (all ok)
pm-gui lib: ok. 117 passed | pm-gui main: 0
pm-image unit: ok. 15 passed (includes the 5 vhdx tests) ... plus formats / mapfile / rescue integration tests, all ok
pm-ops, pm-parttable, pm-platform (3 ignored live tests), pm-recover: all ok
```
(Full list: 45 test binaries, every one `ok`, 0 failed.)
A first full run showed a transient `pm-gui --lib` failure while another agent was editing
`crates/pm-gui` files; it passed on rerun (117 passed) and in the final full run.

`cargo build --release` (via the script): zero warnings in the final run. An earlier run showed one
`private_interfaces` warning in `crates/pm-gui/src/wizards/data_recovery.rs` (another agent's uncommitted
work at that moment); it was gone after that agent's commit `983d8b5`.

## dist/ (gitignored: `.gitignore:9:/dist/`)
```
partition-manager.exe   7,845,376 bytes (7.48 MiB)
pm.exe                  4,319,744 bytes (4.12 MiB)
pm 0.1.0
```
`partition-manager.exe` contains exactly one `requireAdministrator` (no `asInvoker`); `pm.exe` has none.

## Concerns
- `scripts/build-release.ps1` run through `2>&1` in a wrapper shows PowerShell 5.1 `NativeCommandError`
  noise for cargo's stderr; the script itself sets `ErrorActionPreference=Continue` around cargo so a normal
  run is fine and the exit code is checked.
- `.cargo/config.toml` linker paths contain the user name `alain`; README quotes them and tells the reader to adjust.
- Known limitation worth knowing: VHDX log is not replayed, so a VHDX not cleanly closed may show stale metadata.

## Fix round 1

1. vhdx.rs (critical): the device now stores the file length; the FULLY_PRESENT arm requires
   `base >= 1 MiB` and `base.checked_add(block_size) <= file_len`, else `Error::Parse("VHDX: BAT entry out of
   range (block N)")`; the read offset is then computed from a validated base. New test
   `vhdx_bat_entry_out_of_range_is_parse_error` covers an overflowing entry (0xFFFFFFFFFFF00000|6), an entry past
   EOF, one straddling EOF, and one inside the 1 MiB header area, each read at two offsets.
2. vhdx.rs (important): new test `vhdx_skips_sector_bitmap_entries`: 4097 MiB virtual disk, 1 MiB blocks, 512 B
   sectors (chunk ratio 4096, BAT of 4098 entries). Entry 4096 is a bitmap entry deliberately pointing at block 0's
   data, so a reader that does not skip it returns the wrong pattern; payload block 4096 is entry 4097 at 4 MiB.
   Asserts block 0, the ZERO block 1, the NOT_PRESENT block 4095, block 4096's pattern at virtual offset 4096 MiB, and a
   read crossing 4095 -> 4096. The file itself is only 5 MiB (only the virtual size is large), so no sparse file is needed.
3. scripts/build-release.ps1: counts `requireAdministrator` occurrences; fails unless partition-manager.exe has exactly 1
   and pm.exe exactly 0.
4. Minor: ARCHITECTURE dependency table now shows pm-analyze -> pm-image and pm-recover -> pm-image as
   dev-dependencies only (and the "edges not drawn" sentence is generic); README system-disk bullet no longer claims
   "three layers" and points to the four safety layers listed in docs/ARCHITECTURE.md.

Command: `cargo test -p pm-image` (all green, zero warnings):
```
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.43s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.52s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
Script (`scripts\build-release.ps1`, exit 0):
```
dist/:
partition-manager.exe       7.841.280 bytes (7,48 MiB)
pm.exe                      4.319.744 bytes (4,12 MiB)

pm 0.1.0
requireAdministrator in partition-manager.exe: 1 (expected 1)
requireAdministrator in pm.exe: 0 (expected 0)
```
