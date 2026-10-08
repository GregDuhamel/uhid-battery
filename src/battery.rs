//! A live virtual battery.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::descriptor::Kind;
use crate::event::{self, Event, RTYPE_INPUT};
use crate::handle::Handle;
use crate::identity::Identity;
use crate::poll::{PollFd, PollFlags, poll_until};
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

/// The descriptor's `Logical Maximum` for the level. The kernel does not clamp
/// what it is handed: a byte past this would reach sysfs as is, and UPower
/// would show 150 %.
const MAX_PERCENT: u8 = 100;

/// What a battery reports: a level and whether it is charging.
///
/// Plain data; a `percent` above 100 is published as 100 (see
/// [`Battery::create`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Reading {
    /// Charge level, 0 to 100.
    pub percent: u8,
    /// Whether the battery is being charged.
    pub charging: bool,
}

impl Reading {
    /// A reading, as given.
    #[must_use]
    pub const fn new(percent: u8, charging: bool) -> Self {
        Self { percent, charging }
    }

    /// What the kernel is told: the level held to the descriptor's maximum.
    fn clamped(self) -> Self {
        Self {
            percent: self.percent.min(MAX_PERCENT),
            charging: self.charging,
        }
    }
}

/// Why [`serve_all`] (or [`Battery::serve_until`]) returned, short of an error.
///
/// In every case the batteries were serviced once more just before returning,
/// so the kernel is never left waiting on an answer while the caller does its
/// own thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Wakeup {
    /// The wake descriptor is readable. It was not read: that is for the
    /// caller, and until it does every call returns this at once.
    Wake,
    /// The deadline passed.
    Deadline,
    /// A signal interrupted the wait before the deadline, so the caller gets
    /// to look at whatever flag its handler raised. Call again to keep
    /// waiting.
    Interrupted,
}

/// What went wrong in [`Battery::create`], as far as retrying is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CreateErrorKind {
    /// The [`Identity`] holds a string the kernel would truncate, or a `uniq`
    /// that cannot name a power supply. Nothing was written to the kernel;
    /// trying again with the same identity will fail the same way.
    InvalidIdentity,
    /// The kernel refused the device, or `/dev/uhid` could not be written to.
    /// Often transient - a device still being torn down on this handle, say -
    /// so worth another try later.
    Io,
}

/// A failed [`Battery::create`]. It carries the handle back, because a handle
/// that came from the service manager cannot be opened again.
#[derive(Debug)]
pub struct CreateError {
    handle: Handle,
    kind: CreateErrorKind,
    source: io::Error,
}

