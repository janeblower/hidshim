use crate::device;
use crate::kbdproto::{self, CONTROL_HARDWARE_ID, HID_HARDWARE_ID};
use crate::pnp::HARDWARE_ID;
use crate::pnp::{Status, status};
use crate::proto::IOCTL_REMOVE;
use crate::win::{check, wide};
use crate::{sign, store};
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    GUID_DEVCLASS_HIDCLASS, GUID_DEVCLASS_SYSTEM,
};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::core::{GUID, w};

/// One driver package: `<name>.dll` and `<name>.inf`, bound to the devnodes
/// with `hardware_id`.
struct Package {
    name: &'static str,
    dll: &'static [u8],
    inf: &'static [u8],
    hardware_id: &'static str,
    class: GUID,
}

macro_rules! package {
    ($name:literal, $hardware_id:expr, $class:expr) => {
        Package {
            name: $name,
            dll: include_bytes!(concat!(env!("HIDSHIM_DRIVER_DIR"), "/", $name, ".dll")),
            inf: include_bytes!(concat!(env!("HIDSHIM_DRIVER_DIR"), "/", $name, ".inf")),
            hardware_id: $hardware_id,
            class: $class,
        }
    };
}

/// Pad, keyboard control device, keyboard HID device, in the order they are
/// bound.
static PACKAGES: [Package; 3] = [
    package!("hidshimxusb", HARDWARE_ID, store::PAD_NODE_CLASS),
    package!("hidshimkbdctl", CONTROL_HARDWARE_ID, GUID_DEVCLASS_SYSTEM),
    package!("hidshimhid", HID_HARDWARE_ID, GUID_DEVCLASS_HIDCLASS),
];

impl Package {
    fn dll(&self) -> String {
        format!("{}.dll", self.name)
    }

    fn inf(&self) -> String {
        format!("{}.inf", self.name)
    }

    fn catalog(&self) -> String {
        format!("{}.cat", self.name)
    }

    fn files(&self) -> [(String, &'static [u8]); 2] {
        [(self.dll(), self.dll), (self.inf(), self.inf)]
    }
}

/// Installs or updates the drivers of the pad and the keyboard and creates
/// their devnodes. Needs administrator rights.
pub fn install() -> io::Result<()> {
    if is_current() {
        return restore();
    }
    // The driver store keys packages by INF content and keeps an existing
    // copy, whose catalog would then be signed by a certificate no longer
    // trusted. Removing everything first makes the new packages the only
    // ones.
    uninstall()?;

    let stage = Stage::create()?;
    for package in &PACKAGES {
        for (name, bytes) in package.files() {
            stage.put(&name, bytes)?;
        }
    }

    let certificate = {
        let signer = sign::Signer::new()?;
        // Signing makes whatever lies in the stage trusted, so the files are
        // checked right before each catalog is built from them.
        for package in &PACKAGES {
            for (name, bytes) in package.files() {
                stage.verify(&name, bytes)?;
            }
            let files = [package.dll(), package.inf()];
            let files = [files[0].as_str(), files[1].as_str()];
            let catalog = sign::make_catalog(&stage.dir, &package.catalog(), &files)?;
            signer.sign(&catalog)?;
        }
        signer.certificate()
    };
    sign::trust(&certificate)?;

    store::ensure_devices()?;
    store::ensure_keyboard_nodes()?;
    for package in &PACKAGES {
        store::install_driver(package.hardware_id, &stage.dir.join(package.inf()))?;
    }
    Ok(())
}

/// Brings the pads back after a reboot and recreates the keyboard's devnodes
/// if they are gone, and does nothing when the driver is not installed.
/// Needs administrator rights.
pub fn restore() -> io::Result<()> {
    if status()? == Status::NotInstalled {
        return Ok(());
    }
    store::ensure_devices()?;
    if store::keyboard_packaged()? {
        store::restore_keyboard()?;
    }
    Ok(())
}

/// Whether every devnode runs exactly our files from a package that still
/// verifies.
fn is_current() -> bool {
    PACKAGES.iter().all(|package| {
        let Some(dir) = store::current_package_dir(&package.class, package.hardware_id) else {
            return false;
        };
        let same =
            |name: String, bytes: &[u8]| std::fs::read(dir.join(name)).is_ok_and(|b| b == bytes);
        same(package.dll(), package.dll)
            && same(package.inf(), package.inf)
            && sign::verify(&dir.join(package.catalog())) == 0
    })
}

/// Removes the devnodes, the driver packages and the signing certificate.
/// Needs administrator rights.
pub fn uninstall() -> io::Result<()> {
    // Without this, WGI clients holding the device veto its removal and
    // Windows asks for a reboot.
    let attached = store::attach_devices()?;
    for interface in device::interfaces(device::ADMIN)? {
        interface.open()?.ioctl(IOCTL_REMOVE, &[], &mut [])?;
    }
    // Makes the keyboard's devices fail themselves, so an owner's open handle
    // does not veto the removal. Should it not work, uninstalling the devnodes
    // below still removes them.
    if let Some(interface) = device::keyboard(device::ADMIN)? {
        let _ = interface
            .open()
            .and_then(|mut device| device.ioctl(kbdproto::IOCTL_REMOVE, &[], &mut []));
    }
    store::remove_devices(attached)?;
    store::remove_keyboard_nodes()?;
    store::remove_packages()?;
    sign::untrust()
}

/// A fresh directory only SYSTEM and Administrators can write to.
struct Stage {
    dir: PathBuf,
}

impl Stage {
    fn create() -> io::Result<Self> {
        let base = std::env::var_os("ProgramData")
            .ok_or_else(|| io::Error::other("ProgramData is not set"))?;
        let mut descriptor = null_mut();
        // SAFETY: the descriptor is freed below.
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        })?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let result = loop {
            // A new name each time: anyone may create directories in
            // ProgramData, so an existing one is never reused.
            let dir =
                Path::new(&base).join(format!("hidshim-{:016x}", RandomState::new().hash_one(0u8)));
            if !dir.as_os_str().is_ascii() {
                break Err(io::Error::other("ProgramData path must be ASCII"));
            }
            let path = wide(&dir);
            // SAFETY: path is NUL-terminated; attributes outlive the call.
            if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } != 0 {
                break Ok(Self { dir });
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
                break Err(err);
            }
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptor.
        unsafe { LocalFree(descriptor) };
        result
    }

    fn put(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(self.dir.join(name), bytes)
    }

    fn verify(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        if std::fs::read(self.dir.join(name))? != bytes {
            return Err(io::Error::other(format!(
                "{name} changed in the staging directory"
            )));
        }
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        // Antivirus scanners briefly hold the freshly written driver files.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
