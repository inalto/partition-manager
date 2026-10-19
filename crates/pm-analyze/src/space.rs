//! Space analyzer: a parallel directory walk that builds a size tree.

use pm_core::{CancelToken, Error, ProgressSink, ProgressTracker, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

/// Number of entries kept in [`SpaceReport::largest`].
const LARGEST_KEEP: usize = 100;
/// `FILE_ATTRIBUTE_REPARSE_POINT`: symlinks, junctions, mount points, cloud placeholders.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// One directory in the size tree. Files are not nodes: they are aggregated into the sizes and
/// counts of their directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub path: PathBuf,
    /// Bytes of all files below (and directly in) this directory.
    pub size: u64,
    /// Number of files below this directory.
    pub files: u64,
    /// Number of directories below this directory (all depths, not counting itself).
    pub dirs: u64,
    /// Sub-directories only, largest first.
    pub children: Vec<Node>,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpaceReport {
    pub root: Node,
    pub total: u64,
    /// `(lowercased extension without dot ("" = none), bytes, file count)`, largest first.
    pub by_ext: Vec<(String, u64, u64)>,
    /// The 100 largest files, largest first.
    pub largest: Vec<(PathBuf, u64)>,
    /// Entries that could not be read (access denied, vanished, ...).
    pub errors: u64,
}

/// What one directory contributes by itself.
#[derive(Default)]
struct DirRec {
    size: u64,
    files: u64,
    subdirs: Vec<PathBuf>,
}

/// Per-worker accumulators, merged after the walk.
#[derive(Default)]
struct Local {
    by_ext: HashMap<String, (u64, u64)>,
    largest: BinaryHeap<Reverse<(u64, PathBuf)>>,
    errors: u64,
}

impl Local {
    fn add_file(&mut self, path: PathBuf, size: u64) {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let e = self.by_ext.entry(ext).or_default();
        e.0 += size;
        e.1 += 1;
        push_largest(&mut self.largest, size, path);
    }
}

fn push_largest(heap: &mut BinaryHeap<Reverse<(u64, PathBuf)>>, size: u64, path: PathBuf) {
    if heap.len() < LARGEST_KEEP {
        heap.push(Reverse((size, path)));
    } else if heap.peek().map_or(false, |Reverse((min, _))| size > *min) {
        heap.pop();
        heap.push(Reverse((size, path)));
    }
}

struct Queue {
    dirs: VecDeque<PathBuf>,
    /// Directories currently being processed by a worker.
    active: usize,
}

struct Shared<'a> {
    queue: Mutex<Queue>,
    wake: Condvar,
    recs: Mutex<HashMap<PathBuf, DirRec>>,
    progress: Mutex<ProgressTracker<'a>>,
    stop: AtomicBool,
    cancel: &'a CancelToken,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the entry is a symlink / junction / other reparse point (never followed).
fn is_link(ft: &fs::FileType, md: &fs::Metadata) -> bool {
    if ft.is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    let _ = md;
    false
}

/// Read one directory: tally its files, return its sub-directories. Never follows links.
fn process_dir(dir: &Path, local: &mut Local) -> (DirRec, u64) {
    let mut rec = DirRec::default();
    let mut visited = 0u64;
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => {
            local.errors += 1;
            return (rec, 0);
        }
    };
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                local.errors += 1;
                continue;
            }
        };
        visited += 1;
        // `DirEntry::metadata` does not traverse symlinks, so links are seen as links.
        let (ft, md) = match (entry.file_type(), entry.metadata()) {
            (Ok(ft), Ok(md)) => (ft, md),
            _ => {
                local.errors += 1;
                continue;
            }
        };
        if is_link(&ft, &md) {
            continue;
        }
        if ft.is_dir() {
            rec.subdirs.push(entry.path());
        } else if ft.is_file() {
            rec.size += md.len();
            rec.files += 1;
            local.add_file(entry.path(), md.len());
        }
    }
    (rec, visited)
}

