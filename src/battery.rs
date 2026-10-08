//! A live virtual battery.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};

use crate::descriptor::Kind;
use crate::event::{self, Event, RTYPE_INPUT};
use crate::handle::Handle;
use crate::sysfs;

/// uhid registers the device from a worker, and hid-core drops input reports -
/// silently, the write still succeeds - until the driver has finished probing.
/// `UHID_START` arrives part-way through the probe, so the level is pushed
/// again this long after it. Should that still be too early nothing breaks:
/// the kernel asks for the level (`GET_REPORT`) until a push lands.
const START_SETTLE_DELAY: Duration = Duration::from_millis(250);

/// `hid-input` rate-limits the uevents it raises for battery reports: a level
/// that did not change is only announced again 30 s after the last
/// announcement. Whether a charging flip at an unchanged level escapes that
/// window depends on the kernel (7.2 announces it at once; older ones update
/// sysfs quietly, so UPower never reads it again). Pushing the same report once
/// the window has closed costs one redundant uevent at worst.
const CHARGE_FLIP_REPUSH_DELAY: Duration = Duration::from_secs(31);

/// How often [`Battery::wait_for_power_supply`] looks.
const REGISTRATION_POLL: Duration = Duration::from_millis(20);

/// How the virtual device introduces itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Device name. It reaches UPower as the model, which is the label desktops
    /// show, so make it the product's marketing name.
    pub name: String,
    /// Physical path, free-form. A udev rule can match on it, which is what
    /// [`Kind::udev_rule`] relies on; `<daemon>/<device>` is a good shape.
    pub phys: String,
    /// Unique ID. The kernel names the power supply `hid-<uniq>-battery[-<n>]`,
    /// so it has to be unique on the machine and safe as a file name and in a
    /// udev or shell match: 1 to 63 characters from `[A-Za-z0-9._-]`. Never
    /// put a string that came from the device in here.
    pub uniq: String,
    /// Vendor ID, usually mirrored from the real device.
    pub vendor: u32,
    /// Product ID, usually mirrored from the real device.
    pub product: u32,
}

impl Identity {
    /// Checks that the kernel will take the strings as they are.
    ///
    /// Each one lands in a fixed-size, NUL-terminated field. A string that did
    /// not fit would be cut short without a word, and for `uniq` that means a
    /// power supply under a name [`Battery::power_supply`] never looks for.
    /// `uniq` is held to `[A-Za-z0-9._-]` besides: it becomes a directory name
    /// under `/sys/class/power_supply`, and whitespace, a newline or a glob
    /// character in there would make a mess of every udev rule or shell
    /// snippet that matches on it.
    fn validate(&self) -> io::Result<()> {
        let fits = |value: &str, field: usize| value.len() < field && !value.contains('\0');
        if self.name.is_empty() || !fits(&self.name, event::LEN_NAME) {
            return Err(invalid("the name must be 1 to 127 bytes, without NUL"));
        }
        if !fits(&self.phys, event::LEN_PHYS) {
            return Err(invalid("phys must be under 64 bytes, without NUL"));
        }
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
        if self.uniq.is_empty()
            || !fits(&self.uniq, event::LEN_UNIQ)
            || !self.uniq.bytes().all(plain)
        {
            return Err(invalid(
                "uniq must be 1 to 63 characters from [A-Za-z0-9._-]",
            ));
        }
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// A failed [`Battery::create`]. It carries the handle back, because a handle
/// that came from the service manager cannot be opened again.
#[derive(Debug)]
pub struct CreateError {
    handle: Handle,
    source: io::Error,
}

impl CreateError {
    /// Recovers the handle and the underlying error.
    #[must_use]
    pub fn into_parts(self) -> (Handle, io::Error) {
        (self.handle, self.source)
    }
}

impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not create the virtual battery: {}", self.source)
    }
}

