# Known Limitations

What does not work, what works with caveats, and the small findings from code review that
have not been fixed yet. Planned work is on the [[Roadmap]]. Before touching a real disk, see
[[Safety Model]] and [[First Live Test]]: nothing has been run elevated against a real disk
yet.

## Limitations

- **Creating file systems.** Partition Manager does not implement NTFS, ReFS or exFAT
  creation; Windows makes those formats. Only FAT32 has a built-in formatter, used for
  images and tests. On image files nothing can be formatted.
- **Resizing.** Only NTFS can be shrunk and grown (through Windows); ReFS can only be grown.
  Other recognized file systems can be moved but not resized. A partition with no recognized
  file system can grow (in the partition table only) but is never shrunk or trimmed by
  alignment: its file system may simply be one the disk list could not read ("filesystem
  unknown; refresh or check the volume").
- **The running system disk.** Operations on it that are otherwise allowed (creating a
  partition in free space, growing C:, letters and mounts with `--allow-system-disk` or the
  GUI's checkbox) currently stop with "device busy": every write session locks all volumes
  of the disk, and C: cannot be locked while Windows runs from it. Nothing is written. A
  lock-free path is planned.
- **Not supported.** Dynamic disks, BitLocker volumes, RAID/LVM, a bootable rescue-media
  builder, defragmentation.
- **MBR extended layouts** with less than 1 MiB between the extended boot records (legacy
  cylinder-aligned layouts) are read-only: they are shown and can be cloned, but not edited.
  To move such a disk to a modern layout, clone it with fit or proportional sizing.
- **VHDX is read-only.** It can be opened, listed, imaged from and converted from, but not
  created or written. Differencing VHDX/VHD, and VHDX logs from an unclean shutdown (not
  replayed), are not supported.
- **Set label** is queued and validated but not yet wired to a platform API, so applying it
  fails with "set label requires platform support (planned)"; give the label when you
  create or format the partition.
- **ext2/3/4** are read-only (browse, extract, smart clone). **APFS, btrfs, XFS, HFS+, LUKS**
  and others are detected and cloned sector by sector, nothing more.
- **Drive letters of a migrated copy.** Windows remembers which volume is `C:` by disk and
  partition id (`HKLM\SYSTEM\MountedDevices`). A copy made by Migrate OS or Copy Disk gets
  new ids, so the first boot from it may map `C:` to the old disk (if it is still attached)
  or assign the copied system volume another letter. Windows usually repairs the boot
  volume's letter itself, but this has not been verified live. Two ways around it: boot the
  copy with the source detached, or tick *Keep the disk and partition ids of the source* in
  the Migrate OS wizard (`pm migrate-os --keep-ids`); then the source **must** be detached
  before the first boot, because Windows takes a disk with a duplicate id offline. Keeping
  the ids needs the same partition table kind as the source.
- **No VSS snapshot** is taken for live system volumes. Imaging or cloning a mounted,
  in-use volume can capture an inconsistent file system; image the system disk from another
  OS or a disk that is not in use.
- **Hard links, junctions and storage aliases** are not detected by the source/destination
  overlap checks of the direct-copy commands.
- **"Not on the source disk" checks** ask the platform which disk a path is on. A path it
  cannot place (a network share on Linux/macOS, a RAM disk, an LVM / device-mapper volume)
  is refused while the device being read has mounted volumes; choose a folder on a disk of
  the list. Junctions and hard links are not followed.
- **Old scan sessions.** Scan sessions saved before they recorded the scanned region load as
  whole-device sessions. A carve session saved by an older `pm recover carve` on a
  *partition* therefore restores the wrong bytes: delete it and scan again.
- **Copying a volume onto a partition of its own disk** (`pm clone <volume> <partition of
  that volume's disk>`) fails with "busy" by design, because the write session locks every
  volume of the destination disk, the open source among them.
- **Linux and macOS** have their own section below.
- **Still to be verified live:** volume reads past the file-system end, the automount
  drive-letter race on create, the drive letter of a migrated clone, copying a live C:
  without VSS, and formatting on removable media.

## Linux

- **Tested in WSL2 only**, on loop devices (a GPT through a write session, `mkfs.vfat`,
  mount, the busy-unmount rollback). Never run on a physical disk. `mkfs.ntfs`,
  `mkfs.exfat`, `mkfs.ext4`, `mkfs.xfs`, `ntfsresize`, `e2fsck` / `resize2fs` were tested as
  command lines only; `ntfs3` mounting and `smartctl` on a real drive were not exercised.
- **Without root** the disks come from sysfs only: the table shows as "not readable", there
  are no partition types, unmounted partitions show no file system, and nothing is written.
- **Disk numbers are positions.** Loop devices are listed last so attaching an image does
  not renumber real disks, but a hot-plugged USB disk can shift `disk:N`. A write session
  refuses a disk that changed since it was listed; address disks by node (`/dev/sdb`).
- **System disk on LVM, dm-crypt, mdraid, ZFS or overlay** cannot be determined, so every
  disk is protected from whole-disk destructive operations (fail closed). Disks holding
  LVM / RAID / crypt members or active swap refuse write sessions. Editing LVM, dm or md
  devices is not supported.
- **Mounts.** A write session refuses a disk with bind mounts, btrfs subvolumes or covered
  mounts (they could not be restored exactly). Filesystems on a partition the session wrote
  into or changed are left unmounted afterwards.
- **A file system on the whole disk** (no partition table, as on WSL's disks and some USB
  sticks) shows as one partition at offset 0: it can be formatted, wiped, imaged and cloned,
  not deleted or resized; wipe the disk to partition it.
- **ioctl numbers** for powerpc, mips and sparc are encoded by hand and verified on x86_64
  only.
- No boot repair, no drive letters, no ReFS.

## macOS

- **Never run on a Mac.** The macOS layer is compile-checked (`aarch64-apple-darwin`,
  `x86_64-apple-darwin`) and tested on hand-written `diskutil -plist` fixtures (from Apple's
  key names, not captures). To verify on a real Mac: `BusProtocol` strings on Apple Silicon,
  `Ejectable` / `RemovableMedia` on USB media, `FreeSpace` on non-APFS volumes, the APFS
  physical-store key of `diskutil info`, the FAT12/FAT16 personality names, the raw-node
  ioctls, Disk Arbitration re-mounting after a session, and the `hdiutil` live test.
- **Without root** partition offsets come from `diskutil` and are approximate: the disk shows
  "partition table not readable, offsets approximate" and nothing is written from them.
- **APFS.** Containers are shown as their physical store partition, which carries the
  container's mount points; APFS containers are not edited. A disk whose table cannot be
  used (damaged, Apple Partition Map) lists its slices as volumes without a place on the disk.
  Volumes unmounted by a write session are not mounted again by Partition Manager.
- **Format** goes through `diskutil eraseVolume` (FAT32, FAT16, FAT12, exFAT, HFS+, APFS),
  which also sets the partition type and ignores cluster size and quick mode. **NTFS cannot
  be formatted** (third-party tools needed) and **file systems cannot be resized**.
- `smartctl` is looked up in the system directories first, then on `PATH`;
  `disk_of_path` runs one `diskutil` per path component until a mount point answers.
- No boot repair, no drive letters, no ReFS.

## Deferred minor findings

These came out of code review as low-risk items and are not fixed yet. They are grouped by
the part of the program they affect. None of them is known to cause data loss on its own;
the ones that can surprise you are marked.

### Core, partition tables and file systems

- **Partition tables.** A damaged extended-partition (EBR) chain makes the whole MBR parse
  fail instead of keeping the primary partitions. GPT partition names are truncated by
  characters rather than UTF-16 units. There are few tests with deliberately hostile or
  corrupt tables.
- **Errors.** I/O errors do not always include the file path, and an out-of-range write is
  reported as an unexpected end of file.
- **FAT32 formatter.** The size formula can differ from Microsoft's by up to one sector, and
  the formatted volume has no boot stub.
- **File-system parsers.** Deleted FAT long-name fragments are discarded; ext directories
  with holes are rejected as corrupt; some ext inode read errors are skipped when listing; a
  deleted-entry listing swallows I/O errors on NTFS and FAT.

### Platform (Windows)

- Unelevated, GPT partition numbers may differ from the numbering used by the partition
  table code.
- Disk enumeration does not suppress Windows' critical-error dialogs, so Windows may show
  a prompt for media that is not ready.
- The `format.com` fallback has unresolved quoting and input details.
- The public "set offline" call has no system-disk guard of its own (the operations layer
  checks before calling it).

### Images and rescue

- **Memory use.** Deliberately malformed VHD, VMDK or qcow2 files with aliased metadata
  tables can use more memory than necessary; a qcow2 cluster that extends past the end of the
  file is an error rather than zeros.
- **Geometry.** The VMDK CHS geometry written differs slightly from what qemu would compute
  (capacity is correct).
- **Same-file guard.** `pm convert` refuses device and UNC paths it cannot resolve rather
  than guessing, and it does not detect hard links or junctions.
- **Mapfile and rescue.** A failure to save the mapfile periodically aborts the run; a
  mapfile used with a different sector size across runs is not detected; the folder is not
  flushed after the rename; the mapfile reader rejects a byte-order mark and reports invalid
  UTF-8 with a generic error; rate and ETA spike at the start of a phase.
- **Sparse creation** of VMDK cannot be cancelled during its up-front work.

### Operations (planner, executor)

- **Undo** can silently drop later queued operations that become invalid.
- **Set drive letter** removes the old letter before mounting the new one.
- **Failure during the final table write** is not flagged as possibly leaving an
  inconsistent disk.
- **Cancelling during the boot fix** is reported as success.
- **Restoring a smaller whole-disk GPT image** onto a larger disk leaves two backup GPTs.
- **After a move,** NTFS hidden sectors are not updated; after a clone that shrank the
  source, the source stays shrunk (it is not extended back); a shrink can fail cleanly on
  unmovable files.
- **Spanned volumes** are not covered by the aliasing and system-disk checks (those checks
  fail open for them); do not rely on them for spanned volumes.
- **Type and letter validation.** Set type allows an extended type on a logical partition;
  drive letters A and B are accepted.
- Cloning an NTFS partition onto a larger *image file* target waits about five seconds for a
  volume that image files never have.

### Recovery

- Very short signatures (BMP, MZ executables, MP3 frames) can flood the results with false
  hits; back-to-back MP4 files can merge; the list of found files is unbounded in memory.
- Measuring reads during carving are not retried per sector, so files after a bad sector
  can come out incomplete and the "unreadable" count can be overstated. Extraction is not
  read-error tolerant. A PDF's end can be located too late when other text follows it.
- File-name sanitization has gaps for unusual names; directories are returned with an
  integrity of 0.
- The session file has no version field.
- Carved or extracted files with the same name replace each other in the destination.

### Analysis

- NVMe SMART fields are not graded against thresholds, and one of the NVMe access
  fallbacks is ineffective.
- Files with reparse points are hidden from the space scan; a panicking worker could hang a
  scan; the random-mode benchmark warm-up doubles the write I/O (a deliberate trade-off).

### Command line

- Passing the same `--disk-file` twice is not rejected.
- Precheck errors in a dry run exit with 1 where the planner's exit with 2.
- `pm set-type basic` on an MBR disk always writes type 0x07.
- `--select` and `--min-confidence` are silently ignored without `--apply`.
- The SMART disk index is not validated; `pm recover partitions --apply` skips the planner
  and precheck passes; a broken pipe on output exits with 2; there is no Ctrl-C handler for
  cancelling a copy cleanly.
- `pm extract` overwrites duplicate names and can follow junctions inside the source file
  system; the destination check looks at drive letters only.
- Some consoles that inherit `WT_SESSION` can print raw escape sequences in the progress
  display.

### GUI

- **Rescue Copy** can default its mapfile to the process's working directory (System32 when
  elevated). Always set an explicit mapfile path on another drive. A rescue error message can
  stay visible after the problem is fixed.
- The Data Recovery restore confirmation is thin; some wizard edge cases around snapping
  and unaligned legacy layouts can produce blocks the planner then rejects; a wizard keeps
  the typed WIPE confirmation after going Back.
- Tool windows stay interactive while Apply runs; the benchmark is not marked as a writing
  operation (it creates a scratch file and removes it on exit).
- A rebase after the layout changed externally can retarget an operation; the check that the
  disk is the same uses its index rather than its serial number in the worker.
- A refresh requested while a job runs or a dialog is open is dropped; the interface does
  not refresh after a panic during Apply.
- Mount points through folders, UNC paths and junctions are not covered by the letter checks
  (see above).
