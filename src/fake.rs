//! A stand-in for `/dev/uhid`, for the unit tests: one end of a datagram
//! socket pair plays the kernel, the other is adopted as a [`Handle`].
//!
//! A datagram socket because uhid is one event per `read()` and per
//! `write()`, and `SOCK_DGRAM` keeps those boundaries where a stream would
//! not. Nothing here checks the 10:239 device number; that is what
//! [`Handle::from_fd_unchecked`] skips.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;

use crate::event::fake::{Sent, decode_sent};
use crate::event::{Buffer, EVENT_SIZE};
use crate::handle::Handle;

/// The kernel's end of the wire.
pub(crate) struct Kernel {
    socket: UnixDatagram,
}

impl Kernel {
    /// A fake kernel and the handle a daemon would hold on it.
    pub(crate) fn new() -> (Self, Handle) {
        let (kernel, daemon) = UnixDatagram::pair().expect("a socket pair");
        // Reading what was *not* written must not hang the test.
        kernel.set_nonblocking(true).expect("non-blocking");
        let handle = Handle::from_fd_unchecked(OwnedFd::from(daemon)).expect("adopting the socket");
        (Self { socket: kernel }, handle)
    }

    /// The next event the daemon wrote, if any.
    pub(crate) fn sent(&self) -> Option<Sent> {
        let mut buf = [0u8; EVENT_SIZE];
        match self.socket.recv(&mut buf) {
            Ok(n) => {
                assert_eq!(n, EVENT_SIZE, "uhid takes whole events only");
                Some(decode_sent(&buf))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => None,
            Err(err) => panic!("reading the daemon's event: {err}"),
        }
    }

    /// Every event the daemon wrote so far.
    pub(crate) fn sent_all(&self) -> Vec<Sent> {
        std::iter::from_fn(|| self.sent()).collect()
    }

    /// The next event the daemon wrote; there has to be one.
    pub(crate) fn expect_sent(&self) -> Sent {
        self.sent()
            .expect("the daemon should have written an event")
    }

    /// Delivers an event to the daemon, as the kernel would.
    pub(crate) fn send(&self, event: &Buffer) {
        let n = self.socket.send(event).expect("delivering an event");
        assert_eq!(n, EVENT_SIZE);
    }

    /// Goes away, as the kernel never does: the daemon's next write fails.
    pub(crate) fn vanish(self) {
        drop(self.socket);
    }
}
