//! Where a file path lies relative to a disk or partition being read or written, so that an
//! image, a mapfile or recovered files are never written onto the device they come from.
//!
//! The answer comes from [`Platform::disk_of_path`] (Windows: the volume mount point and its
//! extents; Linux: the device of the filesystem; macOS: `diskutil info`), so it covers drive
//! letters, folder mount points and Linux/macOS mount points alike. When the platform cannot
//! tell (`None`: a network share, a RAM disk, an LVM / device-mapper volume, an image
//! platform), the path is treated as possibly on the device whenever the device has a
//! mounted filesystem it could be on.

use pm_core::{gpt_types, DiskInfo, PartitionInfo, PartitionType, VolumeInfo};
use pm_platform::Platform;
use std::path::{Component, Path, PathBuf, Prefix};

/// A disk, or one of its partitions (`Some(number)`).
pub type DevicePlace = (u32, Option<u32>);

/// What [`locate`] reports for a path on a network share (a Windows UNC path or mapped
/// network drive): a "disk" no platform lists, so it is never the device being read.
pub const NETWORK: DevicePlace = (u32::MAX, None);

/// GPT type of a Linux RAID (md) member.
const LINUX_RAID: uuid::Uuid = uuid::uuid!("A19D880F-05FC-4D3B-A006-743F0F84911E");

/// A partition that belongs to a stacked volume (an LVM physical volume or an md RAID member,
/// by its type): it has no mount point of its own, but filesystems on the volume above it
/// live on it.
fn is_stack_member(p: &PartitionInfo) -> bool {
    match p.ptype {
        Some(PartitionType::Gpt(g)) => g == gpt_types::LINUX_LVM || g == LINUX_RAID,
        Some(PartitionType::Mbr(b)) => b == 0x8E || b == 0xFD,
        None => false,
    }
}

/// Where a path lies relative to a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathPlace {
    /// On another disk, another partition of the same disk, or a network share; or the
    /// device has no mounted filesystem, so no path can be on it.
    Elsewhere,
    /// On the device: the same disk and, when both are known, the same partition.
    OnDevice,
    /// The platform cannot tell which disk the path is on, and the device has a mounted
    /// filesystem the path could be on.
    Unknown,
}

/// Which disks are listed and which of their partitions carry a mounted filesystem, from the
/// disks and volumes the platform enumerated (or a [`crate::Model`]'s). An LVM physical volume
/// or md RAID member counts as mounted: the filesystems on the volume above it have no mount
/// point on the partition itself. Small and cheap to keep in a wizard's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountMap {
    listed: Vec<u32>,
    /// `(disk, Some(partition))` for a mounted partition; `(disk, None)` for a mounted volume
    /// the platform places on the disk but on none of its listed partitions.
    mounted: Vec<DevicePlace>,
}

impl MountMap {
    pub fn new(disks: &[DiskInfo], volumes: &[VolumeInfo]) -> Self {
        let mut mounted = Vec::new();
        for d in disks {
            for p in d.partitions.iter().filter(|p| !p.mount_points.is_empty() || is_stack_member(p)) {
                mounted.push((d.index, Some(p.number)));
            }
        }
        for v in volumes.iter().filter(|v| !v.mount_points.is_empty()) {
            let Some(di) = v.disk else { continue };
            let part = disks
                .iter()
                .find(|d| d.index == di)
                .and_then(|d| d.partitions.iter().find(|p| p.start == v.start))
                .map(|p| p.number);
            mounted.push((di, part));
        }
        mounted.sort_unstable();
        mounted.dedup();
        MountMap { listed: disks.iter().map(|d| d.index).collect(), mounted }
    }

    /// Whether `device` has a mounted filesystem; `None` when its disk is not listed.
    pub fn has_mounts(&self, device: DevicePlace) -> Option<bool> {
        if !self.listed.contains(&device.0) {
            return None;
        }
        Some(self.mounted.iter().any(|(d, p)| {
            *d == device.0 && match (device.1, p) {
                (Some(a), Some(b)) => a == *b,
                _ => true,
            }
        }))
    }
}

/// `path` made absolute against the current directory (it need not exist).
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `(disk, partition)` of `path` according to `platform` (the path is made absolute first);
/// [`NETWORK`] when the platform cannot place it and its canonical form is a network share
/// (a mapped drive `Z:` resolves to `\\?\UNC\server\share`).
pub fn locate(platform: &dyn Platform, path: &Path) -> Option<(u32, Option<u32>)> {
    locate_with(platform, path, pm_platform::canonical_existing)
}

