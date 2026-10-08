//! How a virtual device introduces itself to the kernel.

use std::io;

use crate::event;

/// How the virtual device introduces itself.
///
/// Built from the two strings that matter and then adjusted:
///
/// ```
/// use uhid_battery::Identity;
///
/// let identity = Identity::new("Audeze Maxwell", "my-daemon-maxwell")
///     .phys("my-daemon/maxwell")
///     .vendor(0x3329)
///     .product(0x4b18);
/// assert_eq!(identity.uniq, "my-daemon-maxwell");
/// ```
///
/// The fields can be read but not written: the struct is `#[non_exhaustive]`
/// so that a later field (`version`, say) does not break every daemon that
/// builds one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Identity {
    /// Device name. It reaches UPower as the model, which is the label desktops
    /// show, so make it the product's marketing name.
    pub name: String,
    /// Physical path, free-form; empty unless [`Identity::phys`] set it. A udev
    /// rule can match on it, which is what [`crate::Kind::udev_rule`] relies
    /// on; `<daemon>/<device>` is a good shape.
    pub phys: String,
    /// Unique ID. The kernel names the power supply `hid-<uniq>-battery[-<n>]`,
    /// so it has to be unique on the machine and safe as a file name and in a
    /// udev or shell match: 1 to 63 characters from `[A-Za-z0-9._-]`. Never
    /// put a string that came from the device in here.
    pub uniq: String,
    /// Vendor ID, usually mirrored from the real device; `0` unless set.
    pub vendor: u16,
    /// Product ID, usually mirrored from the real device; `0` unless set.
    pub product: u16,
}

impl Identity {
    /// An identity with the given name and unique ID, no `phys`, and zero
    /// vendor and product IDs.
    ///
    /// Nothing is checked here; [`crate::Battery::create`] refuses an identity
    /// the kernel would truncate or that cannot name a power supply, and says
    /// so with [`crate::CreateErrorKind::InvalidIdentity`].
    #[must_use]
    pub fn new(name: impl Into<String>, uniq: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            phys: String::new(),
            uniq: uniq.into(),
            vendor: 0,
            product: 0,
        }
    }

    /// Sets the physical path. [`crate::Kind::Headset`] needs one for its udev
    /// rule to match on; the others do without.
    #[must_use]
    pub fn phys(mut self, phys: impl Into<String>) -> Self {
        self.phys = phys.into();
        self
    }

    /// Sets the vendor ID.
    #[must_use]
    pub fn vendor(mut self, vendor: u16) -> Self {
        self.vendor = vendor;
        self
    }

    /// Sets the product ID.
    #[must_use]
    pub fn product(mut self, product: u16) -> Self {
        self.product = product;
        self
    }

    /// Checks that the kernel will take the strings as they are.
    ///
    /// Each one lands in a fixed-size, NUL-terminated field. A string that did
    /// not fit would be cut short without a word, and for `uniq` that means a
    /// power supply under a name [`crate::Battery::power_supply`] never looks
    /// for. `uniq` is held to `[A-Za-z0-9._-]` besides: it becomes a directory
    /// name under `/sys/class/power_supply`, and whitespace, a newline or a
    /// glob character in there would make a mess of every udev rule or shell
    /// snippet that matches on it.
    pub(crate) fn validate(&self) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, phys: &str, uniq: &str) -> Identity {
        Identity::new(name, uniq).phys(phys)
    }

    #[test]
    fn the_builder_fills_what_it_is_given_and_zeroes_the_rest() {
        let bare = Identity::new("Name", "uniq");
        assert_eq!(bare.name, "Name");
        assert_eq!(bare.uniq, "uniq");
        assert_eq!(bare.phys, "");
        assert_eq!((bare.vendor, bare.product), (0, 0));

        let full = Identity::new("Name", "uniq")
            .phys("d/1")
            .vendor(0x1532)
            .product(0x00cc);
        assert_eq!(full.phys, "d/1");
        assert_eq!((full.vendor, full.product), (0x1532, 0x00cc));
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
