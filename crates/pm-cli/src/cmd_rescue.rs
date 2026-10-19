//! `pm rescue <src> <dst> <mapfile>`: ddrescue-style copy of a failing device with a resumable
//! mapfile and a live status block on stderr.

use crate::cli::{CliError, RescueArgs};
use crate::cmd_clone::{existing_canonical, fs_used_map, open_checked, SharedSource};
use crate::device_arg::Identity;
use pm_core::{format_size, BlockDevice, CancelToken, Error, ProgressReport, ProgressSink};
use pm_image::{rescue, RescueOpts};
use pm_platform::Platform;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Largest accepted `--block-size`: the engine allocates one buffer of that size.
const MAX_BLOCK: u64 = 256 << 20;

/// The source as the rescue engine sees it: read-only, remembering the offset of the latest
/// read so the status block can show the input position (`ipos`).
struct PosSource {
    inner: SharedSource,
    pos: Arc<AtomicU64>,
}

impl BlockDevice for PosSource {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> pm_core::Result<()> {
        self.pos.store(offset, Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> pm_core::Result<()> {
        self.inner.write_at(offset, buf)
    }
    fn flush(&mut self) -> pm_core::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

/// How the status is drawn on stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusMode {
    /// Multi-line block redrawn in place with ANSI cursor movement (interactive terminals).
    Block,
    /// One `\r`-refreshed line (terminals without ANSI support).
    Line,
    /// One line per phase change (redirected stderr, logs).
    Plain,
}

#[derive(Default)]
struct DrawState {
    /// Lines of the block drawn last (`Block`), or whether a `\r` line is open (`Line`).
    drawn: usize,
    last_phase: String,
}

/// Value of `key` in an engine detail string such as `errsize 0 B, errors 3, rate 1.00 MB/s`.
fn detail_field<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    detail
        .split(", ")
        .find_map(|seg| seg.strip_prefix(key).and_then(|v| v.strip_prefix(' ')))
}

/// The ddrescue-like status block for one progress report.
fn render_block(p: &ProgressReport, ipos: u64) -> Vec<String> {
    let pct = if p.total == 0 { 100.0 } else { p.done as f64 * 100.0 / p.total as f64 };
    let eta = match p.eta_secs {
        None => "--".to_string(),
        Some(s) if s >= 3600 => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
        Some(s) => format!("{}:{:02}", s / 60, s % 60),
    };
    let errsize = detail_field(&p.detail, "errsize").unwrap_or("-");
    let errors = detail_field(&p.detail, "errors").unwrap_or("-");
    vec![
        format!("     ipos: {:<12}   phase: {}", format_size(ipos), p.phase),
        format!(
            "  rescued: {} of {} ({pct:.1}%)",
            format_size(p.done),
            format_size(p.total)
        ),
        format!("  errsize: {errsize:<12}   errors: {errors}"),
        format!("     rate: {:<12}   ETA: {eta}", format!("{}/s", format_size(p.rate_bps.max(0.0) as u64))),
    ]
}

fn finished(p: &ProgressReport) -> bool {
    p.detail.ends_with("finished")
}

/// Renders the reports of a rescue run on stderr.
struct RescueStatus {
    mode: StatusMode,
    pos: Arc<AtomicU64>,
    state: Mutex<DrawState>,
}

impl RescueStatus {
    fn new(pos: Arc<AtomicU64>, ansi: bool) -> Self {
        let mode = choose_mode(
            ansi,
            std::io::stderr().is_terminal(),
            std::env::var_os("WT_SESSION").is_some(),
            std::env::var("TERM").ok().as_deref(),
        );
        RescueStatus { mode, pos, state: Mutex::new(DrawState::default()) }
    }

