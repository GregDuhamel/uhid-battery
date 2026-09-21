//! A live virtual battery.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::thread::sleep;
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
    /// so it has to be unique on the machine and filesystem-safe: not empty,
    /// and without a `/`. Never put a string that came from the device in here.
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
    fn validate(&self) -> io::Result<()> {
        let fits = |value: &str, field: usize| value.len() < field && !value.contains('\0');
        if !fits(&self.name, event::LEN_NAME) {
            return Err(invalid("the name must be under 128 bytes, without NUL"));
        }
        if !fits(&self.phys, event::LEN_PHYS) {
            return Err(invalid("phys must be under 64 bytes, without NUL"));
        }
        if self.uniq.is_empty() || self.uniq.contains('/') || !fits(&self.uniq, event::LEN_UNIQ) {
            return Err(invalid("uniq must be 1 to 63 bytes, without NUL or '/'"));
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
    /// whatever flag its handler raised.
    ///
    /// # Errors
    ///
    /// Fails if servicing the battery or waiting on it fails.
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

            let mut fds = Vec::with_capacity(2);
            fds.push(PollFd::new(&self.handle, PollFlags::IN));
            if let Some(wake) = wake.as_ref() {
                fds.push(PollFd::new(wake, PollFlags::IN));
            }
            match poll(&mut fds, timeout) {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => return Ok(false),
                Err(err) => return Err(err),
            }
            if fds.get(1).is_some_and(|fd| !fd.revents().is_empty()) {
                return Ok(true);
            }
            drop(fds);

            if Instant::now() >= deadline {
                self.service()?;
                return Ok(false);
            }
        }
    }

    /// Services the battery until the kernel has registered its power supply,
    /// and returns where. `None` means it did not happen within `timeout`.
    ///
    /// Worth checking: the kernel accepts a device it then builds no battery
    /// for without a word (`CONFIG_HID_BATTERY_STRENGTH` missing, say), and the
    /// only evidence is the absence of the sysfs entry.
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
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            sleep(REGISTRATION_POLL);
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
        assert!(identity("", "", "u").validate().is_ok());

        assert!(identity(&"n".repeat(128), "p", "u").validate().is_err());
        assert!(identity("n", &"p".repeat(64), "u").validate().is_err());
        assert!(identity("n", "p", &"u".repeat(64)).validate().is_err());
        assert!(identity("n", "p", "").validate().is_err());
        assert!(identity("n", "p", "a/b").validate().is_err());
        assert!(identity("n", "p", "a\0b").validate().is_err());
        assert!(identity("n\0", "p", "u").validate().is_err());
    }
}
