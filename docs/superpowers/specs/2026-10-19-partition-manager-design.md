# Partition Manager — Design Spec

Date: 2026-10-19
Status: approved for implementation (user asked for plan + autonomous execution)

## 1. Intent

Build **Partition Manager**, a Rust desktop tool for Windows (portable to other
platforms later) that covers what a user of MiniTool Partition Wizard expects:

- see every disk and partition with capacity, usage, filesystem, type and status;
- create / delete / format / resize / label / set-active / change-letter partitions;
- mount and unmount partitions (drive letters and folder mount points);
- clone a partition or a whole disk (used-sectors only when the filesystem is understood);
- migrate the Windows OS to a new disk and make it bootable;
- image a disk or partition into virtual-machine formats (raw, VHD, VMDK, qcow2) and restore from them;
- rescue data from failing media with a ddrescue-style, resumable, multi-pass copier;
- recover lost partitions, deleted files and carve files by signature;
- analyze a disk: used-space treemap, surface test, benchmark, SMART;
- wizard-driven, friendly GUI, with a pending-operations queue that is only
  applied when the user clicks **Apply** (MiniTool model).

### What the user said

- Rust; Windows first, keep a path to other platforms.
- Inspiration: MiniTool Partition Wizard (screenshots + the install tree in
  `reverse_engineer/`), and GNU ddrescue for the rescue engine.
- Multi-step project, extend functionality over time.
- Easy, user-friendly UI with wizards.

### Assumptions made (flag if wrong)

- GUI toolkit: **egui/eframe**. Pure Rust, one `.exe`, no runtime deps,
  fast to iterate. Native look is secondary to shipping a working wizard UI.
- Raw disk access through documented Win32 APIs (no kernel driver). MiniTool
  ships `pwdrvio.sys`; we deliberately do not. Everything works with an
  elevated process.
- Filesystem *creation* is done by Windows (`fmifs.dll` FormatEx / `format.com`)
  for NTFS/exFAT/ReFS; we ship our own FAT32 formatter for images and tests.
- NTFS resize uses Windows (`FSCTL_SHRINK_VOLUME`, `FSCTL_EXTEND_VOLUME`,
  `IOCTL_DISK_GROW_PARTITION`) rather than reimplementing ntfsresize.
- Bootable WinPE media builder, dynamic disks, BitLocker and RAID are out of
  scope for v1.

### Reference findings (from `reverse_engineer/` and the rea toolkit approach)

Observed from strings and the log in the MiniTool tree, not from decompiling code:

- `ikernel.dll` has a batch command executive with commands
  `InitiateDisk, CreatePartition, DeletePartition, FormatPartition,
  ResizePartition, MergePartition, CopyPartition, MountPartition,
  LabelPartition, ActivePartition, ChangePartitionID, ChangeClusterSize,
  ChangeSerialNumber, HidePartition, WipeDisk, CreateEfiSystemPartition,
  UpdateDiskBootConfig`.
- Volumes are locked and dismounted before every destructive command
  (`Locking volume... Dismounting volume...`), disks opened as `\??\PhysicalDrive%d`,
  alignment is `AlignToMB`.
- `PowerDataRecoveryCore.dll` is built around `IDataRecoveryDeviceIO`
  (disk IO, volume IO, virtual IO), `IFileSystemScanner` per filesystem
  (NTFS, FAT, HFS) and a `KRawFileSystemScanner` (signature carving). Scan
  results are saved to `.rss` files and can be reopened.
- They ship `diskspd` for benchmarking, `BootSect.exe` for boot repair,
  `wimgapi` for WinPE.

We mirror these *ideas* (command queue, lock-before-write, device-IO
abstraction, per-FS scanners, saved scan sessions) in our own code.

## 2. Architecture

Cargo workspace, dependencies flow inward (GUI/CLI → ops/wizards → engines → core).

