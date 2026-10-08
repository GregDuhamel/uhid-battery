//! An open `/dev/uhid`, opened here or handed over by the service manager.

use std::env;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::Path;

use rustix::fs::{FileType, OFlags, fcntl_getfl, fcntl_setfl, fstat, makedev};
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
        if !is_uhid(&fd)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the descriptor is not the uhid character device",
            ));
        }
        Self::adopt(fd)
    }

    /// Wraps a descriptor already known to be `/dev/uhid`.
    fn adopt(fd: OwnedFd) -> io::Result<Self> {
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
    /// Every other descriptor the service manager passed is closed - one under
    /// another name, or one that turns out not to be `/dev/uhid`. A daemon that
    /// is also handed a socket wants [`Handle::inherited_with_others`] instead.
    /// Whenever the `LISTEN_*` variables are addressed to this process they
    /// are removed, even when they announce no descriptor, so neither a second
    /// call nor a child process claims the same descriptors. Variables meant
    /// for a parent are left alone.
    ///
    /// # Safety
    ///
    /// The variables are removed with [`std::env::remove_var`], which is a
    /// data race against any other thread that reads or writes the
    /// environment - through `std::env`, through `getenv(3)` in a C library,
    /// or by a crate doing either. The caller guarantees that no such thread
    /// exists while this runs, which in practice means calling it at the top
    /// of `main`, before anything is spawned.
    #[allow(unsafe_code)] // Honest about `remove_var`; see the Safety section.
    #[must_use]
    pub unsafe fn inherited(prefix: &str) -> Vec<Self> {
        // SAFETY: the caller upholds the contract of `inherited_with_others`,
        // which is the same as ours.
        unsafe { Self::inherited_with_others(prefix) }.0
    }

    /// As [`Handle::inherited`], but hands back the descriptors it did not
    /// adopt, each with the name it was passed under, instead of closing them.
    ///
    /// The variables are removed all the same, so this is the only chance to
    /// get at those descriptors.
    ///
    /// # Safety
    ///
    /// As for [`Handle::inherited`]: no other thread may read or write the
    /// environment while this runs.
    #[allow(unsafe_code)] // Honest about `remove_var`; see the Safety section.
    #[must_use]
    pub unsafe fn inherited_with_others(prefix: &str) -> (Vec<Self>, Vec<(String, OwnedFd)>) {
        let Some(count) = listen_fds_count() else {
            return (Vec::new(), Vec::new());
        };
        let names = env::var("LISTEN_FDNAMES").unwrap_or_default();
        let mut names = names.split(':');

        let mut handles = Vec::new();
        let mut others = Vec::new();
        for offset in 0..count {
            let name = names.next().unwrap_or_default();
            // SAFETY: by the sd_listen_fds protocol the descriptors in
            // `LISTEN_FDS_START..LISTEN_FDS_START + count` belong to this
            // process, this is the only place that adopts them, and the
            // variables are cleared below so nothing adopts them again.
            let fd = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START + offset) };
            // Passed descriptors come without FD_CLOEXEC. A failure means the
            // environment lied and the number is not an open descriptor: let
            // go of it without closing what is not ours, and stop there, as
            // sd_listen_fds(3) does - the numbers after it are no more
            // trustworthy, and the names no longer line up with anything.
            if fcntl_setfd(&fd, FdFlags::CLOEXEC).is_err() {
                let _ = fd.into_raw_fd();
                break;
            }

            if name.starts_with(prefix) && is_uhid(&fd).unwrap_or(false) {
                // Only fcntl() can still fail, and it took the descriptor.
                if let Ok(handle) = Self::adopt(fd) {
                    handles.push(handle);
                }
            } else {
                others.push((name.to_owned(), fd));
            }
        }

        // SAFETY: the caller guarantees no thread touches the environment.
        unsafe { unset_listen_vars() };
        (handles, others)
    }

    pub(crate) fn write(&self, event: &Buffer) -> io::Result<()> {
        // One event per write(); uhid never accepts a partial one. The write
        // is not blocking, but uhid_char_write() takes the device lock with
        // mutex_lock_interruptible(), so a signal can still make it fail with
        // EINTR before a byte was taken: try again.
        let written = loop {
            match rustix::io::write(&self.fd, event) {
                Err(Errno::INTR) => {}
                result => break result?,
            }
        };
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

/// Whether `fd` is the uhid character device, whatever path it was opened by.
fn is_uhid(fd: &OwnedFd) -> io::Result<bool> {
    let stat = fstat(fd)?;
    Ok(
        FileType::from_raw_mode(stat.st_mode) == FileType::CharacterDevice
            && stat.st_rdev == makedev(UHID_MAJOR, UHID_MINOR),
    )
}

/// How many descriptors `LISTEN_FDS` announces, when the `LISTEN_*` variables
/// are addressed to this process. `None` when they are absent or meant for a
/// parent - and then they are not ours to remove either.
fn listen_fds_count() -> Option<RawFd> {
    let pid: u32 = env::var("LISTEN_PID").ok()?.parse().ok()?;
    if pid != std::process::id() {
        // Meant for a parent of ours; not ours to touch.
        return None;
    }
    // Ours. A missing or unparsable count, or one past the bounds
    // sd_listen_fds(3) applies (the last descriptor number has to be one a
    // descriptor can have), announces nothing - but the variables still get
    // removed, as they would for a plain `LISTEN_FDS=0`.
    let count = env::var("LISTEN_FDS")
        .ok()
        .and_then(|count| count.parse::<RawFd>().ok())
        .filter(|&count| count > 0 && count <= RawFd::MAX - LISTEN_FDS_START)
        .unwrap_or(0);
    Some(count)
}

/// Removes the `LISTEN_*` variables.
///
/// # Safety
///
/// No other thread may read or write the environment while this runs; see
/// [`Handle::inherited`].
#[allow(unsafe_code)] // The only place that edits the environment.
unsafe fn unset_listen_vars() {
    for key in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: the caller guarantees the environment is not being read.
        unsafe {
            env::remove_var(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

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
