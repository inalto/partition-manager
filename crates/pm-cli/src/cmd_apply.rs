//! `pm apply <ops.json>` and the shared run logic of every operation subcommand.
//!
//! The flow is always the same: print what will happen, validate the whole list against the
//! current disks (`Planner::simulate_all`, then a dry run of the executor, which also applies
//! the system-disk guard), and only then, and only with `--yes`, write anything.

use crate::cli::CliError;
use crate::progress::StderrProgress;
use clap::Args;
use pm_core::{format_size, CancelToken, Error, NullProgress};
use pm_ops::{execute, ExecOpts, Model, Operation, Planner};
use pm_platform::Platform;
use std::io::Write;
use std::path::PathBuf;

/// What the command runs against.
pub struct Env<'a> {
    pub platform: &'a dyn Platform,
    /// The disks are image files (`--disk-file`): operations that need a volume layer are
    /// refused up front.
    pub on_images: bool,
    pub cancel: &'a CancelToken,
}

/// Flags shared by `apply` and every operation subcommand.
#[derive(Args, Debug, Clone, Copy)]
pub struct RunFlags {
    /// Validate and show what would be done; write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Apply the operations (without it nothing is written; there is no interactive prompt)
    #[arg(long)]
    pub yes: bool,
    /// Let resize, label, drive letter, mount and creating a partition in free space touch the
    /// running system disk (destructive operations stay refused)
    #[arg(long)]
    pub allow_system_disk: bool,
}

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// JSON file holding a list of operations (or a single operation object)
    pub file: PathBuf,
    #[command(flatten)]
    pub run: RunFlags,
}

fn write_err(e: std::io::Error) -> CliError {
    CliError::Runtime(Error::io(0, 0, e))
}

/// Planning and argument errors are the user's to fix (exit 1); anything else is a runtime failure.
fn plan_error(e: Error) -> CliError {
    match e {
        Error::Invalid(m) | Error::NotFound(m) => CliError::Usage(m),
        other => CliError::Runtime(other),
    }
}

/// Parse the contents of an operations file: a JSON array of operations or one operation.
pub fn parse_ops(text: &str) -> Result<Vec<Operation>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    match v {
        serde_json::Value::Array(_) => serde_json::from_value(v).map_err(|e| e.to_string()),
        other => serde_json::from_value::<Operation>(other).map(|o| vec![o]).map_err(|e| e.to_string()),
    }
}

pub fn run_apply(env: &Env, args: &ApplyArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let text = std::fs::read_to_string(&args.file)
        .map_err(|e| CliError::Usage(format!("cannot read '{}': {e}", args.file.display())))?;
    let ops = parse_ops(&text)
        .map_err(|e| CliError::Usage(format!("'{}' is not a valid operations file: {e}", args.file.display())))?;
    run_ops(env, ops, &args.run, out)
}

/// What an image file cannot do: everything that needs the operating system's volumes.
fn check_supported_on_images(model: &Model, ops: &[Operation]) -> Result<(), CliError> {
    use pm_core::FsKind;
    use pm_ops::BootFix;
    for op in ops {
        let needs_volumes = match op {
            // NTFS / ReFS follow a length change through the volume layer
            Operation::ResizeMove { disk, number, new_len, .. } => model
                .partition(*disk, *number)
                .is_some_and(|p| p.len != *new_len && matches!(p.fs, Some(FsKind::Ntfs | FsKind::ReFs))),
            // alignment shrinks partitions, and nothing here can shrink a file system with them
            Operation::AlignPartitions { .. } => true,
            Operation::CreatePartition { fs, letter, .. } => fs.is_some() || letter.is_some(),
            Operation::FormatPartition { .. }
            | Operation::SetLetter { .. }
            | Operation::SetLabel { .. }
            | Operation::Mount { .. }
            | Operation::Unmount { .. } => true,
            Operation::CloneDisk { boot_fix, .. } | Operation::MigrateOs { boot_fix, .. } => {
                *boot_fix != BootFix::None
            }
            _ => false,
        };
        if needs_volumes {
            return Err(CliError::Usage(format!(
                "{}: not available on image files (it needs a volume layer)",
                op.summary()
            )));
        }
    }
    Ok(())
}

