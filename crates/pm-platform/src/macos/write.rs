//! Taking a disk for writing (OS-neutral, the device is opened through a closure so the
//! fixtures can drive every path).
//!
//! 1. refuse unless running as root (opening `/dev/rdiskN` read-write would fail anyway, but
//!    only after everything was unmounted);
//! 2. refuse anything but a physical whole disk (a synthesized APFS container written as a
//!    disk is destroyed);
//! 3. record which slices are mounted (the disk's own slices and the volumes of APFS
//!    containers stored on it);
//! 4. `diskutil unmountDisk diskN`. It is not atomic: on failure the recorded slices that are
//!    no longer mounted are mounted again;
//! 5. open the device; on failure every recorded slice is mounted again.
//!
//! Failures after step 3 are reported like on Linux: `Error::Busy` saying whether the
//! filesystems were mounted again or naming those that could not be.

use super::diskutil::{parse_apfs_containers, parse_identifier, parse_list, DiskutilRunner};
use super::guard::physical_whole_disk;
use pm_core::{Error, Result};

/// `EACCES`: the CLI recognises "access denied" and adds the sudo hint.
const EACCES: u32 = 13;

/// Mounted slices of `diskN` in a `diskutil list -plist` output: its own mounted slices, then
/// the mounted volumes of APFS containers whose physical store is one of its slices.
pub fn mounted_slices(list_xml: &[u8], disk: u32) -> Result<Vec<String>> {
    let on_disk = |ident: &str| matches!(parse_identifier(ident), Some((n, Some(_))) if n == disk);
    let mut out: Vec<String> = parse_list(list_xml)?
        .into_iter()
        .filter(|d| parse_identifier(&d.identifier) == Some((disk, None)))
        .flat_map(|d| d.partitions)
        .filter(|p| p.mount_point.is_some())
        .map(|p| p.identifier)
        .collect();
    for c in parse_apfs_containers(list_xml)? {
        if c.stores.iter().any(|s| on_disk(s)) {
            out.extend(c.volumes.into_iter().filter(|v| v.mount_point.is_some()).map(|v| v.identifier));
        }
    }
    Ok(out)
}

/// `diskutil mount <slice>` for each; the ones that failed.
fn remount(runner: &dyn DiskutilRunner, slices: &[String]) -> Vec<String> {
    slices
        .iter()
        .filter(|s| match runner.run(&["mount", s.as_str()]) {
            Ok(_) => false,
            Err(e) => {
                log::error!("{s} could not be mounted again: {e}");
                true
            }
        })
        .cloned()
        .collect()
}

fn busy(what: String, unmounted: usize, not_remounted: Vec<String>) -> Error {
    let mut msg = what;
    if unmounted > 0 {
        if not_remounted.is_empty() {
            msg.push_str(&format!("; the {unmounted} volume(s) already unmounted were mounted again"));
        } else {
            msg.push_str(&format!("; could NOT mount again: {}", not_remounted.join(", ")));
        }
    }
    Error::Busy(msg)
}

/// Steps 1-5 of the module documentation for `diskN`; `open` opens `/dev/rdiskN` read-write.
pub fn prepare_write<D>(
    runner: &dyn DiskutilRunner,
    disk: u32,
    elevated: bool,
    open: impl FnOnce(&str) -> Result<D>,
) -> Result<D> {
    let ident = format!("disk{disk}");
    if !elevated {
        return Err(Error::Platform {
            code: EACCES,
            msg: format!("{ident}: access denied (writing a disk needs root)"),
        });
    }
    physical_whole_disk(runner, &ident)?;
    let recorded = mounted_slices(&runner.run(&["list", "-plist"])?, disk)?;

    if let Err(e) = runner.run(&["unmountDisk", &ident]) {
        let msg = match e {
            Error::Platform { msg, .. } => msg,
            other => other.to_string(),
        };
        // only what unmountDisk managed to unmount before failing is mounted again
        let still = runner.run(&["list", "-plist"]).and_then(|l| mounted_slices(&l, disk));
        let unmounted: Vec<String> = match still {
            Ok(still) => recorded.iter().filter(|s| !still.contains(s)).cloned().collect(),
            Err(_) => recorded.clone(),
        };
        let failed = remount(runner, &unmounted);
        return Err(busy(msg, unmounted.len(), failed));
    }

    match open(&format!("/dev/rdisk{disk}")) {
        Ok(dev) => Ok(dev),
        Err(e) => {
            let failed = remount(runner, &recorded);
            Err(match e {
                Error::Busy(msg) => busy(msg, recorded.len(), failed),
                other if failed.is_empty() => other,
                other => busy(other.to_string(), recorded.len(), failed),
            })
        }
    }
}
