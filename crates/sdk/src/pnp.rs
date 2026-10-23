//! The pad's root devnode and the driver packages behind it.

use crate::win::{check, cr, wide};
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::{DEVPKEY_Device_DriverInfPath, DEVPROPTYPE};
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::core::w;

const DEVICE_NAME: &str = "HIDSHIM_XUSB";
const HARDWARE_ID: &str = "ROOT\\HIDSHIM_XUSB";
const PROVIDER: &str = "hidshim";
pub(crate) const CATALOG: &str = "hidshimxusb.cat";

struct DeviceSet(HDEVINFO);

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

/// Root devnodes carrying our hardware ID, present or not.
fn our_devices() -> io::Result<(DeviceSet, Vec<SP_DEVINFO_DATA>)> {
    // SAFETY: plain query; the handle is owned by DeviceSet.
    let set = unsafe { SetupDiGetClassDevsW(&GUID_DEVCLASS_SYSTEM, w!("ROOT"), null_mut(), 0) };
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
        let mut ids = [0u16; 512];
        // SAFETY: the buffer size is passed in bytes.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                &data,
                SPDRP_HARDWAREID,
                null_mut(),
                ids.as_mut_ptr().cast(),
                size_of_val(&ids) as u32,
                null_mut(),
            )
        };
        if ok != 0 && first_string(&ids).eq_ignore_ascii_case(HARDWARE_ID) {
            found.push(data);
        }
    }
    Ok((set, found))
}

fn first_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub(crate) fn ensure_device() -> io::Result<()> {
    if !our_devices()?.1.is_empty() {
        return Ok(());
    }
    // SAFETY: the handle is owned by DeviceSet.
    let set = DeviceSet(unsafe { SetupDiCreateDeviceInfoList(&GUID_DEVCLASS_SYSTEM, null_mut()) });
    if set.0 == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(io::Error::last_os_error());
    }
    let mut data = devinfo();
    let name = wide(DEVICE_NAME);
    let mut ids = wide(HARDWARE_ID);
    ids.push(0);
    // SAFETY: strings are NUL-terminated; the hardware ID is a multi-sz
    // whose size is passed in bytes.
    unsafe {
        check(SetupDiCreateDeviceInfoW(
            set.0,
            name.as_ptr(),
            &GUID_DEVCLASS_SYSTEM,
            null(),
            null_mut(),
            DICD_GENERATE_ID,
            &mut data,
        ))?;
        check(SetupDiSetDeviceRegistryPropertyW(
            set.0,
            &mut data,
            SPDRP_HARDWAREID,
            ids.as_ptr().cast(),
            (ids.len() * 2) as u32,
        ))?;
        check(SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, &data))
    }
}

pub(crate) fn remove_devices() -> io::Result<()> {
    let (set, devices) = our_devices()?;
    for data in devices {
        // SAFETY: data belongs to set.
        check(unsafe { DiUninstallDevice(null_mut(), set.0, &data, 0, null_mut()) })?;
    }
    Ok(())
}

/// Adds the package to the driver store and binds it to the devnode.
pub(crate) fn install_driver(inf: &Path) -> io::Result<()> {
    let id = wide(HARDWARE_ID);
    let inf = wide(inf);
    let mut reboot = 0;
    // SAFETY: strings are NUL-terminated.
    check(unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            null_mut(),
            id.as_ptr(),
            inf.as_ptr(),
            INSTALLFLAG_FORCE,
            &mut reboot,
        )
    })
}

/// The driver store package the devnode runs, such as `oem12.inf`.
fn current_package() -> Option<String> {
    let (_set, devices) = our_devices().ok()?;
    let mut buf = [0u16; 64];
    let mut size = size_of_val(&buf) as u32;
    let mut kind: DEVPROPTYPE = 0;
    // SAFETY: the buffer size is passed in bytes.
    cr(unsafe {
        CM_Get_DevNode_PropertyW(
            devices.first()?.DevInst,
            &DEVPKEY_Device_DriverInfPath,
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut size,
            0,
        )
    })
    .ok()?;
    Some(first_string(&buf))
}

/// The driver store directory of the package the devnode runs.
pub(crate) fn current_package_dir() -> Option<PathBuf> {
    let name = wide(current_package()?);
    let mut buf = [0u16; 512];
    // SAFETY: the buffer size is passed in characters.
    let ok = unsafe {
        SetupGetInfDriverStoreLocationW(
            name.as_ptr(),
            null(),
            null(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    let inf = PathBuf::from(OsString::from_wide(
        &buf[..buf.iter().position(|&c| c == 0)?],
    ));
    inf.parent().map(Path::to_path_buf)
}

/// Removes all our driver store packages.
pub(crate) fn remove_packages() -> io::Result<()> {
    for package in our_packages()? {
        let name = wide(package.file_name().unwrap());
        // SAFETY: name is NUL-terminated.
        check(unsafe { SetupUninstallOEMInfW(name.as_ptr(), SUOI_FORCEDELETE, null()) })?;
    }
    Ok(())
}

fn our_packages() -> io::Result<Vec<PathBuf>> {
    let windows =
        std::env::var_os("SystemRoot").ok_or_else(|| io::Error::other("SystemRoot is not set"))?;
    let mut packages = Vec::new();
    for entry in std::fs::read_dir(Path::new(&windows).join("INF"))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        if name.starts_with("oem") && name.ends_with(".inf") && is_ours(&path) {
            packages.push(path);
        }
    }
    Ok(packages)
}

fn is_ours(inf: &Path) -> bool {
    let path = wide(inf);
    let mut line = 0;
    // SAFETY: path is NUL-terminated; the handle is closed below.
    let handle = unsafe { SetupOpenInfFileW(path.as_ptr(), null(), INF_STYLE_WIN4, &mut line) };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let value = |key: &str| {
        let key = wide(key);
        let mut buf = [0u16; 128];
        // SAFETY: the buffer size is passed in characters.
        let ok = unsafe {
            SetupGetLineTextW(
                null(),
                handle,
                w!("Version"),
                key.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                null_mut(),
            )
        };
        if ok == 0 {
            OsString::new()
        } else {
            OsString::from_wide(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)])
        }
    };
    let ours = value("Provider").eq_ignore_ascii_case(PROVIDER)
        && value("CatalogFile").eq_ignore_ascii_case(CATALOG);
    // SAFETY: handle came from SetupOpenInfFileW.
    unsafe { SetupCloseInfFile(handle) };
    ours
}
