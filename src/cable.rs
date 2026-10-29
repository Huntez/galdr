//! VB-CABLE's own buffer. The driver reads its size from the registry when it starts, and the
//! default is far larger than needed; 512 samples measured clean and a few ms quicker.
//!
//! Writing it needs administrator rights, so the client relaunches itself elevated for that
//! (`tune_elevated`), and the elevated copy calls `tune`.

use std::os::windows::process::CommandExt;
use std::process::Command;

use anyhow::{Result, bail};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_CANCELLED, ERROR_SUCCESS, GetLastError,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, RRF_RT_REG_DWORD, RegCloseKey, RegGetValueW,
    RegOpenKeyExW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};

const KEY: &str = r"SYSTEM\CurrentControlSet\Services\VB-Cable";
const VALUE: &str = "VBAudioCableWDM_Latency";
/// The driver service of the cable device, for restarting it.
const DEVICE_SERVICE: &str = "VBAudioVACMME";
/// Samples; what `tune` sets.
pub const LOW_LATENCY: u32 = 512;

/// Exit codes of the elevated copy.
const DONE: u32 = 0;
const REBOOT: u32 = 2;

/// `tune` was run without administrator rights.
#[derive(Debug)]
pub struct NeedsAdmin;

impl std::fmt::Display for NeedsAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("needs administrator rights")
    }
}

impl std::error::Error for NeedsAdmin {}

/// What `tune` managed: the driver picks the new size up only when it restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tuned {
    Restarted,
    RebootNeeded,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// VB-CABLE is installed (its service key exists).
pub fn installed() -> bool {
    let key = wide(KEY);
    let mut handle: HKEY = std::ptr::null_mut();
    let status =
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, key.as_ptr(), 0, KEY_READ, &mut handle) };
    if status == ERROR_SUCCESS {
        unsafe { RegCloseKey(handle) };
    }
    status == ERROR_SUCCESS
}

/// The configured buffer in samples; None when unset (the driver's large default).
pub fn latency() -> Option<u32> {
    let (key, value) = (wide(KEY), wide(VALUE));
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// VB-CABLE is installed with a larger buffer than `LOW_LATENCY`.
pub fn needs_tuning() -> bool {
    installed() && latency().is_none_or(|l| l > LOW_LATENCY)
}

/// Sets the buffer to `LOW_LATENCY` and restarts the cable. Needs administrator rights.
pub fn tune() -> Result<Tuned> {
    let (key, value) = (wide(KEY), wide(VALUE));
    let data = LOW_LATENCY;
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            REG_DWORD,
            (&raw const data).cast(),
            size_of::<u32>() as u32,
        )
    };
    match status {
        ERROR_SUCCESS => {}
        ERROR_ACCESS_DENIED => return Err(NeedsAdmin.into()),
        e => bail!("cannot write the VB-CABLE setting (error {e})"),
    }
    Ok(if restart_device() {
        Tuned::Restarted
    } else {
        Tuned::RebootNeeded
    })
}

/// Restarts the cable so the driver reads the new size; false when Windows wants a reboot.
fn restart_device() -> bool {
    // Found by service name, which unlike the device's description is not translated.
    let script = format!(
        "$d = @(Get-PnpDevice -PresentOnly -Status OK | Where-Object Service -eq '{DEVICE_SERVICE}'); \
         if ($d.Count -eq 0) {{ exit 1 }}; \
         foreach ($x in $d) {{ pnputil /restart-device \"$($x.InstanceId)\" | Out-Null; \
         if ($LASTEXITCODE -ne 0) {{ exit 1 }} }}"
    );
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

/// Exit code for the elevated copy to report `tune`'s result with.
pub fn exit_code(result: &Result<Tuned>) -> i32 {
    match result {
        Ok(Tuned::Restarted) => DONE as i32,
        Ok(Tuned::RebootNeeded) => REBOOT as i32,
        Err(_) => 1,
    }
}

/// Runs this program again as administrator with `arg` (Windows asks first) and waits for its
/// `tune`.
pub fn tune_elevated(arg: &str) -> Result<Tuned> {
    let exe = wide(&std::env::current_exe()?.to_string_lossy());
    let (verb, params) = (wide("runas"), wide(arg));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = exe.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE: no console window for the command-line client
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        match unsafe { GetLastError() } {
            ERROR_CANCELLED => bail!("cancelled"),
            e => bail!("cannot start as administrator (error {e})"),
        }
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    match code {
        DONE => Ok(Tuned::Restarted),
        REBOOT => Ok(Tuned::RebootNeeded),
        _ => bail!("could not change the VB-CABLE setting"),
    }
}
