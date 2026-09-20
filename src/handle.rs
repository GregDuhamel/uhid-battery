//! An open `/dev/uhid`, opened here or handed over by the service manager.

use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl, makedev};
use rustix::io::{Errno, FdFlags, fcntl_setfd};

use crate::event::{self, Buffer, EVENT_SIZE, Event};

/// Default path of the uhid character device.
pub const DEV_UHID: &str = "/dev/uhid";

/// `/dev/uhid` is the misc device (major 10) with minor 239 (`UHID_MINOR`).
const UHID_MAJOR: u32 = 10;
const UHID_MINOR: u32 = 239;

/// First descriptor number passed by the service manager (`sd_listen_fds(3)`).
const LISTEN_FDS_START: RawFd = 3;

/// An open handle on `/dev/uhid`, able to back one virtual device at a time.
///
/// A handle outlives the devices created on it: destroying a device leaves the
/// handle ready for another. That matters when it came from the service
/// manager, because then it cannot be opened again - which is why the fallible
/// operations of this crate hand the handle back instead of dropping it.
#[derive(Debug)]
pub struct Handle {
    fd: OwnedFd,
}

impl Handle {
    /// Opens the uhid character device at `path` (normally [`DEV_UHID`]).
    ///
    /// # Errors
    ///
    /// Fails if the node cannot be opened read/write - the node is
    /// `root:root 0600`, so that means the process is not root - or if `path`
    /// is not the uhid device.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::from_fd(OwnedFd::from(file))
    }

    /// Adopts an already open descriptor.
    ///
    /// # Errors
    ///
    /// Fails if the descriptor is not `/dev/uhid`: an inherited descriptor could
    /// be anything, and uhid events must not be written into something else.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let file = File::from(fd);
        let meta = file.metadata()?;
        if !meta.file_type().is_char_device() || meta.rdev() != makedev(UHID_MAJOR, UHID_MINOR) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the descriptor is not the uhid character device",
            ));
        }

        let fd = OwnedFd::from(file);
        let flags = fcntl_getfl(&fd)?;
        fcntl_setfl(&fd, flags | OFlags::NONBLOCK)?;
        Ok(Self { fd })
    }

    /// Takes the descriptors the service manager passed under a name starting
    /// with `prefix`, for instance `uhid` for a unit that says
    /// `OpenFile=/dev/uhid:uhid`.
    ///
    /// This is what lets a daemon run unprivileged: `/dev/uhid` allows its
    /// holder to create arbitrary input devices, keyboards included, so rather
    /// than widen the node's permissions the unit has systemd open it and pass
    /// the descriptor down.
    ///
    /// Returns an empty vector when the process was not started that way.
    /// Descriptors passed under another name are closed, as are those that turn
    /// out not to be `/dev/uhid`; the `LISTEN_*` variables are removed so
    /// neither a second call nor a child process claims the same descriptors.
    ///
    /// Call this early, before spawning threads: it edits the environment.
    #[must_use]
    pub fn inherited(prefix: &str) -> Vec<Self> {
        let Some(count) = listen_fds_count() else {
            return Vec::new();
        };
        let names = env::var("LISTEN_FDNAMES").unwrap_or_default();
        let mut names = names.split(':');

        let handles = (0..count)
            .filter_map(|offset| {
                // SAFETY: by the sd_listen_fds protocol the descriptors in
                // `LISTEN_FDS_START..LISTEN_FDS_START + count` belong to this
                // process, this is the only place that adopts them, and the
                // variables are cleared below so nothing adopts them again.
                #[allow(unsafe_code)]
                let fd = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START + offset) };
                // Passed descriptors come without FD_CLOEXEC.
                let _ = fcntl_setfd(&fd, FdFlags::CLOEXEC);

                names
                    .next()
                    .unwrap_or_default()
                    .starts_with(prefix)
                    .then(|| Self::from_fd(fd).ok())
                    .flatten()
            })
            .collect();

        unset_listen_vars();
        handles
    }

    pub(crate) fn write(&self, event: &Buffer) -> io::Result<()> {
        // One event per write(); uhid never accepts a partial one.
        let written = rustix::io::write(&self.fd, event)?;
        if written == EVENT_SIZE {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short write to uhid: {written} of {EVENT_SIZE} bytes"),
            ))
        }
    }

    /// Reads the next pending event, or `None` when the queue is empty.
    pub(crate) fn read(&self) -> io::Result<Option<Event>> {
        let mut buf = [0u8; EVENT_SIZE];
        match rustix::io::read(&self.fd, &mut buf) {
            Ok(0) | Err(Errno::AGAIN | Errno::INTR) => Ok(None),
            Ok(_) => Ok(Some(event::decode(&buf))),
            Err(err) => Err(err.into()),
        }
    }

    /// Discards whatever the kernel queued for a device that is gone, so it is
    /// not mistaken for news about the next device created on this handle.
    pub(crate) fn drain(&self) {
        while let Ok(Some(_)) = self.read() {}
    }
}

impl AsFd for Handle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

fn listen_fds_count() -> Option<RawFd> {
    let pid: u32 = env::var("LISTEN_PID").ok()?.parse().ok()?;
    if pid != std::process::id() {
        // Meant for a parent of ours; not ours to touch.
        return None;
    }
    let count: RawFd = env::var("LISTEN_FDS").ok()?.parse().ok()?;
    (count > 0).then_some(count)
}

fn unset_listen_vars() {
    for key in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: `remove_var` is only unsound while another thread reads the
        // environment, and `inherited` is documented to run before any exists.
        #[allow(unsafe_code)]
        unsafe {
            env::remove_var(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_that_is_not_uhid_is_refused() {
        // /dev/null is a character device, but not the right one.
        let null = OwnedFd::from(File::open("/dev/null").unwrap());
        let err = Handle::from_fd(null).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let regular = OwnedFd::from(File::open("/proc/self/status").unwrap());
        assert!(Handle::from_fd(regular).is_err());
    }

    #[test]
    fn opening_a_path_that_is_not_uhid_fails() {
        assert!(Handle::open("/dev/null").is_err());
        assert!(Handle::open("/nonexistent/uhid").is_err());
    }
}
