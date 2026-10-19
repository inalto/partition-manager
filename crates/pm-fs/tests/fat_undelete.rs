//! Reading deleted FAT files: the freed chain is replaced by a contiguous-run assumption.

mod common;

use common::*;
use pm_core::{CancelToken, Error, NullProgress};
use pm_fs::{open, Entry, FileSystem};

/// Add a file the way Windows deletes it: entry's first byte 0xE5, FAT chain entries zeroed.
/// `keep_links` leaves that many leading chain links (cluster -> next) intact.
fn add_deleted(
    img: &mut Img,
    name: [u8; 11],
    clusters: &[u32],
    data: &[u8],
    keep_links: usize,
) {
    img.write_chain(clusters, data);
    for (i, &c) in clusters.iter().enumerate() {
        // link i exists as c -> clusters[i + 1]; keep the first `keep_links` of them
        if i >= keep_links {
            img.set_fat(c, 0);
        }
    }
    let mut e = Short::file(name, clusters[0], data.len() as u32).bytes();
    e[0] = 0xE5;
    img.put_entries(DirLoc::Cluster(2), &[e]);
}

fn deleted(fs: &mut Box<dyn FileSystem>, name: &str) -> Entry {
    let all = fs.deleted_entries(&NullProgress, &CancelToken::new()).unwrap();
    all.into_iter().find(|e| e.name == name).unwrap_or_else(|| panic!("{name} not listed"))
}

fn extract(fs: &mut Box<dyn FileSystem>, e: &Entry) -> Vec<u8> {
    let mut v = Vec::new();
    let n = fs.extract(e, &mut v).unwrap();
    assert_eq!(n as usize, v.len());
    v
}

#[test]
fn fat32_deleted_multi_cluster_files_are_recoverable() {
    let mut fx = fat32_fixture();
    let img = &mut fx.img;

    // fully freed chain: 3 contiguous clusters, every FAT entry zero
    let d1 = sample_bytes(1300, 13);
    add_deleted(img, dos_name("DELONE", "BIN"), &[100, 101, 102], &d1, 0);
    // only the first link survived (110 -> 111), the rest is free
    let d2 = sample_bytes(3 * 512, 17);
    add_deleted(img, dos_name("DELTWO", "BIN"), &[110, 111, 112], &d2, 1);
    // an intact non-contiguous prefix (120 -> 125), then freed: continue after the last link
    let d3 = sample_bytes(3 * 512 - 7, 19);
    add_deleted(img, dos_name("DELTHR", "BIN"), &[120, 125, 126], &d3, 1);

    let mut fs: Box<dyn FileSystem> = open(Box::new(fx.img.dev)).unwrap();
    for (name, size, id, data) in [
        ("?elone.bin", 1300u64, 100u64, &d1),
        ("?eltwo.bin", 1536, 110, &d2),
        ("?elthr.bin", 1529, 120, &d3),
    ] {
        let e = deleted(&mut fs, name);
        assert!(e.deleted);
        assert_eq!(e.size, size, "{name}");
        assert_eq!(e.id, id, "{name}");
        assert_eq!(&extract(&mut fs, &e), data, "{name}");
        // partial reads across cluster boundaries (exercises the cursor's contiguous state)
        let mut buf = vec![0u8; 600];
        assert_eq!(fs.read_file(&e, 300, &mut buf).unwrap(), 600);
        assert_eq!(&buf[..], &data[300..900], "{name}");
        let mut tail = vec![0u8; 100];
        assert_eq!(fs.read_file(&e, size - 40, &mut tail).unwrap(), 40);
        assert_eq!(&tail[..40], &data[size as usize - 40..], "{name}");
    }
}

#[test]
fn fat_deleted_heuristic_does_not_apply_to_live_files_and_is_bounded() {
    let mut fx = fat32_fixture();
    let data = sample_bytes(3 * 512, 23);
    fx.img.write_chain(&[100, 101, 102], &data);
    fx.img.set_fat(100, 0); // live file with a free first entry: still an error
    fx.img.put_entries(DirLoc::Cluster(2), &[Short::file(dos_name("LIVE", "BIN"), 100, 1536).bytes()]);

    // deleted file at the very last cluster of the volume claiming three clusters
    let total_sectors = fx.img.dev.as_slice().len() as u64 / 512;
    let count = total_sectors - fx.img.data_off / 512; // 1 sector per cluster
    let last = count as u32 + 1;
    let mut e = Short::file(dos_name("RUNOFF", "BIN"), last, 1536).bytes();
    e[0] = 0xE5;
    // a deleted file whose first cluster is not a valid cluster at all
    let mut bad = Short::file(dos_name("NOWHERE", "BIN"), 0x0FFF_FF00, 512).bytes();
    bad[0] = 0xE5;
    fx.img.put_entries(DirLoc::Cluster(2), &[e, bad]);

    let mut fs: Box<dyn FileSystem> = open(Box::new(fx.img.dev)).unwrap();
    let root = fs.root();
    let live = fs.list(&root).unwrap().into_iter().find(|e| e.name == "LIVE.BIN").unwrap();
    let mut buf = vec![0u8; 1536];
    assert!(matches!(fs.read_file(&live, 0, &mut buf), Err(Error::Parse(_))));

    let runoff = deleted(&mut fs, "?unoff.bin");
    // the first cluster is readable, the run then leaves the volume
    let mut one = vec![0u8; 512];
    assert_eq!(fs.read_file(&runoff, 0, &mut one).unwrap(), 512);
    assert!(matches!(fs.read_file(&runoff, 0, &mut buf), Err(Error::Parse(_))));

    let nowhere = deleted(&mut fs, "?owhere.bin");
    assert!(matches!(fs.read_file(&nowhere, 0, &mut one), Err(Error::Parse(_))));
}
