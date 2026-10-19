//! `pm recover partitions|files|carve|open-session`: find lost partitions, list and restore
//! deleted files, carve files out of raw data, and show a saved scan session.
//!
//! Scanning only reads. The one command that writes is `partitions --apply --yes`, which adds
//! the chosen candidates to the partition table (to free space only; existing partitions are
//! never touched).

use crate::cli::{arg_error, CliError};
use crate::cmd_ops::TableArg;
use crate::device_arg::{open_spec, parse_device, DeviceArg, Identity, Opened};
use crate::progress::StderrProgress;
use clap::Subcommand;
use pm_core::{format_size, CancelToken, DiskInfo, Error, TableKind};
use pm_ops::Model;
use pm_parttable::{read_table, write_table, Candidate, Table};
use pm_platform::{OpenMode, Platform};
use pm_recover::{
    apply_candidates, carve, deleted_files_at, extract_carved, find_partitions, restore_file, signatures,
    CarveReport, Category, DeletedFile, DeviceIdentity, ScanSession, SessionKind, Signature,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Files listed by `carve` and `open-session` before the rest is summarised.
const LIST_LIMIT: usize = 100;

#[derive(Subcommand, Debug)]
pub enum RecoverCommand {
    /// Search a disk or image for partitions the partition table no longer describes
    Partitions {
        /// `disk:N`, or an image file (`file:PATH` or PATH)
        device: String,
        /// Look at the backup GPT and the free space only, not the whole disk
        #[arg(long)]
        quick: bool,
        /// Add the found partitions to the partition table (needs --yes to write)
        #[arg(long)]
        apply: bool,
        /// With --apply: actually write the table
        #[arg(long)]
        yes: bool,
        /// With --apply: only candidates with at least this confidence (0-100)
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=100))]
        min_confidence: u8,
        /// With --apply: only these candidates, by the numbers in the list (e.g. `1,3`)
        #[arg(long, value_delimiter = ',')]
        select: Vec<usize>,
        /// With --apply: table to create when the disk has none
        #[arg(long, value_enum, default_value = "gpt")]
        table: TableArg,
        /// With --apply: allow writing the table of the running system disk
        #[arg(long)]
        allow_system_disk: bool,
        /// Save the scan result to this file
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// List the deleted files of a filesystem, and restore them with --dest
    Files {
        /// `disk:N:P`, a drive letter, or an image file with a partition (`PATH:P`)
        device: String,
        /// Restore the listed files into this directory
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Save the list to this file
        #[arg(long)]
        session: Option<PathBuf>,
        /// Restore only files whose integrity estimate is at least this (0-100)
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=100))]
        min_integrity: u8,
    },
    /// Find files by their content signatures in raw data
    Carve {
        /// `disk:N`, a drive letter, or an image file
        device: String,
        /// Extract the carved files into this directory (implies --extract)
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Comma-separated categories: image, document, archive, audio, video, executable,
        /// database, other (default: all)
        #[arg(long, value_delimiter = ',')]
        types: Vec<String>,
        /// File starts are looked for at multiples of this many bytes (1 = anywhere, slow)
        #[arg(long, default_value_t = 512, value_parser = clap::value_parser!(u32).range(1..))]
        sector: u32,
        /// Save the scan result to this file
        #[arg(long)]
        session: Option<PathBuf>,
        /// Write the carved files to --dest
        #[arg(long)]
        extract: bool,
    },
    /// Show a saved scan session
    OpenSession {
        file: PathBuf,
    },
}

fn write_err(e: std::io::Error) -> CliError {
    CliError::Runtime(Error::io(0, 0, e))
}

fn session_err(e: Error) -> CliError {
    CliError::Runtime(e)
}

pub fn run(
    platform: &dyn Platform,
    cmd: RecoverCommand,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    match cmd {
        RecoverCommand::Partitions {
            device,
            quick,
            apply,
            yes,
            min_confidence,
            select,
            table,
            allow_system_disk,
            session,
        } => {
            let opts = PartitionOpts { quick, apply, yes, min_confidence, select, table, allow_system_disk, session };
            partitions(platform, &device, &opts, cancel, out)
        }
        RecoverCommand::Files { device, dest, session, min_integrity } => {
            files(platform, &device, dest.as_deref(), session.as_deref(), min_integrity, cancel, out)
        }
        RecoverCommand::Carve { device, dest, types, sector, session, extract } => {
            carve_cmd(platform, &device, dest.as_deref(), &types, sector, session.as_deref(), extract, cancel, out)
        }
        RecoverCommand::OpenSession { file } => open_session(&file, out),
    }
}

