//! Parsing and opening of the `<device>` command line argument.

use crate::cli::{arg_error, CliError};
use pm_core::{BlockDevice, DiskInfo, Error, Result, SliceDevice, VolumeId};
use pm_parttable::read_table;
use pm_platform::{OpenMode, Platform};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceArg {
    /// Physical disk by index (`disk:1`).
    Disk(u32),
    /// Drive letter (`D:`), `\\?\Volume{..}\` path or (Linux, macOS) a `/dev/...` node.
    Volume(String),
    /// Image file (`file:PATH`, or a bare path that exists); any format `pm-image` reads.
    File(PathBuf),
    /// Partition `number` (1-based, as `pm table` prints it) of a disk or an image file:
    /// `disk:N:P`, `file:PATH:P`.
    Partition(Box<DeviceArg>, u32),
}

/// What a device argument is ultimately stored on, for "is this the same device" checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    Disk(u32),
    Volume(String),
    /// Canonicalised path of an image file.
    File(PathBuf),
}

/// An opened device together with where it lives, so commands can refuse to copy a device
/// onto itself (or onto an overlapping partition of the same disk).
pub struct Opened {
    pub dev: Box<dyn BlockDevice>,
    /// The disk, volume or file the data is stored on.
    pub identity: Identity,
    /// The byte range of `identity` that `dev` covers (all of it unless a partition).
    pub range: (u64, u64),
    /// `(disk index, start, end)`: where `dev` lives on a physical disk, when known. Set for
    /// disks and their partitions, and for volumes the platform places on a disk, so a volume
    /// and the disk (or partition) it sits on can be recognised as the same storage.
    pub disk_extent: Option<(u32, u64, u64)>,
    /// Canonical paths of further files the data is stored in (the flat extent of a VMDK
    /// descriptor).
    pub files: Vec<PathBuf>,
}

