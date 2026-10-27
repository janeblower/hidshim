//! Mirror of drivers/xusb/hidshim.h.

use windows_sys::core::GUID;

pub(crate) const PROTOCOL_VERSION: u32 = 1;
pub(crate) const CONTROL_GUID: GUID = GUID::from_u128(0x8d0b6c2e_3f1a_4e5b_9c7d_2a6e1f4b8d30);

use crate::gamepad::State;
use std::sync::atomic::AtomicU32;

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;
const FILE_WRITE_ACCESS: u32 = 2;

const fn ctl_code(function: u32, access: u32) -> u32 {
    (FILE_DEVICE_UNKNOWN << 16) | (access << 14) | (function << 2) | METHOD_BUFFERED
}

pub(crate) const IOCTL_GET_VERSION: u32 = ctl_code(0x800, FILE_ANY_ACCESS);
pub(crate) const IOCTL_SET_REPORT: u32 = ctl_code(0x801, FILE_WRITE_ACCESS);
#[cfg(feature = "install")]
pub(crate) const IOCTL_REMOVE: u32 = ctl_code(0x802, FILE_WRITE_ACCESS);
pub(crate) const IOCTL_GET_PAD_INFO: u32 = ctl_code(0x803, FILE_ANY_ACCESS);
pub(crate) const IOCTL_MAP_REPORT: u32 = ctl_code(0x804, FILE_WRITE_ACCESS);
pub(crate) const IOCTL_HOLD: u32 = ctl_code(0x805, FILE_WRITE_ACCESS);

pub(crate) const CAP_PAD_INFO: u32 = 0x1;
pub(crate) const CAP_MAP_REPORT: u32 = 0x2;

#[repr(C)]
#[derive(Default)]
pub(crate) struct Version {
    pub version: u32,
    pub capabilities: u32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct PadInfo {
    pub owned: u32,
    pub owner_process_id: u32,
}

/// Handle values in this process.
#[repr(C)]
#[derive(Default)]
pub(crate) struct Mapping {
    pub section: u64,
    pub event: u64,
}

/// The section the driver reads reports from, `HIDSHIM_SHARED`.
#[repr(C)]
pub(crate) struct Shared {
    pub sequence: AtomicU32,
    pub report: State,
    pub waiting: AtomicU32,
}

const _: () = assert!(size_of::<Version>() == 8);
const _: () = assert!(size_of::<Mapping>() == 16);
const _: () = assert!(size_of::<Shared>() == 20);
const _: () = assert!(size_of::<PadInfo>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_codes_match_the_driver_header() {
        assert_eq!(IOCTL_GET_VERSION, 0x0022_2000);
        assert_eq!(IOCTL_SET_REPORT, 0x0022_a004);
        #[cfg(feature = "install")]
        assert_eq!(IOCTL_REMOVE, 0x0022_a008);
        assert_eq!(IOCTL_GET_PAD_INFO, 0x0022_200c);
        assert_eq!(IOCTL_MAP_REPORT, 0x0022_a010);
        assert_eq!(IOCTL_HOLD, 0x0022_a014);
    }

    #[test]
    fn shared_has_the_layout_of_the_driver_header() {
        use std::mem::offset_of;
        assert_eq!(offset_of!(Shared, sequence), 0);
        assert_eq!(offset_of!(Shared, report), 4);
        assert_eq!(offset_of!(Shared, waiting), 16);
    }
}
