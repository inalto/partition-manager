//! Filesystem identification kinds.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FsKind {
    Ntfs,
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    ReFs,
    Ext2,
    Ext3,
    Ext4,
    Btrfs,
    Xfs,
    HfsPlus,
    Apfs,
    Luks,
    LinuxSwap,
    Iso9660,
    Udf,
    Unknown,
}

impl FsKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            FsKind::Ntfs => "NTFS",
            FsKind::Fat12 => "FAT12",
            FsKind::Fat16 => "FAT16",
            FsKind::Fat32 => "FAT32",
            FsKind::ExFat => "exFAT",
            FsKind::ReFs => "ReFS",
            FsKind::Ext2 => "Ext2",
            FsKind::Ext3 => "Ext3",
            FsKind::Ext4 => "Ext4",
            FsKind::Btrfs => "Btrfs",
            FsKind::Xfs => "XFS",
            FsKind::HfsPlus => "HFS+",
            FsKind::Apfs => "APFS",
            FsKind::Luks => "LUKS",
            FsKind::LinuxSwap => "Linux swap",
            FsKind::Iso9660 => "ISO 9660",
            FsKind::Udf => "UDF",
            FsKind::Unknown => "Unknown",
        }
    }

    /// Whether Windows can mount this filesystem natively.
    pub fn windows_mountable(&self) -> bool {
        matches!(
            self,
            FsKind::Ntfs | FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::ExFat | FsKind::ReFs
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_and_mountability() {
        assert_eq!(FsKind::Ntfs.display_name(), "NTFS");
        assert_eq!(FsKind::Fat32.display_name(), "FAT32");
        assert_eq!(FsKind::ExFat.display_name(), "exFAT");
        assert_eq!(FsKind::Ext4.display_name(), "Ext4");
        assert_eq!(FsKind::Unknown.display_name(), "Unknown");
        for fs in [FsKind::Ntfs, FsKind::Fat12, FsKind::Fat16, FsKind::Fat32, FsKind::ExFat, FsKind::ReFs] {
            assert!(fs.windows_mountable(), "{fs:?}");
        }
        for fs in [FsKind::Ext4, FsKind::Btrfs, FsKind::Luks, FsKind::LinuxSwap, FsKind::Unknown] {
            assert!(!fs.windows_mountable(), "{fs:?}");
        }
    }
}
