//! `pm analyze space|surface|bench|smart`: disk space usage, read test, throughput benchmark
//! and SMART health. Everything here only reads (the benchmark writes a temporary file that it
//! removes again).

use crate::cli::{arg_error, size_arg, CliError};
use crate::cmd_ops::disk_arg;
use crate::device_arg::open_spec;
use crate::progress::StderrProgress;
use clap::Subcommand;
use pm_analyze::{
    scan_space, smart_summary, surface_test, Health, SpaceReport, SurfaceReport, DEFAULT_BLOCK,
};
use pm_core::{format_size, CancelToken, Error, ProgressReport, ProgressSink};
use pm_platform::{OpenMode, Platform, SmartData};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Subcommand, Debug)]
pub enum AnalyzeCommand {
    /// Show where the space in a directory tree goes
    Space {
        /// Directory (or drive root) to scan
        path: PathBuf,
        /// Worker threads (default: the number of CPUs)
        #[arg(long)]
        threads: Option<usize>,
        /// Entries shown per table
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// Print the whole report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Read every block of a device and report unreadable and slow ones
    Surface {
        /// `disk:N`, a drive letter, or an image file
        device: String,
        /// Block size (default 1M)
        #[arg(long, value_parser = size_arg)]
        block: Option<u64>,
    },
    /// Measure read and write throughput with a temporary file (Windows only)
    Bench {
        /// Directory or drive letter (`D:`) to test
        target: String,
        /// Comma-separated transfer sizes (default 4K,8K,16K,32K)
        #[arg(long, value_delimiter = ',', value_parser = size_arg)]
        sizes: Vec<u64>,
        /// Bytes moved per pass (default 200M)
        #[arg(long, value_parser = size_arg)]
        total: Option<u64>,
        /// Requests issued back to back per worker (default 8)
        #[arg(long)]
        queue: Option<u32>,
        /// Worker threads (default 4)
        #[arg(long)]
        threads: Option<u32>,
        /// Random instead of sequential access
        #[arg(long)]
        random: bool,
        /// Seconds to rest between transfer sizes (default 5)
        #[arg(long)]
        cooldown: Option<u32>,
    },
    /// Show the SMART health of a disk
    Smart {
        /// Disk index (`1` or `disk:1`)
        #[arg(value_parser = disk_arg)]
        disk: u32,
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
    },
}

fn write_err(e: std::io::Error) -> CliError {
    CliError::Runtime(Error::io(0, 0, e))
}

pub fn run(
    platform: &dyn Platform,
    cmd: AnalyzeCommand,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    match cmd {
        AnalyzeCommand::Space { path, threads, top, json } => space(&path, threads, top, json, cancel, out),
        AnalyzeCommand::Surface { device, block } => surface(platform, &device, block, cancel, out),
        AnalyzeCommand::Bench { target, sizes, total, queue, threads, random, cooldown } => {
            let opts = BenchArgs { sizes, total, queue, threads, random, cooldown };
            bench(&target, &opts, cancel, out)
        }
        AnalyzeCommand::Smart { disk, json } => smart(platform, disk, json, out),
    }
}

// ------------------------------------------------------------------------------------------
// space

/// Progress for a scan whose total is unknown: one refreshing "N items" line on stderr.
struct CountProgress {
    last: Mutex<Option<Instant>>,
    drew: AtomicBool,
}

impl CountProgress {
    fn new() -> Self {
        CountProgress { last: Mutex::new(None), drew: AtomicBool::new(false) }
    }

