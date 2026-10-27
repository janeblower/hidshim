//! The pad's software device and the driver packages behind it.

use crate::pnp::{HARDWARE_ID, first_string, our_devices};
use crate::win::{check, cr, hr, wide};
use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Enumeration::Pnp::*;
use windows_sys::Win32::Devices::Properties::{DEVPKEY_Device_DriverInfPath, DEVPROPTYPE};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::core::{GUID, HRESULT, PCWSTR, w};

const ENUMERATOR: PCWSTR = w!("HIDSHIM");
/// XInput serves four slots, physical pads included.
pub(crate) const PADS: u8 = 4;
/// Devnodes of builds that used a root-enumerated device.
const LEGACY_HARDWARE_ID: &str = "ROOT\\HIDSHIM_XUSB";
/// GameInput groups device interfaces by ContainerId: a root devnode only
/// gets the shared sentinel, which folds the pad into unrelated devices, and
/// pads sharing one would show as one controller. Pad n uses this plus n.
const CONTAINER_ID: u128 = 0x6a3e5f71_2c8d_4b9a_9e14_5d7c3b2a1f80;
const PROVIDER: &str = "hidshim";
pub(crate) const CATALOG: &str = "hidshimxusb.cat";

/// Creates the pads' software devnodes, which outlive this process but not a
/// reboot: after one they stay registered yet absent until created again.
pub(crate) fn ensure_devices() -> io::Result<()> {
    let present = instances(DIGCF_PRESENT)?;
    for index in 0..PADS {
        let instance = instance(index);
        if present.iter().any(|p| p.eq_ignore_ascii_case(&instance)) {
            continue;
        }
        let device = SoftwareDevice::create(&instance)?;
        // SAFETY: the handle came from SwDeviceCreate.
        hr(unsafe { SwDeviceSetLifetime(device.0, SWDeviceLifetimeParentPresent) })?;
    }
    Ok(())
}

/// Hands every persistent devnode back to a handle, whose close in
/// [`remove_devices`] removes it for good. Attaching must come before the
/// driver fails the devices: once one of them is removed, attaching another
/// failed one never completes.
pub(crate) fn attach_devices() -> io::Result<Vec<SoftwareDevice>> {
    instances(0)?
        .iter()
        .map(|instance| {
            let device = SoftwareDevice::create(instance)?;
            // SAFETY: the handle came from SwDeviceCreate.
            hr(unsafe { SwDeviceSetLifetime(device.0, SWDeviceLifetimeHandle) })?;
            Ok(device)
        })
        .collect()
}

pub(crate) fn remove_devices(attached: Vec<SoftwareDevice>) -> io::Result<()> {
    drop(attached);
    wait_gone()?;
    let (set, devices) = our_devices(&[HARDWARE_ID, LEGACY_HARDWARE_ID], 0)?;
    for data in devices {
        // SAFETY: data belongs to set.
        check(unsafe { DiUninstallDevice(null_mut(), set.0, &data, 0, null_mut()) })?;
    }
    Ok(())
}

fn instance(index: u8) -> String {
    format!("XUSB{index}")
}

/// Instance names, such as `XUSB0`, of our software devnodes.
fn instances(flags: SETUP_DI_GET_CLASS_DEVS_FLAGS) -> io::Result<Vec<String>> {
    let (_set, devices) = our_devices(&[HARDWARE_ID], flags)?;
    devices
        .iter()
        .map(|data| {
            let mut buf = [0u16; MAX_DEVICE_ID_LEN as usize + 1];
            // SAFETY: the buffer length is passed in characters.
            cr(unsafe { CM_Get_Device_IDW(data.DevInst, buf.as_mut_ptr(), buf.len() as u32, 0) })?;
            let id = first_string(&buf);
            Ok(id.rsplit('\\').next().unwrap_or_default().to_string())
        })
        .collect()
}

/// Uninstalling a devnode PnP is still removing fails.
fn wait_gone() -> io::Result<()> {
    let start = Instant::now();
    while !instances(DIGCF_PRESENT)?.is_empty() {
        if start.elapsed() > Duration::from_secs(30) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "removing the pad devices timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// The single pad of 0.1.0, instance `XUSB`, keeps the base ContainerId.
fn container_id(instance: &str) -> GUID {
    let index = instance
        .get(4..)
        .and_then(|n| n.parse::<u8>().ok())
        .unwrap_or(0);
    GUID::from_u128(CONTAINER_ID + u128::from(index))
}

pub(crate) struct SoftwareDevice(HSWDEVICE);

impl SoftwareDevice {
    fn create(instance: &str) -> io::Result<Self> {
        let mut ids = wide(HARDWARE_ID);
        ids.push(0);
        let name = wide(instance);
        let container = container_id(instance);
        let info = SW_DEVICE_CREATE_INFO {
            cbSize: size_of::<SW_DEVICE_CREATE_INFO>() as u32,
            pszInstanceId: name.as_ptr(),
            pszzHardwareIds: ids.as_ptr(),
            pContainerId: &container,
            CapabilityFlags: (SWDeviceCapabilitiesSilentInstall
                | SWDeviceCapabilitiesDriverRequired) as u32,
            pszDeviceDescription: w!("hidshim XInput pad"),
            ..Default::default()
        };
        let (sender, receiver) = channel::<HRESULT>();
        // The callback owns its sender: it is still inside send() when the
        // receive below returns, so one borrowed from this frame would be
        // dropped under it.
        let context = Box::into_raw(Box::new(sender));
        let mut handle = null_mut();
        // SAFETY: info and its strings outlive the call, which reads them
        // before the callback reports.
        let created = hr(unsafe {
            SwDeviceCreate(
                ENUMERATOR,
                w!("HTREE\\ROOT\\0"),
                &info,
                0,
                null(),
                Some(on_created),
                context.cast_const().cast(),
                &mut handle,
            )
        });
        if let Err(e) = created {
            // SAFETY: the callback never runs when creation fails outright.
            drop(unsafe { Box::from_raw(context) });
            return Err(e);
        }
        let device = Self(handle);
        match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(code) => hr(code).map(|()| device),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "software device creation timed out",
            )),
        }
    }
}

impl Drop for SoftwareDevice {
    fn drop(&mut self) {
        // SAFETY: the handle came from SwDeviceCreate.
        unsafe { SwDeviceClose(self.0) };
    }
}

unsafe extern "system" fn on_created(
    _device: HSWDEVICE,
    result: HRESULT,
    context: *const c_void,
    _instance: PCWSTR,
) {
    // SAFETY: context is the boxed Sender SoftwareDevice::create hands over;
    // the callback runs once.
    let sender = unsafe { Box::from_raw(context.cast_mut().cast::<Sender<HRESULT>>()) };
    let _ = sender.send(result);
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
    let (_set, devices) = our_devices(&[HARDWARE_ID], 0).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pad_gets_its_own_container() {
        let value = |g: GUID| {
            (u128::from(g.data1) << 96)
                | (u128::from(g.data2) << 80)
                | (u128::from(g.data3) << 64)
                | u128::from(u64::from_be_bytes(g.data4))
        };
        for index in 0..PADS {
            let id = value(container_id(&instance(index)));
            assert_eq!(id, CONTAINER_ID + u128::from(index));
        }
        assert_eq!(value(container_id("XUSB")), CONTAINER_ID);
    }
}
