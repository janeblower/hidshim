//! The pad's devnode.

use crate::device::{self, Device};
use std::io;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, GetLastError, INVALID_HANDLE_VALUE};

pub(crate) const HARDWARE_ID: &str = "SWD\\HIDSHIM_XUSB";

pub(crate) struct DeviceSet(pub(crate) HDEVINFO);

impl Drop for DeviceSet {
    fn drop(&mut self) {
        // SAFETY: the set was created by SetupDi.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn devinfo() -> SP_DEVINFO_DATA {
    SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    }
}

/// Devnodes carrying one of our hardware IDs; with `DIGCF_PRESENT` only
/// those present now.
pub(crate) fn our_devices(
    ids: &[&str],
    flags: SETUP_DI_GET_CLASS_DEVS_FLAGS,
) -> io::Result<(DeviceSet, Vec<SP_DEVINFO_DATA>)> {
    // SAFETY: plain query; the handle is owned by DeviceSet.
    let set = unsafe { SetupDiGetClassDevsW(&GUID_DEVCLASS_SYSTEM, null(), null_mut(), flags) };
    if set == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(io::Error::last_os_error());
    }
    let set = DeviceSet(set);
    let mut found = Vec::new();
    for index in 0.. {
        let mut data = devinfo();
        // SAFETY: data is sized for the call.
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) } == 0 {
            // SAFETY: reads the thread's last error.
            if unsafe { GetLastError() } == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(io::Error::last_os_error());
        }
        let mut buf = [0u16; 512];
        // SAFETY: the buffer size is passed in bytes.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                &data,
                SPDRP_HARDWAREID,
                null_mut(),
                buf.as_mut_ptr().cast(),
                size_of_val(&buf) as u32,
                null_mut(),
            )
        };
        let id = first_string(&buf);
        if ok != 0 && ids.iter().any(|ours| id.eq_ignore_ascii_case(ours)) {
            found.push(data);
        }
    }
    Ok((set, found))
}

pub(crate) fn first_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Whether the pad can be opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The driver is not installed.
    NotInstalled,
    /// The driver is installed but the pad's device is not running, as after
    /// a reboot. Installing again brings it back.
    NotPresent,
    /// [`Gamepad::open`](crate::Gamepad::open) finds the pad.
    Ready,
}

/// Reports whether the driver is installed and its device is running. Needs
/// no elevation.
pub fn status() -> io::Result<Status> {
    if Device::any(device::CONTROL)? {
        return Ok(Status::Ready);
    }
    Ok(if our_devices(&[HARDWARE_ID], 0)?.1.is_empty() {
        Status::NotInstalled
    } else {
        Status::NotPresent
    })
}
