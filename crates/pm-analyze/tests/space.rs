use pm_analyze::scan_space;
use pm_core::{CancelToken, Error, FnProgress, NullProgress, ProgressReport};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

fn write(path: &Path, len: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![7u8; len]).unwrap();
}

fn tree() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(&r.join("a.txt"), 100);
    write(&r.join("b.TXT"), 50);
    write(&r.join("noext"), 10);
    write(&r.join("sub/c.bin"), 1000);
    write(&r.join("sub/deep/d.txt"), 200);
    write(&r.join("sub/deep/e.bin"), 300);
    fs::create_dir_all(r.join("empty")).unwrap();
    t
}

#[test]
fn scan_space_sizes_and_counts() {
    let t = tree();
    for threads in [1usize, 4] {
        let rep = scan_space(t.path(), threads, &NullProgress, &CancelToken::new()).unwrap();
        assert_eq!(rep.total, 1660, "threads={threads}");
        assert_eq!(rep.errors, 0);
        let root = &rep.root;
        assert!(root.is_dir);
        assert_eq!((root.size, root.files, root.dirs), (1660, 6, 3));
        // children are directories only, largest first
        let names: Vec<_> = root.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["sub", "empty"]);
        let sub = &root.children[0];
        assert_eq!((sub.size, sub.files, sub.dirs), (1500, 3, 1));
        assert_eq!(sub.path, t.path().join("sub"));
        assert_eq!(sub.children[0].name, "deep");
        assert_eq!((sub.children[0].size, sub.children[0].files), (500, 2));
        assert_eq!(root.children[1].size, 0);

        // by extension: lowercased, "" for none; sorted by size desc
        let ext: Vec<_> = rep.by_ext.iter().map(|(e, s, c)| (e.as_str(), *s, *c)).collect();
        assert_eq!(ext, [("bin", 1300, 2), ("txt", 350, 3), ("", 10, 1)]);
        // largest files first
        let largest: Vec<_> = rep
            .largest
            .iter()
            .map(|(p, s)| (p.file_name().unwrap().to_str().unwrap(), *s))
            .collect();
        assert_eq!(largest[0], ("c.bin", 1000));
        assert_eq!(largest[1], ("e.bin", 300));
        assert_eq!(largest.len(), 6);
    }
}

#[test]
fn scan_space_keeps_only_top_100_largest() {
    let t = tempfile::tempdir().unwrap();
    for i in 0..130usize {
        write(&t.path().join(format!("d{}/f{i}.dat", i % 5)), i + 1);
    }
    let rep = scan_space(t.path(), 3, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(rep.largest.len(), 100);
    assert_eq!(rep.largest[0].1, 130);
    assert_eq!(rep.largest[99].1, 31);
    assert_eq!(rep.root.files, 130);
}

#[test]
fn scan_space_reports_progress_and_honours_cancel() {
    let t = tree();
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    scan_space(t.path(), 2, &sink, &CancelToken::new()).unwrap();
    {
        let r = reports.lock().unwrap();
        let last = r.last().expect("final report");
        assert_eq!(last.phase, "Scanning");
        assert_eq!(last.total, 0);
        assert!(last.done >= 9, "entries visited: {}", last.done);
    }
    let c = CancelToken::new();
    c.cancel();
    assert!(matches!(scan_space(t.path(), 2, &NullProgress, &c), Err(Error::Cancelled)));
}

#[test]
fn scan_space_missing_root_is_error() {
    let t = tempfile::tempdir().unwrap();
    assert!(scan_space(&t.path().join("nope"), 1, &NullProgress, &CancelToken::new()).is_err());
    let f = t.path().join("file");
    write(&f, 3);
    assert!(scan_space(&f, 1, &NullProgress, &CancelToken::new()).is_err());
}

#[cfg(windows)]
#[test]
fn scan_space_does_not_follow_junctions() {
    let t = tree();
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("big.bin"), 5000);
    let link = t.path().join("link");
    let ok = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: cannot create a junction here");
        return;
    }
    let rep = scan_space(t.path(), 2, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(rep.total, 1660, "junction target must not be counted");
    assert!(rep.root.children.iter().all(|c| c.name != "link"));
    // remove the junction itself, not its target
    fs::remove_dir(&link).unwrap();
    assert!(outside.path().join("big.bin").exists());
}
