//! `pm probe <device> [--offset N]`: identify the filesystem on a device.

use crate::cli::CliError;
use crate::device_arg::open_spec;
use pm_core::{format_size, Error};
use pm_platform::{OpenMode, Platform};
use std::io::Write;

pub fn run(platform: &dyn Platform, device: &str, offset: u64, out: &mut dyn Write) -> Result<(), CliError> {
    let mut dev = open_spec(platform, device, OpenMode::ReadOnly)?.dev;
    let text = match pm_fs::detect(&mut *dev, offset) {
        None => "no filesystem detected\n".to_string(),
        Some(p) => {
            let mut s = format!("Filesystem:   {}\n", p.kind.display_name());
            if !p.label.is_empty() {
                s.push_str(&format!("Label:        {}\n", p.label));
            }
            if p.total > 0 {
                s.push_str(&format!("Size:         {} bytes ({})\n", p.total, format_size(p.total)));
            }
            if let Some(used) = p.used {
                s.push_str(&format!("Used:         {used} bytes ({})\n", format_size(used)));
            }
            if let Some(c) = p.cluster {
                s.push_str(&format!("Cluster size: {c}\n"));
            }
            if !p.uuid.is_empty() {
                s.push_str(&format!("UUID:         {}\n", p.uuid));
            }
            s
        }
    };
    out.write_all(text.as_bytes()).map_err(|e| CliError::Runtime(Error::io(0, 0, e)))
}