/// [`locate`] with `canonical` resolving the path (tests pass their own).
fn locate_with(
    platform: &dyn Platform,
    path: &Path,
    canonical: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<(u32, Option<u32>)> {
    let abs = absolute(path);
    platform.disk_of_path(&abs).or_else(|| {
        let network = is_network_path(&abs) || canonical(&abs).is_some_and(|c| is_network_path(&c));
        network.then_some(NETWORK)
    })
}

/// A Windows UNC path (`\\server\share\...`): a network share is never on a local disk.
fn is_network_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    )
}

/// Where `path` lies relative to `device`, given `located`, what
/// [`Platform::disk_of_path`] reported for it (see [`locate`]), and `mounts` (a disk missing
/// from it is treated as possibly holding the path).
pub fn place_of(path: &Path, located: Option<(u32, Option<u32>)>, device: DevicePlace, mounts: &MountMap) -> PathPlace {
    match located {
        Some((d, _)) if d != device.0 => PathPlace::Elsewhere,
        Some((_, p)) => match (device.1, p) {
            (Some(a), Some(b)) if a != b => PathPlace::Elsewhere,
            _ => PathPlace::OnDevice,
        },
        None if is_network_path(path) => PathPlace::Elsewhere,
        None => match mounts.has_mounts(device) {
            Some(false) => PathPlace::Elsewhere,
            Some(true) | None => PathPlace::Unknown,
        },
    }
}

/// Why the file or folder `path` (described as `what`, e.g. "image file") may not be written
/// because of `place` relative to the device described as `device` (e.g. "Disk 1 (the
/// source)"), if it may not.
pub fn place_problem(place: PathPlace, what: &str, path: &Path, device: &str) -> Option<String> {
    match place {
        PathPlace::Elsewhere => None,
        PathPlace::OnDevice => Some(format!(
            "The {what} '{}' is on {device}; choose a location on another disk.",
            path.display()
        )),
        PathPlace::Unknown => Some(format!(
            "Cannot tell which disk the {what} '{}' is on (a network share, RAM disk or LVM / \
             device-mapper volume?), and {device} has mounted volumes it could be on; choose a \
             location on another disk of the disk list.",
            path.display()
        )),
    }
}

