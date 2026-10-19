# Task 5 report: pm-platform

**Status:** DONE_WITH_CONCERNS (the concerns are small; see the end)
**Commit:** `05c964a feat(platform): Platform trait, mock, Windows enumeration/open/mount` on `dev`, on top of `cb50c57`. Task 4 GPT landed while I was working. The commit contains only `crates/pm-platform/**` and my part of `Cargo.lock`. The `pm-fs` hunk belongs to another agent and was left unstaged with `git apply --cached`.

## What was implemented

### Cross-platform
- **`src/traits.rs`**
  - Everything named in the brief, verbatim: `OpenMode`, `MountPoint`, `WriteSession`, `SmartAttr`, `SmartData`, `ExternalTool`, `Platform`.
  - Small additions:
    - `MountPoint::to_path_string()` returns `E:\` or `C:\mnt\x\`.
    - `ExternalTool::exe_name()`.
    - `fs_format_name(FsKind)` maps to the names FormatEx and format.com use (NTFS / FAT32 / FAT / exFAT / ReFS).
- **`src/lib.rs`**
  - `default_platform()`: `WindowsPlatform` on Windows, `NullPlatform` elsewhere.
  - `NullPlatform`: every call returns `Error::Unsupported`. `is_elevated` is false, `system_disk` is None, `free_letters` is empty.
  - `pub fn merge_volumes(&mut DiskInfo, &[VolumeInfo])` merges by `(disk, start)` and sets volume, mount points, label, fs and `used = total - free`. The mock and the Windows code share it, and Task 15's FilePlatform can use it too.
- **`src/aligned.rs`**
  - `RawSectorIo` trait plus `read_unaligned` / `write_unaligned`.
  - Reads use a bounce buffer that is sector-aligned and 4 KiB-aligned in memory. Writes do read-modify-write of the edge sectors only.
  - I/O is split into 8 MiB chunks.
  - Aligned requests go straight through. If one fails with `ERROR_INVALID_PARAMETER` (87), it is retried through the bounce buffer, as the brief asks.
  - It is generic so it can be unit-tested against a strict fake device.
- **`src/mock.rs` (`MockPlatform`)**
  - Disks are `Arc<Mutex<MemDevice>>` plus `DiskInfo` metadata. Volumes come from `add_volume`.
  - Every `enumerate_disks` re-reads the table with `read_table` and calls `to_disk_info`. If `read_table` fails, the caller-supplied `meta` table and partitions are kept.
  - `open_disk` returns a per-call-locking adapter, wrapped in `ReadOnly` for `OpenMode::ReadOnly`. `open_volume` returns a `SliceDevice` window onto the disk.
  - `begin_write` logs to `write_sessions_log()`. Its session's `commit_table` calls `write_table`.
  - `mount` / `unmount` maintain `mounted()` and the volume's `mount_points`.
  - `free_letters()` returns E, F, G minus mounted letters.
  - `format`, `shrink`, `extend`, `set_offline`, `rescan` and `run_tool` are recorded in `calls()`, e.g. `format v NTFS label=Data quick=true` or `run_tool BcdBoot ["C:\\Windows", ...]`. `format` also updates the volume's fs and label; shrink and extend update its len.
  - Extras for the executor tests in Task 14:
    - `disk_bytes(i)`.
    - `fail_calls_matching(prefix)`: recorded calls with that prefix return `Error::Platform`.

### Windows (`src/windows/`, all `#[cfg(windows)]`)
- **`ioctl.rs`**
  - The only module with `unsafe`, apart from none in `device.rs`. Every block has a `// SAFETY:` comment.
  - `Handle` is an owned handle, `Send`/`Sync`, closed on drop.
  - Wrappers return a raw error code (`Err(u32)`):
    - `CreateFileW`, `DeviceIoControl` (output buffer grows on `ERROR_INSUFFICIENT_BUFFER` / `ERROR_MORE_DATA`).
    - `ReadFile` / `WriteFile` with an `OVERLAPPED` that carries the offset, and `FlushFileBuffers`.
    - The volume APIs, `GetLogicalDrives`, `GetWindowsDirectoryW`, the `TokenElevation` query, `FormatMessageW`.
    - fmifs `FormatEx`: progress and done state live in statics behind a global lock (`FormatGuard`).
  - Structured IOCTL input and output is encoded and decoded as bytes, so no padding bytes are ever read.
