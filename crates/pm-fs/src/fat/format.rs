//! FAT32 formatter using the standard Microsoft layout.

use pm_core::{BlockDevice, Error, Result, GIB, MIB};

/// Options for [`format_fat32`].
pub struct FormatOpts {
    /// Volume label, at most 11 ASCII characters; stored upper-case. Empty means no label.
    pub label: String,
    /// Cluster size in bytes; `None` picks one from the volume size.
    pub cluster_size: Option<u32>,
    /// 32-bit volume serial number; `None` derives one from the current time.
    pub volume_id: Option<u32>,
}

const RESERVED_SECTORS: u32 = 32;
const FS_INFO_SECTOR: u32 = 1;
const BACKUP_BOOT_SECTOR: u32 = 6;
const NUM_FATS: u32 = 2;
const MIN_CLUSTERS: u64 = 65_525;
const MAX_CLUSTERS: u64 = 0x0FFF_FFF5;
const ZERO_CHUNK: usize = 1 << 20;

/// `(sectors per FAT, cluster count)` for a volume with the given geometry.
fn geometry(total_sectors: u64, bps: u64, spc: u64) -> (u64, u64) {
    let tmp = total_sectors - RESERVED_SECTORS as u64;
    let per_sector = bps / 4;
    let mut fat = tmp.div_ceil(spc * per_sector + 2);
    loop {
        let clusters = (tmp - NUM_FATS as u64 * fat) / spc;
        if fat * per_sector >= clusters + 2 {
            return (fat, clusters);
        }
        fat += 1;
    }
}

fn default_cluster_bytes(volume_bytes: u64) -> u64 {
    if volume_bytes <= 8 * GIB {
        4096
    } else if volume_bytes <= 16 * GIB {
        8192
    } else if volume_bytes <= 32 * GIB {
        16384
    } else {
        32768
    }
}

fn label_bytes(label: &str) -> Result<[u8; 11]> {
    let up = label.to_ascii_uppercase();
    if up.len() > 11 {
        return Err(Error::Invalid(format!("volume label '{label}' is longer than 11 characters")));
    }
    if !up.bytes().all(|b| (0x20..0x7F).contains(&b) && !b"\"*+,./:;<=>?[\\]|".contains(&b)) {
        return Err(Error::Invalid(format!("volume label '{label}' contains characters FAT does not allow")));
    }
    let mut out = [b' '; 11];
    out[..up.len()].copy_from_slice(up.as_bytes());
    Ok(out)
}

fn zero_range(dev: &mut dyn BlockDevice, mut off: u64, mut len: u64) -> Result<()> {
    let buf = vec![0u8; ZERO_CHUNK.min(len as usize)];
    while len > 0 {
        let n = (buf.len() as u64).min(len) as usize;
        dev.write_at(off, &buf[..n])?;
        off += n as u64;
        len -= n as u64;
    }
    Ok(())
}

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn derived_volume_id() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_secs() as u32) ^ t.subsec_nanos().rotate_left(16)
}