fn worker(sh: &Shared<'_>) -> Local {
    let mut local = Local::default();
    loop {
        let dir = {
            let mut q = lock(&sh.queue);
            loop {
                if sh.stop.load(Ordering::SeqCst) {
                    return local;
                }
                if let Some(d) = q.dirs.pop_front() {
                    q.active += 1;
                    break d;
                }
                if q.active == 0 {
                    sh.wake.notify_all();
                    return local;
                }
                q = sh.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        if sh.cancel.is_cancelled() {
            sh.stop.store(true, Ordering::SeqCst);
        }
        let rec = if sh.stop.load(Ordering::SeqCst) {
            DirRec::default()
        } else {
            let (rec, visited) = process_dir(&dir, &mut local);
            let mut p = lock(&sh.progress);
            p.set_detail(&dir.to_string_lossy());
            p.add(visited);
            rec
        };
        let subdirs = rec.subdirs.clone();
        lock(&sh.recs).insert(dir, rec);
        let mut q = lock(&sh.queue);
        q.dirs.extend(subdirs);
        q.active -= 1;
        sh.wake.notify_all();
    }
}

/// Assemble the tree bottom-up without recursion (directory nesting can be thousands deep).
fn build_tree(root: &Path, recs: &mut HashMap<PathBuf, DirRec>) -> Node {
    let mut order = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if let Some(rec) = recs.get(&p) {
            stack.extend(rec.subdirs.iter().cloned());
        }
        order.push(p);
    }
    let mut built: HashMap<PathBuf, Node> = HashMap::new();
    for path in order.into_iter().rev() {
        let rec = recs.remove(&path).unwrap_or_default();
        let mut node = Node {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            size: rec.size,
            files: rec.files,
            dirs: 0,
            children: Vec::with_capacity(rec.subdirs.len()),
            is_dir: true,
            path: path.clone(),
        };
        for sub in &rec.subdirs {
            if let Some(child) = built.remove(sub) {
                node.size += child.size;
                node.files += child.files;
                node.dirs += 1 + child.dirs;
                node.children.push(child);
            }
        }
        node.children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        built.insert(path, node);
    }
    built.remove(root).expect("root was processed")
}

/// Walk `root` with `threads` workers and build the size tree.
///
/// Symlinks, junctions and other reparse points are never followed (and not counted); entries
/// that cannot be read are counted in [`SpaceReport::errors`] instead of failing the scan.
/// Progress reports phase "Scanning" with `done` = entries visited, `total` = 0 (unknown) and
/// the directory being read in `detail`. Cancellation is checked per directory.
pub fn scan_space(
    root: &Path,
    threads: usize,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<SpaceReport> {
    cancel.check()?;
    let md = fs::metadata(root).map_err(|e| Error::io(0, 0, e))?;
    if !md.is_dir() {
        return Err(Error::Invalid(format!("{} is not a directory", root.display())));
    }
    let shared = Shared {
        queue: Mutex::new(Queue { dirs: VecDeque::from([root.to_path_buf()]), active: 0 }),
        wake: Condvar::new(),
        recs: Mutex::new(HashMap::new()),
        progress: Mutex::new(ProgressTracker::new(progress, "Scanning", 0)),
        stop: AtomicBool::new(false),
        cancel,
    };
    let threads = threads.max(1);
    let locals: Vec<Local> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| worker(&shared))).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    lock(&shared.progress).finish();
    cancel.check()?;

    let mut by_ext: HashMap<String, (u64, u64)> = HashMap::new();
    let mut heap = BinaryHeap::new();
    let mut errors = 0;
    for l in locals {
        errors += l.errors;
        for (ext, (size, count)) in l.by_ext {
            let e = by_ext.entry(ext).or_default();
            e.0 += size;
            e.1 += count;
        }
        for Reverse((size, path)) in l.largest {
            push_largest(&mut heap, size, path);
        }
    }
    let mut by_ext: Vec<_> = by_ext.into_iter().map(|(e, (s, c))| (e, s, c)).collect();
    by_ext.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut largest: Vec<_> = heap.into_iter().map(|Reverse((s, p))| (p, s)).collect();
    largest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut recs = std::mem::take(&mut *lock(&shared.recs));
    let root_node = build_tree(root, &mut recs);
    Ok(SpaceReport { total: root_node.size, root: root_node, by_ext, largest, errors })
}
