//! A stand-in for `/dev/uhid`: one end of a datagram socket pair plays the
//! kernel, the other is adopted as a [`Handle`].
//!
//! This is what the unit tests of this crate drive a [`Battery`] through, and
//! under the `fake` feature a daemon built on the crate can do the same in its
//! own unit tests: no `/dev/uhid`, no root, and the daemon's real code path
//! from `create` to `destroy` - every event it writes is read back here, and
//! the kernel's requests (`UHID_START`, `GET_REPORT`) are sent from here.
//! **Tests only**: enable the feature from the dev-dependencies alone, and
//! expect nothing here to be covered by the API's stability promise.
//!
//! A datagram socket because uhid is one event per `read()` and per
//! `write()`, and `SOCK_DGRAM` keeps those boundaries where a stream would
//! not. Nothing here checks the 10:239 device number; that is what
//! [`Handle::from_fd_unchecked`] skips.
//!
//! What the fake cannot do is what the kernel does beyond the wire: build a
//! power supply from the descriptor and list it under sysfs. A daemon that
//! looks for the supply ([`Battery::wait_for_power_supply`]) has to be told
//! not to in its tests.
//!
//! [`Battery`]: crate::Battery
//! [`Battery::wait_for_power_supply`]: crate::Battery::wait_for_power_supply

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;

use crate::event::fake::decode_sent;
pub use crate::event::fake::{Sent, get_report, set_report, start};
pub use crate::event::{Buffer, EVENT_SIZE, RTYPE_INPUT};
use crate::handle::Handle;

/// The kernel's end of the wire.
#[derive(Debug)]
pub struct Kernel {
    socket: UnixDatagram,
}

impl Kernel {
    /// A fake kernel and the handle a daemon would hold on it.
    ///
    /// # Panics
    ///
    /// Panics if the socket pair cannot be made: a test has nothing better
    /// to do about that.
    #[must_use]
    pub fn new() -> (Self, Handle) {
        let (kernel, daemon) = UnixDatagram::pair().expect("a socket pair");
        // Reading what was *not* written must not hang the test.
        kernel.set_nonblocking(true).expect("non-blocking");
        let handle = Handle::from_fd_unchecked(OwnedFd::from(daemon)).expect("adopting the socket");
        (Self { socket: kernel }, handle)
    }

    /// The next event the daemon wrote, if any.
    ///
    /// # Panics
    ///
    /// Panics if reading the socket fails, or if what came out is not a
    /// whole event: uhid takes nothing else.
    #[must_use]
    pub fn sent(&self) -> Option<Sent> {
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
    #[must_use]
    pub fn sent_all(&self) -> Vec<Sent> {
        std::iter::from_fn(|| self.sent()).collect()
    }

    /// The next event the daemon wrote; there has to be one.
    ///
    /// # Panics
    ///
    /// Panics if the daemon wrote nothing.
    #[must_use]
    pub fn expect_sent(&self) -> Sent {
        self.sent()
            .expect("the daemon should have written an event")
    }

    /// Delivers an event to the daemon, as the kernel would.
    ///
    /// # Panics
    ///
    /// Panics if the daemon's end is gone, or the event was not taken whole.
    pub fn send(&self, event: &Buffer) {
        let n = self.socket.send(event).expect("delivering an event");
        assert_eq!(n, EVENT_SIZE);
    }

    /// Goes away, as the kernel never does: the daemon's next write fails.
    pub fn vanish(self) {
        drop(self.socket);
    }
}
