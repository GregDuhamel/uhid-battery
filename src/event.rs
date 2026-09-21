//! The `/dev/uhid` wire format.
//!
//! A single `struct uhid_event` is written to, and read from, the character
//! device. It is `__packed`, has a fixed size, and has been stable since Linux
//! 3.18, so the offsets are spelled out here rather than generated.

use std::io;

/// `BUS_VIRTUAL` from `linux/input.h`.
///
/// Device-specific kernel HID drivers all match on a physical bus, so a device
/// on the virtual bus is left to `hid-generic` even when it borrows the vendor
/// and product IDs of real hardware.
pub(crate) const BUS_VIRTUAL: u16 = 0x06;

// Event types, from `linux/uhid.h`.
const UHID_DESTROY: u32 = 1;
const UHID_START: u32 = 2;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

/// `UHID_INPUT_REPORT`, the `rtype` of a request about an input report.
pub(crate) const RTYPE_INPUT: u8 = 2;

/// `EIO`, the error carried by replies to requests we do not serve.
pub(crate) const EIO: u16 = 5;

// Field offsets inside `struct uhid_event`: the 4-byte type, then the union.
const OFF_TYPE: usize = 0;
const OFF_CREATE_NAME: usize = 4;
pub(crate) const LEN_NAME: usize = 128;
const OFF_CREATE_PHYS: usize = OFF_CREATE_NAME + LEN_NAME;
pub(crate) const LEN_PHYS: usize = 64;
const OFF_CREATE_UNIQ: usize = OFF_CREATE_PHYS + LEN_PHYS;
pub(crate) const LEN_UNIQ: usize = 64;
const OFF_CREATE_RD_SIZE: usize = OFF_CREATE_UNIQ + LEN_UNIQ;
const OFF_CREATE_BUS: usize = OFF_CREATE_RD_SIZE + 2;
const OFF_CREATE_VENDOR: usize = OFF_CREATE_BUS + 2;
const OFF_CREATE_PRODUCT: usize = OFF_CREATE_VENDOR + 4;
const OFF_CREATE_RD_DATA: usize = OFF_CREATE_PRODUCT + 4 + 4 + 4;

/// `HID_MAX_DESCRIPTOR_SIZE`.
pub(crate) const RD_DATA_MAX: usize = 4096;
/// `UHID_DATA_MAX`.
const DATA_MAX: usize = 4096;

/// Size of `struct uhid_event`: the type tag plus its largest union member.
pub(crate) const EVENT_SIZE: usize = OFF_CREATE_RD_DATA + RD_DATA_MAX;

const OFF_INPUT_SIZE: usize = 4;
const OFF_INPUT_DATA: usize = 6;
const OFF_REQUEST_ID: usize = 4;
const OFF_REQUEST_RNUM: usize = 8;
const OFF_REQUEST_RTYPE: usize = 9;
const OFF_REPLY_ERR: usize = 8;
const OFF_GET_REPLY_SIZE: usize = 10;
const OFF_GET_REPLY_DATA: usize = 12;

/// A full-size, zeroed event. The kernel copies `min(len, sizeof(event))`, so
/// always writing the whole structure is both simplest and exact.
pub(crate) type Buffer = [u8; EVENT_SIZE];

/// An event received from the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// The driver is being attached; sent part-way through the probe.
    Start,
    /// The kernel wants the current value of a report.
    GetReport { id: u32, rnum: u8, rtype: u8 },
    /// The kernel wants to set a report.
    SetReport { id: u32 },
    /// Anything that needs no answer: stop, open, close, output.
    Ignored,
}

pub(crate) fn decode(buf: &Buffer) -> Event {
    match get_u32(buf, OFF_TYPE) {
        UHID_START => Event::Start,
        UHID_GET_REPORT => Event::GetReport {
            id: get_u32(buf, OFF_REQUEST_ID),
            rnum: buf[OFF_REQUEST_RNUM],
            rtype: buf[OFF_REQUEST_RTYPE],
        },
        UHID_SET_REPORT => Event::SetReport {
            id: get_u32(buf, OFF_REQUEST_ID),
        },
        // Stop, open, close, output, and whatever the kernel adds next.
        _ => Event::Ignored,
    }
}

