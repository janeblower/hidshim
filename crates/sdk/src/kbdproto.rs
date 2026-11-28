//! Mirror of drivers/hid/hidshimkbd.h. The IOCTLs that the keyboard's control
//! device shares with the pad's (version, map, hold) are taken from `proto`.

#[cfg(feature = "install")]
use crate::proto::FILE_WRITE_ACCESS;
use crate::proto::{FILE_ANY_ACCESS, ctl_code};
use std::sync::atomic::AtomicU32;
use windows_sys::core::GUID;

pub(crate) const PROTOCOL_VERSION: u32 = 1;
pub(crate) const CONTROL_GUID: GUID = GUID::from_u128(0x428da1f6_1bfe_4c29_9bb4_d6aa00b39a4b);

/// Hardware IDs of the two root devnodes.
#[cfg(feature = "install")]
pub(crate) const HID_HARDWARE_ID: &str = "ROOT\\HIDSHIM_KBD";
pub(crate) const CONTROL_HARDWARE_ID: &str = "ROOT\\HIDSHIM_KBDCTL";

#[cfg(feature = "install")]
pub(crate) const IOCTL_REMOVE: u32 = ctl_code(0x802, FILE_WRITE_ACCESS);
pub(crate) const IOCTL_GET_INFO: u32 = ctl_code(0x803, FILE_ANY_ACCESS);

pub(crate) const REPORT_KEYBOARD: u8 = 1;
pub(crate) const REPORT_CONSUMER: u8 = 2;
pub(crate) const REPORT_MOUSE: u8 = 3;
/// Lengths with the report ID byte.
pub(crate) const KEYBOARD_LEN: usize = 30;
#[cfg(test)]
pub(crate) const CONSUMER_LEN: usize = 9;
#[cfg(test)]
pub(crate) const MOUSE_LEN: usize = 8;

pub(crate) const SLOTS: usize = 64;
pub(crate) const SLOT_LEN: usize = 32;

/// One ready report, the report ID first; the driver ignores bytes past the
/// length its ID implies.
pub(crate) type Slot = [u8; SLOT_LEN];

#[repr(C)]
#[derive(Default)]
pub(crate) struct Info {
    pub owned: u32,
    pub owner_process_id: u32,
    pub hid_connected: u32,
    pub hid_process_id: u32,
}

/// The section, `HIDSHIM_KBD_SHARED`: a ring of reports with one writer (the
/// owner, `head`) and one reader (the driver, `tail`).
#[repr(C)]
pub(crate) struct Shared {
    pub head: AtomicU32,
    pub tail: AtomicU32,
    pub waiting: AtomicU32,
    pub reserved: u32,
    pub slots: [Slot; SLOTS],
}

const _: () = assert!(size_of::<Info>() == 16);
const _: () = assert!(size_of::<Shared>() == 2064);

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn control_codes_match_the_driver_header() {
        #[cfg(feature = "install")]
        assert_eq!(IOCTL_REMOVE, 0x0022_a008);
        assert_eq!(IOCTL_GET_INFO, 0x0022_200c);
    }

    #[test]
    fn shared_has_the_layout_of_the_driver_header() {
        assert_eq!(offset_of!(Shared, head), 0);
        assert_eq!(offset_of!(Shared, tail), 4);
        assert_eq!(offset_of!(Shared, waiting), 8);
        assert_eq!(offset_of!(Shared, slots), 16);
    }
}
