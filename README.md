# uhid-battery

[![CI](https://github.com/GregDuhamel/uhid-battery/actions/workflows/ci.yml/badge.svg)](https://github.com/GregDuhamel/uhid-battery/actions/workflows/ci.yml)
[![Lint](https://github.com/GregDuhamel/uhid-battery/actions/workflows/lint.yml/badge.svg)](https://github.com/GregDuhamel/uhid-battery/actions/workflows/lint.yml)
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
uhid-battery = { git = "https://github.com/GregDuhamel/uhid-battery", tag = "v0.2.0" }
```

Releases are cut from the *Release* workflow (Actions → Release → Run workflow,
pick the semver bump): it runs the lints and tests, writes the version to
`Cargo.toml`, tags, and publishes the GitHub release.

```rust
use std::time::{Duration, Instant};
use uhid_battery::{Battery, Handle, Identity, Kind};

// Passed down by systemd (`OpenFile=/dev/uhid:uhid`), or opened as root.
let handle = match Handle::inherited("uhid").pop() {
    Some(handle) => handle,
    None => Handle::open(uhid_battery::DEV_UHID)?,
};

let identity = Identity {
    name: "Audeze Maxwell".into(),       // the label the desktop shows
    phys: "my-daemon/maxwell".into(),
    uniq: "my-daemon-maxwell".into(),    // -> hid-my-daemon-maxwell-battery-1
    vendor: 0x3329,
    product: 0x4b18,
};
let mut battery = Battery::create(handle, &identity, Kind::Headset, 87, false)?;
battery.wait_for_power_supply(Duration::from_secs(2))?;

loop {
    battery.serve_until(Instant::now() + Duration::from_secs(60), None)?;
    battery.update(read_the_level_somehow(), false)?;
}
```

`Battery` also implements `AsFd` and exposes `service()` and `next_deadline()`,
for daemons that run their own `poll()` loop over several devices.

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
  `Identity` that does not fit, and a `uniq` that is empty or holds a `/`.
* **An inherited descriptor could be anything.** `Handle::from_fd` checks that
  it really is the uhid character device (10:239) before events are written
  into it.

## Running unprivileged

`/dev/uhid` lets its holder create arbitrary input devices — a keyboard, say —
so do not widen its permissions. Let systemd open it and pass the descriptor:

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
gets those descriptors back with their names.

## Testing

```sh
cargo test          # no hardware, no privileges
```

The acceptance tests talk to the real kernel — they create a battery of each
kind and read it back from sysfs — and need root:

```sh
cargo test --test live --no-run
sudo target/debug/deps/live-* --ignored --nocapture --test-threads=1
```

## License

MIT — see [LICENSE](LICENSE).
