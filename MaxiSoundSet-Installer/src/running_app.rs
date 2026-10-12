//! Scope shutdown requests to the executable owned by this installation.
use anyhow::{ensure, Context, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use windows::{
    core::{BOOL, PCWSTR, PWSTR},
    Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, HANDLE, HWND, LPARAM, WPARAM,
        },
        System::{Diagnostics::ToolHelp::*, Threading::*},
        UI::WindowsAndMessaging::*,
    },
};

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn matches(handle: HANDLE, executable: &Path) -> Result<bool> {
    let mut name = vec![0u16; 32768];
    let mut len = name.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(name.as_mut_ptr()),
            &mut len,
        )?;
    }
    Ok(crate::platform::same(
        Path::new(&String::from_utf16_lossy(&name[..len as usize])),
        executable,
    ))
}
fn processes(root: &Path) -> Result<Vec<Process>> {
    let executable = root.join("MaxiSoundSet.exe");
    let snapshot = Process(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let first = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    if let Err(e) = &first {
        ensure!(
            e.code() == ERROR_NO_MORE_FILES.to_hresult(),
            "Cannot enumerate running applications: {e}"
        );
    }
    let mut more = first.is_ok();
    let mut found = Vec::new();
    while more {
        let length = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..length])
            .eq_ignore_ascii_case("MaxiSoundSet.exe")
        {
            match unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    entry.th32ProcessID,
                )
            } {
                Ok(handle) => {
                    let process = Process(handle);
                    let state = unsafe { WaitForSingleObject(process.0, 0) };
                    ensure!(
                        state == windows::Win32::Foundation::WAIT_TIMEOUT
                            || state == windows::Win32::Foundation::WAIT_OBJECT_0,
                        "Cannot verify the running application: {}",
                        windows::core::Error::from_win32()
                    );
                    if state == windows::Win32::Foundation::WAIT_TIMEOUT
                        && matches(process.0, &executable)
                            .context("Cannot verify the running MAXI SOUNDSET installation")?
                    {
                        found.push(process);
                    }
                }
                Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => (), // Already exited.
                Err(e) => {
                    return Err(e).context(
                        "Cannot check the running application; close it manually and retry",
                    )
                }
            }
        }
        let next = unsafe { Process32NextW(snapshot.0, &mut entry) };
        if let Err(e) = &next {
            ensure!(
                e.code() == ERROR_NO_MORE_FILES.to_hresult(),
                "Cannot enumerate running applications: {e}"
            );
        }
        more = next.is_ok();
    }
    Ok(found)
}
pub fn is_running(root: &Path) -> Result<bool> {
    Ok(!processes(root)?.is_empty())
}
pub fn close(root: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    crate::operations::manifest(root)?;
    let processes = processes(root)?;
    ensure!(
        processes.len() <= 64,
        "Too many application processes to verify safe exit"
    );
    let message = unsafe {
        RegisterWindowMessageW(PCWSTR(
            crate::platform::wide("MAXISOUNDSET.RequestSafeExit.v1").as_ptr(),
        ))
    };
    ensure!(message != 0, "Cannot register safe-exit request");
    struct Request {
        handle: HANDLE,
        message: u32,
    }
    unsafe extern "system" fn request(hwnd: HWND, data: LPARAM) -> BOOL {
        let request = unsafe { &*(data.0 as *const Request) };
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        // Retain the process handle and verify its identity before posting.
        if pid == unsafe { GetProcessId(request.handle) } {
            let state = unsafe { WaitForSingleObject(request.handle, 0) };
            if state != windows::Win32::Foundation::WAIT_TIMEOUT
                && state != windows::Win32::Foundation::WAIT_OBJECT_0
            {
                return false.into(); // EnumWindows fails; do not authorize deletion.
            }
            if state == windows::Win32::Foundation::WAIT_OBJECT_0 {
                return true.into();
            }
            unsafe {
                let _ = PostMessageW(Some(hwnd), request.message, WPARAM(0), LPARAM(0));
            }
        }
        true.into()
    }
    // Repost while a newly launched process creates its window, but wake immediately
    // when all held process handles signal. Never kill a slow restoration worker.
    let handles: Vec<HANDLE> = processes.iter().map(|process| process.0).collect();
    loop {
        let mut waiting = false;
        for process in &processes {
            let state = unsafe { WaitForSingleObject(process.0, 0) };
            ensure!(
                state == windows::Win32::Foundation::WAIT_TIMEOUT
                    || state == windows::Win32::Foundation::WAIT_OBJECT_0,
                "Cannot verify application exit: {}",
                windows::core::Error::from_win32()
            );
            if state == windows::Win32::Foundation::WAIT_TIMEOUT {
                waiting = true;
                let request_data = Request {
                    handle: process.0,
                    message,
                };
                unsafe {
                    EnumWindows(
                        Some(request),
                        LPARAM((&request_data as *const Request) as isize),
                    )?;
                }
            }
        }
        if !waiting {
            break;
        }
        ensure!(Instant::now() < deadline, "The application has not exited. Open it and use Exit app, then retry. Older versions may require manual exit.");
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining.as_millis().min(25) as u32;
        let state = unsafe { WaitForMultipleObjects(&handles, true, timeout) };
        ensure!(
            state == windows::Win32::Foundation::WAIT_TIMEOUT
                || state == windows::Win32::Foundation::WAIT_OBJECT_0,
            "Cannot wait for application exit: {}",
            windows::core::Error::from_win32()
        );
    }
    ensure!(
        !is_running(root)?,
        "The application was reopened. Close it before removing files."
    );
    ensure!(Instant::now() <= deadline, "Safe exit did not complete within one second. Files were kept; let audio restore, then retry.");
    Ok(())
}
