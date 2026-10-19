# Partition Manager Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust, Windows-first partition manager with a wizard GUI: enumerate/manage/mount partitions, clone and image disks (raw/VHD/VMDK/qcow2), migrate Windows to a new disk, ddrescue-style rescue copy, partition/file recovery, and disk analysis.

**Architecture:** Cargo workspace of focused crates; only `pm-platform` touches the OS, every engine works on the `BlockDevice` trait so it is testable on files and memory; `pm-ops` holds a pending-operation queue simulated in memory and applied through lock/dismount write sessions; `pm-cli` and `pm-gui` are thin front ends over `pm-ops`.

**Tech Stack:** Rust 2021 (stable; on this machine the `stable-x86_64-pc-windows-gnu` toolchain is used because no MSVC toolset is installed), `windows-sys 0.59`, `eframe/egui 0.29`, `clap 4`, `serde/serde_json`, `thiserror`, `crc32fast`, `uuid`, `byteorder`-free manual LE helpers, `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-10-19-partition-manager-design.md`

## Global Constraints

- Workspace root `F:\Apps\PartitionManager`, `rust-toolchain.toml` pins `channel = "stable-x86_64-pc-windows-gnu"` (switch to msvc when Build Tools exist). Build from PowerShell: `cargo build`, `cargo test --workspace`.
- Crate names: `pm-core, pm-platform, pm-parttable, pm-fs, pm-image, pm-recover, pm-analyze, pm-ops, pm-cli, pm-gui`; lib names use underscores.
- No OS API outside `pm-platform`. No `unsafe` outside `pm-platform` (and the fmifs FFI there).
- All sizes in **bytes** (`u64`) in public APIs; sectors only inside parsers. Alignment default 1 MiB.
- Human sizes use binary units shown as `KB/MB/GB/TB` like the screenshots (1 GB = 1024³), format `"1858.19 GB"` (two decimals), via `pm_core::units::format_size`.
- Every long operation takes `&dyn ProgressSink` and `&CancelToken`; check cancel at least once per 64 MiB.
- Nothing ever writes to a source device of a clone/rescue/recovery. Read-only opens are the default; `OpenMode::ReadWrite` must be explicit.
- Commit after every task with a conventional-commit message ending in the attribution line `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.

## Review Focus

1. A 4Kn disk (sector 4096) with a GPT: header at LBA1 = byte 4096, entries sized by header fields, not assumed 512. Test in Task 4 (`gpt_roundtrip_4k_sectors`).
2. A rescue source that errors forever on one sector must still finish with that sector marked `-` and the mapfile resumable; no infinite loop. Test in Task 11 (`rescue_permanent_bad_sector_terminates`).
3. Clone target smaller than the source's *used* data must be refused at planning time, not at write time. Test in Task 14 (`plan_clone_rejects_small_target`).
4. The running system disk must never be a destination of clone/wipe/restore/initialize; planner error, GUI red warning. Test in Task 14 (`planner_guards_system_disk`).
5. NTFS with `$MFT` fragmented (data runs across several extents) and attribute lists: used map and file read must follow runs, not assume contiguity. Test in Task 7 (`ntfs_fragmented_mft_runs`).

---

## Phase 1 — Foundation

### Task 1: Workspace scaffold and pm-core device layer

**Files:**
- Create: `Cargo.toml` (workspace), `rust-toolchain.toml`, `README.md`, `crates/pm-core/Cargo.toml`, `crates/pm-core/src/lib.rs`, `crates/pm-core/src/error.rs`, `crates/pm-core/src/device.rs`, `crates/pm-core/src/progress.rs`, `crates/pm-core/src/units.rs`
- Test: inline `#[cfg(test)]` in each module

**Interfaces (Produces):**
```rust
// error.rs
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at offset {offset} (len {len}): {source}")]
    Io { offset: u64, len: usize, #[source] source: std::io::Error },
    #[error("parse error: {0}")] Parse(String),
    #[error("unsupported: {0}")] Unsupported(String),
    #[error("cancelled")] Cancelled,
    #[error("platform error {code}: {msg}")] Platform { code: u32, msg: String },
    #[error("device busy: {0}")] Busy(String),
    #[error("invalid: {0}")] Invalid(String),
    #[error("not found: {0}")] NotFound(String),
}
impl Error { pub fn io(offset: u64, len: usize, e: std::io::Error) -> Self; }

// device.rs
pub trait BlockDevice: Send {
    fn len(&self) -> u64;
    fn sector_size(&self) -> u32;             // default 512
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;  // exact read; Err if out of range
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    fn is_read_only(&self) -> bool;
}
impl<D: BlockDevice + ?Sized> BlockDevice for Box<D>;   // forward
impl<D: BlockDevice + ?Sized> BlockDevice for &mut D;   // forward
pub struct MemDevice { .. }  // new(len: u64), from_vec(Vec<u8>), with_sector_size(self, u32), as_slice(&self)->&[u8]
pub struct FileDevice { .. } // open(path, read_only: bool) -> Result<Self>; create(path, len: u64) -> Result<Self>;
                             // uses seek+read/write (std::fs::File); on windows use FileExt::seek_read/seek_write
pub struct SliceDevice<D: BlockDevice> { .. } // new(inner: D, offset: u64, len: u64) -> Result<Self>; bounds-checked
pub struct ReadOnly<D: BlockDevice>(pub D);   // write_at -> Err(Invalid("read-only")), is_read_only()==true
pub fn copy_range(src: &mut dyn BlockDevice, dst: &mut dyn BlockDevice, src_off: u64, dst_off: u64, len: u64, buf_size: usize) -> Result<()>;

// progress.rs
#[derive(Clone, Debug, Default)] pub struct ProgressReport { pub phase: String, pub done: u64, pub total: u64, pub rate_bps: f64, pub eta_secs: Option<u64>, pub detail: String }
pub trait ProgressSink: Send + Sync { fn report(&self, p: &ProgressReport); }
pub struct NullProgress; pub struct FnProgress<F: Fn(&ProgressReport)+Send+Sync>(pub F);
pub struct ProgressTracker { .. } // new(sink: &dyn ProgressSink, phase: &str, total: u64); add(&mut self, n: u64) computes rate (moving window) and eta, reports at most every 100 ms; finish()
#[derive(Clone, Default)] pub struct CancelToken(Arc<AtomicBool>); // cancel(), is_cancelled(), check() -> Result<()> (Err(Cancelled))

// units.rs
pub fn format_size(bytes: u64) -> String;          // 0 -> "0 B"; 1015.63 MB style; units B,KB,MB,GB,TB,PB binary
pub fn parse_size(s: &str) -> Result<u64>;         // "100M", "1.5GiB", "512", "2T"
pub const MIB: u64 = 1 << 20; pub const GIB: u64 = 1 << 30;
pub fn align_up(v: u64, a: u64) -> u64; pub fn align_down(v: u64, a: u64) -> u64;
```

- [ ] Write `Cargo.toml` workspace with `members = ["crates/*"]`, `[workspace.dependencies]` for thiserror 1, serde 1 (derive), serde_json 1, uuid 1 (v4, serde), crc32fast 1, tempfile 3, clap 4 (derive), log 0.4, env_logger 0.11, windows-sys 0.59, eframe 0.29; `[profile.release] opt-level=3, lto="thin", strip=true`.
- [ ] Write `rust-toolchain.toml` with `[toolchain] channel = "stable-x86_64-pc-windows-gnu"`.
- [ ] Tests: `mem_device_roundtrip`, `read_past_end_is_error`, `slice_device_offsets_and_bounds`, `readonly_rejects_write`, `file_device_create_open_roundtrip` (tempfile), `copy_range_copies_exact_bytes`, `format_size_matches_screenshots` (`1015.63 MB` for 1_065_000_000-ish → use exact: `format_size(1_858_190_000_000)`? no — assert `format_size(1024*1024*1024*1858 + 194_000_000) == "1858.18 GB"` style; pick values and assert), `parse_size_variants`, `align_helpers`, `cancel_token_check_errs`.
- [ ] Run `cargo test -p pm-core` → all pass. Commit `feat(core): workspace scaffold and block device layer`.