fn ranges_overlap(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

impl Opened {
    /// Every file this device reads from: the file itself plus its dependent extents.
    pub fn source_files(&self) -> Vec<PathBuf> {
        let mut v = self.files.clone();
        if let Identity::File(p) = &self.identity {
            v.push(p.clone());
        }
        v
    }

    /// The physical disk this device is on, if known.
    pub fn disk_index(&self) -> Option<u32> {
        self.disk_extent.map(|e| e.0)
    }

    /// Whether writing `len` bytes at `offset` of `self` would touch the storage `other`
    /// covers: the same disk / volume / file range (the bytes written), overlapping extents of
    /// one physical disk (a volume against its disk or a partition), or a file against
    /// another file's extent.
    pub fn overlaps(&self, offset: u64, len: u64, other: &Opened) -> bool {
        let start = self.range.0.saturating_add(offset);
        let window = (start, start.saturating_add(len));
        if self.identity == other.identity {
            return ranges_overlap(window, other.range);
        }
        // Different handles on one physical disk (a volume and its disk, a volume and a
        // partition): compare whole extents, not just the bytes about to be written, so a copy
        // never lands in a device that contains (or is contained by) the other one.
        if let (Some(a), Some(b)) = (self.disk_extent, other.disk_extent) {
            if a.0 == b.0 && ranges_overlap((a.1, a.2), (b.1, b.2)) {
                return true;
            }
        }
        match (&self.identity, &other.identity) {
            (Identity::File(a), Identity::File(b)) => self.files.contains(b) || other.files.contains(a),
            _ => false,
        }
    }
}

/// Canonical paths of the extent files a VMDK text descriptor at `path` refers to (empty for
/// every other file).
pub fn vmdk_extent_files(path: &Path) -> Vec<PathBuf> {
    use std::io::Read;
    const SIGNATURE: &str = "# Disk DescriptorFile";
    let mut head = Vec::new();
    if std::fs::File::open(path).and_then(|f| f.take(1 << 20).read_to_end(&mut head)).is_err() {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(head) else {
        return Vec::new();
    };
    if !text.starts_with(SIGNATURE) {
        return Vec::new();
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    text.lines()
        .map(str::trim)
        .filter(|l| ["RW ", "RDONLY ", "NOACCESS "].iter().any(|p| l.starts_with(p)))
        .filter_map(|l| {
            let (a, b) = (l.find('"')?, l.rfind('"')?);
            (b > a).then(|| &l[a + 1..b])
        })
        .filter_map(|name| {
            let p = PathBuf::from(name);
            let p = if p.is_absolute() { p } else { dir.join(p) };
            std::fs::canonicalize(p).ok()
        })
        .collect()
}

/// Where the volume `id` lives on a physical disk, according to the platform.
pub fn volume_extent(p: &dyn Platform, id: &VolumeId) -> Option<(u32, u64, u64)> {
    if let Ok(vols) = p.enumerate_volumes() {
        if let Some(v) = vols.iter().find(|v| &v.id == id) {
            if let (Some(d), true) = (v.disk, v.len > 0) {
                return Some((d, v.start, v.start.saturating_add(v.len)));
            }
        }
    }
    p.enumerate_disks().ok()?.into_iter().find_map(|d| {
        d.partitions
            .iter()
            .find(|part| part.volume.as_ref() == Some(id))
            .map(|part| (d.index, part.start, part.end()))
    })
}

/// `D:` / `D:\` -> `Some('D')`.
fn drive_letter(s: &str) -> Option<char> {
    let b = s.as_bytes();
    let ok = b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')));
    ok.then(|| (b[0] as char).to_ascii_uppercase())
}

/// `prefix` (ASCII, case-insensitive) stripped from the start of `s`.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

fn is_volume_path(s: &str) -> bool {
    strip_prefix_ci(s, r"\\?\Volume{").is_some()
}

/// A device node path (`/dev/sda1`, `/dev/rdisk3s2`) as used on Linux and macOS.
pub fn is_dev_node(s: &str) -> bool {
    s.strip_prefix("/dev/").is_some_and(|rest| !rest.is_empty() && !rest.ends_with('/'))
}

/// The other name of a macOS whole disk or slice: `/dev/rdiskN` (raw) and `/dev/diskN`
/// (buffered) are the same storage. `None` for every other node.
fn disk_node_twin(node: &str) -> Option<String> {
    if let Some(rest) = node.strip_prefix("/dev/rdisk") {
        return rest.starts_with(|c: char| c.is_ascii_digit()).then(|| format!("/dev/disk{rest}"));
    }
    let rest = node.strip_prefix("/dev/disk")?;
    rest.starts_with(|c: char| c.is_ascii_digit()).then(|| format!("/dev/rdisk{rest}"))
}

/// The index of the disk whose whole-disk node is `node` (`/dev/sdb`, `/dev/nvme0n1`,
/// `/dev/disk2` or `/dev/rdisk2`), matched against [`DiskInfo::path`]. The macOS raw /
/// buffered twin matches too, and a symlink (`/dev/disk/by-id/...`) is followed.
pub fn disk_of_node(disks: &[DiskInfo], node: &str) -> Option<u32> {
    if !is_dev_node(node) {
        return None;
    }
    let mut names = vec![node.to_string()];
    if let Ok(real) = std::fs::canonicalize(node) {
        if let Some(r) = real.to_str() {
            names.push(r.to_string());
        }
    }
    let twins: Vec<String> = names.iter().filter_map(|n| disk_node_twin(n)).collect();
    names.extend(twins);
    disks.iter().find(|d| names.iter().any(|n| *n == d.path)).map(|d| d.index)
}

/// A whole-disk node, or a whole-disk node followed by `:P`, as `disk:N` / `disk:N:P`.
/// `None` when `s` names no whole disk of `disks` (a partition node stays a volume).
pub fn disk_spec_of_node(disks: &[DiskInfo], s: &str) -> Option<DeviceArg> {
    if let Some(i) = disk_of_node(disks, s) {
        return Some(DeviceArg::Disk(i));
    }
    let (base, num) = s.rsplit_once(':')?;
    let n = num.parse::<u32>().ok().filter(|n| *n >= 1)?;
    disk_of_node(disks, base).map(|i| DeviceArg::Partition(Box::new(DeviceArg::Disk(i)), n))
}

/// Whole-disk nodes of the running system (path, index), registered by `main` before the
/// command line is parsed so that arguments parsed as a disk index (`pm op ... <disk>`) accept
/// `/dev/sdb` too. Empty when nothing was registered (no `/dev/...` argument, `--disk-file`).
static DISK_NODES: OnceLock<Vec<DiskInfo>> = OnceLock::new();

/// Make `disks`' nodes known to [`registered_disk_of_node`] (first call wins).
pub fn register_disk_nodes(disks: Vec<DiskInfo>) {
    let _ = DISK_NODES.set(disks);
}

/// [`disk_of_node`] over the disks registered with [`register_disk_nodes`].
pub fn registered_disk_of_node(node: &str) -> Option<u32> {
    disk_of_node(DISK_NODES.get().map_or(&[], |d| d.as_slice()), node)
}

/// `a` with a whole-disk `/dev` node (optionally `:P`) turned into `disk:N[:P]`, so the
/// same-device and overlap checks compare it with `disk:N` and its partitions. Volumes that are
/// not whole disks, and every non-`/dev` argument, are returned unchanged (no enumeration).
pub fn resolve_disk_node(platform: &dyn Platform, a: DeviceArg) -> Result<DeviceArg> {
    match &a {
        DeviceArg::Volume(v) if is_dev_node(v) => {
            let disks = platform.enumerate_disks()?;
            Ok(disk_spec_of_node(&disks, v).unwrap_or(a))
        }
        _ => Ok(a),
    }
}

/// [`parse_device`] followed by [`resolve_disk_node`].
pub fn parse_device_on(platform: &dyn Platform, s: &str) -> Result<DeviceArg> {
    resolve_disk_node(platform, parse_device(s)?)
}

/// The volume path with exactly one trailing backslash, as the platform expects it.
fn normalize_volume_path(s: &str) -> String {
    let mut v = s.to_string();
    if !v.ends_with('\\') {
        v.push('\\');
    }
    v
}

/// `"N"` as a 1-based partition number.
fn parse_partition(n: &str, whole: &str) -> Result<u32> {
    match n.parse::<u32>() {
        Ok(0) => Err(Error::Invalid(format!("'{whole}': partition numbers start at 1"))),
        Ok(n) => Ok(n),
        Err(_) => Err(Error::Invalid(format!("'{whole}': '{n}' is not a partition number"))),
    }
}

/// `PATH` or `PATH:P` where `PATH` is an existing file: the whole file, or its partition `P`.
/// `None` when `s` is an existing file in neither form.
fn file_or_partition(s: &str) -> Option<DeviceArg> {
    if Path::new(s).is_file() {
        return Some(DeviceArg::File(PathBuf::from(s)));
    }
    let (base, num) = s.rsplit_once(':')?;
    if base.is_empty() || num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Path::new(base).is_file().then(|| {
        // a number too large for u32 can never name an existing partition (0 is rejected on open)
        DeviceArg::Partition(Box::new(DeviceArg::File(PathBuf::from(base))), num.parse().unwrap_or(0))
    })
}

/// Parse `"disk:1"`, `"disk:1:2"`, `"D:"`, `"\\?\Volume{..}\"`, `"file:PATH"`, `"file:PATH:2"` or
/// a path of an existing file (optionally followed by `:P`).
pub fn parse_device(s: &str) -> Result<DeviceArg> {
    if let Some(rest) = strip_prefix_ci(s, "disk:") {
        let bad = || Error::Invalid(format!("'{s}': expected disk:<index> or disk:<index>:<partition>, e.g. disk:1"));
        let (n, part) = match rest.split_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (rest, None),
        };
        let disk = DeviceArg::Disk(n.parse::<u32>().map_err(|_| bad())?);
        return match part {
            None => Ok(disk),
            Some(p) => Ok(DeviceArg::Partition(Box::new(disk), parse_partition(p, s)?)),
        };
    }
    if is_volume_path(s) {
        return Ok(DeviceArg::Volume(normalize_volume_path(s)));
    }
    if let Some(path) = strip_prefix_ci(s, "file:") {
        if path.is_empty() {
            return Err(Error::Invalid("file: needs a path".into()));
        }
        return file_or_partition(path)
            .ok_or_else(|| Error::NotFound(format!("image file '{path}' does not exist")));
    }
    if let Some(c) = drive_letter(s) {
        return Ok(DeviceArg::Volume(format!("{c}:")));
    }
    if let Some(a) = file_or_partition(s) {
        return Ok(a);
    }
    if is_dev_node(s) {
        return Ok(DeviceArg::Volume(s.to_string()));
    }
    Err(Error::Invalid(format!(
        "'{s}' is not a device: use disk:N[:P], a /dev/... node (Linux, macOS; a whole disk also as /dev/NODE:P), a drive letter (D:), a \\\\?\\Volume{{..}}\\ path, or an existing image file[:P]"
    )))
}

/// Resolve `D:` (via the volumes' mount points), a `\\?\Volume{..}\` path or a `/dev/...` node
/// (in the platform's canonical form) to a [`VolumeId`].
pub fn resolve_volume(platform: &dyn Platform, s: &str) -> Result<VolumeId> {
    if is_volume_path(s) {
        return Ok(VolumeId(normalize_volume_path(s)));
    }
    if is_dev_node(s) {
        // `/dev/rdisk2s1` → `/dev/disk2s1` on macOS, as the platform lists it
        return Ok(platform.canonical_volume_id(&VolumeId(s.to_string())));
    }
    let letter = drive_letter(s)
        .ok_or_else(|| Error::Invalid(format!("'{s}' is not a drive letter or volume path")))?;
    platform
        .enumerate_volumes()?
        .into_iter()
        .find(|v| v.mount_points.iter().any(|m| drive_letter(m) == Some(letter)))
        .map(|v| v.id)
        .ok_or_else(|| Error::NotFound(format!("no volume is mounted at {letter}:")))
}

/// Open the device described by `a`.
///
/// A device that does not exist is reported as `Error::NotFound` (an argument error for the CLI),
/// whatever the OS says when opening it; genuine failures (access denied, I/O) pass through.
pub fn open_device(p: &dyn Platform, a: &DeviceArg, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
    open_device_with_slice(p, a, mode).map(|o| o.dev)
}

/// Like [`open_device`], also reporting which disk / volume / file the device is stored on and
/// the byte range it covers (see [`Opened`]). Partitions are bounds-checked slices of their
/// parent, located through `pm_parttable::read_table`.
pub fn open_device_with_slice(p: &dyn Platform, a: &DeviceArg, mode: OpenMode) -> Result<Opened> {
    let whole = |dev: Box<dyn BlockDevice>, identity: Identity, disk_extent: Option<(u32, u64, u64)>| Opened {
        range: (0, dev.len()),
        dev,
        identity,
        disk_extent,
        files: Vec::new(),
    };
    match a {
        DeviceArg::Disk(i) => {
            let dev = p.open_disk(*i, mode).map_err(|e| {
                let missing = p.enumerate_disks().map_or(false, |d| !d.iter().any(|x| x.index == *i));
                if missing {
                    Error::NotFound(format!("no disk with index {i}"))
                } else {
                    e
                }
            })?;
            let len = dev.len();
            Ok(whole(dev, Identity::Disk(*i), Some((*i, 0, len))))
        }
        DeviceArg::Volume(v) => {
            let id = resolve_volume(p, v)?;
            let dev = p.open_volume(&id, mode).map_err(|e| {
                let missing = p.enumerate_volumes().map_or(false, |vs| !vs.iter().any(|x| x.id == id));
                if missing {
                    Error::NotFound(format!("no volume {}", id.0))
                } else {
                    e
                }
            })?;
            let extent = volume_extent(p, &id);
            Ok(whole(dev, Identity::Volume(id.0), extent))
        }
        // Any image format pm-image understands (raw, VHD, VMDK, qcow2), detected by content.
        DeviceArg::File(path) => {
            let dev = pm_image::open(path, mode == OpenMode::ReadOnly)?;
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let mut opened = whole(dev, Identity::File(canonical), None);
            opened.files = vmdk_extent_files(path);
            Ok(opened)
        }
        DeviceArg::Partition(parent, number) => {
            let mut parent = open_device_with_slice(p, parent, mode)?;
            let table = read_table(&mut *parent.dev)?;
            let entry = table.entries.iter().find(|e| e.number == *number).ok_or_else(|| {
                if table.entries.is_empty() {
                    Error::NotFound(format!("partition {number}: the device has no partition table"))
                } else {
                    let have: Vec<String> = table.entries.iter().map(|e| e.number.to_string()).collect();
                    Error::NotFound(format!("no partition {number} (the table has {})", have.join(", ")))
                }
            })?;
            let (start, len) = (entry.start, entry.len);
            let slice = SliceDevice::new(parent.dev, start, len)?;
            Ok(Opened {
                dev: Box::new(slice),
                identity: parent.identity,
                range: (parent.range.0 + start, parent.range.0 + start + len),
                disk_extent: parent.disk_extent.map(|(d, s, _)| (d, s + start, s + start + len)),
                files: parent.files,
            })
        }
    }
}

/// Parse and open a `<device>` command line argument. A device that does not exist is an
/// argument error (exit 1); anything else that goes wrong while opening it is a runtime error.
pub fn open_spec(platform: &dyn Platform, spec: &str, mode: OpenMode) -> std::result::Result<Opened, CliError> {
    let arg = parse_device_on(platform, spec).map_err(arg_error)?;
    open_device_with_slice(platform, &arg, mode).map_err(|e| match e {
        Error::NotFound(m) => CliError::Usage(m),
        other => CliError::Runtime(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{FileDevice, MemDevice, VolumeInfo};
    use pm_platform::MockPlatform;

    #[test]
    fn cli_parse_device_variants() {
        assert_eq!(parse_device("disk:1").unwrap(), DeviceArg::Disk(1));
        assert_eq!(parse_device("DISK:12").unwrap(), DeviceArg::Disk(12));
        assert!(parse_device("disk:x").is_err());
        assert!(parse_device("disk:").is_err());
        assert!(parse_device("disk:-1").is_err());

        assert_eq!(parse_device("D:").unwrap(), DeviceArg::Volume("D:".into()));
        assert_eq!(parse_device("d:").unwrap(), DeviceArg::Volume("D:".into()));
        assert_eq!(parse_device(r"E:\").unwrap(), DeviceArg::Volume("E:".into()));

        let v = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\";
        assert_eq!(parse_device(v).unwrap(), DeviceArg::Volume(v.into()));
        // trailing backslash is normalised
        assert_eq!(parse_device(v.trim_end_matches('\\')).unwrap(), DeviceArg::Volume(v.into()));

        assert!(matches!(parse_device("file:rel/does-not-exist.img"), Err(Error::NotFound(_))));
        assert!(matches!(parse_device(r"file:C:\no\such.img"), Err(Error::NotFound(_))));
        assert!(parse_device("file:").is_err());

        // a bare path is a File only if it exists
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.raw");
        std::fs::write(&img, b"x").unwrap();
        let s = img.to_str().unwrap();
        assert_eq!(parse_device(s).unwrap(), DeviceArg::File(img.clone()));
        assert!(matches!(
            parse_device(dir.path().join("missing.raw").to_str().unwrap()),
            Err(Error::Invalid(_))
        ));
        assert_eq!(parse_device(&format!("file:{s}")).unwrap(), DeviceArg::File(img.clone()));
        assert!(parse_device("").is_err());
        assert!(parse_device("C").is_err());
    }

    #[test]
    fn dev_nodes_parse_as_volumes() {
        assert_eq!(parse_device("/dev/sda1").unwrap(), DeviceArg::Volume("/dev/sda1".into()));
        assert_eq!(parse_device("/dev/rdisk3s2").unwrap(), DeviceArg::Volume("/dev/rdisk3s2".into()));
        assert!(parse_device("/dev/").is_err());
        assert!(parse_device("/devices/sda").is_err());
        let p = MockPlatform::new();
        assert_eq!(resolve_volume(&p, "/dev/nvme0n1p2").unwrap(), VolumeId("/dev/nvme0n1p2".into()));
    }

    #[test]
    fn cli_parse_partition_addressing() {
        assert_eq!(
            parse_device("disk:2:3").unwrap(),
            DeviceArg::Partition(Box::new(DeviceArg::Disk(2)), 3)
        );
        assert!(matches!(parse_device("disk:2:0"), Err(Error::Invalid(_))));
        assert!(parse_device("disk:2:").is_err());
        assert!(parse_device("disk:2:x").is_err());
        assert!(parse_device("disk:2:1:1").is_err());

        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.raw");
        std::fs::write(&img, b"x").unwrap();
        let s = img.to_str().unwrap();
        let part = DeviceArg::Partition(Box::new(DeviceArg::File(img.clone())), 2);
        assert_eq!(parse_device(&format!("file:{s}:2")).unwrap(), part);
        assert_eq!(parse_device(&format!("{s}:2")).unwrap(), part);
        // the base file must exist
        let missing = dir.path().join("missing.raw");
        let m = missing.to_str().unwrap();
        assert!(matches!(parse_device(&format!("file:{m}:2")), Err(Error::NotFound(_))));
        assert!(matches!(parse_device(&format!("{m}:2")), Err(Error::Invalid(_))));
        // a non-numeric suffix is not a partition
        assert!(parse_device(&format!("file:{s}:x")).is_err());
    }

    #[test]
    fn overlap_rules() {
        let mk = |id: Identity, range| Opened {
            dev: Box::new(MemDevice::new(0)),
            identity: id,
            range,
            disk_extent: None,
            files: Vec::new(),
        };
        let f = |s: &str| Identity::File(PathBuf::from(s));
        let whole = mk(f("a"), (0, 100));
        let other = mk(f("b"), (0, 100));
        assert!(!whole.overlaps(0, 100, &other));
        assert!(whole.overlaps(0, 100, &mk(f("a"), (0, 100))));
        // partitions of one disk: [10, 20) and [20, 30)
        let p1 = mk(Identity::Disk(0), (10, 20));
        let p2 = mk(Identity::Disk(0), (20, 30));
        assert!(!p1.overlaps(0, 10, &p2));
        assert!(p1.overlaps(0, 11, &p2));
        assert!(p1.overlaps(5, 10, &p2));
        assert!(!p2.overlaps(0, 10, &p1));
        assert!(!p1.overlaps(u64::MAX, 10, &p2));
    }

    #[test]
    fn open_device_variants() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.raw");
        FileDevice::create(&img, 4096).unwrap();
        let plat = MockPlatform::new();

        let d = open_device(&plat, &DeviceArg::File(img.clone()), OpenMode::ReadOnly).unwrap();
        assert!(d.is_read_only());
        assert_eq!(d.len(), 4096);
        let d = open_device(&plat, &DeviceArg::File(img), OpenMode::ReadWrite).unwrap();
        assert!(!d.is_read_only());

        plat.add_disk(0, MemDevice::new(8192), Default::default());
        let d = open_device(&plat, &DeviceArg::Disk(0), OpenMode::ReadOnly).unwrap();
        assert_eq!(d.len(), 8192);
        assert!(matches!(
            open_device(&plat, &DeviceArg::Disk(7), OpenMode::ReadOnly),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            open_device(&plat, &DeviceArg::Volume("E:".into()), OpenMode::ReadOnly),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            open_device(&plat, &DeviceArg::Volume(r"\\?\Volume{zzzz}\".into()), OpenMode::ReadOnly),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn resolve_letter_through_volumes() {
        let plat = MockPlatform::new();
        plat.add_disk(0, MemDevice::new(1 << 20), Default::default());
        plat.add_volume(
            VolumeInfo {
                id: VolumeId(r"\\?\Volume{aaaa}\".into()),
                mount_points: vec![r"D:\".into()],
                ..Default::default()
            },
            0,
            0,
            4096,
        );
        assert_eq!(resolve_volume(&plat, "d:").unwrap(), VolumeId(r"\\?\Volume{aaaa}\".into()));
        assert!(matches!(resolve_volume(&plat, "E:"), Err(Error::NotFound(_))));
        let raw = r"\\?\Volume{bbbb}\";
        assert_eq!(resolve_volume(&plat, raw).unwrap(), VolumeId(raw.into()));
        // a missing trailing backslash is added here too (mount / unmount call this directly)
        assert_eq!(
            resolve_volume(&plat, raw.trim_end_matches('\\')).unwrap(),
            VolumeId(raw.into())
        );
        assert!(resolve_volume(&plat, "nonsense").is_err());
    }

    fn disk_at(index: u32, path: &str) -> DiskInfo {
        DiskInfo { index, path: path.into(), ..Default::default() }
    }

    #[test]
    fn whole_disk_nodes_map_to_disk_indexes() {
        // indexes need not be contiguous (Linux keeps a removed disk's index)
        let disks = [disk_at(0, "/dev/sda"), disk_at(2, "/dev/sdb"), disk_at(5, "/dev/rdisk2")];
        assert_eq!(disk_of_node(&disks, "/dev/sdb"), Some(2));
        assert_eq!(disk_of_node(&disks, "/dev/sda"), Some(0));
        // macOS: the buffered twin of a raw whole disk, and the raw node itself
        assert_eq!(disk_of_node(&disks, "/dev/disk2"), Some(5));
        assert_eq!(disk_of_node(&disks, "/dev/rdisk2"), Some(5));
        // partitions / slices are volumes, not disks
        assert_eq!(disk_of_node(&disks, "/dev/sdb1"), None);
        assert_eq!(disk_of_node(&disks, "/dev/disk2s1"), None);
        assert_eq!(disk_of_node(&disks, "/dev/disk22"), None);
        assert_eq!(disk_of_node(&disks, "/dev/disk/by-id/none"), None);
        assert_eq!(disk_of_node(&disks, "sdb"), None);
        // NODE:P addresses a partition of the disk
        assert_eq!(disk_spec_of_node(&disks, "/dev/sdb"), Some(DeviceArg::Disk(2)));
        assert_eq!(
            disk_spec_of_node(&disks, "/dev/sdb:3"),
            Some(DeviceArg::Partition(Box::new(DeviceArg::Disk(2)), 3))
        );
        assert_eq!(disk_spec_of_node(&disks, "/dev/sdb:0"), None);
        assert_eq!(disk_spec_of_node(&disks, "/dev/sdc:1"), None);
        disk_twin_ok();
    }

    fn disk_twin_ok() {
        assert_eq!(disk_node_twin("/dev/rdisk3s2").as_deref(), Some("/dev/disk3s2"));
        assert_eq!(disk_node_twin("/dev/disk3").as_deref(), Some("/dev/rdisk3"));
        assert_eq!(disk_node_twin("/dev/disk/by-id/x"), None);
        assert_eq!(disk_node_twin("/dev/sda"), None);
    }

    #[test]
    fn dev_node_arguments_resolve_to_disks_on_the_platform() {
        let plat = MockPlatform::new();
        for (i, path) in [(0, "/dev/sda"), (2, "/dev/sdb"), (5, "/dev/nvme0n1")] {
            plat.add_disk(i, MemDevice::new(1 << 20), DiskInfo { path: path.into(), ..Default::default() });
        }
        assert_eq!(parse_device_on(&plat, "/dev/nvme0n1").unwrap(), DeviceArg::Disk(5));
        assert_eq!(
            parse_device_on(&plat, "/dev/sdb:1").unwrap(),
            DeviceArg::Partition(Box::new(DeviceArg::Disk(2)), 1)
        );
        // a partition node stays a volume; other syntaxes are untouched
        assert_eq!(parse_device_on(&plat, "/dev/sdb1").unwrap(), DeviceArg::Volume("/dev/sdb1".into()));
        assert_eq!(parse_device_on(&plat, "disk:2").unwrap(), DeviceArg::Disk(2));
        // opened through the node, the device is the disk: the overlap checks see disk:2
        let by_node = open_spec(&plat, "/dev/sdb", OpenMode::ReadOnly).unwrap();
        let by_index = open_spec(&plat, "disk:2", OpenMode::ReadOnly).unwrap();
        assert_eq!(by_node.identity, Identity::Disk(2));
        assert!(by_node.overlaps(0, 512, &by_index));
        // registered nodes (main) serve arguments parsed as a disk index
        register_disk_nodes(plat.enumerate_disks().unwrap());
        assert_eq!(registered_disk_of_node("/dev/sdb"), Some(2));
        assert_eq!(registered_disk_of_node("/dev/sdz"), None);
    }

    /// A platform whose canonical volume ids drop the `r` of macOS raw nodes.
    struct RawToBuffered(MockPlatform);

    impl Platform for RawToBuffered {
        fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> { self.0.enumerate_disks() }
        fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>> { self.0.enumerate_volumes() }
        fn open_disk(&self, i: u32, m: OpenMode) -> Result<Box<dyn BlockDevice>> { self.0.open_disk(i, m) }
        fn open_volume(&self, id: &VolumeId, m: OpenMode) -> Result<Box<dyn BlockDevice>> { self.0.open_volume(id, m) }
        fn begin_write(&self, d: u32) -> Result<Box<dyn pm_platform::WriteSession>> { self.0.begin_write(d) }
        fn mount(&self, id: &VolumeId, at: &pm_platform::MountPoint) -> Result<()> { self.0.mount(id, at) }
        fn unmount(&self, id: &VolumeId, at: &pm_platform::MountPoint) -> Result<()> { self.0.unmount(id, at) }
        fn set_offline(&self, d: u32, o: bool) -> Result<()> { self.0.set_offline(d, o) }
        fn rescan(&self, d: u32) -> Result<()> { self.0.rescan(d) }
        fn format(&self, id: &VolumeId, fs: pm_core::FsKind, l: &str, c: Option<u32>, q: bool, p: &dyn pm_core::ProgressSink) -> Result<()> {
            self.0.format(id, fs, l, c, q, p)
        }
        fn shrink_volume(&self, id: &VolumeId, n: u64) -> Result<()> { self.0.shrink_volume(id, n) }
        fn extend_volume(&self, id: &VolumeId, n: u64) -> Result<()> { self.0.extend_volume(id, n) }
        fn smart(&self, d: u32) -> Result<Option<pm_platform::SmartData>> { self.0.smart(d) }
        fn is_elevated(&self) -> bool { true }
        fn system_disk(&self) -> Option<u32> { None }
        fn free_letters(&self) -> Vec<char> { Vec::new() }
        fn run_tool(&self, t: pm_platform::ExternalTool, a: &[String]) -> Result<String> { self.0.run_tool(t, a) }
        fn canonical_volume_id(&self, id: &VolumeId) -> VolumeId {
            VolumeId(id.0.replacen("/dev/rdisk", "/dev/disk", 1))
        }
    }

    #[test]
    fn typed_volume_ids_are_canonicalised_by_the_platform() {
        let mock = MockPlatform::new();
        mock.add_disk(2, MemDevice::new(1 << 20), DiskInfo { path: "/dev/rdisk2".into(), ..Default::default() });
        mock.add_volume(VolumeInfo { id: VolumeId("/dev/disk2s1".into()), ..Default::default() }, 2, 4096, 8192);
        let plat = RawToBuffered(mock);
        assert_eq!(resolve_volume(&plat, "/dev/rdisk2s1").unwrap(), VolumeId("/dev/disk2s1".into()));
        let a = open_spec(&plat, "/dev/rdisk2s1", OpenMode::ReadOnly).unwrap();
        let b = open_spec(&plat, "/dev/disk2s1", OpenMode::ReadOnly).unwrap();
        assert_eq!(a.identity, b.identity);
        assert_eq!(a.disk_extent, Some((2, 4096, 4096 + 8192)));
        // the whole disk under either name is disk:2, overlapping its slice
        let disk = open_spec(&plat, "/dev/disk2", OpenMode::ReadOnly).unwrap();
        assert_eq!(disk.identity, Identity::Disk(2));
        assert!(disk.overlaps(0, 1 << 20, &a));
    }
}
