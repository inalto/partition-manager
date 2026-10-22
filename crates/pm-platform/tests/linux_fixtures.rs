//! Linux enumeration parsers over fixture trees: a sysfs `/sys/block` lookalike built in a temp
//! dir and `/proc/mounts` samples.
#![cfg(target_os = "linux")]

use pm_core::{DiskBus, DiskInfo, VolumeId};
use pm_platform::linux::{parse_block_dir, parse_proc_mounts, system_disk_from, MountEntry};
use std::fs;
use std::path::Path;

fn write(p: &Path, content: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, format!("{content}\n")).unwrap();
}

/// `<root>/<name>` with size (512-byte units), sector sizes and a `device` link to a directory
/// under `<devices>/<device_path>` holding `files`.
fn disk(root: &Path, devices: &Path, name: &str, size512: u64, logical: u32, device_path: Option<&str>, files: &[(&str, &str)]) {
    let d = root.join(name);
    write(&d.join("size"), &size512.to_string());
    write(&d.join("ro"), "0");
    write(&d.join("removable"), "0");
    write(&d.join("queue/logical_block_size"), &logical.to_string());
    write(&d.join("queue/physical_block_size"), "4096");
    write(&d.join("queue/discard_max_bytes"), "0");
    if let Some(dp) = device_path {
        let target = devices.join(dp);
        fs::create_dir_all(&target).unwrap();
        for (f, v) in files {
            write(&target.join(f), v);
        }
        std::os::unix::fs::symlink(&target, d.join("device")).unwrap();
    }
}

fn part(root: &Path, disk: &str, name: &str, number: u32, start512: u64, size512: u64) {
    let p = root.join(disk).join(name);
    write(&p.join("partition"), &number.to_string());
    write(&p.join("start"), &start512.to_string());
    write(&p.join("size"), &size512.to_string());
    write(&p.join("ro"), "0");
}

/// sda (SATA, 512-byte sectors, 2 partitions), nvme0n1 (NVMe, 4Kn), mmcblk0 (removable SD),
/// loop0 (unattached), loop1 (attached), dm-0, ram0, mmcblk0boot0, sr0.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("block");
    let devices = t.path().join("devices");

    disk(
        &root,
        &devices,
        "sda",
        1_953_525_168,
        512,
        Some("pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0"),
        &[("vendor", "ATA     "), ("model", "Samsung SSD 870 EVO 1TB ")],
    );
    write(&root.join("sda/queue/discard_max_bytes"), "2147450880");
    part(&root, "sda", "sda1", 1, 2048, 1_048_576);
    part(&root, "sda", "sda2", 2, 1_050_624, 1_952_472_064);

    disk(
        &root,
        &devices,
        "nvme0n1",
        2_000_409_264,
        4096,
        Some("pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0"),
        &[("model", "WD_BLACK SN850X 1000GB"), ("serial", "  23181Z800123  ")],
    );
    // 4Kn: sysfs still counts 512-byte units — LBA 32 (4 KiB) = 512-unit 256
    part(&root, "nvme0n1", "nvme0n1p1", 1, 256, 1_048_576);
    part(&root, "nvme0n1", "nvme0n1p2", 2, 1_048_832, 1_999_360_000);

    disk(&root, &devices, "mmcblk0", 124_735_488, 512, Some("platform/fe320000.mmc/mmc_host/mmc0/mmc0:aaaa"), &[("name", "SC64G")]);
    write(&root.join("mmcblk0/removable"), "1");
    part(&root, "mmcblk0", "mmcblk0p1", 1, 8192, 124_727_296);

    disk(&root, &devices, "loop0", 0, 512, None, &[]);
    disk(&root, &devices, "loop1", 131_072, 512, None, &[]);
    write(&root.join("loop1/loop/backing_file"), "/tmp/test image.img");
    disk(&root, &devices, "dm-0", 41_943_040, 512, None, &[]);
    disk(&root, &devices, "ram0", 131_072, 512, None, &[]);
    disk(&root, &devices, "mmcblk0boot0", 8192, 512, None, &[]);
    disk(&root, &devices, "sr0", 2_097_152, 2048, None, &[]);
    (t, root)
}

fn by_path<'a>(disks: &'a [DiskInfo], path: &str) -> &'a DiskInfo {
    disks.iter().find(|d| d.path == path).unwrap_or_else(|| panic!("{path} missing"))
}

#[test]
fn block_dir_lists_managed_disks_in_order() {
    let (_t, root) = fixture();
    let disks = parse_block_dir(&root);
    let paths: Vec<&str> = disks.iter().map(|d| d.path.as_str()).collect();
    // natural order, loop devices last; loop0 (no backing file), dm-0, ram0, eMMC boot area
    // and the optical drive are skipped
    assert_eq!(paths, ["/dev/mmcblk0", "/dev/nvme0n1", "/dev/sda", "/dev/loop1"]);
    for (i, d) in disks.iter().enumerate() {
        assert_eq!(d.index, i as u32);
    }
}

#[test]
fn sata_disk_with_two_partitions() {
    let (_t, root) = fixture();
    let disks = parse_block_dir(&root);
    let sda = by_path(&disks, "/dev/sda");
    assert_eq!(sda.bus, DiskBus::Sata);
    assert_eq!(sda.model, "Samsung SSD 870 EVO 1TB", "the ATA vendor is dropped");
    assert_eq!(sda.size, 1_953_525_168 * 512);
    assert_eq!((sda.sector_size, sda.physical_sector_size), (512, 4096));
    assert!(sda.flags.trim && !sda.flags.removable && !sda.flags.read_only);
    assert_eq!(sda.serial, "");
    assert_eq!(sda.partitions.len(), 2);
    let p1 = &sda.partitions[0];
    assert_eq!((p1.number, p1.start, p1.len), (1, 1 << 20, 512 << 20));
    assert_eq!(p1.volume, Some(VolumeId("/dev/sda1".into())));
    assert_eq!(sda.partitions[1].volume, Some(VolumeId("/dev/sda2".into())));
    assert_eq!(sda.partitions[1].start, 1_050_624 * 512);
}

