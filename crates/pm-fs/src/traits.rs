//! The read-only filesystem interface shared by every filesystem reader.

use crate::bitmap::UsedMap;
use pm_core::{BlockDevice, CancelToken, Error, FsKind, ProgressSink, Result};
use serde::{Deserialize, Serialize};

/// A file or directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// Absolute path: "/" for the root, otherwise "/" followed by the components joined by "/".
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Modification time, unix seconds.
    pub modified: Option<i64>,
    /// Filesystem-specific identifier: first cluster / MFT reference / inode number.
    pub id: u64,
    pub deleted: bool,
    /// Filesystem-specific attribute bits (FAT: the attribute byte).
    pub attrs: u32,
}

pub trait FileSystem: Send {
    fn kind(&self) -> FsKind;
    fn label(&self) -> String;
    /// Filesystem size in bytes.
    fn total(&self) -> u64;
    fn cluster_size(&self) -> u32;
    fn root(&self) -> Entry;
    fn list(&mut self, dir: &Entry) -> Result<Vec<Entry>>;
    /// Read up to `buf.len()` bytes of the file starting at `offset`; returns the byte count
    /// (0 at or past the end of the file).
    fn read_file(&mut self, e: &Entry, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn used_map(&mut self) -> Result<UsedMap>;
    fn deleted_entries(&mut self, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<Vec<Entry>>;

    /// Copy the whole file to `dest`; returns the number of bytes written.
    fn extract(&mut self, e: &Entry, dest: &mut dyn std::io::Write) -> Result<u64> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut off = 0u64;
        loop {
            let n = self.read_file(e, off, &mut buf)?;
            if n == 0 {
                break;
            }
            dest.write_all(&buf[..n]).map_err(|err| Error::io(off, n, err))?;
            off += n as u64;
        }
        Ok(off)
    }
}

/// Open the filesystem at the start of `dev`, dispatching on [`crate::detect`].
/// Kinds without a parser yield [`Error::Unsupported`].
pub fn open(mut dev: Box<dyn BlockDevice>) -> Result<Box<dyn FileSystem>> {
    let probe = crate::detect::detect(&mut *dev, 0)
        .ok_or_else(|| Error::Unsupported("unrecognised filesystem".into()))?;
    match probe.kind {
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => Ok(Box::new(crate::fat::FatFs::open(dev, 0)?)),
        FsKind::Ntfs => Ok(Box::new(crate::ntfs::Ntfs::open(dev)?)),
        FsKind::ExFat => Ok(Box::new(crate::exfat::ExFat::open(dev)?)),
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 => Ok(Box::new(crate::ext::Ext::open(dev)?)),
        k => Err(Error::Unsupported(format!("no reader for {}", k.display_name()))),
    }
}
