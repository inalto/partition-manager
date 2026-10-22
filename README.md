# Partition Manager

![CI](https://github.com/inalto/partition-manager/actions/workflows/ci.yml/badge.svg)

A disk partition manager for Windows, Linux and macOS, written in Rust (Windows is the
reference platform). It shows every disk and partition, creates, formats, resizes and moves
partitions, clones and migrates disks, images them to virtual-disk formats, rescues data from
failing media, and recovers lost partitions and deleted files. It ships as a wizard-driven
desktop application (`partition-manager`, `partition-manager.exe` on Windows) and a
scriptable command line (`pm`, `pm.exe`) that share the same engines.

![Partition Manager showing a demo image file as a disk](docs/screenshots/gui-shell-demo-image.png)

Everything destructive goes through a pending-operations queue, a confirmation and a
write session that locks the disk first (see [Safety model](#safety-model)), and everything
can be practised risk-free on image files with `--disk-file`.

## Features

- **Enumerate, manage and mount.** Disks and partitions with capacity, usage, file system,
  type and status; create, delete, format, resize and move, set active, set type, change drive
  letter, wipe, convert MBR/GPT, align to 1 MiB, rebuild the MBR; mount and unmount at a drive
  letter or an empty folder.
- **Clone.** Copy a partition, volume, disk or image file. Smart mode copies only the blocks
  the file system marks used (NTFS, FAT, exFAT, ext); sector mode copies everything.
- **Migrate the OS.** Copy the Windows system partitions to another disk with fit or
  proportional sizing, as GPT or MBR (an MBR disk can be migrated to a GPT one on the way,
  and the other way round), optionally keeping the source's disk and partition ids, and with
  an optional boot fix (`bcdboot` / `bootsect`).
- **Image and restore.** Save a disk or partition as raw, VHD (fixed or dynamic), VMDK
  (flat or sparse) or qcow2, convert between those formats, restore an image onto a disk.
  VHDX images can be opened read-only.
- **Rescue.** A ddrescue-style copier (copy, trim, scrape, retry) with a resumable mapfile in
  GNU ddrescue's format, for failing drives; it can also export a failing disk or partition
  straight into a VM image (raw, VHD, VMDK, qcow2), with unreadable blocks left as zeros.
- **Recover.** Find lost partitions, list and restore deleted files from NTFS and FAT, carve
  files out of raw data by signature (images, documents, archives, audio, video, executables,
  databases), and save or reopen scan sessions.
- **Analyze.** Space analyzer (largest folders, files and extensions), surface test, disk
  benchmark, SMART health.

## Safety model

- **Pending queue and Apply.** The GUI never writes when you pick an action: it queues an
  operation and shows the resulting layout. Nothing happens until you click **Apply**, which
  lists every destructive step and asks for confirmation. **Undo** and **Discard** remove
  queued operations.
- **Dry run.** `Settings > Dry run` (or `partition-manager --dry-run`) makes Apply validate
  everything against the live disks and write nothing; the CLI has `--dry-run` as well.
- **CLI needs `--yes`.** `pm create`, `format`, `resize`, `apply`, ... print the operation and
  write nothing without `--yes`.
- **System-disk guard.** The running system disk is refused for destructive operations at
  several layers (the planner and the executor pre-flight; the direct-copy commands add
  their own check; all four safety layers are listed in
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)). Resize, label, letter,
  mount and creating a partition in free space on it need an explicit `--allow-system-disk`.
  If the system disk cannot be determined, every disk is treated as protected.
- **Lock and dismount.** Writes happen inside a write session that locks and dismounts every
  volume on the disk (and takes a non-system disk offline). If a volume cannot be locked the
  operation stops before anything is written. There is no kernel driver. The direct-copy
  commands (`pm clone`, `restore`, `rescue`) write disks, partitions and volumes through the
  same session.
- **Same disks as queued.** Before each operation the disks are read again, and every disk
  and partition the operation names must still be the one it was queued against (size, serial
  number and disk id; partition position and GUID). Otherwise the run stops with "disk layout
  changed since the operations were queued; refresh and queue again".
- **Elevation.** The release `partition-manager.exe` carries a `requireAdministrator` manifest
  and asks for UAC on launch. Debug builds do not, so `cargo run` does not prompt.
- **Practice on image files.** `--disk-file PATH` presents a raw image file as a disk in
  both the GUI and the CLI. No administrator rights are needed and no physical disk is
  touched.

More detail, including where each check lives: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Platform support

| OS | Status | Verified |
|---|---|---|
| Windows | Full: disks, partitions, volumes, write sessions, mount, format, resize, SMART, boot repair. | Test suite; real-disk checklist in [docs/FIRST-LIVE-TEST.md](docs/FIRST-LIVE-TEST.md) still to be run. |
| Linux | Supported: disks from sysfs, raw I/O on `/dev` nodes, write sessions (unmount, `O_EXCL`, `flock`, `BLKRRPART`), mount/unmount at folders, format with `mkfs.*`, NTFS/ext resize with `ntfsresize`/`resize2fs`, SMART with `smartctl`. | Test suite in WSL2 (Ubuntu 24.04) plus a live loop-device test as root: GPT through a write session, `mkfs.vfat`, mount, busy-unmount rollback. Never run on a physical disk. |
| macOS | Supported: disks from `diskutil -plist`, raw I/O on `/dev/rdiskN` (sector-aligned), write sessions (`diskutil unmountDisk`), mount/unmount/erase through `diskutil`, SMART with `smartctl`. | Compile checks and fixture tests only (`cargo check --target aarch64-apple-darwin`, the CI macOS job); **never run on a Mac**. |

Continuous integration builds and tests the workspace on all three OSes
(`cargo test --workspace`; tests that touch real hardware are `#[ignore]`d). The macOS
module and the `plist` dependency compile on every OS, so its `diskutil` fixtures run on
Windows and Linux too.

**Not available on Linux and macOS:** boot repair (`bcdboot`/`bootsect`: a clone or Migrate OS
is not made bootable from these hosts), drive letters (volumes mount at folders), ReFS,
editing LVM / device-mapper / mdraid / APFS containers (they are shown, and a disk holding
one refuses writes). On macOS also: NTFS formatting (needs third-party tools) and resizing a
filesystem (shrink/extend are `Unsupported`).

**Devices on Linux and macOS** are named by their nodes: `pm table /dev/sdb`, `pm list` shows
each disk's node next to `disk:N`. A whole-disk node (`/dev/sdb`, `/dev/nvme0n1`,
`/dev/disk2` or `/dev/rdisk2`) is the same as its `disk:N` for every command, `/dev/sdb:2`
is its partition 2, and a partition node (`/dev/sdb2`, `/dev/disk2s1`) is a volume. Prefer
the node: `disk:N` is a position in the disk list that can change when a disk is plugged in.
Without root the disks are listed read-only; on macOS their partition offsets are then
approximate (the list says so) and nothing is written from them.

## Build

You need a Rust toolchain (https://rustup.rs). On Windows, builds run from PowerShell:

```powershell
cargo build --release
```

The release binaries land in `target\release\`: `pm.exe` and `partition-manager.exe`.
`scripts\build-release.ps1` runs the build and copies both into `dist\`.

There are two ways to get a linker on Windows.

### Route A: MSVC (default for a fresh clone)

1. Install **Visual Studio Build Tools** with the **Desktop development with C++** workload
   and a **Windows SDK**.
2. Build with the default `stable-x86_64-pc-windows-msvc` toolchain: `cargo build --release`.

Nothing in the repository pins a toolchain, so this is what a fresh clone uses.

### Route B: GNU toolchain with a portable MinGW (no Visual Studio needed)

For a machine without Visual Studio Build Tools. Install a portable WinLibs MinGW-w64 in user
scope, then copy the two example files and adjust the paths to your user name:

```powershell
winget install --id BrechtSanders.WinLibs.POSIX.UCRT --scope user
Copy-Item .cargo\config.example.toml .cargo\config.toml
Copy-Item rust-toolchain.example.toml rust-toolchain.toml
```

`rust-toolchain.toml` pins `stable-x86_64-pc-windows-gnu` and `.cargo/config.toml` points the
linker and `dlltool` at the MinGW directory, for example:

```toml
[target.x86_64-pc-windows-gnu]
linker = "C:/Users/<you>/AppData/Local/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin/gcc.exe"
rustflags = ["-C", "dlltool=C:/Users/<you>/AppData/Local/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin/dlltool.exe"]
```

Both files are git-ignored, so they never leak a local path into the repository. Because the
linker path is explicit, the same `cargo build --release` also works from Git Bash. The first
build installs the pinned toolchain through rustup if it is missing. (Rust's bundled GNU
`dlltool` lacks an assembler, which is why a real MinGW is needed for `windows-sys`.)

The elevation manifest is embedded with `embed-resource` under MSVC, and with `windres` from
the MinGW directory under GNU; either way only in release builds (`PM_GUI_MANIFEST=1` forces
it into a debug build, `PM_GUI_MANIFEST=0` leaves it out of a release build).

### Linux and macOS

```sh
cargo build --release          # target/release/pm and target/release/partition-manager
cargo test --workspace
scripts/build-release.sh       # the same build, copied into dist/<os>-<arch>/ plus a tarball
```

On Debian/Ubuntu the GUI needs the usual windowing development packages:

```sh
sudo apt-get install -y build-essential pkg-config libgtk-3-dev libxkbcommon-dev \
  libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

At run time the Linux platform layer uses the system tools when they are installed:
`mkfs.vfat` (dosfstools), `mkfs.exfat` (exfatprogs), `mkfs.ntfs` and `ntfsresize`
(ntfs-3g), `mkfs.ext4`, `e2fsck` and `resize2fs` (e2fsprogs), `partprobe` (parted),
`udevadm` and `smartctl` (smartmontools); a missing tool makes only its operation fail, naming
the tool. `pm doctor` lists what it found.

macOS needs only the Xcode command line tools to build; `diskutil` ships with the system and
SMART data needs `brew install smartmontools`.

If a `rust-toolchain.toml` pinned to a Windows channel (Route B above) is present in your
checkout, override it on other machines: `RUSTUP_TOOLCHAIN=stable cargo build --release`.

## Run

### GUI

```powershell
.\dist\partition-manager.exe                       # elevated (UAC prompt), real disks
.\dist\partition-manager.exe --dry-run             # Apply only simulates
.\dist\partition-manager.exe --disk-file C:\img\practice.img
```

The release build always starts elevated. For development, `cargo run -p pm-gui` runs
unelevated: the real disk list is then empty with a banner, and `--disk-file` still works.

On Linux and macOS start the GUI as root for real disks, and as yourself for image files:

```sh
sudo ./target/release/partition-manager              # real disks
./target/release/partition-manager --disk-file practice.img
```

Without root it lists the disks read-only (the status bar says "Not root: read-only").
See [crates/pm-gui/README-gui.md](crates/pm-gui/README-gui.md) for the layout and the wizards.

### CLI

`pm.exe` needs an elevated prompt for physical disks and no elevation for image files; on
Linux and macOS run it with `sudo` for physical disks (`sudo ./target/release/pm list`,
`sudo pm table /dev/sdb`). The full, generated reference with every flag is in
[docs/CLI.md](docs/CLI.md); the main commands are:

```powershell
# Inspect
pm list [--json]                         # disks and partitions
pm table disk:1 [--json]                 # partition table of disk 1 (also: D:, an image file)
pm probe disk:1:2                        # which file system is on partition 2 of disk 1
pm ls disk:1:2 /Users                    # list a directory of a file system (read-only)
pm cat disk:1:2 /notes.txt               # write a file to stdout
pm extract disk:1:2 /Photos C:\restore   # copy a file or tree out
pm doctor                                # elevation, toolchain, disks seen

# Copy and images (these start copying immediately, after the safety refusals)
pm clone disk:1:2 disk:2:1 --smart       # partition to partition, used blocks only
pm clone C: E:\spare\c.img --sector      # c.img must already exist and be at least as large
pm image disk:1 D:\backup\disk1.vhd      # format from the extension, or --format vhd|vhd-fixed|vmdk|vmdk-flat|qcow2|raw
pm image disk:1:2 D:\backup\c.qcow2 --smart
pm restore D:\backup\disk1.vhd disk:2    # image onto a disk (--dst-offset 1M to place it)
pm convert disk1.vhd disk1.qcow2         # image to image
pm rescue disk:3 D:\rescue\copy.img D:\rescue\copy.map -r 2 -K 64K   # onto an existing copy.img (big enough), or a new one

# Rescue a failing disk into a VM image: unreadable blocks are retried, then left as zeros;
# the ddrescue mapfile records them; running the same command again resumes an interrupted
# export, or retries the unreadable areas of a finished image in place (more retries with -r)
pm image disk:3 D:\vm\old-pc.vhd --rescue -r 2           # mapfile D:\vm\old-pc.vhd.map (or --mapfile)
pm image-disk 3 D:\vm\old-pc.qcow2 --rescue --yes        # the operation form (also image-partition)
pm rescue disk:3 D:\vm\old-pc.vmdk D:\vm\old-pc.map --format vmdk   # creates the image; refuses an existing one unless resuming

# Operations (print and validate; write only with --yes; --dry-run validates only)
pm init-disk 2 --gpt --yes
pm create 2 --start 1M --len 100G --fs ntfs --label Data --letter E --yes
pm format 2 1 --fs fat32 --label USB --yes
pm resize 2 1 --start 1M --len 200G --yes
pm clone-disk 0 2 --sizing fit --boot-fix uefi --yes
pm migrate-os 0 2 --boot-fix uefi --yes
pm migrate-os 0 2 --table gpt --boot-fix uefi --yes   # an MBR system disk onto a GPT target
pm migrate-os 0 2 --keep-ids --yes       # same ids as the source: detach the source before the first boot
pm image-disk 1 D:\backup\disk1.vhd --yes       # the operation form of `image` (also image-partition, restore-image)
pm create 2 --start 1M --len 10G --emit-json > op.json   # build an operations file
pm apply ops.json --dry-run              # validate; add --yes to run
pm mount D: --letter X                   # or --folder C:\mnt\data
pm unmount X: --letter X

# Recover
pm recover partitions disk:3 --quick --session scan.json
pm recover partitions disk:3 --apply --yes --min-confidence 80
pm recover files disk:3:1 --dest C:\restore --min-integrity 50
pm recover carve disk:3 --types image,document --dest C:\carved --sector 512
pm recover open-session scan.json

# Analyze
pm analyze space C:\ --top 20
pm analyze surface disk:3 --block 1M
pm analyze bench D: --sizes 4K,64K --random
pm analyze smart 1
```

Exit codes: 0 success, 1 usage error, 2 platform or runtime error. Add `--disk-file PATH`
(repeatable; `disk:0` is the first file) to run any disk-taking command against image files.

## Try it on an image file

This walkthrough never touches a real disk and needs no elevation.

```powershell
# 1. Make a 2 GB raw image (it occupies 2 GB on disk until you delete it)
fsutil file createnew C:\img\practice.img 2147483648

# 2. Give it a GPT and two partitions (omit --yes first to see what would happen)
pm --disk-file C:\img\practice.img init-disk 0 --gpt --yes
pm --disk-file C:\img\practice.img create 0 --start 1M --len 1G --name Data --yes
pm --disk-file C:\img\practice.img create 0 --start 1025M --len 512M --type efi --yes

# 3. Look at the result
pm --disk-file C:\img\practice.img list
pm --disk-file C:\img\practice.img table disk:0

# 4. Open the GUI on it: queue, undo, dry-run and Apply all work on the file
partition-manager.exe --disk-file C:\img\practice.img

# 5. Save it as a VHD and as qcow2, then check them
pm image C:\img\practice.img C:\img\practice.vhd
pm convert C:\img\practice.vhd C:\img\practice.qcow2
```

On image files there is no volume layer, so creating a partition leaves it unformatted;
`format`, `mount`, `unmount` and drive letters report "not available on image files".
To practise a file-system workflow, image or clone a real partition into the file.

## Verifying images with external tools

The test suite checks every header field, but the real tools have the last word. Details and
the on-disk layouts are in [crates/pm-image/README-formats.md](crates/pm-image/README-formats.md).

```sh
qemu-img info    disk.qcow2        # format, virtual size, "corrupt: false"
qemu-img check   disk.qcow2        # expect: No errors were found on the image.
qemu-img check   disk.vmdk
qemu-img check   disk.vhd

# Byte-for-byte equality with the source after a convert/image:
qemu-img compare -f raw source.img -F qcow2 disk.qcow2
qemu-img compare -f raw source.img -F vmdk  disk.vmdk
qemu-img compare -f raw source.img -F vpc   disk.vhd
```

On Windows, a VHD can be attached with Hyper-V's tooling: Disk Management > Action >
Attach VHD, or

```powershell
Mount-DiskImage -ImagePath C:\img\practice.vhd
Dismount-DiskImage -ImagePath C:\img\practice.vhd
```

VMDK files attach to VMware or VirtualBox (`VBoxManage showmediuminfo disk.vmdk`).

## Before the first real-disk run

**Nothing has yet been run elevated against a real disk.** Everything is tested on in-memory
disks and image files (and, on Linux, on loop devices). The checklist has Linux (loop
device) and macOS (`hdiutil` disk image) variants for a first run without a spare disk. Before you point Partition Manager at a disk with data on it, work
through the 12-step checklist in [docs/FIRST-LIVE-TEST.md](docs/FIRST-LIVE-TEST.md) on a
dedicated USB stick or an attached VHD, with every other external drive unplugged and a
backup of the machine. In short:

- Start with `pm doctor` (the system disk must match `(Get-Disk | ? IsSystem).Number`) and
  `pm list --json` against `Get-Partition`, then read-only checks, an image test, and only
  then the first write (`pm init-disk N --gpt --dry-run`, then `--yes`).
- Run elevated with `$env:RUST_LOG="debug"` and stderr redirected to a log file; the GUI logs
  to `%LOCALAPPDATA%\PartitionManager\partition-manager.log` (*Settings > Open log folder*).
- Operations on the running system disk currently stop with "device busy" (see Known
  limitations).
- Migrate OS copies a live C: without VSS: close applications, expect chkdsk on the first
  boot, and test the copy with the source detached.
- A migrated copy gets new disk and partition ids unless you ask to keep them (see
  "Drive letters of a migrated copy" under Known limitations).
- Smart copies leave old data in unused areas of the destination (wipe it first if that
  matters); a move cannot be cancelled once copying starts (use a UPS or battery); any open
  handle on a target volume blocks the session (nothing is written).
- Still to be verified live: volume reads past the filesystem end
  (`FSCTL_ALLOW_EXTENDED_DASD_IO`), the automount drive-letter race on create, the drive
  letter of a migrated clone (`MountedDevices`), copying a live C: without VSS, and
  formatting on removable media (`FormatEx` media type).

## Known limitations

- **Creating file systems.** Partition Manager does not implement NTFS, ReFS or exFAT
  creation; those formats are made by Windows (`format`, via the platform layer). Only FAT32
  has a built-in formatter, used for images and tests. On image files nothing can be formatted.
- **Resizing.** Only NTFS can be shrunk and grown (through Windows); ReFS can only be grown.
  Other recognised file systems can be moved but not resized. A partition with no recognised
  file system can grow (in the partition table only) but is never shrunk or trimmed by
  alignment: its file system may simply be one the disk list could not read ("filesystem
  unknown; refresh or check the volume").
- **The running system disk.** Operations on it that are otherwise allowed (creating a
  partition in free space, growing C:, letters and mounts with `--allow-system-disk` or the
  GUI's checkbox) currently stop with "device busy": every write session locks all volumes of
  the disk, and C: cannot be locked while Windows runs from it. Nothing is written. A
  lock-free path for these changes is planned.
- **Not supported.** Dynamic disks, BitLocker volumes, RAID/LVM, a bootable rescue-media
  builder, defragmentation.
- **MBR extended layouts** with less than 1 MiB between the EBRs (legacy cylinder-aligned
  layouts) are read-only: they are shown and can be cloned, but not edited.
- **VHDX is read-only.** It can be opened, listed, imaged from and converted from, but not
  created or written; writing VHDX is a TODO. Differencing VHDX/VHD, and VHDX logs from an
  unclean shutdown (not replayed), are not supported.
- **Set label** is queued and validated but not yet wired to a platform API, so applying it
  fails with "set label requires platform support (planned)"; give the label when you
  create or format the partition.
- **ext2/3/4** are read-only (browse, extract, smart clone). **APFS, btrfs, XFS, HFS+, LUKS**
  and others are detected and cloned sector by sector, nothing more.
- **Drive letters of a migrated copy.** Windows remembers which volume is `C:` by disk and
  partition id (`HKLM\SYSTEM\MountedDevices`). A copy made by Migrate OS or Copy Disk gets
  new ids, so the first boot from it may map `C:` to the old disk (if it is still attached)
  or assign the copied system volume another letter; Windows usually repairs the boot
  volume's letter itself, but this has not been verified live. Two ways around it: boot the
  copy with the source detached, or tick *Keep the disk and partition ids of the source* in
  the Migrate OS wizard (`pm migrate-os --keep-ids`), which makes the copy identical to the
  source for `MountedDevices` — then the source **must** be detached before the first boot,
  because Windows takes a disk with a duplicate id offline. Keeping the ids needs the same
  partition table kind as the source.
- **No VSS snapshot** is taken for live system volumes. Imaging or cloning a mounted, in-use
  volume can capture an inconsistent file system; image the system disk from another OS or a
  disk that is not in use.
- **Hard links, junctions and storage aliases** are not detected by the source/destination
  overlap checks of the direct-copy commands.
- **"Not on the source disk" checks** for image files, rescue mapfiles and recovery
  destinations ask the platform which disk a path is on (`Platform::disk_of_path`: drive
  letters and folder mount points on Windows, the mount table on Linux with LVM / dm / md
  volumes placed through their member partitions, `diskutil` on macOS). When it cannot tell
  (a RAM disk, a network share on Linux/macOS, a volume spanning several disks) and the
  device has mounted volumes or LVM / RAID members, the path is refused; choose a folder on
  a disk of the list. Windows UNC shares and mapped network drives are accepted.
- **Old scan sessions.** Scan sessions saved before they recorded the scanned region load as
  whole-device sessions. A carve session saved by an older `pm recover carve` on a
  *partition* therefore restores the wrong bytes: delete it and scan again.
- **Linux and macOS.** See [Platform support](#platform-support) for what is unavailable.
  Linux was tested in WSL2 on loop devices only, macOS was never run on a Mac; `disk:N` is a
  position in the list (use the `/dev/...` node); a Linux write session refuses bind mounts
  and btrfs subvolumes on the disk, and on macOS the volumes a session unmounted are not
  mounted again by Partition Manager. Details per OS are on the wiki's Known Limitations
  page.

## Project layout

| Crate | Purpose |
|---|---|
| `pm-core` | `BlockDevice` trait, file/memory/slice devices, errors, progress and cancellation, size units, disk/partition domain model |
| `pm-parttable` | MBR and GPT read/plan/write, alignment, MBR/GPT conversion, lost-table search |
| `pm-platform` | `Platform` trait; Windows (`windows-sys`), Linux (sysfs, ioctls, `mkfs.*`) and macOS (`diskutil`, raw `rdisk` nodes) implementations with shared POSIX helpers; `FilePlatform` (`--disk-file`), `MockPlatform` (tests), `NullPlatform` (other OSes) |
| `pm-fs` | File-system detection; read-only NTFS, FAT, exFAT and ext parsers; used-block bitmaps; FAT32 formatter |
| `pm-image` | Raw, VHD, VMDK, qcow2 images (and read-only VHDX) as block devices; format conversion; ddrescue-style rescue engine and mapfile |
| `pm-recover` | Lost-partition finder, deleted-file recovery, signature carver, scan sessions |
| `pm-analyze` | Space analyzer, surface test, benchmark, SMART |
| `pm-ops` | Operation model, pending queue, planner/simulator, executor, clone/migrate sizing |
| `pm-cli` | The `pm` command line |
| `pm-gui` | The `partition-manager` desktop application (eframe/egui) |

Other documents: [docs/CLI.md](docs/CLI.md) (command reference),
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (dependency graph, clone data flow, safety
checks), [docs/superpowers/specs/2026-10-19-partition-manager-design.md](docs/superpowers/specs/2026-10-19-partition-manager-design.md)
(design spec).

## Testing

```powershell
cargo test --workspace
```

The tests run on synthetic images and the mock platform and need no administrator rights.
Tests that touch real Windows volumes or disks are marked `#[ignore]`; run them from an
**elevated** prompt with `cargo test --workspace -- --ignored`. On Linux the loop-device test
runs as root (`sudo -E env "PATH=$PATH" cargo test -p pm-platform --test linux_live --
--ignored`); the macOS live test (`macos_live`, an `hdiutil` disk image) needs `sudo` on a
Mac.

## License

MIT (see `license = "MIT"` in `Cargo.toml`). See the `LICENSE` file.

## Credits

The UI is inspired by MiniTool Partition Wizard, and the rescue engine follows the algorithm of
GNU ddrescue (and writes the same mapfile format). No code from either project is used.
