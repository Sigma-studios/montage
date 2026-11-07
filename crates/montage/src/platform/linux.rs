use std::io;

/// nice 19 and ionice idle, as `gsr-clip-export` ran.
pub fn lower_priority() -> io::Result<()> {
    // SAFETY: plain syscalls on the calling thread, with constant arguments.
    unsafe {
        if libc::setpriority(libc::PRIO_PROCESS, 0, 19) != 0 {
            return Err(io::Error::last_os_error());
        }
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
        let idle = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;
        if libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, idle) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
