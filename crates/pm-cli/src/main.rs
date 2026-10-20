//! `pm`: the Partition Manager command line.
//!
//! Exit codes: 0 success, 1 usage / invalid argument, 2 platform or runtime error.

mod cli;
mod cmd_analyze;
mod cmd_apply;
mod cmd_clone;
mod cmd_doctor;
mod cmd_image;
mod cmd_list;
mod cmd_ls;
mod cmd_mount;
mod cmd_ops;
mod cmd_probe;
mod cmd_recover;
mod cmd_rescue;
mod cmd_table;
mod device_arg;
mod disk_files;
mod partial;
mod progress;

use clap::Parser;
use cli::{Cli, CliError, Command, EXIT_OK};
use pm_core::CancelToken;
use pm_platform::Platform;
use std::io::Write;
use std::sync::Arc;

fn dispatch(
    platform: &dyn Platform,
    on_images: bool,
    command: Command,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    match command {
        Command::List { json } => cmd_list::run(platform, json, out),
        Command::Table { device, json } => cmd_table::run(platform, &device, json, out),
        Command::Mount { volume, at } => cmd_mount::run_mount(platform, &volume, &at, out),
        Command::Unmount { volume, at } => cmd_mount::run_unmount(platform, &volume, &at, out),
        Command::Doctor => cmd_doctor::run(platform, out),
        Command::Clone { src, dst, mode, dst_offset } => {
            cmd_clone::run(platform, &src, &dst, mode.mode(), dst_offset, cancel, out)
        }
        Command::Image { src, file, format, mode, rescue } => match rescue.settings() {
            Some(settings) => {
                let env = cmd_apply::Env { platform, on_images, cancel };
                cmd_image::run_image_rescue(&env, &src, &file, format, mode, settings, out)
            }
            None => cmd_image::run_image(platform, &src, &file, format, mode.mode(), cancel, out),
        },
        Command::Restore { file, dst, dst_offset } => {
            cmd_image::run_restore(platform, &file, &dst, dst_offset, cancel, out)
        }
        Command::Convert { input, output, format } => {
            cmd_image::run_convert(&input, &output, format, cancel, out)
        }
        Command::Rescue(args) => cmd_rescue::run(platform, &args, cancel, out),
        Command::Probe { device, offset } => cmd_probe::run(platform, &device, offset, out),
        Command::Ls { device, path } => cmd_ls::run_ls(platform, &device, path.as_deref(), out),
        Command::Cat { device, path } => cmd_ls::run_cat(platform, &device, &path, out),
        Command::Extract { device, path, dest } => {
            cmd_ls::run_extract(platform, &device, &path, &dest, cancel, out)
        }
        Command::Apply(args) => cmd_apply::run_apply(&cmd_apply::Env { platform, on_images, cancel }, &args, out),
        Command::Op(op) => cmd_ops::run(&cmd_apply::Env { platform, on_images, cancel }, op, out),
        Command::Recover(cmd) => cmd_recover::run(platform, cmd, cancel, out),
        Command::Analyze(cmd) => cmd_analyze::run(platform, cmd, cancel, out),
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { cli::EXIT_USAGE } else { EXIT_OK };
            let _ = e.print();
            std::process::exit(code);
        }
    };

    let on_images = !cli.disk_files.is_empty();
    let platform: Arc<dyn Platform> = if on_images {
        match disk_files::open_platform(cli.disk_files) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(e.exit_code());
            }
        }
    } else {
        pm_platform::default_platform()
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Not triggered yet: a Ctrl-C handler can cancel this token to stop a running copy cleanly.
    let cancel = CancelToken::new();
    let result = dispatch(platform.as_ref(), on_images, cli.command, &cancel, &mut out);
    let _ = out.flush();
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(e.exit_code());
    }
}