/// [`place_problem`] of `path` against `device`, asking `platform` where `path` is.
pub fn path_problem_on(
    platform: &dyn Platform,
    mounts: &MountMap,
    path: &Path,
    device: DevicePlace,
    what: &str,
    device_desc: &str,
) -> Option<String> {
    let place = place_of(path, locate(platform, path), device, mounts);
    place_problem(place, what, path, device_desc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::PartitionInfo;

    fn disk(index: u32, mounts: &[(u32, &str)]) -> DiskInfo {
        DiskInfo {
            index,
            partitions: mounts
                .iter()
                .map(|(n, m)| PartitionInfo {
                    number: *n,
                    start: u64::from(*n) << 20,
                    mount_points: if m.is_empty() { Vec::new() } else { vec![m.to_string()] },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn known_places() {
        let p = Path::new("/mnt/x/file.img");
        let m = MountMap::new(&[disk(0, &[(1, "/")]), disk(2, &[(1, "/mnt/x"), (2, "")])], &[]);
        assert_eq!(place_of(p, Some((2, Some(1))), (0, None), &m), PathPlace::Elsewhere);
        assert_eq!(place_of(p, Some((2, Some(1))), (2, None), &m), PathPlace::OnDevice);
        assert_eq!(place_of(p, Some((2, Some(1))), (2, Some(1)), &m), PathPlace::OnDevice);
        assert_eq!(place_of(p, Some((2, Some(1))), (2, Some(2)), &m), PathPlace::Elsewhere);
        // partition unknown on one side: same disk is enough
        assert_eq!(place_of(p, Some((2, None)), (2, Some(2)), &m), PathPlace::OnDevice);
    }

    #[test]
    fn unknown_places_fail_closed_only_when_the_device_is_mounted() {
        let p = Path::new("/srv/nas/file.img");
        let disks = [disk(0, &[(1, "/"), (2, "")]), disk(1, &[(1, "")]), disk(5, &[]), disk(6, &[(1, "")])];
        let vols = [
            VolumeInfo { disk: Some(5), mount_points: vec!["/v".into()], ..Default::default() },
            VolumeInfo { disk: Some(6), start: 1 << 20, mount_points: vec!["/w".into()], ..Default::default() },
        ];
        let m = MountMap::new(&disks, &[]);
        assert_eq!(place_of(p, None, (0, None), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (0, Some(1)), &m), PathPlace::Unknown);
        // nothing of the device is mounted: no path can be on it
        assert_eq!(place_of(p, None, (0, Some(2)), &m), PathPlace::Elsewhere);
        assert_eq!(place_of(p, None, (1, None), &m), PathPlace::Elsewhere);
        assert_eq!(place_of(p, None, (5, None), &m), PathPlace::Elsewhere);
        // a disk the list does not have
        assert_eq!(place_of(p, None, (9, None), &m), PathPlace::Unknown);
        // mounted volumes the platform lists on a disk: on no partition (anywhere on disk 5),
        // or on partition 1 of disk 6 (matched by start)
        let m = MountMap::new(&disks, &vols);
        assert_eq!(place_of(p, None, (5, None), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (5, Some(3)), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (6, Some(1)), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (6, Some(2)), &m), PathPlace::Elsewhere);
        if cfg!(windows) {
            let unc = Path::new(r"\\server\share\x.img");
            assert_eq!(place_of(unc, None, (0, None), &m), PathPlace::Elsewhere);
        }
    }

    #[test]
    fn stack_members_count_as_mounted() {
        let member = |n: u32, t: PartitionType| PartitionInfo { number: n, start: u64::from(n) << 20, ptype: Some(t), ..Default::default() };
        let disks = [
            DiskInfo { index: 0, partitions: vec![member(1, PartitionType::Gpt(gpt_types::LINUX_LVM)), member(2, PartitionType::Gpt(gpt_types::BASIC_DATA))], ..Default::default() },
            DiskInfo { index: 1, partitions: vec![member(1, PartitionType::Mbr(0xFD)), member(2, PartitionType::Mbr(0x8E))], ..Default::default() },
            DiskInfo { index: 2, partitions: vec![member(1, PartitionType::Gpt(LINUX_RAID))], ..Default::default() },
        ];
        let m = MountMap::new(&disks, &[]);
        let p = Path::new("/home/rec");
        // /home on an LV the platform cannot place: the PV's disk could hold it
        assert_eq!(place_of(p, None, (0, None), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (0, Some(1)), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (0, Some(2)), &m), PathPlace::Elsewhere);
        assert_eq!(place_of(p, None, (1, Some(1)), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (1, Some(2)), &m), PathPlace::Unknown);
        assert_eq!(place_of(p, None, (2, None), &m), PathPlace::Unknown);
    }

    #[test]
    fn network_shares_are_never_the_device() {
        let plat = pm_platform::MockPlatform::new();
        let disks = [disk(0, &[(1, "C:\\")])];
        let m = MountMap::new(&disks, &[]);
        let local = std::env::temp_dir().join("pm-loc-net").join("x.img");
        // a mapped drive: not placed by the platform, canonical form \\?\UNC\server\share
        let unc = |_: &Path| Some(PathBuf::from(r"\\?\UNC\server\share\x.img"));
        let located = locate_with(&plat, &local, unc);
        if cfg!(windows) {
            assert_eq!(located, Some(NETWORK));
            assert_eq!(place_of(&local, located, (0, None), &m), PathPlace::Elsewhere);
            // a verbatim UNC path typed as such
            let typed = Path::new(r"\\?\UNC\server\share\x.img");
            assert_eq!(place_of(typed, None, (0, None), &m), PathPlace::Elsewhere);
        } else {
            assert_eq!(located, None, "no UNC prefixes off Windows");
        }
        // a local path that resolves locally stays unknown
        assert_eq!(locate_with(&plat, &local, |p: &Path| Some(p.to_path_buf())), None);
        assert_eq!(place_of(&local, None, (0, None), &m), PathPlace::Unknown);
    }

    #[test]
    fn problems_name_the_path_and_device() {
        let p = Path::new("/a/b.img");
        assert_eq!(place_problem(PathPlace::Elsewhere, "image file", p, "Disk 1"), None);
        let on = place_problem(PathPlace::OnDevice, "image file", p, "Disk 1 (the source)").unwrap();
        assert!(on.contains("/a/b.img") && on.contains("Disk 1 (the source)") && on.contains("another disk"), "{on}");
        let unk = place_problem(PathPlace::Unknown, "mapfile", p, "Disk 1").unwrap();
        assert!(unk.starts_with("Cannot tell which disk the mapfile"), "{unk}");
    }

    #[test]
    fn located_through_the_platform() {
        let plat = pm_platform::MockPlatform::new();
        let base = std::env::temp_dir().join("pm-location-test");
        plat.set_disk_of_path(&base, 3, Some(2));
        let m = MountMap::new(&[disk(3, &[(2, "x")])], &[]);
        let f = base.join("sub").join("f.img");
        assert_eq!(locate(&plat, &f), Some((3, Some(2))));
        assert!(path_problem_on(&plat, &m, &f, (3, None), "image file", "Disk 3").is_some());
        assert!(path_problem_on(&plat, &m, &f, (3, Some(1)), "image file", "Disk 3").is_none());
        assert!(path_problem_on(&plat, &m, &f, (4, None), "image file", "Disk 4").is_none());
    }
}
