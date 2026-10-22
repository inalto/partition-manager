# GUI Guide

`partition-manager.exe` is an egui/eframe desktop application modeled on the MiniTool
Partition Wizard workflow: you choose actions, they are queued, you review the resulting
layout, and only **Apply** writes. The CLI offers the same operations; see
[[CLI Reference]]. The screenshot on [[Home]] shows the main window with a demo image file
open as a disk.

Starting it: see [[Getting Started]]. The release build asks for administrator rights on
launch. `--dry-run` starts it in dry-run mode, and `--disk-file PATH` presents an image file
as a disk, with no elevation and no physical disk touched.

## The main window

- **Toolbar.** Apply, Undo and Discard on the left; then the big tools (Data Recovery,
  Partition Recovery, Disk Benchmark, Space Analyzer, Image/Backup); then Settings.
- **Sidebar.** Context actions grouped as Wizards, Convert, Clean, Check and Manage. They
  enable and disable depending on what is selected. A "Pending operations: N" counter sits
  in the sidebar.
- **Table.** One block per disk listing Partition, Capacity, Used, Unused, File system, Type
  and Status.
- **Disk map.** One bar per disk with segments proportional to size, a usage fill, color by
  partition type and a highlight for the selection. Right-clicking a segment or a row opens
  a context menu with the same actions. A legend sits at the bottom.

Long jobs run on a worker thread. A progress dialog shows the phase, bytes done, rate and
estimated time, and has a Cancel button; the interface does not block.

## Pending operations: Apply, Undo, Discard

Choosing an action never writes. It adds an operation to the **pending queue**, simulates
the result in memory and redraws the table and disk map to show how the layout will look.

- **Apply** executes the queue in order. It first shows a confirmation that lists every
  destructive step. Operations on the running system disk show an extra red warning.
- **Undo** removes the last queued operation.
- **Discard** clears the whole queue.

If the disks changed between queuing and Apply (size, serial number or disk id of a disk,
position or GUID of a partition), the run stops with "disk layout changed since the
operations were queued; refresh and queue again". Execution stops at the first failing
operation and reports which step failed and what had been written. See [[Safety Model]].

### Confirmation and dry run

**Settings > Dry run** (or `partition-manager --dry-run`) makes Apply validate everything
against the live disks and write nothing. Use it for any new kind of operation before doing
it for real. Operations the executor can never perform are refused when you queue them
rather than at Apply.

Some operations on the running system disk (resize, label, letter, mount, or creating a
partition in free space) are offered behind an explicit checkbox; currently they stop with
"device busy" because Windows cannot lock C: while it runs from it. Nothing is written.
See [[Known Limitations]].

## Partition wizards

Wizards are multi-step dialogs with Back, Next, Finish and Cancel, validation on each page
and a summary page before anything is queued.

- **Create.** Pick free space, a start and length (aligned to 1 MiB), an optional file
  system (NTFS, FAT32, FAT16, exFAT, ReFS), label, drive letter, GPT name and partition
  type (basic, EFI, MSR, recovery, Linux). Windows creates NTFS, ReFS and exFAT; FAT32 is
  the only built-in formatter, used for images and tests.
- **Format.** Choose the file system, label, cluster size and quick or full format. Not
  available on image files.
- **Resize/Move.** Change a partition's start and length, with a draggable bar. Only NTFS
  can be shrunk and grown (through Windows) and ReFS only grown; other recognized file
  systems can be moved but not resized.
- **Delete.** Remove a partition from the table.
- **Label/Letter.** Assign or remove a drive letter. Setting a label is currently queued
  and validated but fails on Apply with "set label requires platform support (planned)";
  give the label when you create or format instead.
- **Wipe.** Overwrite a partition or a whole disk with zeros, random data or a three-pass
  DoD pattern. Destroys the data; the wizard asks you to type a confirmation.
- **Convert.** Convert a disk between MBR and GPT. Try it on an empty disk first.
- **Initialize.** Write an empty GPT or MBR table to a disk that has no partitions.
- **Mount.** Mount a volume at a drive letter or an empty NTFS folder, or remove the mount
  point. Not available on image files.

Smaller actions without a wizard: set active (MBR), set type, align all partitions to
1 MiB, and rebuild the MBR (or the protective MBR and GPT copies).

## Copy, migrate and image wizards

