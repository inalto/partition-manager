//! Shrinking and growing filesystems in place with `ntfsresize` and `e2fsck` + `resize2fs`.
//! The volume must be unmounted; other filesystems are `Unsupported`.

use super::format::ensure_unmounted;
use super::mount::{detect_fs, ready_node};
use crate::posix::cmd;
use pm_core::{Error, FsKind, Result, VolumeId};

/// One external command of a resize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub tool: &'static str,
    pub args: Vec<String>,
    /// Exit codes that still mean success (`e2fsck`: 1 = errors corrected).
    pub ok_codes: &'static [i32],
}

/// The commands that resize the `fs` filesystem on `dev` to `new_len` bytes.
pub fn resize_steps(fs: FsKind, dev: &str, new_len: u64) -> Result<Vec<Step>> {
    match fs {
        FsKind::Ntfs => Ok(vec![Step {
            tool: "ntfsresize",
            args: vec!["--force".into(), "--no-progress-bar".into(), "-s".into(), new_len.to_string(), dev.into()],
            ok_codes: &[0],
        }]),
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 => Ok(vec![
            Step { tool: "e2fsck", args: vec!["-f".into(), "-y".into(), dev.into()], ok_codes: &[0, 1] },
            // the `s` suffix: 512-byte sectors
            Step { tool: "resize2fs", args: vec![dev.into(), format!("{}s", new_len / 512)], ok_codes: &[0] },
        ]),
        other => Err(Error::Unsupported(format!(
            "Linux cannot resize {} filesystems in place",
            other.display_name()
        ))),
    }
}

/// The error for a failed step: an NTFS volume `ntfsresize` refuses because it is marked dirty
/// (scheduled for a consistency check) gets an actionable message.
pub fn step_error(tool: &str, code: Option<i32>, text: &str) -> Error {
    let lower = text.to_ascii_lowercase();
    if tool == "ntfsresize" && (lower.contains("scheduled for check") || lower.contains("dirty")) {
        return Error::Invalid(format!(
            "the NTFS volume is marked dirty (scheduled for a consistency check): run `chkdsk /f`              on it in Windows (or `ntfsfix -d` here) and retry. ntfsresize said: {text}"
        ));
    }
    Error::Platform { code: code.map_or(u32::MAX, |c| c as u32), msg: format!("{tool} failed: {text}") }
}

fn run_step(s: &Step) -> Result<()> {
    let out = cmd::output(s.tool, &s.args)?;
    match out.status.code() {
        Some(c) if s.ok_codes.contains(&c) => Ok(()),
        code => Err(step_error(s.tool, code, &cmd::failure_text(&out))),
    }
}

/// Resize the filesystem of volume `id` to `new_len` bytes (shrink or grow).
/// `start` is where the last enumeration placed the volume (checked against the kernel; the
/// length is not, since growing the partition first is the normal way to extend).
pub fn resize(id: &VolumeId, start: Option<u64>, new_len: u64) -> Result<()> {
    let dev = ready_node(id, start.map(|s| (s, None)))?;
    let dev = dev.as_str();
    ensure_unmounted(dev)?;
    let fs = detect_fs(dev).ok_or_else(|| Error::Unsupported(format!("no recognizable filesystem on {dev}")))?;
    for s in resize_steps(fs, dev, new_len)? {
        run_step(&s)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntfs_and_ext_commands() {
        let s = resize_steps(FsKind::Ntfs, "/dev/sdb2", 10 << 30).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tool, "ntfsresize");
        assert_eq!(s[0].args, ["--force", "--no-progress-bar", "-s", "10737418240", "/dev/sdb2"]);
        let s = resize_steps(FsKind::Ext4, "/dev/sdb3", 1 << 30).unwrap();
        assert_eq!((s[0].tool, s[0].args.join(" ")), ("e2fsck", "-f -y /dev/sdb3".to_string()));
        assert_eq!(s[0].ok_codes, &[0, 1]);
        assert_eq!((s[1].tool, s[1].args.join(" ")), ("resize2fs", "/dev/sdb3 2097152s".to_string()));
        assert!(matches!(resize_steps(FsKind::Xfs, "/dev/x", 1), Err(Error::Unsupported(_))));
        assert!(matches!(resize_steps(FsKind::Fat32, "/dev/x", 1), Err(Error::Unsupported(_))));
    }

    #[test]
    fn dirty_ntfs_gets_a_clear_message() {
        let text = "Volume is scheduled for check.
Run chkdsk /f and please try again, or see option -f.";
        match step_error("ntfsresize", Some(1), text) {
            Error::Invalid(m) => assert!(m.contains("marked dirty") && m.contains("chkdsk /f"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(step_error("resize2fs", Some(1), "dirty"), Error::Platform { code: 1, .. }));
    }

    #[test]
    fn accepted_exit_codes() {
        let ok = Step { tool: "sh", args: vec!["-c".into(), "exit 1".into()], ok_codes: &[0, 1] };
        assert!(run_step(&ok).is_ok());
        let bad = Step { tool: "sh", args: vec!["-c".into(), "echo broken >&2; exit 4".into()], ok_codes: &[0, 1] };
        match run_step(&bad) {
            Err(Error::Platform { code, msg }) => assert_eq!((code, msg.as_str()), (4, "sh failed: broken")),
            other => panic!("{other:?}"),
        }
    }
}