/// Who a scan was run on, for the session file. Best effort: never fails.
fn device_identity(platform: &dyn Platform, o: &Opened) -> DeviceIdentity {
    let from_table = |dev: &mut dyn pm_core::BlockDevice| {
        read_table(dev)
            .map(|t| t.to_disk_info(DiskInfo::default()).disk_id)
            .unwrap_or_default()
    };
    match &o.identity {
        Identity::Disk(i) => platform
            .enumerate_disks()
            .ok()
            .and_then(|d| d.into_iter().find(|d| d.index == *i))
            .map(|d| DeviceIdentity::from_disk(&d))
            .unwrap_or_else(|| DeviceIdentity {
                model: format!("Disk {i}"),
                serial: String::new(),
                size: o.range.1,
                table_id: String::new(),
            }),
        Identity::File(p) => {
            let (size, table_id) = match pm_image::open(p, true) {
                Ok(mut dev) => (dev.len(), from_table(&mut *dev)),
                Err(_) => (o.range.1, String::new()),
            };
            DeviceIdentity {
                model: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                serial: String::new(),
                size,
                table_id,
            }
        }
        Identity::Volume(v) => DeviceIdentity {
            model: v.clone(),
            serial: String::new(),
            size: o.range.1,
            table_id: String::new(),
        },
    }
}

/// The drive letters of the volumes stored on the device `arg` names.
fn source_letters(platform: &dyn Platform, arg: &DeviceArg) -> Vec<char> {
    let letter_of = |m: &String| {
        let b = m.as_bytes();
        (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| (b[0] as char).to_ascii_uppercase())
    };
    let disk_letters = |index: u32, number: Option<u32>| -> Vec<char> {
        platform
            .enumerate_disks()
            .unwrap_or_default()
            .iter()
            .filter(|d| d.index == index)
            .flat_map(|d| d.partitions.iter().filter(|p| number.map_or(true, |n| p.number == n)))
            .filter_map(|p| p.letter())
            .collect()
    };
    match arg {
        DeviceArg::Volume(v) => {
            if let Some(c) = letter_of(v) {
                return vec![c];
            }
            platform
                .enumerate_volumes()
                .unwrap_or_default()
                .iter()
                .filter(|vol| vol.id.0.eq_ignore_ascii_case(v))
                .flat_map(|vol| vol.mount_points.iter().filter_map(letter_of))
                .collect()
        }
        DeviceArg::Disk(i) => disk_letters(*i, None),
        DeviceArg::Partition(parent, n) => match &**parent {
            DeviceArg::Disk(i) => disk_letters(*i, Some(*n)),
            _ => Vec::new(),
        },
        DeviceArg::File(_) => Vec::new(),
    }
}

/// The drive letter `dest` is on (it need not exist yet).
fn dest_letter(dest: &Path) -> Option<char> {
    use std::path::{Component, Prefix};
    let abs = std::path::absolute(dest).ok()?;
    match abs.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(b) | Prefix::VerbatimDisk(b) => Some((b as char).to_ascii_uppercase()),
            _ => None,
        },
        _ => None,
    }
}

/// Recovered data must not be written onto the drive it is recovered from: that can overwrite
/// the very data being recovered.
fn check_dest(platform: &dyn Platform, device: &str, dest: &Path) -> Result<(), CliError> {
    let arg = parse_device(device).map_err(arg_error)?;
    match dest_letter(dest) {
        Some(c) if source_letters(platform, &arg).contains(&c) => Err(CliError::Usage(format!(
            "'{}' is on drive {c}:, the drive being recovered from; choose a destination on another drive",
            dest.display()
        ))),
        _ => Ok(()),
    }
}

