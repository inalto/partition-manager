//! The ext2/3/4 superblock and group descriptors.

use crate::util::{cstr_utf8, le16, le32};
use pm_core::{Error, FsKind, Result};

pub const INCOMPAT_COMPRESSION: u32 = 0x1;
pub const INCOMPAT_RECOVER: u32 = 0x4;
pub const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
pub const INCOMPAT_META_BG: u32 = 0x10;
pub const INCOMPAT_EXTENTS: u32 = 0x40;
pub const INCOMPAT_64BIT: u32 = 0x80;
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
pub const RO_COMPAT_GDT_CSUM: u32 = 0x10;
pub const RO_COMPAT_BIGALLOC: u32 = 0x200;
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
pub const COMPAT_SPARSE_SUPER2: u32 = 0x200;

pub const BG_INODE_UNINIT: u16 = 0x1;
pub const BG_BLOCK_UNINIT: u16 = 0x2;

/// Byte offset of the superblock.
pub const SB_OFFSET: u64 = 1024;
pub const SB_SIZE: usize = 1024;

#[derive(Clone, Debug)]
pub struct SuperBlock {
    pub kind: FsKind,
    pub block_size: u32,
    pub blocks_count: u64,
    pub first_data_block: u64,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u32,
    pub first_ino: u32,
    pub desc_size: u32,
    pub groups: u32,
    pub reserved_gdt_blocks: u32,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub backup_bgs: [u32; 2],
    pub label: String,
}

/// One block group descriptor.
#[derive(Clone, Copy, Debug)]
pub struct GroupDesc {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub flags: u16,
}

impl SuperBlock {
    /// Parse and validate the 1024 superblock bytes against the device length. Every geometry
    /// field later used as an offset or a size is checked here.
    pub fn parse(sb: &[u8], dev_len: u64) -> Result<SuperBlock> {
        if sb.len() < SB_SIZE {
            return Err(Error::Parse("ext superblock truncated".into()));
        }
        if le16(sb, 56) != 0xEF53 {
            return Err(Error::Parse("not an ext superblock (bad magic)".into()));
        }
        let bad = |what: &str| Error::Parse(format!("ext {what}"));
        let compat = le32(sb, 92);
        let incompat = le32(sb, 96);
        let ro_compat = le32(sb, 100);
        if incompat & INCOMPAT_COMPRESSION != 0 {
            return Err(Error::Unsupported("ext compression".into()));
        }
        if incompat & INCOMPAT_JOURNAL_DEV != 0 {
            return Err(Error::Unsupported("ext external journal device".into()));
        }
        if incompat & INCOMPAT_META_BG != 0 {
            return Err(Error::Unsupported("ext META_BG group descriptor layout".into()));
        }
        if ro_compat & RO_COMPAT_BIGALLOC != 0 {
            return Err(Error::Unsupported("ext BIGALLOC (per-cluster bitmaps)".into()));
        }
        let log = le32(sb, 24);
        if log > 6 {
            return Err(bad("block size out of range"));
        }
        let bs = 1024u32 << log;
        let (inode_size, first_ino) = if le32(sb, 76) == 0 { (128, 11) } else { (le16(sb, 88) as u32, le32(sb, 84)) };
        if !inode_size.is_power_of_two() || inode_size < 128 || inode_size > bs {
            return Err(bad("inode size out of range"));
        }
        let bpg = le32(sb, 32);
        let ipg = le32(sb, 40);
        if bpg == 0 || bpg as u64 > 8 * bs as u64 {
            return Err(bad("blocks per group out of range"));
        }
        if ipg == 0 || ipg as u64 > 8 * bs as u64 {
            return Err(bad("inodes per group out of range"));
        }
        let mut blocks_count = le32(sb, 4) as u64;
        let desc_size = if incompat & INCOMPAT_64BIT != 0 {
            blocks_count |= (le32(sb, 0x150) as u64) << 32;
            let ds = le16(sb, 254) as u32;
            if !ds.is_power_of_two() || ds < 64 || ds > bs {
                return Err(bad("group descriptor size out of range"));
            }
            ds
        } else {
            32
        };
        if blocks_count == 0 {
            return Err(bad("block count is zero"));
        }
        let total = blocks_count.checked_mul(bs as u64).ok_or_else(|| bad("size overflows"))?;
        if total > dev_len {
            return Err(Error::Parse(format!("ext filesystem of {total} bytes exceeds the {dev_len}-byte device")));
        }
        let fdb = le32(sb, 20) as u64;
        if fdb > 1 || fdb >= blocks_count {
            return Err(bad("first data block out of range"));
        }
        let groups = (blocks_count - fdb).div_ceil(bpg as u64);
        if groups == 0 || groups > u32::MAX as u64 / 2 {
            return Err(bad("group count out of range"));
        }
        // the descriptor table must lie inside the filesystem
        let gdt_end = (fdb + 1) * bs as u64 + groups * desc_size as u64;
        if gdt_end > total {
            return Err(bad("group descriptor table lies beyond the filesystem"));
        }
        // every group's inode table is part of the filesystem; this also bounds a scan of all of
        // them by the device length
        let tables = groups
            .checked_mul(ipg as u64)
            .and_then(|t| t.checked_mul(inode_size as u64))
            .ok_or_else(|| bad("inode table size overflows"))?;
        if tables > total {
            return Err(bad("inode tables are larger than the filesystem"));
        }
        let kind = if incompat & (INCOMPAT_EXTENTS | INCOMPAT_64BIT) != 0 || ro_compat & (0x8 | 0x40) != 0 {
            FsKind::Ext4
        } else if compat & COMPAT_HAS_JOURNAL != 0 {
            FsKind::Ext3
        } else {
            FsKind::Ext2
        };
        Ok(SuperBlock {
            kind,
            block_size: bs,
            blocks_count,
            first_data_block: fdb,
            blocks_per_group: bpg,
            inodes_per_group: ipg,
            inode_size,
            first_ino,
            desc_size,
            groups: groups as u32,
            reserved_gdt_blocks: le16(sb, 0xCE) as u32,
            compat,
            incompat,
            ro_compat,
            backup_bgs: [le32(sb, 0x24C), le32(sb, 0x250)],
            label: cstr_utf8(&sb[120..136]),
        })
    }

