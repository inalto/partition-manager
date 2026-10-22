//! `pm doctor`: environment diagnostics. Never fails because of an enumeration error.

use crate::cli::CliError;
use pm_core::DiskInfo;
use pm_platform::Platform;
use std::io::Write;
use std::path::PathBuf;

pub fn run(platform: &dyn Platform, out: &mut dyn Write) -> Result<(), CliError> {
    out.write_all(report(platform).as_bytes())
        .map_err(|e| CliError::Runtime(pm_core::Error::io(0, 0, e)))
}

fn disk_summary(d: &DiskInfo) -> String {
    format!(
        "  {} - {} partition{}",
        d.display_name(),
        d.partitions.len(),
        if d.partitions.len() == 1 { "" } else { "s" }
    )
}

/// The external tools the platform layer of `os` (`Platform::os_name`) runs: mkfs / resize /
/// udev tools on Linux, `diskutil` on macOS, the boot and format tools on Windows; `smartctl`
/// (smartmontools) for SMART data on Linux and macOS.
pub fn tool_names(os: &str) -> &'static [&'static str] {
    match os {
        "linux" => &[
            "mkfs.vfat",
            "mkfs.exfat",
            "mkfs.ntfs",
            "mkfs.ext4",
            "ntfsresize",
            "resize2fs",
            "e2fsck",
            "partprobe",
            "udevadm",
            "smartctl",
        ],
        "macos" => &["/usr/sbin/diskutil", "smartctl"],
        "windows" => &["bcdboot.exe", "bootsect.exe", "mbr2gpt.exe", "format.com", "diskpart.exe"],
        _ => &[],
    }
}

/// Where `name` is found the way the platform layer looks for it (display only).
#[cfg(unix)]
fn locate_tool(name: &str) -> Option<PathBuf> {
    pm_platform::posix::cmd::find_tool(name)
}

/// Where `name` is found the way the platform layer looks for it (display only).
#[cfg(windows)]
fn locate_tool(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("SystemRoot")?).join("System32").join(name);
    p.is_file().then_some(p)
}

#[cfg(not(any(unix, windows)))]
fn locate_tool(_name: &str) -> Option<PathBuf> {
    None
}

/// The `Tools:` block: each tool of `os` with where it was found.
fn tools_report(os: &str, locate: impl Fn(&str) -> Option<PathBuf>) -> String {
    let names = tool_names(os);
    if names.is_empty() {
        return String::new();
    }
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let mut s = String::from("Tools:\n");
    for n in names {
        let at = locate(n).map_or("not found".to_string(), |p| p.display().to_string());
        s.push_str(&format!("  {n:<width$}  {at}\n"));
    }
    s
}

pub fn report(platform: &dyn Platform) -> String {
    let mut s = String::new();
    s.push_str(&format!("pm {}\n", env!("CARGO_PKG_VERSION")));
    let elevated = platform.is_elevated();
    let rights = if elevated {
        "yes".to_string()
    } else {
        format!("no (read-only; {} to write)", platform.elevation_hint())
    };
    s.push_str(&format!("Elevated:     {rights}\n"));
    s.push_str(&format!("Platform:     {}\n", platform.os_name()));
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
    s.push_str(&tools_report(platform.os_name(), locate_tool));
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
        assert!(r.contains("Platform:     none"), "{r}");
        assert!(r.contains("System disk:  unknown"), "{r}");
        assert!(r.contains("Free letters: none"), "{r}");
        assert!(r.contains("Elevated:     no (read-only;"), "{r}");
    }

    #[test]
    fn tools_per_os() {
        assert!(tool_names("linux").contains(&"mkfs.vfat") && tool_names("linux").contains(&"smartctl"));
        assert_eq!(tool_names("macos"), ["/usr/sbin/diskutil", "smartctl"]);
        assert!(tool_names("windows").contains(&"bcdboot.exe"));
        assert!(tool_names("none").is_empty());
        let r = tools_report("macos", |n| (n == "smartctl").then(|| PathBuf::from("/opt/homebrew/bin/smartctl")));
        assert_eq!(r, "Tools:\n  /usr/sbin/diskutil  not found\n  smartctl            /opt/homebrew/bin/smartctl\n");
        assert_eq!(tools_report("none", |_| None), "");
        // the platform's OS decides the list
        let mock = report(&MockPlatform::new());
        assert!(mock.contains("Tools:") && mock.contains(tool_names(std::env::consts::OS)[0]), "{mock}");
    }
}
