//! Tests against the real machine. Ignored by default; run with
//! `cargo test -p pm-platform --test windows_live -- --ignored --nocapture`.
//! `live_enumerate_disks_nonempty` works unelevated; `live_open_disk0_reads_sector0` needs an
//! elevated (administrator) shell. Both are read-only.
#![cfg(windows)]

use pm_core::{BlockDevice, TableKind};
use pm_platform::{OpenMode, Platform, WindowsPlatform};

#[test]
#[ignore = "touches real hardware (read-only)"]
fn live_enumerate_disks_nonempty() {
    let p = WindowsPlatform::new();
    let disks = p.enumerate_disks().unwrap();
    for d in &disks {
        println!(
            "{} | {} | serial {:?} | {:?} | {} B | {}/{} | {:?} {} | {:?}",
            d.path,
            d.display_name(),
            d.serial,
            d.bus,
            d.size,
            d.sector_size,
            d.physical_sector_size,
            d.table,
            d.disk_id,
            d.flags
        );
        for part in &d.partitions {
            println!(
                "    #{} start={} len={} {:?} {:?} name={:?} fs={:?} label={:?} mp={:?} used={:?} flags={:?}",
                part.number,
                part.start,
                part.len,
                part.ptype.map(|t| t.name()),
                part.guid,
                part.name,
                part.fs,
                part.label,
                part.mount_points,
                part.used,
                part.flags
            );
        }
    }
    assert!(!disks.is_empty());
    let sys = p.system_disk().expect("system disk");
    let sd = disks.iter().find(|d| d.index == sys).expect("system disk enumerated");
    assert!(sd.flags.system);
    assert!(sd.size > 0);
    assert!(sd.sector_size >= 512);
    assert!(matches!(sd.table, TableKind::Mbr | TableKind::Gpt));
    assert!(!sd.partitions.is_empty());
    // the Windows volume is merged into one of the system disk's partitions
    assert!(sd.partitions.iter().any(|p| p.volume.is_some() && p.fs.is_some()));

    let vols = p.enumerate_volumes().unwrap();
    assert!(vols.iter().any(|v| v.disk == Some(sys)));
    println!("elevated: {}, free letters: {:?}", p.is_elevated(), p.free_letters());
}

#[test]
#[ignore = "needs administrator rights (read-only)"]
fn live_open_disk0_reads_sector0() {
    let p = WindowsPlatform::new();
    let mut d = p.open_disk(0, OpenMode::ReadOnly).unwrap();
    assert!(d.len() > 0);
    assert!(d.is_read_only());
    let mut sector = vec![0u8; d.sector_size() as usize];
    d.read_at(0, &mut sector).unwrap();
    assert_eq!(&sector[510..512], &[0x55, 0xAA]);
    // unaligned reads are bounced through an aligned buffer
    let mut sig = [0u8; 2];
    d.read_at(510, &mut sig).unwrap();
    assert_eq!(sig, [0x55, 0xAA]);
    assert!(d.write_at(0, &sector).is_err());
}

#[test]
#[ignore = "touches real hardware (read-only); NVMe/ATA SMART usually needs administrator rights"]
fn live_smart_disk0() {
    let p = WindowsPlatform::new();
    match p.smart(0).unwrap() {
        Some(d) => println!("SMART disk 0: {d:#?}"),
        None => println!("SMART disk 0: not available (elevated: {})", p.is_elevated()),
    }
}
