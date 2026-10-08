//! A one-function wrapper over `poll(2)`, speaking [`Duration`].
//!
//! [`crate::serve_all`] is built on it, and it is public for daemons that
//! watch other descriptors - a dongle's hidraw node, a socket - next to their
//! batteries, so they do not have to write the same ten lines. [`PollFd`] and
//! [`PollFlags`] are re-exported here so that such a daemon need not depend on
//! `rustix` itself.

use std::io;
use std::time::Duration;

use rustix::event::Timespec;
pub use rustix::event::{PollFd, PollFlags};

/// Waits until one of `fds` is ready, or `timeout` elapses.
///
/// Returns the number of ready descriptors, which is `0` on timeout. A
/// timeout too long for the kernel's clock is as good as none.
///
/// # Errors
///
/// Propagates `poll(2)` failures, including [`io::ErrorKind::Interrupted`]
/// when a signal arrives: `poll(2)` is never restarted after a handler ran,
/// whatever `SA_RESTART` says, which is what makes it the right way for a
/// daemon to sleep.
pub fn poll(fds: &mut [PollFd<'_>], timeout: Duration) -> io::Result<usize> {
    poll_until(fds, Some(timeout))
}

/// As [`poll`], blocking for ever when `timeout` is `None`.
pub(crate) fn poll_until(fds: &mut [PollFd<'_>], timeout: Option<Duration>) -> io::Result<usize> {
    let timeout = timeout.map(|timeout| Timespec {
        tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: timeout.subsec_nanos().into(),
    });
    rustix::event::poll(fds, timeout.as_ref()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;

    use super::*;

    #[test]
    fn a_readable_descriptor_is_reported_and_a_timeout_is_zero() {
        let (left, right) = UnixDatagram::pair().unwrap();
        let mut fds = [PollFd::new(&left, PollFlags::IN)];
        assert_eq!(poll(&mut fds, Duration::from_millis(10)).unwrap(), 0);

        right.send(b"x").unwrap();
        assert_eq!(poll(&mut fds, Duration::from_secs(5)).unwrap(), 1);
        assert!(fds[0].revents().contains(PollFlags::IN));
    }
}
