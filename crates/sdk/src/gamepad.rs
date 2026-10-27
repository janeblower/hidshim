use crate::device::{self, Device};
use crate::pnp::{Status, status};
use crate::proto::{IOCTL_GET_VERSION, IOCTL_SET_REPORT, PROTOCOL_VERSION, Version};
use std::io;
use std::ops::{BitOr, BitOrAssign};
use windows_sys::Win32::Foundation::{
    ERROR_BAD_COMMAND, ERROR_BUSY, ERROR_DEVICE_REMOVED, ERROR_NO_SUCH_DEVICE,
};

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
    device: Device,
}

impl Gamepad {
    /// Claims the pad. Fails with [`io::ErrorKind::NotFound`] when the pad is
    /// not there, see [`status`](crate::status) for why, and with
    /// [`io::ErrorKind::ResourceBusy`] when another process owns it.
    pub fn open() -> io::Result<Self> {
        let device = Device::open_all(device::CONTROL)?
            .into_iter()
            .next()
            .unwrap_or_else(|| Err(not_found()))
            .map_err(|e| match e.raw_os_error() {
                Some(code) if code == ERROR_BUSY as i32 => io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "the hidshim pad is in use by another process",
                ),
                _ => e,
            })?;
        let mut version = Version::default();
        // SAFETY: Version is repr(C) plain data; any bytes are a valid value.
        let reply = unsafe {
            std::slice::from_raw_parts_mut((&raw mut version).cast::<u8>(), size_of::<Version>())
        };
        device.ioctl(IOCTL_GET_VERSION, &[], reply)?;
        if version.version != PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "hidshim driver speaks protocol {}, expected {PROTOCOL_VERSION}",
                version.version
            )));
        }
        Ok(Self { device })
    }

    /// Sends the state XInput reports from now on. Fails with
    /// [`io::ErrorKind::NotConnected`] once the pad has been removed, as by
    /// uninstalling; the pad is then gone for good.
    pub fn submit(&mut self, state: &State) -> io::Result<()> {
        // SAFETY: State is repr(C) plain data without padding.
        let bytes = unsafe {
            std::slice::from_raw_parts((state as *const State).cast::<u8>(), size_of::<State>())
        };
        self.device
            .ioctl(IOCTL_SET_REPORT, bytes, &mut [])
            .map_err(|e| match e.raw_os_error().map(|code| code as u32) {
                // A failed or removed device answers with ERROR_BAD_COMMAND.
                Some(ERROR_BAD_COMMAND | ERROR_DEVICE_REMOVED | ERROR_NO_SUCH_DEVICE) => {
                    io::Error::new(io::ErrorKind::NotConnected, "the hidshim pad was removed")
                }
                _ => e,
            })
    }
}

fn not_found() -> io::Error {
    let message = match status() {
        Ok(Status::NotInstalled) => "hidshim is not installed",
        _ => "the hidshim pad is not running; installing hidshim again restores it",
    };
    io::Error::new(io::ErrorKind::NotFound, message)
}
