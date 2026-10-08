//! Publish a battery level to UPower - and so to the desktop - through a
//! virtual HID device.
//!
//! UPower does not accept battery devices over D-Bus; it reports what the
//! kernel exposes under `/sys/class/power_supply`. So a daemon that knows the
//! charge of some wireless peripheral, but has no kernel driver to say so,
//! creates a virtual HID device through `/dev/uhid` whose report descriptor
//! declares a battery. `hid-input` registers the power supply, UPower picks it
//! up like any other peripheral battery, and the desktop's power applet lists
//! the device.
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//! use uhid_battery::{Battery, Handle, Identity, Kind, Reading, Wakeup};
//!
//! # fn read_the_level_somehow() -> Reading { Reading::new(86, false) }
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Passed down by systemd (`OpenFile=/dev/uhid:uhid`), or opened as root.
//! // SAFETY: first thing in main, before any thread could read the
//! // environment that `inherited` edits.
//! let handle = match unsafe { Handle::inherited("uhid") }.pop() {
//!     Some(handle) => handle,
//!     None => Handle::open(uhid_battery::DEV_UHID)?,
//! };
//!
//! let identity = Identity::new("Audeze Maxwell", "my-daemon-maxwell")
//!     .phys("my-daemon/maxwell")
//!     .vendor(0x3329)
//!     .product(0x4b18);
//! let mut battery = Battery::create(handle, &identity, Kind::Headset, Reading::new(87, false))?;
//! battery.wait_for_power_supply(Duration::from_secs(2))?;
//!
//! loop {
//!     match battery.serve_until(Instant::now() + Duration::from_secs(60), None)? {
//!         Wakeup::Deadline => battery.update(read_the_level_somehow())?,
//!         Wakeup::Interrupted | Wakeup::Wake => break,
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! The crate exists because the kernel and UPower each have rules that are only
//! discovered by breaking them; see [`Kind`] for the descriptor ones and
//! [`Battery`] for the timing ones.

mod battery;
mod descriptor;
mod event;
#[cfg(test)]
mod fake;
mod handle;
mod identity;
pub mod listen_fds;
pub mod poll;
mod sysfs;

pub use battery::{Battery, CreateError, CreateErrorKind, Reading, ServeError, Wakeup, serve_all};
pub use descriptor::Kind;
pub use handle::{DEV_UHID, Handle};
pub use identity::Identity;
pub use sysfs::find_power_supply;