/// A session of `kind` recorded on the region of the device `o` covers: offsets in `kind`
/// are relative to that region (a partition's start, 0 for a whole disk / image / volume).
fn session_of(identity: DeviceIdentity, kind: SessionKind, o: &Opened) -> ScanSession {
    ScanSession::new(identity, kind).with_region(o.range.0, o.range.1.saturating_sub(o.range.0))
}

fn save_session(
    platform: &dyn Platform,
    o: &Opened,
    kind: SessionKind,
    path: &Path,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    session_of(device_identity(platform, o), kind, o).save(path).map_err(session_err)?;
    writeln!(out, "Session saved to {}", path.display()).map_err(write_err)
}

// ------------------------------------------------------------------------------------------
// partitions

struct PartitionOpts {
    quick: bool,
    apply: bool,
    yes: bool,
    min_confidence: u8,
    select: Vec<usize>,
    table: TableArg,
    allow_system_disk: bool,
    session: Option<PathBuf>,
}

fn render_candidates(cands: &[Candidate]) -> String {
    if cands.is_empty() {
        return "No lost partitions found.\n".to_string();
    }
    let mut s = format!(
        "{:>3}  {:>14}  {:>10}  {:<10}  {:<12}  {:>10}  {}\n",
        "#", "Start", "Length", "File system", "Label", "Confidence", "Source"
    );
    for (i, c) in cands.iter().enumerate() {
        s.push_str(&format!(
            "{:>3}  {:>14}  {:>10}  {:<10}  {:<12}  {:>9}%  {:?}\n",
            i + 1,
            c.start,
            format_size(c.len),
            c.fs.display_name(),
            c.label,
            c.confidence,
            c.source
        ));
    }
    s
}

/// The table to add the candidates to: the one on the device, or a new empty one when the
/// device has none. A table that exists but cannot be understood is never replaced.
fn base_table(dev: &mut dyn pm_core::BlockDevice, new_kind: TableKind) -> Result<Table, CliError> {
    let t = read_table(dev)?;
    match t.kind {
        TableKind::Gpt | TableKind::Mbr => Ok(t),
        TableKind::None => Ok(Table::empty(new_kind, dev.len(), dev.sector_size())),
        TableKind::Unknown => Err(CliError::Usage(
            "the disk has a partition table this tool cannot read; refusing to replace it".into(),
        )),
    }
}

fn partitions(
    platform: &dyn Platform,
    device: &str,
    opts: &PartitionOpts,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let arg = parse_device(device).map_err(arg_error)?;
    if !matches!(arg, DeviceArg::Disk(_) | DeviceArg::File(_)) {
        return Err(CliError::Usage("partitions are searched for on a whole disk or image file".into()));
    }
    let mut opened = open_spec(platform, device, OpenMode::ReadOnly)?;
    let cands = find_partitions(&mut *opened.dev, opts.quick, &StderrProgress::new(), cancel)?;
    out.write_all(render_candidates(&cands).as_bytes()).map_err(write_err)?;
    if let Some(path) = &opts.session {
        save_session(platform, &opened, SessionKind::Partitions(cands.clone()), path, out)?;
    }
    if !opts.apply {
        if !cands.is_empty() {
            writeln!(out, "Add them to the partition table with --apply (and --yes).").map_err(write_err)?;
        }
        return Ok(());
    }

    let mut chosen = Vec::new();
    for (i, c) in cands.iter().enumerate() {
        let wanted = if opts.select.is_empty() { true } else { opts.select.contains(&(i + 1)) };
        if wanted && c.confidence >= opts.min_confidence {
            chosen.push(c.clone());
        }
    }
    if let Some(bad) = opts.select.iter().find(|&&n| n == 0 || n > cands.len()) {
        return Err(CliError::Usage(format!("--select {bad}: there is no such candidate")));
    }
    if chosen.is_empty() {
        return Err(CliError::Usage("no candidate selected".into()));
    }
    let table = base_table(&mut *opened.dev, opts.table.kind())?;
    let new = apply_candidates(&table, &chosen).map_err(arg_error)?;
    writeln!(out, "Would add {} partition(s) to the {:?} table.", chosen.len(), new.kind).map_err(write_err)?;
    if !opts.yes {
        return writeln!(out, "Re-run with --yes to apply").map_err(write_err);
    }
    drop(opened);

    match arg {
        DeviceArg::Disk(i) => write_disk_table(platform, i, &new, opts.allow_system_disk)?,
        DeviceArg::File(path) => {
            let mut dev = pm_image::open(&path, false)?;
            write_table(&mut *dev, &new)?;
            dev.flush()?;
        }
        _ => unreachable!("checked above"),
    }
    writeln!(out, "Partition table written ({} partition(s) added).", chosen.len()).map_err(write_err)
}