#[test]
fn nvme_4kn_offsets_are_in_512_byte_units() {
    // Review Focus 3
    let (_t, root) = fixture();
    let disks = parse_block_dir(&root);
    let nvme = by_path(&disks, "/dev/nvme0n1");
    assert_eq!(nvme.bus, DiskBus::Nvme);
    assert_eq!(nvme.sector_size, 4096);
    assert_eq!(nvme.size, 2_000_409_264 * 512);
    assert_eq!(nvme.model, "WD_BLACK SN850X 1000GB");
    assert_eq!(nvme.serial, "23181Z800123");
    let p1 = &nvme.partitions[0];
    assert_eq!(p1.start, 131_072, "start 256 (512-byte units) = byte 131072, not 256 * 4096");
    assert_eq!(p1.len, 1_048_576 * 512);
    assert_eq!(p1.volume, Some(VolumeId("/dev/nvme0n1p1".into())));
    assert_eq!(nvme.partitions[1].start, 1_048_832 * 512);
}

#[test]
fn removable_sd_card_and_loop_device() {
    let (_t, root) = fixture();
    let disks = parse_block_dir(&root);
    let sd = by_path(&disks, "/dev/mmcblk0");
    assert_eq!(sd.bus, DiskBus::Sd);
    assert!(sd.flags.removable);
    assert_eq!(sd.model, "SC64G");
    assert_eq!(sd.partitions[0].volume, Some(VolumeId("/dev/mmcblk0p1".into())));
    let lp = by_path(&disks, "/dev/loop1");
    assert_eq!(lp.bus, DiskBus::Virtual);
    assert_eq!(lp.size, 64 << 20);
    assert_eq!(lp.model, "Loop device (/tmp/test image.img)");
    assert!(lp.partitions.is_empty());
}

const MOUNTS: &str = "\
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p2 / ext4 rw,relatime,errors=remount-ro 0 0
/dev/nvme0n1p1 /boot/efi vfat rw,relatime,fmask=0077,dmask=0077,codepage=437 0 0
/dev/sda1 /media/alain/My\\040Disk ntfs3 ro,nosuid,nodev,relatime,uid=1000 0 0
tmpfs /run/user/1000 tmpfs rw,nosuid,nodev,relatime,size=1631340k,mode=700 0 0
/dev/mmcblk0p1 /mnt/tab\\011and\\134slash exfat rw 0 0
";

#[test]
fn proc_mounts_with_escapes() {
    let m = parse_proc_mounts(MOUNTS);
    assert_eq!(m.len(), 7);
    let ntfs = m.iter().find(|e| e.device == "/dev/sda1").unwrap();
    assert_eq!(ntfs.mount_point, "/media/alain/My Disk");
    assert_eq!(ntfs.fstype, "ntfs3");
    assert!(ntfs.read_only());
    assert_eq!(m[6].mount_point, "/mnt/tab\tand\\slash");
    assert_eq!(
        m[2],
        MountEntry {
            device: "/dev/nvme0n1p2".into(),
            mount_point: "/".into(),
            fstype: "ext4".into(),
            options: "rw,relatime,errors=remount-ro".into(),
        }
    );
    assert!(!m[2].read_only());
}

#[test]
fn system_disk_from_root_partition() {
    let (_t, root) = fixture();
    let disks = parse_block_dir(&root);
    let nvme = by_path(&disks, "/dev/nvme0n1").index;
    assert_eq!(system_disk_from(&parse_proc_mounts(MOUNTS), &disks), Some(nvme));
    // a filesystem directly on a whole disk (WSL's /dev/sdd)
    let whole = parse_proc_mounts("/dev/sda / ext4 rw 0 0\n");
    assert_eq!(system_disk_from(&whole, &disks), Some(by_path(&disks, "/dev/sda").index));
    // the last mount of / wins (a later mount on top)
    let stacked = format!("{MOUNTS}/dev/sda2 / btrfs rw 0 0\n");
    assert_eq!(system_disk_from(&parse_proc_mounts(&stacked), &disks), Some(by_path(&disks, "/dev/sda").index));
}

#[test]
fn system_disk_fails_closed_on_lvm_dm_and_pseudo_roots() {
    // Review Focus 4: never a wrong index
    let (_t, root) = fixture();
    let disks = parse_block_dir(&root);
    for line in [
        "/dev/mapper/ubuntu--vg-ubuntu--lv / ext4 rw,relatime 0 0",
        "/dev/dm-0 / xfs rw 0 0",
        "/dev/md127 / ext4 rw 0 0",
        "overlay / overlay rw,lowerdir=/a,upperdir=/b,workdir=/c 0 0",
        "rootfs / rootfs rw 0 0",
        "rpool/ROOT/ubuntu / zfs rw 0 0",
        "/dev/root / ext4 rw 0 0",
    ] {
        let m = parse_proc_mounts(&format!("{line}\n/dev/nvme0n1p1 /boot/efi vfat rw 0 0\n"));
        assert_eq!(system_disk_from(&m, &disks), None, "{line}");
    }
    assert_eq!(system_disk_from(&[], &disks), None, "no / at all");
}
