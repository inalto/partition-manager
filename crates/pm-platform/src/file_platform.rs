//! [`FilePlatform`]: a [`Platform`] whose disks are raw image files, so the CLI and the GUI can
//! be exercised (and used to prepare images) without administrator rights or a real disk.
//!
//! Disk `i` is the `i`-th file passed to [`FilePlatform::new`]. Partition tables are read and
//! written with `pm-parttable`; data is read and written through [`pm_core::FileDevice`], so only
//! raw images are supported (no VHD / VMDK / qcow2 layer here).
//!
//! Operations that need an operating system volume layer (mount, unmount, format, shrink,
//! extend, external tools) fail with [`Error::Unsupported`]; there are no volumes, so
//! partitions have no drive letters and no used-space figure. Each partition's file system
//! kind and label are read from its first sectors (`pm_fs::detect`), which lets the planner
//! and executor protect them: with the fill level unknown a shrink is refused, and so is a
//! length change of a non-NTFS file system.
//!
//! # The system disk
//!
//! The planner and the executor refuse destructive work when the running system disk cannot be
//! determined (`Platform::system_disk() == None`), which would block every operation here.
//! [`FilePlatform::system_disk`] therefore returns `Some(`[`NO_SYSTEM_DISK`]`)`, an index that
//! matches no file: "the system disk is known, and it is none of these images".

use crate::traits::{ExternalTool, MountPoint, OpenMode, Platform, SmartData, WriteSession};
use pm_core::{
    BlockDevice, DiskBus, DiskInfo, Error, FileDevice, FsKind, ProgressSink, Result, VolumeId,
    VolumeInfo,
};
use pm_parttable::{read_table, write_table, Table};
use std::path::{Path, PathBuf};

/// The index [`FilePlatform::system_disk`] reports: no image file can have it.
pub const NO_SYSTEM_DISK: u32 = u32::MAX;

/// Drive letters reported as free (nothing is ever mounted on image files).
const FREE_LETTERS: [char; 3] = ['E', 'F', 'G'];

fn unsupported<T>() -> Result<T> {
    Err(Error::Unsupported("not available on image files".into()))
}

/// Disks backed by raw image files; see the [module documentation](self).
#[derive(Clone, Debug)]
pub struct FilePlatform {
    files: Vec<PathBuf>,
}

impl FilePlatform {
    /// One disk per file, indexed by position. `Error::Invalid` for an empty list or a path
    /// that is not a regular file, `Error::NotFound` for a file that does not exist.
    pub fn new(files: Vec<PathBuf>) -> Result<Self> {
        if files.is_empty() {
            return Err(Error::Invalid("no image files given".into()));
        }
        for f in &files {
            match std::fs::metadata(f) {
                Ok(m) if m.is_file() => {}
                Ok(_) => return Err(Error::Invalid(format!("'{}' is not a file", f.display()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::NotFound(format!("image file '{}' does not exist", f.display())))
                }
                Err(e) => return Err(Error::io(0, 0, e)),
            }
        }
        Ok(FilePlatform { files })
    }

    /// The image files, in disk-index order.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    fn path(&self, index: u32) -> Result<&Path> {
        self.files
            .get(index as usize)
            .map(PathBuf::as_path)
            .ok_or_else(|| Error::NotFound(format!("disk {index}")))
    }

    fn open(&self, index: u32, read_only: bool) -> Result<FileDevice> {
        FileDevice::open(self.path(index)?, read_only)
    }
}

struct FileSession {
    disk: u32,
    dev: FileDevice,
}

impl WriteSession for FileSession {
    fn disk(&self) -> u32 {
        self.disk
    }
    fn device(&mut self) -> &mut dyn BlockDevice {
        &mut self.dev
    }
    fn commit_table(&mut self, t: &Table) -> Result<()> {
        write_table(&mut self.dev, t)?;
        self.dev.flush()
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.dev.flush()
    }
}

impl Platform for FilePlatform {
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        let mut out = Vec::with_capacity(self.files.len());
        for (i, path) in self.files.iter().enumerate() {
            let mut dev = self.open(i as u32, true)?;
            let base = DiskInfo {
                index: i as u32,
                path: path.display().to_string(),
                model: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                bus: DiskBus::Virtual,
                size: dev.len(),
                sector_size: dev.sector_size(),
                physical_sector_size: dev.sector_size(),
                ..Default::default()
            };
            out.push(match read_table(&mut dev) {
                Ok(t) => {
                    let mut info = t.to_disk_info(base);
                    // kind and label only: the fill level stays unknown (`used == None`), so the
                    // planner's "unknown fill level" rule keeps shrinks of a filesystem away
                    for p in &mut info.partitions {
                        if let Some(probe) = pm_fs::detect(&mut dev, p.start) {
                            if probe.kind != FsKind::Unknown {
                                p.fs = Some(probe.kind);
                                p.label = probe.label;
                            }
                        }
                    }
                    info
                }
                Err(e @ (Error::Io { .. } | Error::Cancelled)) => return Err(e),
                // a damaged table: show the disk without partitions rather than failing the list
                Err(e) => {
                    log::debug!("image {}: read_table failed: {e}", path.display());
                    base
                }
            });
        }
        Ok(out)
    }

    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(Vec::new())
    }

    fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(self.open(index, mode == OpenMode::ReadOnly)?))
    }

    fn open_volume(&self, id: &VolumeId, _mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        Err(Error::NotFound(format!("volume {}", id.0)))
    }

    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>> {
        Ok(Box::new(FileSession { disk, dev: self.open(disk, false)? }))
    }

    fn mount(&self, _id: &VolumeId, _at: &MountPoint) -> Result<()> {
        unsupported()
    }

    fn unmount(&self, _id: &VolumeId, _at: &MountPoint) -> Result<()> {
        unsupported()
    }

    fn set_offline(&self, _disk: u32, _offline: bool) -> Result<()> {
        Ok(())
    }

    fn rescan(&self, _disk: u32) -> Result<()> {
        Ok(())
    }

    fn format(
        &self,
        _id: &VolumeId,
        _fs: FsKind,
        _label: &str,
        _cluster: Option<u32>,
        _quick: bool,
        _progress: &dyn ProgressSink,
    ) -> Result<()> {
        unsupported()
    }

    fn shrink_volume(&self, _id: &VolumeId, _new_len: u64) -> Result<()> {
        unsupported()
    }

    fn extend_volume(&self, _id: &VolumeId, _new_len: u64) -> Result<()> {
        unsupported()
    }

    fn smart(&self, _disk: u32) -> Result<Option<SmartData>> {
        Ok(None)
    }

    fn is_elevated(&self) -> bool {
        true
    }

    fn system_disk(&self) -> Option<u32> {
        Some(NO_SYSTEM_DISK)
    }

    fn free_letters(&self) -> Vec<char> {
        FREE_LETTERS.to_vec()
    }

    fn run_tool(&self, _tool: ExternalTool, _args: &[String]) -> Result<String> {
        unsupported()
    }
}
