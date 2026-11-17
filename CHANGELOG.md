# Changelog

All notable changes to Partition Manager. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow semver while the
project is pre-1.0 (minor bumps may change behaviour).

## [0.4.1] - 2026-11-17

### Added
- Create Image with "Continue on read errors": **Finish now** in the progress window, once
  everything was read once and only the damaged areas are being read again sector by
  sector (which can take hours or days). It completes the image with what was read
  (`.partial` renamed into place, the mapfile kept); the same operation run again goes on
  with the damaged areas in place. Cancel still keeps the `.partial` file for resuming.
- Create Image and Restore Image: **File…** next to the image path opens the system file
  dialog; Create Image also has **Folder…**. Choosing the `.partial` file (or the mapfile)
  of an interrupted image selects that image, its format and "Continue on read errors", so
  that it is continued; the page says when an image will be continued or retried in place.

### Fixed
- The progress window of a rescue image said "100%" while damaged areas were still being
  read, with no way out but Cancel, which left the image as `.partial`. The percentage now
  has one decimal and reaches 100.0% only at the end; the window says what the phase is
  doing, and the time-left row is not shown while it has no estimate.

## [0.4.0] - 2026-11-16

### Added
- Windows installer: `partition-manager-<version>-windows-x86_64-setup.exe`, next to the
  zip. Installs for all users into Program Files with a Start menu entry and an
  uninstaller; optionally a desktop shortcut and `pm` on the system PATH. Unsigned.
- An installed copy updates itself by downloading the new installer, verifying it against
  `SHA256SUMS.txt` and running it silently on **Restart now**.
- The application has its logo (`assets/logo.png`) as icon of the executable, the window,
  the installer and the shortcuts (`scripts/make-icon.py` builds the icon files from it).
- **Settings > About Partition Manager**: the logo, the version and platform, how and where
  this copy is installed, what the last update check found, and buttons for the notes of
  this release, the project page and a new update check.
- Create Image: the progress bar is a map of the source. Each stretch of the bar shows
  what happened to that part of the disk: copied, skipped because unused, not read yet,
  read error still being retried (orange) or damaged (red). A damaged area stays visible
  however small it is, the tooltip names the state under the pointer, and a legend appears
  once something failed. Without "Continue on read errors" the bar shows where the read
  error that stopped the copy is.

### Changed
- The tools are tabs. The top bar starts with Disks, Data Recovery, Partition Recovery,
  Disk Benchmark, Space Analyzer, Backup / Image and Disk tools; a tab shows its content in
  the main area of the window instead of opening a window or a dialog over it. The two
  former menus are lists in the side panel (Create Image, Restore Image, Rescue Copy;
  Surface Test, SMART), which also says what the tool on screen does. A tool keeps its
  state and its running job while another tab is shown, and its tab shows a dot meanwhile.
  While a tool writes to a disk, the Disks tab is read-only. The wizards of the actions on
  a selection (Create, Format, Resize...) are still dialogs.
- The side panel follows the selection: it names the selected disk, partition or
  unallocated space and lists only the actions meant for it, instead of every action
  greyed out. An action that cannot run stays listed, disabled, with a tooltip saying why
  (for example "The disk is already GPT"). With nothing selected it says to select a disk
  or a partition.
- Apply stands out while operations are pending: a filled button showing how many it will
  run ("Apply 3"). Apply, Undo and Discard also appear next to the pending operations.
- Disk map: a label that does not fit a narrow partition is shortened by whole words (the
  drive letter alone, the size alone) or left out, never cut mid-word; the tooltip has it in
  full. Partition kinds use different hues (blue, green, vermilion, purple, grey) on a
  taller strip, and the legend explains the "Used" and "Free" fill.
- Table: sizes are right-aligned; partitions read "C: Windows", or the label alone when
  there is no drive letter (no more "*:"); unknown usage and "no status" are empty cells
  instead of "-" and "None"; on Windows the disk row no longer repeats the device path
  (it is in the tooltip).

## [0.3.0] - 2026-11-15

### Added
- **Update check and in-place update.** The desktop application looks for a newer release at
  startup when the last successful check is more than 24 hours old (a failed check is tried
  again at the next start; `api.github.com` only, no other data than a generic user agent)
  and shows a notice with What's new, Update and Skip this version. A failed automatic check
  is only logged; `Settings > Check for updates now` reports the error. Update downloads the
  archive for the platform, only from this repository's release downloads, verifies it
  against `SHA256SUMS.txt`, replaces `partition-manager` and, beside it, `pm` (both or
  neither) and offers a restart; the previous files stay as `*.old` until the new version
  has opened its window at the next start. Turn the check off with `Settings > Check for
  updates at startup` or `--no-update-check`; the settings file is `settings.json` in the
  per-user configuration folder. Releases are not signed.

### Fixed
- **The Apply button of the confirmation starts the job.** The confirmation closed itself
  before its command was handled, so the dialog vanished and nothing ran.

