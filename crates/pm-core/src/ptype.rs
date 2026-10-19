//! Partition type identifiers (MBR type bytes and GPT type GUIDs).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Well-known GPT partition type GUIDs.
pub mod gpt_types {
    use uuid::{uuid, Uuid};

    pub const EFI_SYSTEM: Uuid = uuid!("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    pub const MSR: Uuid = uuid!("E3C9E316-0B5C-4DB8-817D-F92DF00215AE");
    pub const BASIC_DATA: Uuid = uuid!("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
    pub const WINDOWS_RECOVERY: Uuid = uuid!("DE94BBA4-06D1-4D40-A16A-BFD50179D6AC");
    pub const LINUX_FS: Uuid = uuid!("0FC63DAF-8483-4772-8E79-3D69D8477DE4");
    pub const LINUX_SWAP: Uuid = uuid!("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F");
    pub const LINUX_LVM: Uuid = uuid!("E6D6D379-F507-44C2-A23C-238F2A3DF928");
    pub const APPLE_HFS: Uuid = uuid!("48465300-0000-11AA-AA11-00306543ECAC");
    pub const APPLE_APFS: Uuid = uuid!("7C3457EF-0000-11AA-AA11-00306543ECAC");
    pub const BIOS_BOOT: Uuid = uuid!("21686148-6449-6E6F-744E-656564454649");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionType {
    Mbr(u8),
    Gpt(Uuid),
}

impl PartitionType {
    /// Human-readable name, e.g. `"GPT (EFI system partition)"` or `"NTFS/exFAT (0x07)"`.
    pub fn name(&self) -> String {
        match self {
            PartitionType::Gpt(g) => match *g {
                gpt_types::EFI_SYSTEM => "GPT (EFI system partition)".to_string(),
                gpt_types::MSR => "GPT (Microsoft reserved)".to_string(),
                gpt_types::BASIC_DATA => "GPT (basic data)".to_string(),
                gpt_types::WINDOWS_RECOVERY => "GPT (recovery)".to_string(),
                gpt_types::LINUX_FS => "Linux filesystem".to_string(),
                gpt_types::LINUX_SWAP => "Linux swap".to_string(),
                gpt_types::LINUX_LVM => "Linux LVM".to_string(),
                gpt_types::APPLE_HFS => "Apple HFS+".to_string(),
                gpt_types::APPLE_APFS => "Apple APFS".to_string(),
                gpt_types::BIOS_BOOT => "GPT (BIOS boot)".to_string(),
                other => format!("GPT ({other})"),
            },
            PartitionType::Mbr(b) => {
                let label = match b {
                    0x00 => "Empty",
                    0x01 => "FAT12",
                    0x04 => "FAT16 <32M",
                    0x05 => "Extended",
                    0x06 => "FAT16",
                    0x07 => "NTFS/exFAT",
                    0x0B => "FAT32",
                    0x0C => "FAT32 LBA",
                    0x0E => "FAT16 LBA",
                    0x0F => "Extended LBA",
                    0x27 => "Windows recovery",
                    0x82 => "Linux swap",
                    0x83 => "Linux",
                    0x85 => "Linux extended",
                    0x8E => "Linux LVM",
                    0xAF => "Apple HFS",
                    0xEE => "GPT protective",
                    0xEF => "EFI",
                    _ => "Unknown",
                };
                format!("{label} (0x{b:02X})")
            }
        }
    }

    pub fn is_efi(&self) -> bool {
        matches!(self, PartitionType::Gpt(g) if *g == gpt_types::EFI_SYSTEM)
            || matches!(self, PartitionType::Mbr(0xEF))
    }

    pub fn is_msr(&self) -> bool {
        matches!(self, PartitionType::Gpt(g) if *g == gpt_types::MSR)
    }

    pub fn is_recovery(&self) -> bool {
        matches!(self, PartitionType::Gpt(g) if *g == gpt_types::WINDOWS_RECOVERY)
            || matches!(self, PartitionType::Mbr(0x27))
    }

    /// Ordinary data partitions: GPT basic data / Linux filesystem, and MBR FAT, NTFS/exFAT and Linux types.
    pub fn is_data(&self) -> bool {
        match self {
            PartitionType::Gpt(g) => *g == gpt_types::BASIC_DATA || *g == gpt_types::LINUX_FS,
            PartitionType::Mbr(b) => matches!(b, 0x01 | 0x04 | 0x06 | 0x07 | 0x0B | 0x0C | 0x0E | 0x83),
        }
    }

    /// MBR extended-partition container types (0x05, 0x0F, 0x85).
    pub fn is_extended_mbr(&self) -> bool {
        matches!(self, PartitionType::Mbr(0x05 | 0x0F | 0x85))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn ptype_names_for_known_guids() {
        assert_eq!(PartitionType::Gpt(gpt_types::EFI_SYSTEM).name(), "GPT (EFI system partition)");
        assert_eq!(PartitionType::Gpt(gpt_types::MSR).name(), "GPT (Microsoft reserved)");
        assert_eq!(PartitionType::Gpt(gpt_types::BASIC_DATA).name(), "GPT (basic data)");
        assert_eq!(PartitionType::Gpt(gpt_types::WINDOWS_RECOVERY).name(), "GPT (recovery)");
        assert_eq!(PartitionType::Gpt(gpt_types::LINUX_FS).name(), "Linux filesystem");
        let unk = Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap();
        assert_eq!(
            PartitionType::Gpt(unk).name(),
            "GPT (12345678-1234-1234-1234-123456789abc)"
        );
        assert_eq!(PartitionType::Mbr(0x07).name(), "NTFS/exFAT (0x07)");
        assert_eq!(PartitionType::Mbr(0x0C).name(), "FAT32 LBA (0x0C)");
        assert_eq!(PartitionType::Mbr(0x83).name(), "Linux (0x83)");
        assert_eq!(PartitionType::Mbr(0x05).name(), "Extended (0x05)");
        assert_eq!(PartitionType::Mbr(0x0F).name(), "Extended LBA (0x0F)");
        assert_eq!(PartitionType::Mbr(0xEF).name(), "EFI (0xEF)");
        assert_eq!(PartitionType::Mbr(0x42).name(), "Unknown (0x42)");
        assert_eq!(PartitionType::Mbr(0x0A).name(), "Unknown (0x0A)");
    }

    #[test]
    fn gpt_constants_have_standard_values() {
        assert_eq!(gpt_types::EFI_SYSTEM.to_string(), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
        assert_eq!(gpt_types::MSR.to_string(), "e3c9e316-0b5c-4db8-817d-f92df00215ae");
        assert_eq!(gpt_types::BASIC_DATA.to_string(), "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7");
        assert_eq!(gpt_types::WINDOWS_RECOVERY.to_string(), "de94bba4-06d1-4d40-a16a-bfd50179d6ac");
        assert_eq!(gpt_types::LINUX_FS.to_string(), "0fc63daf-8483-4772-8e79-3d69d8477de4");
        assert_eq!(gpt_types::LINUX_SWAP.to_string(), "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f");
        assert_eq!(gpt_types::LINUX_LVM.to_string(), "e6d6d379-f507-44c2-a23c-238f2a3df928");
        assert_eq!(gpt_types::APPLE_HFS.to_string(), "48465300-0000-11aa-aa11-00306543ecac");
        assert_eq!(gpt_types::APPLE_APFS.to_string(), "7c3457ef-0000-11aa-aa11-00306543ecac");
        assert_eq!(gpt_types::BIOS_BOOT.to_string(), "21686148-6449-6e6f-744e-656564454649");
    }

    #[test]
    fn ptype_predicates() {
        assert!(PartitionType::Gpt(gpt_types::EFI_SYSTEM).is_efi());
        assert!(PartitionType::Mbr(0xEF).is_efi());
        assert!(!PartitionType::Mbr(0x07).is_efi());
        assert!(PartitionType::Gpt(gpt_types::MSR).is_msr());
        assert!(!PartitionType::Gpt(gpt_types::BASIC_DATA).is_msr());
        assert!(PartitionType::Gpt(gpt_types::WINDOWS_RECOVERY).is_recovery());
        assert!(PartitionType::Mbr(0x27).is_recovery());
        assert!(PartitionType::Gpt(gpt_types::BASIC_DATA).is_data());
        assert!(PartitionType::Gpt(gpt_types::LINUX_FS).is_data());
        assert!(PartitionType::Mbr(0x07).is_data());
        assert!(!PartitionType::Gpt(gpt_types::EFI_SYSTEM).is_data());
        assert!(!PartitionType::Mbr(0x05).is_data());
        assert!(PartitionType::Mbr(0x05).is_extended_mbr());
        assert!(PartitionType::Mbr(0x0F).is_extended_mbr());
        assert!(PartitionType::Mbr(0x85).is_extended_mbr());
        assert!(!PartitionType::Mbr(0x07).is_extended_mbr());
        assert!(!PartitionType::Gpt(gpt_types::BASIC_DATA).is_extended_mbr());
    }
}