### Task 2: pm-core domain model

**Files:**
- Create: `crates/pm-core/src/model.rs`, `crates/pm-core/src/fs_kind.rs`, `crates/pm-core/src/ptype.rs`
- Modify: `crates/pm-core/src/lib.rs` (re-export `model::*`, `fs_kind::FsKind`, `ptype::*`)

**Interfaces (Produces):**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FsKind { Ntfs, Fat12, Fat16, Fat32, ExFat, ReFs, Ext2, Ext3, Ext4, Btrfs, Xfs, HfsPlus, Apfs, Luks, LinuxSwap, Iso9660, Udf, Unknown }
impl FsKind { pub fn display_name(&self) -> &'static str /* "NTFS","FAT32","exFAT","Ext4"... */; pub fn windows_mountable(&self) -> bool /* Ntfs,Fat*,ExFat,ReFs */ }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum TableKind { Mbr, Gpt, None, Unknown }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum PartitionType { Mbr(u8), Gpt(uuid::Uuid) }
impl PartitionType { pub fn name(&self) -> String; /* "GPT (EFI system partition)", "GPT (Microsoft reserved)", "GPT (basic data)", "GPT (recovery)", "Linux filesystem", "NTFS/exFAT (0x07)" ... */ pub fn is_efi(&self)->bool; pub fn is_msr(&self)->bool; pub fn is_recovery(&self)->bool; pub fn is_data(&self)->bool; pub fn is_extended_mbr(&self)->bool }
pub mod gpt_types { pub const EFI_SYSTEM: Uuid; pub const MSR: Uuid; pub const BASIC_DATA: Uuid; pub const WINDOWS_RECOVERY: Uuid; pub const LINUX_FS: Uuid; pub const LINUX_SWAP: Uuid; pub const LINUX_LVM: Uuid; pub const APPLE_HFS: Uuid; pub const APPLE_APFS: Uuid; pub const BIOS_BOOT: Uuid; }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum DiskBus { Nvme, Sata, Usb, Scsi, Sd, Virtual, Unknown }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)] pub struct DiskFlags { pub removable: bool, pub system: bool, pub offline: bool, pub read_only: bool, pub trim: bool }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)] pub struct PartFlags { pub active: bool, pub hidden: bool, pub required: bool, pub no_letter: bool, pub read_only: bool }
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)] pub struct VolumeId(pub String); // Windows: \\?\Volume{guid}\ ; images: "img:<path>#<index>"
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PartitionInfo { pub number: u32, pub start: u64, pub len: u64, pub ptype: Option<PartitionType>, pub guid: Option<Uuid>, pub name: String, pub fs: Option<FsKind>, pub label: String, pub mount_points: Vec<String>, pub used: Option<u64>, pub flags: PartFlags, pub volume: Option<VolumeId>, pub logical: bool }
impl PartitionInfo { pub fn end(&self)->u64; pub fn letter(&self)->Option<char>; pub fn status_text(&self)->String /* "Active & System", "Boot", "None" */ }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiskInfo { pub index: u32, pub path: String, pub model: String, pub serial: String, pub bus: DiskBus, pub size: u64, pub sector_size: u32, pub physical_sector_size: u32, pub table: TableKind, pub disk_id: String /* mbr sig hex or gpt guid */, pub flags: DiskFlags, pub partitions: Vec<PartitionInfo> }
impl DiskInfo { pub fn free_ranges(&self, min_len: u64) -> Vec<(u64,u64)> /* (start,len) gaps >= min_len, honoring 1MiB front reserve and GPT 34-sector tail */; pub fn display_name(&self)->String /* "Disk 1 (Fanxiang S880E 2TB, GPT, 1.82 TB)" */ }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VolumeInfo { pub id: VolumeId, pub disk: Option<u32>, pub start: u64, pub len: u64, pub fs: Option<FsKind>, pub label: String, pub mount_points: Vec<String>, pub total: u64, pub free: u64 }
```

- [ ] Tests: `ptype_names_for_known_guids`, `free_ranges_finds_gaps` (disk 100 MiB with parts at 1–50 MiB and 60–90 MiB → gaps (50MiB,10MiB) and (90MiB, ~10MiB-tail)), `display_name_format`, `serde_roundtrip_diskinfo`.
- [ ] `cargo test -p pm-core` passes. Commit `feat(core): domain model`.

### Task 3: pm-parttable MBR

**Files:**
- Create: `crates/pm-parttable/Cargo.toml`, `src/lib.rs`, `src/table.rs`, `src/mbr.rs`, `src/bytes.rs` (LE helpers `rd_u16/rd_u32/rd_u64/wr_*`)
- Test: `crates/pm-parttable/tests/mbr.rs`

**Interfaces (Produces):**
```rust
// table.rs
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry { pub number: u32, pub start: u64, pub len: u64, pub ptype: PartitionType, pub guid: Option<Uuid>, pub name: String, pub attrs: u64 /* GPT attrs or MBR: bit0=active */, pub logical: bool }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Table { pub kind: TableKind, pub sector_size: u32, pub disk_len: u64, pub disk_guid: Option<Uuid>, pub mbr_signature: u32, pub bootcode: Vec<u8> /* 440 bytes, kept */, pub entries: Vec<Entry>, pub gpt_first_usable: u64, pub gpt_last_usable: u64 /* bytes */ }
impl Table { pub fn empty(kind: TableKind, disk_len: u64, sector_size: u32) -> Table; pub fn free_ranges(&self, min_len: u64) -> Vec<(u64,u64)>; pub fn validate(&self) -> Result<()> /* overlaps, bounds, max entries (MBR 4 primaries), alignment to sector */; pub fn next_number(&self)->u32; pub fn to_disk_info(&self, base: DiskInfo) -> DiskInfo; pub fn regenerate_ids(&mut self) /* new disk guid+part guids or new mbr sig */ }
pub fn read_table(dev: &mut dyn BlockDevice) -> Result<Table>;   // dispatch: GPT if valid header at LBA1 (or backup), else MBR if 0x55AA and not protective, else kind None
pub fn write_table(dev: &mut dyn BlockDevice, t: &Table) -> Result<()>;
// mbr.rs
pub fn parse_mbr(dev: &mut dyn BlockDevice) -> Result<Table>;   // follows EBR chain for type 0x05/0x0F/0x85
pub fn write_mbr(dev: &mut dyn BlockDevice, t: &Table) -> Result<()>; // primaries + EBR chain (logical entries become EBRs 1 MiB-aligned inside the extended)
pub fn is_protective_mbr(sector: &[u8]) -> bool;
pub fn chs_from_lba(lba: u64) -> [u8;3];  // clamp 0xFE,0xFF,0xFF beyond 8 GiB
```

- [ ] Tests: `mbr_parse_four_primaries`, `mbr_parse_extended_chain_two_logicals`, `mbr_roundtrip_write_then_parse_equal`, `mbr_preserves_bootcode_and_signature`, `mbr_rejects_no_55aa`, `mbr_overlap_validate_fails`.
- [ ] `cargo test -p pm-parttable` passes. Commit `feat(parttable): MBR parse/write`.

### Task 4: pm-parttable GPT, conversion, lost-table search

**Files:**
- Create: `crates/pm-parttable/src/gpt.rs`, `src/convert.rs`, `src/search.rs`
- Modify: `src/table.rs` (`read_table`/`write_table` dispatch), `src/lib.rs`
- Test: `crates/pm-parttable/tests/gpt.rs`, `tests/convert.rs`, `tests/search.rs`

**Interfaces (Produces):**
```rust
pub fn parse_gpt(dev: &mut dyn BlockDevice) -> Result<Table>;   // primary; on CRC/signature failure try backup at last LBA; Error::Parse if both bad
pub fn write_gpt(dev: &mut dyn BlockDevice, t: &Table) -> Result<()>; // protective MBR (keep bootcode), primary hdr LBA1, entries LBA2.., backup entries + header at end; 128 entries × 128 bytes; CRC32 (crc32fast)
pub struct GptHealth { pub primary_ok: bool, pub backup_ok: bool, pub entries_crc_ok: bool }
pub fn gpt_health(dev: &mut dyn BlockDevice) -> Result<GptHealth>;
pub fn gpt_repair_backup_from_primary(dev) / primary_from_backup(dev) -> Result<()>;
// convert.rs
pub fn mbr_to_gpt(t: &Table) -> Result<Table>;   // maps 0x07→BASIC_DATA, 0x0B/0x0C→BASIC_DATA, 0x82→LINUX_SWAP, 0x83→LINUX_FS, 0xEF→EFI_SYSTEM, 0x27→WINDOWS_RECOVERY; logicals become primaries; Err(Invalid) if first partition starts < 1 MiB (no room for entries at 512-sector disks needs 34 sectors = 17 KiB → fine; check last partition end > last_usable → Err)
pub fn gpt_to_mbr(t: &Table) -> Result<Table>;   // Err(Invalid) if > 4 entries or disk > 2 TiB; MSR dropped
pub fn align_table(t: &Table, align: u64) -> Table; // moves starts up to alignment, shrinks len accordingly; returns new table (data move is pm-ops' job)
// search.rs
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct Candidate { pub start: u64, pub len: u64, pub fs: FsKind, pub label: String, pub source: CandidateSource /* BootSector | BackupBootSector | GptBackupEntry */, pub confidence: u8 /* 0-100 */ }
pub fn search_lost(dev: &mut dyn BlockDevice, probe: &dyn Fn(&mut dyn BlockDevice, u64) -> Option<(FsKind, String, u64 /*fs len*/)>, step: u64, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<Vec<Candidate>>;
// `probe` is injected (pm-fs::detect) to keep pm-parttable free of pm-fs. Scan every `step` bytes (default 1 MiB) plus backup-GPT entries; NTFS backup boot sector at fs end gives BackupBootSector candidates. Dedupe overlapping candidates, highest confidence wins.
```

- [ ] Tests: `gpt_roundtrip_512`, `gpt_roundtrip_4k_sectors` (MemDevice with sector 4096; header at 4096, entries at 8192), `gpt_uses_backup_when_primary_corrupt`, `gpt_rejects_both_corrupt`, `gpt_protective_mbr_written`, `gpt_names_utf16`, `mbr_to_gpt_maps_types`, `gpt_to_mbr_rejects_five_entries`, `align_table_moves_starts`, `search_finds_fs_at_offset_with_injected_probe`.
- [ ] `cargo test -p pm-parttable` passes. Commit `feat(parttable): GPT, conversion, lost-partition search`.

### Task 5: pm-platform trait, mock, Windows enumeration + open + mount

**Files:**
- Create: `crates/pm-platform/Cargo.toml` (deps: pm-core, pm-parttable, windows-sys with features `Win32_Foundation, Win32_Storage_FileSystem, Win32_System_IO, Win32_System_Ioctl, Win32_Security, Win32_System_Threading, Win32_System_Diagnostics_Debug, Win32_System_LibraryLoader`, log), `src/lib.rs`, `src/traits.rs`, `src/mock.rs`, `src/windows/mod.rs`, `src/windows/disk.rs`, `src/windows/volume.rs`, `src/windows/ioctl.rs`, `src/windows/error.rs`, `src/windows/mount.rs`, `src/windows/device.rs` (`WinDevice: BlockDevice` over HANDLE with ReadFile/WriteFile at OVERLAPPED offset, sector-aligned bounce buffer), `src/windows/elevation.rs`
- Test: `crates/pm-platform/tests/mock.rs`; Windows tests gated `#[ignore]` (need admin) under `tests/windows_live.rs`

**Interfaces (Produces):**
```rust
#[derive(Clone, Copy, PartialEq, Eq)] pub enum OpenMode { ReadOnly, ReadWrite }
#[derive(Clone, Debug, PartialEq, Eq)] pub enum MountPoint { Letter(char), Folder(PathBuf) }
pub trait WriteSession: Send { fn disk(&self) -> u32; fn device(&mut self) -> &mut dyn BlockDevice; fn commit_table(&mut self, t: &Table) -> Result<()>; /* write via parttable then rescan */ fn finish(self: Box<Self>) -> Result<()>; /* unlock, online, rescan */ }
#[derive(Clone, Debug, Default, Serialize, Deserialize)] pub struct SmartAttr { pub id: u8, pub name: String, pub value: u8, pub worst: u8, pub threshold: u8, pub raw: u64 }
#[derive(Clone, Debug, Default, Serialize, Deserialize)] pub struct SmartData { pub healthy: Option<bool>, pub temperature_c: Option<u32>, pub power_on_hours: Option<u64>, pub attrs: Vec<SmartAttr>, pub nvme_percent_used: Option<u8> }
#[derive(Clone, Copy, Debug)] pub enum ExternalTool { BcdBoot, BootSect, Mbr2Gpt, FormatCom, Diskpart }
pub trait Platform: Send + Sync {
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>>;          // partitions filled from pm-parttable::read_table on the opened disk + volumes merged (mount points, label, used, fs via GetVolumeInformation)
    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>>;
    fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>>;
    fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>>;
    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>>;
    fn mount(&self, id: &VolumeId, at: &MountPoint) -> Result<()>;
    fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()>;
    fn set_offline(&self, disk: u32, offline: bool) -> Result<()>;
    fn rescan(&self, disk: u32) -> Result<()>;
    fn format(&self, id: &VolumeId, fs: FsKind, label: &str, cluster: Option<u32>, quick: bool, progress: &dyn ProgressSink) -> Result<()>;
    fn shrink_volume(&self, id: &VolumeId, new_len: u64) -> Result<()>;
    fn extend_volume(&self, id: &VolumeId, new_len: u64) -> Result<()>;
    fn smart(&self, disk: u32) -> Result<Option<SmartData>>;
    fn is_elevated(&self) -> bool;
    fn system_disk(&self) -> Option<u32>;
    fn free_letters(&self) -> Vec<char>;
    fn run_tool(&self, tool: ExternalTool, args: &[String]) -> Result<String>;
}
pub fn default_platform() -> Arc<dyn Platform>;   // Windows → WindowsPlatform; other OS → Err-returning Unsupported stub `NullPlatform`
pub struct MockPlatform { .. } // new(); add_disk(index, MemDevice, DiskInfo meta) ; add_volume(VolumeInfo, disk, start, len); set_system_disk(u32); mounted(&self)->Vec<(VolumeId, MountPoint)>; write_sessions_log()->Vec<u32>; format/shrink/extend record calls
```
Windows notes: `WinDevice` must handle `ERROR_INVALID_PARAMETER` on unaligned I/O by bouncing through a sector-aligned buffer; `enumerate_disks` iterates `\\.\PhysicalDrive0..=63` stopping after 8 consecutive `ERROR_FILE_NOT_FOUND`; `begin_write` = for every volume on the disk `FSCTL_LOCK_VOLUME` then `FSCTL_DISMOUNT_VOLUME` (Err(Busy) listing mount points if lock fails) and for non-system disks `IOCTL_DISK_SET_DISK_ATTRIBUTES` offline; `finish` reverses and calls `IOCTL_DISK_UPDATE_PROPERTIES`. `format` via `fmifs.dll!FormatEx` loaded with `LoadLibraryW`; fallback `format.com <letter>: /FS:x /Q /V:label /Y`. `system_disk` = disk extent of the volume holding `%SystemRoot%`. Error table in `error.rs`: 5→"Access denied – run as administrator", 32→"Volume in use", 1117→"I/O device error", 21→"Device not ready", 23→"Data error (CRC) – bad sector".

- [ ] Tests (mock): `mock_enumerates_added_disks`, `mock_mount_unmount_tracked`, `mock_begin_write_logs_disk`, `mock_open_disk_returns_device_with_len`.
- [ ] Live Windows tests (`#[ignore]`): `live_enumerate_disks_nonempty`, `live_open_disk0_reads_sector0`.
- [ ] `cargo test -p pm-platform` passes; `cargo test -p pm-platform -- --ignored` passes when run elevated (document). Commit `feat(platform): Platform trait, mock, Windows enumeration/open/mount`.

### Task 6: pm-cli foundation (`list`, `table`, `mount`, `unmount`, `doctor`)

**Files:**
- Create: `crates/pm-cli/Cargo.toml` (bin name `pm`), `src/main.rs`, `src/cli.rs` (clap), `src/cmd_list.rs`, `src/cmd_table.rs`, `src/cmd_mount.rs`, `src/cmd_doctor.rs`, `src/progress.rs` (stderr bar implementing `ProgressSink`), `src/device_arg.rs`
- Test: `crates/pm-cli/tests/cli.rs` (uses `assert_cmd`-free approach: `std::process::Command` on `env!("CARGO_BIN_EXE_pm")`)

**Interfaces (Produces):**
```rust
// device_arg.rs
pub enum DeviceArg { Disk(u32), Volume(String), File(PathBuf) }
pub fn parse_device(s: &str) -> Result<DeviceArg>;  // "disk:1", "D:", "\\?\Volume{..}\", "C:\img.raw", "file:path"
pub fn open_device(p: &dyn Platform, a: &DeviceArg, mode: OpenMode) -> Result<Box<dyn BlockDevice>>; // File → pm-image later; Task 6 opens raw FileDevice
```
Commands: `pm list [--json]` prints a table like the screenshot (Disk header line, then rows Partition/Capacity/Used/Unused/File system/Type/Status); `pm table <device> [--json]` prints the parsed `Table` of a disk or image file; `pm mount <volume> --letter X | --folder P`, `pm unmount <volume> --letter X`; `pm doctor` prints elevation, toolchain, disks seen, system disk, free letters.

- [ ] Tests: `cli_table_on_image_file_prints_gpt` (build a GPT image in a tempfile with pm-parttable, run `pm table file`), `cli_list_json_runs` (does not require admin: on error prints a friendly message and exits 2).
- [ ] `cargo test -p pm-cli` passes; `cargo run -p pm-cli -- doctor` prints something sensible. Commit `feat(cli): list/table/mount/unmount/doctor`.

## Phase 2 — Filesystems

### Task 7: pm-fs detection, FAT (12/16/32) parser + FAT32 formatter

**Files:**
- Create: `crates/pm-fs/Cargo.toml`, `src/lib.rs`, `src/detect.rs`, `src/bitmap.rs`, `src/traits.rs`, `src/fat/mod.rs`, `src/fat/bpb.rs`, `src/fat/dir.rs`, `src/fat/format.rs`, `src/util.rs`
- Test: `crates/pm-fs/tests/detect.rs`, `tests/fat.rs`

**Interfaces (Produces):**
```rust
// detect.rs
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct FsProbe { pub kind: FsKind, pub label: String, pub total: u64 /* fs size bytes */, pub used: Option<u64>, pub cluster: Option<u32>, pub uuid: String }
pub fn detect(dev: &mut dyn BlockDevice, offset: u64) -> Option<FsProbe>;   // reads up to 68 KiB; checks NTFS "NTFS    "@3, exFAT "EXFAT   "@3, FAT via BPB sanity + FAT12/16/32 by cluster count, ext magic 0xEF53 @ 0x400+0x38 (+feature flags → ext2/3/4), btrfs "_BHRfS_M" @ 0x10040, XFS "XFSB"@0, HFS+ "H+"/"HX" @0x400, APFS "NXSB"@0x20, ReFS "ReFS"@3 ("FSRS" @0x10), LUKS "LUKS\xba\xbe", swap "SWAPSPACE2" at page_end-10, ISO "CD001"@0x8001, UDF "BEA01"/"NSR0x"
pub fn probe_for_search(dev: &mut dyn BlockDevice, off: u64) -> Option<(FsKind, String, u64)>; // adapter for pm_parttable::search_lost
// bitmap.rs
#[derive(Clone, Debug)] pub struct UsedMap { pub unit: u32 /* bytes per bit */, pub bits: Vec<u8>, pub len: u64 /* fs bytes */ }
impl UsedMap { pub fn new(len, unit); pub fn set(&mut self, byte_off, byte_len); pub fn is_used(&self, byte_off)->bool; pub fn used_bytes(&self)->u64; pub fn ranges(&self) -> impl Iterator<Item=(u64,u64)> /* coalesced used (start,len) */; pub fn all_used(len)->UsedMap }
// traits.rs
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct Entry { pub name: String, pub path: String, pub is_dir: bool, pub size: u64, pub modified: Option<i64 /*unix secs*/>, pub id: u64 /* fs-specific: cluster / MFT ref / inode */, pub deleted: bool, pub attrs: u32 }
pub trait FileSystem: Send {
    fn kind(&self) -> FsKind; fn label(&self) -> String; fn total(&self) -> u64; fn cluster_size(&self) -> u32;
    fn root(&self) -> Entry; fn list(&mut self, dir: &Entry) -> Result<Vec<Entry>>;
    fn read_file(&mut self, e: &Entry, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn used_map(&mut self) -> Result<UsedMap>;
    fn deleted_entries(&mut self, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<Vec<Entry>>;
    fn extract(&mut self, e: &Entry, dest: &mut dyn std::io::Write) -> Result<u64>; // default impl via read_file
}
pub fn open(dev: Box<dyn BlockDevice>) -> Result<Box<dyn FileSystem>>;   // dispatch by detect(); Unsupported for kinds without a parser
// fat/format.rs
pub struct FormatOpts { pub label: String, pub cluster_size: Option<u32>, pub volume_id: Option<u32> }
pub fn format_fat32(dev: &mut dyn BlockDevice, opts: &FormatOpts) -> Result<()>;  // standard Microsoft layout: 32 reserved sectors, 2 FATs, FSInfo, backup boot at 6; cluster default by size table (<=8GiB 4K, <=16 8K, <=32 16K, else 32K); label in root dir + BPB
```
- [ ] Tests: `detect_recognizes_all_signatures` (synthesize minimal headers for each kind), `format_fat32_then_detect_and_list_empty_root`, `fat32_write_entries_with_builder_then_list` (a test helper writes short+LFN entries and FAT chains), `fat32_used_map_counts_reserved_fats_and_root`, `fat_deleted_entries_lists_e5`, `fat16_small_image_parses` (builder).
- [ ] `cargo test -p pm-fs` passes. Commit `feat(fs): detection, FAT parser, FAT32 formatter`.

### Task 8: pm-fs NTFS reader

**Files:**
- Create: `src/ntfs/mod.rs`, `src/ntfs/boot.rs`, `src/ntfs/mft.rs` (record fixups, attributes, attribute lists), `src/ntfs/runs.rs` (data run decoding), `src/ntfs/index.rs` (INDEX_ROOT/INDEX_ALLOCATION B-tree walk), `src/ntfs/fs.rs` (FileSystem impl), `tests/ntfs_builder.rs` (test-only synthetic NTFS image builder: boot sector, $MFT with records 0..=15 plus test files, $Bitmap, root index)
- Test: `tests/ntfs.rs`

**Interfaces (Produces):** `pub struct Ntfs` implementing `FileSystem`; `pub fn open_ntfs(dev: Box<dyn BlockDevice>) -> Result<Ntfs>`; `pub(crate) fn decode_runs(data: &[u8]) -> Result<Vec<(Option<u64> /*lcn, None = sparse*/, u64 /*clusters*/)>>`; deleted entries = MFT records with `flags & 1 == 0` that still have a `$FILE_NAME`; `used_map` from `$Bitmap` (record 6) data at cluster granularity; `read_file` handles resident, non-resident, sparse, compressed → `Unsupported("NTFS compressed")`.
- [ ] Tests: `decode_runs_examples` (spec examples incl. negative offsets), `ntfs_lists_root_and_reads_resident_file`, `ntfs_reads_nonresident_file_across_runs`, `ntfs_fragmented_mft_runs` ($MFT itself in two runs), `ntfs_used_map_from_bitmap`, `ntfs_deleted_record_listed`, `ntfs_attribute_list_followed`.
- [ ] `cargo test -p pm-fs` passes. Commit `feat(fs): NTFS reader`.

### Task 9: pm-fs exFAT and ext2/3/4 readers

**Files:**
- Create: `src/exfat/{mod.rs,boot.rs,dir.rs,fs.rs}`, `src/ext/{mod.rs,super_block.rs,inode.rs,extent.rs,dir.rs,fs.rs}`, test builders `tests/exfat_builder.rs`, `tests/ext_builder.rs`
- Test: `tests/exfat.rs`, `tests/ext.rs`

**Interfaces (Produces):** `pub struct ExFat`, `pub struct Ext` implementing `FileSystem`; `open()` dispatch updated. exFAT: allocation bitmap → `used_map`, up-case table ignored, entry sets (0x85/0xC0/0xC1), deleted = type bit 7 clear. ext: group descriptors (32/64-byte), block bitmaps → `used_map`, inodes with extent tree (depth ≥1) or indirect blocks, linear and htree dirs read linearly, deleted inodes via `dtime != 0` listed with name "inode-N" (names are unrecoverable in ext4; document).
- [ ] Tests: `exfat_lists_and_reads`, `exfat_used_map_from_bitmap`, `ext4_extent_file_read`, `ext2_indirect_blocks_read`, `ext_used_map_from_group_bitmaps`, `ext_deleted_inodes_listed`.
- [ ] `cargo test -p pm-fs` passes. Commit `feat(fs): exFAT and ext readers`.

## Phase 3 — Imaging

### Task 10: pm-image virtual disk formats

**Files:**
- Create: `crates/pm-image/Cargo.toml`, `src/lib.rs`, `src/format.rs`, `src/raw.rs`, `src/vhd.rs`, `src/vmdk.rs`, `src/qcow2.rs`, `src/sparse.rs` (shared "allocate-on-write block table" helper)
- Test: `tests/formats.rs`

**Interfaces (Produces):**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum ImageFormat { Raw, VhdFixed, VhdDynamic, VmdkFlat, VmdkSparse, Qcow2 }
impl ImageFormat { pub fn from_extension(p: &Path) -> Option<Self> /* .img/.raw/.dd→Raw, .vhd→VhdDynamic, .vmdk→VmdkSparse, .qcow2→Qcow2 */; pub fn extension(&self)->&'static str; pub fn display(&self)->&'static str }
pub struct CreateOpts { pub format: ImageFormat, pub capacity: u64, pub sparse: bool }
pub fn create(path: &Path, opts: &CreateOpts) -> Result<Box<dyn BlockDevice>>;
pub fn open(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>>;      // sniff: "conectix" footer → VHD; "KDMV" → VMDK sparse; descriptor text "# Disk DescriptorFile" → VMDK flat; "QFI\xfb" → qcow2; else raw
pub fn identify(path: &Path) -> Result<ImageFormat>;
pub fn convert(src: &Path, dst: &Path, fmt: ImageFormat, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<()>; // skips all-zero 1 MiB chunks on sparse formats
```
Format details to pin: VHD footer 512 B at end (cookie `conectix`, features 2, version 0x10000, data offset, timestamp secs since 2000-01-01, creator `pman`, geometry per VHD algorithm, type 2 fixed / 3 dynamic, checksum ones-complement), dynamic header `cxsparse` at 512, BAT entries big-endian sector offsets, block 2 MiB + sector bitmap (1 sector per 4096 sectors, padded to 512). VMDK sparse: `KDMV` LE header v1, flags 3, capacity, grainSize 128 sectors, descriptorOffset 1, descriptorSize 20 sectors, numGTEsPerGT 512, rgdOffset, gdOffset, overHead, embedded descriptor `createType="monolithicSparse"`, `RW <sectors> SPARSE "<file>"`, ddb geometry. VMDK flat: `<name>.vmdk` descriptor + `<name>-flat.vmdk`. qcow2: v3 header BE, cluster_bits 16, refcount_order 4, L1 size, refcount table 1 cluster + blocks allocated as needed, no features; compatible with qemu-img (document how to verify: `qemu-img check`).
- [ ] Tests per format: `create_write_reopen_read_<fmt>` (write at 0, at 2 MiB+7, at end-4 KiB; reopen; compare; unwritten reads zero), `vhd_footer_checksum_valid`, `vhd_dynamic_bat_grows`, `vmdk_sparse_header_fields`, `vmdk_flat_descriptor_text`, `qcow2_header_fields_and_refcounts_consistent` (walk refcount table and verify every allocated cluster has refcount 1), `identify_each_format`, `convert_raw_to_each_and_back_equal`.
- [ ] `cargo test -p pm-image` passes. Commit `feat(image): raw, VHD, VMDK, qcow2 virtual disks`.

### Task 11: pm-image rescue copier (ddrescue semantics)

**Files:**
- Create: `src/rescue/mod.rs`, `src/rescue/mapfile.rs`, `src/rescue/engine.rs`, `src/rescue/faulty.rs` (test device injecting errors; `#[cfg(any(test, feature="testing"))]`)
- Test: `tests/rescue.rs`, `tests/mapfile.rs`

**Interfaces (Produces):**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum BlockStatus { NonTried, NonTrimmed, NonScraped, Bad, Finished } // chars ? * / - +
pub struct MapFile { pub current_pos: u64, pub current_status: char, pub pass: u32, pub blocks: Vec<(u64, u64, BlockStatus)> }
impl MapFile { pub fn new(len: u64) -> Self; pub fn parse(s: &str) -> Result<Self>; pub fn to_string(&self) -> String /* GNU ddrescue format incl. "# Mapfile. Created by pm" header */; pub fn load(p: &Path) -> Result<Self>; pub fn save(&self, p: &Path) -> Result<()>; pub fn mark(&mut self, pos, len, st); pub fn ranges(&self, st: BlockStatus) -> Vec<(u64,u64)>; pub fn stats(&self) -> MapStats { rescued, non_tried, non_trimmed, non_scraped, bad_bytes, bad_areas } ; pub fn merge_coalesce(&mut self) }
#[derive(Clone, Debug)] pub struct RescueOpts { pub block_size: usize /* default 128 KiB */, pub sector: u32 /* 512 */, pub skip_size: u64 /* initial 64 KiB, doubles, max 1% of len */, pub max_retries: u32 /* 0 */, pub reverse: bool, pub no_trim: bool, pub no_scrape: bool, pub used_map: Option<UsedMap> /* skip unused */, pub zero_unused: bool, pub abort_on_error: bool /* plain clone mode */, pub mapfile: Option<PathBuf>, pub min_read_rate: Option<u64> }
#[derive(Clone, Debug, Default, Serialize, Deserialize)] pub struct RescueStats { pub rescued: u64, pub bad_bytes: u64, pub bad_areas: u64, pub read_errors: u64, pub elapsed_secs: f64, pub passes: u32 }
pub fn rescue(src: &mut dyn BlockDevice, dst: &mut dyn BlockDevice, dst_offset: u64, opts: &RescueOpts, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<RescueStats>;
// Phase order: copy (forward or reverse; on read error mark block NonTrimmed, jump skip_size, double skip up to max; reset skip after a good read) → trim (for each NonTrimmed range read sector-by-sector from the front until an error, then from the back until an error; middle becomes NonScraped) → scrape (every sector in NonScraped ranges, bad → Bad) → retry passes (re-read Bad sectors max_retries times, alternating direction). Mapfile saved every 30 s and at phase changes. Cancel leaves mapfile consistent. Progress phase strings: "Copying", "Trimming", "Scraping", "Retrying (pass N)".
pub fn copy_simple(src, dst, src_off, dst_off, len, used: Option<&UsedMap>, progress, cancel) -> Result<u64>; // abort_on_error shortcut used by clone
pub struct FaultyDevice { .. } // new(inner: MemDevice, bad_sectors: Vec<u64 /*byte offsets*/>, transient: HashMap<u64, u32 /*fail N times*/>)
```
- [ ] Tests: `mapfile_roundtrip_gnu_format` (parse a literal GNU mapfile sample, print, parse again equal), `mapfile_stats`, `rescue_clean_source_copies_all`, `rescue_bad_sector_isolated_to_one_sector` (bad at 1 MiB+512 → exactly 512 B marked `-`, everything else `+`), `rescue_permanent_bad_sector_terminates`, `rescue_transient_error_recovered_on_retry` (max_retries 2), `rescue_resume_from_mapfile_skips_finished` (count reads on inner device), `rescue_reverse_direction`, `rescue_used_map_skips_unused_and_zero_fills`, `copy_simple_aborts_on_error`.
- [ ] `cargo test -p pm-image` passes. Commit `feat(image): ddrescue-style rescue engine with mapfile`.

### Task 12: CLI imaging commands

**Files:**
- Create: `crates/pm-cli/src/cmd_clone.rs`, `cmd_image.rs` (`image`, `restore`, `convert`), `cmd_rescue.rs`, `cmd_probe.rs` (`pm probe <device> [--offset]` prints FsProbe), `cmd_ls.rs` (`pm ls <device>[:<part>] [path]`, `pm cat`, `pm extract` using pm-fs)
- Modify: `cli.rs`, `main.rs`, `device_arg.rs` (File → `pm_image::open`, `disk:N:P` partition slices via pm-parttable)
- Test: `tests/cli_image.rs`

Commands: `pm clone <src> <dst> [--smart|--sector] [--dst-offset N]`; `pm image <src> <file> [--format raw|vhd|vhd-fixed|vmdk|vmdk-flat|qcow2] [--smart]`; `pm restore <file> <dst> [--dst-offset]`; `pm convert <in> <out> --format X`; `pm rescue <src> <dst> <mapfile> [-b size] [-K skip] [-r retries] [-R] [-n] [-N]` (flags mirror ddrescue), prints a live status block like ddrescue (rescued, errsize, errors, rate, phase).
- [ ] Tests: `cli_image_and_restore_roundtrip_qcow2` (raw tempfile with pattern → image → restore → identical), `cli_convert_vhd_to_vmdk`, `cli_rescue_on_file_writes_mapfile`, `cli_ls_fat32_image_lists_files` (format via pm-fs, add files with test builder).
- [ ] Commit `feat(cli): clone, image, restore, convert, rescue, ls`.

## Phase 4 — Operations

### Task 13: pm-ops model, operation enum, planner/simulator

**Files:**
- Create: `crates/pm-ops/Cargo.toml`, `src/lib.rs`, `src/model.rs`, `src/operation.rs`, `src/planner.rs`, `src/sizing.rs`
- Test: `tests/planner.rs`

**Interfaces (Produces):**
```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize)] pub struct Model { pub disks: Vec<DiskInfo>, pub volumes: Vec<VolumeInfo>, pub system_disk: Option<u32> }
impl Model { pub fn from_platform(p: &dyn Platform) -> Result<Model>; pub fn disk(&self, i: u32) -> Option<&DiskInfo>; pub fn disk_mut(..); pub fn partition(&self, disk: u32, number: u32) -> Option<&PartitionInfo>; }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum CloneMode { Smart, Sector }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum Sizing { Exact, Fit, Proportional }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum WipeMethod { Zero, Random, Dod3 }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum BootFix { None, Uefi, Bios, All }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    InitializeDisk { disk: u32, kind: TableKind },
    CreatePartition { disk: u32, start: u64, len: u64, ptype: PartitionType, fs: Option<FsKind>, label: String, letter: Option<char>, name: String },
    DeletePartition { disk: u32, number: u32 },
    FormatPartition { disk: u32, number: u32, fs: FsKind, label: String, cluster: Option<u32>, quick: bool },
    ResizeMove { disk: u32, number: u32, new_start: u64, new_len: u64 },
    SetActive { disk: u32, number: u32 },
    SetLabel { disk: u32, number: u32, label: String },
    SetLetter { disk: u32, number: u32, letter: Option<char> },
    SetType { disk: u32, number: u32, ptype: PartitionType },
    Mount { volume: VolumeId, at: MountPoint }, Unmount { volume: VolumeId, at: MountPoint },
    WipeDisk { disk: u32, method: WipeMethod }, WipePartition { disk: u32, number: u32, method: WipeMethod },
    ConvertTable { disk: u32, to: TableKind },
    AlignPartitions { disk: u32 },
    RebuildMbr { disk: u32 },
    ClonePartition { src_disk: u32, src_number: u32, dst_disk: u32, dst_start: u64, dst_len: u64, mode: CloneMode },
    CloneDisk { src_disk: u32, dst_disk: u32, sizing: Sizing, mode: CloneMode, boot_fix: BootFix },
    MigrateOs { src_disk: u32, dst_disk: u32, only_system_parts: bool, sizing: Sizing, boot_fix: BootFix },
    ImageDisk { disk: u32, path: PathBuf, format: ImageFormat, mode: CloneMode },
    ImagePartition { disk: u32, number: u32, path: PathBuf, format: ImageFormat, mode: CloneMode },
    RestoreImage { path: PathBuf, dst_disk: u32, dst_start: Option<u64> /* None = whole disk */ },
}
impl Operation { pub fn summary(&self) -> String /* "Delete partition 2 on Disk 3" */; pub fn is_destructive(&self) -> bool; pub fn touches_disk(&self, d: u32) -> bool }
pub struct Planner;
impl Planner {
    pub fn simulate(model: &Model, op: &Operation) -> Result<Model>;   // validates then returns the resulting model (new partitions get provisional numbers/GUIDs, letters assigned)
    pub fn simulate_all(model: &Model, ops: &[Operation]) -> Result<Model>;
}
// sizing.rs
pub struct TargetLayout { pub table: Table, pub map: Vec<(u32 /*src number*/, u32 /*dst number*/)> }
pub fn plan_disk_clone(src: &DiskInfo, dst_len: u64, dst_sector: u32, sizing: Sizing, only_system: bool, used_of: &dyn Fn(&PartitionInfo)->u64) -> Result<TargetLayout>;
// Exact: same starts/lens (Err if dst smaller). Proportional: scale every data partition by dst/src ratio, keep EFI/MSR/recovery fixed, align 1 MiB. Fit: shrink/grow only the last data partition... no: Fit = each data partition min(used*1.1, src len) then distribute leftover to the largest. only_system keeps EFI, MSR, the Windows partition (the one with flags.system or largest NTFS with \Windows — caller passes), WinRE.
```
Validation rules in `simulate`: no overlap, inside disk, ≥ 1 MiB, GPT ≤ 128 entries, MBR ≤ 4 primaries (logical allowed inside an extended), destination ≥ used for clone (`Error::Invalid("Target smaller than used data …")`), system disk cannot be destination of Initialize/Wipe/CloneDisk/MigrateOs/Restore/ConvertTable (`Error::Invalid("Disk N is the running system disk")`), letters unique.
- [ ] Tests: `simulate_create_in_free_space_assigns_number`, `simulate_create_overlap_rejected`, `simulate_delete_then_create_reuses_space`, `plan_clone_rejects_small_target`, `planner_guards_system_disk`, `plan_disk_clone_proportional_keeps_efi_msr`, `plan_disk_clone_fit_uses_used_sizes`, `simulate_all_chains_models`, `operation_summary_text`.
- [ ] Commit `feat(ops): model, operations, planner`.

### Task 14: pm-ops queue and executor

**Files:**
- Create: `src/queue.rs`, `src/executor.rs`, `src/steps/{mod.rs, table_ops.rs, clone.rs, migrate.rs, wipe.rs, format.rs, resize.rs}`
- Test: `tests/executor.rs` (MockPlatform)

**Interfaces (Produces):**
```rust
pub struct OpQueue { base: Model, ops: Vec<Operation>, preview: Model }
impl OpQueue { pub fn new(base: Model)->Self; pub fn push(&mut self, op: Operation) -> Result<()> /* simulate on preview, append */; pub fn undo(&mut self) -> Option<Operation>; pub fn discard(&mut self); pub fn preview(&self)->&Model; pub fn ops(&self)->&[Operation]; pub fn len(&self)->usize; pub fn reset(&mut self, base: Model) }
#[derive(Clone, Debug)] pub struct StepEvent { pub op_index: usize, pub step: String, pub progress: ProgressReport }
pub struct ExecOpts { pub dry_run: bool, pub allow_system_disk: bool }
pub fn execute(platform: &dyn Platform, ops: &[Operation], opts: &ExecOpts, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<ExecReport>;
#[derive(Clone, Debug, Default, Serialize, Deserialize)] pub struct ExecReport { pub completed: usize, pub failed: Option<(usize, String)>, pub log: Vec<String>, pub rescue_stats: Vec<RescueStats> }
```
Executor per op (re-reads the live table from the device before each op): table ops → `begin_write(disk)`, mutate `Table`, `commit_table`, `finish`; Format → session finish then `platform.format` (needs volume id after rescan: poll `enumerate_volumes` up to 5 s for a volume at that offset); ResizeMove → if only end changes and fs is NTFS: `platform.shrink_volume/extend_volume` + table update (grow: table first; shrink: volume first); if start changes → copy data with `copy_simple` backwards/forwards depending on overlap direction, then table; Wipe → write pattern with `ProgressTracker` in 4 MiB chunks; Clone/Migrate → `plan_disk_clone`, session on dst, write empty table, per partition `rescue::copy_simple` with `used_map` from pm-fs when Smart, write final table, `regenerate_ids`, finish, then boot fix: assign temp letters, `run_tool(BcdBoot, ["<W>:\\Windows","/s","<S>:","/f","UEFI|BIOS|ALL"])`, `BootSect` `/nt60 <W>: /mbr` for BIOS, remove temp letters; Image/Restore → pm-image create/open + copy; Mount/Unmount → platform.
- [ ] Tests (mock): `execute_create_partition_writes_table_in_session`, `execute_order_and_stop_on_first_failure`, `execute_dry_run_touches_nothing`, `execute_clone_disk_copies_used_clusters_only` (FAT32 image on mock disk, assert unused clusters zero on target, files identical via pm-fs), `execute_migrate_regenerates_disk_guid_and_calls_bcdboot` (mock records run_tool), `queue_push_undo_discard_preview`.
- [ ] Commit `feat(ops): queue and executor`.

### Task 15: CLI `apply`, `ops` JSON, `format`, `wipe`, `migrate`, `clone-disk`

**Files:**
- Create: `crates/pm-cli/src/cmd_apply.rs`, `cmd_ops.rs` (one subcommand per simple op that builds an `Operation` and runs `execute` after a `--yes` confirmation; `--dry-run`)
- Modify: `cli.rs`
- Test: `tests/cli_ops.rs` — `apply` on a JSON ops file against an image file via `FilePlatform` (add to pm-platform: a `Platform` impl whose disks are image files passed as `--disk-file path` so the CLI/GUI can be exercised without admin; `tests` use it)

- [ ] Add `pm_platform::FilePlatform::new(files: Vec<PathBuf>)` (mount/format/shrink → Unsupported; everything else works on files). Tests: `file_platform_enumerates_image`.
- [ ] Tests: `cli_apply_json_creates_partitions_on_image`, `cli_clone_disk_image_to_image_proportional`.
- [ ] Commit `feat(cli): apply and operation subcommands; file-backed platform`.

## Phase 5 — Recovery and analysis

### Task 16: pm-recover partitions, deleted files, carving, sessions

**Files:**
- Create: `crates/pm-recover/Cargo.toml`, `src/lib.rs`, `src/partitions.rs`, `src/files.rs`, `src/carve/{mod.rs, signatures.rs, scanner.rs}`, `src/session.rs`
- Test: `tests/partitions.rs`, `tests/files.rs`, `tests/carve.rs`, `tests/session.rs`

**Interfaces (Produces):**
```rust
pub fn find_partitions(dev: &mut dyn BlockDevice, quick: bool /* quick = backup GPT + current table gaps; full = scan every 1 MiB */, progress, cancel) -> Result<Vec<pm_parttable::Candidate>>;
pub fn apply_candidates(table: &Table, chosen: &[Candidate]) -> Result<Table>;   // add entries for chosen, type from fs kind
pub struct DeletedFile { pub entry: pm_fs::Entry, pub integrity: u8 /* 0-100 */, pub fs_offset: u64 }
pub fn deleted_files(fs: &mut dyn FileSystem, progress, cancel) -> Result<Vec<DeletedFile>>;
pub fn restore_file(fs: &mut dyn FileSystem, f: &DeletedFile, dest_dir: &Path) -> Result<PathBuf>;
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct Signature { pub name: &'static str, pub ext: &'static str, pub header: &'static [u8], pub header_offset: usize, pub footer: Option<&'static [u8]>, pub max_len: u64, pub category: Category /* Image, Document, Archive, Audio, Video, Executable, Database, Other */ }
pub fn signatures() -> &'static [Signature];   // ≥ 25 entries as listed in the spec
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct CarvedFile { pub offset: u64, pub len: u64, pub sig: String, pub ext: String, pub category: Category, pub complete: bool /* footer found */ }
pub fn carve(dev: &mut dyn BlockDevice, sigs: &[Signature], sector: u32 /* headers checked at sector boundaries */, progress, cancel) -> Result<Vec<CarvedFile>>;  // Aho-Corasick-free: byte scan with first-byte table, 4 MiB chunks with overlap of max header len
pub fn extract_carved(dev, f: &CarvedFile, dest: &Path) -> Result<PathBuf>;
#[derive(Serialize, Deserialize)] pub struct ScanSession { pub created: String, pub device: DeviceIdentity { model, serial, size, table_id }, pub kind: SessionKind { Partitions(Vec<Candidate>), Deleted(Vec<DeletedFile>), Carved(Vec<CarvedFile>) } }
impl ScanSession { pub fn save(&self, p: &Path) -> Result<()>; pub fn load(p: &Path) -> Result<Self>; pub fn matches(&self, d: &DiskInfo) -> bool }
```
- [ ] Tests: `find_partitions_after_table_wiped` (GPT image with two FAT32 partitions, zero LBA0-33 → both found with confidence ≥ 80, `apply_candidates` yields a table whose `read_table` output equals original starts/lens), `deleted_fat_file_restored_contents_match`, `carve_finds_jpeg_png_pdf_zip_in_synthetic_disk` (embedded at odd sector offsets; PDF footer `%%EOF` respected; zip via EOCD), `carve_truncates_at_max_len`, `session_roundtrip_json`.
- [ ] Commit `feat(recover): partition finder, deleted files, carver, sessions`.

### Task 17: pm-analyze space, surface, bench, smart

**Files:**
- Create: `crates/pm-analyze/Cargo.toml`, `src/lib.rs`, `src/space.rs`, `src/surface.rs`, `src/bench.rs`, `src/smart.rs`
- Test: `tests/space.rs`, `tests/surface.rs`, `tests/bench.rs`

**Interfaces (Produces):**
```rust
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct Node { pub name: String, pub path: PathBuf, pub size: u64, pub files: u64, pub dirs: u64, pub children: Vec<Node> /* dirs only, sorted by size desc */, pub is_dir: bool }
pub struct SpaceReport { pub root: Node, pub total: u64, pub by_ext: Vec<(String, u64, u64 /*count*/)>, pub largest: Vec<(PathBuf, u64)> /* top 100 */, pub errors: u64 }
pub fn scan_space(root: &Path, threads: usize, progress, cancel) -> Result<SpaceReport>;  // std::thread scoped workers over a work queue; follows no symlinks/junctions
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct SurfaceReport { pub block: u64, pub blocks: u32, pub bad: Vec<u32>, pub slow: Vec<u32> /* > 10× median */, pub ms_per_block: Vec<u16>, pub elapsed_secs: f64 }
pub fn surface_test(dev: &mut dyn BlockDevice, block: u64 /* default 1 MiB */, progress, cancel) -> Result<SurfaceReport>;
#[derive(Clone, Copy, Debug, Serialize, Deserialize)] pub enum BenchMode { Sequential, Random }
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct BenchOpts { pub transfer_sizes: Vec<u32> /* 4K..32K default steps */, pub total_len: u64 /* 200 MiB */, pub queue_depth: u32 /* 8 */, pub threads: u32 /* 4 */, pub mode: BenchMode, pub cooldown_secs: u32 /* 5 */ }
#[derive(Clone, Debug, Serialize, Deserialize)] pub struct BenchPoint { pub size: u32, pub read_mbps: f64, pub write_mbps: f64, pub read_iops: f64, pub write_iops: f64 }
pub trait BenchTarget: Send { fn open_for_bench(&self, len: u64) -> Result<Box<dyn BlockDevice>>; } // platform provides unbuffered temp file; tests use MemDevice
pub fn benchmark(t: &dyn BenchTarget, opts: &BenchOpts, progress, cancel) -> Result<Vec<BenchPoint>>;
pub fn smart_summary(d: &SmartData) -> Vec<(String, String, Health /* Ok, Warn, Bad */)>;
```
- [ ] Tests: `scan_space_sizes_and_counts` (temp tree), `surface_reports_bad_block_from_faulty_device` (reuse `pm_image::rescue::FaultyDevice` behind feature `testing`), `benchmark_runs_on_mem_device_returns_points`, `smart_summary_flags_reallocated_sectors`.
- [ ] Add to pm-platform Windows: `BenchTarget` impl (temp file with `FILE_FLAG_NO_BUFFERING|FILE_FLAG_WRITE_THROUGH`, deleted on drop) and `smart()` via `IOCTL_STORAGE_QUERY_PROPERTY` (StorageDeviceHealth / NVMe log) with ATA `SMART_RCV_DRIVE_DATA` fallback.
- [ ] CLI: `pm recover partitions|files|carve`, `pm analyze space|surface|bench|smart`. Commit `feat(analyze): space, surface, bench, SMART` and `feat(cli): recover and analyze commands`.

## Phase 6 — GUI

### Task 18: pm-gui shell: main window, disk table, disk map, pending queue

**Files:**
- Create: `crates/pm-gui/Cargo.toml` (bin `partition-manager`, deps eframe, egui_extras (table), pm-*), `build.rs` (embed manifest `requireAdministrator` via `embed-resource` or `winres`), `partition-manager.manifest`, `src/main.rs`, `src/app.rs` (state, refresh thread), `src/theme.rs`, `src/views/{toolbar.rs, sidebar.rs, disk_table.rs, disk_map.rs, pending.rs, legend.rs, context_menu.rs}`, `src/jobs.rs` (worker thread + channel; `JobHandle { progress: Arc<Mutex<ProgressReport>>, cancel: CancelToken, result: Receiver<Result<JobOutput>> }`), `src/dialogs/{progress.rs, confirm.rs, properties.rs}`, `src/format_util.rs`

Layout per spec §2.10 and screenshot 1: toolbar with Apply/Undo/Discard (disabled when queue empty), tool buttons; left sidebar groups "Wizards", "Convert disk", "Clean disk", "Check disk", "Manage partition" with actions enabled by selection; central `egui_extras::TableBuilder` grouped by disk; disk map bars with proportional segments (min 40 px, colors: GPT/primary blue, logical cyan, unallocated grey, selected gold border) and a usage fill; status bar legend; pending list bottom-left. Selection model: `Selected { Disk(u32) | Partition(u32,u32) | Free(u32, start, len) }`. Right-click menu mirrors sidebar. Apply → confirm dialog listing `op.summary()` with red text for destructive and a system-disk banner → `execute` on worker → progress dialog with phase/bytes/rate/ETA/cancel → refresh.

- [ ] Tests: `src/views/disk_map.rs` has pure layout fn `segments(disk: &DiskInfo, width: f32) -> Vec<Segment>` tested (`segments_sum_to_width`, `min_width_enforced`); `jobs::JobHandle` tested with a fake job.
- [ ] Manual check: `cargo run -p pm-gui -- --file-platform <image>` shows the image as a disk (FilePlatform flag so the GUI runs without admin).
- [ ] Commit `feat(gui): main window, disk table, disk map, pending operations`.

### Task 19: pm-gui wizards (partition ops)

**Files:**
- Create: `src/wizard.rs` (generic `Wizard { steps: Vec<Box<dyn WizardStep>>, idx }` with Back/Next/Finish/Cancel, validation per step, summary step), `src/wizards/{create.rs, format.rs, resize.rs (draggable bar), delete.rs, label_letter.rs, wipe.rs, convert.rs, initialize.rs, mount.rs}`
- [ ] Each wizard produces an `Operation` pushed to the `OpQueue`; the Resize wizard shows before/after via `Planner::simulate`. Tests: `resize_bar_math` (pixels↔bytes, min = used).
- [ ] Commit `feat(gui): partition wizards`.

### Task 20: pm-gui wizards (clone, migrate, image, rescue, recovery) and tool windows

**Files:**
- Create: `src/wizards/{copy_disk.rs, copy_partition.rs, migrate_os.rs, image_create.rs, image_restore.rs, rescue.rs, partition_recovery.rs, data_recovery.rs}`, `src/tools/{benchmark.rs (egui bar chart), space_analyzer.rs (treemap + file/folder tabs), surface_test.rs (grid), smart.rs}`
- Migrate OS wizard = screenshot 4 (option A copy all / B only system partitions) → target disk → sizing (Fit/Proportional/Exact + "align to 1 MB", "use GUID partition table") → boot fix → summary. Data recovery = screenshot 2 (device cards grouped "Logical drives" / "Hard disks" + "Open saved session") → scan mode (deleted files / full carve) → results tree with type filter and search → destination folder (must not be the source) → progress. Benchmark window = screenshot 5 controls. Space analyzer = screenshot 6 tabs.
- [ ] Tests: treemap layout `squarify(items, rect)` areas proportional; rescue status text formatting.
- [ ] Commit `feat(gui): clone/migrate/image/rescue/recovery wizards and tool windows`.

## Phase 7 — Hardening

### Task 21: packaging, docs, platform stub, VHDX reader

- [ ] `README.md`: features, build (PowerShell, GNU toolchain note, MSVC alternative), running elevated, CLI reference, safety notes, how to verify images with `qemu-img check`.
- [ ] `pm-platform::NullPlatform` for non-Windows compiles (`cargo check --target x86_64-unknown-linux-gnu` is not required on this machine; keep `cfg(windows)` boundaries clean).
- [ ] `pm-image::vhdx` read-only open (header, region table, BAT, PB entries) with test `vhdx_read_fixture_header`; writing stays a TODO in README.
- [ ] `cargo build --release`, copy `pm.exe` and `partition-manager.exe` to `dist/`. Commit `chore: packaging and docs`.
