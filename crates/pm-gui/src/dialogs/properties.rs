//! Disk Properties window: the fields of a `DiskInfo`.

use crate::app::Cmd;
use eframe::egui;
use pm_core::{format_size, DiskBus, DiskInfo, TableKind};

/// `(label, value)` pairs shown for `d`.
pub fn fields(d: &DiskInfo) -> Vec<(&'static str, String)> {
    let yes_no = |b: bool| if b { "Yes" } else { "No" }.to_string();
    let or_dash = |s: &str| if s.is_empty() { "-".to_string() } else { s.to_string() };
    let bus = match d.bus {
        DiskBus::Nvme => "NVMe",
        DiskBus::Sata => "SATA",
        DiskBus::Usb => "USB",
        DiskBus::Scsi => "SCSI",
        DiskBus::Sd => "SD",
        DiskBus::Virtual => "Virtual",
        DiskBus::Unknown => "Unknown",
    };
    let table = match d.table {
        TableKind::Gpt => "GPT",
        TableKind::Mbr => "MBR",
        TableKind::None => "None (uninitialized)",
        TableKind::Unknown => "Unknown",
    };
    vec![
        ("Disk", d.index.to_string()),
        ("Model", or_dash(&d.model)),
        ("Serial number", or_dash(&d.serial)),
        ("Path", or_dash(&d.path)),
        ("Bus", bus.to_string()),
        ("Capacity", format!("{} ({} bytes)", format_size(d.size), d.size)),
        ("Logical sector size", format!("{} bytes", d.sector_size)),
        ("Physical sector size", format!("{} bytes", d.physical_sector_size)),
        ("Partition table", table.to_string()),
        ("Disk ID", or_dash(&d.disk_id)),
        ("Partitions", d.partitions.len().to_string()),
        ("System disk", yes_no(d.flags.system)),
        ("Removable", yes_no(d.flags.removable)),
        ("Offline", yes_no(d.flags.offline)),
        ("Read-only", yes_no(d.flags.read_only)),
        ("TRIM", yes_no(d.flags.trim)),
    ]
}

pub fn show(ctx: &egui::Context, d: &DiskInfo, out: &mut Vec<Cmd>) {
    let mut open = true;
    egui::Window::new(format!("Disk {} Properties", d.index))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("disk_properties").num_columns(2).striped(true).spacing([24.0, 4.0]).show(ui, |ui| {
                for (k, v) in fields(d) {
                    ui.strong(k);
                    ui.label(v);
                    ui.end_row();
                }
            });
        });
    if !open {
        out.push(Cmd::CloseProperties);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::gpt_disk;

    #[test]
    fn fields_cover_the_disk_info() {
        let f = fields(&gpt_disk(1));
        let get = |k: &str| f.iter().find(|(l, _)| *l == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("Model"), "Test SSD");
        assert_eq!(get("Capacity"), "1.00 GB (1073741824 bytes)");
        assert_eq!(get("Partition table"), "GPT");
        assert_eq!(get("Serial number"), "-");
        assert_eq!(get("Partitions"), "3");
        assert_eq!(get("Physical sector size"), "4096 bytes");
    }
}