/// Format the whole device as FAT32 (reserved area, both FATs and the root directory
/// cluster are rewritten; the rest of the data area is left untouched).
pub fn format_fat32(dev: &mut dyn BlockDevice, opts: &FormatOpts) -> Result<()> {
    let bps = dev.sector_size();
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return Err(Error::Invalid(format!("unsupported sector size {bps}")));
    }
    let bps64 = bps as u64;
    let len = dev.len();
    if len < 33 * MIB {
        return Err(Error::Invalid("FAT32 needs at least 33 MiB".into()));
    }
    let total_sectors = len / bps64;
    if total_sectors > u32::MAX as u64 {
        return Err(Error::Invalid("volume too large for a FAT32 boot sector (max 2^32 sectors)".into()));
    }
    let label = label_bytes(&opts.label)?;

    // choose the cluster size
    let (spc, fat_sectors, clusters) = match opts.cluster_size {
        Some(cs) => {
            let cs64 = cs as u64;
            if !cs.is_power_of_two() || cs64 < bps64 || cs64 > 128 * bps64 {
                return Err(Error::Invalid(format!("invalid cluster size {cs}")));
            }
            let spc = cs64 / bps64;
            let (fat, clusters) = geometry(total_sectors, bps64, spc);
            if !(MIN_CLUSTERS..=MAX_CLUSTERS).contains(&clusters) {
                return Err(Error::Invalid(format!(
                    "cluster size {cs} gives {clusters} clusters; FAT32 needs between {MIN_CLUSTERS} and {MAX_CLUSTERS}"
                )));
            }
            (spc, fat, clusters)
        }
        None => {
            let mut spc = (default_cluster_bytes(len) / bps64).max(1);
            let mut geo = geometry(total_sectors, bps64, spc);
            // small volumes: shrink the cluster until the count reaches the FAT32 minimum
            while geo.1 < MIN_CLUSTERS && spc > 1 {
                spc /= 2;
                geo = geometry(total_sectors, bps64, spc);
            }
            // huge volumes with small sectors: grow it until the count fits
            while geo.1 > MAX_CLUSTERS && spc < 128 {
                spc *= 2;
                geo = geometry(total_sectors, bps64, spc);
            }
            if !(MIN_CLUSTERS..=MAX_CLUSTERS).contains(&geo.1) {
                return Err(Error::Invalid(format!(
                    "a {len}-byte volume with {bps}-byte sectors cannot be formatted as FAT32"
                )));
            }
            (spc, geo.0, geo.1)
        }
    };

    let volume_id = opts.volume_id.unwrap_or_else(derived_volume_id);
    let reserved_bytes = RESERVED_SECTORS as u64 * bps64;
    let fat_bytes = fat_sectors * bps64;
    let data_start = reserved_bytes + NUM_FATS as u64 * fat_bytes;
    let cluster_bytes = spc * bps64;

    // wipe what we own: reserved area, both FATs, root cluster
    zero_range(dev, 0, data_start)?;
    zero_range(dev, data_start, cluster_bytes)?;

    // boot sector
    let mut boot = vec![0u8; bps as usize];
    boot[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    put16(&mut boot, 11, bps as u16);
    boot[13] = spc as u8;
    put16(&mut boot, 14, RESERVED_SECTORS as u16);
    boot[16] = NUM_FATS as u8;
    boot[21] = 0xF8;
    put16(&mut boot, 24, 63);
    put16(&mut boot, 26, 255);
    put32(&mut boot, 32, total_sectors as u32);
    put32(&mut boot, 36, fat_sectors as u32);
    put32(&mut boot, 44, 2);
    put16(&mut boot, 48, FS_INFO_SECTOR as u16);
    put16(&mut boot, 50, BACKUP_BOOT_SECTOR as u16);
    boot[64] = 0x80;
    boot[66] = 0x29;
    put32(&mut boot, 67, volume_id);
    if opts.label.is_empty() {
        boot[71..82].copy_from_slice(b"NO NAME    ");
    } else {
        boot[71..82].copy_from_slice(&label);
    }
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;

    // FSInfo
    let mut info = vec![0u8; bps as usize];
    info[0..4].copy_from_slice(b"RRaA");
    info[484..488].copy_from_slice(b"rrAa");
    put32(&mut info, 488, (clusters - 1) as u32);
    put32(&mut info, 492, 3);
    info[508..512].copy_from_slice(&[0x00, 0x00, 0x55, 0xAA]);

    // sector 2 carries only the boot signature
    let mut tail = vec![0u8; bps as usize];
    tail[510] = 0x55;
    tail[511] = 0xAA;

    for (i, sector) in [&boot, &info, &tail].into_iter().enumerate() {
        dev.write_at(i as u64 * bps64, sector)?;
        dev.write_at((BACKUP_BOOT_SECTOR as u64 + i as u64) * bps64, sector)?;
    }

    // FATs: media descriptor, end-of-chain marker, root directory chain
    let mut fat0 = vec![0u8; 12];
    put32(&mut fat0, 0, 0x0FFF_FFF8);
    put32(&mut fat0, 4, 0x0FFF_FFFF);
    put32(&mut fat0, 8, 0x0FFF_FFFF);
    for i in 0..NUM_FATS as u64 {
        dev.write_at(reserved_bytes + i * fat_bytes, &fat0)?;
    }

    // root directory: volume label entry
    if !opts.label.is_empty() {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&label);
        e[11] = 0x08;
        dev.write_at(data_start, &e)?;
    }
    dev.flush()?;
    Ok(())
}
