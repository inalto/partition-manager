//! S.M.A.R.T. data from `smartctl -j -a <device>` (smartmontools 7+ JSON output).
//!
//! NVMe drives report `nvme_smart_health_information_log`; ATA drives report
//! `ata_smart_attributes.table[]`. The overall verdict is `smart_status.passed`. smartctl's
//! exit status is a bit mask that is often non-zero on healthy drives (e.g. "error log has
//! entries"), so callers parse stdout whatever the status and rely on [`parse`] returning
//! `None` when no SMART data came back (unsupported device, USB bridge without SAT, ...).

use crate::traits::{SmartAttr, SmartData};
use serde_json::Value;

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_u64()
}

fn sat_u8(n: Option<u64>) -> u8 {
    n.map_or(0, |n| n.min(u8::MAX as u64) as u8)
}

fn nvme_attr(id: u8, name: &str, raw: Option<u64>) -> Option<SmartAttr> {
    Some(SmartAttr { id, name: name.to_string(), raw: raw?, ..Default::default() })
}

/// NVMe health log fields as attributes, with the ids and names the Windows parser uses (the
/// byte offset of the field in log page 02h).
fn nvme_attrs(log: &Value) -> Vec<SmartAttr> {
    let f = |k: &str| log.get(k).and_then(Value::as_u64);
    let spare = f("available_spare");
    let mut v: Vec<SmartAttr> = [
        nvme_attr(0, "Critical warning", f("critical_warning")),
        spare.map(|s| SmartAttr {
            id: 3,
            name: "Available spare".into(),
            value: sat_u8(Some(s)),
            worst: sat_u8(Some(s)),
            threshold: sat_u8(f("available_spare_threshold")),
            raw: s,
        }),
        nvme_attr(32, "Data units read", f("data_units_read")),
        nvme_attr(48, "Data units written", f("data_units_written")),
        nvme_attr(112, "Power cycles", f("power_cycles")),
        nvme_attr(128, "Power-on hours", f("power_on_hours")),
        nvme_attr(144, "Unsafe shutdowns", f("unsafe_shutdowns")),
        nvme_attr(160, "Media and data integrity errors", f("media_errors")),
    ]
    .into_iter()
    .flatten()
    .collect();
    v.sort_by_key(|a| a.id);
    v
}

fn ata_attrs(table: &[Value]) -> Vec<SmartAttr> {
    table
        .iter()
        .filter_map(|a| {
            let id = u8::try_from(a.get("id")?.as_u64()?).ok()?;
            Some(SmartAttr {
                id,
                name: a
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|n| n.replace('_', " "))
                    .unwrap_or_else(|| format!("Attribute {id}")),
                value: sat_u8(u64_at(a, &["value"])),
                worst: sat_u8(u64_at(a, &["worst"])),
                threshold: sat_u8(u64_at(a, &["thresh"])),
                raw: u64_at(a, &["raw", "value"]).unwrap_or(0),
            })
        })
        .collect()
}

