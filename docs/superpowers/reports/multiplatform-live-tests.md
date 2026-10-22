# Multi-platform live tests (2026-10-22)

First runs of the Linux and macOS layers outside WSL2 and CI, on two machines of the user's
network reached over SSH. Nothing elevated touched a real disk: Linux used a loop device,
macOS an `hdiutil` disk image attached without mounting.

## Linux: AlmaLinux 9.8, kernel 5.14, root on LVM

Machine: QEMU guest, 8 CPUs, 7 GB RAM, `/` on `/dev/mapper/cl-root` (LVM on `sda2`), `/home`
on `sdb1` (ext4, 3.9 TB), passwordless `sudo`. Toolchain installed with rustup (stable
1.99.1); build dependencies and filesystem tools from `dnf` (`gtk3-devel`, `libxkbcommon-devel`,
`wayland-devel`, X11 devel packages, `exfatprogs`, `ntfsprogs`, `ntfs-3g`, `dosfstools`).

- `cargo build --release --workspace`: clean, no warnings (3 min 27 s cold).
- `cargo test --workspace --no-fail-fast` as the user: 59 test binaries, **711 passed, 0 failed**,
  including the two wall-clock surface tests that CI skips.
- `sudo … cargo test -p pm-platform --test linux_live -- --ignored`: **1 passed** (0.5 s): loop
  device enumerated, GPT written through a session, `mkfs.vfat`, mount, the busy-partition
  rollback ("the 1 filesystem(s) already unmounted were mounted again"), detach. No loop device
  or mount left behind. Note: `sudo` resets `HOME`, so the command needs
  `RUSTUP_HOME`, `CARGO_HOME` and `RUSTUP_TOOLCHAIN=stable` passed explicitly (or `sudo -E` with a
  sudoers that keeps `HOME`); a separate `CARGO_TARGET_DIR` avoids root-owned build artefacts.
- The documented walk-through (`docs/FIRST-LIVE-TEST.md`, "Linux: a loop device") with the
  release `pm`, as root: `doctor` (System disk: 0, all ten tools found), `init-disk --gpt`
  (dry run, then real), `create --fs fat32 --label T1` (table written, `BLKRRPART`, `mkfs.vfat`;
  `lsblk` shows `loop0p1 vfat T1`), `mount --folder` (mounted `rw,nosuid,nodev`), `image` of the
  partition onto itself refused ("on partition 1 of Disk 2, the one being imaged"), `image` to
  `/tmp` (3.09 MB smart copy, 200 MB raw file), `unmount`, `delete --dry-run`, `table` (GPT,
  one entry at 1 MiB). Without `sudo`, `pm list` shows every disk as "Unreadable" with the
  "run with sudo (root) for exact offsets" note and no filesystem on unmounted partitions.

**Finding fixed during the run.** With `/` on LVM, `system_disk()` returned `None` and the
fail-closed rule refused *every* write, even on the loop device ("cannot determine the running
system disk; refusing destructive operation"). LVM roots are the default on RHEL, Fedora and
Ubuntu Server. Fix `0afc014`: the Linux system disk follows `/sys/class/block/*/slaves` through
dm/LVM/LUKS/md down to the physical disks; one disk gives its index, several still give `None`
(now logged with a reason). Re-run on the box: `System disk: 0`, every step above passed.

**Also fixed.** A disk whose table could not be read printed as "Uninitialized" in the CLI
(`ebd774c`: "Unreadable", as the GUI already said).

**Not covered.** No physical disk (the box has none to spare): `/dev/sdb` was only listed.
`mkfs.ntfs`/`mkfs.exfat`/`mkfs.ext4`, `ntfsresize`, `resize2fs`, `smartctl` on a real drive and
the GUI (no desktop session on the box) were not exercised.

## macOS: Mac mini (Apple M2), macOS 15.5

Machine: Apple Silicon, 8 cores, 8 GB RAM, Command Line Tools present, no Homebrew, no
`smartctl`; the user account is an admin but `sudo` asks for a password, so only the
unprivileged part ran.

- rustup (stable 1.99.1, `aarch64-apple-darwin`); `cargo build --release --workspace`: clean, no
  warnings (1 min 05 s).
- `cargo test --workspace --no-fail-fast`: 59 test binaries, **670 passed, 0 failed** — the first
  run of the whole suite on a Mac (the `macos_fixtures` tests, the `cfg(macos)` unit tests, the
  GUI tests with the host reporting `macos`).
- Real `diskutil list -plist` and `diskutil info -plist` output (internal SSD `disk0`, its
  `disk0s1`/`disk0s2` slices, the synthesized container `disk3`, `/`) was captured and compared
  with the hand-written fixtures: the key names and shapes match (`APFSPhysicalStores` as an
  array of `{DeviceIdentifier}` dicts in `list`, the singular `APFSPhysicalStore` string in
  `info` of a volume, `BusProtocol` "Apple Fabric", `DeviceBlockSize` 4096 on the internal SSD,
  `VirtualOrPhysical` "Unknown" for the SSD, `/` being the snapshot `disk3s1s1`).
- `pm doctor` (not root): Platform macos, System disk 0 (through the APFS physical store of the
  root snapshot), `/usr/sbin/diskutil` found, `smartctl` not found. `pm list`: `disk0` with its
  three slices (ISC, APFS store with the container's 103 GB in use, Recovery), the synthesized
  containers skipped, "Unreadable … run with sudo" note; `pm table /dev/rdisk0` refused with the
  sudo hint.
- `hdiutil create -size 64m -layout NONE` + `hdiutil attach -nomount` → `/dev/disk4`, owned by
  the user: `pm list` shows "Disk 4 (Disk Image, Uninitialized, 64.00 MB) /dev/rdisk4" (index =
  diskutil number, bus from "Disk Image"); `pm table /dev/disk4` reads the (empty) table through
  the raw node, i.e. `DKIOCGETBLOCKCOUNT`/`DKIOCGETBLOCKSIZE` and the aligned reader work on a
  real `rdisk`; `init-disk --gpt --dry-run` plans; `init-disk --gpt --yes` is refused:
  "access denied (writing a disk needs root)" and the table is unchanged afterwards.

**Not covered (needs `sudo` on the Mac).** The write session (`diskutil unmountDisk`, GPT write,
the kernel re-reading the map), `diskutil mount`/`unmount`/`eraseVolume`, the FAT16/FAT12
personality names, `smartctl`, the `macos_live` test, and the GUI. To run them: give the account
passwordless `sudo` (or run the macOS section of `docs/FIRST-LIVE-TEST.md` by hand), then
`sudo env "PATH=$PATH" cargo test -p pm-platform --test macos_live -- --ignored`.

The captured plists are not committed (they carry the machine's volume UUIDs); the key
comparison above is the record.