/// Write `table` to disk `disk` through a platform write session, then have the OS re-read the
/// layout. Same system-disk rule as the executor: refused when the disk is the running system
/// disk, and when that cannot be determined, unless `allow_system_disk` is given (adding
/// partitions in free space is in the family that flag relaxes).
fn write_disk_table(
    platform: &dyn Platform,
    disk: u32,
    table: &Table,
    allow_system_disk: bool,
) -> Result<(), CliError> {
    let sys = Model::from_platform(platform)?.effective_system_disk();
    if !(sys.is_some_and(|s| s != disk) || allow_system_disk) {
        return Err(CliError::Usage(match sys {
            Some(_) => format!(
                "Disk {disk} is the running system disk; refusing to write to it (--allow-system-disk overrides)"
            ),
            None => format!(
                "Disk {disk}: cannot determine the running system disk; refusing to write to it (--allow-system-disk overrides)"
            ),
        }));
    }
    let mut session = platform.begin_write(disk)?;
    let r = session.commit_table(table);
    let fin = session.finish();
    r?;
    fin?;
    platform.rescan(disk)?;
    Ok(())
}

// ------------------------------------------------------------------------------------------
// files

fn render_deleted(files: &[DeletedFile]) -> String {
    if files.is_empty() {
        return "No deleted files found.\n".to_string();
    }
    let mut s = format!("{:>9}  {:>10}  {}\n", "Integrity", "Size", "Path");
    for f in files {
        s.push_str(&format!(
            "{:>8}%  {:>10}  {}{}\n",
            f.integrity,
            if f.entry.is_dir { String::new() } else { format_size(f.entry.size) },
            f.entry.path,
            if f.entry.is_dir { "/" } else { "" }
        ));
    }
    s
}

fn files(
    platform: &dyn Platform,
    device: &str,
    dest: Option<&Path>,
    session: Option<&Path>,
    min_integrity: u8,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if let Some(dest) = dest {
        check_dest(platform, device, dest)?;
    }
    let opened = open_spec(platform, device, OpenMode::ReadOnly)?;
    let offset = opened.range.0;
    let opened_len = opened.range.1.saturating_sub(opened.range.0);
    let identity = device_identity(platform, &opened);
    let mut fs = pm_fs::open(opened.dev).map_err(|e| match e {
        Error::Unsupported(m) | Error::Parse(m) => CliError::Usage(format!("{device}: no readable filesystem ({m})")),
        other => CliError::Runtime(other),
    })?;
    let mut list = deleted_files_at(&mut *fs, offset, &StderrProgress::new(), cancel)?;
    list.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    out.write_all(render_deleted(&list).as_bytes()).map_err(write_err)?;
    if let Some(path) = session {
        let region = (offset, opened_len);
        ScanSession::new(identity, SessionKind::Deleted(list.clone()))
            .with_region(region.0, region.1)
            .save(path)
            .map_err(session_err)?;
        writeln!(out, "Session saved to {}", path.display()).map_err(write_err)?;
    }
    let Some(dest) = dest else {
        if !list.is_empty() {
            writeln!(out, "Restore them with --dest DIR.").map_err(write_err)?;
        }
        return Ok(());
    };
    let (mut restored, mut failed) = (0usize, 0usize);
    for f in list.iter().filter(|f| !f.entry.is_dir && f.integrity >= min_integrity) {
        cancel.check()?;
        match restore_file(&mut *fs, f, dest) {
            Ok(p) => {
                restored += 1;
                writeln!(out, "Restored {} -> {}", f.entry.path, p.display()).map_err(write_err)?;
            }
            Err(e) => {
                failed += 1;
                writeln!(out, "Could not restore {}: {e}", f.entry.path).map_err(write_err)?;
            }
        }
    }
    writeln!(out, "{restored} file(s) restored, {failed} failed.").map_err(write_err)?;
    if failed > 0 {
        return Err(CliError::Failed(format!("{failed} file(s) could not be restored")));
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// carve

fn category_name(c: Category) -> &'static str {
    match c {
        Category::Image => "Image",
        Category::Document => "Document",
        Category::Archive => "Archive",
        Category::Audio => "Audio",
        Category::Video => "Video",
        Category::Executable => "Executable",
        Category::Database => "Database",
        Category::Other => "Other",
    }
}

fn parse_category(s: &str) -> Result<Category, CliError> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "image" | "images" => Category::Image,
        "document" | "documents" => Category::Document,
        "archive" | "archives" => Category::Archive,
        "audio" => Category::Audio,
        "video" => Category::Video,
        "executable" | "executables" => Category::Executable,
        "database" | "databases" => Category::Database,
        "other" => Category::Other,
        _ => {
            return Err(CliError::Usage(format!(
                "unknown file category '{s}' (expected image, document, archive, audio, video, \
                 executable, database or other)"
            )))
        }
    })
}