- **Copy Disk.** Copy a whole disk onto another and lay out the partitions on the target
  (this overwrites the target). Choose the sizing (exact, fit or proportional), smart or
  sector mode, an optional boot fix (UEFI, BIOS or both) and the **partition table on the
  target**. The default is the same kind as the source; GPT is preselected for an MBR source
  that has an EFI partition or a target larger than 2 TiB. Converting maps the partition
  types; an MBR copy holds at most four partitions within 2 TiB.
- **Copy Partition.** Copy one partition onto a disk, in smart mode (only the blocks the
  file system marks used) or sector mode (everything).
- **Migrate OS.** Like Copy Disk, but copies only the Windows system partitions (or every
  partition, if you ask). It has the same sizing, boot-fix and target-table choices, so an
  MBR system disk can be migrated to a GPT target and the other way round. Tick **Keep the
  disk and partition ids of the source** to make the copy identical to the source for
  `MountedDevices`; then the source must be detached before the copy first boots, because
  Windows takes a disk with a duplicate id offline. Keeping ids needs the same table kind as
  the source. Read the migration notes in [[Safety Model]] and [[First Live Test]] first.
- **Create Image.** Save a disk or partition to raw, VHD, VMDK or qcow2, in smart or sector
  mode (see [[Imaging and VM Formats]]). A page named **Damaged disk** offers ddrescue-style
  imaging of a failing source:
  - A checkbox "Continue on read errors (ddrescue mode)", with the note that unreadable
    blocks are retried, then written as zeros, and that a mapfile next to the image records
    them so the export can be resumed.
  - Retry passes (0 to 10), the initial skip size, and toggles for the trimming and
    scraping phases and for reverse direction.
  - The mapfile path, which defaults to the image path plus `.map`. It must be an absolute
    path, its folder must exist, and it must not be on the source drive.

  The final report shows the rescue statistics per image (rescued, error size, errors) in
  orange when there were errors, with the mapfile path. See [[Rescue Mode]].
- **Restore Image.** Write an image onto a disk, optionally at an offset (to restore a
  partition image). The Damaged-disk page does not appear here because it concerns reading.
- **Rescue Copy.** Copy a failing device with ddrescue's algorithm into another device or an
  image file, keeping a resumable mapfile. If the destination image file does not exist yet
  it can be created, with a format choice. See [[Rescue Mode]].

## Recovery wizards

- **Partition Recovery.** Search a disk or image for partitions the table no longer
  describes, in a quick scan (backup GPT and free space only) or a full scan. Candidates
  are listed with a confidence value; choose which to add back to the partition table. The
  scan can be saved as a session.
- **Data Recovery.** Choose a device, a scan mode (deleted files of a file system, or
  signature carving of raw data), browse the results as a tree with filters, then choose a
  destination folder, which must not be on the device being scanned. Scan sessions can be
  saved and reopened. See [[Recovery]].

## Tool windows

- **Benchmark.** Sequential or random read and write throughput with a temporary file on
  the chosen directory or drive, with transfer sizes, total bytes per pass, queue depth,
  thread count and a cool-down between sizes; results as a chart. Windows only.
- **Space Analyzer.** Scans a directory tree with several threads and shows where the space
  goes: largest folders, files and extensions, with a treemap.
- **Surface Test.** Reads every block of a device and shows unreadable and slow blocks on a
  grid. Read-only.
- **SMART.** Shows the SMART health attributes of a disk in a table with thresholds.

## On Linux and macOS

The same window and wizards, with these differences:

- Start it with `sudo` for physical disks; without root the disks are listed read-only and
  the status bar says "Not root: read-only".
- Disk headers show the device node (`/dev/sdb`, `/dev/rdisk2`). A disk whose partition
  table could not be read (without root) says "partition table not readable, offsets
  approximate"; nothing is planned on it.
- There are no drive letters: Create has no letter field, Mount takes an empty folder, and
  Change Letter is not offered.
- Copy Disk and Migrate OS show boot repair disabled: "Boot repair is Windows-only; the
  clone will not be made bootable from this host."
- macOS does not offer NTFS when creating or formatting; ReFS is Windows-only.
- "Open in file manager" uses `xdg-open` (Linux) or `open` (macOS).



**Settings** holds the Dry run toggle and **Open log folder**. The GUI writes its log to:

```text
%LOCALAPPDATA%\PartitionManager\partition-manager.log                 (Windows)
$XDG_STATE_HOME/partition-manager/partition-manager.log              (Linux; default ~/.local/state)
~/Library/Logs/PartitionManager/partition-manager.log                (macOS)
```

When reporting a problem, include that file. For the `pm` command line, set
`$env:RUST_LOG="debug"` and redirect stderr to a file.
