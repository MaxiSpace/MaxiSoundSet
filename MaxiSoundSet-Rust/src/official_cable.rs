//! Official VB-CABLE package acquisition. No undocumented setup arguments.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{fs, io::{Read, Write}, path::{Path, PathBuf}, time::Duration};
use windows::{core::PCWSTR, Win32::{Foundation::{CloseHandle, HANDLE}, System::Threading::{GetExitCodeProcess, WaitForSingleObject}, UI::{Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW}, WindowsAndMessaging::SW_SHOWNORMAL}}};
fn wide(value: &str) -> Vec<u16> {value.encode_utf16().chain(Some(0)).collect()}
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
struct PackageStage(PathBuf);
impl Drop for PackageStage { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

// The archive acts as a manifest: extracted files must match every safe,
// bounded entry. Recheck executable trust each time, including legacy caches.
fn validate_cached(folder: &Path, verify: impl Fn(&Path) -> Result<()>) -> Result<(PathBuf, String)> {
    let archive_path = folder.join("official-package.zip");
    regular_path(folder)?;
    regular_path(&archive_path)?;
    anyhow::ensure!(fs::metadata(&archive_path)?.len() <= 30_000_000, "Cached package exceeds limit");
    let bytes = fs::read(&archive_path)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    anyhow::ensure!(archive.len() <= 100, "Too many cached entries");
    let mut expanded = 0u64;
    let mut has_installer = false;
    let mut expected_files = vec![archive_path.clone()];
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.enclosed_name().context("Unsafe cached path")?;
        expanded = expanded.checked_add(entry.size()).context("Cached size overflow")?;
        anyhow::ensure!(expanded <= 100_000_000, "Cached package is too large");
        if entry.is_dir() { continue; }
        has_installer |= name == Path::new("VBCABLE_Setup_x64.exe");
        let path = folder.join(name);
        expected_files.push(path.clone());
        regular_path(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == entry.size(), "Incomplete cached package");
        let mut expected = Vec::new();
        entry.read_to_end(&mut expected)?;
        anyhow::ensure!(fs::read(&path)? == expected, "Cached file differs from official archive");
    }
    anyhow::ensure!(has_installer, "Cached x64 installer missing from archive");
    check_cache_tree(folder, &expected_files)?;
    let installer = folder.join("VBCABLE_Setup_x64.exe");
    verify(&installer)?;
    Ok((installer, hash))
}
fn regular_path(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    anyhow::ensure!(metadata.file_attributes() & 0x400 == 0 && !metadata.file_type().is_symlink(), "Package contains a linked path");
    Ok(())
}
fn check_cache_tree(folder: &Path, expected: &[PathBuf]) -> Result<()> {
    regular_path(folder)?;
    for entry in fs::read_dir(folder)? {
        let path = entry?.path();
        regular_path(&path)?;
        if path.is_dir() { check_cache_tree(&path, expected)?; }
        else {
            // Official setup may leave diagnostic logs next to its executable.
            // Executable payload files still must match the verified archive.
            let setup_log = path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("log"));
            anyhow::ensure!(expected.contains(&path) || setup_log, "Unexpected file in cached package");
        }
    }
    Ok(())
}
pub struct Acquisition { pub pack: u32, pub installer: PathBuf, pub hash: String, pub reused: bool }
pub fn cached_package(data: &Path) -> Option<Acquisition> {
    let mut packs: Vec<u32> = fs::read_dir(data.join("Downloads")).ok()?.filter_map(|entry| {
        let name = entry.ok()?.file_name().to_str()?.to_owned();
        name.strip_prefix("VB-CABLE-Pack")?.parse().ok()
    }).collect();
    packs.sort_unstable_by(|a,b| b.cmp(a));
    for pack in packs {
        let folder = data.join("Downloads").join(format!("VB-CABLE-Pack{pack}"));
        match validate_cached(&folder, verify_signature) {
            Ok((installer, hash)) => return Some(Acquisition {pack, installer, hash, reused: true}),
            Err(error) => log::warn!("Ignoring invalid cached VB-CABLE Pack{pack}: {error:#}"),
        }
    }
    None
}
pub fn acquire_package(data: &Path, force: bool, progress: impl FnMut(f32)) -> Result<Acquisition> {
    acquire_with(force, || cached_package(data), || {
        let pack = latest_pack()?;
        let (installer, hash) = download_fresh(data, pack, progress)?;
        Ok(Acquisition {pack, installer, hash, reused: false})
    })
}
fn acquire_with(force: bool, cached: impl FnOnce() -> Option<Acquisition>, fresh: impl FnOnce() -> Result<Acquisition>) -> Result<Acquisition> {
    if !force { if let Some(package) = cached() { return Ok(package); } }
    fresh()
}
pub fn download_package(data: &Path, pack: u32, progress: impl FnMut(f32)) -> Result<(PathBuf, String)> {
    let folder = data.join("Downloads").join(format!("VB-CABLE-Pack{pack}"));
    if let Ok(cached) = validate_cached(&folder, verify_signature) { return Ok(cached); }
    download_fresh(data, pack, progress)
}
fn download_fresh(data: &Path, pack: u32, mut progress: impl FnMut(f32)) -> Result<(PathBuf, String)> {
    let downloads = data.join("Downloads");
    fs::create_dir_all(&downloads)?;
    let destination = downloads.join(format!("VB-CABLE-Pack{pack}"));
    let folder = downloads.join(format!(".VB-CABLE-Pack{pack}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()));
    fs::create_dir(&folder)?;
    let _cleanup = PackageStage(folder.clone());
    let url = format!("https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack{pack}.zip");

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
        progress((bytes.len() as f32 / size as f32).min(1.0));
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
        expanded = expanded.checked_add(entry.size()).context("Expanded package size overflow")?;
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
    validate_cached(&folder, verify_signature)?;
    let backup = folder.with_extension("previous");
    if destination.exists() { fs::rename(&destination, &backup).context("Preserving previous cable package")?; }
    if let Err(error) = fs::rename(&folder, &destination) {
        if backup.exists() { fs::rename(&backup, &destination).context("Restoring previous cable package")?; }
        return Err(error).context("Publishing verified cable package");
    }
    if backup.exists() { let _ = fs::remove_dir_all(&backup); }
    Ok((destination.join("VBCABLE_Setup_x64.exe"), hash))
}
struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
pub fn verify_signature(path: &Path) -> Result<()> {
    use windows::Win32::{Foundation::HWND, Security::WinTrust::*};
    let name = wide(&path.to_string_lossy());
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchResult { Exit(u32), Cancelled, Failed, StillRunning }
// Retain a timed-out setup handle without terminating it. A subsequent action
// in this helper must observe its completion before starting another setup.
static PENDING: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);
fn pending_active(slot: &mut Option<usize>) -> bool {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    if let Some(raw) = *slot {
        if raw == 0 { return true; } // Successful launch without a trackable handle.
        let handle = HANDLE(raw as *mut std::ffi::c_void);
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 { return true; }
        unsafe { let _ = CloseHandle(handle); }
        *slot = None;
    }
    false
}
fn official_process_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("VBCABLE_Setup_x64.exe") || name.eq_ignore_ascii_case("VBCABLE_Setup.exe")
}
fn official_process_present() -> Result<bool> {
    use windows::Win32::{Foundation::ERROR_NO_MORE_FILES, System::Diagnostics::ToolHelp::*};
    let snapshot = Process(unsafe {CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0)?});
    let mut entry = PROCESSENTRY32W {dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,..Default::default()};
    let mut next = unsafe {Process32FirstW(snapshot.0,&mut entry)};
    loop {
        match next {
            Ok(()) => {
                let len = entry.szExeFile.iter().position(|c|*c==0).unwrap_or(entry.szExeFile.len());
                // Exact setup executable names are a conservative duplicate-launch
                // guard, not a publisher or driver-identity claim.
                if official_process_name(&String::from_utf16_lossy(&entry.szExeFile[..len])) {return Ok(true);}
                next = unsafe {Process32NextW(snapshot.0,&mut entry)};
            }
            Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
}
struct SetupLease(HANDLE);
impl Drop for SetupLease {fn drop(&mut self) {unsafe {let _=windows::Win32::System::Threading::ReleaseMutex(self.0);let _=CloseHandle(self.0);}}}
fn setup_lease() -> Result<Option<SetupLease>> {
    setup_lease_named("Local\\MAXI-VBCABLE-OfficialSetup-v1")
}
fn setup_lease_named(name: &str) -> Result<Option<SetupLease>> {
    use windows::Win32::{Foundation::{WAIT_OBJECT_0,WAIT_ABANDONED}, System::Threading::CreateMutexW};
    let name=wide(name);
    let handle=unsafe {CreateMutexW(None,false,PCWSTR(name.as_ptr()))?};
    let wait=unsafe {WaitForSingleObject(handle,0)};
    if wait==WAIT_OBJECT_0 || wait==WAIT_ABANDONED {Ok(Some(SetupLease(handle)))}
    else {unsafe {let _=CloseHandle(handle);} Ok(None)}
}
pub fn setup_still_running() -> bool {
    // Match launcher lock ordering. The lease covers another helper's UAC
    // window even before its setup process becomes visible to enumeration.
    let Ok(mut pending)=PENDING.try_lock() else {return true};
    if pending_active(&mut pending) {return true;}
    let Ok(Some(_probe))=setup_lease() else {return true};
    official_process_present().unwrap_or(true)
}
#[derive(Debug,PartialEq,Eq)]
enum WaitDecision {Complete,Continue,Uncertain}
fn wait_decision(value: u32, expired: bool) -> WaitDecision {
    use windows::Win32::Foundation::{WAIT_OBJECT_0,WAIT_TIMEOUT};
    if value == WAIT_OBJECT_0.0 {WaitDecision::Complete}
    else if value == WAIT_TIMEOUT.0 && !expired {WaitDecision::Continue}
    else {WaitDecision::Uncertain}
}
fn launch_failure(code: i32) -> LaunchResult {
    if code as u32 == 0x800704c7 {LaunchResult::Cancelled} else {LaunchResult::Failed}
}
struct ShellCom;
impl Drop for ShellCom {fn drop(&mut self) {unsafe {windows::Win32::System::Com::CoUninitialize();}}}
pub fn launch_official(path: &Path) -> LaunchResult {

    let Ok(mut pending) = PENDING.try_lock() else {return LaunchResult::StillRunning};
    if pending_active(&mut pending) {return LaunchResult::StillRunning;}
    let Ok(Some(_lease)) = setup_lease() else {return LaunchResult::StillRunning;};
    if official_process_present().unwrap_or(true) {return LaunchResult::StillRunning;}
    use windows::Win32::System::Com::{CoInitializeEx,COINIT_APARTMENTTHREADED,COINIT_DISABLE_OLE1DDE};
    if unsafe {CoInitializeEx(None,COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()}.is_err() {return LaunchResult::Failed;}
    let _com=ShellCom;
    // Validate the extracted official executable again immediately before runas.
    if verify_signature(path).is_err() {return LaunchResult::Failed;}
    let file = wide(&path.to_string_lossy());
    let verb = wide("runas");
    let Some(parent) = path.parent() else {return LaunchResult::Failed};
    let cwd = wide(&parent.to_string_lossy());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()), lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR::null(), lpDirectory: PCWSTR(cwd.as_ptr()),
        nShow: SW_SHOWNORMAL.0, ..Default::default()
    };
    unsafe {
        if let Err(error) = ShellExecuteExW(&mut info) {
            return launch_failure(error.code().0);
        }
        if info.hProcess.is_invalid() { *pending = Some(0); return LaunchResult::StillRunning; }
        let process = Process(info.hProcess);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
        loop {
            use windows::Win32::UI::WindowsAndMessaging::{MSG,PeekMessageW,PM_REMOVE,TranslateMessage,DispatchMessageW};
            let mut message=MSG::default();
            while PeekMessageW(&mut message,None,0,0,PM_REMOVE).as_bool() {let _=TranslateMessage(&message);DispatchMessageW(&message);}
            let wait = WaitForSingleObject(process.0, 500);
            match wait_decision(wait.0, std::time::Instant::now() >= deadline) {
                WaitDecision::Complete => break,
                WaitDecision::Continue => continue,
                WaitDecision::Uncertain => {
                *pending = Some(process.0.0 as usize);
                std::mem::forget(process); // Setup may still need the complete package.
                return LaunchResult::StillRunning;
                }
            }
        }
        let mut code = 0;
        if GetExitCodeProcess(process.0, &mut code).is_err() { return LaunchResult::Failed; }
        log::info!("VB-CABLE official setup exit code: {code}");
        LaunchResult::Exit(code)
    }
}
pub fn launch_installer(path: &Path) -> Result<u32> {
    match launch_official(path) {
        LaunchResult::Exit(code) if code == 0 || code == 3010 => Ok(code),
        other => anyhow::bail!("Official VB-CABLE setup did not complete successfully: {other:?}"),
    }
}

#[cfg(test)]
mod un04_tests {
    use super::*;
    #[test] fn untracked_success_blocks_subsequent_actions_without_touching_handle() {
        let mut pending=Some(0); assert!(pending_active(&mut pending)); assert_eq!(pending,Some(0));
        assert!(!pending_active(&mut None));
    }
    #[test] fn duplicate_guard_uses_exact_names_only() {
        for name in ["VBCABLE_Setup_x64.exe","vbcable_setup.EXE"] {assert!(official_process_name(name));}
        for name in ["VoicemeeterSetup.exe","setup.exe","FakeVBCABLE_Setup_x64.exe","VBCABLE_Setup_x64.exe.other"] {assert!(!official_process_name(name));}
    }
}
#[cfg(test)]
mod un04_wait_tests {
    use super::*;
    #[test] fn wait_failure_timeout_and_unknown_are_uncertain_not_success() {
        assert_eq!(wait_decision(0,false),WaitDecision::Complete);
        assert_eq!(wait_decision(0x102,false),WaitDecision::Continue);
        for value in [0x102,0xffffffff,0x80,7] {assert_eq!(wait_decision(value,true),WaitDecision::Uncertain);}
        assert_eq!(wait_decision(0xffffffff,false),WaitDecision::Uncertain);
    }
    #[test] fn win32_cancel_is_distinct_from_launch_errors() {
        assert_eq!(launch_failure(0x800704c7u32 as i32),LaunchResult::Cancelled);
        for code in [0x80070005u32 as i32,0x80070002u32 as i32,1223] {assert_eq!(launch_failure(code),LaunchResult::Failed);}
    }
}