```
pm-core        domain types, errors, BlockDevice trait, progress/cancel, units
pm-platform    OS abstraction (Platform trait) + Windows impl (+ mock for tests)
pm-parttable   MBR + GPT parse/plan/write, alignment, lost-table search
pm-fs          filesystem detection; NTFS/FAT/exFAT/ext read-only parsers,
               used-block bitmaps, directory walking, deleted-entry listing;
               FAT32 formatter
pm-image       virtual disk formats (raw, VHD, VMDK, qcow2) as BlockDevices;
               rescue copier (ddrescue-compatible mapfile, multi-pass)
pm-recover     lost-partition finder, deleted-file recovery, signature carver,
               scan sessions (save/load)
pm-analyze     space analyzer, surface test, benchmark, SMART
pm-ops         operation model: pending queue, in-memory simulation, Apply
               executor with lock/dismount, undo/discard; clone/migrate planners
pm-cli         `pm` command line (list, clone, image, rescue, recover, ...)
pm-gui         eframe application: main view, disk map, wizards, progress
```

Crate rules:

- Only `pm-platform` touches OS APIs. Everything else works on `BlockDevice`
  and plain data, so engines are testable on files and in-memory buffers.
- `pm-ops` never writes without a `WriteSession` from `pm-platform`, which
  encapsulates lock → dismount → write → rescan.
- GUI and CLI share `pm-ops` entirely; the CLI is also the integration-test
  harness.

### 2.1 pm-core

```rust
pub trait BlockDevice: Send {
    fn len(&self) -> u64;                       // bytes
    fn sector_size(&self) -> u32;               // logical
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()>;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    fn is_read_only(&self) -> bool;
}
```

Implementations: `FileDevice` (any file / `\\.\PhysicalDriveN`), `MemDevice`,
`SliceDevice` (a partition window over a parent), `ReadOnly<D>`.
Also: `Progress` (bytes done/total, rate, ETA, phase label), `CancelToken`,
`ByteSize` formatting, `Error` enum (`Io { offset, len, source }`, `Parse`,
`Unsupported`, `Cancelled`, `Platform(code,msg)`, `Busy`).

Domain model (`DiskInfo`, `PartitionInfo`, `VolumeInfo`, `TableKind`,
`PartitionType` (MBR byte or GPT GUID with well-known names), `FsKind`,
`DiskBus`, `DiskFlags{removable, system, offline, readonly}`) is plain data,
serializable with serde.

### 2.2 pm-platform

```rust
pub trait Platform: Send + Sync {
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>>;
    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>>;
    fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>>;
    fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>>;
    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>>; // lock+dismount all volumes on disk
    fn mount(&self, id: &VolumeId, at: MountPoint) -> Result<()>;  // letter or folder
    fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()>;
    fn set_offline(&self, disk: u32, offline: bool) -> Result<()>;
    fn rescan(&self, disk: u32) -> Result<()>;       // IOCTL_DISK_UPDATE_PROPERTIES
    fn format(&self, id: &VolumeId, fs: FsKind, label: &str, cluster: Option<u32>, quick: bool, progress: &dyn Fn(u8)) -> Result<()>;
    fn shrink_volume(&self, id: &VolumeId, new_len: u64) -> Result<()>;
    fn extend_volume(&self, id: &VolumeId, new_len: u64) -> Result<()>;
    fn smart(&self, disk: u32) -> Result<Option<SmartData>>;
    fn is_elevated(&self) -> bool;
    fn system_disk(&self) -> Option<u32>;
    fn run_tool(&self, tool: ExternalTool, args: &[String]) -> Result<String>; // bcdboot, bootsect, mbr2gpt
}
```

Windows implementation (`windows-sys`): `\\.\PhysicalDriveN` +
`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, `IOCTL_STORAGE_QUERY_PROPERTY`
(bus, model, serial, removable, TRIM), `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`,
`FindFirstVolumeW`/`GetVolumePathNamesForVolumeNameW`,
`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, `GetVolumeInformationW`,
`GetDiskFreeSpaceExW`, `FSCTL_LOCK_VOLUME`/`FSCTL_DISMOUNT_VOLUME`,
`SetVolumeMountPointW`/`DeleteVolumeMountPointW`,
`IOCTL_DISK_SET_DISK_ATTRIBUTES` (offline), `IOCTL_DISK_UPDATE_PROPERTIES`,
`IOCTL_DISK_SET_DRIVE_LAYOUT_EX` (preferred way to commit tables so the OS
notices), `FSCTL_SHRINK_VOLUME`, `FSCTL_EXTEND_VOLUME`,
`fmifs.dll!FormatEx` (fallback `format.com`), SMART via
`SMART_RCV_DRIVE_DATA`/`IOCTL_STORAGE_QUERY_PROPERTY` (StorageDeviceHealth).

