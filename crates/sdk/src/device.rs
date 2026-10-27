//! Handles to the driver's control interfaces.

use crate::proto::{CONTROL_GUID, Mapping, PadInfo, Version};
use crate::win::{check, cr};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_BUFFER_SMALL,
};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::CreateEventW;

/// Reference strings of the control interfaces, see drivers/xusb/hidshim.h.
pub(crate) const CONTROL: &str = "ctl";
pub(crate) const ADMIN: &str = "admin";

/// An overlapped handle, so that a pending IOCTL_HIDSHIM_HOLD does not block
/// the others, and the event its own requests complete on.
#[derive(Debug)]
pub(crate) struct Device {
    handle: OwnedHandle,
    done: OwnedHandle,
}

/// An overlapped that completes on `event`.
pub(crate) fn overlapped(event: &OwnedHandle) -> OVERLAPPED {
    // SAFETY: OVERLAPPED is plain data; all zeroes is its initial state, in
    // which GetOverlappedResult does not wait.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.as_raw_handle();
    overlapped
}

/// A manual-reset event, unsignaled.
pub(crate) fn event() -> io::Result<OwnedHandle> {
    // SAFETY: no name, default security; the result is checked.
    let raw = unsafe { CreateEventW(null(), 1, 0, null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh handle owned by nobody else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// A present control interface of one pad.
pub(crate) struct Interface {
    pub index: u8,
    path: Vec<u16>,
}

impl Interface {
    pub(crate) fn open(&self) -> io::Result<Device> {
        Device::open(&self.path)
    }
}

/// Replies the driver fills in whole: `repr(C)` structs of `u32` fields.
pub(crate) trait Reply: Default {}
impl Reply for Version {}
impl Reply for PadInfo {}
impl Reply for Mapping {}

impl Device {
    fn open(path: &[u16]) -> io::Result<Self> {
        // SAFETY: path is NUL-terminated; the result is checked.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh handle owned by nobody else.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        Ok(Self {
            handle,
            done: event()?,
        })
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    /// Sends a request and waits for it. `&mut` because requests share the
    /// completion event.
    pub(crate) fn ioctl(&mut self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<()> {
        let mut overlapped = overlapped(&self.done);
        let mut returned = 0;
        // SAFETY: buffers and overlapped live until GetOverlappedResult below
        // has waited for the request; lengths match the buffers.
        let sent = unsafe {
            DeviceIoControl(
                self.raw(),
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                null_mut(),
                &mut overlapped,
            )
        };
        // SAFETY: plain thread-local read.
        if sent == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the request was started with this overlapped; bWait = TRUE.
        check(unsafe { GetOverlappedResult(self.raw(), &overlapped, &mut returned, 1) })?;
        if returned as usize != output.len() {
            return Err(io::Error::other("short reply from hidshim driver"));
        }
        Ok(())
    }

    pub(crate) fn query<T: Reply>(&mut self, code: u32) -> io::Result<T> {
        let mut reply = T::default();
        // SAFETY: Reply types are repr(C) plain data; any bytes are a valid value.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut((&raw mut reply).cast::<u8>(), size_of::<T>())
        };
        self.ioctl(code, &[], bytes)?;
        Ok(reply)
    }
}

/// Present control interfaces with this reference string, by pad index.
pub(crate) fn interfaces(reference: &str) -> io::Result<Vec<Interface>> {
    let mut found: Vec<Interface> = paths()?
        .split(|&c| c == 0)
        .filter_map(|path| {
            let text = String::from_utf16_lossy(path);
            if !has_reference(&text, reference) {
                return None;
            }
            Some(Interface {
                index: pad_index(&text)?,
                path: path.iter().copied().chain(Some(0)).collect(),
            })
        })
        .collect();
    found.sort_by_key(|interface| interface.index);
    Ok(found)
}

fn has_reference(path: &str, reference: &str) -> bool {
    path.rsplit_once('\\')
        .is_some_and(|(_, tail)| tail.eq_ignore_ascii_case(reference))
}

/// The pad index in an interface path such as
/// `\\?\SWD#HIDSHIM#XUSB2#{...}\ctl`. The single pad of 0.1.0 had no number.
fn pad_index(path: &str) -> Option<u8> {
    const MARKER: &str = "#HIDSHIM#XUSB";
    let start = path.to_ascii_uppercase().find(MARKER)? + MARKER.len();
    let rest = &path[start..];
    let digits = &rest[..rest.find('#')?];
    if digits.is_empty() {
        return Some(0);
    }
    digits.parse().ok()
}

/// The present interface list of the control GUID, NUL-separated.
fn paths() -> io::Result<Vec<u16>> {
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
        if status != CR_BUFFER_SMALL {
            cr(status)?;
            return Ok(list);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUID: &str = "{8d0b6c2e-3f1a-4e5b-9c7d-2a6e1f4b8d30}";

    #[test]
    fn pad_index_comes_from_the_instance_id() {
        let path = |instance: &str| format!(r"\\?\{instance}#{GUID}\ctl");
        assert_eq!(pad_index(&path("SWD#HIDSHIM#XUSB0")), Some(0));
        assert_eq!(pad_index(&path("swd#hidshim#xusb3")), Some(3));
        assert_eq!(pad_index(&path("SWD#HIDSHIM#XUSB")), Some(0));
        assert_eq!(pad_index(&path("SWD#HIDSHIM#XUSBx")), None);
        assert_eq!(pad_index(&path("ROOT#SYSTEM#0001")), None);
    }

    #[test]
    fn reference_must_be_the_whole_last_component() {
        let path = |tail: &str| format!(r"\\?\SWD#HIDSHIM#XUSB0#{GUID}\{tail}");
        assert!(has_reference(&path("CTL"), CONTROL));
        assert!(!has_reference(&path("ctl"), ADMIN));
        assert!(!has_reference(&path("xctl"), CONTROL));
    }
}