## [0.2.2] - 2026-11-15

### Fixed
- **Images of fragmented disks no longer crawl.** The copy engine's block map cost time in
  proportion to the number of used-space fragments on every chunk: about 12 MiB/s at
  3,000,000 fragments in a synthetic benchmark, against about 3,200 MiB/s now, whatever the
  fragmentation. The mapfile format is unchanged.

## [0.2.1] - 2026-11-15

### Fixed
- **Create Image starts when the wizard is finished.** It used to only queue the image, and
  as an image changes nothing in the disk views it looked as if nothing had happened. The
  wizard's last button now reads "Start"; with other operations pending (they would run
  first) the Apply confirmation opens instead. Restore Image opens the Apply confirmation
  when finished and never writes before it is confirmed.

### Changed
- The progress window shows a moving activity bar while work is running without a byte count
  yet (reading the disks, scanning used space) instead of a bar standing at 0%, and hides
  "Time left" until there is one.
- Smart-mode images name their preparation step ("scan used space").

## [0.2.0] - 2026-10-22

### Added
- **Linux support.** Disks from sysfs, raw I/O on `/dev` nodes, write sessions that unmount
  the disk's filesystems (and mount them again if anything fails or nothing was written),
  `O_EXCL` + `flock`, `BLKRRPART`/`partprobe`/udev cooperation, mount/unmount at folders,
  formatting with `mkfs.*`, NTFS/ext resizing with `ntfsresize`/`resize2fs`, SMART through
  `smartctl`. The system disk is found through LVM/dm/LUKS/md stacks; disk indexes stay stable
  for the life of the process (`diskseq`), and `/dev/sdX` names work wherever `disk:N` does.
- **macOS support.** Disks from `diskutil -plist`, raw I/O on `/dev/rdiskN` in whole sectors,
  write sessions through `diskutil unmountDisk`, mount/unmount/erase through `diskutil`,
  SMART through `smartctl`. Synthesized APFS containers are shown through their physical
  store; the system disk is found through the root volume's physical store (fail-closed on
  Fusion drives).
- **Rescue-mode image export** (`pm image --rescue`, `pm rescue`, the GUI's "Damaged disk"
  mode): unreadable areas are skipped after a number of failures and padded, recorded in a
  GNU ddrescue-compatible mapfile, retried in place on a later run; fresh sparse images skip
  zero writes.
- Clone / Migrate OS choose the target table kind (`--table gpt|mbr`) and can keep the
  source's disk and partition ids (`--keep-ids`).
- `pm doctor` prints the OS, elevation and the external tools found; `pm list` shows each
  disk's device node.
- CI on Windows, Linux and macOS; a release workflow that publishes binaries for the four
  targets; `scripts/build-release.sh` for Linux/macOS.

### Changed
- Wizard windows stay inside the viewport (scrolling bodies).
- Drive-letter choosers are hidden where letters do not exist; the Migrate OS boot repair is
  disabled off Windows with a note.
- A disk whose partition table could not be read is shown as "Unreadable" (not
  "Uninitialized"), with a "run with sudo for exact offsets" note.
- File open/create errors name the path; a missing directory for an image is reported as
  such.

### Fixed (safety)
- Image and rescue outputs on the very device being read are refused; destination checks go
  through the platform (`disk_of_path`) instead of drive letters, cover LVM/md members, and
  accept mapped network drives on Windows.
- Operations that need a partition table (wipe, image, clone source, delete, resize/move) refuse
  disks whose table is unknown or approximate.
- Boot repair is refused up front on Linux/macOS instead of failing after the copy.
- The CLI closes its destination probe before the write session starts.

### Verified
- Windows: 694 tests. Linux: 711 tests and the root loop-device test on an AlmaLinux 9.8 host
  (`/` on LVM) plus the documented CLI walk-through. macOS: 670 tests, the `macos_live` test and
  the documented walk-through as root on a Mac mini (Apple M2, macOS 15.5) with an `hdiutil`
  image. Nothing has run elevated against a physical disk on any OS yet: see
  `docs/FIRST-LIVE-TEST.md`.

## [0.1.0] - 2026-10-19

First version, Windows only: partition management (create, delete, format, resize/move,
label, letters, convert MBR/GPT, align), clone partition/disk, migrate Windows OS, disk and
partition images (raw, VHD, VMDK, qcow2; VHDX read-only), partition and file recovery
(NTFS, FAT, exFAT, ext), file carving, disk analysis (SMART, surface, benchmark, space),
a multi-step wizard GUI (egui/eframe) and the `pm` command line; a planner / precheck /
executor pipeline with write sessions, identity checks and a dry run; `--disk-file`
practice mode on image files.

[0.4.1]: https://github.com/inalto/partition-manager/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/inalto/partition-manager/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/inalto/partition-manager/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/inalto/partition-manager/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/inalto/partition-manager/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/inalto/partition-manager/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/inalto/partition-manager/releases/tag/v0.1.0
