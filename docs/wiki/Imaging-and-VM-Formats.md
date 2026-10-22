# Imaging and VM Formats

Partition Manager saves a disk, partition or volume as an image file, converts between image
formats, and writes an image back to a disk. Images are block devices to the rest of the
program, so you can also list and extract files from them (`pm ls`, `pm cat`, `pm extract`)
and clone from them. The commands are in the [[CLI Reference]]; imaging a failing disk is
covered in [[Rescue Mode]].

## Supported formats

| Format | `--format` | Layout summary |
|---|---|---|
| Raw (`.img`, `.raw`, `.dd`) | `raw` | The file content is the disk |
| VHD fixed (`.vhd`) | `vhd-fixed` | Data followed by a 512-byte `conectix` footer (disk type 2) |
| VHD dynamic (`.vhd`) | `vhd` | Footer copy at 0, `cxsparse` header, block allocation table, 2 MiB blocks each preceded by a 512-byte sector bitmap, footer at the end (disk type 3) |
| VMDK flat (`.vmdk`) | `vmdk-flat` | Text descriptor `<name>.vmdk` (`monolithicFlat`) plus a raw extent `<name>-flat.vmdk` |
| VMDK sparse (`.vmdk`) | `vmdk` | `KDMV` header v1 with an embedded descriptor (`monolithicSparse`), redundant and primary grain directory and tables, 64 KiB grains |
| qcow2 v3 (`.qcow2`) | `qcow2` | 64 KiB clusters, L1/L2 tables, 16-bit refcounts; no compression, encryption, snapshots or backing file |
| VHDX (`.vhdx`), **read-only** | (open only) | Fixed and dynamic; no differencing; the log is not replayed |

`pm image` and `pm convert` choose the format from the file extension unless you pass
`--format raw|vhd|vhd-fixed|vmdk|vmdk-flat|qcow2`.

The VHD footer's creator application is `win ` (what Windows itself writes), so tools that
trust the footer report the exact capacity.

### VHDX is read-only

A VHDX can be opened, listed, imaged from and converted from, but not created or written.
The opener uses the header with a valid checksum and the highest sequence number, and
unallocated, zero and unmapped blocks read as zeros. Differencing VHDX and VHDX logs from an
unclean shutdown are not supported. Writing VHDX is on the [[Roadmap]].

### Not supported

Differencing VHDs, writing or creating VHDX, split, streamOptimized or compressed VMDKs,
VMDK descriptors with more than one extent, qcow2 compression, encryption and backing files,
and writing to qcow2 images that contain snapshots.

## Creating an image

```powershell
pm image disk:1 D:\backup\disk1.vhd            # format from the extension
pm image disk:1:2 D:\backup\c.qcow2 --smart    # one partition, used blocks only
pm image disk:1 D:\backup\disk1.img --format raw
pm image-disk 1 D:\backup\disk1.vhd --yes      # the operation form (also image-partition)
```

- The image is written to `FILE.partial` and renamed when complete. A failed run leaves an
  existing `FILE` untouched, and an existing `FILE` is overwritten on success.
- `pm image` refuses to overwrite its source or a VMDK extent it reads from.
- **Smart mode** (`--smart`) copies only the blocks the file system marks used. It
  understands NTFS, FAT, exFAT and ext; a file system it cannot read is copied in full.
  Unused areas of a smart copy are not read, so they are zero in the image.
  **Sector mode** (`--sector`) copies every sector, including stale data in unused areas.
- **Sparse behavior.** New sparse images (`vhd`, `vmdk`, `qcow2`) allocate nothing for
  zero areas, so an image of a mostly empty or partly unreadable disk is much smaller than
  its capacity. Fixed VHD and flat VMDK are not sparse formats, and a raw image is a plain
  file of the disk's size.
- Imaging a live, in-use volume can capture an inconsistent file system (no VSS snapshot;
  see [[Safety Model]]).

## Converting

```powershell
pm convert disk1.vhd disk1.qcow2
pm convert disk1.img disk1.vmdk --format vmdk-flat
```

`pm convert` writes `OUTPUT.partial` and renames it when complete. It refuses to overwrite
its input or the input's VMDK extent file. It converts image files only; to convert a
disk's partition table between MBR and GPT use `pm convert-table`.

## Restoring

```powershell
pm restore D:\backup\disk1.vhd disk:2                    # image onto a disk
pm restore D:\backup\c.vhd disk:2 --dst-offset 1M        # place it at an offset
pm restore-image D:\backup\disk1.vhd 2 --yes             # the operation form (--start sets the offset)
```

The destination must be at least as large as the image. `restore` refuses to write onto the
source, onto a partition or volume overlapping it, onto the running system disk and onto a
VMDK extent file, and it writes disks through a locked write session. Restoring a smaller
whole-disk GPT image onto a larger disk leaves the backup GPT of the image in the middle of
the disk (see [[Known Limitations]]).

## Verifying an image with external tools

The test suite checks every header field, but the real tools have the last word.

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

For VHDs written by other tools that qemu sizes from the CHS geometry (a few KiB smaller than
the real capacity), you can tell qemu to use the stored size:

```sh
qemu-img info --image-opts driver=vpc,force_size_calc=current_size,file.filename=other.vhd
```

On Windows, attach a VHD with Hyper-V's tooling: Disk Management > Action > Attach VHD, or

```powershell
Mount-DiskImage -ImagePath C:\img\practice.vhd
Dismount-DiskImage -ImagePath C:\img\practice.vhd
```

VMDK files attach to VMware Workstation/Player or VirtualBox (`VBoxManage showmediuminfo
disk.vmdk` reports format and capacity and also reads `.vhd`). qcow2 has no native Windows
support; use `qemu-img`. A VHDX can be checked with `qemu-img info disk.vhdx` or attached
with `Mount-DiskImage`.

The on-disk layouts are described in
[`crates/pm-image/README-formats.md`](https://github.com/inalto/partition-manager/blob/main/crates/pm-image/README-formats.md).
