# Roadmap

This page lists what Partition Manager deliberately does not do, and the follow-ups that are
known and planned. It is a list of intentions, not a schedule or a promise. The current gaps
are described in [[Known Limitations]].

## Out of scope for version 1

The design spec excludes these:

- A bootable media builder (WinPE or similar).
- Dynamic disks, LVM and RAID.
- BitLocker.
- APFS and btrfs parsing (they are detected, and cloned sector by sector, nothing more).
- Creating NTFS or ext file systems (Windows formats NTFS, ReFS and exFAT; only FAT32 has
  a built-in formatter, for images and tests).
- Defragmentation.
- Writing to ext or APFS.

## Planned follow-ups

- **Live verification.** Run the [[First Live Test]] on real hardware and fix what it finds.
  Open questions: volume reads past the file-system end, the drive-letter race on create,
  `MountedDevices` after a migration, a live C: copied without VSS, and formatting on
  removable media.
- **Volume Shadow Copy (VSS).** Take a snapshot when imaging, cloning or migrating a live
  system volume, so the copy is consistent.
- **`MountedDevices` patching.** Make a migrated copy keep its drive letters without
  needing the source to be detached or the ids to be kept (today you either boot with the
  source detached or use *Keep the disk and partition ids*).
- **Lock-free system-disk edits.** A path for creating a partition in free space, growing
  C:, and drive-letter or mount changes on the running system disk, which today stop with
  "device busy" because C: cannot be locked.
- **Set label.** Wire the queued "set label" operation to a platform API.
- **VHDX writing.** Create and write VHDX images (today read-only), and replay the log of an
  unclean shutdown.
- **Linux platform.** A real `Platform` implementation; today the non-Windows build is a
  stub that lets the engines compile and test.
- **Legacy MBR layouts.** Editing extended layouts with less than 1 MiB between the EBRs by
  preserving the existing EBR positions (today they are read-only: shown and cloneable, but
  not editable).
- **Localization hooks** and further hardening, listed in the design spec's last phase.
- **The deferred minor findings** listed on [[Known Limitations]].

Ideas and bug reports are welcome as issues at
<https://github.com/inalto/partition-manager/issues>.
