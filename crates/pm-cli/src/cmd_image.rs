//! `pm image`, `pm restore` and `pm convert`: save a device to an image file, write an image
//! back to a device, and convert between image formats.

use crate::cli::{arg_error, CliError, CopyMode, CopyModeArgs};
use crate::cmd_apply::{run_ops, Env, RunFlags};
use crate::cmd_clone::{
    copy_device, copy_planned, existing_canonical, finish, open_checked, plan_used_map, summary, SharedSource,
};
use crate::device_arg::{open_spec, parse_device, vmdk_extent_files, DeviceArg};
use crate::partial::Partial;
use crate::progress::StderrProgress;
use pm_core::{align_up, format_size, BlockDevice, CancelToken, Error};
use pm_image::{CreateOpts, ImageFormat};
use pm_ops::{Operation, RescueSettings};
use pm_platform::{OpenMode, Platform};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn write_err(e: std::io::Error) -> CliError {
    CliError::Runtime(Error::io(0, 0, e))
}

/// The explicit format, else the one the file extension implies.
pub(crate) fn pick_format(explicit: Option<ImageFormat>, file: &Path) -> Result<ImageFormat, CliError> {
    explicit.or_else(|| ImageFormat::from_extension(file)).ok_or_else(|| {
        CliError::Usage(format!(
            "cannot tell the image format of '{}' from its extension; pass --format raw|vhd|vhd-fixed|vmdk|vmdk-flat|qcow2",
            file.display()
        ))
    })
}

/// Refuse when any file the output would create or replace is one of the files `read_from`
/// is being read from (the output is written before the source is fully read).
fn refuse_source_overwrite(read_from: &[PathBuf], written: &[PathBuf]) -> Result<(), CliError> {
    for w in written {
        if let Some(c) = existing_canonical(w) {
            if read_from.contains(&c) {
                return Err(CliError::Usage(format!(
                    "'{}' is (part of) the source itself; refusing to overwrite it",
                    w.display()
                )));
            }
        }
    }
    Ok(())
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
    let partial = Partial::new(file, fmt);
    refuse_source_overwrite(&source.source_files(), &partial.files_written())?;

    // Everything that can be refused is refused before any output file is touched.
    let mut shared = SharedSource::new(source.dev);
    let used = plan_used_map(&shared, mode)?;

    let start = Instant::now();
    let written = (|| -> Result<u64, CliError> {
        let mut dev = pm_image::create(
            partial.path(),
            &CreateOpts {
                format: fmt,
                capacity: align_up(len, 512),
                sparse: true,
            },
        )?;
        copy_planned(&mut shared, used.as_ref(), &mut *dev, 0, cancel)
        // `dev` is closed here, before the rename
    })();
    let copied = match written {
        Ok(n) => {
            partial.commit()?;
            n
        }
        Err(e) => {
            partial.discard();
            return Err(e);
        }
    };
    writeln!(out, "{}", summary("Imaged", copied, start.elapsed(), used.is_some()))
        .and_then(|_| writeln!(out, "Wrote {} ({})", file.display(), fmt.display()))
        .map_err(write_err)
}

/// `pm image SRC FILE --rescue`: a `disk:N` / `disk:N:P` source runs the `image-disk` /
/// `image-partition` operation (validated like every operation, and run right away as `pm
/// image` always is); any other source (an image file, a volume) goes through the same export
/// ([`pm_ops::export_rescue_image`]) directly.
pub fn run_image_rescue(
    env: &Env,
    src: &str,
    file: &Path,
    format: Option<ImageFormat>,
    mode: CopyModeArgs,
    settings: RescueSettings,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let fmt = pick_format(format, file)?;
    let clone_mode = crate::cmd_ops::clone_mode(mode);
    let op = match parse_device(src).map_err(arg_error)? {
        DeviceArg::Disk(disk) => Some(Operation::ImageDisk {
            disk,
            path: file.to_path_buf(),
            format: fmt,
            mode: clone_mode,
            rescue: Some(settings.clone()),
        }),
        DeviceArg::Partition(parent, number) => match *parent {
            DeviceArg::Disk(disk) => Some(Operation::ImagePartition {
                disk,
                number,
                path: file.to_path_buf(),
                format: fmt,
                mode: clone_mode,
                rescue: Some(settings.clone()),
            }),
            _ => None,
        },
        _ => None,
    };
    if let Some(op) = op {
        let flags = RunFlags { dry_run: false, yes: true, allow_system_disk: false };
        return run_ops(env, vec![op], &flags, out);
    }

    let source = open_spec(env.platform, src, OpenMode::ReadOnly)?;
    if source.dev.len() == 0 {
        return Err(CliError::Usage("the source is empty; nothing to image".into()));
    }
    let mut written = Partial::new(file, fmt).files_written();
    written.push(pm_ops::mapfile_path(file, &settings));
    refuse_source_overwrite(&source.source_files(), &written)?;
    let mut shared = SharedSource::new(source.dev);
    let used = plan_used_map(&shared, mode.mode())?;

    let start = Instant::now();
    let mut notes = Vec::new();
    let r = pm_ops::export_rescue_image(
        &mut shared,
        used,
        file,
        fmt,
        &settings,
        &StderrProgress::new(),
        env.cancel,
        &mut notes,
    );
    for n in &notes {
        writeln!(out, "{n}").map_err(write_err)?;
    }
    let stats = r.map_err(|e| match e {
        Error::Invalid(m) => CliError::Usage(m),
        other => CliError::Runtime(other),
    })?;
    writeln!(
        out,
        "Imaged {} in {:.1}s (rescue mode); wrote {} ({})",
        format_size(stats.rescued),
        start.elapsed().as_secs_f64(),
        file.display(),
        fmt.display()
    )
    .map_err(write_err)?;
    if stats.bad_bytes > 0 {
        writeln!(
            out,
            "Unreadable data: {} in {} area(s) is zero in the image; run the same command again (more retries with -r) to retry it",
            format_size(stats.bad_bytes),
            stats.bad_areas
        )
        .map_err(write_err)?;
    }
    Ok(())
}

pub fn run_restore(
    platform: &dyn Platform,
    file: &str,
    dst: &str,
    dst_offset: u64,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let (source, target, session) = open_checked(platform, file, dst, dst_offset)?;
    let start = Instant::now();
    let (copied, smart) = copy_device(source, target, dst_offset, CopyMode::Sector, cancel)?;
    finish(session)?;
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
    let canonical = std::fs::canonicalize(input).map_err(write_err)?;
    let mut read_from = vmdk_extent_files(input);
    read_from.push(canonical);
    let partial = Partial::new(output, fmt);
    refuse_source_overwrite(&read_from, &partial.files_written())?;

    let start = Instant::now();
    match pm_image::convert(input, partial.path(), fmt, &StderrProgress::new(), cancel) {
        Ok(()) => partial.commit()?,
        Err(e) => {
            partial.discard();
            return Err(e.into());
        }
    }
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
