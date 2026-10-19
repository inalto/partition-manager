//! `pm`: the Partition Manager command line.
//!
//! Exit codes: 0 success, 1 usage / invalid argument, 2 platform or runtime error.

mod cli;
mod cmd_doctor;
mod cmd_list;
mod cmd_mount;
mod cmd_table;
mod device_arg;
// Used by the long-running commands added in later tasks.
#[allow(dead_code)]
mod progress;

use clap::Parser;
use cli::{Cli, CliError, Command, EXIT_OK};
use pm_platform::Platform;
use std::io::Write;

fn dispatch(platform: &dyn Platform, command: Command, out: &mut dyn Write) -> Result<(), CliError> {
    match command {
        Command::List { json } => cmd_list::run(platform, json, out),
        Command::Table { device, json } => cmd_table::run(platform, &device, json, out),
        Command::Mount { volume, at } => cmd_mount::run_mount(platform, &volume, &at, out),
        Command::Unmount { volume, at } => cmd_mount::run_unmount(platform, &volume, &at, out),
        Command::Doctor => cmd_doctor::run(platform, out),
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

    let platform = pm_platform::default_platform();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let result = dispatch(platform.as_ref(), cli.command, &mut out);
    let _ = out.flush();
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(e.exit_code());
    }
}
