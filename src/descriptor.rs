//! HID report descriptors that make `hid-input` register a battery.
//!
//! Three rules, each learned from `drivers/hid/hid-input.c` refusing to
//! cooperate:
//!
//! * The top-level collection must be an *input application*
//!   (`IS_INPUT_APPLICATION`: generic desktop, digitizer, consumer control).
//!   Otherwise `hidinput_connect()` returns before looking at a single field,
//!   and the device gets a hidraw node and nothing else.
//! * The device must end up with a populated input node, or it is torn down
//!   again ("No inputs registered, leaving") and the battery goes with it.
//! * `Battery Strength` (Generic Device Controls, usage `0x20`) registers the
//!   power supply; `Charging` (Battery System, usage `0x44`) flips its status.
//!   The strength comes first in its report: when the kernel has to *ask* for
//!   the level it reads the byte right after the report ID.

/// What the desktop should take the device for.
///
/// UPower types a HID battery after its sibling input node, so the kind decides
/// which input usages the descriptor declares next to the battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A pointer: udev tags the input node `ID_INPUT_MOUSE` and UPower reports
    /// a mouse. The pointer is never moved.
    Mouse,
    /// No recognisable input class: UPower reports a plain battery.
    Generic,
    /// As [`Kind::Generic`] on the wire. There is no input class for headsets;
    /// UPower only gets there through a sibling node tagged like a sound card,
    /// which takes the udev rule from [`Kind::udev_rule`].
    Headset,
}

impl Kind {
    /// The report descriptor for this kind.
    #[must_use]
    pub fn descriptor(self) -> &'static [u8] {
        match self {
            Self::Mouse => MOUSE.as_slice(),
            Self::Generic | Self::Headset => GENERIC.as_slice(),
        }
    }

    /// ID of the input report carrying the battery.
    ///
    /// Recent kernels put it in the power supply's name
    /// (`hid-<uniq>-battery-<id>`), so it is part of what users see.
    #[must_use]
    pub fn report_id(self) -> u8 {
        match self {
            Self::Mouse => 2,
            Self::Generic | Self::Headset => 1,
        }
    }

    /// The udev rule this kind needs, if any, for devices whose `phys` matches
    /// the glob `phys`.
    ///
    /// Only [`Kind::Headset`] has one. UPower promotes a HID battery to
    /// "headset" when a sibling node carries the properties systemd puts on a
    /// sound card, and it accepts an `input` node as that sibling - which the
    /// virtual device has. Install the rule under `/etc/udev/rules.d`.
    ///
    /// Also `None` when `phys` holds a double quote or a control character:
    /// either would end the match early and let the rest be read as rule text,
    /// in a file that root installs.
    #[must_use]
    pub fn udev_rule(self, phys: &str) -> Option<String> {
        let quotable = !phys.chars().any(|c| c == '"' || c.is_control());
        (self == Self::Headset && quotable).then(|| {
            format!(
                "SUBSYSTEM==\"input\", KERNEL==\"input*\", ATTR{{phys}}==\"{phys}\", \
                 ENV{{SOUND_INITIALIZED}}=\"1\", ENV{{SOUND_FORM_FACTOR}}=\"headset\""
            )
        })
    }
}

/// The battery fields shared by every descriptor: a level from 0 to 100, then
/// a charging bit. Each descriptor pads the rest of the byte itself.
const BATTERY_FIELDS: [u8; 25] = [
    0x05, 0x06, //       Usage Page (Generic Device Controls)
    0x09, 0x20, //       Usage (Battery Strength)
    0x15, 0x00, //       Logical Minimum (0)
    0x26, 0x64, 0x00, // Logical Maximum (100)
    0x75, 0x08, //       Report Size (8)
    0x95, 0x01, //       Report Count (1)
    0x81, 0x02, //       Input (Data, Variable, Absolute)
    0x05, 0x85, //       Usage Page (Battery System)
    0x09, 0x44, //       Usage (Charging)
    0x25, 0x01, //       Logical Maximum (1)
    0x75, 0x01, //       Report Size (1)
    0x81, 0x02, //       Input (Data, Variable, Absolute)
];

/// One Mouse application: report 1 is a minimal pointer that is never sent,
/// there so the input node is populated and tagged `ID_INPUT_MOUSE`; report 2
/// is the battery.
static MOUSE: Assembled = concat_bytes(&[
    &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x02, // Usage (Mouse)
        0xa1, 0x01, // Collection (Application)
        0x85, 0x01, //   Report ID (1)
        0x09, 0x01, //   Usage (Pointer)
        0xa1, 0x00, //   Collection (Physical)
        0x05, 0x09, //     Usage Page (Button)
        0x19, 0x01, //     Usage Minimum (1)
        0x29, 0x03, //     Usage Maximum (3)
        0x15, 0x00, //     Logical Minimum (0)
        0x25, 0x01, //     Logical Maximum (1)
        0x75, 0x01, //     Report Size (1)
        0x95, 0x03, //     Report Count (3)
        0x81, 0x02, //     Input (Data, Variable, Absolute)
        0x75, 0x05, //     Report Size (5)
        0x95, 0x01, //     Report Count (1)
        0x81, 0x03, //     Input (Constant) - padding
        0x05, 0x01, //     Usage Page (Generic Desktop)
        0x09, 0x30, //     Usage (X)
        0x09, 0x31, //     Usage (Y)
        0x15, 0x81, //     Logical Minimum (-127)
        0x25, 0x7f, //     Logical Maximum (127)
        0x75, 0x08, //     Report Size (8)
        0x95, 0x02, //     Report Count (2)
        0x81, 0x06, //     Input (Data, Variable, Relative)
        0xc0, //         End Collection
        0x85, 0x02, //   Report ID (2)
    ],
    &BATTERY_FIELDS,
    &[
        0x75, 0x07, // Report Size (7)
        0x81, 0x03, // Input (Constant) - padding
        0xc0, //       End Collection
    ],
]);

