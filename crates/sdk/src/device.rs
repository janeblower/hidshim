//! Handles to the driver's control interfaces.

use crate::proto::CONTROL_GUID;
use crate::win::{check, cr};
use std::io;
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

/// Reference strings of the control interfaces, see drivers/xusb/hidshim.h.
pub(crate) const CONTROL: &str = "ctl";
pub(crate) const ADMIN: &str = "admin";

#[derive(Debug)]
pub(crate) struct Device(OwnedHandle);

impl Device {
    /// Opens every present control interface with this reference string.
    pub(crate) fn open_all(reference: &str) -> io::Result<Vec<io::Result<Self>>> {
        Ok(paths(reference)?
            .iter()
            .map(|path| Self::open(path))
            .collect())
    }

    fn open(path: &[u16]) -> io::Result<Self> {
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
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(raw) }))
    }

    pub(crate) fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<()> {
        let mut returned = 0;
        // SAFETY: buffers outlive the synchronous call; lengths match them.
        check(unsafe {
            DeviceIoControl(
                self.0.as_raw_handle(),
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

/// NUL-terminated paths of the present control interfaces ending in
/// `\reference`.
fn paths(reference: &str) -> io::Result<Vec<Vec<u16>>> {
    let suffix: Vec<u16> = format!("\\{reference}").encode_utf16().collect();
    let list = loop {
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
            break list;
        }
    };
    Ok(list
        .split(|&c| c == 0)
        .filter(|path| {
            path.len() >= suffix.len()
                && String::from_utf16_lossy(&path[path.len() - suffix.len()..])
                    .eq_ignore_ascii_case(&String::from_utf16_lossy(&suffix))
        })
        .map(|path| path.iter().copied().chain(Some(0)).collect())
        .collect())
}