#[cfg(test)]
mod un04_lease_tests {
    use super::*;
    #[test] fn unique_test_lease_blocks_cross_worker_then_becomes_free() {
        let name=format!("Local\\MAXI-VBCABLE-TEST-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let held=setup_lease_named(&name).unwrap().expect("test lease available");
        let busy_name=name.clone();
        assert!(std::thread::spawn(move || setup_lease_named(&busy_name).unwrap().is_none()).join().unwrap());
        drop(held);
        assert!(std::thread::spawn(move || setup_lease_named(&name).unwrap().is_some()).join().unwrap());
    }
}

#[cfg(test)]
mod package_cache_tests {
    use super::*;
    fn cached() -> Acquisition { Acquisition { pack: 45, installer: "synthetic.exe".into(), hash: "hash".into(), reused: true } }
    #[test]
    fn cached_retry_needs_no_network_and_force_never_falls_back() {
        assert!(acquire_with(false, || Some(cached()), || panic!("network must not run")).unwrap().reused);
        assert!(acquire_with(true, || panic!("force must not inspect cache"), || anyhow::bail!("offline")).is_err());
        assert!(acquire_with(false, || None, || anyhow::bail!("offline")).is_err());
    }
    #[test]
    fn package_validation_rejects_missing_modified_extra_and_untrusted_files() {
        let folder = std::env::temp_dir().join(format!("maxi-cache-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir(&folder).unwrap();
        let _cleanup = PackageStage(folder.clone());
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("VBCABLE_Setup_x64.exe", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"synthetic signed content").unwrap();
        zip.start_file("driver.inf", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"driver").unwrap();
        fs::write(folder.join("official-package.zip"), zip.finish().unwrap().into_inner()).unwrap();
        fs::write(folder.join("VBCABLE_Setup_x64.exe"), b"synthetic signed content").unwrap();
        assert!(validate_cached(&folder, |_| Ok(())).is_err());
        fs::write(folder.join("driver.inf"), b"driver").unwrap();
        assert!(validate_cached(&folder, |_| Ok(())).is_ok());
        assert!(validate_cached(&folder, |_| anyhow::bail!("untrusted")).is_err());
        fs::write(folder.join("driver.inf"), b"edited").unwrap();
        assert!(validate_cached(&folder, |_| Ok(())).is_err());
        fs::write(folder.join("driver.inf"), b"driver").unwrap();
        fs::write(folder.join("setup.log"), b"official setup diagnostic output").unwrap();
        assert!(validate_cached(&folder, |_| Ok(())).is_ok());
        fs::write(folder.join("unexpected.dll"), b"extra").unwrap();
        assert!(validate_cached(&folder, |_| Ok(())).is_err());
        fs::write(folder.join("official-package.zip"), b"partial zip").unwrap();
        assert!(validate_cached(&folder, |_| Ok(())).is_err());
    }
}