impl std::error::Error for CreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// A virtual HID device exposing one battery.
///
/// The kernel tears the device - and its power supply - down when the handle is
/// closed, so dropping this, or the process dying, is cleanup enough. Use
/// [`Battery::destroy`] to keep the handle for a later device.
///
/// Something has to keep calling [`Battery::service`] (or
/// [`Battery::serve_until`]) while the battery exists: reading the level from
/// sysfs can make the kernel ask *us* for it, and it waits up to five seconds
/// for the answer, holding up whoever was reading.
#[derive(Debug)]
pub struct Battery {
    handle: Handle,
    kind: Kind,
    uniq: String,
    percent: u8,
    charging: bool,
    repush_at: Option<Instant>,
}

impl Battery {
    /// Creates the virtual device and pushes a first reading.
    ///
    /// The power supply shows up a moment later; see
    /// [`Battery::wait_for_power_supply`].
    ///
    /// # Errors
    ///
    /// Fails if `identity` holds a string the kernel would truncate or that
    /// cannot name a power supply, or if the kernel refuses the device, for
    /// instance because one already exists on this handle. The handle comes
    /// back in the error.
    pub fn create(
        handle: Handle,
        identity: &Identity,
        kind: Kind,
        percent: u8,
        charging: bool,
    ) -> Result<Self, CreateError> {
        let created = identity
            .validate()
            .and_then(|()| {
                event::create2(
                    &identity.name,
                    &identity.phys,
                    &identity.uniq,
                    identity.vendor,
                    identity.product,
                    kind.descriptor(),
                )
            })
            .and_then(|event| handle.write(&event));
        if let Err(source) = created {
            return Err(CreateError { handle, source });
        }

        let battery = Self {
            handle,
            kind,
            uniq: identity.uniq.clone(),
            percent: percent.min(100),
            charging,
            repush_at: None,
        };
        // Most likely dropped - the probe has barely begun - but harmless, and
        // the settle push scheduled on UHID_START makes up for it.
        match battery.push() {
            Ok(()) => Ok(battery),
            Err(source) => Err(CreateError {
                handle: battery.destroy(),
                source,
            }),
        }
    }

    /// Publishes a new reading. Pushing an unchanged one is fine, and a good
    /// idea now and then: the kernel rate-limits the resulting uevents itself.
    ///
    /// # Errors
    ///
    /// Fails if the write to `/dev/uhid` fails.
    pub fn update(&mut self, percent: u8, charging: bool) -> io::Result<()> {
        let percent = percent.min(100);
        if charging != self.charging && percent == self.percent {
            self.schedule_repush(CHARGE_FLIP_REPUSH_DELAY);
        }
        self.percent = percent;
        self.charging = charging;
        self.push()
    }

    /// Answers whatever the kernel has asked since the last call, and pushes
    /// the level again if a push is due. Never blocks.
    ///
    /// # Errors
    ///
    /// Fails if reading from or writing to `/dev/uhid` fails.
    pub fn service(&mut self) -> io::Result<()> {
        while let Some(event) = self.handle.read()? {
            match event {
                Event::Start => self.schedule_repush(START_SETTLE_DELAY),
                Event::GetReport { id, rnum, rtype } => {
                    let ours = rnum == self.kind.report_id() && rtype == RTYPE_INPUT;
                    let report = self.report();
                    let reply = event::get_report_reply(id, ours.then_some(&report[..]))?;
                    self.handle.write(&reply)?;
                }
                Event::SetReport { id } => self.handle.write(&event::set_report_reply(id))?,
                Event::Ignored => {}
            }
        }

        if self.repush_at.is_some_and(|due| Instant::now() >= due) {
            self.repush_at = None;
            self.push()?;
        }
        Ok(())
    }

