use std::io;

// From <sys/resource.h>; the libc crate doesn't bind them.
const IOPOL_TYPE_DISK: libc::c_int = 0;
const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
const IOPOL_THROTTLE: libc::c_int = 3;

unsafe extern "C" {
    fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int) -> libc::c_int;
}

/// nice 19 and throttled disk I/O.
pub fn lower_priority() -> io::Result<()> {
    // SAFETY: plain calls on the current process, with constant arguments.
    unsafe {
        if libc::setpriority(libc::PRIO_PROCESS, 0, 19) != 0 {
            return Err(io::Error::last_os_error());
        }
        if setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_THROTTLE) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