fn select_signatures(types: &[String]) -> Result<Vec<Signature>, CliError> {
    if types.is_empty() {
        return Ok(signatures().to_vec());
    }
    let cats = types.iter().map(|t| parse_category(t)).collect::<Result<Vec<_>, _>>()?;
    Ok(signatures().iter().filter(|s| cats.contains(&s.category)).cloned().collect())
}

fn render_carved(r: &CarveReport) -> String {
    let mut s = String::new();
    if r.files.is_empty() {
        s.push_str("No files found.\n");
    } else {
        let mut per: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
        for f in &r.files {
            let e = per.entry(category_name(f.category)).or_default();
            e.0 += 1;
            e.1 += f.len;
            e.2 += f.complete as u64;
        }
        s.push_str(&format!("{:<12}  {:>6}  {:>10}  {:>8}\n", "Category", "Files", "Size", "Complete"));
        for (cat, (n, bytes, complete)) in per {
            s.push_str(&format!("{cat:<12}  {n:>6}  {:>10}  {complete:>8}\n", format_size(bytes)));
        }
        s.push_str(&format!("\n{:>14}  {:>10}  {:<10}  {:<5}  {}\n", "Offset", "Size", "Type", "Ext", "Complete"));
        for f in r.files.iter().take(LIST_LIMIT) {
            s.push_str(&format!(
                "{:>14}  {:>10}  {:<10}  {:<5}  {}\n",
                f.offset,
                format_size(f.len),
                f.sig,
                f.ext,
                if f.complete { "yes" } else { "no" }
            ));
        }
        if r.files.len() > LIST_LIMIT {
            s.push_str(&format!("... and {} more\n", r.files.len() - LIST_LIMIT));
        }
    }
    s.push_str(&format!("Scanned {}.\n", format_size(r.bytes_scanned)));
    if !r.unreadable.is_empty() {
        s.push_str("Unreadable ranges (scanned as zeros):\n");
        for (off, len) in &r.unreadable {
            s.push_str(&format!("  {off} + {len} ({})\n", format_size(*len)));
        }
    }
    s
}