pub(crate) fn create2(
    name: &str,
    phys: &str,
    uniq: &str,
    vendor: u32,
    product: u32,
    descriptor: &[u8],
) -> io::Result<Buffer> {
    let size = u16::try_from(descriptor.len())
        .ok()
        .filter(|size| usize::from(*size) <= RD_DATA_MAX)
        .ok_or_else(|| invalid("report descriptor exceeds HID_MAX_DESCRIPTOR_SIZE"))?;

    let mut event = blank(UHID_CREATE2);
    put_str(
        &mut event[OFF_CREATE_NAME..OFF_CREATE_NAME + LEN_NAME],
        name,
    );
    put_str(
        &mut event[OFF_CREATE_PHYS..OFF_CREATE_PHYS + LEN_PHYS],
        phys,
    );
    put_str(
        &mut event[OFF_CREATE_UNIQ..OFF_CREATE_UNIQ + LEN_UNIQ],
        uniq,
    );
    put_u16(&mut event, OFF_CREATE_RD_SIZE, size);
    put_u16(&mut event, OFF_CREATE_BUS, BUS_VIRTUAL);
    put_u32(&mut event, OFF_CREATE_VENDOR, vendor);
    put_u32(&mut event, OFF_CREATE_PRODUCT, product);
    event[OFF_CREATE_RD_DATA..OFF_CREATE_RD_DATA + descriptor.len()].copy_from_slice(descriptor);
    Ok(event)
}

pub(crate) fn destroy() -> Buffer {
    blank(UHID_DESTROY)
}

pub(crate) fn input2(report: &[u8]) -> io::Result<Buffer> {
    let size = payload_len(report)?;
    let mut event = blank(UHID_INPUT2);
    put_u16(&mut event, OFF_INPUT_SIZE, size);
    event[OFF_INPUT_DATA..OFF_INPUT_DATA + report.len()].copy_from_slice(report);
    Ok(event)
}

/// Answers a `GET_REPORT`. With `None` the request is refused with `EIO`.
///
/// The kernel expects the report ID as the first byte of the data, the same
/// convention `hid_hw_raw_request()` uses: `hid-input` reads the level from
/// the byte that follows it.
pub(crate) fn get_report_reply(id: u32, report: Option<&[u8]>) -> io::Result<Buffer> {
    let mut event = blank(UHID_GET_REPORT_REPLY);
    put_u32(&mut event, OFF_REQUEST_ID, id);
    match report {
        Some(report) => {
            put_u16(&mut event, OFF_GET_REPLY_SIZE, payload_len(report)?);
            event[OFF_GET_REPLY_DATA..OFF_GET_REPLY_DATA + report.len()].copy_from_slice(report);
        }
        None => put_u16(&mut event, OFF_REPLY_ERR, EIO),
    }
    Ok(event)
}

/// Refuses a `SET_REPORT`: nothing on a battery is writable.
pub(crate) fn set_report_reply(id: u32) -> Buffer {
    let mut event = blank(UHID_SET_REPORT_REPLY);
    put_u32(&mut event, OFF_REQUEST_ID, id);
    put_u16(&mut event, OFF_REPLY_ERR, EIO);
    event
}

fn blank(kind: u32) -> Buffer {
    let mut event = [0u8; EVENT_SIZE];
    put_u32(&mut event, OFF_TYPE, kind);
    event
}

