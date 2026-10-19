//! `partition-manager`: the Partition Manager desktop application.

// Release builds are GUI-subsystem executables (no console window); debug builds keep the
// console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use pm_gui::args::{self, USAGE};
use std::process::ExitCode;

/// GUI-subsystem executables have no console: borrow the parent's, if any, so `--help` and
/// argument errors are visible when started from a terminal.
fn attach_console() {
    #[cfg(windows)]
    // SAFETY: plain Win32 call without pointers; failure (no parent console) is harmless.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

fn main() -> ExitCode {
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