    /// When [`Battery::service`] next has something to do on its own, if ever.
    ///
    /// For callers running their own `poll()` loop over [`Battery::as_fd`]:
    /// wake up no later than this.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.repush_at
    }

    /// Services the battery until `deadline`.
    ///
    /// `wake`, if given, is watched too: the call returns `true` as soon as it
    /// is readable (it is never read here), and `false` at the deadline - or
    /// earlier if a signal interrupts the wait, so the caller gets to look at
    /// whatever flag its handler raised. The battery is serviced once more
    /// before any of those returns, so the kernel is never left waiting on an
    /// answer while the caller does its own thing.
    ///
    /// # Errors
    ///
    /// Fails if servicing the battery or waiting on it fails, or if `wake` is
    /// hung up, in error or not an open descriptor: such a descriptor is
    /// reported by every `poll()` and never becomes readable, so returning
    /// `true` for it would have the caller spin.
    pub fn serve_until(
        &mut self,
        deadline: Instant,
        wake: Option<BorrowedFd<'_>>,
    ) -> io::Result<bool> {
        loop {
            self.service()?;

            let now = Instant::now();
            let until = self.repush_at.map_or(deadline, |due| due.min(deadline));
            let timeout = until.saturating_duration_since(now);

            match self.wait(wake, timeout)? {
                Wakeup::Device => {}
                Wakeup::Wake => {
                    self.service()?;
                    return Ok(true);
                }
                Wakeup::Signal => {
                    self.service()?;
                    return Ok(false);
                }
            }

            if Instant::now() >= deadline {
                self.service()?;
                return Ok(false);
            }
        }
    }

    /// One `poll()` over the device and, if given, `wake`, for at most
    /// `timeout`.
    fn wait(&self, wake: Option<BorrowedFd<'_>>, timeout: Duration) -> io::Result<Wakeup> {
        let handle = self.handle.as_fd();
        // A fixed pair rather than a Vec: this runs on every turn of the loop.
        // Without a wake descriptor the second slot holds the device again,
        // and the slice handed to poll() leaves it out.
        let mut fds = [
            PollFd::from_borrowed_fd(handle, PollFlags::IN),
            PollFd::from_borrowed_fd(wake.unwrap_or(handle), PollFlags::IN),
        ];
        let watched = if wake.is_some() { 2 } else { 1 };
        match poll(&mut fds[..watched], timeout) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => return Ok(Wakeup::Signal),
            Err(err) => return Err(err),
        }
        if wake.is_none() {
            return Ok(Wakeup::Device);
        }

        let revents = fds[1].revents();
        if revents.contains(PollFlags::IN) {
            return Ok(Wakeup::Wake);
        }
        // poll() reports these whether or not they were asked for, and keeps
        // reporting them: a pipe whose writer is gone, or a descriptor that
        // was closed, would otherwise look like a wake-up on every call.
        if revents.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
            return Err(io::Error::other(
                "the wake descriptor is hung up, in error or not open",
            ));
        }
        Ok(Wakeup::Device)
    }

    /// Services the battery until the kernel has registered its power supply,
    /// and returns where. `None` means it did not happen within `timeout`.
    ///
    /// Worth checking: the kernel accepts a device it then builds no battery
    /// for without a word (`CONFIG_HID_BATTERY_STRENGTH` missing, say), and the
    /// only evidence is the absence of the sysfs entry.
    ///
    /// The battery is serviced the whole time - this is the moment the kernel
    /// sends `UHID_START` and asks for the first report, and a `GET_REPORT`
    /// left unanswered holds the probe up for seconds.
    ///
    /// # Errors
    ///
    /// Fails if servicing the battery fails.
    pub fn wait_for_power_supply(&mut self, timeout: Duration) -> io::Result<Option<PathBuf>> {
        // A timeout too long to be a point in time is no deadline at all.
        let deadline = Instant::now().checked_add(timeout);
        loop {
            self.service()?;
            if let Some(path) = self.power_supply() {
                return Ok(Some(path));
            }
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(None);
            }
            // Serve rather than sleep between two looks at sysfs. A signal
            // cuts the wait short; the loop simply looks again.
            let next = now + REGISTRATION_POLL;
            self.serve_until(deadline.map_or(next, |deadline| deadline.min(next)), None)?;
        }
    }

    /// The power supply the kernel registered for this battery, if it has.
    #[must_use]
    pub fn power_supply(&self) -> Option<PathBuf> {
        sysfs::find_power_supply(&self.uniq)
    }

    /// The level last published, in percent.
    #[must_use]
    pub fn percent(&self) -> u8 {
        self.percent
    }

    /// Whether the battery was last published as charging.
    #[must_use]
    pub fn charging(&self) -> bool {
        self.charging
    }

    /// Removes the device from the kernel and hands the handle back, ready for
    /// a later [`Battery::create`].
    ///
    /// A failure to destroy is not reported: there is nothing to do about it,
    /// and the kernel removes the device anyway once the handle is closed.
    #[must_use = "dropping the handle closes /dev/uhid, which cannot be reopened if it was inherited"]
    pub fn destroy(self) -> Handle {
        let _ = self.handle.write(&event::destroy());
        self.handle.drain();
        self.handle
    }

    fn report(&self) -> [u8; 3] {
        [self.kind.report_id(), self.percent, u8::from(self.charging)]
    }

    fn push(&self) -> io::Result<()> {
        self.handle.write(&event::input2(&self.report())?)
    }

    fn schedule_repush(&mut self, delay: Duration) {
        let due = Instant::now() + delay;
        self.repush_at = Some(self.repush_at.map_or(due, |current| current.min(due)));
    }
}

