//! Fixtures shared by the unit tests.

use pm_core::{gpt_types, DiskInfo, FsKind, PartFlags, PartitionInfo, PartitionType, TableKind, VolumeId, MIB};
use pm_ops::Model;

pub fn part(number: u32, start_mib: u64, len_mib: u64) -> PartitionInfo {
    PartitionInfo {
        number,
        start: start_mib * MIB,
        len: len_mib * MIB,
        ..Default::default()
    }
}

/// 1 GiB GPT disk: EFI 100 MiB, MSR 16 MiB, data 684 MiB (E:, NTFS, half used), free tail.
pub fn gpt_disk(index: u32) -> DiskInfo {
    let mut efi = part(1, 1, 100);
    efi.ptype = Some(PartitionType::Gpt(gpt_types::EFI_SYSTEM));
    efi.fs = Some(FsKind::Fat32);
    efi.used = Some(30 * MIB);
    let mut msr = part(2, 101, 16);
    msr.ptype = Some(PartitionType::Gpt(gpt_types::MSR));
    let mut data = part(3, 117, 684);
    data.ptype = Some(PartitionType::Gpt(gpt_types::BASIC_DATA));
    data.fs = Some(FsKind::Ntfs);
    data.label = "Data".into();
    data.mount_points = vec!["E:\\".into()];
    data.used = Some(342 * MIB);
    data.volume = Some(VolumeId("vol-e".into()));
    for (i, p) in [&mut efi, &mut msr, &mut data].into_iter().enumerate() {
        p.guid = Some(uuid::Uuid::from_u128(0x1000 + i as u128 + index as u128 * 16));
    }
    DiskInfo {
        index,
        path: format!(r"\.\PhysicalDrive{index}"),
        model: "Test SSD".into(),
        size: 1024 * MIB,
        sector_size: 512,
        physical_sector_size: 4096,
        table: TableKind::Gpt,
        partitions: vec![efi, msr, data],
        ..Default::default()
    }
}

/// 1 GiB MBR disk: primary 1 (active, 200 MiB), extended 2 (400 MiB) with logicals 5 and 6.
pub fn mbr_disk(index: u32) -> DiskInfo {
    let mut p1 = part(1, 1, 200);
    p1.ptype = Some(PartitionType::Mbr(0x07));
    p1.fs = Some(FsKind::Ntfs);
    p1.flags = PartFlags { active: true, ..Default::default() };
    let mut ext = part(2, 300, 400);
    ext.ptype = Some(PartitionType::Mbr(0x0F));
    let mut l5 = part(5, 301, 100);
    l5.ptype = Some(PartitionType::Mbr(0x0B));
    l5.fs = Some(FsKind::Fat32);
    l5.logical = true;
    let mut l6 = part(6, 402, 100);
    l6.ptype = Some(PartitionType::Mbr(0x83));
    l6.fs = Some(FsKind::Ext4);
    l6.logical = true;
    DiskInfo {
        index,
        model: "Old HDD".into(),
        size: 1024 * MIB,
        sector_size: 512,
        table: TableKind::Mbr,
        disk_id: "1234ABCD".into(),
        partitions: vec![p1, ext, l5, l6],
        ..Default::default()
    }
}

pub fn model_of(disks: Vec<DiskInfo>) -> Model {
    // Test models are Windows-shaped (drive letters, NTFS defaults) whatever the host runs.
    Model { disks, volumes: Vec::new(), system_disk: None, windows_found: Vec::new(), os: "windows".into() }
}

/// A platform that places every path under `dir` on a disk no test model has (disk 99), so
/// files written there are on no source or target of the test.
pub fn platform_placing(dir: &std::path::Path) -> std::sync::Arc<dyn pm_platform::Platform> {
    let p = pm_platform::MockPlatform::new();
    p.set_disk_of_path(dir, 99, None);
    std::sync::Arc::new(p)
}
