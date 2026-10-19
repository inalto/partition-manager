//! The `--disk-file` option: check the files and build the [`FilePlatform`] over them.

use crate::cli::{arg_error, CliError};
use pm_image::ImageFormat;
use pm_platform::FilePlatform;
use std::path::PathBuf;

/// A [`FilePlatform`] over `files`, after checking that
/// * they exist and are regular files,
/// * none is given twice (by canonical path), and
/// * each is a raw image: the platform reads and writes the bytes of the file directly, which
///   would corrupt a VHD / VMDK / qcow2 container.
pub fn open_platform(files: Vec<PathBuf>) -> Result<FilePlatform, CliError> {
    let platform = FilePlatform::new(files).map_err(arg_error)?;
    let mut seen: Vec<PathBuf> = Vec::new();
    for f in platform.files() {
        let canonical = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
        if seen.contains(&canonical) {
            return Err(CliError::Usage(format!("--disk-file '{}' is given more than once", f.display())));
        }
        seen.push(canonical);
        let content = pm_image::identify(f)?;
        let by_name = ImageFormat::from_extension(f);
        if content != ImageFormat::Raw || by_name.is_some_and(|e| e != ImageFormat::Raw) {
            return Err(CliError::Usage(format!(
                "'{}' is not a raw image: only raw images can be used as --disk-file; \
                 convert with `pm convert {} <out>.img --format raw`",
                f.display(),
                f.display()
            )));
        }
    }
    Ok(platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_image::CreateOpts;

    fn raw(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![0u8; 4096]).unwrap();
        p
    }

    #[test]
    fn raw_files_are_accepted_and_duplicates_refused() {
        let dir = tempfile::tempdir().unwrap();
        let a = raw(dir.path(), "a.img");
        let b = raw(dir.path(), "b");
        assert!(open_platform(vec![a.clone(), b]).is_ok());
        // the same file spelled two ways
        let alias = dir.path().join(".").join("a.img");
        let r = open_platform(vec![a, alias]);
        assert!(matches!(&r, Err(CliError::Usage(m)) if m.contains("more than once")), "{:?}", r.err());
    }

    #[test]
    fn containers_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let vhd = dir.path().join("d.vhd");
        pm_image::create(&vhd, &CreateOpts { format: ImageFormat::VhdFixed, capacity: 1 << 20, sparse: false }).unwrap();
        let r = open_platform(vec![vhd]);
        assert!(matches!(&r, Err(CliError::Usage(m)) if m.contains("only raw images")), "{:?}", r.err());
        // a raw file with a container extension is refused too
        let fake = raw(dir.path(), "fake.qcow2");
        assert!(open_platform(vec![fake]).is_err());
    }
}
