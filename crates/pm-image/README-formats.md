# pm-image: on-disk formats and how to verify them

`pm-image` writes images that external tools must accept. The unit/integration tests
check every header field and table invariant, but the final word belongs to the real
tools. None of them are needed to build or test this crate.

| Format | Create with | Layout summary |
|---|---|---|
| Raw (`.img`, `.raw`, `.dd`) | `ImageFormat::Raw` | file content is the disk |
| VHD fixed (`.vhd`) | `ImageFormat::VhdFixed` | data, then 512-byte `conectix` footer (disk type 2) |
| VHD dynamic (`.vhd`) | `ImageFormat::VhdDynamic` | footer copy @0, `cxsparse` header @512, BAT @1536, 2 MiB blocks each preceded by a 512-byte sector bitmap, footer at EOF (disk type 3) |
| VMDK flat (`.vmdk`) | `ImageFormat::VmdkFlat` | text descriptor `<name>.vmdk` (`monolithicFlat`) + raw extent `<name>-flat.vmdk` |
| VMDK sparse (`.vmdk`) | `ImageFormat::VmdkSparse` | `KDMV` header v1, embedded descriptor (`monolithicSparse`), redundant + primary grain directory/tables, 64 KiB grains |
| qcow2 v3 (`.qcow2`) | `ImageFormat::Qcow2` | 64 KiB clusters, L1/L2 tables, 16-bit refcounts, no compression/encryption/snapshots/backing file |

## Verifying with qemu-img

```sh
qemu-img info  disk.qcow2          # format, virtual size, cluster size, "corrupt: false"
qemu-img check disk.qcow2          # must report "No errors were found on the image."
qemu-img check disk.vmdk           # sparse VMDK consistency (grain tables)
qemu-img check disk.vhd            # qemu's vpc driver supports check for dynamic VHDs

# Byte-for-byte equality with the source after `convert`:
qemu-img compare -f raw source.img -F qcow2 disk.qcow2
qemu-img compare -f raw source.img -F vmdk  disk.vmdk
qemu-img compare -f raw source.img -F vpc   disk.vhd
```

### VHD size and the creator application

The footer's creator application is `win ` (what Windows itself writes). qemu reads
the disk size from `current_size` only for creators it knows (`win `, `qem2`, `d2v `,
`CTXS`, `tap`). For any other creator it derives the size from the CHS geometry, which is
usually a few KiB smaller than the real capacity. With `win `, `qemu-img info` therefore
reports the exact capacity, the same value Windows, Hyper-V and VirtualBox use. For VHDs
from other tools that hit the CHS case, qemu can be told to use `current_size`:

```sh
qemu-img info --image-opts driver=vpc,force_size_calc=current_size,file.filename=other.vhd
```

## Verifying on Windows

- **VHD (fixed or dynamic)**: Disk Management → *Action → Attach VHD* → pick the file.
  The disk appears with the expected size; partitions inside are mounted as usual.
  Detach again with right-click on the disk → *Detach VHD*. PowerShell equivalent:
  `Mount-DiskImage -ImagePath C:\path\disk.vhd` / `Dismount-DiskImage -ImagePath ...`.
- **VMDK**: attach to a VM in VMware Workstation/Player or VirtualBox
  (VirtualBox: `VBoxManage showmediuminfo disk.vmdk` reports format and capacity;
  the same command also reads `.vhd` files).
- **qcow2**: no native Windows support; use `qemu-img` (above).

## Not supported

Differencing VHDs, VHDX (planned), split / streamOptimized / compressed VMDKs, VMDK
descriptors with more than one extent, qcow2 compression, encryption, backing files,
and writing to qcow2 images that contain snapshots.