impl AsFd for Battery {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.handle.as_fd()
    }
}

/// Why a wait in [`Battery::serve_until`] ended, short of an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wakeup {
    /// The device has something to say, or the timeout ran out.
    Device,
    /// The wake descriptor is readable.
    Wake,
    /// A signal interrupted the wait.
    Signal,
}

fn poll(fds: &mut [PollFd<'_>], timeout: Duration) -> io::Result<usize> {
    let timeout = Timespec {
        tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: timeout.subsec_nanos().into(),
    };
    rustix::event::poll(fds, Some(&timeout)).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, phys: &str, uniq: &str) -> Identity {
        Identity {
            name: name.into(),
            phys: phys.into(),
            uniq: uniq.into(),
            vendor: 0,
            product: 0,
        }
    }

    #[test]
    fn an_identity_the_kernel_would_alter_is_refused() {
        assert!(
            identity("Test", "daemon/test", "daemon-test")
                .validate()
                .is_ok()
        );
        assert!(
            identity(&"n".repeat(127), &"p".repeat(63), &"u".repeat(63))
                .validate()
                .is_ok()
        );
        assert!(identity("n", "", "u").validate().is_ok());
        // What the daemons in the field use, and what the live tests use.
        for uniq in [
            "razerd",
            "headset-3329-4b18",
            "uhid-battery-test-generic-4242",
            "a.b_c",
        ] {
            assert!(identity("n", "p", uniq).validate().is_ok(), "{uniq}");
        }

        assert!(identity(&"n".repeat(128), "p", "u").validate().is_err());
        assert!(identity("", "p", "u").validate().is_err());
        assert!(identity("n", &"p".repeat(64), "u").validate().is_err());
        assert!(identity("n", "p", &"u".repeat(64)).validate().is_err());
        assert!(identity("n", "p", "").validate().is_err());
        assert!(identity("n", "p", "a\0b").validate().is_err());
        assert!(identity("n\0", "p", "u").validate().is_err());
        // Anything a file name, a udev match or a shell would trip on.
        for uniq in ["a/b", "a b", "a\nb", "a*", "a?", "a[b]", "a:b", "ä", "a\tb"] {
            assert!(identity("n", "p", uniq).validate().is_err(), "{uniq:?}");
        }
    }
}