- **`layout.rs`** holds the pure parsers and encoders:
  - Parsers for `DRIVE_LAYOUT_INFORMATION_EX`, `VOLUME_DISK_EXTENTS`, `STORAGE_DEVICE_DESCRIPTOR`, the alignment and trim descriptors, `DISK_GEOMETRY_EX`, `GET_LENGTH_INFORMATION` and `GET_DISK_ATTRIBUTES`.
  - Encoders for `STORAGE_PROPERTY_QUERY`, `SET_DISK_ATTRIBUTES` and `SHRINK_VOLUME_INFORMATION`.
  - Helpers: `guid_from_bytes` (mixed-endian, same result as `Uuid::from_fields`), `gpt_flags`, `bus_from_storage`, `fs_from_name`.
  - A test checks every offset I use against `offset_of!` and `size_of` on the real `windows-sys` structs.
- **`error.rs`**
  - `pub fn friendly_error(code)` covers the brief's table exactly, plus 2, 19, 112 and 1167. Other codes fall back to `FormatMessageW`, then to `"Windows error N"`.
  - `platform_error(code, context)` returns `Error::Platform { code, msg: "<context>: <friendly>" }`.
  - `io_error(code, offset, len)` returns `Error::Io` wrapping `from_raw_os_error`.
- **`device.rs` (`WinDevice`)**
  - `open(path, OpenMode)` uses `GENERIC_READ`, or `GENERIC_READ | GENERIC_WRITE` for read-write.
  - `len` comes from `IOCTL_DISK_GET_LENGTH_INFO`, falling back to the geometry DiskSize. `sector_size` is the geometry's BytesPerSector.
  - `read_at` / `write_at` go through `aligned`. Writing to a read-only device returns `Error::Invalid("read-only")`. `flush` calls `FlushFileBuffers`.
  - The handle is closed on drop (through `Handle`).
- **`disk.rs` (`enumerate_disks`)**
  - Probes `\\.\PhysicalDrive0..=63` and stops after 8 consecutive `ERROR_FILE_NOT_FOUND`.
  - Each drive is opened with `GENERIC_READ`. On access denied it is reopened with **0 access (query-only)**.
  - A disk whose geometry query fails is skipped with a warning.
  - Fields filled:
    - model, serial, bus and removable from `StorageDeviceProperty`.
    - Logical and physical sector size from `StorageAccessAlignmentProperty`, falling back to geometry.
    - trim from `StorageDeviceTrimProperty`.
    - offline and read_only from `IOCTL_DISK_GET_DISK_ATTRIBUTES`.
    - table, disk_id and partitions from `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`.
    - `system` = whether `index == system_disk()`.
  - Layout parsing:
    - MBR: empty slots and extended or EBR-link entries (0x05/0x0F/0x85) are skipped. `logical` means layout slot ≥ 4. Numbers follow pm-parttable's convention: slot 1–4 for primaries, 5+ for logicals. `active` is BootIndicator.
    - GPT: type, guid, UTF-16 name, and the attribute bits 0 / 60 / 62 / 63 mapped to required / read_only / hidden / no_letter.
    - RAW: `TableKind::None`.
  - Enrichment, only when the handle is readable: if `read_table` succeeds and agrees on the table kind, partition numbers are aligned to its entry numbers by matching start and length.
  - Fallback, only when the layout IOCTL fails and the disk is readable: use `read_table`. If that returns `Unsupported`, read sector 0 and report `Gpt` when `is_protective_mbr` matches.
  - Finally, volumes are merged in.
- **`volume.rs`**
  - Volumes come from `FindFirstVolumeW` / `FindNextVolumeW`, and mount points from `GetVolumePathNamesForVolumeNameW`.
  - Extents use a query-only handle and `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`. With more than one extent, disk is None and start/len are from the first extent.
  - Label and fs come from `GetVolumeInformationW`. RAW or failing volumes get fs None.
  - total/free come from `GetDiskFreeSpaceExW` on the first mount point, or on the GUID path if there is none.