    /// End the refreshing line.
    fn finish(&self) {
        if self.drew.load(Ordering::SeqCst) {
            eprintln!();
        }
    }
}

impl ProgressSink for CountProgress {
    fn report(&self, p: &ProgressReport) {
        let now = Instant::now();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(200)) {
            return;
        }
        *last = Some(now);
        let mut detail: String = p.detail.chars().rev().take(48).collect::<Vec<_>>().into_iter().rev().collect();
        if detail.len() < p.detail.len() {
            detail.insert_str(0, "...");
        }
        eprint!("\r{:<79}", format!("{}  {} entries  {detail}", p.phase, p.done));
        self.drew.store(true, Ordering::SeqCst);
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

fn render_space(r: &SpaceReport, root: &Path, top: usize) -> String {
    let mut s = format!(
        "{}: {} in {} file(s) and {} folder(s){}\n",
        root.display(),
        format_size(r.total),
        r.root.files,
        r.root.dirs,
        if r.errors > 0 { format!(", {} entries could not be read", r.errors) } else { String::new() }
    );
    s.push_str("\nLargest folders\n");
    s.push_str(&format!("{:>10}  {:>6}  {:>8}  {}\n", "Size", "Share", "Files", "Folder"));
    for c in r.root.children.iter().take(top) {
        s.push_str(&format!(
            "{:>10}  {:>5.1}%  {:>8}  {}\n",
            format_size(c.size),
            pct(c.size, r.total),
            c.files,
            c.name
        ));
    }
    if r.root.children.is_empty() {
        s.push_str("  (no subfolders)\n");
    }
    s.push_str("\nBy file type\n");
    s.push_str(&format!("{:>10}  {:>6}  {:>8}  {}\n", "Size", "Share", "Files", "Extension"));
    for (ext, size, count) in r.by_ext.iter().take(top) {
        let name = if ext.is_empty() { "(none)".to_string() } else { format!(".{ext}") };
        s.push_str(&format!("{:>10}  {:>5.1}%  {count:>8}  {name}\n", format_size(*size), pct(*size, r.total)));
    }
    s.push_str("\nLargest files\n");
    for (path, size) in r.largest.iter().take(top) {
        s.push_str(&format!("{:>10}  {}\n", format_size(*size), path.display()));
    }
    s
}

fn space(
    path: &Path,
    threads: Option<usize>,
    top: usize,
    json: bool,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let threads = threads
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(4)
        .max(1);
    let progress = CountProgress::new();
    let report = scan_space(path, threads, &progress, cancel);
    progress.finish();
    let report = report.map_err(|e| match e {
        Error::Invalid(m) => CliError::Usage(m),
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            CliError::Usage(format!("'{}' does not exist", path.display()))
        }
        other => CliError::Runtime(other),
    })?;
    let text = if json {
        let mut s = serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::Usage(format!("cannot serialize the report: {e}")))?;
        s.push('\n');
        s
    } else {
        render_space(&report, path, top)
    };
    out.write_all(text.as_bytes()).map_err(write_err)
}

// ------------------------------------------------------------------------------------------
// surface

/// One character per 1% of the blocks (one per block when there are fewer than 100):
/// `.` ok, `S` slow, `X` bad; the worst block of the cell wins.
pub fn surface_map(r: &SurfaceReport) -> String {
    let blocks = r.blocks as usize;
    if blocks == 0 {
        return String::new();
    }
    let cells = blocks.min(100);
    let mut bad = vec![false; blocks];
    let mut slow = vec![false; blocks];
    r.bad.iter().for_each(|&b| bad[b as usize] = true);
    r.slow.iter().for_each(|&b| slow[b as usize] = true);
    (0..cells)
        .map(|c| {
            let range = c * blocks / cells..(c + 1) * blocks / cells;
            if range.clone().any(|b| bad[b]) {
                'X'
            } else if range.into_iter().any(|b| slow[b]) {
                'S'
            } else {
                '.'
            }
        })
        .collect()
}

fn render_surface(r: &SurfaceReport, dev_len: u64) -> String {
    let mut s = format!(
        "Surface test of {} ({} block(s) of {}) took {:.1} s\nBad blocks: {}\nSlow blocks: {}\n",
        format_size(dev_len),
        r.blocks,
        format_size(r.block),
        r.elapsed_secs,
        r.bad.len(),
        r.slow.len()
    );
    s.push_str("Map (. ok, S slow, X bad):\n");
    s.push_str(&format!("[{}]\n", surface_map(r)));
    if !r.bad.is_empty() {
        s.push_str("Bad block offsets:\n");
        for &b in r.bad.iter().take(100) {
            s.push_str(&format!("  {}  (block {b})\n", b as u64 * r.block));
        }
        if r.bad.len() > 100 {
            s.push_str(&format!("  ... and {} more\n", r.bad.len() - 100));
        }
    }
    s
}

