//! Virtual disk images (raw, VHD fixed/dynamic, VMDK flat/sparse, qcow2 v3) exposed as
//! [`BlockDevice`]s whose `len()` is the virtual disk capacity, plus format
//! identification and conversion.

mod format;
mod qcow2;
mod raw;
pub mod rescue;
mod sparse;
mod vhd;
mod vmdk;

pub use format::ImageFormat;
pub use rescue::{copy_simple, rescue, BlockStatus, MapFile, MapStats, RescueOpts, RescueStats};
use pm_core::{BlockDevice, CancelToken, Error, ProgressSink, ProgressTracker, Result};
use std::path::{Component, Path, PathBuf};

/// Options for [`create`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateOpts {
    pub format: ImageFormat,
    /// Virtual disk size in bytes; must be a non-zero multiple of 512.
    pub capacity: u64,
    /// Raw images only: request a sparse file (currently ignored).
    pub sparse: bool,
}

const CONVERT_CHUNK: usize = 1 << 20;

/// Create a new image (truncating any existing file) and open it read-write.
pub fn create(path: &Path, opts: &CreateOpts) -> Result<Box<dyn BlockDevice>> {
    let cap = opts.capacity;
    if cap == 0 || cap % 512 != 0 {
        return Err(Error::Invalid(format!(
            "image capacity must be a non-zero multiple of 512 bytes (got {cap})"
        )));
    }
    match opts.format {
        ImageFormat::Raw => raw::create(path, cap),
        ImageFormat::VhdFixed => vhd::create_fixed(path, cap),
        ImageFormat::VhdDynamic => vhd::create_dynamic(path, cap),
        ImageFormat::VmdkFlat => vmdk::create_flat(path, cap),
        ImageFormat::VmdkSparse => vmdk::create_sparse(path, cap),
        ImageFormat::Qcow2 => qcow2::create(path, cap),
    }
}

/// Open an existing image of any supported format (detected by content).
pub fn open(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    match format::sniff(path)? {
        ImageFormat::Raw => raw::open(path, read_only),
        ImageFormat::VhdFixed | ImageFormat::VhdDynamic => vhd::open(path, read_only),
        ImageFormat::VmdkFlat => vmdk::open_flat(path, read_only),
        ImageFormat::VmdkSparse => vmdk::open_sparse(path, read_only),
        ImageFormat::Qcow2 => qcow2::open(path, read_only),
    }
}

/// Detect the format of an image file without opening it for I/O.
pub fn identify(path: &Path) -> Result<ImageFormat> {
    format::sniff(path)
}

/// Copy the image `src` (any format) into a new image `dst` of format `fmt` with the
/// same capacity, in 1 MiB chunks. All-zero chunks are not written to sparse formats so
/// they stay unallocated. An existing destination is overwritten (truncated), but a
/// destination that is the source itself — directly, through another spelling of the
/// path, or through a flat VMDK extent on either side — is refused with
/// [`Error::Invalid`] before anything is created. On cancellation the partial
/// destination is left in place.
/// Known limitation: hard links (and aliases canonicalization cannot see) are not detected.
pub fn convert(
    src: &Path,
    dst: &Path,
    fmt: ImageFormat,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<()> {
    cancel.check()?;
    refuse_same_file(src, dst, fmt)?;
    let mut input = open(src, true)?;
    let capacity = input.len();
    let mut output = create(
        dst,
        &CreateOpts {
            format: fmt,
            capacity,
            sparse: true,
        },
    )?;
    let skip_zero = fmt.is_sparse();
    let mut tracker = ProgressTracker::new(progress, "Converting", capacity);
    let mut buf = vec![0u8; CONVERT_CHUNK];
    let mut off = 0u64;
    while off < capacity {
        cancel.check()?;
        let n = (capacity - off).min(CONVERT_CHUNK as u64) as usize;
        input.read_at(off, &mut buf[..n])?;
        if !(skip_zero && buf[..n].iter().all(|&b| b == 0)) {
            output.write_at(off, &buf[..n])?;
        }
        off += n as u64;
        tracker.add(n as u64);
    }
    output.flush()?;
    tracker.finish();
    Ok(())
}

/// Refuse a conversion whose destination files (including a VmdkFlat destination's
/// `<stem>-flat.vmdk`) overlap the files the source is read from (including a flat
/// descriptor's extent): `create` would truncate the source before it is read.
fn refuse_same_file(src: &Path, dst: &Path, fmt: ImageFormat) -> Result<()> {
    let mut targets = vec![file_identity(dst)?];
    if fmt == ImageFormat::VmdkFlat {
        targets.push(file_identity(&vmdk::flat_sibling(dst)?)?);
    }
    let clash = |source: &Path| -> Result<()> {
        if targets.iter().any(|t| same_path(source, t)) {
            return Err(Error::Invalid(
                "destination is the same file as the source".into(),
            ));
        }
        Ok(())
    };
    // Compare the source path before reading it, then its flat extent if any.
    clash(&file_identity(src)?)?;
    if format::sniff(src)? == ImageFormat::VmdkFlat {
        clash(&file_identity(&vmdk::flat_extent_path(src)?)?)?;
    }
    Ok(())
}

/// Canonical identity of a path that may not exist yet. The longest existing ancestor
/// is canonicalized and the remaining components are applied lexically (`.` dropped,
/// `..` pops, even past that ancestor), which is how Windows itself resolves `..`.
/// Fails closed: a path that exists but cannot be canonicalized is an error rather
/// than being compared by its raw spelling.
fn file_identity(p: &Path) -> Result<PathBuf> {
    let fail = || Error::Invalid(format!("cannot resolve path {}", p.display()));
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_err(|_| fail())?.join(p)
    };
    let comps: Vec<Component> = abs.components().collect();
    for k in (1..=comps.len()).rev() {
        let prefix: PathBuf = comps[..k].iter().collect();
        if matches!(prefix.try_exists(), Ok(false)) {
            continue;
        }
        // exists (or existence is unknowable): it must canonicalize
        let mut base = std::fs::canonicalize(&prefix).map_err(|_| fail())?;
        for c in &comps[k..] {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    base.pop();
                }
                Component::Normal(name) => base.push(name),
                Component::RootDir | Component::Prefix(_) => return Err(fail()),
            }
        }
        return Ok(base);
    }
    Err(fail())
}

/// Path equality; case-insensitive on Windows, where file names are.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_identity_resolves_missing_ancestors_lexically() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(dir.path().join("f.img"), b"x").unwrap();
        // existing file
        assert_eq!(file_identity(&dir.path().join("f.img")).unwrap(), base.join("f.img"));
        // missing directories, `.` and `..` applied after the existing ancestor
        let p = dir.path().join("a").join("b").join("..").join(".").join("c");
        assert_eq!(file_identity(&p).unwrap(), base.join("a").join("c"));
        let p = dir.path().join("missing").join("..").join("f.img");
        assert_eq!(file_identity(&p).unwrap(), base.join("f.img"));
        // `..` popping past the existing ancestor applies to the ancestor itself
        let p = dir.path().join("m").join("..").join("..").join("z");
        assert_eq!(file_identity(&p).unwrap(), base.parent().unwrap().join("z"));
        // relative paths resolve against the current directory
        assert_eq!(
            file_identity(Path::new("Cargo.toml")).unwrap(),
            std::fs::canonicalize("Cargo.toml").unwrap()
        );
    }
}