`MockPlatform` holds in-memory `MemDevice` disks for unit tests of `pm-ops`.

### 2.3 pm-parttable

Pure functions over `&mut dyn BlockDevice`:

- `read_table(dev) -> Table { kind: Mbr|Gpt|None, disk_id, parts: Vec<Entry>, free: Vec<Range> }`
- MBR: 4 primaries, EBR chain for logical, bootstrap preservation, disk signature.
- GPT: header + backup header validation (CRC32, LBA checks), up to 128 entries,
  protective MBR, attribute bits, well-known type GUIDs (EFI, MSR, Basic data,
  WinRE, Linux fs/swap/LVM, Apple...), name UTF-16.
- `plan_*` helpers: find free ranges, align start/end to 1 MiB (configurable),
  convert MBR↔GPT in-model (fails if > 4 primaries needed for GPT→MBR or if
  no room for GPT structures), regenerate disk/partition GUIDs/signature.
- `write_table(dev, &Table)` writes MBR/EBR or GPT primary + backup. Also
  `to_drive_layout_ex` for the Windows commit path.
- `search_lost_tables(dev, progress)`: scan for backup GPT headers and
  filesystem boot sectors (uses `pm-fs::detect`) to propose recoverable
  partitions, with confidence.

### 2.4 pm-fs

- `detect(dev, offset) -> Option<FsProbe { kind, label, total, used, cluster, uuid }>`
  for NTFS, FAT12/16/32, exFAT, ext2/3/4, btrfs, XFS, HFS+, APFS, ReFS, LUKS,
  Linux swap, ISO9660, UDF. Detection only needs the first few KiB (plus the
  ext superblock at 1 KiB and btrfs at 64 KiB).
- `FileSystem` trait (read-only): `root() -> Dir`, `list(dir)`, `open(file) -> Reader`,
  `used_map() -> Bitmap` (sector granularity, used for smart clone),
  `deleted_entries()` for NTFS (MFT records not in use) and FAT (0xE5 entries).
  Implemented for **NTFS** (boot sector, MFT, attribute lists, data runs,
  $Bitmap, index roots/allocations), **FAT12/16/32**, **exFAT**, **ext2/3/4**
  (extents and old block maps, group descriptor bitmaps).
  Other filesystems: detection + full-sector clone only.
- `fat32::format(dev, label, cluster)`: our own formatter for images and tests.
- Everything is tested on synthetic images generated in tests (FAT32 from our
  formatter; NTFS/ext4 from small fixtures built by hand-written builders).

### 2.5 pm-image

**Virtual disk formats** all implement `BlockDevice` (open existing) and
`create(path, capacity, opts)`:

- `raw` (optionally sparse on NTFS via `FSCTL_SET_SPARSE` through platform hook),
- `vhd` fixed and dynamic (footer, dynamic header, BAT, 2 MiB blocks with
  sector bitmaps, conectix/cxsparse),
- `vmdk` monolithicFlat (descriptor + flat extent) and monolithicSparse
  (sparse extent header, grain directory/tables, 64 KiB grains, embedded descriptor),
- `qcow2` v3 (header, L1/L2 tables, refcount table/blocks, 64 KiB clusters,
  no compression, no snapshots, no backing file).
- `vhdx`: stretch goal, planned behind the same trait.
- `convert(src, dst)` is just `copy` between two `BlockDevice`s.

**Rescue copier** (`rescue` module): ddrescue semantics.

- Mapfile compatible with GNU ddrescue (`current_pos status pass` line, then
  `pos size status` lines with `?`, `*`, `/`, `-`, `+`).
- Phases: copy (large blocks, skip ahead on error with exponential skip),
  trim (read bad areas sector-by-sector from both ends), scrape (every sector
  in remaining bad areas), retry (N passes, optionally reversed).
- Options: block size, skip size, max retries, direction, read-only source
  (never writes to source), sparse/zero handling, resume.
- Reports: rescued bytes, error size, error count, rate, ETA, per-phase.
- The same engine with `errors = abort` is the fast path for ordinary clones.
  For "smart" clones the planner passes a used-sector bitmap so unused space
  is skipped (and zero-filled or left sparse in the target).

### 2.6 pm-recover