fn payload_len(data: &[u8]) -> io::Result<u16> {
    u16::try_from(data.len())
        .ok()
        .filter(|len| usize::from(*len) <= DATA_MAX)
        .ok_or_else(|| invalid("report exceeds UHID_DATA_MAX"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Copies `value` into a fixed-size, NUL-padded field, truncating on a `char`
/// boundary so the kernel never sees a partial UTF-8 sequence. `Identity` is
/// validated before it gets here, so the truncation is a backstop.
fn put_str(field: &mut [u8], value: &str) {
    let mut end = value.len().min(field.len() - 1);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    field[..end].copy_from_slice(&value.as_bytes()[..end]);
}

fn put_u16(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn get_u32(buf: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_ne_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_is_as_large_as_the_kernel_structure() {
        // 4 (type) + 128 + 64 + 64 (strings) + 2 + 2 + 4 * 4 + 4096.
        assert_eq!(EVENT_SIZE, 4376);
        assert_eq!(OFF_CREATE_RD_DATA, 280);
    }

    #[test]
    fn create2_is_laid_out_where_the_kernel_expects() {
        let event = create2(
            "Audeze Maxwell",
            "test/0",
            "headset-1",
            0x3329,
            0x4b18,
            &[1, 2, 3],
        )
        .unwrap();
        assert_eq!(get_u32(&event, OFF_TYPE), UHID_CREATE2);
        assert_eq!(
            &event[OFF_CREATE_NAME..OFF_CREATE_NAME + 14],
            b"Audeze Maxwell"
        );
        assert_eq!(event[OFF_CREATE_NAME + 14], 0, "strings stay NUL padded");
        assert_eq!(&event[OFF_CREATE_UNIQ..OFF_CREATE_UNIQ + 9], b"headset-1");
        assert_eq!(event[OFF_CREATE_RD_SIZE], 3);
        assert_eq!(event[OFF_CREATE_BUS], 0x06);
        assert_eq!(get_u32(&event, OFF_CREATE_VENDOR), 0x3329);
        assert_eq!(get_u32(&event, OFF_CREATE_PRODUCT), 0x4b18);
        assert_eq!(
            &event[OFF_CREATE_RD_DATA..OFF_CREATE_RD_DATA + 3],
            [1, 2, 3]
        );
    }

    #[test]
    fn an_oversized_descriptor_is_refused() {
        assert!(create2("n", "p", "u", 0, 0, &[0; RD_DATA_MAX + 1]).is_err());
        assert!(create2("n", "p", "u", 0, 0, &[0; RD_DATA_MAX]).is_ok());
    }

    #[test]
    fn long_strings_are_truncated_on_a_char_boundary() {
        let mut field = [0u8; 8];
        put_str(&mut field, "ééééééééé");
        assert_eq!(&field, b"\xc3\xa9\xc3\xa9\xc3\xa9\0\0");
    }

    #[test]
    fn requests_are_decoded() {
        let mut buf = [0u8; EVENT_SIZE];
        put_u32(&mut buf, OFF_TYPE, UHID_GET_REPORT);
        put_u32(&mut buf, OFF_REQUEST_ID, 7);
        buf[OFF_REQUEST_RNUM] = 2;
        buf[OFF_REQUEST_RTYPE] = RTYPE_INPUT;
        assert_eq!(
            decode(&buf),
            Event::GetReport {
                id: 7,
                rnum: 2,
                rtype: RTYPE_INPUT
            }
        );

        put_u32(&mut buf, OFF_TYPE, UHID_START);
        assert_eq!(decode(&buf), Event::Start);
        put_u32(&mut buf, OFF_TYPE, 5); // UHID_CLOSE
        assert_eq!(decode(&buf), Event::Ignored);
        put_u32(&mut buf, OFF_TYPE, 999);
        assert_eq!(decode(&buf), Event::Ignored);
    }

    #[test]
    fn replies_carry_the_request_id_and_either_data_or_an_error() {
        let served = get_report_reply(9, Some(&[2, 80, 1])).unwrap();
        assert_eq!(get_u32(&served, OFF_REQUEST_ID), 9);
        assert_eq!(served[OFF_REPLY_ERR], 0);
        assert_eq!(served[OFF_GET_REPLY_SIZE], 3);
        assert_eq!(
            &served[OFF_GET_REPLY_DATA..OFF_GET_REPLY_DATA + 3],
            [2, 80, 1]
        );

        let refused = get_report_reply(9, None).unwrap();
        assert_eq!(u16::from(refused[OFF_REPLY_ERR]), EIO);
        assert_eq!(u16::from(set_report_reply(4)[OFF_REPLY_ERR]), EIO);
    }
}