- **`session.rs` (`WindowsWriteSession`)**
  - `begin_write`, for each volume on the disk:
    - open it read-write, then `FSCTL_LOCK_VOLUME` (5 tries, 200 ms apart), then `FSCTL_DISMOUNT_VOLUME`.
    - If a lock fails, the volumes already locked are released and it returns `Error::Busy("<mount points or volume id>")`.
  - Then it opens the disk read-write. If the disk is not the system disk, it sets it offline with `persist=false`.
  - `commit_table` calls `write_table`, then flush, then `IOCTL_DISK_UPDATE_PROPERTIES`. On Windows it now dispatches to GPT too, since Task 4 has landed.
  - `finish` brings the disk back online, sends `UPDATE_PROPERTIES`, then unlocks and closes the volumes in reverse order. It runs every step and returns the first error.
  - `Drop` does the same restore if `finish` was not called.
- **`mount.rs`**
  - `mount` calls `SetVolumeMountPointW(mp\, volume\)`.
  - `unmount` first checks that the mount point belongs to that volume, then calls `DeleteVolumeMountPointW`.
  - `free_letters` reads the `GetLogicalDrives` mask and offers C to Z (never A or B).
  - `set_offline` uses the attribute IOCTL with **persist=true**, then `UPDATE_PROPERTIES`.
  - `rescan` sends `UPDATE_PROPERTIES`.
- **`format.rs`**
  - `format` uses an existing mount point. If the volume has none, it assigns the highest free letter temporarily and removes it afterwards.
  - fmifs `FormatEx` runs on a scoped worker thread. The calling thread polls the percentage every 100 ms and reports it to the `ProgressSink`.
  - If fmifs fails to load, it falls back to `format.com X: /FS:.. [/Q] /V:label [/A:n] /Y`.
  - `shrink_volume` sends `FSCTL_SHRINK_VOLUME` ShrinkPrepare, then ShrinkCommit, with ShrinkAbort if the commit fails.
  - `extend_volume` sends `FSCTL_EXTEND_VOLUME` with the new sector count.
- **`elevation.rs`**
  - `is_elevated` uses `TokenElevation`.
  - `find_system_disk` takes `GetWindowsDirectoryW`, opens `\\.\C:` query-only and reads its first extent's disk number. `WindowsPlatform` caches the result in a `OnceLock`.
- **`mod.rs` (`WindowsPlatform`)**
  - `smart()` returns `Ok(None)` with a `// Task 17` comment.
  - `run_tool` resolves `%SystemRoot%\System32\<exe>` and uses `CREATE_NO_WINDOW`. It returns stdout plus stderr. A non-zero exit gives `Error::Platform { code: exit, msg }`.

## Tests (RED → GREEN)
- `tests/mock.rs`: `mock_enumerates_added_disks`, `mock_mount_unmount_tracked`, `mock_begin_write_logs_disk`, `mock_open_disk_returns_device_with_len`.
  - RED: written first. The build failed with `unresolved import pm_platform::MockPlatform`.
  - GREEN: 4/4 pass.
- `windows::layout` tests (11).
  - RED: written against `unimplemented!()` stubs. 8 failed and 2 passed: `offsets_match_windows_sys`, which confirmed the hand-computed offsets, and the GUID test.
  - GREEN: all pass.
  - The encoder and small-parser test was added afterwards together with its code; it is not RED-first.
- `aligned` tests (7). I wrote them together with the implementation, not RED-first. They cover:
  - unaligned reads and writes, neighbours preserved, 4Kn sectors;
  - multi-chunk transfers larger than 8 MiB;
  - the fallback on a misaligned buffer address (error 87);
  - out-of-range requests and empty requests.
- Other unit tests:
  - friendly-error table, system-message fallback, and context and offsets in errors;
  - wide strings, and `PhysicalDrive999` giving `ERROR_FILE_NOT_FOUND`;
  - system disk found unelevated;
  - free-letter mask, volume paths, NullPlatform, MountPoint strings, fs names;
  - System32 tool resolution, and `run_tool(BcdBoot, [])` giving `Err(Platform { code: 87 })`.
- Result: `cargo test -p pm-platform` gives **37 passed** (33 lib + 4 mock), 2 ignored, 0 warnings. `cargo build --workspace` has 0 warnings. Clippy is not installed for this toolchain.

