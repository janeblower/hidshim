//! hidshim as an installed program: a copy in Program Files and an entry in
//! the installed apps list that uninstalls it.

use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, DETACHED_PROCESS, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    OpenProcessToken, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, FOLDERID_System, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    SHGetKnownFolderPath, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows_sys::core::GUID;

const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\hidshim";
const EXE: &str = "hidshim.exe";

/// Copies this program to Program Files, lists it among installed apps and
/// installs the driver.
pub fn install() -> io::Result<()> {
    let dir = known_folder(&FOLDERID_ProgramFiles)?.join("hidshim");
    let exe = dir.join(EXE);
    let current = std::env::current_exe()?;
    if !same_file(&current, &exe) {
        std::fs::create_dir_all(&dir)?;
        // Renamed into place so the listed program is never half written.
        let part = dir.join("hidshim.exe.part");
        std::fs::copy(&current, &part)?;
        std::fs::rename(&part, &exe)?;
    }
    // Listed before the driver goes in, so a failed install can still be
    // removed from the installed apps list.
    register(&dir, &exe)?;
    hidshim::install()
}

/// Removes the driver, the installed apps entry and the copy in Program Files.
pub fn uninstall() -> io::Result<()> {
    hidshim::uninstall()?;
    let key = wide(UNINSTALL_KEY);
    // SAFETY: key is NUL-terminated.
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, key.as_ptr()) };
    if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let dir = known_folder(&FOLDERID_ProgramFiles)?.join("hidshim");
    let exe = dir.join(EXE);
    if same_file(&std::env::current_exe()?, &exe) {
        return remove_after_exit(&dir, &exe);
    }
    match std::fs::remove_file(&exe) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

/// Runs this program again elevated with the same arguments and returns its
/// exit code, or `None` when this process is elevated already.
pub fn relaunch_elevated() -> io::Result<Option<i32>> {
    if is_elevated()? {
        return Ok(None);
    }
    let exe = wide(std::env::current_exe()?);
    let args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let args = wide(&args);
    let verb = wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb.as_ptr(),
        lpFile: exe.as_ptr(),
        lpParameters: args.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    // SAFETY: info and the strings it points to outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut code = 0;
    // SAFETY: hProcess is a process handle owned here and closed below.
    let ok = unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let ok = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        ok
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(code as i32))
}

fn is_elevated() -> io::Result<bool> {
    let mut token = null_mut();
    // SAFETY: the pseudo handle needs no closing; token is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    // SAFETY: elevation is sized for TokenElevation.
    let ok = unsafe {
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

fn register(dir: &Path, exe: &Path) -> io::Result<()> {
    let key = RegKey::create(UNINSTALL_KEY)?;
    let quoted = format!("\"{}\"", exe.display());
    key.string("DisplayName", "hidshim")?;
    key.string("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    key.string("Publisher", "hidshim")?;
    key.string("URLInfoAbout", "https://github.com/janeblower/hidshim")?;
    key.string("DisplayIcon", &exe.display().to_string())?;
    key.string("InstallLocation", &dir.display().to_string())?;
    key.string("UninstallString", &format!("{quoted} uninstall"))?;
    key.string("QuietUninstallString", &format!("{quoted} uninstall"))?;
    key.dword("NoModify", 1)?;
    key.dword("NoRepair", 1)?;
    let size_kb = std::fs::metadata(exe)?.len().div_ceil(1024);
    key.dword("EstimatedSize", u32::try_from(size_kb).unwrap_or(u32::MAX))
}

/// A running program cannot delete its own file, so cmd does it once this
/// process is gone.
fn remove_after_exit(dir: &Path, exe: &Path) -> io::Result<()> {
    let cmd = known_folder(&FOLDERID_System)?.join("cmd.exe");
    Command::new(cmd)
        .raw_arg(format!(
            "/d /c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"",
            exe.display(),
            dir.display()
        ))
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .map(drop)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        _ => false,
    }
}

fn known_folder(id: &GUID) -> io::Result<PathBuf> {
    let mut path = null_mut();
    // SAFETY: path receives a CoTaskMem string freed below.
    let hr = unsafe { SHGetKnownFolderPath(id, 0, null_mut(), &mut path) };
    if hr < 0 {
        return Err(io::Error::from_raw_os_error(hr));
    }
    // SAFETY: path is a NUL-terminated string from the shell.
    let folder = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        let folder = std::ffi::OsString::from_wide(std::slice::from_raw_parts(path, len));
        CoTaskMemFree(path.cast());
        folder
    };
    Ok(PathBuf::from(folder))
}

fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

struct RegKey(HKEY);

impl RegKey {
    fn create(path: &str) -> io::Result<Self> {
        let path = wide(path);
        let mut key = null_mut();
        // SAFETY: path is NUL-terminated; key is closed on drop.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_WOW64_64KEY,
                null(),
                &mut key,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(Self(key))
    }

    fn set(&self, name: &str, kind: u32, data: &[u8]) -> io::Result<()> {
        let name = wide(name);
        // SAFETY: name is NUL-terminated; data is passed with its length.
        let status = unsafe {
            RegSetValueExW(
                self.0,
                name.as_ptr(),
                0,
                kind,
                data.as_ptr(),
                data.len() as u32,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    fn string(&self, name: &str, value: &str) -> io::Result<()> {
        let data: Vec<u8> = wide(value).iter().flat_map(|c| c.to_le_bytes()).collect();
        self.set(name, REG_SZ, &data)
    }

    fn dword(&self, name: &str, value: u32) -> io::Result<()> {
        self.set(name, REG_DWORD, &value.to_le_bytes())
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegCreateKeyExW.
        unsafe { RegCloseKey(self.0) };
    }
}
