# Multi-platform Task D report: apps, docs, wiki, cross-cutting fixes

## Commits

- `79a75e6 fix(ops,cli): table and volume-id guards for non-Windows platforms` (D1)
- `15de5e0 feat(apps): Linux and macOS support in the CLI and GUI` (D2, with the coordinator's
  addendum on device paths and non-contiguous disk indexes)
- `f142eb5 fix(cli): refuse image and rescue outputs on the device being read` (found by the
  Linux loop-device walk-through, see D3)
- `docs: Linux and macOS support` (D3 + this report)

Nothing under `crates/pm-platform/` was edited. Commit trailers use the attribution the
harness asked for in this session (`Claude Opus 5.5`), not the plan's `Claude Fable 5.1`.

## D1. Cross-cutting safety fixes

### Planner (`pm-ops/src/planner.rs`)

- `require_table` is now called in the `DeletePartition` and `ResizeMove` arms (both rewrite
  the table). Its message distinguishes `TableKind::Unknown` ("The partition table of Disk N
  could not be read, so its partition offsets are not exact; ...") from `None` ("not
  initialized").
- New `require_exact_range(disk, partition)` for `WipePartition`, `ImagePartition` (source)
  and `ClonePartition` (source).

**Ruling (superfloppy).** On Windows a superfloppy is reported by
`IOCTL_DISK_GET_DRIVE_LAYOUT_EX` as an MBR disk with one partition at offset 0
(`windows/layout.rs`: `TableKind::None` is only produced for `PARTITION_STYLE_RAW`, with no
partitions), and `FilePlatform` produces no partitions for a table-less image, so Windows is
unaffected by `require_table`. The table-less case with a partition is Linux's whole-disk
pseudo-partition (`linux/mod.rs::whole_disk_partition`, WSL's disks, superfloppy sticks).
Rule, documented on `require_exact_range`: the byte range of a partition is exact on an
MBR/GPT disk, and on a `TableKind::None` disk whose only partition starts at 0 and covers the
whole disk; there Wipe / Image / Clone-source are allowed. Delete and Resize/Move need
MBR/GPT always. A `TableKind::Unknown` disk (approximate macOS starts, unreadable Linux
table) is refused for all of them.

Tests (`pm-ops/tests/planner.rs`): `approximate_partitions_are_never_read_or_written` (Unknown
disk refuses all five operations; a table-less disk whose partition is not at 0 refuses
Wipe/Image/Clone) and `whole_disk_volume_without_a_table_can_be_wiped_imaged_and_cloned`
(table-less whole-disk volume and a Windows-style MBR superfloppy allow Wipe/Image/Clone; the
table-less one refuses Delete/Resize; the MBR one still allows Delete). The existing 20
planner tests pass unchanged.

### CLI device arguments (`pm-cli/src/device_arg.rs`, `cmd_ops.rs`, `main.rs`)

- `resolve_volume` passes `/dev/...` ids through `Platform::canonical_volume_id`.
- `disk_of_node(disks, node)` maps a whole-disk node to its index by `DiskInfo.path`, with the
  macOS `rdiskN`/`diskN` twin and symlinks (`/dev/disk/by-id/...`) followed;
  `disk_spec_of_node` also accepts `NODE:P` (partition P).
- `parse_device_on(platform, s)` (= `parse_device` + node resolution) replaces `parse_device`
  in `open_spec`, `cmd_clone` (destination), `cmd_image` (rescue op selection), `cmd_recover`,
  `cmd_rescue`, `cmd_table`, so a whole-disk node becomes `Identity::Disk(N)` and the
  same-device / overlap checks see it.
- Arguments parsed by clap as a disk index (`disk_arg`, every `pm <op> <DISK>` and `pm analyze
  smart`) accept the node too: `main` registers the enumerated disks (only when an argument
  starts with `/dev/` and no `--disk-file` is given, so nothing is enumerated otherwise) and
  `disk_arg` looks the node up; a partition node gives "not a whole disk of this system".
- Tests: `whole_disk_nodes_map_to_disk_indexes` (indexes 0, 2, 5; twins; partition nodes stay
  volumes; `NODE:P`), `dev_node_arguments_resolve_to_disks_on_the_platform` (MockPlatform
  disks `/dev/sda`, `/dev/sdb`, `/dev/nvme0n1`; `/dev/sdb` overlaps `disk:2`),
  `typed_volume_ids_are_canonicalised_by_the_platform` (a test platform with a macOS-style
  `canonical_volume_id`), `disk_arguments` (node via the lookup).

### Access-denied detection (`pm-cli/src/cli.rs`)

`denied_code`: 5 on Windows, 13 (EACCES) / 1 (EPERM) on unix; plus the message containing
"access denied" or "permission denied"; `ErrorKind::PermissionDenied` I/O errors as before.
Test `access_denied_codes_follow_the_os`.

### Same-device guards via `disk_of_path`

New `pm_ops::location` (OS-neutral): `MountMap` (which partitions carry a mount, from disks +
volumes), `locate(platform, path)` (absolute path → `disk_of_path`), `place_of(path,
located, device, mounts) -> Elsewhere | OnDevice | Unknown`, `place_problem`,
`path_problem_on`. Rule: a located path is on the device when it is the same disk and, if
both partitions are known, the same partition; an unlocatable path is `Unknown` (refused)
when the device has a mounted filesystem (or its disk is not listed), `Elsewhere` otherwise
(nothing unmounted can hold a path; this keeps `--disk-file` and unmounted sources working);
Windows UNC paths are `Elsewhere`.

Users: `pm recover files --dest` (replaces the drive-letter `check_dest`; volumes the
platform cannot place fall back to a longest-mount-point check), and in the GUI the Create
Image (image file, mapfile), Restore Image (disks holding the image are not offered), Rescue
Copy (destination file, mapfile; the worker's `recheck` asks the platform again) and Data
Recovery (destination) wizards. The GUI caches `disk_of_path` per typed text
(`tools::PathLocation`), since the Windows / macOS answers enumerate volumes or run
`diskutil`; `WizardCtx` gained `platform: Option<Arc<dyn Platform>>` and `os_name`. The old
letter helpers (`path_letter`, `on_letters_problem`, `Device::letters`) are gone; the
Windows-only letter tests became OS-neutral tests on MockPlatform's `set_disk_of_path`
(drive letters keep a `cfg!(windows)` case). The letter check was not kept as a fast path:
Windows' `disk_of_path` covers letters and folder mount points.

Extra (commit `f142eb5`): `pm image` (direct and `--rescue`) and `pm rescue` had no
"output on the source" check at all on any OS. The Linux walk-through showed `pm image
/dev/loop0:1 <file on loop0p1>` filling the partition (ENOSPC). They now refuse the image,
its `.partial` and the mapfile on the device being imaged/rescued, and a rescue mapfile on
the destination disk (`refuse_files_on`, test `image_files_on_the_source_are_refused`).

## D2. Apps

GUI:
- No drive letters (`free_letters()` empty): Create hides the letter row and summary line,
  Mount starts on a folder with no letter radio, Change Letter is not offered (Label only).
- `fs_unavailable(os, fs)`: NTFS disabled on macOS, ReFS off Windows (Create and Format);
  Create defaults to exFAT where NTFS is unavailable.
- Boot repair: `boot_fix_for` / `boot_fix_ui(.., os)` in Copy Disk and Migrate OS: forced to
  None and the combo disabled off Windows with "Boot repair is Windows-only; the clone will
  not be made bootable from this host."
- Disk headers (table rows, disk-map tooltip) show `DiskInfo.path`; a `TableKind::Unknown`
  disk adds "[partition table not readable, offsets approximate: <elevation hint> for exact
  offsets]" and the map label says "Unreadable" instead of "Uninitialized".
- Benchmark / Space Analyzer offer every mount point (`mount_roots`), not only letters; the
  initial folder is the partition's first mount point.
- Example paths in hints/messages per OS (`format_util::example_path`).
- Verified unchanged: status bar wording via `os_name` ("Running as root" / "Not root:
  read-only"), `elevation_advice`, `shell::open_in_file_manager` (`xdg-open` / `open`).

CLI:
- `pm list`: `Disk N (...)  /dev/sdb`; Unknown tables say "partition table not readable,
  offsets approximate: run with sudo (root) for exact offsets".
- `pm doctor`: "Elevated: no (read-only; <hint> to write)", and a `Tools:` block per OS
  (Linux: mkfs.vfat/exfat/ntfs/ext4, ntfsresize, resize2fs, e2fsck, partprobe, udevadm,
  smartctl via `posix::cmd::find_tool`; macOS: /usr/sbin/diskutil, smartctl; Windows:
  bcdboot, bootsect, mbr2gpt, format.com, diskpart under System32). No `Platform` method
  for tools exists, so this is display-only `cfg` code in `cmd_doctor.rs`.
- `pm mount --letter` is refused when `free_letters()` is empty ("drive letters exist on
  Windows only; ... --folder <DIR>"); `pm unmount --letter` off Windows when there are no
  letters; folders are printed as given (no trailing backslash).
- Help text documents `/dev/...` (Table, Mount/Unmount, source/device args, disk indexes).
- `cli_unknown_volume_exits_1` lost its NullPlatform escape and checks a missing
  `/dev/pm-no-such-node` (exit 1) on every OS.

Non-contiguous indexes (addendum): `Model::disk` already looks up by index; tests with
indexes 0, 2, 5 in `pm-ops` (`disk_indexes_need_not_be_contiguous`: from MockPlatform,
planner by index), `pm list` (`disks_listed_by_index_with_their_paths`) and the GUI table
(`disk_indexes_need_not_be_contiguous`). No position-based lookups were found in the GUI or
CLI outside tests.

## D3. Docs, wiki, release script

- `README.md`: intro (three OSes), platform table with what is verified, unavailable features
  per OS, `/dev` and `disk:N` syntax, build (apt line incl. `build-essential`, runtime tools,
  `brew install smartmontools`, `scripts/build-release.sh`), run with `sudo`, known
  limitations, crate table, live tests. Notes that `macos/` and `plist` compile everywhere.
- `docs/FIRST-LIVE-TEST.md` + wiki `First-Live-Test`: Linux loop-device walk-through (run in
  WSL as root, output matched) and macOS `hdiutil` walk-through (not run; flagged).
- `docs/ARCHITECTURE.md` + wiki `Architecture`: module layout, the per-OS method table,
  six `Platform` implementations, planner and session safety additions.
- Wiki `Home`, `Getting-Started` (build/run per OS), `Safety-Model` (Linux/macOS sessions,
  `disk_of_path` checks, device normalisation), `Known-Limitations` (Linux and macOS
  sections from the three reports' concerns), `Roadmap` (deferred items from the ledger),
  `GUI-Guide` (Linux/macOS differences, log paths), `CLI-Reference` (synced with the
  regenerated `docs/CLI.md`; `scripts/gen-cli-docs.sh` header updated). No new pages,
  `_Sidebar.md` unchanged. The wiki repo was not pushed.
- `scripts/build-release.sh`: release build, `dist/<os>-<arch>/` with `pm`,
  `partition-manager`, README, LICENSE, and `dist/partition-manager-<version>-<os>-<arch>.tar.gz`;
  sets `RUSTUP_TOOLCHAIN=stable` when `rust-toolchain.toml` pins a Windows toolchain. Ran it
  in WSL: 1m54s, tarball listed `pm`, `partition-manager`, README.md, LICENSE (artifacts
  removed afterwards).
- `.github/workflows/ci.yml` unchanged (Task B needed no extra apt package).

## Tests

Windows (`cargo test --workspace --no-fail-fast`): **690 passed, 0 failed, 3 ignored**, no
warnings. One earlier run showed the known flaky `pm-gui --lib` failure while building
(Task A report); the next runs were green.

Cross-checks from Windows: `cargo check --workspace --tests --target x86_64-unknown-linux-gnu`
and `--target aarch64-apple-darwin`: Finished, no warnings.

WSL Ubuntu 24.04 (`RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR=~/pm-target CI=true cargo test
--workspace --no-fail-fast`): **706 passed, 0 failed, 1 ignored** (`linux_live`), no
warnings. Selected `test result:` lines: pm (bin) 60, cli 7, cli_image 18, cli_ops 19, pm_gui
134, pm_ops planner 23 / executor 31, pm_platform lib 68, linux_fixtures 11, macos_fixtures 20,
mock 8.

CLI smoke in WSL, unprivileged: `pm doctor` (Elevated: no (read-only; run with sudo (root) to
write), Platform: linux, 4 disks, System disk: 3, Tools: all ten found), `pm list` (four
virtual disks with `/dev/sdX` and the unreadable-table note), `pm --disk-file
/tmp/pm-taskd.img list` (unchanged, path shown), `pm table /dev/sda` (exit 2, access denied
with the sudo hint), `pm table /dev/pm-nope` (exit 1), `pm analyze smart /dev/sda` (no data),
`pm mount /dev/sda --letter E` (exit 1, Windows only).

Loop-device walk-through as root in WSL (the documented Linux procedure): init-disk GPT,
create FAT32 200 MB via `mkfs.vfat` (label T1, `lsblk` agrees), mount/unmount at a folder,
`delete --dry-run` by node, `pm recover files` destination on the partition refused. `pm
image /dev/loop0:1 <file on that partition>` failed with ENOSPC before `f142eb5`; it is
refused now (unit-tested; not re-run live).

## Concerns

1. **Unplaceable paths are refused more often.** On Linux/macOS a path on NFS/SMB, tmpfs or
   LVM is "Cannot tell which disk" and refused while the source disk has a mounted volume
   (as the brief asked). Recovering or imaging a mounted system disk to a NAS mount is
   therefore refused; Windows UNC paths are accepted. A disk holding an LVM PV shows no
   mount points, so an LVM destination is not refused for it (gap).
2. **Behaviour change on Windows:** `pm image disk:N <file on disk N>` and `pm rescue`
   outputs on the source disk are now refused (they were not checked before).
3. `disk_arg` uses a process-global registry filled by `main` (only when a `/dev/...`
   argument is present); library callers of `disk_arg` without registration get "not a
   whole disk of this system" for nodes.
4. `DiskInfo::display_name` (pm-core, not touched) still says "Uninitialized" for an Unknown
   table; the CLI/GUI append the unreadable note, and the GUI map label says "Unreadable".
5. macOS: nothing here was run on a Mac (fixtures, `cargo check` only). The `hdiutil`
   walk-through is unverified; `diskutil eraseVolume FAT32` on a 48 MB slice is assumed to
   work.
6. Not verified live: the GUI on Linux (only its unit tests ran in WSL), the new
   `refuse_files_on` path on a real mount, `pm doctor` on macOS.
7. clippy is not installed for the pinned toolchain here; not run.
