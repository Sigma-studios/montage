use std::io;

use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, IDLE_PRIORITY_CLASS, PROCESS_MODE_BACKGROUND_BEGIN, SetPriorityClass,
};

/// The idle priority class, then background mode, which also lowers the
/// process's I/O and memory priority.
pub fn lower_priority() -> io::Result<()> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    unsafe {
        let process = GetCurrentProcess();
        if SetPriorityClass(process, IDLE_PRIORITY_CLASS) == 0 {
            return Err(io::Error::last_os_error());
        }
        if SetPriorityClass(process, PROCESS_MODE_BACKGROUND_BEGIN) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn attach_parent_console() {
    // SAFETY: fails harmlessly when there's no parent console.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
