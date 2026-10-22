# First Live Test

Everything in Partition Manager is tested on in-memory disks and image files; the Linux layer
also on loop devices (WSL2 and an AlmaLinux host with `/` on LVM), the macOS layer read-only on
a Mac mini. **Nothing has yet been run elevated against a real disk.** This
checklist is the first live test. Work through it in order, on scratch media, and stop at the
first surprise. Background on the protections: [[Safety Model]].

The numbered checklist is for Windows. On Linux and macOS start with the loop-device and
disk-image walk-throughs further down ([Linux](#linux-a-loop-device),
[macOS](#macos-a-disk-image)), then repeat the checklist's steps on a scratch USB stick with
the `/dev/...` node in place of `N` and `sudo` in place of the elevated prompt.

## Target and setup

- **Target:** a dedicated USB stick, or a VHD created and attached in Disk Management
  (*Action > Create VHD*, 4 GB, fixed size). Unplug every other external drive. Back up the
  machine first.
- **Shell:** an elevated PowerShell.
- **Logging:** `$env:RUST_LOG="debug"`, and redirect stderr of every command, for example
  `pm ... 2>> C:\t\pm.log`. The GUI writes its own log to
  `%LOCALAPPDATA%\PartitionManager\partition-manager.log` (*Settings > Open log folder*).

In the steps below `N` is the disk number of the test disk.

## Checklist

1. **`pm doctor`.** "System disk" must equal `(Get-Disk | ? IsSystem).Number`. Stop if it is
   unknown or different.
2. **`pm list --json`** against `Get-Partition -DiskNumber N`: compare offsets, sizes,
   offline and read-only flags, and file systems. `pm` numbers are table-slot numbers and
   can differ from Windows' numbers on GPT disks with gaps. Partitions without a drive
   letter should also show their file system (read from the on-disk signature).
3. **Read-only checks on the test disk:** `pm table disk:N`, `pm probe disk:N:1`,
   `pm analyze smart N`, and `pm analyze surface disk:N --block 1M` (Ctrl-C after a while).
4. **Image test:** `pm image disk:N C:\t\t.vhd`, then `Mount-DiskImage C:\t\t.vhd`; check it
   in Disk Management; dismount it.
5. **First write:** `pm init-disk N --gpt --dry-run`, then the same with `--yes`. Expect GPT
   and Unallocated after a rescan.
   - "Busy": close Explorer windows on the stick.
   - Error 19 or access denied: `Get-Disk N | fl IsReadOnly,IsOffline,OfflineReason`, then
     `Set-Disk N -IsReadOnly $false`. (`pm` refuses a disk Windows reports as read-only or
     offline before it starts.)
6. **Create:** `pm create N --start 1M --len 1G --fs ntfs --label T1 --yes` (no `--letter`
   the first time). If it reports "no volume appeared", the partition exists:
   `pm format N 1 --fs ntfs --yes`. Then create a second partition **with** `--letter`; this
   verifies the drive-letter assignment while Windows automounts the new volume.
7. **Shrink and grow in place:** copy some files on, then
   `pm resize N 1 --start 1M --len 600M --yes`; run `chkdsk X: /scan` and compare file
   hashes; grow back to 1G; check again.
8. **Move:** `pm resize N 1 --start 101M --len 600M --yes`; `chkdsk` and hashes again.
9. **Copy and restore through the operation forms:** `pm image-partition`, `restore-image`,
   `clone-disk ... --yes`. The direct commands `pm clone`, `restore` and `rescue` to a disk
   write through a write session as well; try one of them on the test disk too.
10. **GUI:** queue one operation on the test disk, Apply with *Dry run* on, then for real.
    Check that the confirmation lists the destructive steps.
11. **Convert MBR to GPT and back**, only on an empty disk first.
12. **Migrate OS**, only VM to VM first (two virtual disks). Boot the clone with the source
    detached; check the Windows drive letter and run `chkdsk`.

## If something fails

- `Set-Disk N -IsOffline $false`: a write session takes the disk offline, and that is not
  persistent, but bring it back if a run was interrupted.
- Event Viewer > *System* (sources partmgr, disk, Ntfs, volmgr).
- `pm.log` (or the GUI log above).
- Disk Management > *Rescan Disks*.
- `chkdsk X: /scan`.

## Linux: a loop device

A loop device over a sparse file behaves like a real disk (sysfs entry, partition nodes,
udev, mount) and needs no spare hardware. Run as root (`sudo`); `pm` is
`./target/release/pm` (or `target/debug/pm`). This walk-through was run in WSL2 (Ubuntu
24.04) and, with the release build, on an AlmaLinux 9.8 host with `/` on LVM (both on
2026-10-22; see `docs/superpowers/reports/multiplatform-live-tests.md`).

```sh
truncate -s 256M /tmp/pm-live.img
LOOP=$(sudo losetup -fP --show /tmp/pm-live.img)     # e.g. /dev/loop0
sudo pm doctor            # Platform: linux, Elevated: yes, System disk: the disk of /
sudo pm list              # the loop device is listed last, with its node: "... /dev/loop0"
sudo pm table $LOOP       # "None (no partition table)"
sudo pm init-disk $LOOP --gpt --dry-run
sudo pm init-disk $LOOP --gpt --yes
sudo pm create $LOOP --start 1M --len 200M --fs fat32 --label T1 --yes    # mkfs.vfat
sudo pm list              # GPT, *:T1 FAT32 200 MB
lsblk -o NAME,SIZE,FSTYPE,LABEL $LOOP                                     # loop0p1 vfat T1
mkdir -p /tmp/pm-mnt
sudo pm mount ${LOOP}p1 --folder /tmp/pm-mnt
findmnt /tmp/pm-mnt
sudo pm image $LOOP:1 /tmp/pm-mnt/self.img     # refused: the image would be on the partition
sudo pm unmount ${LOOP}p1 --folder /tmp/pm-mnt
sudo pm delete $LOOP 1 --dry-run
sudo losetup -d $LOOP && rm /tmp/pm-live.img
```

- Whole-disk nodes (`/dev/loop0`, `/dev/sdb`) work wherever a disk index does; partition
  nodes (`/dev/loop0p1`) are volumes. Prefer them to `disk:N`, which is a position in the list.
- Without `sudo`, `pm list` shows the disks from sysfs only, with "partition table not
  readable" and no writes.
- A write session unmounts every filesystem of the disk first (and mounts them all again if
  it cannot unmount one); when it ends it mounts them again, except those on a partition it
  wrote into or changed.
- `pm doctor` lists the external tools found (`mkfs.*`, `ntfsresize`, `resize2fs`, `e2fsck`,
  `partprobe`, `udevadm`, `smartctl`).
- A file written by `sudo pm image` belongs to root; remove it with `sudo` too.
- The `#[ignore]`d platform test covers the same ground at the API level:
  `sudo -E env "PATH=$PATH" cargo test -p pm-platform --test linux_live -- --ignored --test-threads=1`.
  Where `sudo` resets `HOME` (RHEL-style sudoers), add `RUSTUP_HOME=$HOME/.rustup
  CARGO_HOME=$HOME/.cargo RUSTUP_TOOLCHAIN=stable` and a `CARGO_TARGET_DIR` of its own, so that
  rustup finds the toolchain and no root-owned files land in `target/`.

## macOS: a disk image

**Only the read-only part has run on a Mac** (Mac mini M2, macOS 15.5, 2026-10-22: `pm doctor`,
`pm list`, `pm table` of an attached image, and the refusal of `init-disk` without root). The
`sudo` steps below are still untried, so treat them as the first. A disk image attached without mounting gives a `/dev/diskN`
that `diskutil` treats like an external disk. Run as root (`sudo`).

```sh
hdiutil create -size 64m -layout NONE /tmp/pm-live.dmg
hdiutil attach -nomount /tmp/pm-live.dmg          # prints /dev/diskN
sudo pm doctor            # Platform: macos, Elevated: yes, System disk: the internal disk
sudo pm list              # diskN with its node (/dev/rdiskN)
sudo pm table /dev/diskN
sudo pm init-disk /dev/diskN --gpt --dry-run
sudo pm init-disk /dev/diskN --gpt --yes
diskutil list /dev/diskN  # GUID_partition_scheme
sudo pm create /dev/diskN --start 1M --len 48M --fs fat32 --label T1 --yes  # diskutil eraseVolume
diskutil list /dev/diskN  # diskNs1 MS-DOS FAT32 T1
hdiutil detach /dev/diskN && rm /tmp/pm-live.dmg
```

- `/dev/diskN` and `/dev/rdiskN` name the same disk; I/O goes through the raw `rdisk` node in
  whole sectors.
- Without `sudo` the partition offsets come from `diskutil` and are approximate: the list
  says so and nothing is written from them.
- Not available: NTFS formatting, resizing a filesystem, boot repair.
- If a step behaves differently, keep `RUST_LOG=debug` output (`2>> pm.log`), the
  `diskutil list -plist` and `diskutil info -plist /dev/diskN` output, and report them; the
  fixtures in `crates/pm-platform/tests/fixtures/macos/` were written by hand from Apple's key
  names; a capture from an Apple Silicon Mac matched their keys and shapes.
- The `#[ignore]`d `macos_live` test in `pm-platform` automates the image part.

## What to know before a real disk

- **Nothing has run elevated against a real disk yet.** Start on scratch media and keep
  backups.
- **Operations on the running system disk currently stop with "device busy".** Creating a
  partition in free space or growing C: needs every volume of the disk locked, and C: cannot
  be locked while Windows runs from it. Nothing is written. A lock-free path is planned.
- **Migrate OS copies a live C: without VSS.** Close applications, expect `chkdsk` on the
  first boot of the copy, and test the copy with the source detached.
- **The migrated copy gets new disk and partition ids.** Windows may assign a different
  letter to the copied system volume through `MountedDevices`; verify the copy boots to C:.
  Try it both ways: with new ids, and with *Keep the disk and partition ids of the source*
  (`--keep-ids`), which requires the source to be detached before the copy's first boot.
- **Smart copies leave old data in unused areas** of the destination. Wipe the destination
  first if that matters.
- **A move cannot be cancelled once copying starts.** Use a UPS or a charged battery.
- **Any open handle on a target volume blocks the session** (Explorer windows, antivirus,
  the indexer): the operation stops before anything is written.
- **Items that still need live verification:**
  - Volume reads past the file-system end: volume handles request
    `FSCTL_ALLOW_EXTENDED_DASD_IO`. Verify with `pm image X: C:\t\v.img` on an NTFS volume
    that the image contains the backup boot sector (the last sector of the partition) and
    that the log shows no "FSCTL_ALLOW_EXTENDED_DASD_IO failed" warning.
  - The drive letter assigned while Windows automounts a new volume (step 6).
  - `MountedDevices` on a migrated clone (step 12).
  - Consistency of a live C: copied without VSS (step 12).
  - Formatting a volume on a USB stick: the removable media type passed to `FormatEx`
    (step 6, on the stick).
- **The GUI log** is at `%LOCALAPPDATA%\PartitionManager\partition-manager.log`.

If you run the checklist, please report what you found as an issue at
<https://github.com/inalto/partition-manager/issues>.
