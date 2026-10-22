//! `partition-manager`: the Partition Manager desktop application.

// Windows release builds are GUI-subsystem executables (no console window); debug builds keep
// the console for logs. Other OSes have no such subsystem.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

use pm_gui::args::{self, USAGE};
use std::process::ExitCode;

/// GUI-subsystem executables have no console: borrow the parent's, if any, so `--help` and
/// argument errors are visible when started from a terminal.
fn attach_console() {
    pm_platform::console::attach_parent_console();
}

fn main() -> ExitCode {
    pm_gui::logging::init();
    let args = match args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            attach_console();
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.help {
        attach_console();
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match pm_gui::main_window::run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            attach_console();
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
