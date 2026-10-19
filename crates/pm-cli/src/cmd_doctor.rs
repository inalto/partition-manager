//! `pm doctor`: environment diagnostics. Never fails because of an enumeration error.

use crate::cli::CliError;
use pm_core::DiskInfo;
use pm_platform::Platform;
use std::io::Write;

pub fn run(platform: &dyn Platform, out: &mut dyn Write) -> Result<(), CliError> {
    out.write_all(report(platform).as_bytes())
        .map_err(|e| CliError::Runtime(pm_core::Error::io(0, 0, e)))
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn disk_summary(d: &DiskInfo) -> String {
    format!(
        "  {} - {} partition{}",
        d.display_name(),
        d.partitions.len(),
        if d.partitions.len() == 1 { "" } else { "s" }
    )
}

pub fn report(platform: &dyn Platform) -> String {
    let mut s = String::new();
    s.push_str(&format!("pm {}\n", env!("CARGO_PKG_VERSION")));
    s.push_str(&format!("Elevated:     {}\n", yes_no(platform.is_elevated())));
    s.push_str(&format!(
        "Platform:     {}\n",
        if cfg!(windows) { "windows" } else { "other (unsupported)" }
    ));
    s.push_str(&format!(
        "Toolchain:    {} ({} {})\n",
        option_env!("RUSTUP_TOOLCHAIN").unwrap_or("unknown"),
        std::env::consts::ARCH,
        std::env::consts::OS
    ));
    match platform.enumerate_disks() {
        Ok(disks) => {
            s.push_str(&format!("Disks:        {}\n", disks.len()));
            for d in &disks {
                s.push_str(&disk_summary(d));
                s.push('\n');
            }
        }
        Err(e) => {
            log::debug!("doctor: enumeration failed: {e}");
            s.push_str(&format!("Disks:        enumeration failed: {e}\n"));
        }
    }
    match platform.system_disk() {
        Some(pm_platform::NO_SYSTEM_DISK) => s.push_str("System disk:  none (image files)\n"),
        Some(i) => s.push_str(&format!("System disk:  {i}\n")),
        None => s.push_str("System disk:  unknown\n"),
    }
    let letters = platform.free_letters();
    if letters.is_empty() {
        s.push_str("Free letters: none\n");
    } else {
        let l: Vec<String> = letters.iter().map(|c| format!("{c}:")).collect();
        s.push_str(&format!("Free letters: {}\n", l.join(" ")));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::MemDevice;
    use pm_platform::{MockPlatform, NullPlatform};

    #[test]
    fn reports_mock_disks() {
        let p = MockPlatform::new();
        p.add_disk(
            3,
            MemDevice::new(8 << 20),
            DiskInfo {
                model: "Mock".into(),
                ..Default::default()
            },
        );
        p.set_system_disk(3);
        let r = report(&p);
        assert!(r.contains("Disks:        1"), "{r}");
        assert!(r.contains("Mock"), "{r}");
        assert!(r.contains("System disk:  3"), "{r}");
        assert!(r.contains("Free letters: E: F: G:"), "{r}");
        assert!(r.contains("Elevated:"), "{r}");
    }

    #[test]
    fn enumeration_error_does_not_abort() {
        let r = report(&NullPlatform);
        assert!(r.contains("enumeration failed"), "{r}");
        assert!(r.contains("System disk:  unknown"), "{r}");
        assert!(r.contains("Free letters: none"), "{r}");
    }
}