    pub fn total_bytes(&self) -> u64 {
        self.blocks_count * self.block_size as u64
    }

    pub fn needs_recovery(&self) -> bool {
        self.incompat & INCOMPAT_RECOVER != 0
    }

    /// Whether `bg_flags` carry meaning (they do only with descriptor checksums).
    pub fn has_group_flags(&self) -> bool {
        self.ro_compat & (RO_COMPAT_GDT_CSUM | RO_COMPAT_METADATA_CSUM) != 0
    }

    /// Byte offset of the group descriptor table.
    pub fn gdt_offset(&self) -> u64 {
        (self.first_data_block + 1) * self.block_size as u64
    }

    pub fn gdt_bytes(&self) -> u64 {
        self.groups as u64 * self.desc_size as u64
    }

    pub fn gdt_blocks(&self) -> u64 {
        self.gdt_bytes().div_ceil(self.block_size as u64)
    }

    pub fn inode_table_blocks(&self) -> u64 {
        (self.inodes_per_group as u64 * self.inode_size as u64).div_ceil(self.block_size as u64)
    }

    pub fn group_first_block(&self, g: u32) -> u64 {
        self.first_data_block + g as u64 * self.blocks_per_group as u64
    }

    /// Whether group `g` may hold a superblock and descriptor table backup. An
    /// over-approximation under `SPARSE_SUPER2`, which is safe for a used map.
    pub fn may_have_backup(&self, g: u32) -> bool {
        let sparse = self.ro_compat & RO_COMPAT_SPARSE_SUPER != 0 || self.compat & COMPAT_SPARSE_SUPER2 != 0;
        if !sparse || g <= 1 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 && self.backup_bgs.contains(&g) {
            return true;
        }
        [3u64, 5, 7].iter().any(|&b| {
            let mut p = b;
            while p < g as u64 {
                p *= b;
            }
            p == g as u64
        })
    }

    /// Parse a descriptor from its `desc_size` bytes.
    pub fn parse_group(&self, d: &[u8]) -> GroupDesc {
        let wide = self.desc_size >= 64 && d.len() >= 0x2C;
        let hi = |o: usize| if wide { (le32(d, o) as u64) << 32 } else { 0 };
        GroupDesc {
            block_bitmap: le32(d, 0) as u64 | hi(0x20),
            inode_bitmap: le32(d, 4) as u64 | hi(0x24),
            inode_table: le32(d, 8) as u64 | hi(0x28),
            flags: le16(d, 0x12),
        }
    }
}