/// Parse `smartctl -j -a` output. `None` when the JSON is invalid or carries no SMART data
/// at all (no health verdict, no NVMe log, no ATA attribute table).
pub fn parse(json: &str) -> Option<SmartData> {
    let v: Value = serde_json::from_str(json).ok()?;
    let passed = v.get("smart_status").and_then(|s| s.get("passed")).and_then(Value::as_bool);
    let nvme = v.get("nvme_smart_health_information_log").filter(|l| l.is_object());
    let ata = v
        .get("ata_smart_attributes")
        .and_then(|a| a.get("table"))
        .and_then(Value::as_array)
        .filter(|t| !t.is_empty());
    if passed.is_none() && nvme.is_none() && ata.is_none() {
        return None;
    }

    let mut data = SmartData { healthy: passed, ..Default::default() };
    if let Some(log) = nvme {
        data.attrs = nvme_attrs(log);
        data.nvme_percent_used = log.get("percentage_used").and_then(Value::as_u64).map(|p| sat_u8(Some(p)));
        data.temperature_c = u64_at(log, &["temperature"]).map(|t| t as u32);
        data.power_on_hours = u64_at(log, &["power_on_hours"]);
        if data.healthy.is_none() {
            data.healthy = u64_at(log, &["critical_warning"]).map(|c| c == 0);
        }
    }
    if let Some(table) = ata {
        data.attrs = ata_attrs(table);
        let raw = |id: u8| data.attrs.iter().find(|a| a.id == id).map(|a| a.raw);
        data.power_on_hours = raw(9).map(|r| r & 0xFFFF_FFFF);
        data.temperature_c = raw(194).or_else(|| raw(190)).map(|r| (r & 0xFF) as u32);
    }
    // smartctl's own summary wins when present (it decodes vendor-specific raw formats)
    if let Some(t) = u64_at(&v, &["temperature", "current"]) {
        data.temperature_c = Some(t as u32);
    }
    if let Some(h) = u64_at(&v, &["power_on_time", "hours"]) {
        data.power_on_hours = Some(h);
    }
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVME: &str = include_str!("../../tests/fixtures/smartctl-nvme.json");
    const ATA: &str = include_str!("../../tests/fixtures/smartctl-ata.json");

    #[test]
    fn nvme_health_log() {
        let d = parse(NVME).unwrap();
        assert_eq!(d.healthy, Some(true));
        assert_eq!(d.temperature_c, Some(41));
        assert_eq!(d.power_on_hours, Some(6543));
        assert_eq!(d.nvme_percent_used, Some(3));
        let by = |n: &str| d.attrs.iter().find(|a| a.name == n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(by("Critical warning").raw, 0);
        assert_eq!(by("Media and data integrity errors").raw, 0);
        assert_eq!(by("Unsafe shutdowns").raw, 42);
        assert_eq!(by("Data units read").raw, 31_415_926);
        let spare = by("Available spare");
        assert_eq!((spare.value, spare.threshold, spare.raw), (100, 10, 100));
        assert_eq!(d.attrs.len(), 8);
    }

    #[test]
    fn ata_attribute_table() {
        let d = parse(ATA).unwrap();
        assert_eq!(d.healthy, Some(false), "smart_status.passed = false");
        assert_eq!(d.temperature_c, Some(33));
        assert_eq!(d.power_on_hours, Some(21734));
        assert_eq!(d.nvme_percent_used, None);
        assert_eq!(d.attrs.len(), 5);
        let realloc = d.attrs.iter().find(|a| a.id == 5).unwrap();
        assert_eq!(realloc.name, "Reallocated Sector Ct");
        assert_eq!((realloc.value, realloc.worst, realloc.threshold, realloc.raw), (140, 140, 140, 1832));
        let temp = d.attrs.iter().find(|a| a.id == 194).unwrap();
        assert_eq!(temp.raw & 0xFF, 33);
    }

    #[test]
    fn summary_fields_fall_back_to_the_raw_attributes() {
        // no top-level temperature / power_on_time: taken from attributes 194 and 9
        let mut v: Value = serde_json::from_str(ATA).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("temperature");
        o.remove("power_on_time");
        o.remove("smart_status");
        let d = parse(&v.to_string()).unwrap();
        assert_eq!(d.temperature_c, Some(33));
        assert_eq!(d.power_on_hours, Some(21734));
        assert_eq!(d.healthy, None);
        // NVMe without smart_status: healthy from critical_warning
        let mut v: Value = serde_json::from_str(NVME).unwrap();
        v.as_object_mut().unwrap().remove("smart_status");
        v["nvme_smart_health_information_log"]["critical_warning"] = 4.into();
        assert_eq!(parse(&v.to_string()).unwrap().healthy, Some(false));
    }

    #[test]
    fn no_smart_data_is_none() {
        assert!(parse("not json").is_none());
        // what smartctl prints for a loop device
        let none = r#"{"json_format_version":[1,0],"smartctl":{"version":[7,4],"exit_status":2,
            "messages":[{"string":"/dev/loop0: Unable to detect device type","severity":"error"}]},
            "device":{"name":"/dev/loop0"}}"#;
        assert!(parse(none).is_none());
        assert!(parse(r#"{"ata_smart_attributes":{"table":[]}}"#).is_none());
    }
}