/// One Consumer Control application holding the battery and a single
/// vendor-defined bit.
///
/// The bit is what keeps the input node alive: an unknown one-bit usage is
/// mapped to `BTN_MISC`, which is outside every range systemd's `input_id`
/// looks at, so udev tags the node as nothing in particular. It is never set.
/// The vendor page is `0xff21` because the kernel ignores the usages of
/// `0xff00`, `0xff01`, `0xff09`, `0xff31`, `0xff43`, `0xff7f`, `0xffa0`,
/// `0xffbc` and `0xffd1` instead of mapping them.
static GENERIC: Assembled = concat_bytes(&[
    &[
        0x05, 0x0c, // Usage Page (Consumer)
        0x09, 0x01, // Usage (Consumer Control)
        0xa1, 0x01, // Collection (Application)
        0x85, 0x01, //   Report ID (1)
    ],
    &BATTERY_FIELDS,
    &[
        0x06, 0x21, 0xff, // Usage Page (Vendor Defined 0xff21)
        0x09, 0x02, //       Usage (0x02) -> BTN_MISC, never reported
        0x81, 0x02, //       Input (Data, Variable, Absolute)
        0x75, 0x06, //       Report Size (6)
        0x81, 0x03, //       Input (Constant) - padding
        0xc0, //           End Collection
    ],
]);

/// Longest descriptor this module builds.
const MAX_LEN: usize = 96;

/// A descriptor assembled at compile time, with its length.
struct Assembled {
    bytes: [u8; MAX_LEN],
    len: usize,
}

impl Assembled {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

const fn concat_bytes(parts: &[&[u8]]) -> Assembled {
    let mut bytes = [0u8; MAX_LEN];
    let mut len = 0;
    let mut part = 0;
    while part < parts.len() {
        let mut i = 0;
        while i < parts[part].len() {
            bytes[len] = parts[part][i];
            len += 1;
            i += 1;
        }
        part += 1;
    }
    Assembled { bytes, len }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// razerd's descriptor, byte for byte: changing it would rename the power
    /// supply its users already have history for.
    const RAZERD_MOUSE: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19,
        0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x75, 0x05,
        0x95, 0x01, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75,
        0x08, 0x95, 0x02, 0x81, 0x06, 0xc0, 0x85, 0x02, 0x05, 0x06, 0x09, 0x20, 0x15, 0x00, 0x26,
        0x64, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0x05, 0x85, 0x09, 0x44, 0x25, 0x01, 0x75,
        0x01, 0x81, 0x02, 0x75, 0x07, 0x81, 0x03, 0xc0,
    ];

    /// The descriptor HeadsetBatteryIndicator shipped with.
    const HBI_GENERIC: &[u8] = &[
        0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x06, 0x09, 0x20, 0x15, 0x00, 0x26,
        0x64, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0x05, 0x85, 0x09, 0x44, 0x25, 0x01, 0x75,
        0x01, 0x81, 0x02, 0x06, 0x21, 0xff, 0x09, 0x02, 0x81, 0x02, 0x75, 0x06, 0x81, 0x03, 0xc0,
    ];

    #[test]
    fn descriptors_match_the_ones_already_in_the_field() {
        assert_eq!(Kind::Mouse.descriptor(), RAZERD_MOUSE);
        assert_eq!(Kind::Generic.descriptor(), HBI_GENERIC);
        assert_eq!(Kind::Headset.descriptor(), HBI_GENERIC);
    }

    #[test]
    fn every_descriptor_opens_with_an_input_application() {
        // Generic Desktop / Mouse, and Consumer / Consumer Control.
        assert_eq!(
            &Kind::Mouse.descriptor()[..6],
            [0x05, 0x01, 0x09, 0x02, 0xa1, 0x01]
        );
        assert_eq!(
            &Kind::Generic.descriptor()[..6],
            [0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01]
        );
    }

    #[test]
    fn the_battery_report_id_is_the_one_the_descriptor_declares() {
        for kind in [Kind::Mouse, Kind::Generic, Kind::Headset] {
            let descriptor = kind.descriptor();
            let battery = descriptor
                .windows(4)
                .position(|w| w == [0x05, 0x06, 0x09, 0x20])
                .expect("a battery strength usage");
            // The last Report ID item before the battery fields.
            let id = descriptor[..battery]
                .windows(2)
                .rev()
                .find(|w| w[0] == 0x85)
                .map(|w| w[1]);
            assert_eq!(id, Some(kind.report_id()), "{kind:?}");
        }
    }

    #[test]
    fn only_headsets_need_a_udev_rule() {
        assert_eq!(Kind::Mouse.udev_rule("x/*"), None);
        assert_eq!(Kind::Generic.udev_rule("x/*"), None);
        let rule = Kind::Headset.udev_rule("my-daemon/*").unwrap();
        assert!(rule.contains(r#"ATTR{phys}=="my-daemon/*""#), "{rule}");
        assert!(
            rule.contains(r#"ENV{SOUND_FORM_FACTOR}="headset""#),
            "{rule}"
        );
        assert_eq!(Kind::Headset.udev_rule("x\", RUN+=\"/bin/true"), None);
        assert_eq!(Kind::Headset.udev_rule("x\nKERNEL==\"*\""), None);
    }
}