impl CreateError {
    /// Whether the identity is at fault, or the kernel.
    #[must_use]
    pub fn kind(&self) -> CreateErrorKind {
        self.kind
    }

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

/// A failed [`serve_all`], saying which battery failed - so a daemon can give
/// up on that one and keep the others.
#[derive(Debug)]
pub struct ServeError {
    index: Option<usize>,
    source: io::Error,
}

impl ServeError {
    /// Position in the slice of the battery that could not be serviced, or
    /// `None` when the wait itself failed: `poll()` refused, or the wake
    /// descriptor is hung up, in error or not open. Every battery is then as
    /// sound as it was.
    #[must_use]
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    /// The underlying error.
    #[must_use]
    pub fn into_source(self) -> io::Error {
        self.source
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.index {
            Some(index) => write!(f, "battery #{index}: {}", self.source),
            None => write!(f, "waiting on the devices: {}", self.source),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// For callers that do not care which battery failed: the underlying error,
/// with the index gone.
impl From<ServeError> for io::Error {
    fn from(err: ServeError) -> Self {
        err.source
    }
}

/// A virtual HID device exposing one battery.
///
/// The kernel tears the device - and its power supply - down when the handle is
/// closed, so dropping this, or the process dying, is cleanup enough. Use
/// [`Battery::destroy`] to keep the handle for a later device.
///
/// Something has to keep calling [`Battery::service`] (or [`serve_all`],
/// [`Battery::serve_until`]) while the battery exists: reading the level from
/// sysfs can make the kernel ask *us* for it, and it waits up to five seconds
/// for the answer, holding up whoever was reading.
#[derive(Debug)]
pub struct Battery {
    handle: Handle,
    kind: Kind,
    uniq: String,
    reading: Reading,
    repush_at: Option<Instant>,
}

impl Battery {
    /// Creates the virtual device and pushes a first reading.
    ///
    /// A `percent` above 100 is published as 100: the descriptor declares that
    /// as the maximum, and the kernel passes a larger byte on to sysfs as it
    /// is. [`Battery::reading`] returns the clamped value.
    ///
    /// The power supply shows up a moment later; see
    /// [`Battery::wait_for_power_supply`].
    ///
    /// # Errors
    ///
    /// Fails if `identity` holds a string the kernel would truncate or that
    /// cannot name a power supply ([`CreateErrorKind::InvalidIdentity`]), or
    /// if the kernel refuses the device, for instance because one already
    /// exists on this handle ([`CreateErrorKind::Io`]). The handle comes back
    /// in the error.
    pub fn create(
        handle: Handle,
        identity: &Identity,
        kind: Kind,
        reading: Reading,
    ) -> Result<Self, CreateError> {
        if let Err(source) = identity.validate() {
            return Err(CreateError {
                handle,
                kind: CreateErrorKind::InvalidIdentity,
                source,
            });
        }
        let created = event::create2(
            &identity.name,
            &identity.phys,
            &identity.uniq,
            u32::from(identity.vendor),
            u32::from(identity.product),
            kind.descriptor(),
        )
        .and_then(|event| handle.write(&event));
        if let Err(source) = created {
            return Err(CreateError {
                handle,
                kind: CreateErrorKind::Io,
                source,
            });
        }

        let battery = Self {
            handle,
            kind,
            uniq: identity.uniq.clone(),
            reading: reading.clamped(),
            repush_at: None,
        };
        // Most likely dropped - the probe has barely begun - but harmless, and
        // the settle push scheduled on UHID_START makes up for it.
        match battery.push() {
            Ok(()) => Ok(battery),
            Err(source) => Err(CreateError {
                handle: battery.destroy(),
                kind: CreateErrorKind::Io,
                source,
            }),
        }
    }

    /// Publishes a new reading, clamped as in [`Battery::create`]. Pushing an
    /// unchanged one is fine, and a good idea now and then: the kernel
    /// rate-limits the resulting uevents itself.
    ///
    /// # Errors
    ///
    /// Fails if the write to `/dev/uhid` fails.
    pub fn update(&mut self, reading: Reading) -> io::Result<()> {
        let reading = reading.clamped();
        if reading.charging != self.reading.charging && reading.percent == self.reading.percent {
            self.schedule_repush(CHARGE_FLIP_REPUSH_DELAY);
        }
        self.reading = reading;
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
    /// For callers running their own [`poll`](crate::poll::poll) loop over
    /// [`Battery::as_fd`]: wake up no later than this.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.repush_at
    }

    /// Services this battery until `deadline`: [`serve_all`] for one device,
    /// and without an allocation per turn.
    ///
    /// # Errors
    ///
    /// As [`serve_all`], as a plain [`io::Error`]: there is only one battery
    /// to blame.
    pub fn serve_until(
        &mut self,
        deadline: Instant,
        wake: Option<BorrowedFd<'_>>,
    ) -> io::Result<Wakeup> {
        serve(
            std::slice::from_mut(self),
            Some(deadline),
            |batteries, timeout| {
                let device = batteries[0].as_fd();
                // A fixed pair rather than a Vec: this runs on every turn of the
                // loop. Without a wake descriptor the second slot holds the device
                // again, and the slice handed to poll() leaves it out.
                let mut fds = [
                    PollFd::from_borrowed_fd(device, PollFlags::IN),
                    PollFd::from_borrowed_fd(wake.unwrap_or(device), PollFlags::IN),
                ];
                let watched = if wake.is_some() { 2 } else { 1 };
                wait(&mut fds[..watched], wake.is_some(), timeout)
            },
        )
        .map_err(Into::into)
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

    /// The reading last published, after clamping.
    #[must_use]
    pub fn reading(&self) -> Reading {
        self.reading
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

    /// The battery's input report: its ID, the level, the charging bit.
    fn report(&self) -> [u8; 3] {
        [
            self.kind.report_id(),
            self.reading.percent,
            u8::from(self.reading.charging),
        ]
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

/// Services every battery until `deadline`, if there is one.
///
/// Each battery is serviced whenever its device has something to say and
/// whenever one of its own pushes is due, so a daemon with several devices
/// need not run its own `poll()` loop over them. With no battery at all this
/// is a plain sleep - but through `poll()`, so a signal still cuts it short.
///
/// `wake`, if given, is watched too: the call returns [`Wakeup::Wake`] as soon
/// as it is readable (it is never read here). With neither a deadline nor a
/// wake descriptor the call only ever returns on a signal, or on an error.
///
/// # Errors
///
/// Fails if servicing a battery fails - the error says which one, by its
/// position in `batteries`, and the call stops there, before the batteries
/// after it are serviced - or if waiting on the devices fails, or if `wake`
/// is hung up, in error or not an open descriptor: such a descriptor is
/// reported by every `poll()` and never becomes readable, so returning
/// [`Wakeup::Wake`] for it would have the caller spin. The last two carry no
/// index. A daemon that gives up on a failing battery removes it from the
/// slice and calls again; the others are untouched.
pub fn serve_all(
    batteries: &mut [Battery],
    deadline: Option<Instant>,
    wake: Option<BorrowedFd<'_>>,
) -> Result<Wakeup, ServeError> {
    serve(batteries, deadline, |batteries, timeout| {
        let mut fds: Vec<PollFd<'_>> = batteries
            .iter()
            .map(|battery| PollFd::from_borrowed_fd(battery.as_fd(), PollFlags::IN))
            .collect();
        if let Some(wake) = wake {
            fds.push(PollFd::from_borrowed_fd(wake, PollFlags::IN));
        }
        wait(&mut fds, wake.is_some(), timeout)
    })
}

/// The loop behind [`serve_all`] and [`Battery::serve_until`]: service, wait
/// on the devices through `wait_once`, decide. `wait_once` gets the longest it
/// may block, `None` for ever.
fn serve(
    batteries: &mut [Battery],
    deadline: Option<Instant>,
    mut wait_once: impl FnMut(&[Battery], Option<Duration>) -> io::Result<Ready>,
) -> Result<Wakeup, ServeError> {
    loop {
        service_all(batteries)?;

        let now = Instant::now();
        let until = batteries
            .iter()
            .filter_map(Battery::next_deadline)
            .chain(deadline)
            .min();
        let timeout = until.map(|until| until.saturating_duration_since(now));

        let ready = wait_once(batteries, timeout).map_err(|source| ServeError {
            index: None,
            source,
        })?;
        match ready {
            Ready::Device => {}
            Ready::Wake => {
                service_all(batteries)?;
                return Ok(Wakeup::Wake);
            }
            Ready::Interrupted => {
                service_all(batteries)?;
                return Ok(Wakeup::Interrupted);
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            service_all(batteries)?;
            return Ok(Wakeup::Deadline);
        }
    }
}

/// Services every battery in turn, naming the first that fails.
fn service_all(batteries: &mut [Battery]) -> Result<(), ServeError> {
    batteries
        .iter_mut()
        .enumerate()
        .try_for_each(|(index, battery)| {
            battery.service().map_err(|source| ServeError {
                index: Some(index),
                source,
            })
        })
}

/// Why one `poll()` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ready {
    /// A device has something to say, or the timeout ran out.
    Device,
    /// The wake descriptor is readable.
    Wake,
    /// A signal interrupted the wait.
    Interrupted,
}

/// One `poll()` over `fds`, the last of which is the wake descriptor when
/// `with_wake` says so, for at most `timeout`.
fn wait(fds: &mut [PollFd<'_>], with_wake: bool, timeout: Option<Duration>) -> io::Result<Ready> {
    match poll_until(fds, timeout) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::Interrupted => return Ok(Ready::Interrupted),
        Err(err) => return Err(err),
    }
    let Some(wake) = with_wake.then(|| fds.last()).flatten() else {
        return Ok(Ready::Device);
    };

    let revents = wake.revents();
    if revents.contains(PollFlags::IN) {
        return Ok(Ready::Wake);
    }
    // poll() reports these whether or not they were asked for, and keeps
    // reporting them: a pipe whose writer is gone, or a descriptor that was
    // closed, would otherwise look like a wake-up on every call.
    if revents.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
        return Err(io::Error::other(
            "the wake descriptor is hung up, in error or not open",
        ));
    }
    Ok(Ready::Device)
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixDatagram;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use super::*;
    use crate::descriptor::input_report_len;
    use crate::event::fake::{self, Sent};
    use crate::event::{BUS_VIRTUAL, EIO};
    use crate::fake::Kernel;

    /// `UHID_FEATURE_REPORT`: a report type the battery never serves.
    const RTYPE_FEATURE: u8 = 0;

    fn identity() -> Identity {
        Identity::new("Test Device", "test-device")
            .phys("test/0")
            .vendor(0x1234)
            .product(0x5678)
    }

    /// A battery on a fake kernel, with its creation events already read.
    fn on_fake(kind: Kind, reading: Reading) -> (Kernel, Battery) {
        let (kernel, handle) = Kernel::new();
        let battery = Battery::create(handle, &identity(), kind, reading).expect("creating");
        let sent = kernel.sent_all();
        assert_eq!(sent.len(), 2, "{sent:?}");
        (kernel, battery)
    }

    /// Whether `deadline` is about `delay` from now, give or take the time a
    /// test takes to get from one line to the next.
    fn is_about(deadline: Option<Instant>, delay: Duration) -> bool {
        let Some(deadline) = deadline else {
            return false;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        remaining <= delay && remaining + Duration::from_millis(100) >= delay
    }

    /// A pipe whose writer is gone, as a hung-up wake descriptor: `poll()`
    /// reports `POLLHUP` on it, and never `POLLIN`.
    ///
    /// Made in-process on purpose. Spawning a child for it would copy this
    /// process's descriptor table between `fork()` and `exec()`, and a fake
    /// kernel another test had just dropped would live on in that copy long
    /// enough for the write it expects to fail to succeed.
    fn hung_up_pipe() -> OwnedFd {
        let (reader, writer) = rustix::pipe::pipe().expect("a pipe");
        drop(writer);
        reader
    }

    #[test]
    fn create_writes_a_create2_the_kernel_accepts_then_a_first_push() {
        for kind in [Kind::Mouse, Kind::Generic, Kind::Headset] {
            let (kernel, handle) = Kernel::new();
            let battery = Battery::create(handle, &identity(), kind, Reading::new(87, true))
                .expect("creating");
            assert_eq!(
                kernel.expect_sent(),
                Sent::Create2 {
                    name: "Test Device".into(),
                    phys: "test/0".into(),
                    uniq: "test-device".into(),
                    bus: BUS_VIRTUAL,
                    vendor: 0x1234,
                    product: 0x5678,
                    descriptor: kind.descriptor().to_vec(),
                },
                "{kind:?}"
            );
            assert_eq!(
                kernel.expect_sent(),
                Sent::Input2(vec![kind.report_id(), 87, 1]),
                "{kind:?}"
            );
            assert_eq!(kernel.sent(), None);
            assert_eq!(battery.reading(), Reading::new(87, true));
            assert_eq!(battery.next_deadline(), None);
        }
    }

    #[test]
    fn the_level_is_clamped_to_the_descriptor_maximum() {
        let (kernel, mut battery) = on_fake(Kind::Generic, Reading::new(100, false));
        battery.update(Reading::new(150, false)).unwrap();
        assert_eq!(kernel.expect_sent(), Sent::Input2(vec![1, 100, 0]));
        assert_eq!(battery.reading(), Reading::new(100, false));

        let (kernel, handle) = Kernel::new();
        let battery =
            Battery::create(handle, &identity(), Kind::Generic, Reading::new(255, true)).unwrap();
        assert_eq!(battery.reading().percent, 100);
        assert_eq!(kernel.sent_all()[1], Sent::Input2(vec![1, 100, 1]));
    }

    #[test]
    fn an_invalid_identity_is_refused_before_anything_is_written() {
        let (kernel, handle) = Kernel::new();
        let bad = Identity::new("Test", "not valid");
        let err = Battery::create(handle, &bad, Kind::Generic, Reading::default()).unwrap_err();
        assert_eq!(err.kind(), CreateErrorKind::InvalidIdentity);
        assert!(err.to_string().contains("could not create"), "{err}");
        assert_eq!(kernel.sent(), None);

        // The handle is intact and good for a proper identity.
        let (handle, source) = err.into_parts();
        assert_eq!(source.kind(), io::ErrorKind::InvalidInput);
        let battery =
            Battery::create(handle, &identity(), Kind::Generic, Reading::default()).unwrap();
        assert_eq!(kernel.sent_all().len(), 2);
        drop(battery);
    }

    #[test]
    fn a_kernel_that_refuses_the_device_is_an_io_error_and_the_handle_comes_back() {
        let (kernel, handle) = Kernel::new();
        kernel.vanish();
        let err =
            Battery::create(handle, &identity(), Kind::Generic, Reading::default()).unwrap_err();
        assert_eq!(err.kind(), CreateErrorKind::Io);
        let (handle, source) = err.into_parts();
        assert_ne!(source.kind(), io::ErrorKind::InvalidInput);
        drop(handle);
    }

    #[test]
    fn start_schedules_a_push_once_the_probe_has_settled() {
        let (kernel, mut battery) = on_fake(Kind::Mouse, Reading::new(42, false));
        kernel.send(&fake::start());
        battery.service().unwrap();
        // Scheduled, not pushed: the kernel would drop it right now.
        assert_eq!(kernel.sent(), None);
        assert!(is_about(battery.next_deadline(), START_SETTLE_DELAY));

        // Serving past the delay pushes it, on its own.
        let outcome = battery
            .serve_until(Instant::now() + START_SETTLE_DELAY * 2, None)
            .unwrap();
        assert_eq!(outcome, Wakeup::Deadline);
        assert_eq!(kernel.sent_all(), [Sent::Input2(vec![2, 42, 0])]);
        assert_eq!(battery.next_deadline(), None);
    }

    #[test]
    fn get_report_is_answered_for_the_battery_report_and_refused_for_others() {
        let (kernel, mut battery) = on_fake(Kind::Mouse, Reading::new(73, true));
        let id = Kind::Mouse.report_id();
        kernel.send(&fake::get_report(11, id, RTYPE_INPUT));
        kernel.send(&fake::get_report(12, id + 1, RTYPE_INPUT));
        kernel.send(&fake::get_report(13, id, RTYPE_FEATURE));
        battery.service().unwrap();
        assert_eq!(
            kernel.sent_all(),
            [
                Sent::GetReportReply {
                    id: 11,
                    err: 0,
                    data: vec![id, 73, 1],
                },
                Sent::GetReportReply {
                    id: 12,
                    err: EIO,
                    data: Vec::new(),
                },
                Sent::GetReportReply {
                    id: 13,
                    err: EIO,
                    data: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn set_report_is_refused() {
        let (kernel, mut battery) = on_fake(Kind::Generic, Reading::default());
        kernel.send(&fake::set_report(21, 1, RTYPE_INPUT));
        battery.service().unwrap();
        assert_eq!(
            kernel.sent_all(),
            [Sent::SetReportReply { id: 21, err: EIO }]
        );
    }

    #[test]
    fn update_pushes_the_new_reading() {
        let (kernel, mut battery) = on_fake(Kind::Generic, Reading::new(50, false));
        battery.update(Reading::new(49, false)).unwrap();
        assert_eq!(kernel.sent_all(), [Sent::Input2(vec![1, 49, 0])]);
        assert_eq!(battery.reading(), Reading::new(49, false));
        assert_eq!(battery.next_deadline(), None);

        // An unchanged reading is pushed all the same, and schedules nothing.
        battery.update(Reading::new(49, false)).unwrap();
        assert_eq!(kernel.sent_all(), [Sent::Input2(vec![1, 49, 0])]);
        assert_eq!(battery.next_deadline(), None);
    }

    #[test]
    fn a_charging_flip_at_the_same_level_schedules_a_repush() {
        let (kernel, mut battery) = on_fake(Kind::Generic, Reading::new(50, false));
        battery.update(Reading::new(50, true)).unwrap();
        assert_eq!(kernel.sent_all(), [Sent::Input2(vec![1, 50, 1])]);
        assert!(is_about(battery.next_deadline(), CHARGE_FLIP_REPUSH_DELAY));

        // The earliest push wins: a START brings the deadline forward.
        kernel.send(&fake::start());
        battery.service().unwrap();
        assert!(is_about(battery.next_deadline(), START_SETTLE_DELAY));

        // A flip with a level change needs no repush: the level is news.
        let (_kernel, mut battery) = on_fake(Kind::Generic, Reading::new(50, false));
        battery.update(Reading::new(51, true)).unwrap();
        assert_eq!(battery.next_deadline(), None);
    }

    #[test]
    fn serve_until_returns_at_the_deadline_after_a_last_service() {
        let (kernel, mut battery) = on_fake(Kind::Generic, Reading::new(50, false));
        let started = Instant::now();
        kernel.send(&fake::get_report(1, 1, RTYPE_INPUT));
        let outcome = battery
            .serve_until(started + Duration::from_millis(50), None)
            .unwrap();
        assert_eq!(outcome, Wakeup::Deadline);
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_eq!(kernel.sent_all().len(), 1);
    }

    #[test]
    fn serve_until_returns_when_the_wake_descriptor_is_readable() {
        let (kernel, mut battery) = on_fake(Kind::Generic, Reading::new(50, false));
        let (wake, waker) = UnixDatagram::pair().unwrap();
        waker.send(b"go").unwrap();
        // A request pending at the same time is answered before returning.
        kernel.send(&fake::get_report(1, 1, RTYPE_INPUT));
        let outcome = battery
            .serve_until(Instant::now() + Duration::from_secs(5), Some(wake.as_fd()))
            .unwrap();
        assert_eq!(outcome, Wakeup::Wake);
        assert_eq!(kernel.sent_all().len(), 1);

        // Not read here: it is the caller's, and keeps waking until read.
        let outcome = battery
            .serve_until(Instant::now() + Duration::from_secs(5), Some(wake.as_fd()))
            .unwrap();
        assert_eq!(outcome, Wakeup::Wake);
    }

    #[test]
    fn a_hung_up_wake_descriptor_is_an_error_not_a_wakeup() {
        let (_kernel, mut battery) = on_fake(Kind::Generic, Reading::new(50, false));
        let pipe = hung_up_pipe();
        let err = battery
            .serve_until(Instant::now() + Duration::from_secs(5), Some(pipe.as_fd()))
            .unwrap_err();
        assert!(err.to_string().contains("hung up"), "{err}");
    }

    /// A thread-directed signal is the only way to get `EINTR` out of a
    /// `poll()`, and installing a handler is the only way to make a signal
    /// interrupt anything without killing the process.
    #[allow(unsafe_code)] // The C signal API; see above.
    mod signals {
        extern "C" fn noop(_: libc::c_int) {}

        /// Installs a handler for `SIGUSR1` that does nothing. Process-wide,
        /// and harmless to the other tests: nothing else raises it.
        pub(super) fn install() {
            let handler = noop as extern "C" fn(libc::c_int) as libc::sighandler_t;
            // SAFETY: `noop` is a valid handler for the lifetime of the
            // process, and `signal(2)` has no other precondition.
            unsafe {
                libc::signal(libc::SIGUSR1, handler);
            }
        }

        pub(super) fn current_thread() -> libc::pthread_t {
            // SAFETY: no preconditions.
            unsafe { libc::pthread_self() }
        }

        /// Sends `SIGUSR1` to `thread`, which is still running: the sender
        /// stops before the receiver's test function returns.
        pub(super) fn interrupt(thread: libc::pthread_t) {
            // SAFETY: the caller keeps `thread` alive while this is called.
            unsafe {
                libc::pthread_kill(thread, libc::SIGUSR1);
            }
        }
    }

    #[test]
    fn serve_until_returns_when_a_signal_interrupts_the_wait() {
        signals::install();
        let (_kernel, mut battery) = on_fake(Kind::Generic, Reading::new(50, false));

        // Keep knocking until the wait has been interrupted: a single signal
        // could land before poll() is entered, and then nothing would happen.
        let served = signals::current_thread();
        let stop = Arc::new(AtomicBool::new(false));
        let knocker = {
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    signals::interrupt(served);
                    thread::sleep(Duration::from_millis(10));
                }
            })
        };

        let started = Instant::now();
        let outcome = battery
            .serve_until(started + Duration::from_secs(5), None)
            .unwrap();
        stop.store(true, Ordering::Relaxed);
        knocker.join().unwrap();
        assert_eq!(outcome, Wakeup::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn serve_all_services_every_battery_and_sleeps_with_none() {
        let (kernel_a, mut a) = on_fake(Kind::Mouse, Reading::new(10, false));
        let (kernel_b, mut b) = on_fake(Kind::Generic, Reading::new(20, true));
        kernel_a.send(&fake::get_report(1, 2, RTYPE_INPUT));
        kernel_b.send(&fake::get_report(2, 1, RTYPE_INPUT));

        let mut batteries = [a, b];
        let outcome = serve_all(
            &mut batteries,
            Some(Instant::now() + Duration::from_millis(50)),
            None,
        )
        .unwrap();
        assert_eq!(outcome, Wakeup::Deadline);
        assert_eq!(
            kernel_a.sent_all(),
            [Sent::GetReportReply {
                id: 1,
                err: 0,
                data: vec![2, 10, 0],
            }]
        );
        assert_eq!(
            kernel_b.sent_all(),
            [Sent::GetReportReply {
                id: 2,
                err: 0,
                data: vec![1, 20, 1],
            }]
        );

        // A due push on one battery wakes the wait for that battery alone.
        [a, b] = batteries;
        kernel_b.send(&fake::start());
        let outcome = serve_all(
            &mut [a, b],
            Some(Instant::now() + START_SETTLE_DELAY * 2),
            None,
        )
        .unwrap();
        assert_eq!(outcome, Wakeup::Deadline);
        assert_eq!(kernel_a.sent_all(), []);
        assert_eq!(kernel_b.sent_all(), [Sent::Input2(vec![1, 20, 1])]);

        // No battery: a sleep, and a wake descriptor still counts.
        let started = Instant::now();
        let outcome = serve_all(&mut [], Some(started + Duration::from_millis(20)), None).unwrap();
        assert_eq!(outcome, Wakeup::Deadline);
        assert!(started.elapsed() >= Duration::from_millis(20));
        let (wake, waker) = UnixDatagram::pair().unwrap();
        waker.send(b"go").unwrap();
        let outcome = serve_all(&mut [], None, Some(wake.as_fd())).unwrap();
        assert_eq!(outcome, Wakeup::Wake);
    }

    #[test]
    fn serve_all_names_the_battery_that_failed_and_spares_the_others() {
        let (kernel_a, a) = on_fake(Kind::Generic, Reading::new(10, false));
        let (kernel_b, b) = on_fake(Kind::Generic, Reading::new(20, false));
        // A request the second battery will try to answer after its kernel
        // has gone: the write fails, and it is the one named.
        kernel_b.send(&fake::get_report(1, 1, RTYPE_INPUT));
        kernel_b.vanish();

        let mut batteries = [a, b];
        let err = serve_all(
            &mut batteries,
            Some(Instant::now() + Duration::from_secs(5)),
            None,
        )
        .unwrap_err();
        assert_eq!(err.index(), Some(1));
        assert!(err.to_string().starts_with("battery #1: "), "{err}");
        assert_eq!(kernel_a.sent_all(), []);

        // The first is untouched and keeps working on its own.
        let [mut a, b] = batteries;
        drop(b);
        a.update(Reading::new(11, false)).unwrap();
        assert_eq!(kernel_a.sent_all(), [Sent::Input2(vec![1, 11, 0])]);
        let outcome = serve_all(
            std::slice::from_mut(&mut a),
            Some(Instant::now() + Duration::from_millis(20)),
            None,
        )
        .unwrap();
        assert_eq!(outcome, Wakeup::Deadline);

        // A broken wake descriptor is nobody's fault among the batteries.
        let pipe = hung_up_pipe();
        let err = serve_all(std::slice::from_mut(&mut a), None, Some(pipe.as_fd())).unwrap_err();
        assert_eq!(err.index(), None);
        assert!(
            err.to_string().starts_with("waiting on the devices: "),
            "{err}"
        );
        let plain: io::Error = err.into();
        assert!(plain.to_string().contains("hung up"), "{plain}");
    }

    #[test]
    fn destroy_writes_destroy_drains_the_queue_and_hands_the_handle_back() {
        let (kernel, battery) = on_fake(Kind::Generic, Reading::new(50, false));
        // Queued by the kernel for the old device: not news for the next one.
        kernel.send(&fake::start());
        let handle = battery.destroy();
        assert_eq!(kernel.sent_all(), [Sent::Destroy]);

        let mut again =
            Battery::create(handle, &identity(), Kind::Generic, Reading::new(60, true)).unwrap();
        assert_eq!(kernel.sent_all().len(), 2);
        again.service().unwrap();
        assert_eq!(again.next_deadline(), None, "the old START was drained");
    }

    /// The report is `[id, level, charging]`; the descriptor had better say
    /// the same, or the kernel reads the level from the wrong byte.
    #[test]
    fn the_report_is_as_long_as_the_descriptor_says() {
        for kind in [Kind::Mouse, Kind::Generic, Kind::Headset] {
            let (_kernel, battery) = on_fake(kind, Reading::default());
            assert_eq!(
                input_report_len(kind.descriptor(), kind.report_id()),
                battery.report().len(),
                "{kind:?}"
            );
        }
    }
}
