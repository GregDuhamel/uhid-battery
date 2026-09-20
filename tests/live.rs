//! Acceptance test against the real kernel: create a virtual battery of each
//! kind, then read it back from sysfs the way UPower does.
//!
//! It needs write access to `/dev/uhid`, which the node grants to root only, so
//! it is ignored by default:
//!
//! ```sh
//! cargo test --test live --no-run
//! sudo target/debug/deps/live-* --ignored --nocapture --test-threads=1
//! ```

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use uhid_battery::{Battery, DEV_UHID, Handle, Identity, Kind, find_power_supply};

/// Polls until `check` returns a value, or gives up after `timeout`.
fn wait_for<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50));
    }
}

fn attr(base: &Path, name: &str) -> Option<String> {
    fs::read_to_string(base.join(name))
        .ok()
        .map(|value| value.trim().to_owned())
}

fn expect_attr(base: &Path, name: &str, expected: &str) {
    let seen = wait_for(Duration::from_secs(10), || {
        attr(base, name).filter(|value| value == expected)
    });
    assert_eq!(
        seen.as_deref(),
        Some(expected),
        "{name} should read {expected:?}, last saw {:?}",
        attr(base, name)
    );
}

/// What UPower makes of the device. Informational: it may not be running.
fn upower_kind(uniq: &str) -> Option<String> {
    let wanted = uniq.replace('-', "_");
    let path = wait_for(Duration::from_secs(10), || {
        let output = Command::new("upower").arg("-e").output().ok()?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|line| line.contains(&wanted))
            .map(str::to_owned)
    })?;
    let details = Command::new("upower").args(["-i", &path]).output().ok()?;
    // The kind is the only line indented by exactly two spaces with no colon.
    String::from_utf8_lossy(&details.stdout)
        .lines()
        .find(|line| line.starts_with("  ") && !line.starts_with("   ") && !line.contains(':'))
        .map(|line| line.trim().to_owned())
}

/// Runs the whole life of one battery. The device is served from its own
/// thread, because whoever reads sysfs must not be the one who answers the
/// kernel - in production they are different processes.
fn exercise(kind: Kind, label: &str) {
    let handle = Handle::open(DEV_UHID)
        .expect("opening /dev/uhid: run this test as root (see the file header)");

    let uniq = format!("uhid-battery-test-{label}-{}", std::process::id());
    let identity = Identity {
        name: format!("Test {label}"),
        phys: format!("uhid-battery-test/{label}"),
        uniq: uniq.clone(),
        vendor: 0x1234,
        product: 0x5678,
    };
    let battery = Battery::create(handle, &identity, kind, 42, true).expect("creating the device");

    let stop = Arc::new(AtomicBool::new(false));
    let level = Arc::new(AtomicU8::new(42));
    let charging = Arc::new(AtomicBool::new(true));
    let server = {
        let (stop, level, charging) = (stop.clone(), level.clone(), charging.clone());
        thread::spawn(move || {
            let mut battery = battery;
            while !stop.load(Ordering::Relaxed) {
                battery
                    .serve_until(Instant::now() + Duration::from_millis(100), None)
                    .expect("serving the device");
                let (want, plugged) = (
                    level.load(Ordering::Relaxed),
                    charging.load(Ordering::Relaxed),
                );
                if want != battery.percent() || plugged != battery.charging() {
                    battery.update(want, plugged).expect("updating the level");
                }
            }
            battery
        })
    };

    let base = wait_for(Duration::from_secs(5), || find_power_supply(&uniq))
        .expect("the kernel should have registered a power supply (see `journalctl -k`)");
    eprintln!("{label}: the kernel registered {}", base.display());
    assert!(
        base.to_string_lossy()
            .contains(&format!("hid-{uniq}-battery")),
        "{}",
        base.display()
    );

    expect_attr(&base, "capacity", "42");
    expect_attr(&base, "status", "Charging");
    // UPower must see a peripheral battery, not a system one.
    expect_attr(&base, "scope", "Device");
    // This is the label desktops show.
    expect_attr(&base, "model_name", &format!("Test {label}"));

    match upower_kind(&uniq) {
        Some(seen) => eprintln!("{label}: UPower reports a {seen:?}"),
        None => eprintln!("{label}: UPower did not list the device (is it running?)"),
    }

    level.store(17, Ordering::Relaxed);
    charging.store(false, Ordering::Relaxed);
    expect_attr(&base, "capacity", "17");
    expect_attr(&base, "status", "Discharging");

    stop.store(true, Ordering::Relaxed);
    let battery = server.join().expect("the serving thread");
    let handle = battery.destroy();
    let gone = wait_for(Duration::from_secs(5), || (!base.exists()).then_some(()));
    assert!(
        gone.is_some(),
        "the power supply should be gone: {}",
        base.display()
    );

    // The handle is good for another device: that is the point of getting it
    // back, since an inherited one could not be reopened.
    let again = Battery::create(handle, &identity, kind, 50, false).expect("reusing the handle");
    drop(again);
}

#[test]
#[ignore = "needs write access to /dev/uhid; run as root"]
fn a_generic_battery_reaches_sysfs() {
    exercise(Kind::Generic, "generic");
}

#[test]
#[ignore = "needs write access to /dev/uhid; run as root"]
fn a_mouse_battery_reaches_sysfs() {
    exercise(Kind::Mouse, "mouse");
}
