mod common;

use common::*;
use pm_core::units::MIB;
use pm_core::{CancelToken, Error, MemDevice, NullProgress};
use pm_fs::{open, FileSystem};
use pm_recover::{deleted_files, deleted_files_at, restore_file, DeletedFile};

const GONE: &[u8] = b"the quick brown fox jumps over the lazy dog";

struct Fixture {
    fs: Box<dyn FileSystem>,
    big: Vec<u8>,
    lost: Vec<u8>,
}

/// 40 MiB FAT32 volume (512-byte clusters) with these root entries:
///   KEEP.TXT   live
///   GONE.TXT   deleted, 43 bytes, chain freed (as a real delete does)
///   EMPTY.TXT  deleted, 0 bytes
///   BIG.BIN    deleted, 1300 bytes, FAT chain left intact
///   LOST.BIN   deleted, 1300 bytes, FAT chain freed
fn fixture() -> Fixture {
    let mut dev = MemDevice::new(40 * MIB);
    format_part(&mut dev, 0, 40 * MIB, "RECOVER");
    let big = text_bytes(1300, 1);
    let lost = text_bytes(1300, 2);
    {
        let mut w = Fat32Writer::new(&mut dev);
        w.add_file(dos_name("KEEP", "TXT"), b"still here");
        w.add_file(dos_name("GONE", "TXT"), GONE);
        w.add_file(dos_name("EMPTY", "TXT"), b"");
        w.add_file(dos_name("BIG", "BIN"), &big);
        w.add_file(dos_name("LOST", "BIN"), &lost);
        w.delete(dos_name("GONE", "TXT"), true);
        w.delete(dos_name("EMPTY", "TXT"), true);
        w.delete(dos_name("BIG", "BIN"), false);
        w.delete(dos_name("LOST", "BIN"), true);
    }
    Fixture { fs: open(Box::new(dev)).unwrap(), big, lost }
}

fn find<'a>(v: &'a [DeletedFile], name: &str) -> &'a DeletedFile {
    v.iter().find(|f| f.entry.name == name).unwrap_or_else(|| panic!("{name} not in {v:?}"))
}

#[test]
fn deleted_fat_file_restored_contents_match() {
    let mut fx = fixture();
    let list = deleted_files(&mut *fx.fs, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(list.len(), 4, "{list:?}");
    assert!(list.iter().all(|f| f.entry.deleted && f.fs_offset == 0));
    assert!(list.iter().all(|f| f.entry.name != "?eep.txt"));

    let dest = tempfile::tempdir().unwrap();
    let gone = find(&list, "?one.txt");
    let p = restore_file(&mut *fx.fs, gone, dest.path()).unwrap();
    assert_eq!(p, dest.path().join("one.txt"));
    assert_eq!(std::fs::read(&p).unwrap(), GONE);

    // a name collision gets a numeric suffix instead of overwriting
    let p2 = restore_file(&mut *fx.fs, gone, dest.path()).unwrap();
    assert_eq!(p2, dest.path().join("one_1.txt"));
    let p3 = restore_file(&mut *fx.fs, gone, dest.path()).unwrap();
    assert_eq!(p3, dest.path().join("one_2.txt"));
    assert_eq!(std::fs::read(&p3).unwrap(), GONE);

    // a multi-cluster file whose chain survived restores completely
    let big = find(&list, "?ig.bin");
    let p = restore_file(&mut *fx.fs, big, dest.path()).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), fx.big);

    // an empty file restores as an empty file
    let empty = find(&list, "?mpty.txt");
    let p = restore_file(&mut *fx.fs, empty, dest.path()).unwrap();
    assert_eq!(std::fs::read(&p).unwrap().len(), 0);
}

#[test]
fn integrity_estimates_follow_readability() {
    let mut fx = fixture();
    let list = deleted_files(&mut *fx.fs, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(find(&list, "?mpty.txt").integrity, 100);
    assert_eq!(find(&list, "?one.txt").integrity, 80);
    assert_eq!(find(&list, "?ig.bin").integrity, 80);
    // the start is readable but the freed chain ends after the first cluster
    assert_eq!(find(&list, "?ost.bin").integrity, 40);
    assert!(list.iter().all(|f| f.integrity <= 100));
}

#[test]
fn restore_failure_leaves_no_partial_file() {
    let mut fx = fixture();
    let list = deleted_files(&mut *fx.fs, &NullProgress, &CancelToken::new()).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let lost = find(&list, "?ost.bin");
    assert!(restore_file(&mut *fx.fs, lost, dest.path()).is_err());
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    let _ = &fx.lost;
}

#[test]
fn restore_creates_destination_and_sanitizes_names() {
    let mut fx = fixture();
    let list = deleted_files(&mut *fx.fs, &NullProgress, &CancelToken::new()).unwrap();
    let base = tempfile::tempdir().unwrap();
    let dest = base.path().join("out").join("nested");
    let mut f = find(&list, "?one.txt").clone();
    f.entry.name = "../x\\y:z*?\"<>|.txt".into();
    let p = restore_file(&mut *fx.fs, &f, &dest).unwrap();
    assert_eq!(p.parent().unwrap(), dest);
    assert_eq!(p.file_name().unwrap().to_str().unwrap(), ".._x_y_z______.txt");
    assert_eq!(std::fs::read(&p).unwrap(), GONE);

    for hostile in ["..", ".", "", "?", "   ", "CON"] {
        f.entry.name = hostile.into();
        let p = restore_file(&mut *fx.fs, &f, &dest).unwrap();
        assert_eq!(p.parent().unwrap(), dest, "{hostile:?}");
        let n = p.file_name().unwrap().to_str().unwrap().to_string();
        assert!(!n.is_empty() && n != "." && n != "..", "{hostile:?} -> {n:?}");
    }
}

#[test]
fn deleted_files_at_records_the_filesystem_offset_and_honours_cancel() {
    let mut fx = fixture();
    let list = deleted_files_at(&mut *fx.fs, 1 << 20, &NullProgress, &CancelToken::new()).unwrap();
    assert!(list.iter().all(|f| f.fs_offset == 1 << 20));
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(deleted_files(&mut *fx.fs, &NullProgress, &cancel), Err(Error::Cancelled)));
}
