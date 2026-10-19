//! `pm table <device> [--json]`: the parsed partition table of a disk, volume or image file.

use crate::cli::{arg_error, CliError};
use crate::device_arg::{open_device, parse_device};
use pm_core::{format_size, TableKind, MIB};
use pm_parttable::{read_table, Table};
use pm_platform::{OpenMode, Platform};
use std::io::Write;

pub fn run(platform: &dyn Platform, device: &str, json: bool, out: &mut dyn Write) -> Result<(), CliError> {
    let arg = parse_device(device).map_err(arg_error)?;
    let mut dev = open_device(platform, &arg, OpenMode::ReadOnly)?;
    let table = read_table(&mut *dev)?;
    let text = if json {
        let mut s = serde_json::to_string_pretty(&table)
            .map_err(|e| CliError::Usage(format!("cannot serialize table: {e}")))?;
        s.push('\n');
        s
    } else {
        render_table(&table)
    };
    out.write_all(text.as_bytes())
        .map_err(|e| CliError::Runtime(pm_core::Error::io(0, 0, e)))
}

fn kind_name(k: TableKind) -> &'static str {
    match k {
        TableKind::Gpt => "GPT",
        TableKind::Mbr => "MBR",
        TableKind::None => "None (no partition table)",
        TableKind::Unknown => "Unknown",
    }
}

/// Readable flag list from the entry attributes (`-` when none).
fn flag_text(kind: TableKind, attrs: u64) -> String {
    let mut f = Vec::new();
    match kind {
        TableKind::Mbr => {
            if attrs & 1 != 0 {
                f.push("active");
            }
        }
        TableKind::Gpt => {
            for (bit, name) in [(0, "required"), (60, "read-only"), (62, "hidden"), (63, "no-letter")] {
                if attrs >> bit & 1 != 0 {
                    f.push(name);
                }
            }
        }
        _ => {}
    }
    if f.is_empty() {
        "-".to_string()
    } else {
        f.join(",")
    }
}

fn bytes_with_size(n: u64) -> String {
    format!("{n} ({})", format_size(n))
}

fn pad_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (w, c) in widths.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let fmt = |cells: Vec<&str>| -> String {
        let mut s = String::new();
        for (i, (c, w)) in cells.iter().zip(&widths).enumerate() {
            if i > 0 {
                s.push_str(" | ");
            }
            s.push_str(c);
            s.push_str(&" ".repeat(w - c.chars().count()));
        }
        s.trim_end().to_string()
    };
    let mut out = fmt(headers.to_vec());
    out.push('\n');
    for r in rows {
        out.push_str(&fmt(r.iter().map(String::as_str).collect()));
        out.push('\n');
    }
    out
}

/// Text output: table summary, entries (`# | Start | Length | Type | Name | Flags`), free ranges.
/// Start and length are exact byte values (human size in brackets for lengths).
pub fn render_table(t: &Table) -> String {
    let mut out = String::new();
    out.push_str(&format!("Table type:  {}\n", kind_name(t.kind)));
    let id = match t.kind {
        TableKind::Gpt => t.disk_guid.map(|g| g.to_string()).unwrap_or_default(),
        TableKind::Mbr => format!("{:08X}", t.mbr_signature),
        _ => String::new(),
    };
    if !id.is_empty() {
        out.push_str(&format!("Disk ID:     {id}\n"));
    }
    out.push_str(&format!("Sector size: {}\n", t.sector_size));
    out.push_str(&format!("Disk size:   {}\n", bytes_with_size(t.disk_len)));
    if t.kind == TableKind::Gpt {
        out.push_str(&format!(
            "Usable area: {} .. {}\n",
            t.gpt_first_usable, t.gpt_last_usable
        ));
    }
    out.push('\n');

    if t.entries.is_empty() {
        out.push_str("No partitions.\n");
    } else {
        let rows: Vec<Vec<String>> = t
            .entries
            .iter()
            .map(|e| {
                let mut ty = e.ptype.name();
                if e.logical {
                    ty.push_str(" [logical]");
                }
                vec![
                    e.number.to_string(),
                    e.start.to_string(),
                    bytes_with_size(e.len),
                    ty,
                    e.name.clone(),
                    flag_text(t.kind, e.attrs),
                ]
            })
            .collect();
        out.push_str(&pad_table(&["#", "Start", "Length", "Type", "Name", "Flags"], &rows));
    }

    let free = t.free_ranges(MIB);
    if !free.is_empty() {
        out.push_str("\nFree ranges:\n");
        let rows: Vec<Vec<String>> = free
            .iter()
            .map(|(s, l)| vec![s.to_string(), bytes_with_size(*l)])
            .collect();
        out.push_str(&pad_table(&["Start", "Length"], &rows));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{gpt_types, PartitionType};
    use pm_parttable::Entry;

    #[test]
    fn renders_gpt_entries_and_free() {
        let mut t = Table::empty(TableKind::Gpt, 64 * MIB, 512);
        t.entries.push(Entry {
            number: 1,
            start: 2 * MIB,
            len: 10 * MIB,
            ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
            guid: None,
            name: "First".into(),
            attrs: 1 << 63,
            logical: false,
        });
        let s = render_table(&t);
        assert!(s.contains("Table type:  GPT"), "{s}");
        assert!(s.contains("Sector size: 512"), "{s}");
        assert!(s.contains("# | Start"), "{s}");
        assert!(s.contains(&(2 * MIB).to_string()) && s.contains("First") && s.contains("no-letter"), "{s}");
        assert!(s.contains("Free ranges:"), "{s}");
    }

    #[test]
    fn renders_empty_table() {
        let s = render_table(&Table::empty(TableKind::None, 8 * MIB, 512));
        assert!(s.contains("No partitions."), "{s}");
        assert!(!s.contains("Disk ID"), "{s}");
    }

    #[test]
    fn flags_text() {
        assert_eq!(flag_text(TableKind::Mbr, 1), "active");
        assert_eq!(flag_text(TableKind::Mbr, 0), "-");
        assert_eq!(flag_text(TableKind::Gpt, 1 | 1 << 62), "required,hidden");
    }
}
