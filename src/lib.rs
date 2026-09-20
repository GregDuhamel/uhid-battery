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
//! use uhid_battery::{Battery, Handle, Identity, Kind};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Passed down by systemd (`OpenFile=/dev/uhid:uhid`), or opened as root.
//! let handle = match Handle::inherited("uhid").pop() {
//!     Some(handle) => handle,
//!     None => Handle::open(uhid_battery::DEV_UHID)?,
//! };
//!
//! let identity = Identity {
//!     name: "Audeze Maxwell".into(),
//!     phys: "my-daemon/maxwell".into(),
//!     uniq: "my-daemon-maxwell".into(),
//!     vendor: 0x3329,
//!     product: 0x4b18,
//! };
//! let mut battery = Battery::create(handle, &identity, Kind::Headset, 87, false)?;
//! battery.wait_for_power_supply(Duration::from_secs(2))?;
//!
//! loop {
//!     battery.serve_until(Instant::now() + Duration::from_secs(60), None)?;
//!     battery.update(86, false)?;
//! }
//! # }
//! ```
//!
//! The crate exists because the kernel and UPower each have rules that are only
//! discovered by breaking them; see [`Kind`] for the descriptor ones and
//! [`Battery`] for the timing ones.

mod battery;
mod descriptor;
mod event;
mod handle;
mod sysfs;

pub use battery::{Battery, CreateError, Identity};
pub use descriptor::Kind;
pub use handle::{DEV_UHID, Handle};
pub use sysfs::find_power_supply;
