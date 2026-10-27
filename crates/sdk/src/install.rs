use crate::device::{self, Device};
use crate::proto::IOCTL_REMOVE;
use crate::win::{check, wide};
use crate::{sign, store};
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::core::w;

const DRIVER: &str = "hidshimxusb.dll";
const INF: &str = "hidshimxusb.inf";
static DRIVER_BYTES: &[u8] =
    include_bytes!(concat!(env!("HIDSHIM_DRIVER_DIR"), "/hidshimxusb.dll"));
static INF_BYTES: &[u8] = include_bytes!(concat!(env!("HIDSHIM_DRIVER_DIR"), "/hidshimxusb.inf"));

/// Installs or updates the driver and creates the pad's devnode. Needs
/// administrator rights.
pub fn install() -> io::Result<()> {
    if is_current() {
        return store::ensure_device();
    }
    // The driver store keys packages by INF content and keeps an existing
    // copy, whose catalog would then be signed by a certificate no longer
    // trusted. Removing everything first makes the new package the only one.
    uninstall()?;

    let stage = Stage::create()?;
    stage.put(DRIVER, DRIVER_BYTES)?;
    stage.put(INF, INF_BYTES)?;

    let certificate = {
        let signer = sign::Signer::new()?;
        // Signing makes whatever lies in the stage trusted, so the files are
        // checked right before the catalog is built from them.
        stage.verify(DRIVER, DRIVER_BYTES)?;
        stage.verify(INF, INF_BYTES)?;
        let catalog = sign::make_catalog(&stage.dir, store::CATALOG, &[DRIVER, INF])?;
        signer.sign(&catalog)?;
        signer.certificate()
    };
    sign::trust(&certificate)?;

    store::ensure_device()?;
    store::install_driver(&stage.dir.join(INF))
}

/// Whether the devnode runs exactly our files from a package that still
/// verifies.
fn is_current() -> bool {
    let Some(dir) = store::current_package_dir() else {
        return false;
    };
    let same = |name: &str, bytes: &[u8]| std::fs::read(dir.join(name)).is_ok_and(|b| b == bytes);
    same(DRIVER, DRIVER_BYTES)
        && same(INF, INF_BYTES)
        && sign::verify(&dir.join(store::CATALOG)) == 0
}

/// Removes the devnode, the driver packages and the signing certificate.
/// Needs administrator rights.
pub fn uninstall() -> io::Result<()> {
    // Without this, WGI clients holding the device veto its removal and
    // Windows asks for a reboot.
    for device in Device::open_all(device::ADMIN)? {
        device?.ioctl(IOCTL_REMOVE, &[], &mut [])?;
    }
    store::remove_devices()?;
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