## Live checks (session NOT elevated)
`cargo test -p pm-platform --test windows_live -- --ignored --nocapture`:
- `live_enumerate_disks_nonempty` **passes unelevated**. It found three disks:
  - Disk 0: Fanxiang S880E 2TB, NVMe, GPT, system, trim, 512/4096 sectors. Four partitions: ESP on D:, MSR, C: "Windows" NTFS, WinRE.
  - Disk 1: LaCie USB, GPT. EFI partition, plus an NTFS F: partition.
  - Disk 2: Netac USB, MBR signature 190038E4, exFAT on G:.
  - I cross-checked against PowerShell `Get-Disk` and `Get-Partition`. The disk GUIDs, partition GUIDs, GPT types, offsets and friendly names all match exactly.
- `live_open_disk0_reads_sector0` **fails as expected unelevated**: `Platform { code: 5, msg: "open \\.\PhysicalDrive0: Access denied – run as administrator" }`. Elevated runs are not verified; the test header documents the command.
- Fields that need elevation:
  - `GENERIC_READ` on `PhysicalDriveN` is denied without admin, so enumeration falls back to a query-only handle (access 0).
  - These work with the query-only handle: geometry (size and sector), storage properties (model, serial, bus, alignment, trim), disk attributes, the drive layout (all partitions), volume extents, and the system disk.
  - These need elevation:
    - `IOCTL_DISK_GET_LENGTH_INFO`; geometry DiskSize is used instead and gave the identical value here.
    - the `read_table` enrichment and fallback.
    - all I/O, write sessions, mount/unmount, set_offline, rescan, format, shrink and extend.

## Deviations / decisions (please check)
1. **Bus-type numbers.** The dispatch message gave "10 Virtual / 12 FileBackedVirtual → Virtual, 14 Sd", but windows-sys defines Sas=10, Sd=12, Mmc=13, Virtual=14 and FileBackedVirtual=15. I used the named constants: Nvme; Sata and Ata → Sata; Usb; Scsi and Sas → Scsi; Sd and Mmc → Sd; Virtual and FileBackedVirtual → Virtual; everything else Unknown.
2. **`RecognizedPartition` is not used as a filter.** Filtering on it would hide Linux and other partitions Windows does not recognise, which a partition manager must show. Only empty and extended/link entries are skipped.
3. **MBR `logical` is based on the layout slot (≥ 4), not on `PartitionNumber > 4`.** Windows numbers partitions sequentially, so a logical can have number 3. MBR numbers follow pm-parttable's slot convention so they match `read_table`. GPT keeps Windows' numbers; when elevated they are re-aligned to `read_table`.
4. **Sector-0 protective-MBR detection is only a fallback**, used when the layout IOCTL fails. The layout's PartitionStyle is authoritative otherwise.
5. **FormatEx callback.** It returns `BOOLEAN` (u8), and the DONE payload is read as a single byte (BOOLEAN). Reading one byte is also correct for a BOOL.
6. **`set_offline` (the public API) persists the change**, which matches diskpart "offline disk". The write session's offline uses `persist=false`.
7. **Offline failure in a session is not fatal.** If taking the disk offline fails during `begin_write`, a warning is logged and the session continues, because the locked and dismounted volumes already give exclusive access.
8. **Mock extras not in the brief:** `fail_calls_matching`, and `set_offline` / `rescan` being recorded in `calls()`.

## Concerns
- Nothing that needs elevation has run live: raw reads, write sessions (lock, dismount, offline, commit, finish), mount/unmount, FormatEx (including the progress poller and the temporary letter), and shrink/extend. Only the unit-tested pieces and the unelevated enumeration are verified. Before Task 14 relies on them, run the ignored tests elevated, and ideally exercise a write session on a scratch VHD.
- Multi-extent volumes (spanned dynamic disks) have `disk = None`, so `begin_write` does not lock them.
- No cross-check on non-Windows: the Linux target is not installed. The `cfg` boundaries were checked by reading the code. `aligned`, `mock`, `traits` and `NullPlatform` have no Windows dependencies.
- The `windows::layout` module is public (pure parsers); `windows::ioctl` is private.
- This machine's Disk 0 partition GUIDs look timestamp-based and its recovery partition has no attribute bits. That matches Windows' own report, so it is not a parser bug.

