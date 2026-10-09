# uhid-battery

[![CI](https://github.com/GregDuhamel/uhid-battery/actions/workflows/ci.yml/badge.svg)](https://github.com/GregDuhamel/uhid-battery/actions/workflows/ci.yml)
[![Live](https://github.com/GregDuhamel/uhid-battery/actions/workflows/live.yml/badge.svg)](https://github.com/GregDuhamel/uhid-battery/actions/workflows/live.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Publish a battery level to **UPower** — and so to KDE's, GNOME's or any other
desktop's power applet — for a peripheral the kernel has no driver for.

UPower does not accept battery devices over D-Bus; it reports what the kernel
exposes under `/sys/class/power_supply`. So a daemon that knows the charge of a
wireless mouse or headset creates a virtual HID device through `/dev/uhid` whose
report descriptor declares a battery. `hid-input` registers the power supply,
UPower picks it up, and the desktop lists the device next to the others.

This crate is that mechanism, extracted from two daemons that each learned half
of its pitfalls the hard way:
[razerd](https://github.com/GregDuhamel/razerd) (a Razer mouse) and
[HeadsetBatteryIndicator](https://github.com/GregDuhamel/HeadsetBatteryIndicator)
(wireless headsets).

## Use

It is not on crates.io; depend on it through git, pinned to a release tag:

```toml
[dependencies]
uhid-battery = { git = "https://github.com/GregDuhamel/uhid-battery", tag = "v0.4.1" }
```

Releases are made in two steps. The pull request bumps the version in
`Cargo.toml` and `Cargo.lock` (`cargo update --workspace`) and to the
`tag = "vX.Y.Z"` snippet above, with its
CHANGELOG entry. Once it is on `main`, the *Release* workflow (Actions →
Release → Run workflow) checks the three agree, refuses a version that is
already tagged, runs the lints and tests, tags `main` and publishes the
GitHub release. It never commits: `main` only takes signed commits through
pull requests.

```rust
use std::time::{Duration, Instant};
use uhid_battery::{Battery, Handle, Identity, Kind, Reading, Wakeup};

// Passed down by systemd (`OpenFile=/dev/uhid:uhid`), or opened as root.
// SAFETY: first thing in main, before any thread could read the environment
// that `inherited` edits.
let handle = match unsafe { Handle::inherited("uhid") }.pop() {
    Some(handle) => handle,
    None => Handle::open(uhid_battery::DEV_UHID)?,
};

// The name is the label the desktop shows; the unique ID names the power
// supply (-> hid-my-daemon-maxwell-battery-1).
let identity = Identity::new("Audeze Maxwell", "my-daemon-maxwell")
    .phys("my-daemon/maxwell")
    .vendor(0x3329)
    .product(0x4b18);
let mut battery = Battery::create(handle, &identity, Kind::Headset, Reading::new(87, false))?;
battery.wait_for_power_supply(Duration::from_secs(2))?;

loop {
    match battery.serve_until(Instant::now() + Duration::from_secs(60), None)? {
        Wakeup::Deadline => battery.update(read_the_level_somehow())?,
        Wakeup::Interrupted | Wakeup::Wake => break, // look at the flag the handler raised
    }
}
```

A daemon with several devices hands them all to `uhid_battery::serve_all`,
which waits on every one of them (and on an optional wake descriptor) until a
deadline. When one of them fails the `ServeError` says which (`index()`), so
the daemon can drop that one and carry on with the rest; `From<ServeError>
for io::Error` is there for a daemon that does not care. Underneath are
`Battery::service()`, `Battery::next_deadline()`, `AsFd` and
`uhid_battery::poll::poll`, for a daemon that runs its own loop.

`Battery::create` tells a failure apart from a mistake:
`CreateError::kind()` is `InvalidIdentity` when the identity can never be
accepted, and `Io` when the kernel said no this time; both give the `Handle`
back through `into_parts()`.

## What it knows so you do not have to

Every item below cost an afternoon. None of them produces an error message.

* **The descriptor's top-level collection must be an input application**
  (`IS_INPUT_APPLICATION`: generic desktop, digitizer, consumer control). With
  anything else `hidinput_connect()` returns before looking at a single field:
  the device gets a hidraw node and no battery.
* **A device with no populated input node is torn down**, battery included
  ("No inputs registered, leaving"). `Kind::Mouse` declares a pointer it never
  moves; the other kinds declare one vendor-defined bit, mapped to `BTN_MISC`,
  that is never set and that udev does not classify as anything.
* **The kernel drops input reports while it probes the device** — and the write
  still succeeds. `UHID_START` arrives part-way through the probe, so the level
  is pushed again 250 ms after it.
* **Reading the level from sysfs can make the kernel ask you for it**, and it
  waits up to five seconds for the answer. Something has to keep servicing the
  device, and it must not be the thread that reads sysfs.
* **A charging flip at an unchanged level is not always announced.** Kernels
  rate-limit battery uevents to one per 30 s for an unchanged level, so the
  report is pushed again once that window has closed.
* **The power supply's name changed.** It was `hid-<uniq>-battery`; recent
  kernels append the report ID (`hid-<uniq>-battery-1`). `find_power_supply`
  matches both.
* **UPower types the battery after its sibling input node.** A mouse needs
  nothing more. There is no input class for headsets: UPower only reports one
  when a sibling carries the properties systemd puts on a sound card, and it
  accepts an `input` node as that sibling — so `Kind::Headset.udev_rule(..)`
  returns the rule that tags ours. Without it the desktop draws a laptop
  battery.
* **A handle passed by systemd cannot be reopened.** Every fallible operation
  that consumes a `Handle` gives it back: `CreateError::into_parts()`,
  `Battery::destroy()`.
* **The kernel truncates the identity strings without a word.** A `uniq` cut
  short names a power supply nobody looks for, so `Battery::create` refuses an
  `Identity` that does not fit, an empty `name`, and a `uniq` outside
  `[A-Za-z0-9._-]` - it becomes a directory name that udev rules and shell
  snippets match on.
* **An inherited descriptor could be anything.** `Handle::from_fd` checks that
  it really is the uhid character device (10:239) before events are written
  into it.

## Running unprivileged

`/dev/uhid` lets its holder create arbitrary input devices — a keyboard, say —
so do not widen its permissions. Let systemd open it and pass the descriptor
(`OpenFile=` needs systemd 253 or later):

```ini
[Service]
OpenFile=/dev/uhid:uhid
DynamicUser=yes
DevicePolicy=closed
# Needed for OpenFile= itself: systemd opens the node from inside this unit's
# device cgroup. It does not let the process open it.
DeviceAllow=/dev/uhid rw
```

`Handle::inherited("uhid")` then returns the handle, and the node stays
`root:root 0600`. It closes whatever else the service manager passed; a daemon
that is also socket-activated calls `Handle::inherited_with_others("uhid")` and
gets those descriptors back with their names. Both are filters over
`uhid_battery::listen_fds::take()`, the `sd_listen_fds(3)` protocol on its
own, for a daemon with other ideas about what it was passed.

All three are `unsafe fn`: they remove the `LISTEN_*` variables from the
environment, which is a data race against any thread that reads it (Rust's
`std::env`, or `getenv(3)` in a C library). Call them at the top of `main`,
before spawning anything.

## Testing

```sh
cargo test          # no hardware, no privileges
```

The unit tests stand a datagram socket pair in for `/dev/uhid` and drive the
whole state machine through it: what `create` writes, the push after
`UHID_START`, `GET_REPORT` answered or refused, the re-push after a charging
flip, every way `serve_until` and `serve_all` return, `destroy`. They run under
`cargo test` with the doctests, on both stable and the MSRV, in the *CI*
workflow, next to rustfmt, clippy, rustdoc with warnings denied, and
`cargo audit`. That workflow is the shared one of
[`rust-ci.yml`](.github/workflows/rust-ci.yml), which this author's other
Rust repositories call as
`GregDuhamel/uhid-battery/.github/workflows/rust-ci.yml@main`.

A daemon built on this crate can test its own code the same way: the `fake`
feature makes that stand-in public as `uhid_battery::fake`, with
`Handle::from_fd_unchecked` to adopt the daemon's end of the pair. Enable it
from the dev-dependencies alone - it is for tests, and nothing behind it is
covered by the API's stability promise:

```toml
[dev-dependencies]
uhid-battery = { git = "https://github.com/GregDuhamel/uhid-battery", tag = "v0.4.1", features = ["fake"] }
```

The acceptance tests talk to the real kernel — they create a battery of each
kind and read it back from sysfs — and need root:

```sh
cargo test --test live --no-run
sudo target/debug/deps/live-* --ignored --nocapture --test-threads=1
```

The *Live* workflow builds them on every pull request and runs them under
`sudo` where the runner's kernel can expose a HID battery: `/dev/uhid` present
and `CONFIG_HID_BATTERY_STRENGTH=y`. The Azure kernel of GitHub's hosted
runners lacks that option — the kernel accepts the device and never registers
a power supply — so there the job only builds the binary and warns; the tests
proper run on a self-hosted runner, or on a developer's machine as above.

## Who uses it

* [razerd](https://github.com/GregDuhamel/razerd) (`--upower`): one
  `Kind::Mouse` battery for a Razer mouse, served with `serve_until` and the
  dock's hidraw descriptor as the wake descriptor.
* [HeadsetBatteryIndicator](https://github.com/GregDuhamel/HeadsetBatteryIndicator):
  one `Kind::Headset` battery per wireless headset, all of them served through
  `serve_all`, with the udev rule from `Kind::Headset.udev_rule(..)`.

This crate is the publishing half of such a daemon. The other half — reading
the real device over hidraw: finding its node through sysfs, feature and
input reports, `poll()` with a timeout, telling an unplugged device from a
transient error — is the sibling crate
[hidraw](https://github.com/GregDuhamel/hidraw).

## License

MIT — see [LICENSE](LICENSE).
