# Before the first real-disk run

Everything in this repository is tested on in-memory disks (`MockPlatform`) and image files
(`FilePlatform`, `--disk-file`). **Nothing has yet been run elevated against a real disk.**
This checklist is the first live test. Work through it in order, on scratch media, and stop
at the first surprise.

## Target and setup

- **Target:** a dedicated USB stick, or a VHD created and attached in Disk Management
  (*Action > Create VHD*, 4 GB, fixed size). Unplug every other external drive. Back up the
  machine first.
- **Shell:** an elevated PowerShell.
- **Logging:** `$env:RUST_LOG="debug"`, and redirect stderr of every command, e.g.
  `pm ... 2>> C:\t\pm.log`. The GUI writes its own log to
  `%LOCALAPPDATA%\PartitionManager\partition-manager.log` (*Settings > Open log folder*).

In the steps below `N` is the disk number of the test disk.

## Checklist

1. **`pm doctor`** — "System disk" must equal `(Get-Disk | ? IsSystem).Number`. Stop if it is
   unknown or different.
2. **`pm list --json`** against `Get-Partition -DiskNumber N`: compare offsets, sizes,
   offline / read-only flags and filesystems. pm numbers are table-slot numbers and can differ
   from Windows' numbers on GPT disks with gaps. Partitions without a drive letter should now
   show their filesystem too (read from the on-disk signature).
3. **Read-only checks on the test disk:** `pm table disk:N`, `pm probe disk:N:1`,
   `pm analyze smart N`, `pm analyze surface disk:N --block 1M` (Ctrl-C after a while).
4. **Image test:** `pm image disk:N C:\t\t.vhd`, then `Mount-DiskImage C:\t\t.vhd`; check it in
   Disk Management; dismount it.
5. **First write:** `pm init-disk N --gpt --dry-run`, then the same with `--yes`. Expect GPT and
   Unallocated after a rescan.
   - "Busy": close Explorer windows on the stick.
   - Error 19 / access denied: `Get-Disk N | fl IsReadOnly,IsOffline,OfflineReason`, then
     `Set-Disk N -IsReadOnly $false`. (pm now refuses a disk Windows reports as read-only or
     offline before it starts.)
6. **Create:** `pm create N --start 1M --len 1G --fs ntfs --label T1 --yes` (no `--letter` the
   first time). If it reports "no volume appeared", the partition exists:
   `pm format N 1 --fs ntfs --yes`. Then create a second partition **with** `--letter`: this
   verifies the automount drive-letter race (I4).
7. **Shrink and grow in place:** copy some files on, then
   `pm resize N 1 --start 1M --len 600M --yes`; run `chkdsk X: /scan` and compare file hashes;
   grow back to 1G; check again.
8. **Move:** `pm resize N 1 --start 101M --len 600M --yes`; chkdsk and hashes again.
9. **Copy / restore through the operation forms:** `pm image-partition`, `restore-image`,
   `clone-disk ... --yes`. The direct commands `pm clone` / `restore` / `rescue` to a disk now
   write through a write session as well; try one of them on the test disk too.
10. **GUI:** queue one operation on the test disk, Apply with *Dry run* on, then for real.
    Check that the confirmation lists the destructive steps.
11. **Convert MBR ↔ GPT** — only on an empty disk first.
12. **Migrate OS** — only VM to VM first (two virtual disks). Boot the clone with the source
    detached; check the Windows drive letter (MountedDevices, I6) and run chkdsk.

## If something fails

- `Set-Disk N -IsOffline $false` — a write session takes the disk offline, and that is not
  persistent, but bring it back if a run was interrupted.
- Event Viewer > *System* (sources partmgr, disk, Ntfs, volmgr).
- `pm.log` (or the GUI log above).
- Disk Management > *Rescan Disks*.
- `chkdsk X: /scan`.

## What to know before a real disk

- **Nothing has run elevated against a real disk yet.** Start on scratch media (step list
  above) and keep backups.
- **Operations on the running system disk currently stop with "device busy".** Creating a
  partition in free space or growing C: needs every volume of the disk locked, and C: cannot
  be locked while Windows runs from it. Nothing is written. A lock-free path is planned.
- **Migrate OS copies a live C: without VSS (I7).** Close applications, expect chkdsk on the
  first boot of the copy, and test the copy with the source detached.
- **The migrated copy gets new disk and partition ids (I6).** Windows may assign a different
  letter to the copied system volume through `MountedDevices`; verify the copy boots to C:.
- **Smart copies leave old data in unused areas** of the destination. Wipe the destination
  first if that matters.
- **A move cannot be cancelled once copying starts.** Use a UPS or a charged battery.
- **Any open handle on a target volume blocks the session** (Explorer windows, antivirus,
  indexer): the operation stops before anything is written.
- **Items that still need a live verification:**
  - I3: volume handles request `FSCTL_ALLOW_EXTENDED_DASD_IO`; verify with
    `pm image X: C:\t\v.img` on an NTFS volume that the image contains the backup boot sector
    (the last sector of the partition), and that the log shows no
    "FSCTL_ALLOW_EXTENDED_DASD_IO failed" warning.
  - I4: drive letter assigned while Windows automounts the new volume (step 6).
  - I6: MountedDevices on a migrated clone (step 12).
  - I7: consistency of a live C: copied without VSS (step 12).
  - I10: formatting a volume on a USB stick (removable media type for `FormatEx`, step 6 on
    the stick).
- **The GUI log** is at `%LOCALAPPDATA%\PartitionManager\partition-manager.log`.
