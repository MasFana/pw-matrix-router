//! Best-effort Linux pipe sizing and per-thread priority, with observed results.

use std::io;
use std::os::fd::AsRawFd;

pub(crate) fn configure_pipe_size(file: &impl AsRawFd, size_bytes: usize) -> io::Result<usize> {
    let size = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_SETPIPE_SZ,
            size_bytes.min(i32::MAX as usize) as i32,
        )
    };
    if size < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(size as usize)
    }
}

pub(crate) fn nonblocking(file: &impl AsRawFd) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct PriorityGuard {
    tid: libc::id_t,
    original: i32,
    pub(crate) nice: i32,
    pub(crate) realtime: bool,
}

impl PriorityGuard {
    pub(crate) fn new() -> Self {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::id_t };
        unsafe {
            *libc::__errno_location() = 0;
        }
        let original = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid) };
        let valid = unsafe { *libc::__errno_location() == 0 };
        if valid {
            let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, -15) };
            if result != 0 {
                unsafe {
                    libc::setpriority(libc::PRIO_PROCESS, tid, -5);
                }
            }
        }
        let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid) };
        let policy = unsafe { libc::sched_getscheduler(tid as libc::pid_t) };
        let realtime = policy == libc::SCHED_FIFO || policy == libc::SCHED_RR;
        tracing::debug!(tid, nice, realtime, "Observed audio thread scheduling");
        Self {
            tid,
            original: if valid { original } else { nice },
            nice,
            realtime,
        }
    }
}

impl Drop for PriorityGuard {
    fn drop(&mut self) {
        if self.nice != self.original {
            let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, self.tid, self.original) };
            if result != 0 {
                tracing::debug!(error = %io::Error::last_os_error(), "Could not restore audio thread nice value");
            }
        }
    }
}
