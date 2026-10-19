//! Image format identification by extension and by content sniffing.

use crate::{qcow2, vhd, vhdx, vmdk};
use pm_core::{BlockDevice, Error, FileDevice, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    Raw,
    VhdFixed,
    VhdDynamic,
    VmdkFlat,
    VmdkSparse,
    Qcow2,
    /// Hyper-V VHDX: can be opened (read-only) but not created.
    Vhdx,
}

impl ImageFormat {
    /// Default format for a file extension (case-insensitive): `.img`/`.raw`/`.dd` → Raw,
    /// `.vhd` → dynamic VHD, `.vmdk` → sparse VMDK, `.qcow2` → qcow2, `.vhdx` → VHDX (read-only).
    pub fn from_extension(p: &Path) -> Option<Self> {
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "img" | "raw" | "dd" => Some(ImageFormat::Raw),
            "vhd" => Some(ImageFormat::VhdDynamic),
            "vmdk" => Some(ImageFormat::VmdkSparse),
            "qcow2" => Some(ImageFormat::Qcow2),
            "vhdx" => Some(ImageFormat::Vhdx),
            _ => None,
        }
    }

    /// Conventional file extension, without the dot.
    pub fn extension(&self) -> &'static str {
        match self {
            ImageFormat::Raw => "img",
            ImageFormat::VhdFixed | ImageFormat::VhdDynamic => "vhd",
            ImageFormat::VmdkFlat | ImageFormat::VmdkSparse => "vmdk",
            ImageFormat::Qcow2 => "qcow2",
            ImageFormat::Vhdx => "vhdx",
        }
    }

    pub fn display(&self) -> &'static str {
        match self {
            ImageFormat::Raw => "Raw",
            ImageFormat::VhdFixed => "VHD (fixed)",
            ImageFormat::VhdDynamic => "VHD (dynamic)",
            ImageFormat::VmdkFlat => "VMDK (flat)",
            ImageFormat::VmdkSparse => "VMDK (sparse)",
            ImageFormat::Qcow2 => "QCOW2",
            ImageFormat::Vhdx => "VHDX (read-only)",
        }
    }

    /// Formats that only store written blocks (unwritten areas cost no space).
    pub fn is_sparse(&self) -> bool {
        matches!(
            self,
            ImageFormat::VhdDynamic | ImageFormat::VmdkSparse | ImageFormat::Qcow2
        )
    }
}

/// Identify a file by content: `KDMV` magic → VMDK sparse, `QFI\xfb` → qcow2, text
/// starting with `# Disk DescriptorFile` → VMDK flat, `conectix` footer in the last
/// 512 bytes → VHD (fixed or dynamic from the disk type), anything else → raw.
pub(crate) fn sniff(path: &Path) -> Result<ImageFormat> {
    let mut dev = FileDevice::open(path, true)?;
    let len = dev.len();
    let mut head = vec![0u8; len.min(512) as usize];
    dev.read_at(0, &mut head)?;
    if head.starts_with(vmdk::SPARSE_MAGIC) {
        return Ok(ImageFormat::VmdkSparse);
    }
    if head.starts_with(vhdx::SIGNATURE) {
        return Ok(ImageFormat::Vhdx);
    }
    if head.starts_with(qcow2::MAGIC) {
        return Ok(ImageFormat::Qcow2);
    }
    if head.starts_with(vmdk::DESCRIPTOR_SIGNATURE.as_bytes()) {
        return Ok(ImageFormat::VmdkFlat);
    }
    if len >= 512 {
        let mut footer = [0u8; 512];
        dev.read_at(len - 512, &mut footer)?;
        if &footer[0..8] == b"conectix" {
            return match vhd::disk_type(&footer) {
                vhd::TYPE_FIXED => Ok(ImageFormat::VhdFixed),
                vhd::TYPE_DYNAMIC => Ok(ImageFormat::VhdDynamic),
                t => Err(Error::Unsupported(format!("VHD disk type {t}"))),
            };
        }
    }
    Ok(ImageFormat::Raw)
}