## Files
- `crates/pm-platform/Cargo.toml`
- `crates/pm-platform/src/{lib.rs, traits.rs, mock.rs, aligned.rs}`
- `crates/pm-platform/src/windows/{mod.rs, ioctl.rs, layout.rs, error.rs, device.rs, disk.rs, volume.rs, session.rs, mount.rs, format.rs, elevation.rs}`
- `crates/pm-platform/tests/{mock.rs, windows_live.rs}`
- `Cargo.lock` (pm-platform, log, windows-sys 0.59 entries only)

## Fix round 1

Review findings addressed in commit `3f571b9`:

1. **DLL planting and executable hijack**
   - `ioctl::format_ex` now loads `fmifs.dll` with `LoadLibraryExW(.., LOAD_LIBRARY_SEARCH_SYSTEM32)`.
   - Tool paths come from the new `ioctl::system_directory()` (`GetSystemDirectoryW`). The environment (`SystemRoot`, `PATH`) is no longer read. If the API fails, `run_tool` returns an error instead of guessing a path.
   - `ioctl::windows_directory()` now uses `GetSystemWindowsDirectoryW`; `system_disk` relies on it.
   - Stale `GetLastError` fix: a `directory_result` helper reads `GetLastError` only when the call returns 0. When the buffer is too small (`n >= len`), it returns `ERROR_INSUFFICIENT_BUFFER`.
   - Tests:
     - `ioctl::tests::system_directories_come_from_the_api`: System32 is under the Windows directory and `fmifs.dll` exists there.
     - `ioctl::tests::directory_result_distinguishes_too_small_buffer`.
     - `windows::tests::tools_resolve_in_system32` now unwraps the `Result` and asserts the path is absolute.
2. **The previous offline state is restored correctly**
   - `begin_write` reads `IOCTL_DISK_GET_DISK_ATTRIBUTES` first.
   - A new pure function, `should_take_offline(is_system, Option<attrs>)`, makes the decision: offline only if the disk is not the system disk and is known to be online now. If the attributes cannot be read, the disk is left untouched.
   - `offlined`, and so the online step in `finish`/Drop, is only set when this session actually took the disk offline.
   - Test: `session::tests::offline_only_when_online_and_not_system`.
3. **Spanned volumes and volumes with unknown extents**
   - `begin_write` no longer uses `enumerate_volumes` and its first-extent `disk` field. It lists every volume and queries its extents directly (`query_extents`).
   - A new pure function, `volume_action(result, disk, name)`, decides per volume:
     - **Lock** if ANY extent is on the target disk, including spanned volumes.
     - **Skip** if all extents are on other disks.
     - **Refuse** (`Error::Busy("cannot determine extents of <mount points or id> (<friendly error>)")`) if the query fails, after releasing the volumes already locked.
   - One deliberate exception to "refuse on any failure", please check it: `ERROR_INVALID_FUNCTION`, `ERROR_NOT_SUPPORTED` and `ERROR_NOT_READY` are **skipped**. These are the codes for volumes that are not backed by the volume manager (CD/DVD class drivers) or that have no media. Such volumes have no mounted filesystem that a raw write to our disk could corrupt, while refusing on them would block every session on machines with an empty optical drive or card reader. Every other code refuses. If you want a strict "refuse on any failure", delete that one match arm.
   - Tests: `session::tests::volumes_with_any_extent_on_the_disk_are_locked` and `session::tests::unknown_extents_refuse_the_session`.
4. **The default test suite no longer launches System32 tools**
   - `run_tool` now delegates to `run_exe(path, name, args)`.
   - `run_tool_reports_nonzero_exit` (which ran bcdboot) is replaced by `run_exe_captures_output_and_exit_code`. It runs only `cmd.exe /c echo hello` and `cmd.exe /c "echo oops 1>&2 & exit 3"`, and expects `Err(Platform { code: 3, msg: "cmd.exe failed: ...oops..." })`.
   - No other test executes a tool.

Verification:
- `cargo test -p pm-platform`: `test result: ok. 38 passed` (lib) and `ok. 4 passed` (mock), 2 ignored, 0 warnings.
- `cargo build --workspace`: 0 warnings.
- `cargo test -p pm-platform --test windows_live -- --ignored`: `live_enumerate_disks_nonempty` passes unelevated. `live_open_disk0_reads_sector0` fails with code 5 (Access denied), as expected without elevation.
- The new session logic (lock by any extent, conditional offline) is covered only by the pure-function tests. Like the rest of the write-session path, it has not run against a real disk, because that needs elevation.