    /// The text to write for `p`.
    fn frame(&self, p: &ProgressReport, ipos: u64) -> String {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match self.mode {
            StatusMode::Block => {
                let lines = render_block(p, ipos);
                let mut s = String::new();
                if st.drawn > 0 {
                    s.push_str(&format!("\x1b[{}A", st.drawn));
                }
                for l in &lines {
                    s.push_str("\r\x1b[2K");
                    s.push_str(l);
                    s.push('\n');
                }
                st.drawn = lines.len();
                s
            }
            StatusMode::Line => {
                let mut s = String::new();
                if st.drawn > 0 && st.last_phase != p.phase {
                    s.push('\n');
                }
                let line = format!("{}: {} rescued, {}", p.phase, format_size(p.done), p.detail);
                s.push_str(&format!("\r{line:<79}"));
                st.drawn = 1;
                st.last_phase = p.phase.clone();
                s
            }
            StatusMode::Plain => {
                if st.last_phase == p.phase && !finished(p) {
                    return String::new();
                }
                st.last_phase = p.phase.clone();
                let b = render_block(p, ipos);
                format!("{}\n", b.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" | "))
            }
        }
    }

    /// End the status display, leaving the cursor on a fresh line below it.
    fn close(&self) {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.mode == StatusMode::Line && st.drawn > 0 {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(b"\n");
            let _ = err.flush();
        }
    }
}

impl ProgressSink for RescueStatus {
    fn report(&self, p: &ProgressReport) {
        let text = self.frame(p, self.pos.load(Ordering::Relaxed));
        if !text.is_empty() {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(text.as_bytes());
            let _ = err.flush();
        }
    }
}

/// How to draw the status. The in-place block needs ANSI escapes, which are only used when
/// asked for (`--ansi`) or when the environment says the terminal handles them (Windows
/// Terminal sets `WT_SESSION`, Unix terminals set `TERM`); any other terminal gets the plain
/// `\r`-refreshed line, and redirected stderr one line per phase.
fn choose_mode(ansi_flag: bool, is_terminal: bool, wt_session: bool, term: Option<&str>) -> StatusMode {
    if ansi_flag {
        StatusMode::Block
    } else if !is_terminal {
        StatusMode::Plain
    } else if wt_session || term.is_some_and(|t| !t.is_empty() && t != "dumb") {
        StatusMode::Block
    } else {
        StatusMode::Line
    }
}

/// Engine options from the flags. `-b` is the sector size in bytes (a multiple of the
/// device's), `-c` the sectors per copy-phase read; the read size is their product.
fn opts_from_args(a: &RescueArgs, device_sector: u32) -> Result<RescueOpts, CliError> {
    let device_sector = device_sector.max(1) as u64;
    let sector = a.sector_size.unwrap_or(device_sector);
    if sector == 0 || sector > u32::MAX as u64 || sector % device_sector != 0 {
        return Err(CliError::Usage(format!(
            "--sector-size must be a non-zero multiple of the device's sector size ({device_sector})"
        )));
    }
    let default_cluster = (RescueOpts::default().block_size as u64 / sector).max(1);
    let cluster = a.cluster_size.map_or(default_cluster, u64::from);
    let block = cluster.checked_mul(sector).filter(|&b| b <= MAX_BLOCK).ok_or_else(|| {
        CliError::Usage(format!(
            "--cluster-size x --sector-size must be at most {}",
            format_size(MAX_BLOCK)
        ))
    })?;
    let mut opts = RescueOpts {
        sector: sector as u32,
        block_size: block as usize,
        ..RescueOpts::default()
    };
    if let Some(k) = a.skip_size {
        opts.skip_size = k;
    }
    opts.max_retries = a.retries;
    opts.reverse = a.reverse;
    opts.no_trim = a.no_trim;
    opts.no_scrape = a.no_scrape;
    opts.mapfile = Some(a.mapfile.clone());
    Ok(opts)
}