- `partitions::find(dev)` → candidates (offset, length, fs kind, label,
  confidence, overlaps) using `pm-parttable::search_lost_tables` plus
  pm-fs probes at cluster-aligned offsets; produce a proposed `Table`.
- `files::deleted(fs)` → list of recoverable entries with estimated
  integrity (NTFS: clusters re-used? FAT: chain plausible?); `restore(entry, dest)`.
- `carve::scan(dev, signatures, progress)` → files by header/footer/max size:
  JPEG, PNG, GIF, BMP, TIFF, PDF, ZIP(docx/xlsx/pptx/odt/jar/apk), 7z, RAR,
  GZIP, MP3, MP4/MOV (ftyp), AVI/WAV (RIFF), MKV, OLE2 (doc/xls/ppt),
  SQLite, EXE/DLL, ELF, TTF, PSD, XML/HTML text heuristics. Signatures are a
  data table so adding one is one line.
- `session`: JSON file with scan parameters, device identity (model + serial +
  size + table signature), found entries; reopenable later.

### 2.7 pm-analyze

- `space`: directory walker (multi-threaded) producing a tree with sizes,
  counts, biggest files, by extension; serializable; drives treemap & lists.
- `surface`: read every block, report bad blocks + time map (grid for the UI).
- `bench`: sequential/random read/write with transfer size, total length,
  queue depth, thread count, cool-down; writes only to a temp file on the
  volume (unbuffered, write-through); results MB/s and IOPS per point.
- `smart`: parse attributes and NVMe health log into a table with thresholds.

### 2.8 pm-ops

The MiniTool model: the user queues **operations**; the model is simulated
in memory; the view shows the resulting layout; **Apply** executes them in
order; **Undo** pops the last; **Discard** clears.

```rust
pub enum Operation {
    InitializeDisk { disk, kind },
    CreatePartition { disk, start, len, ptype, fs: Option<FsKind>, label, letter, align },
    DeletePartition { disk, part },
    FormatPartition { disk, part, fs, label, cluster },
    ResizeMove { disk, part, new_start, new_len },
    SetActive { disk, part }, SetLabel { .. }, SetLetter { .. }, SetType { .. },
    Mount { volume, at }, Unmount { volume, at },
    WipeDisk { disk, method }, WipePartition { .. },
    ConvertTable { disk, to },
    AlignPartitions { disk },
    ClonePartition { src, dst_disk, dst_start, dst_len, mode: Smart|Sector },
    CloneDisk { src_disk, dst_disk, sizing: Fit|Proportional|Exact, mode },
    MigrateOs { src_disk, dst_disk, only_system_parts, sizing, fix_boot },
    ImageDisk/ImagePartition { src, dst_path, format, mode, rescue_opts },
    RestoreImage { src_path, dst_disk, dst_start },
    RebuildMbr { disk },
}
```

- `Planner::simulate(model, op) -> Result<model'>` validates (overlaps,
  alignment, min sizes, system-disk guard, destination larger than used data)
  and yields the new model shown in the UI.
- `Executor::apply(ops, platform, progress, cancel)` runs each op inside a
  `WriteSession`, step-reporting; destructive steps refuse to touch the
  running system disk unless the op is explicitly allowed (resize/letter/label).
- Migration/clone planners live here: they build a destination `Table`
  (EFI 100–300 MiB, MSR 16 MiB, Windows, WinRE), compute sizes
  (`Fit`: shrink/grow data partitions proportionally with min = used + 10%),
  then clone each partition (smart when fs known), write the table, regenerate
  GPT GUIDs / MBR signature, and run `bcdboot <W>:\Windows /s <S>: /f UEFI|BIOS|ALL`
  plus `bootsect` for BIOS; BCD device entries are fixed by bcdboot.

### 2.9 pm-cli

`pm` with subcommands mirroring ops: `list [--json]`, `table <disk>`, `probe`,
`clone`, `image`, `restore`, `convert` (image→image), `rescue` (ddrescue-like
flags: `-b -K -r -R -n`), `recover partitions|files|carve`, `analyze space|surface|bench|smart`,
`mount`, `unmount`, `apply <ops.json>`. Progress bars on stderr. The CLI is
the reproducible way to exercise every engine and is used by integration tests
on image files (no admin needed).

### 2.10 pm-gui

eframe app, dark/light. Layout modelled on the screenshots:

- Top toolbar: Apply, Undo, Discard | Data Recovery, Partition Recovery,
  Disk Benchmark, Space Analyzer, Image/Backup | Settings.
- Left panel: context actions grouped (Wizards / Convert / Clean / Check /
  Manage) that enable based on the selection, plus "Pending operations: N".
- Center: table (Partition, Capacity, Used, Unused, File system, Type, Status)
  grouped by disk, and a **disk map**: one bar per disk, segments proportional
  to size with usage fill, color by type, selection highlight.
- Bottom: legend. Right-click context menu on segments/rows.
- Wizards (multi-step dialogs with Back/Next/Finish/Cancel, validation, summary
  page before queueing): Migrate OS, Copy Disk, Copy Partition, Partition
  Recovery, Data Recovery (device → scan mode → results tree with filters →
  destination), Create Image, Restore Image, Rescue Copy, Create Partition,
  Format, Resize/Move (draggable bar), Wipe.
- Tool windows: Benchmark (controls + bar chart), Space Analyzer (treemap +
  file/folder tabs), Surface Test (grid), SMART table, Properties.
- Long jobs run on a worker thread; a progress dialog shows phase, bytes, rate,
  ETA, cancel. State messages via channels; UI never blocks.
- Safety: Apply shows a confirmation listing every destructive step; system
  disk operations show an extra red warning; the app runs elevated (manifest
  `requireAdministrator`); a "dry run" toggle simulates Apply.

## 3. Data flow (clone disk example)

1. GUI wizard collects src disk, dst disk, sizing, mode → `Operation::CloneDisk`.
2. `Planner::simulate` builds dst `Table`, checks each dst partition ≥ used
   bytes of src, returns new model → disk map preview.
3. Apply → `Executor`: `platform.begin_write(dst)` (lock/dismount/offline),
   write empty table, for each partition: open src `SliceDevice`, get
   `used_map` from `pm-fs`, run `rescue::copy` with bitmap, then write final
   table, regenerate IDs, `rescan`, optionally boot fix. Progress per step.

## 4. Error handling

- All engine errors carry offset/length context; the rescue engine converts
  read errors into mapfile state, everything else fails fast.
- Executor stops on the first failing operation, leaves the mapfile for
  resume, and reports exactly which step failed and what was already written.
- Platform errors include the Win32 code and a friendly message table
  (e.g. 5 → "Access denied: run as administrator", 32 → "Volume in use").

## 5. Testing

- Unit tests per crate on `MemDevice`/temp files: MBR/GPT round-trips and
  corruption cases, FAT32 format→parse, NTFS/exFAT/ext4 parsers on fixtures,
  each image format create→write→reopen→read, qcow2/VMDK/VHD header fields
  cross-checked against spec constants, mapfile parse/print round trip,
  rescue phases on a `FaultyDevice` that injects errors, carver on a synthetic
  disk with embedded files, planner invariants (no overlap, alignment).
- `MockPlatform` tests for Executor ordering and lock sessions.
- CLI integration tests on image files.
- Manual QA on real disks is done with a USB stick / VHD attached in Disk
  Management; a `pm doctor` command prints what the platform layer sees.

## 6. Phases (each is its own plan section)

1. Foundation: workspace, pm-core, pm-parttable, pm-platform (enumerate +
   open + mount/unmount), pm-cli `list/table/mount/unmount/doctor`.
2. Filesystems: detection, NTFS/FAT/exFAT/ext parsers, used maps, FAT32 formatter.
3. Imaging: raw/VHD/VMDK/qcow2 devices, rescue copier with mapfile, CLI
   `clone/image/restore/convert/rescue`.
4. Operations: pm-ops model/planner/executor, Windows write sessions, format,
   resize, wipe, convert, clone/migrate planners, CLI `apply`.
5. Recovery & analysis: pm-recover (partitions, deleted files, carving,
   sessions), pm-analyze (space, surface, bench, SMART).
6. GUI: shell, disk map, pending queue, all wizards and tool windows,
   elevation manifest, packaging.
7. Hardening: VHDX, Linux platform stub, localization hooks, docs.

## 7. Out of scope for v1

Bootable media builder, dynamic disks/LVM/RAID, BitLocker, APFS/btrfs
parsing (detection only), NTFS/ext creation, defragmentation, writing to
ext/APFS.
