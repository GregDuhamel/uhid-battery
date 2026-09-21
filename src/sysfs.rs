//! Finding the power supply the kernel registered for a virtual device.

use std::path::{Path, PathBuf};

const POWER_SUPPLY_CLASS: &str = "/sys/class/power_supply";

/// Locates the power supply of the device whose unique ID is `uniq`.
///
/// The name is not stable across kernels: it was `hid-<uniq>-battery` for
/// years, and recent kernels append the report ID (`hid-<uniq>-battery-1`) now
/// that a HID device may carry several batteries. Match on the prefix, and
/// accept nothing after it but that number.
#[must_use]
pub fn find_power_supply(uniq: &str) -> Option<PathBuf> {
    find_power_supply_in(Path::new(POWER_SUPPLY_CLASS), uniq)
}

pub(crate) fn find_power_supply_in(class_dir: &Path, uniq: &str) -> Option<PathBuf> {
    let prefix = format!("hid-{uniq}-battery");
    std::fs::read_dir(class_dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix(&prefix)
                .is_some_and(is_report_suffix)
        })
        .map(|entry| entry.path())
}

/// What may follow `hid-<uniq>-battery`: nothing, or `-<report id>`. Anything
/// looser would claim the supply of a device whose unique ID is `<uniq>-battery`.
fn is_report_suffix(rest: &str) -> bool {
    rest.is_empty()
        || rest
            .strip_prefix('-')
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_supply_is_found_under_both_kernel_naming_schemes() {
        let dir = std::env::temp_dir().join(format!("uhid-battery-psy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for name in [
            "hid-headset-3329-4b18-battery-1",
            "hid-razerd-battery",
            // Somebody else's device whose uniq merely starts the same way.
            "hid-headset-3329-4b18-batteryx",
            // And one whose uniq is ours followed by `-battery`.
            "hid-razerd-battery-battery",
            "hid-absent-battery-",
        ] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }

        assert_eq!(
            find_power_supply_in(&dir, "headset-3329-4b18"),
            Some(dir.join("hid-headset-3329-4b18-battery-1"))
        );
        assert_eq!(
            find_power_supply_in(&dir, "razerd"),
            Some(dir.join("hid-razerd-battery"))
        );
        assert_eq!(find_power_supply_in(&dir, "absent"), None);
        assert_eq!(
            find_power_supply_in(Path::new("/nonexistent"), "razerd"),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
