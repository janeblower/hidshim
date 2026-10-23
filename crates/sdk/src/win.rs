use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_MapCrToWin32Err, CONFIGRET, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::ERROR_GEN_FAILURE;
use windows_sys::core::{BOOL, HRESULT};

pub(crate) fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub(crate) fn check(ok: BOOL) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn hr(code: HRESULT) -> io::Result<()> {
    if code < 0 {
        Err(io::Error::from_raw_os_error(code))
    } else {
        Ok(())
    }
}

pub(crate) fn cr(code: CONFIGRET) -> io::Result<()> {
    if code == CR_SUCCESS {
        return Ok(());
    }
    // SAFETY: pure mapping function.
    let err = unsafe { CM_MapCrToWin32Err(code, ERROR_GEN_FAILURE) };
    Err(io::Error::from_raw_os_error(err as i32))
}