/// Validate `ops`, show them, and (with `--yes`) run them. See the module documentation.
pub fn run_ops(env: &Env, ops: Vec<Operation>, flags: &RunFlags, out: &mut dyn Write) -> Result<(), CliError> {
    if ops.is_empty() {
        return Err(CliError::Usage("the operations list is empty".into()));
    }
    let model = Model::from_platform(env.platform)?;
    if env.on_images {
        check_supported_on_images(&model, &ops)?;
    }
    let mut text = String::from("Operations:\n");
    for (i, op) in ops.iter().enumerate() {
        text.push_str(&format!("  {}. {}\n", i + 1, op.summary()));
    }
    out.write_all(text.as_bytes()).map_err(write_err)?;

    Planner::simulate_all(&model, &ops).map_err(plan_error)?;

    // the dry run re-checks every operation against the model its predecessors produce and
    // applies the system-disk guard, so a refusal is reported before anything is written
    let preview = ExecOpts { dry_run: true, allow_system_disk: flags.allow_system_disk };
    let report = execute(env.platform, &ops, &preview, &NullProgress, env.cancel)?;
    if let Some((_, msg)) = &report.failed {
        return Err(CliError::Usage(msg.clone()));
    }
    if flags.dry_run {
        return print_log(out, &report.log);
    }
    if !flags.yes {
        return writeln!(out, "Re-run with --yes to apply").map_err(write_err);
    }

    let opts = ExecOpts { dry_run: false, allow_system_disk: flags.allow_system_disk };
    let report = execute(env.platform, &ops, &opts, &StderrProgress::new(), env.cancel)?;
    print_log(out, &report.log)?;
    for st in &report.rescue_stats {
        if st.bad_bytes > 0 || st.read_errors > 0 {
            writeln!(
                out,
                "Unreadable data: {} in {} area(s), {} read error(s); {} rescued",
                format_size(st.bad_bytes),
                st.bad_areas,
                st.read_errors,
                format_size(st.rescued)
            )
            .map_err(write_err)?;
        }
    }
    if let Some((i, msg)) = report.failed {
        return Err(CliError::Failed(format!(
            "operation {} of {} failed ({} completed): {msg}",
            i + 1,
            ops.len(),
            report.completed
        )));
    }
    writeln!(out, "Done: {} of {} operation(s) completed", report.completed, ops.len()).map_err(write_err)
}

fn print_log(out: &mut dyn Write, log: &[String]) -> Result<(), CliError> {
    for line in log {
        writeln!(out, "{line}").map_err(write_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::TableKind;

    #[test]
    fn ops_file_accepts_array_or_single_object() {
        let one = r#"{"InitializeDisk": {"disk": 0, "kind": "Gpt"}}"#;
        let ops = parse_ops(one).unwrap();
        assert_eq!(ops, vec![Operation::InitializeDisk { disk: 0, kind: TableKind::Gpt }]);
        let many = format!("[{one}, {one}]");
        assert_eq!(parse_ops(&many).unwrap().len(), 2);
        assert!(parse_ops("[]").unwrap().is_empty());
        assert!(parse_ops("nonsense").is_err());
        assert!(parse_ops(r#"{"Bogus": {}}"#).is_err());
    }

    #[test]
    fn volume_operations_are_refused_on_images() {
        let fmt = Operation::FormatPartition {
            disk: 0,
            number: 1,
            fs: pm_core::FsKind::Ntfs,
            label: String::new(),
            cluster: None,
            quick: true,
        };
        let model = Model::default();
        assert!(
            matches!(check_supported_on_images(&model, &[fmt]), Err(CliError::Usage(m)) if m.contains("image files"))
        );
        let init = Operation::InitializeDisk { disk: 0, kind: TableKind::Mbr };
        assert!(check_supported_on_images(&model, &[init]).is_ok());
        let align = Operation::AlignPartitions { disk: 0 };
        assert!(check_supported_on_images(&model, &[align]).is_err());
    }
}
