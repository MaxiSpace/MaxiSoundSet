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
                    if unsafe { WaitForSingleObject(process.0, 0) }
                        == windows::Win32::Foundation::WAIT_TIMEOUT
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
    crate::operations::manifest(root)?;
    let processes = processes(root)?;
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
        if pid == unsafe { GetProcessId(request.handle) }
            && unsafe { WaitForSingleObject(request.handle, 0) }
                == windows::Win32::Foundation::WAIT_TIMEOUT
        {
            unsafe {
                let _ = PostMessageW(Some(hwnd), request.message, WPARAM(0), LPARAM(0));
            }
        }
        true.into()
    }
    // A freshly launched process may still be completing its first device scan while the
    // installer requests shutdown. Give the cooperative path time to restore audio and exit.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut waiting = false;
        for process in &processes {
            if unsafe { WaitForSingleObject(process.0, 0) }
                == windows::Win32::Foundation::WAIT_TIMEOUT
            {
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
        std::thread::sleep(Duration::from_millis(100));
    }
    ensure!(
        !is_running(root)?,
        "The application was reopened. Close it before removing files."
    );
    Ok(())
}
