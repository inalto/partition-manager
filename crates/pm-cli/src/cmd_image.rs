//! `pm image`, `pm restore` and `pm convert`: save a device to an image file, write an image
//! back to a device, and convert between image formats.

use crate::cli::{CliError, CopyMode};
use crate::cmd_clone::{copy_device, existing_canonical, summary};
use crate::device_arg::{open_spec, Identity, Opened};
use crate::progress::StderrProgress;
use pm_core::{align_up, CancelToken, Error};
use pm_image::{CreateOpts, ImageFormat};
use pm_platform::{OpenMode, Platform};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

fn write_err(e: std::io::Error) -> CliError {
    CliError::Runtime(Error::io(0, 0, e))
}

/// The explicit format, else the one the file extension implies.
fn pick_format(explicit: Option<ImageFormat>, file: &Path) -> Result<ImageFormat, CliError> {
    explicit.or_else(|| ImageFormat::from_extension(file)).ok_or_else(|| {
        CliError::Usage(format!(
            "cannot tell the image format of '{}' from its extension; pass --format raw|vhd|vhd-fixed|vmdk|vmdk-flat|qcow2",
            file.display()
        ))
    })
}

/// Creating `file` truncates it, so refuse before that when it is the file `src` is read from.
fn refuse_source_overwrite(src: &Identity, file: &Path) -> Result<(), CliError> {
    match (existing_canonical(file), src) {
        (Some(c), Identity::File(s)) if &c == s => Err(CliError::Usage(format!(
            "'{}' is the source itself; refusing to overwrite it",
            file.display()
        ))),
        _ => Ok(()),
    }
}

pub fn run_image(
    platform: &dyn Platform,
    src: &str,
    file: &Path,
    format: Option<ImageFormat>,
    mode: CopyMode,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let fmt = pick_format(format, file)?;
    let source = open_spec(platform, src, OpenMode::ReadOnly)?;
    let len = source.dev.len();
    if len == 0 {
        return Err(CliError::Usage("the source is empty; nothing to image".into()));
    }
    refuse_source_overwrite(&source.identity, file)?;

    let start = Instant::now();
    let dev = pm_image::create(
        file,
        &CreateOpts {
            format: fmt,
            capacity: align_up(len, 512),
            sparse: true,
        },
    )?;
    let identity = Identity::File(std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf()));
    let range = (0, dev.len());
    let target = Opened { dev, identity, range };
    let (copied, smart) = copy_device(source, target, 0, mode, cancel)?;
    writeln!(out, "{}", summary("Imaged", copied, start.elapsed(), smart))
        .and_then(|_| writeln!(out, "Wrote {} ({})", file.display(), fmt.display()))
        .map_err(write_err)
}

pub fn run_restore(
    platform: &dyn Platform,
    file: &str,
    dst: &str,
    dst_offset: u64,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let source = open_spec(platform, file, OpenMode::ReadOnly)?;
    let target = open_spec(platform, dst, OpenMode::ReadWrite)?;
    let start = Instant::now();
    let (copied, smart) = copy_device(source, target, dst_offset, CopyMode::Sector, cancel)?;
    writeln!(out, "{}", summary("Restored", copied, start.elapsed(), smart)).map_err(write_err)
}

pub fn run_convert(
    input: &Path,
    output: &Path,
    format: Option<ImageFormat>,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let fmt = pick_format(format, output)?;
    if !input.is_file() {
        return Err(CliError::Usage(format!("image file '{}' does not exist", input.display())));
    }
    refuse_source_overwrite(&Identity::File(std::fs::canonicalize(input).map_err(|e| {
        CliError::Runtime(Error::io(0, 0, e))
    })?), output)?;
    let start = Instant::now();
    pm_image::convert(input, output, fmt, &StderrProgress::new(), cancel)?;
    writeln!(
        out,
        "Converted {} to {} ({}) in {:.1}s",
        input.display(),
        output.display(),
        fmt.display(),
        start.elapsed().as_secs_f64()
    )
    .map_err(write_err)
}