fn surface(
    platform: &dyn Platform,
    device: &str,
    block: Option<u64>,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut opened = open_spec(platform, device, OpenMode::ReadOnly)?;
    let len = opened.dev.len();
    let report = surface_test(&mut *opened.dev, block.unwrap_or(DEFAULT_BLOCK), &StderrProgress::new(), cancel)
        .map_err(arg_error)?;
    out.write_all(render_surface(&report, len).as_bytes()).map_err(write_err)
}

// ------------------------------------------------------------------------------------------
// bench

struct BenchArgs {
    sizes: Vec<u64>,
    total: Option<u64>,
    queue: Option<u32>,
    threads: Option<u32>,
    random: bool,
    cooldown: Option<u32>,
}

#[cfg(windows)]
fn bench(target: &str, a: &BenchArgs, cancel: &CancelToken, out: &mut dyn Write) -> Result<(), CliError> {
    use crate::cli::parse_letter;
    use pm_analyze::{benchmark, BenchMode, BenchOpts};

    let dir = if Path::new(target).is_dir() {
        PathBuf::from(target)
    } else {
        let letter = parse_letter(target)
            .map_err(|_| CliError::Usage(format!("'{target}' is neither a directory nor a drive letter")))?;
        let root = PathBuf::from(format!("{letter}:\\"));
        if !root.is_dir() {
            return Err(CliError::Usage(format!("drive {letter}: does not exist")));
        }
        root
    };
    let mut opts = BenchOpts::default();
    if !a.sizes.is_empty() {
        opts.transfer_sizes = a
            .sizes
            .iter()
            .map(|&s| u32::try_from(s).map_err(|_| CliError::Usage(format!("transfer size {s} is too large"))))
            .collect::<Result<_, _>>()?;
    }
    opts.total_len = a.total.unwrap_or(opts.total_len);
    opts.queue_depth = a.queue.unwrap_or(opts.queue_depth);
    opts.threads = a.threads.unwrap_or(opts.threads);
    opts.cooldown_secs = a.cooldown.unwrap_or(opts.cooldown_secs);
    if a.random {
        opts.mode = BenchMode::Random;
    }
    let target = pm_platform::FileBenchTarget::new(dir.clone());
    let points = benchmark(&target, &opts, &StderrProgress::new(), cancel).map_err(arg_error)?;
    let mut s = format!(
        "Benchmark of {} ({}, {} thread(s), {} per pass)\n",
        dir.display(),
        if a.random { "random" } else { "sequential" },
        opts.threads,
        format_size(opts.total_len)
    );
    s.push_str(&format!(
        "{:>10}  {:>12}  {:>12}  {:>12}  {:>12}\n",
        "Size", "Read MB/s", "Write MB/s", "Read IOPS", "Write IOPS"
    ));
    for p in points {
        s.push_str(&format!(
            "{:>10}  {:>12.1}  {:>12.1}  {:>12.0}  {:>12.0}\n",
            format_size(p.size as u64),
            p.read_mbps,
            p.write_mbps,
            p.read_iops,
            p.write_iops
        ));
    }
    out.write_all(s.as_bytes()).map_err(write_err)
}

#[cfg(not(windows))]
fn bench(_target: &str, _a: &BenchArgs, _cancel: &CancelToken, _out: &mut dyn Write) -> Result<(), CliError> {
    Err(CliError::Runtime(Error::Unsupported(
        "the benchmark needs the Windows unbuffered file target".into(),
    )))
}

// ------------------------------------------------------------------------------------------
// smart

fn marker(h: Health) -> &'static str {
    match h {
        Health::Ok => "[ OK ]",
        Health::Warn => "[WARN]",
        Health::Bad => "[BAD ]",
    }
}

