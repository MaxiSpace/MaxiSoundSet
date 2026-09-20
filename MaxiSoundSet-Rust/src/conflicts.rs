//! Detect running user-space audio processors. Drivers/services cannot be inferred
//! reliably from their companion UI, so this is a possible-interference warning.
use anyhow::{ensure, Context, Result};
use std::path::Path;
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE},
        System::{Diagnostics::ToolHelp::*, Threading::*},
    },
};

#[derive(Clone, PartialEq, Eq)]
pub struct AudioApp {
    pub pid: u32,
    pub name: String,
    pub exe: String,
    path: String,
    created: u64,
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn known(exe: &str) -> Option<&'static str> {
    match exe.to_ascii_lowercase().as_str() {
        "fxsound.exe" | "fxsoundpro.exe" => Some("FxSound"),
        "voicemeeter.exe"
        | "voicemeeterpro.exe"
        | "voicemeeter8.exe"
        | "voicemeeterpro64.exe"
        | "voicemeeter8x64.exe"
        | "voicemeeter64.exe" => Some("Voicemeeter"),
        "peace.exe" | "peace64.exe" => Some("Peace / Equalizer APO"),
        "steelseries-sonar.exe" => Some("SteelSeries Sonar"),
        "nahimic3.exe" | "nahimic.exe" => Some("Nahimic"),
        "razersurround.exe" | "razer surround.exe" => Some("Razer Surround"),
        "dolbydax2desktopui.exe" | "dolbydax3desktopui.exe" => Some("Dolby Audio"),
        "dtsu2papp.exe" => Some("DTS Audio"),
        _ => None,
    }
}
fn identity(handle: HANDLE) -> Result<(String, u64)> {
    let mut path = vec![0u16; 32768];
    let mut size = path.len() as u32;
    let mut created = FILETIME::default();
    let mut end = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut size,
        )?;
        GetProcessTimes(handle, &mut created, &mut end, &mut kernel, &mut user)?;
    }
    Ok((
        String::from_utf16(&path[..size as usize])?,
        ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
    ))
}
pub fn scan() -> Result<Vec<AudioApp>> {
    let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut apps = vec![];
    unsafe {
        Process32FirstW(snapshot.0, &mut entry)?;
    }
    loop {
        let exe = String::from_utf16_lossy(
            &entry.szExeFile[..entry
                .szExeFile
                .iter()
                .position(|u| *u == 0)
                .unwrap_or(entry.szExeFile.len())],
        );
        if entry.th32ProcessID != std::process::id() {
            if let Some(name) = known(&exe) {
                if let Ok(handle) = unsafe {
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION,
                        false,
                        entry.th32ProcessID,
                    )
                }
                .map(Handle)
                {
                    if let Ok((path, created)) = identity(handle.0) {
                        apps.push(AudioApp {
                            pid: entry.th32ProcessID,
                            name: name.into(),
                            exe,
                            path,
                            created,
                        });
                    }
                }
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    Ok(apps)
}
/// Called only after the user confirms ending this exact listed app.
pub fn end(app: &AudioApp) -> Result<()> {
    ensure!(
        app.pid > 4 && app.pid != std::process::id() && known(&app.exe).is_some(),
        "Process is not an eligible audio application"
    );
    let handle = Handle(
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                false,
                app.pid,
            )
        }
        .context("App already exited, or Windows denied access")?,
    );
    let (path, created) = identity(handle.0)?;
    ensure!(
        created == app.created && path.eq_ignore_ascii_case(&app.path),
        "Application changed since it was listed; refresh first"
    );
    ensure!(
        Path::new(&path)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&app.exe)),
        "Executable identity changed"
    );
    unsafe {
        TerminateProcess(handle.0, 0)?;
    }
    log::info!("User ended audio app {} (PID {})", app.name, app.pid);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Child-process fixture; launched only by the termination integration test"]
    fn idle_fixture() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
    #[test]
    fn end_selected_processor_terminates_only_the_owned_test_fixture() {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        struct Fixture {
            child: std::process::Child,
            exe: std::path::PathBuf,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_file(&self.exe);
                let _ = std::fs::remove_dir(self.exe.parent().unwrap());
            }
        }
        let directory = std::env::temp_dir().join(format!(
            "maxi-conflict-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let exe = directory.join("FxSound.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let child = Command::new(&exe)
            .args(["--exact", "conflicts::tests::idle_fixture", "--ignored"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        let mut fixture = Fixture { child, exe };
        let pid = fixture.child.id();
        let app = scan()
            .unwrap()
            .into_iter()
            .find(|a| a.pid == pid)
            .expect("Owned fixture was not detected");
        let mut changed = app.clone();
        changed.created ^= 1;
        assert!(
            end(&changed).is_err(),
            "A changed process identity was accepted"
        );
        assert!(
            fixture.child.try_wait().unwrap().is_none(),
            "Identity check terminated the fixture"
        );
        end(&app).unwrap();
        assert!(
            fixture.child.wait().unwrap().success(),
            "Selected fixture did not terminate"
        );
    }
    #[test]
    fn recognize_processors_without_flagging_players_or_system_audio() {
        assert_eq!(known("FxSound.exe"), Some("FxSound"));
        assert!(known("VOICEMEETER8X64.EXE").is_some());
        for exe in [
            "audiodg.exe",
            "svchost.exe",
            "MaxiSoundSet.exe",
            "chrome.exe",
            "Spotify.exe",
        ] {
            assert!(known(exe).is_none());
        }
    }
}
