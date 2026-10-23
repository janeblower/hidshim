use crate::proto::{CONTROL_GUID, IOCTL_GET_VERSION, IOCTL_SET_REPORT, PROTOCOL_VERSION, Version};
use crate::win::{check, cr};
use std::io;
use std::ops::{BitOr, BitOrAssign};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_BUFFER_SMALL,
};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Pad buttons, the `wButtons` bits of `XINPUT_GAMEPAD`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Buttons(pub u16);

impl Buttons {
    pub const DPAD_UP: Self = Self(0x0001);
    pub const DPAD_DOWN: Self = Self(0x0002);
    pub const DPAD_LEFT: Self = Self(0x0004);
    pub const DPAD_RIGHT: Self = Self(0x0008);
    pub const START: Self = Self(0x0010);
    pub const BACK: Self = Self(0x0020);
    pub const LEFT_THUMB: Self = Self(0x0040);
    pub const RIGHT_THUMB: Self = Self(0x0080);
    pub const LEFT_SHOULDER: Self = Self(0x0100);
    pub const RIGHT_SHOULDER: Self = Self(0x0200);
    pub const GUIDE: Self = Self(0x0400);
    pub const A: Self = Self(0x1000);
    pub const B: Self = Self(0x2000);
    pub const X: Self = Self(0x4000);
    pub const Y: Self = Self(0x8000);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Buttons {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Buttons {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Pad state with the layout of `XINPUT_GAMEPAD`: sticks are centered on
/// zero with positive Y up, triggers run from 0 to 255.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub buttons: Buttons,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

const _: () = assert!(size_of::<State>() == 12);

/// The virtual pad. XInput sees it while this value is alive; dropping it
/// returns the pad to neutral and removes it from XInput.
#[derive(Debug)]
pub struct Gamepad {
    handle: OwnedHandle,
}

impl Gamepad {
    /// Claims the pad. Fails with [`io::ErrorKind::NotFound`] when the driver
    /// is not installed and with `ERROR_BUSY` when another process owns it.
    pub fn open() -> io::Result<Self> {
        let path = control_path()?;
        // SAFETY: path is NUL-terminated; the result is checked.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh handle owned by nobody else.
        let pad = Self {
            handle: unsafe { OwnedHandle::from_raw_handle(raw) },
        };
        let mut version = Version::default();
        // SAFETY: Version is repr(C) plain data; any bytes are a valid value.
        let reply = unsafe {
            std::slice::from_raw_parts_mut((&raw mut version).cast::<u8>(), size_of::<Version>())
        };
        pad.ioctl(IOCTL_GET_VERSION, &[], reply)?;
        if version.version != PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "hidshim driver speaks protocol {}, expected {PROTOCOL_VERSION}",
                version.version
            )));
        }
        Ok(pad)
    }

    /// Sends the state XInput reports from now on.
    pub fn submit(&mut self, state: &State) -> io::Result<()> {
        // SAFETY: State is repr(C) plain data without padding.
        let bytes = unsafe {
            std::slice::from_raw_parts((state as *const State).cast::<u8>(), size_of::<State>())
        };
        self.ioctl(IOCTL_SET_REPORT, bytes, &mut [])
    }

    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<()> {
        let mut returned = 0;
        // SAFETY: buffers outlive the synchronous call; lengths match them.
        check(unsafe {
            DeviceIoControl(
                self.handle.as_raw_handle(),
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut returned,
                null_mut(),
            )
        })?;
        if returned as usize != output.len() {
            return Err(io::Error::other("short reply from hidshim driver"));
        }
        Ok(())
    }
}

/// The first present control interface, as a NUL-terminated path.
fn control_path() -> io::Result<Vec<u16>> {
    loop {
        let mut len = 0;
        // SAFETY: out-pointer to a local.
        cr(unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &CONTROL_GUID,
                null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        })?;
        let mut list = vec![0u16; len as usize];
        // SAFETY: list holds len characters.
        let status = unsafe {
            CM_Get_Device_Interface_ListW(
                &CONTROL_GUID,
                null(),
                list.as_mut_ptr(),
                len,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        // The list can grow between the two calls.
        if status == CR_BUFFER_SMALL {
            continue;
        }
        cr(status)?;
        let end = list.iter().position(|&c| c == 0).unwrap_or(0);
        if end == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "hidshim driver is not installed",
            ));
        }
        list.truncate(end + 1);
        return Ok(list);
    }
}
