//! Wayland clients: screen capture, cursor capture, virtual pointer.

pub mod capture;
pub mod cursor;
pub mod vpointer;

use std::os::fd::AsFd;
use std::time::Duration;

use anyhow::{Context, Result};
use wayland_client::{Connection, EventQueue};

/// Dispatch events, waiting at most `timeout` for new ones. Returns after one read.
pub fn dispatch_timeout<S>(conn: &Connection, q: &mut EventQueue<S>, state: &mut S, timeout: Duration) -> Result<()> {
    if q.dispatch_pending(state)? > 0 {
        return Ok(());
    }
    conn.flush().context("wayland flush")?;
    if let Some(guard) = q.prepare_read() {
        let ready = {
            let fd = guard.connection_fd();
            let bfd = fd.as_fd();
            let mut fds = [rustix::event::PollFd::new(&bfd, rustix::event::PollFlags::IN)];
            let ts = rustix::time::Timespec {
                tv_sec: timeout.as_secs() as i64,
                tv_nsec: timeout.subsec_nanos() as i64,
            };
            match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(n) => n > 0,
                Err(rustix::io::Errno::INTR) => false,
                Err(e) => return Err(e.into()),
            }
        };
        if ready {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    q.dispatch_pending(state)?;
    Ok(())
}

/// Anonymous shared memory file of `size` bytes, mapped read/write.
pub struct ShmFile {
    pub fd: std::os::fd::OwnedFd,
    ptr: *mut u8,
    pub size: usize,
}

unsafe impl Send for ShmFile {}

impl ShmFile {
    pub fn new(size: usize) -> Result<Self> {
        let fd = rustix::fs::memfd_create("hyprvr-shm", rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING)?;
        rustix::fs::ftruncate(&fd, size as u64)?;
        let ptr = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                size,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
                &fd,
                0,
            )?
        } as *mut u8;
        Ok(ShmFile { fd, ptr, size })
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
    }

}

impl Drop for ShmFile {
    fn drop(&mut self) {
        unsafe {
            let _ = rustix::mm::munmap(self.ptr as *mut _, self.size);
        }
    }
}
