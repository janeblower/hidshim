//! Mirror of drivers/xusb/hidshim.h.

use windows_sys::core::GUID;

pub(crate) const PROTOCOL_VERSION: u32 = 1;
pub(crate) const CONTROL_GUID: GUID = GUID::from_u128(0x8d0b6c2e_3f1a_4e5b_9c7d_2a6e1f4b8d30);

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;
const FILE_WRITE_ACCESS: u32 = 2;

const fn ctl_code(function: u32, access: u32) -> u32 {
    (FILE_DEVICE_UNKNOWN << 16) | (access << 14) | (function << 2) | METHOD_BUFFERED
}

pub(crate) const IOCTL_GET_VERSION: u32 = ctl_code(0x800, FILE_ANY_ACCESS);
pub(crate) const IOCTL_SET_REPORT: u32 = ctl_code(0x801, FILE_WRITE_ACCESS);

#[repr(C)]
#[derive(Default)]
pub(crate) struct Version {
    pub version: u32,
    pub capabilities: u32,
}

const _: () = assert!(size_of::<Version>() == 8);