#[allow(clippy::too_many_arguments)]
fn carve_cmd(
    platform: &dyn Platform,
    device: &str,
    dest: Option<&Path>,
    types: &[String],
    sector: u32,
    session: Option<&Path>,
    extract: bool,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let extract = extract || dest.is_some();
    if extract && dest.is_none() {
        return Err(CliError::Usage("--extract needs --dest DIR".into()));
    }
    if let Some(dest) = dest {
        check_dest(platform, device, dest)?;
    }
    let sigs = select_signatures(types)?;
    if sigs.is_empty() {
        return Err(CliError::Usage("no file signatures match the requested types".into()));
    }
    let mut opened = open_spec(platform, device, OpenMode::ReadOnly)?;
    let report = carve(&mut *opened.dev, &sigs, sector, &StderrProgress::new(), cancel)?;
    out.write_all(render_carved(&report).as_bytes()).map_err(write_err)?;
    if let Some(path) = session {
        save_session(platform, &opened, SessionKind::Carved(report.clone()), path, out)?;
    }
    if let (true, Some(dest)) = (extract, dest) {
        let (mut done, mut failed) = (0usize, 0usize);
        for f in &report.files {
            cancel.check()?;
            match extract_carved(&mut *opened.dev, f, dest) {
                Ok(_) => done += 1,
                Err(e) => {
                    failed += 1;
                    writeln!(out, "Could not extract the file at {}: {e}", f.offset).map_err(write_err)?;
                }
            }
        }
        writeln!(out, "{done} file(s) extracted to {}, {failed} failed.", dest.display()).map_err(write_err)?;
        if failed > 0 {
            return Err(CliError::Failed(format!("{failed} file(s) could not be extracted")));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// open-session

/// Which part of the device a session covers; carved offsets are relative to it.
fn region_text(s: &ScanSession) -> String {
    match (s.offset, s.scanned_len) {
        (0, 0) => "Scanned region: the whole device".to_string(),
        (off, 0) => format!("Scanned region: from offset {off} to the end of the device (offsets below are relative to it)"),
        (off, len) => format!(
            "Scanned region: {} at offset {off} (offsets below are relative to it)",
            format_size(len)
        ),
    }
}

fn open_session(file: &Path, out: &mut dyn Write) -> Result<(), CliError> {
    let s = ScanSession::load(file).map_err(|e| match e {
        Error::Parse(m) => CliError::Usage(m),
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            CliError::Usage(format!("session file '{}' does not exist", file.display()))
        }
        other => CliError::Runtime(other),
    })?;
    let d = &s.device;
    writeln!(
        out,
        "Session created {}\nDevice: {} (serial '{}', {}, table id '{}')",
        s.created,
        d.model,
        d.serial,
        format_size(d.size),
        d.table_id
    )
    .map_err(write_err)?;
    writeln!(out, "{}", region_text(&s)).map_err(write_err)?;
    let body = match &s.kind {
        SessionKind::Partitions(c) => render_candidates(c),
        SessionKind::Deleted(f) => render_deleted(f),
        SessionKind::Carved(r) => render_carved(r),
    };
    out.write_all(body.as_bytes()).map_err(write_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A carve of a partition records the partition's region, so the stored (partition-
    /// relative) offsets read the right bytes through a slice of the whole image.
    #[test]
    fn partition_carve_session_records_its_region() {
        use pm_core::{BlockDevice, MIB};
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("disk.img");
        let mut dev = pm_image::create(&img, &pm_image::CreateOpts { format: pm_image::ImageFormat::Raw, capacity: 16 * MIB, sparse: false }).unwrap();
        let mut t = Table::empty(TableKind::Gpt, 16 * MIB, 512);
        t.entries.push(pm_parttable::Entry {
            number: 1,
            start: 4 * MIB,
            len: 8 * MIB,
            ptype: pm_core::PartitionType::Gpt(pm_core::gpt_types::BASIC_DATA),
            guid: Some(uuid::Uuid::from_u128(9)),
            name: String::new(),
            attrs: 0,
            logical: false,
        });
        pm_parttable::write_table(&mut *dev, &t).unwrap();
        let pdf = b"%PDF-1.4
1 0 obj << >> endobj
trailer << >>
%%EOF
";
        dev.write_at(4 * MIB + MIB, pdf).unwrap();
        dev.flush().unwrap();
        drop(dev);
        let session = dir.path().join("s.json");
        let platform = pm_platform::MockPlatform::new();
        let cmd = RecoverCommand::Carve {
            device: format!("{}:1", img.display()),
            dest: None,
            types: vec!["document".into()],
            sector: 512,
            session: Some(session.clone()),
            extract: false,
        };
        run(&platform, cmd, &CancelToken::new(), &mut Vec::new()).unwrap();
        let s = ScanSession::load(&session).unwrap();
        assert_eq!((s.offset, s.scanned_len), (4 * MIB, 8 * MIB));
        let SessionKind::Carved(r) = &s.kind else { panic!("{:?}", s.kind) };
        let f = r.files.iter().find(|f| f.ext == "pdf").expect("the PDF is carved");
        assert_eq!(f.offset, MIB, "relative to the partition");
        // reading it back the way a restore does: the region of the whole image
        let whole = pm_image::open(&img, true).unwrap();
        let (off, len) = s.region(whole.len()).unwrap();
        let mut region = pm_core::SliceDevice::new(whole, off, len).unwrap();
        let out = extract_carved(&mut region, f, &dir.path().join("out")).unwrap();
        assert!(std::fs::read(out).unwrap().starts_with(b"%PDF-1.4"));
        let mut text = Vec::new();
        open_session(&session, &mut text).unwrap();
        assert!(String::from_utf8(text).unwrap().contains("Scanned region: 8.00 MB at offset 4194304"));
    }

    #[test]
    fn categories_parse() {
        assert_eq!(parse_category("Image").unwrap(), Category::Image);
        assert_eq!(parse_category(" documents ").unwrap(), Category::Document);
        assert!(parse_category("bogus").is_err());
    }

    #[test]
    fn signatures_filter_by_category() {
        let all = select_signatures(&[]).unwrap();
        let images = select_signatures(&["image".into()]).unwrap();
        assert!(!images.is_empty() && images.len() < all.len());
        assert!(images.iter().all(|s| s.category == Category::Image));
        let two = select_signatures(&["image".into(), "document".into()]).unwrap();
        assert!(two.len() > images.len());
    }

    #[test]
    fn destination_on_the_source_drive_is_refused() {
        use pm_core::{MemDevice, VolumeId, VolumeInfo};
        use pm_platform::MockPlatform;
        let p = MockPlatform::new();
        p.add_disk(0, MemDevice::new(1 << 20), DiskInfo::default());
        let vol = r"\\?\Volume{aaaa}\";
        p.add_volume(
            VolumeInfo { id: VolumeId(vol.into()), mount_points: vec![r"D:\".into()], ..Default::default() },
            0,
            0,
            4096,
        );
        assert_eq!(dest_letter(Path::new(r"d:\out")), Some('D'));
        assert_eq!(dest_letter(Path::new(r"\\server\share\x")), None);
        assert!(matches!(check_dest(&p, "D:", Path::new(r"D:\recovered")), Err(CliError::Usage(_))));
        assert!(check_dest(&p, "D:", Path::new(r"E:\recovered")).is_ok());
        assert!(check_dest(&p, vol, Path::new(r"d:\x")).is_err());
        // a relative destination is resolved against the current directory
        assert!(check_dest(&p, "disk:0", Path::new("relative")).is_ok());
    }

    #[test]
    fn disk_table_write_follows_the_system_disk_rule_and_rescans() {
        use pm_core::MemDevice;
        use pm_platform::MockPlatform;
        let table = Table::empty(TableKind::Gpt, 8 << 20, 512);
        let p = MockPlatform::new();
        p.add_disk(0, MemDevice::new(8 << 20), DiskInfo::default());
        p.add_disk(1, MemDevice::new(8 << 20), DiskInfo::default());

        // the system disk is unknown: refused (fails closed), nothing written
        let r = write_disk_table(&p, 0, &table, false);
        assert!(matches!(&r, Err(CliError::Usage(m)) if m.contains("cannot determine")), "{r:?}");
        assert!(p.write_sessions_log().is_empty());

        // the disk is the system disk: refused
        p.set_system_disk(0);
        let r = write_disk_table(&p, 0, &table, false);
        assert!(matches!(&r, Err(CliError::Usage(m)) if m.contains("running system disk")), "{r:?}");
        assert!(p.write_sessions_log().is_empty());

        // another disk is the system disk: written, session finished, layout re-read
        write_disk_table(&p, 1, &table, false).unwrap();
        assert_eq!(p.write_sessions_log(), vec![1]);
        assert_eq!(p.finished_sessions_log(), vec![1]);
        assert!(p.calls().contains(&"rescan 1".to_string()), "{:?}", p.calls());

        // the override lets the system disk through
        write_disk_table(&p, 0, &table, true).unwrap();
        assert_eq!(p.write_sessions_log(), vec![1, 0]);
    }

    #[test]
    fn empty_results_render() {
        assert!(render_candidates(&[]).contains("No lost partitions"));
        assert!(render_deleted(&[]).contains("No deleted files"));
        assert!(render_carved(&CarveReport::default()).contains("No files found"));
    }
}
