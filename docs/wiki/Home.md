# Partition Manager

Partition Manager is a disk partition manager for Windows, Linux and macOS, written in Rust
(Windows is the reference platform). It shows every disk and partition, creates, formats,
resizes and moves partitions, clones and migrates disks, saves disks as virtual-disk images,
rescues data from failing media, and recovers lost partitions and deleted files. It ships as
a wizard-driven desktop application (`partition-manager`, `.exe` on Windows) and a
scriptable command line (`pm`) that share the same engines.

| OS | Status |
|---|---|
| Windows | Full support, including boot repair. |
| Linux | Supported (sysfs, `/dev` nodes, `mkfs.*`, `ntfsresize` / `resize2fs`, `smartctl`); tested on loop devices in WSL2 and on an LVM-rooted AlmaLinux host. |
| macOS | Supported through `diskutil` and raw `rdisk` nodes; tested on an Apple Silicon Mac with an `hdiutil` disk image (GPT, FAT32, image, unmount); never on a physical disk. |

Linux and macOS have no boot repair and no drive letters; macOS cannot format NTFS or resize
file systems. See [[Known Limitations]].

![Partition Manager showing a demo image file as a disk](https://raw.githubusercontent.com/inalto/partition-manager/main/docs/screenshots/gui-shell-demo-image.png)

> **Safety warning.** Nothing has yet been run elevated against a real disk. Everything is
> tested on in-memory disks and image files (on Linux also on loop devices). Before you point Partition Manager at a disk
> that holds data, work through the [[First Live Test]] checklist on scratch media (a
> dedicated USB stick or an attached VHD), with every other external drive unplugged and a
> backup of the machine. Until then, practice on image files with `--disk-file` (see
> [[Getting Started]]); that needs no administrator rights and touches no physical disk.

## Features

- **Enumerate, manage and mount.** Disks and partitions with capacity, usage, file system,
  type and status; create, delete, format, resize and move, set active, set type, change
  drive letter, wipe, convert MBR/GPT, align to 1 MiB, rebuild the MBR; mount and unmount at
  a drive letter or an empty folder.
- **Clone.** Copy a partition, volume, disk or image file. Smart mode copies only the blocks
  the file system marks used (NTFS, FAT, exFAT, ext); sector mode copies everything.
- **Migrate the OS.** Copy the Windows system partitions to another disk with fit or
  proportional sizing, as GPT or MBR, optionally keeping the source's disk and partition
  ids, with an optional boot fix.
- **Image and restore.** Save a disk or partition as raw, VHD (fixed or dynamic), VMDK (flat
  or sparse) or qcow2, convert between those formats, restore an image onto a disk. VHDX
  images can be opened read-only. See [[Imaging and VM Formats]].
- **Rescue.** A ddrescue-style copier (copy, trim, scrape, retry) with a resumable mapfile in
  GNU ddrescue's format, for failing drives. It can also export a failing disk straight into
  a VM image. See [[Rescue Mode]].
- **Recover.** Find lost partitions, list and restore deleted files from NTFS, FAT, exFAT and ext (ext entries without names), carve
  files out of raw data by signature, and save or reopen scan sessions. See [[Recovery]].
- **Analyze.** Space analyzer, surface test, disk benchmark, SMART health.
- **Safe by construction.** A pending-operations queue, a confirmation before anything is
  written, a dry-run mode, and four independent layers of checks. See [[Safety Model]].

## Pages

| Page | What it covers |
|---|---|
| [[Getting Started]] | Building, running elevated, practicing on an image file |
| [[CLI Reference]] | Every `pm` command with its help text |
| [[GUI Guide]] | Toolbar, disk map, pending operations, every wizard and tool window |
| [[Safety Model]] | The four safety layers and the rules behind them |
| [[Imaging and VM Formats]] | Raw, VHD, VMDK, qcow2, VHDX; smart and sector mode; verifying images |
| [[Rescue Mode]] | ddrescue-style copying, mapfiles, resuming, the Damaged-disk page |
| [[Recovery]] | Lost partitions, deleted files, signature carving, scan sessions |
| [[Architecture]] | Crates, dependency direction, data flow of a clone |
| [[First Live Test]] | The checklist for the first run against a real disk |
| [[Known Limitations]] | What does not work, and the deferred minor findings |
| [[Roadmap]] | What is out of scope and what is planned |

## License and credits

MIT (`license = "MIT"` in `Cargo.toml`; there is no separate `LICENSE` file yet). The UI is
inspired by MiniTool Partition Wizard, and the rescue engine follows the algorithm of GNU
ddrescue and writes the same mapfile format. No code from either project is used.

Source and issues: <https://github.com/inalto/partition-manager>
