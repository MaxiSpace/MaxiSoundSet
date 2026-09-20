use crate::{core_audio, settings};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT},
        System::Threading::{GetExitCodeProcess, WaitForSingleObject},
        UI::{
            Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
};

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PackageState {
    pub latest: u32,
    pub confirmed_pack: u32,
    pub sha256: String,
}
pub enum Event {
    Metadata(PackageState),
    Progress(f32),
    Message(String),
    Done(bool),
    Error(String),
}
pub struct Manager {
    rx: Receiver<Event>,
    tx: Sender<Event>,
    pub busy: bool,
    pub state: PackageState,
}
impl Manager {
    pub fn new(data: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = fs::read(data.join("components.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            rx,
            tx,
            busy: false,
            state,
        }
    }
    pub fn start(&mut self, data: PathBuf, install: bool) {
        if self.busy {
            return;
        }
        self.busy = true;
        let tx = self.tx.clone();
        let previous = self.state.clone();
        thread::spawn(move || {
            let result = (|| -> Result<bool> {
                let mut state = previous;
                state.latest = latest_pack()?;
                let _ = tx.send(Event::Metadata(state.clone()));
                log::info!("Official VB-CABLE latest package: {}", state.latest);
                if !install {
                    settings::write_json(&data.join("components.json"), &state)?;
                    return installed();
                }
                let installed_before = installed()?;
                let (installer, hash) = download_package(&data, state.latest, &tx)?;
                state.sha256 = hash;
                let _ = tx.send(Event::Message("Windows will request administrator access for the official VB-CABLE installer. Installing a new driver may require a restart.".into()));
                log::info!(
                    "Launching official VB-CABLE installer: {}",
                    installer.display()
                );
                launch_installer(&installer)?;
                let mut detected = false;
                for _ in 0..15 {
                    detected = installed()?;
                    if detected {
                        break;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
                // A detected endpoint alone cannot prove that an existing driver was updated.
                if detected && !installed_before {
                    state.confirmed_pack = state.latest;
                }
                settings::write_json(&data.join("components.json"), &state)?;
                let _ = tx.send(Event::Metadata(state));
                let _ = tx.send(Event::Message(if detected { "VB-CABLE is available. Select Full boost and your playback device." } else { "Installer finished; VB-CABLE is not available yet. Restart Windows, then refresh devices. Check Log if it remains absent." }.into()));
                Ok(detected)
            })();
            match result {
                Ok(found) => {
                    let _ = tx.send(Event::Done(found));
                }
                Err(e) => {
                    log::error!("Audio components: {e:#}");
                    let _ = tx.send(Event::Error(format!("{e:#}")));
                }
            }
        });
    }
    pub fn poll(&mut self) -> Vec<Event> {
        let events: Vec<_> = self.rx.try_iter().collect();
        for e in &events {
            match e {
                Event::Metadata(s) => self.state = s.clone(),
                Event::Done(_) | Event::Error(_) => self.busy = false,
                _ => (),
            }
        }
        events
    }
}
pub fn installed() -> Result<bool> {
    Ok(core_audio::devices()?
        .iter()
        .any(|d| d.name.to_ascii_lowercase().contains("cable input")))
}
pub fn latest_pack() -> Result<u32> {
    let client = client()?;
    let html = client
        .get("https://vb-audio.com/Cable/")
        .send()?
        .error_for_status()?
        .take(2_000_000)
        .bytes()
        .collect::<std::io::Result<Vec<_>>>()?;
    let html = String::from_utf8_lossy(&html);
    let re = regex::Regex::new(
        r"https://download\.vb-audio\.com/Download_CABLE/VBCABLE_Driver_Pack(\d+)\.zip",
    )?;
    re.captures_iter(&html)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .max()
        .context("Official download link was not found. Open the VB-Audio website from Components.")
}
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("MaxiSoundSet/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.url().scheme() == "https"
                && a.url()
                    .host_str()
                    .is_some_and(|h| h == "vb-audio.com" || h == "download.vb-audio.com")
                && a.previous().len() < 5
            {
                a.follow()
            } else {
                a.stop()
            }
        }))
        .build()?)
}
fn download_package(data: &Path, pack: u32, tx: &Sender<Event>) -> Result<(PathBuf, String)> {
    let folder = data.join("Downloads").join(format!("VB-CABLE-Pack{pack}"));
    fs::create_dir_all(&folder)?;
    let url = format!("https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack{pack}.zip");
    let _ = tx.send(Event::Message(
        "Downloading the official VB-CABLE package over verified HTTPS…".into(),
    ));
    let mut response = client()?.get(&url).send()?.error_for_status()?;
    let size = response.content_length().unwrap_or(1_500_000);
    anyhow::ensure!(size <= 30_000_000, "Package exceeds download limit");
    let mut bytes = Vec::new();
    let mut block = [0u8; 32768];
    loop {
        let n = response.read(&mut block)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&block[..n]);
        anyhow::ensure!(bytes.len() <= 30_000_000, "Package exceeds download limit");
        let _ = tx.send(Event::Progress((bytes.len() as f32 / size as f32).min(1.0)));
    }
    let hash = format!("{:x}", Sha256::digest(&bytes));
    log::info!(
        "Downloaded VB-CABLE Pack{pack}: {} bytes, SHA256={hash}",
        bytes.len()
    );
    fs::write(folder.join("official-package.zip"), &bytes)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    anyhow::ensure!(archive.len() <= 100, "Too many package entries");
    let mut expanded = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry
            .enclosed_name()
            .context("Unsafe path in downloaded package")?;
        expanded += entry.size();
        anyhow::ensure!(expanded <= 100_000_000, "Expanded package is too large");
        let path = folder.join(name);
        if entry.is_dir() {
            fs::create_dir_all(path)?;
        } else {
            if let Some(p) = path.parent() {
                fs::create_dir_all(p)?;
            }
            let mut f = fs::File::create(path)?;
            std::io::copy(&mut entry, &mut f)?;
            f.flush()?;
        }
    }
    let installer = folder.join("VBCABLE_Setup_x64.exe");
    anyhow::ensure!(installer.is_file(), "Official x64 installer is missing");
    verify_signature(&installer)?;
    log::info!("Official installer Authenticode signature verified");
    Ok((installer, hash))
}
struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn verify_signature(path: &Path) -> Result<()> {
    use windows::Win32::{Foundation::HWND, Security::WinTrust::*};
    let name = core_audio::wide(&path.to_string_lossy());
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        dwStateAction: WTD_STATEACTION_VERIFY,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            HWND(std::ptr::null_mut()),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            HWND(std::ptr::null_mut()),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
    }
    anyhow::ensure!(
        status == 0,
        "Installer signature validation failed (0x{:08X}). Open the official website.",
        status as u32
    );
    Ok(())
}
fn launch_installer(path: &Path) -> Result<()> {
    let file = core_audio::wide(&path.to_string_lossy());
    let verb = core_audio::wide("runas");
    let cwd = core_audio::wide(&path.parent().unwrap().to_string_lossy());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR::null(),
        lpDirectory: PCWSTR(cwd.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).context("Installer was cancelled or could not start")?;
        anyhow::ensure!(
            !info.hProcess.is_invalid(),
            "Installer process handle unavailable"
        );
        let process = Process(info.hProcess);
        while WaitForSingleObject(process.0, 500) == WAIT_TIMEOUT {}
        let mut code = 0;
        GetExitCodeProcess(process.0, &mut code)?;
        log::info!("VB-CABLE installer exit code: {code}");
        anyhow::ensure!(code == 0 || code == 3010, "VB-CABLE installer returned {code}. Restart Windows or open the official installer manually.");
    }
    Ok(())
}
pub fn check_download(data: &Path) -> Result<String> {
    let pack = latest_pack()?;
    let (tx, _) = mpsc::channel();
    let (installer, hash) = download_package(data, pack, &tx)?;
    Ok(format!(
        "Official Pack{pack}; installer={}; SHA256={hash}; elevated installer was NOT launched",
        installer.display()
    ))
}
