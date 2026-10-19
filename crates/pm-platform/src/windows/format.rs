//! Format (fmifs `FormatEx`, falling back to format.com), shrink and extend of volumes.

use super::device::WinDevice;
use super::error::platform_error;
use super::ioctl;
use super::mount;
use super::volume::{device_path, root_path};
use crate::traits::{fs_format_name, ExternalTool, MountPoint, OpenMode};
use pm_core::{Error, FsKind, ProgressReport, ProgressSink, Result, VolumeId};
use std::time::Duration;
use windows_sys::Win32::System::Ioctl::{ShrinkAbort, ShrinkCommit, ShrinkPrepare};

const PROGRESS_POLL: Duration = Duration::from_millis(100);
/// `ERROR_GEN_FAILURE`, reported when FormatEx signals failure.
const ERROR_GEN_FAILURE: u32 = 31;

fn report(progress: &dyn ProgressSink, percent: u32) {
    progress.report(&ProgressReport {
        phase: "format".into(),
        done: percent.min(100) as u64,
        total: 100,
        ..Default::default()
    });
}

/// A mount point usable as FormatEx's drive root; assigns a temporary free letter when the
/// volume has none (returned so it can be removed afterwards).
fn format_root(id: &VolumeId) -> Result<(String, Option<MountPoint>)> {
    let existing = ioctl::volume_path_names(&root_path(id))
        .map_err(|c| platform_error(c, &format!("query mount points of {}", id.0)))?;
    if let Some(m) = existing.into_iter().next() {
        return Ok((m, None));
    }
    let letter = *mount::free_letters()
        .last()
        .ok_or_else(|| Error::Busy("no free drive letter for formatting".into()))?;
    let mp = MountPoint::Letter(letter);
    mount::mount(id, &mp)?;
    Ok((mp.to_path_string(), Some(mp)))
}

pub fn format(
    id: &VolumeId,
    fs: FsKind,
    label: &str,
    cluster: Option<u32>,
    quick: bool,
    progress: &dyn ProgressSink,
) -> Result<()> {
    let fs_name = fs_format_name(fs)
        .ok_or_else(|| Error::Unsupported(format!("Windows cannot format {}", fs.display_name())))?;
    let (root, temp) = format_root(id)?;
    let result = format_at(&root, fs_name, label, cluster, quick, progress);
    if let Some(mp) = temp {
        if let Err(e) = mount::unmount(id, &mp) {
            log::warn!("removing temporary mount point {}: {e}", mp.to_path_string());
        }
    }
    result
}

fn format_at(
    root: &str,
    fs_name: &str,
    label: &str,
    cluster: Option<u32>,
    quick: bool,
    progress: &dyn ProgressSink,
) -> Result<()> {
    let guard = ioctl::format_lock();
    report(progress, 0);
    let outcome = std::thread::scope(|s| {
        let worker = s.spawn(|| ioctl::format_ex(&guard, root, fs_name, label, quick, cluster.unwrap_or(0)));
        let mut last = u32::MAX;
        while !worker.is_finished() {
            let pct = guard.percent();
            if pct != last {
                report(progress, pct);
                last = pct;
            }
            std::thread::sleep(PROGRESS_POLL);
        }
        worker.join()
    });
    match outcome {
        Ok(Ok(true)) => {
            report(progress, 100);
            Ok(())
        }
        Ok(Ok(false)) => Err(Error::Platform {
            code: ERROR_GEN_FAILURE,
            msg: format!(
                "format {root} as {fs_name} failed (FormatEx callback command {})",
                guard.last_command()
            ),
        }),
        Ok(Err(code)) => {
            log::warn!("fmifs.dll unavailable (error {code}); falling back to format.com");
            drop(guard);
            format_com(root, fs_name, label, cluster, quick)?;
            report(progress, 100);
            Ok(())
        }
        Err(_) => Err(Error::Platform {
            code: ERROR_GEN_FAILURE,
            msg: "format worker thread panicked".into(),
        }),
    }
}

/// `format.com <root> /FS:<fs> [/Q] /V:<label> [/A:<cluster>] /Y`.
fn format_com(root: &str, fs_name: &str, label: &str, cluster: Option<u32>, quick: bool) -> Result<()> {
    let target = if root.len() == 3 && root.ends_with(":\\") {
        root[..2].to_string()
    } else {
        root.trim_end_matches('\\').to_string()
    };
    let mut args = vec![target, format!("/FS:{fs_name}")];
    if quick {
        args.push("/Q".into());
    }
    args.push(format!("/V:{label}"));
    if let Some(c) = cluster {
        args.push(format!("/A:{c}"));
    }
    args.push("/Y".into());
    super::run_tool(ExternalTool::FormatCom, &args).map(|_| ())
}

/// Open a volume read-write for FSCTLs and return it with its new length in sectors.
fn open_for_resize(id: &VolumeId, new_len: u64) -> Result<(WinDevice, u64)> {
    let dev = WinDevice::open(&device_path(id), OpenMode::ReadWrite)?;
    let ss = pm_core::BlockDevice::sector_size(&dev).max(1) as u64;
    Ok((dev, new_len / ss))
}

/// Shrink the filesystem on `id` to `new_len` bytes (`FSCTL_SHRINK_VOLUME` prepare + commit).
pub fn shrink_volume(id: &VolumeId, new_len: u64) -> Result<()> {
    let (dev, sectors) = open_for_resize(id, new_len)?;
    let h = dev.handle();
    ioctl::shrink_volume(h, ShrinkPrepare, sectors)
        .map_err(|c| platform_error(c, &format!("prepare shrink of {}", id.0)))?;
    if let Err(c) = ioctl::shrink_volume(h, ShrinkCommit, sectors) {
        let _ = ioctl::shrink_volume(h, ShrinkAbort, 0);
        return Err(platform_error(c, &format!("shrink {}", id.0)));
    }
    Ok(())
}

/// Grow the filesystem on `id` to `new_len` bytes (`FSCTL_EXTEND_VOLUME`); the partition must
/// already have been grown.
pub fn extend_volume(id: &VolumeId, new_len: u64) -> Result<()> {
    let (dev, sectors) = open_for_resize(id, new_len)?;
    ioctl::extend_volume(dev.handle(), sectors).map_err(|c| platform_error(c, &format!("extend {}", id.0)))
}