fn render_smart(d: &SmartData) -> String {
    let mut s = String::new();
    for (name, value, health) in smart_summary(d) {
        s.push_str(&format!("{}  {:<20}  {value}\n", marker(health), name));
    }
    if !d.attrs.is_empty() {
        s.push_str(&format!(
            "\n{:>4}  {:<32}  {:>5}  {:>5}  {:>6}  {}\n",
            "ID", "Attribute", "Value", "Worst", "Thresh", "Raw"
        ));
        for a in &d.attrs {
            s.push_str(&format!(
                "{:>4}  {:<32}  {:>5}  {:>5}  {:>6}  {}\n",
                a.id, a.name, a.value, a.worst, a.threshold, a.raw
            ));
        }
    }
    s
}

fn smart(platform: &dyn Platform, disk: u32, json: bool, out: &mut dyn Write) -> Result<(), CliError> {
    let data = platform.smart(disk).map_err(|e| match e {
        Error::NotFound(m) => CliError::Usage(m),
        other => CliError::Runtime(other),
    })?;
    let text = if json {
        let v = match &data {
            None => serde_json::Value::Null,
            Some(d) => {
                let rows: Vec<_> = smart_summary(d)
                    .into_iter()
                    .map(|(name, value, health)| serde_json::json!({ "name": name, "value": value, "health": health }))
                    .collect();
                serde_json::json!({ "data": d, "summary": rows })
            }
        };
        let mut s = serde_json::to_string_pretty(&v)
            .map_err(|e| CliError::Usage(format!("cannot serialize SMART data: {e}")))?;
        s.push('\n');
        s
    } else {
        match &data {
            None => format!("No SMART data available for Disk {disk}.\n"),
            Some(d) => render_smart(d),
        }
    };
    out.write_all(text.as_bytes()).map_err(write_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_platform::SmartAttr;

    fn report(blocks: u32, bad: &[u32], slow: &[u32]) -> SurfaceReport {
        SurfaceReport {
            block: 1 << 20,
            blocks,
            bad: bad.to_vec(),
            slow: slow.to_vec(),
            ms_per_block: vec![0; blocks as usize],
            elapsed_secs: 0.5,
        }
    }

    #[test]
    fn map_is_one_character_per_percent() {
        assert_eq!(surface_map(&report(4, &[], &[])), "....");
        assert_eq!(surface_map(&report(4, &[1], &[2])), ".XS.");
        let m = surface_map(&report(1000, &[500], &[999]));
        assert_eq!(m.len(), 100);
        assert_eq!(m.chars().nth(50), Some('X'));
        assert_eq!(m.chars().nth(99), Some('S'));
        assert_eq!(m.chars().filter(|&c| c == '.').count(), 98);
        // a bad block beats a slow one in the same cell
        assert_eq!(surface_map(&report(200, &[10], &[11])).chars().nth(5), Some('X'));
        assert_eq!(surface_map(&report(0, &[], &[])), "");
    }

    #[test]
    fn surface_report_lists_bad_offsets() {
        let text = render_surface(&report(8, &[3], &[]), 8 << 20);
        assert!(text.contains("Bad blocks: 1") && text.contains("Slow blocks: 0"), "{text}");
        assert!(text.contains(&(3u64 << 20).to_string()), "{text}");
        assert!(text.contains("...X...."), "{text}");
    }

    #[test]
    fn smart_rows_carry_markers() {
        let d = SmartData {
            healthy: Some(true),
            temperature_c: Some(80),
            attrs: vec![SmartAttr { id: 5, name: "Reallocated_Sector_Ct".into(), value: 100, worst: 100, threshold: 10, raw: 12 }],
            ..Default::default()
        };
        let text = render_smart(&d);
        assert!(text.contains("[ OK ]  Health"), "{text}");
        assert!(text.contains("[BAD ]  Temperature"), "{text}");
        assert!(text.contains("Reallocated_Sector_Ct"), "{text}");
    }
}
