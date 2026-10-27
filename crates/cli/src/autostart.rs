//! A boot task that brings the pads back after a reboot, when Windows drops
//! the software devices.

use std::io;
use std::path::Path;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::TaskScheduler::{
    ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_SERVICE_ACCOUNT, TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, HRESULT};

const TASK: &str = "hidshim";
/// Users may read the task, so `hidshim autostart` reports it without
/// elevation; only SYSTEM and Administrators may change it.
const TASK_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;AU)";

/// Whether the boot task exists and is enabled. Needs no elevation.
pub fn enabled() -> io::Result<bool> {
    // SAFETY: plain COM calls; the arguments outlive them.
    match unsafe { root()?.GetTask(&BSTR::from(TASK)) } {
        Ok(task) => Ok(unsafe { task.Enabled() }?.as_bool()),
        Err(e) if e.code() == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Registers the boot task running `exe restore`, or removes it. Needs
/// administrator rights.
pub fn set(on: bool, exe: &Path) -> io::Result<()> {
    let folder = root()?;
    if !on {
        // SAFETY: plain COM call; the arguments outlive it.
        return match unsafe { folder.DeleteTask(&BSTR::from(TASK), 0) } {
            Err(e) if e.code() == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0) => Ok(()),
            result => result.map_err(Into::into),
        };
    }
    // SAFETY: plain COM call; the arguments outlive it.
    unsafe {
        folder.RegisterTask(
            &BSTR::from(TASK),
            &BSTR::from(definition(exe)),
            TASK_CREATE_OR_UPDATE.0,
            &VARIANT::default(),
            &VARIANT::default(),
            TASK_LOGON_SERVICE_ACCOUNT,
            &VARIANT::from(TASK_SDDL),
        )
    }?;
    Ok(())
}

fn root() -> io::Result<ITaskFolder> {
    // SAFETY: COM stays initialized for the rest of this short-lived process.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if init.is_err() && init != RPC_E_CHANGED_MODE {
        return Err(windows::core::Error::from(init).into());
    }
    // SAFETY: plain COM calls; the arguments outlive them.
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let none = VARIANT::default();
        service.Connect(&none, &none, &none, &none)?;
        Ok(service.GetFolder(&BSTR::from("\\"))?)
    }
}

/// Runs as SYSTEM at boot. Batteries must not stop it: on a laptop the
/// default settings would leave the pads gone until a boot on mains power.
fn definition(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Brings the hidshim pads back after a reboot.</Description>
  </RegistrationInfo>
  <Triggers>
    <BootTrigger><Enabled>true</Enabled></BootTrigger>
  </Triggers>
  <Principals>
    <Principal id="System">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT5M</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="System">
    <Exec>
      <Command>{}</Command>
      <Arguments>restore</Arguments>
    </Exec>
  </Actions>
</Task>"#,
        escape(&exe.display().to_string())
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_runs_restore_from_the_installed_copy() {
        let xml = definition(Path::new(r"C:\Program Files\hidshim\hidshim.exe"));
        assert!(xml.contains(r"<Command>C:\Program Files\hidshim\hidshim.exe</Command>"));
        assert!(xml.contains("<Arguments>restore</Arguments>"));
        assert!(xml.contains("<BootTrigger>"));
        assert!(xml.contains("<UserId>S-1-5-18</UserId>"));
    }

    #[test]
    fn task_survives_running_on_batteries() {
        let xml = definition(Path::new("hidshim.exe"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
    }

    #[test]
    fn path_is_escaped_for_xml() {
        let xml = definition(Path::new(r"C:\A&B <x>\hidshim.exe"));
        assert!(xml.contains(r"<Command>C:\A&amp;B &lt;x&gt;\hidshim.exe</Command>"));
    }
}
