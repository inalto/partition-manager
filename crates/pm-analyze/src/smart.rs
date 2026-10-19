//! SMART summary: turns [`SmartData`] into display rows with a health level.

use pm_platform::SmartData;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Health {
    Ok,
    Warn,
    Bad,
}

const TEMP_WARN_C: u32 = 55;
const TEMP_BAD_C: u32 = 70;
const USED_WARN_PCT: u8 = 80;
const USED_BAD_PCT: u8 = 100;
/// Raw counts above this are `Bad` rather than `Warn` for the sector-error attributes.
const ERROR_COUNT_BAD: u64 = 100;

/// ATA attributes where any non-zero raw count is a warning (and a large one is bad):
/// reallocated sectors, reported uncorrectable, current pending, offline uncorrectable.
fn is_sector_error_attr(id: u8) -> bool {
    matches!(id, 5 | 187 | 197 | 198)
}

/// Display rows `(name, value, health)`: overall health, temperature, power-on hours, NVMe
/// wear, then one row per SMART attribute (temperature and power-on hours are not repeated).
///
/// Unknown overall health is reported as `Warn`, not `Ok`.
pub fn smart_summary(d: &SmartData) -> Vec<(String, String, Health)> {
    let mut rows = Vec::new();
    rows.push(match d.healthy {
        Some(true) => ("Health".into(), "Passed".into(), Health::Ok),
        Some(false) => ("Health".into(), "Failing".into(), Health::Bad),
        None => ("Health".into(), "Unknown".into(), Health::Warn),
    });
    if let Some(t) = d.temperature_c {
        let h = if t >= TEMP_BAD_C {
            Health::Bad
        } else if t >= TEMP_WARN_C {
            Health::Warn
        } else {
            Health::Ok
        };
        rows.push(("Temperature".into(), format!("{t} °C"), h));
    }
    if let Some(h) = d.power_on_hours {
        rows.push(("Power-on hours".into(), format!("{h} h"), Health::Ok));
    }
    if let Some(p) = d.nvme_percent_used {
        let h = if p >= USED_BAD_PCT {
            Health::Bad
        } else if p >= USED_WARN_PCT {
            Health::Warn
        } else {
            Health::Ok
        };
        rows.push(("Percentage used".into(), format!("{p} %"), h));
    }
    for a in &d.attrs {
        if (a.id == 9 && d.power_on_hours.is_some()) || (a.id == 194 && d.temperature_c.is_some()) {
            continue;
        }
        let h = if is_sector_error_attr(a.id) {
            if a.raw > ERROR_COUNT_BAD {
                Health::Bad
            } else if a.raw > 0 {
                Health::Warn
            } else {
                Health::Ok
            }
        } else if a.id == 199 && a.raw > 0 {
            Health::Warn
        } else {
            Health::Ok
        };
        rows.push((a.name.clone(), a.raw.to_string(), h));
    }
    rows
}
