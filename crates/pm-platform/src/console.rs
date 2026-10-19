//! Console helpers for GUI-subsystem executables.

/// Attach the calling process to the console of its parent process, if it has one, so a
/// GUI-subsystem executable started from a terminal can print `--help` and errors there.
/// Returns `true` when a console was attached. A no-op returning `false` outside Windows.
pub fn attach_parent_console() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        // SAFETY: AttachConsole takes a process id by value and touches no caller memory;
        // failure (no parent console, already attached) is reported through the return value.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn attach_is_harmless_when_already_attached_or_absent() {
        // Under the test harness the process either has a console already (attach fails) or
        // none at all; both must simply return without panicking.
        let _ = super::attach_parent_console();
    }
}
