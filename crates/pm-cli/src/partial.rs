//! Write-then-rename output files: an image is created at `<out>.partial` and only moved to its
//! final name once complete, so a failed or cancelled run never leaves a half-written image
//! (or clobbers an existing good one).

use crate::cli::CliError;
use pm_core::Error;
use pm_image::ImageFormat;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A local file system failure, keeping the path in the message.
pub fn path_err(path: &Path, e: std::io::Error) -> CliError {
    CliError::Runtime(Error::Platform {
        code: e.raw_os_error().unwrap_or(0) as u32,
        msg: format!("{}: {e}", path.display()),
    })
}

/// `<stem>-flat.vmdk` next to descriptor `p`: the extent file pm-image writes for `VmdkFlat`.
fn flat_extent(p: &Path) -> Option<PathBuf> {
    let stem = p.file_stem()?.to_str()?;
    Some(p.with_file_name(format!("{stem}-flat.vmdk")))
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub struct Partial {
    target: PathBuf,
    partial: PathBuf,
    fmt: ImageFormat,
}

impl Partial {
    pub fn new(target: &Path, fmt: ImageFormat) -> Self {
        let mut s: OsString = target.as_os_str().to_owned();
        s.push(".partial");
        Partial { target: target.to_path_buf(), partial: PathBuf::from(s), fmt }
    }

    /// Where the image is written until [`Partial::commit`].
    pub fn path(&self) -> &Path {
        &self.partial
    }

    /// Every file this output can create or replace (final and partial names, and the flat
    /// extents of a `VmdkFlat` image), for checking against the files being read.
    pub fn files_written(&self) -> Vec<PathBuf> {
        let mut v = vec![self.target.clone(), self.partial.clone()];
        if self.fmt == ImageFormat::VmdkFlat {
            v.extend(flat_extent(&self.target));
            v.extend(flat_extent(&self.partial));
        }
        v
    }

    /// Move the finished image to its final name (a flat VMDK's extent and descriptor too),
    /// replacing any previous file. On failure the partial files are removed.
    pub fn commit(&self) -> Result<(), CliError> {
        let r = self.try_commit();
        if r.is_err() {
            self.discard();
        }
        r
    }

    fn try_commit(&self) -> Result<(), CliError> {
        if self.fmt == ImageFormat::VmdkFlat {
            let (Some(from), Some(to)) = (flat_extent(&self.partial), flat_extent(&self.target)) else {
                return Err(CliError::Usage(format!("bad image path {}", self.target.display())));
            };
            // the descriptor names its extent: point it at the final extent name
            let text = std::fs::read_to_string(&self.partial).map_err(|e| path_err(&self.partial, e))?;
            let patched = text.replace(&format!("\"{}\"", file_name(&from)), &format!("\"{}\"", file_name(&to)));
            std::fs::write(&self.partial, patched).map_err(|e| path_err(&self.partial, e))?;
            std::fs::rename(&from, &to).map_err(|e| path_err(&to, e))?;
        }
        std::fs::rename(&self.partial, &self.target).map_err(|e| path_err(&self.target, e))
    }

    /// Remove the partial files (best effort).
    pub fn discard(&self) {
        let _ = std::fs::remove_file(&self.partial);
        if self.fmt == ImageFormat::VmdkFlat {
            if let Some(e) = flat_extent(&self.partial) {
                let _ = std::fs::remove_file(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let p = Partial::new(Path::new("d/x.vmdk"), ImageFormat::VmdkFlat);
        assert_eq!(p.path(), Path::new("d/x.vmdk.partial"));
        let files = p.files_written();
        assert!(files.contains(&PathBuf::from("d/x-flat.vmdk")), "{files:?}");
        assert!(files.contains(&PathBuf::from("d/x.vmdk-flat.vmdk")), "{files:?}");
        let q = Partial::new(Path::new("a.qcow2"), ImageFormat::Qcow2);
        assert_eq!(q.files_written().len(), 2);
    }
}