pub fn run(
    platform: &dyn Platform,
    a: &RescueArgs,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    // Source and destination are checked (and the destination opened read-only) before the
    // destination is opened for writing; the remaining checks below also precede any write.
    let (source, mut target) = open_checked(platform, &a.src, &a.dst, a.dst_offset)?;
    if let Some(dir) = a.mapfile.parent().filter(|d| !d.as_os_str().is_empty()) {
        if !dir.is_dir() {
            return Err(CliError::Usage(format!(
                "the mapfile's directory '{}' does not exist",
                dir.display()
            )));
        }
    }
    if let Some(m) = existing_canonical(&a.mapfile) {
        if source.identity == Identity::File(m.clone()) || target.identity == Identity::File(m) {
            return Err(CliError::Usage(format!(
                "'{}' is the source or destination; it cannot also be the mapfile",
                a.mapfile.display()
            )));
        }
    }
    let mut opts = opts_from_args(a, source.dev.sector_size())?;
    let len = source.dev.len();
    let shared = SharedSource::new(source.dev);
    if a.used_map {
        let used = fs_used_map(&shared).map_err(|e| {
            CliError::Runtime(Error::Unsupported(format!(
                "--used-map needs a supported filesystem at the start of the source ({e})"
            )))
        })?;
        opts.used_map = Some(used);
        opts.zero_unused = true;
    }

    let pos = Arc::new(AtomicU64::new(0));
    let mut src = PosSource { inner: shared, pos: pos.clone() };
    let status = RescueStatus::new(pos, a.ansi);
    let start = Instant::now();
    let result = rescue(&mut src, &mut *target.dev, a.dst_offset, &opts, &status, cancel);
    status.close();
    let stats = result.map_err(|e| match e {
        Error::Invalid(m) => CliError::Usage(m),
        other => CliError::Runtime(other),
    })?;

    let skipped = len.saturating_sub(stats.rescued).saturating_sub(stats.bad_bytes);
    let mut text = format!(
        "Rescued {} ({} bytes) of {} in {:.1}s\n",
        format_size(stats.rescued),
        stats.rescued,
        format_size(len),
        start.elapsed().as_secs_f64()
    );
    if skipped > 0 {
        text.push_str(&format!("Unused areas skipped: {}\n", format_size(skipped)));
    }
    text.push_str(&format!(
        "errsize: {}, bad areas: {}, read errors: {}, retry passes: {}\n",
        format_size(stats.bad_bytes),
        stats.bad_areas,
        stats.read_errors,
        stats.passes
    ));
    if stats.bad_bytes > 0 {
        text.push_str("Unrecovered areas remain; run again with the same mapfile (more retries with -r) to continue.\n");
    }
    text.push_str(&format!("Mapfile: {}\n", a.mapfile.display()));
    out.write_all(text.as_bytes())
        .map_err(|e| CliError::Runtime(Error::io(0, 0, e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn report(detail: &str) -> ProgressReport {
        ProgressReport {
            phase: "Copying".into(),
            done: 512 << 20,
            total: 1024 << 20,
            rate_bps: (100u64 << 20) as f64,
            eta_secs: Some(65),
            detail: detail.into(),
        }
    }

    #[test]
    fn detail_fields() {
        let d = "errsize 4.00 KB, errors 3, rate 1.00 MB/s";
        assert_eq!(detail_field(d, "errsize"), Some("4.00 KB"));
        assert_eq!(detail_field(d, "errors"), Some("3"));
        assert_eq!(detail_field(d, "rate"), Some("1.00 MB/s"));
        assert_eq!(detail_field(d, "missing"), None);
        assert_eq!(detail_field("", "errors"), None);
    }

    #[test]
    fn block_has_the_ddrescue_fields() {
        let b = render_block(&report("errsize 4.00 KB, errors 3, rate 1.00 MB/s"), 300 << 20).join("\n");
        for needle in ["ipos: 300.00 MB", "phase: Copying", "rescued: 512.00 MB of 1.00 GB (50.0%)", "errsize: 4.00 KB", "errors: 3", "rate: 100.00 MB/s", "ETA: 1:05"] {
            assert!(b.contains(needle), "missing {needle:?} in\n{b}");
        }
        // a report without engine detail still renders
        let b = render_block(&ProgressReport::default(), 0).join("\n");
        assert!(b.contains("(100.0%)") && b.contains("errors: -"), "{b}");
    }

    #[test]
    fn block_redraws_in_place_and_plain_prints_per_phase() {
        let mk = |mode| RescueStatus {
            mode,
            pos: Arc::new(AtomicU64::new(0)),
            state: Mutex::new(DrawState::default()),
        };
        let st = mk(StatusMode::Block);
        let p = report("errsize 0 B, errors 0");
        let first = st.frame(&p, 0);
        assert!(!first.contains("\x1b[") || !first.starts_with("\x1b["), "{first:?}");
        assert_eq!(first.matches('\n').count(), 4);
        let second = st.frame(&p, 1);
        assert!(second.starts_with("\x1b[4A"), "{second:?}");

        let st = mk(StatusMode::Plain);
        assert!(st.frame(&p, 0).ends_with('\n'));
        assert_eq!(st.frame(&p, 0), "", "same phase is not repeated");
        let mut q = p.clone();
        q.phase = "Trimming".into();
        assert!(st.frame(&q, 0).contains("Trimming"));
        q.detail = "errsize 0 B, errors 0, finished".into();
        assert!(st.frame(&q, 0).contains("Trimming"), "the final report is always shown");
    }

    #[test]
    fn mode_selection() {
        use StatusMode::*;
        assert_eq!(choose_mode(true, false, false, None), Block);
        assert_eq!(choose_mode(false, false, true, Some("xterm")), Plain);
        assert_eq!(choose_mode(false, true, true, None), Block);
        assert_eq!(choose_mode(false, true, false, Some("xterm-256color")), Block);
        assert_eq!(choose_mode(false, true, false, Some("dumb")), Line);
        assert_eq!(choose_mode(false, true, false, Some("")), Line);
        assert_eq!(choose_mode(false, true, false, None), Line);
    }

    fn args() -> RescueArgs {
        RescueArgs {
            src: "a".into(),
            dst: "b".into(),
            mapfile: "m".into(),
            sector_size: None,
            cluster_size: None,
            skip_size: None,
            retries: 0,
            reverse: false,
            no_scrape: false,
            no_trim: false,
            dst_offset: 0,
            used_map: false,
            ansi: false,
        }
    }

    #[test]
    fn options_follow_the_flags() {
        let a = RescueArgs {
            sector_size: Some(4096),
            cluster_size: Some(16),
            skip_size: Some(1 << 20),
            retries: 2,
            reverse: true,
            no_trim: true,
            ..args()
        };
        let o = opts_from_args(&a, 512).unwrap();
        assert_eq!((o.sector, o.block_size, o.skip_size, o.max_retries), (4096, 16 * 4096, 1 << 20, 2));
        assert!(o.reverse && o.no_trim && !o.no_scrape && !o.abort_on_error);
        assert_eq!(o.mapfile.as_deref(), Some(Path::new("m")));

        // defaults: the device's sector size and the engine's default read size
        let o = opts_from_args(&args(), 512).unwrap();
        assert_eq!((o.sector, o.block_size), (512, RescueOpts::default().block_size));
        assert_eq!(o.skip_size, RescueOpts::default().skip_size);
        let o = opts_from_args(&args(), 4096).unwrap();
        assert_eq!((o.sector, o.block_size), (4096, RescueOpts::default().block_size));

        // -n is no-scrape, -N is no-trim (checked through the parser in the integration tests)
        let o = opts_from_args(&RescueArgs { no_scrape: true, ..args() }, 512).unwrap();
        assert!(o.no_scrape && !o.no_trim);

        // invalid combinations
        assert!(opts_from_args(&RescueArgs { sector_size: Some(0), ..args() }, 512).is_err());
        assert!(opts_from_args(&RescueArgs { sector_size: Some(100), ..args() }, 512).is_err());
        assert!(opts_from_args(&RescueArgs { sector_size: Some(512), ..args() }, 4096).is_err());
        let huge = RescueArgs { cluster_size: Some(u32::MAX), ..args() };
        assert!(opts_from_args(&huge, 512).is_err());
    }
}
