//! An open `/dev/uhid`, opened here or handed over by the service manager.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{FileType, OFlags, fcntl_getfl, fcntl_setfl, fstat, makedev};
use rustix::io::Errno;

use crate::event::{self, Buffer, EVENT_SIZE, Event};
use crate::listen_fds;

/// Default path of the uhid character device.
pub const DEV_UHID: &str = "/dev/uhid";

/// `/dev/uhid` is the misc device (major 10) with minor 239 (`UHID_MINOR`).
const UHID_MAJOR: u32 = 10;
const UHID_MINOR: u32 = 239;

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

    /// Adopts `fd` without checking that it is `/dev/uhid`, so that a unit
    /// test can stand a socket in for the kernel (see [`crate::fake`]).
    ///
    /// **Tests only.** Writing uhid events into a descriptor that is not
    /// `/dev/uhid` is exactly what [`Handle::from_fd`] exists to prevent.
    /// This is behind the `fake` feature so that no daemon reaches it by
    /// accident: enable the feature from the dev-dependencies alone.
    ///
    /// # Errors
    ///
    /// Fails if the descriptor cannot be made non-blocking.
    #[cfg(any(test, feature = "fake"))]
    pub fn from_fd_unchecked(fd: OwnedFd) -> io::Result<Self> {
        Self::adopt(fd)
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
    /// is also handed a socket wants [`Handle::inherited_with_others`] instead,
    /// and one with its own ideas about names can filter
    /// [`listen_fds::take`] itself and go through [`Handle::from_fd`]. The
    /// `LISTEN_*` variables are removed as [`listen_fds::take`] describes.
    ///
    /// # Safety
    ///
    /// As for [`listen_fds::take`]: the variables are removed with
    /// [`std::env::remove_var`], which is a data race against any other thread
    /// that reads or writes the environment. The caller guarantees that no
    /// such thread exists while this runs, which in practice means calling it
    /// at the top of `main`, before anything is spawned.
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
        // SAFETY: the caller upholds the contract of `take`, which is ours.
        let passed = unsafe { listen_fds::take() };
        Self::adopt_named(passed, prefix)
    }

    /// Sorts passed descriptors into handles - those named with `prefix` that
    /// really are `/dev/uhid` - and the rest.
    fn adopt_named(
        passed: Vec<(String, OwnedFd)>,
        prefix: &str,
    ) -> (Vec<Self>, Vec<(String, OwnedFd)>) {
        let mut handles = Vec::new();
        let mut others = Vec::new();
        for (name, fd) in passed {
            if name.starts_with(prefix) && is_uhid(&fd).unwrap_or(false) {
                // Only fcntl() can still fail, and it took the descriptor.
                if let Ok(handle) = Self::adopt(fd) {
                    handles.push(handle);
                }
            } else {
                others.push((name, fd));
            }
        }
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
    fn passed_descriptors_are_sorted_by_name_and_by_what_they_are() {
        // Nothing here is uhid, so every descriptor comes back as "other" -
        // including the one named right: the name is not proof.
        let null = || OwnedFd::from(File::open("/dev/null").unwrap());
        let passed = vec![
            ("uhid".to_owned(), null()),
            ("socket".to_owned(), null()),
            (String::new(), null()),
        ];
        let (handles, others) = Handle::adopt_named(passed, "uhid");
        assert_eq!(handles.len(), 0);
        let names: Vec<&str> = others.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["uhid", "socket", ""]);
    }

    #[test]
    fn opening_a_path_that_is_not_uhid_fails() {
        assert!(Handle::open("/dev/null").is_err());
        assert!(Handle::open("/nonexistent/uhid").is_err());
    }
}
