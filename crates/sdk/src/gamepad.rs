use crate::device::{self, Device};
use crate::pnp::{Status, status};
use crate::proto::{
    CAP_MAP_REPORT, CAP_PAD_INFO, IOCTL_GET_PAD_INFO, IOCTL_GET_VERSION, IOCTL_SET_REPORT,
    PROTOCOL_VERSION, PadInfo, Version,
};
use crate::shared::{Hold, SharedReport};
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

/// One virtual pad. XInput sees it while this value is alive; dropping it
/// returns the pad to neutral and removes it from XInput.
#[derive(Debug)]
pub struct Gamepad {
    /// Declared before device: it is dropped first and borrows its handle.
    shared: Option<SharedReport>,
    device: Device,
    index: u8,
}

impl Gamepad {
    /// Claims the first free pad. Fails with [`io::ErrorKind::NotFound`] when
    /// there are no pads, see [`status`](crate::status) for why, and with
    /// [`io::ErrorKind::ResourceBusy`] when other handles own them all.
    pub fn open() -> io::Result<Self> {
        let interfaces = device::interfaces(device::CONTROL)?;
        if interfaces.is_empty() {
            return Err(not_found());
        }
        for interface in &interfaces {
            match interface.open() {
                Ok(device) => return Self::claimed(device, interface.index),
                Err(e) if e.raw_os_error() == Some(ERROR_BUSY as i32) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "every hidshim pad is in use",
        ))
    }

    fn claimed(mut device: Device, index: u8) -> io::Result<Self> {
        let version: Version = device.query(IOCTL_GET_VERSION)?;
        if version.version != PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "hidshim driver speaks protocol {}, expected {PROTOCOL_VERSION}",
                version.version
            )));
        }
        let shared = if version.capabilities & CAP_MAP_REPORT != 0 {
            Some(SharedReport::new(&mut device)?)
        } else {
            None
        };
        Ok(Self {
            shared,
            device,
            index,
        })
    }

    /// Which of the installed pads this is, as [`pads`] lists them.
    pub fn index(&self) -> u8 {
        self.index
    }

    /// Sends the state XInput reports from now on. Fails with
    /// [`io::ErrorKind::NotConnected`] once the pad has been removed, as by
    /// uninstalling; the pad is then gone for good.
    pub fn submit(&mut self, state: &State) -> io::Result<()> {
        if let Some(shared) = &mut self.shared {
            match shared.hold() {
                Hold::Pending => {}
                // A device that is gone also fails a plain request.
                Hold::Cancelled => {
                    let alive = self.device.query::<Version>(IOCTL_GET_VERSION).is_ok();
                    if !alive || !shared.send_hold() {
                        return Err(removed());
                    }
                }
                Hold::Ended => return Err(removed()),
            }
            shared.write(state);
            return Ok(());
        }
        // SAFETY: State is repr(C) plain data without padding.
        let bytes = unsafe {
            std::slice::from_raw_parts((state as *const State).cast::<u8>(), size_of::<State>())
        };
        self.device
            .ioctl(IOCTL_SET_REPORT, bytes, &mut [])
            .map_err(|e| match e.raw_os_error().map(|code| code as u32) {
                // A failed or removed device answers with ERROR_BAD_COMMAND.
                Some(ERROR_BAD_COMMAND | ERROR_DEVICE_REMOVED | ERROR_NO_SUCH_DEVICE) => removed(),
                _ => e,
            })
    }
}

fn removed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the hidshim pad was removed")
}

/// An installed pad as [`pads`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pad {
    pub index: u8,
    /// The process holding the pad open, `None` while it is free.
    pub owner: Option<u32>,
}

/// Lists the running pads and who holds them. Needs no elevation and claims
/// nothing.
pub fn pads() -> io::Result<Vec<Pad>> {
    device::interfaces(device::ADMIN)?
        .iter()
        .map(|interface| {
            let mut device = interface.open()?;
            let version: Version = device.query(IOCTL_GET_VERSION)?;
            if version.capabilities & CAP_PAD_INFO == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the installed hidshim driver cannot list its pads; install hidshim again",
                ));
            }
            let info: PadInfo = device.query(IOCTL_GET_PAD_INFO)?;
            Ok(Pad {
                index: interface.index,
                owner: (info.owned != 0).then_some(info.owner_process_id),
            })
        })
        .collect()
}

fn not_found() -> io::Error {
    let message = match status() {
        Ok(Status::NotInstalled) => "hidshim is not installed",
        _ => "the hidshim pads are not running; installing hidshim again restores them",
    };
    io::Error::new(io::ErrorKind::NotFound, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn state_has_the_layout_of_xinput_gamepad() {
        assert_eq!(offset_of!(State, buttons), 0);
        assert_eq!(offset_of!(State, left_trigger), 2);
        assert_eq!(offset_of!(State, right_trigger), 3);
        assert_eq!(offset_of!(State, left_x), 4);
        assert_eq!(offset_of!(State, left_y), 6);
        assert_eq!(offset_of!(State, right_x), 8);
        assert_eq!(offset_of!(State, right_y), 10);
    }

    #[test]
    fn buttons_combine_and_test_by_bits() {
        let mut held = Buttons::A | Buttons::DPAD_LEFT;
        assert!(held.contains(Buttons::A));
        assert!(!held.contains(Buttons::B));
        assert!(!held.contains(Buttons::A | Buttons::B));
        held |= Buttons::B;
        assert!(held.contains(Buttons::A | Buttons::B));
        assert_eq!(held.0, 0x1000 | 0x2000 | 0x0004);
        assert!(Buttons::default().contains(Buttons::default()));
    }
}
