//! Saveable scan results, so a long scan can be reviewed or resumed later.

use crate::{CarvedFile, DeletedFile};
use pm_core::{DiskInfo, Error, Result};
use pm_parttable::Candidate;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// What identifies the disk a session was recorded on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub model: String,
    pub serial: String,
    /// Disk size in bytes.
    pub size: u64,
    /// MBR signature or GPT disk GUID at scan time (empty when the table was gone).
    pub table_id: String,
}

impl DeviceIdentity {
    pub fn from_disk(d: &DiskInfo) -> DeviceIdentity {
        DeviceIdentity {
            model: d.model.clone(),
            serial: d.serial.clone(),
            size: d.size,
            table_id: d.disk_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SessionKind {
    Partitions(Vec<Candidate>),
    Deleted(Vec<DeletedFile>),
    Carved(Vec<CarvedFile>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanSession {
    /// Creation time, ISO 8601 UTC (`2026-10-19T12:34:56Z`).
    pub created: String,
    pub device: DeviceIdentity,
    pub kind: SessionKind,
}

impl ScanSession {
    /// A session created now.
    pub fn new(device: DeviceIdentity, kind: SessionKind) -> ScanSession {
        ScanSession { created: now_iso8601(), device, kind }
    }

    /// Write the session as JSON. The file is written next to `p` and renamed into place, so an
    /// existing session is never left half-written.
    pub fn save(&self, p: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| Error::Parse(format!("session: {e}")))?;
        let mut tmp = p.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
            std::fs::rename(&tmp, p)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            Error::io(0, json.len(), e)
        })
    }

    pub fn load(p: &Path) -> Result<ScanSession> {
        let bytes = std::fs::read(p).map_err(|e| Error::io(0, 0, e))?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Parse(format!("{}: {e}", p.display())))
    }

    /// Whether `d` is the disk this session was recorded on: same model, serial and size. The
    /// table id is compared only when both sides have one, because recovering a table gives the
    /// disk a new id.
    pub fn matches(&self, d: &DiskInfo) -> bool {
        let id = &self.device.table_id;
        self.device.model.trim() == d.model.trim()
            && self.device.serial.trim() == d.serial.trim()
            && self.device.size == d.size
            && (id.is_empty() || d.disk_id.is_empty() || id.eq_ignore_ascii_case(&d.disk_id))
    }
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn iso8601(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn now_iso8601() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    iso8601(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_known_dates() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(1_781_870_096), "2026-06-19T11:54:56Z");
    }
}
