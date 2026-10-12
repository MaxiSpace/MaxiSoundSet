//! Explicit, verified installer handoff and the bounded startup acknowledgement.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io::{Read, Write}, path::{Path, PathBuf}, process::Command, time::{Duration, Instant}};

pub const OWNED_FILES: [&str; 7] = ["MaxiSoundSet.exe", "Uninstall.exe", "EULA_FA.md", "EULA_EN.md", "LICENSE.txt", "THIRD-PARTY-NOTICES.md", "ThirdParty-Licenses.txt"];
pub const MARKER: &str = ".maxi-install.json";
pub const IDENTITY: &str = "MAXI-SOUNDSET-INSTALL-v1";
pub const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\MAXISOUNDSET";
const JSON_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug)]
pub struct VerifiedInstaller { pub path: PathBuf, pub version: String, pub size: u64, pub sha256: String }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UpdateStatus { Updated, RolledBack, RecoveryRequired, Failed }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateResult { pub status: UpdateStatus, pub version: String, pub message: String, pub backup_path: Option<PathBuf> }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health { pub token: String, pub pid: u32, pub executable: PathBuf, pub version: String }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEnvelope { pub token: String, pub result: UpdateResult }
#[derive(Clone, Deserialize)]
pub struct OwnedManifest { pub identity: String, pub root: PathBuf, pub version: String, pub files: Vec<String>, pub sha256: String }

pub fn version(value: &str) -> Result<[u32; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    ensure!(parts.len() == 3 && parts.iter().all(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) && (s.len() == 1 || !s.starts_with('0'))), "Invalid stable version");
    Ok([parts[0].parse()?, parts[1].parse()?, parts[2].parse()?])
}
pub fn token() -> Result<String> {
    let guid = unsafe { windows::Win32::System::Com::CoCreateGuid()? };
    Ok(format!("{:08x}{:04x}{:04x}{}", guid.data1, guid.data2, guid.data3, guid.data4.iter().map(|v| format!("{v:02x}")).collect::<String>()))
}
pub fn valid_token(value: &str) -> bool { value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit()) }
pub fn no_links(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    for parent in path.ancestors() {
        match fs::symlink_metadata(parent) {
            Ok(m) => ensure!(!m.is_symlink() && m.file_attributes() & 0x400 == 0, "Linked paths are not accepted"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn canonical(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute() && !path.components().any(|p| matches!(p, std::path::Component::ParentDir | std::path::Component::CurDir)), "Absolute local path required");
    let spelling = path.to_string_lossy();
    ensure!(!spelling.starts_with("\\\\") || spelling.starts_with("\\\\?\\"), "Network paths are not accepted");
    no_links(path)?;
    let result = fs::canonicalize(path)?;
    Ok(PathBuf::from(result.to_string_lossy().trim_start_matches("\\\\?\\")))
}
pub fn same(a: &Path, b: &Path) -> bool {
    match (canonical(a), canonical(b)) { (Ok(a), Ok(b)) => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()), _ => false }
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    no_links(path)?;
    let file = fs::File::open(path)?;
    ensure!(file.metadata()?.is_file() && file.metadata()?.len() <= JSON_LIMIT, "Invalid or oversized update record");
    let mut bytes = Vec::new(); file.take(JSON_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= JSON_LIMIT, "Oversized update record");
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    no_links(path)?;
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() as u64 <= JSON_LIMIT, "Oversized update record");
    let temporary = path.with_extension("writing");
    let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    f.write_all(&bytes)?; f.sync_all()?; drop(f);
    fs::rename(&temporary, path).with_context(|| format!("Publishing update record {}",path.display()))?;
    Ok(())
}
pub fn verify_file(path: &Path, expected_size: u64, expected_sha256: &str) -> Result<()> {
    ensure!(expected_size > 0 && expected_sha256.len() == 64 && expected_sha256.bytes().all(|c| c.is_ascii_hexdigit()), "Invalid installer digest or size");
    no_links(path)?;
    let mut f = fs::File::open(path)?;
    ensure!(f.metadata()?.is_file() && f.metadata()?.len() == expected_size, "Installer size changed");
    let mut hash = Sha256::new(); let mut bytes = [0u8; 65536]; let mut total = 0u64;
    loop { let count = f.read(&mut bytes)?; if count == 0 { break; } total += count as u64; ensure!(total <= expected_size, "Installer grew during verification"); hash.update(&bytes[..count]); }
    ensure!(total == expected_size && format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected_sha256), "Installer SHA-256 mismatch");
    Ok(())
}
pub fn validate_target(registry_root: &Path, current_exe: &Path) -> Result<(PathBuf, OwnedManifest)> {
    let root = canonical(registry_root)?;
    ensure!(root.components().count() >= 3 && same(current_exe, &root.join("MaxiSoundSet.exe")), "Running executable and registered installation differ");
    let manifest: OwnedManifest = read_json(&root.join(MARKER))?;
    ensure!(manifest.identity == IDENTITY && same(&manifest.root, &root) && manifest.files == OWNED_FILES.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "Invalid installation ownership record");
    version(&manifest.version)?;
    verify_file(&root.join("MaxiSoundSet.exe"), fs::metadata(root.join("MaxiSoundSet.exe"))?.len(), &manifest.sha256)?;
    for name in OWNED_FILES { no_links(&root.join(name))?; ensure!(root.join(name).is_file(), "Owned installation file is missing"); }
    Ok((root, manifest))
}
pub fn registered_root() -> Result<PathBuf> {
    use windows::{core::PCWSTR, Win32::System::Registry::*};
    use std::os::windows::ffi::OsStrExt;
    let key: Vec<u16> = std::ffi::OsStr::new(UNINSTALL_KEY).encode_wide().chain(Some(0)).collect();
    let name: Vec<u16> = "InstallLocation".encode_utf16().chain(Some(0)).collect();
    let mut bytes = vec![0u16; 32768]; let mut len = (bytes.len() * 2) as u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, Some(bytes.as_mut_ptr().cast()), Some(&mut len)).ok()?; }
    ensure!(len >= 2 && len as usize <= bytes.len() * 2 && len % 2 == 0, "Invalid registered installation path");
    bytes.truncate(len as usize / 2); ensure!(bytes.last() == Some(&0), "Unterminated installation path"); bytes.pop();
    ensure!(!bytes.contains(&0), "Invalid registered installation path");
    Ok(PathBuf::from(String::from_utf16(&bytes)?))
}
pub fn helper_directory(token: &str) -> Result<PathBuf> {
    ensure!(valid_token(token), "Invalid update token");
    Ok(canonical(&std::env::temp_dir())?.join(format!("MAXI-SOUNDSET-Update-{token}")))
}
/// Resolve missing destination suffixes against their real existing ancestor.
fn canonical_destination(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute() && !path.components().any(|p| matches!(p,std::path::Component::ParentDir | std::path::Component::CurDir)), "Absolute update destination required");
    no_links(path)?;
    let mut ancestor = path; let mut suffix = Vec::new();
    while !ancestor.try_exists()? {
        suffix.push(ancestor.file_name().context("Missing update destination name")?.to_os_string());
        ancestor = ancestor.parent().context("Missing existing update ancestor")?;
    }
    let mut resolved = canonical(ancestor)?;
    for component in suffix.iter().rev() { resolved.push(component); }
    Ok(resolved)
}
pub fn ensure_helper_outside_data(helper: &Path, installed_root: &Path) -> Result<()> {
    let root = canonical(installed_root)?;
    let data = canonical_destination(&root.join("Data"))?;
    let helper = canonical_destination(helper)?;
    let normalize = |path:&Path| path.to_string_lossy().replace('/',"\\").trim_end_matches('\\').to_lowercase();
    let data = normalize(&data); let helper = normalize(&helper);
    ensure!(helper != data && !helper.starts_with(&format!("{data}\\")), "Temporary update helper must be outside the installed Data directory");
    Ok(())
}
pub fn launch_verified(installer: &VerifiedInstaller, current_version: &str) -> Result<HealthProbe> {
    ensure!(version(&installer.version)? > version(current_version)?, "The update is not newer than this application");
    let current_exe = canonical(&std::env::current_exe()?)?;
    let registry = registered_root().context("UPD_INSTALL_PATH")?;
    let (root, manifest) = validate_target(&registry, &current_exe).context("UPD_INSTALL_PATH")?;
    ensure!(manifest.version == current_version, "UPD_INSTALL_PATH: registered installation version differs from the running application");
    ensure!(fs::metadata(&installer.path).context("UPD_SIZE")?.len() == installer.size, "UPD_SIZE: installer size changed");
    verify_file(&installer.path, installer.size, &installer.sha256).context("UPD_HASH")?;
    let token = token().context("UPD_HANDOFF")?; let dir = helper_directory(&token).context("UPD_HANDOFF")?;
    ensure_helper_outside_data(&dir,&root).context("UPD_HANDOFF")?;
    fs::create_dir(&dir).context("UPD_HANDOFF")?;
    let helper = dir.join("Setup.exe");
    let launch = (|| -> Result<HealthProbe> {
        let mut output = fs::OpenOptions::new().create_new(true).write(true).open(&helper)?;
        std::io::copy(&mut fs::File::open(&installer.path)?, &mut output)?; output.sync_all()?; drop(output);
        verify_file(&helper, installer.size, &installer.sha256)?;
        let mut child = Command::new(&helper).arg("--update-root").arg(&root).arg("--update-from-exe").arg(&current_exe).arg("--update-version").arg(&installer.version).arg("--update-token").arg(&token).arg("--update-size").arg(installer.size.to_string()).arg("--update-sha256").arg(&installer.sha256).spawn()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if dir.join("ready.json").exists() { let ready: Health = read_json(&dir.join("ready.json"))?; ensure!(ready.token == token && ready.pid == child.id() && same(&ready.executable, &helper) && ready.version == installer.version, "Invalid installer readiness acknowledgement"); write_json(&dir.join("accepted.json"), &ready)?; return Ok(HealthProbe { health: None, result: dir.join("result.json"), token: token.clone(), acknowledged: true, consumed: false, helper: HelperProcess::Child(child), stage: root.join(format!(".maxi-update-{token}")), version: current_version.into() }); }
            if let Some(status) = child.try_wait()? { bail!("Update helper rejected the handoff ({status})"); }
            ensure!(Instant::now() < deadline, "Update helper did not accept the handoff in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // Never remove a running helper or its records after launch uncertainty.
    if launch.is_err() && !dir.join("ready.json").exists() { let _ = fs::remove_file(&helper); let _ = fs::remove_dir(&dir); }
    launch.context("UPD_HANDOFF")
}

enum HelperProcess {
    Child(std::process::Child),
    Handle(std::os::windows::io::OwnedHandle),
    Unavailable(String),
}
impl HelperProcess {
    fn observe(pid: u32, executable: &Path) -> Self {
        use std::os::windows::io::{AsRawHandle,FromRawHandle,OwnedHandle};
        use windows::{core::PWSTR,Win32::{Foundation::HANDLE,System::Threading::*}};
        let observation = (|| -> Result<OwnedHandle> {
            let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,false,pid)? };
            let handle = unsafe { OwnedHandle::from_raw_handle(raw.0) };
            ensure!(unsafe { GetProcessId(HANDLE(handle.as_raw_handle())) } == pid, "Helper process identity changed");
            let mut name = vec![0u16;32768]; let mut length = name.len() as u32;
            unsafe { QueryFullProcessImageNameW(HANDLE(handle.as_raw_handle()),PROCESS_NAME_WIN32,PWSTR(name.as_mut_ptr()),&mut length)?; }
            let actual = PathBuf::from(String::from_utf16(&name[..length as usize])?);
            ensure!(same(&actual,executable), "Helper process executable identity changed");
            Ok(handle)
        })();
        match observation { Ok(handle) => Self::Handle(handle), Err(error) => Self::Unavailable(format!("Helper exited or could not be verified: {error:#}")) }
    }
    fn ended(&mut self) -> Result<Option<String>> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{Foundation::{HANDLE,WAIT_OBJECT_0,WAIT_TIMEOUT},System::Threading::WaitForSingleObject};
        match self {
            Self::Child(child) => Ok(child.try_wait()?.map(|status|format!("Update helper exited ({status}) before recording its final result"))),
            Self::Handle(handle) => {
                let state = unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()),0) };
                ensure!(state == WAIT_OBJECT_0 || state == WAIT_TIMEOUT, "Cannot observe update helper completion");
                Ok((state == WAIT_OBJECT_0).then(|| "Update helper exited before recording its final result".into()))
            },
            Self::Unavailable(message) => Ok(Some(message.clone())),
        }
    }
}
pub struct HealthProbe { health: Option<PathBuf>, result: PathBuf, token: String, acknowledged: bool, consumed: bool, helper: HelperProcess, stage: PathBuf, version: String }
impl HealthProbe {
    pub fn from_args(args: &[String], current_version: &str) -> Result<Option<Self>> {
        if !args.iter().skip(1).any(|a| a.starts_with("--update-")) { return Ok(None); }
        version(current_version)?;
        ensure!(args.len() == 5 || args.len() == 7, "Invalid update startup argument count");
        let mut health = None; let mut result = None; let mut token = None;
        for pair in args[1..].chunks_exact(2) { match pair[0].as_str() {
            "--update-health" if health.is_none() => health = Some(PathBuf::from(&pair[1])),
            "--update-result" if result.is_none() => result = Some(PathBuf::from(&pair[1])),
            "--update-token" if token.is_none() => token = Some(pair[1].clone()),
            _ => bail!("Unknown or repeated update startup argument"),
        } }
        let token = token.context("Missing update token")?; ensure!(valid_token(&token), "Invalid update token");
        let result = result.context("Missing update result path")?;
        ensure!(result == helper_directory(&token)?.join("result.json"), "Update result path is not owned by this handoff"); no_links(&result)?;
        let executable = canonical(&std::env::current_exe()?)?;
        let root = executable.parent().context("Missing executable parent")?;
        let helper_dir = result.parent().context("Missing update result directory")?;
        ensure_helper_outside_data(helper_dir,root)?;
        let ready: Health = read_json(&helper_dir.join("ready.json"))?;
        ensure!(ready.token == token && ready.pid > 0 && same(&ready.executable,&helper_dir.join("Setup.exe")), "Invalid helper readiness identity on application startup");
        version(&ready.version)?;
        let helper = HelperProcess::observe(ready.pid,&ready.executable);
        let stage = root.join(format!(".maxi-update-{token}"));
        if let Some(path) = &health {
            let exe = canonical(&std::env::current_exe()?)?; let root = exe.parent().context("Missing executable parent")?;
            validate_target(&registered_root()?, &exe)?;
            ensure!(*path == root.join(format!(".maxi-update-{token}")).join("health.json"), "Update health path is not in the owned stage");
            no_links(path)?; ensure!(path.parent().is_some_and(|p| p.is_dir()), "Update stage is missing");
        }
        Ok(Some(Self { health, result, token, acknowledged: false, consumed: false, helper, stage, version: current_version.into() }))
    }
    pub fn acknowledge(&mut self, current_version: &str) -> Result<()> {
        if self.acknowledged { return Ok(()); }
        version(current_version)?;
        if let Some(path) = &self.health { write_json(path, &Health { token: self.token.clone(), pid: std::process::id(), executable: canonical(&std::env::current_exe()?)?, version: current_version.into() })?; }
        self.acknowledged = true; Ok(())
    }
    pub fn poll_result(&mut self) -> Result<Option<UpdateResult>> {
        if self.consumed { return Ok(None); }
        // A recorded result always wins, including when the helper exited before startup.
        if self.result.try_exists()? {
            let value: ResultEnvelope = read_json(&self.result)?; ensure!(value.token == self.token, "Update result token mismatch");
            self.consumed = true; return Ok(Some(value.result));
        }
        if let Some(message) = self.helper.ended()? {
            // Close the publish-versus-exit race: re-read after observing completion.
            if self.result.try_exists()? {
                let value: ResultEnvelope = read_json(&self.result)?; ensure!(value.token == self.token, "Update result token mismatch");
                self.consumed = true; return Ok(Some(value.result));
            }
            no_links(&self.stage)?;
            let backup = self.stage.try_exists()?.then(||self.stage.clone());
            self.consumed = true;
            return Ok(Some(UpdateResult { status: if backup.is_some() { UpdateStatus::RecoveryRequired } else { UpdateStatus::Failed }, version: self.version.clone(), message, backup_path: backup }));
        }
        Ok(None)
    }
}

/// Read the fixed file version from a PE's RT_VERSION resource without executing it.
pub fn payload_version(bytes: &[u8]) -> Result<String> {
    fn u16at(b: &[u8], p: usize) -> Result<u16> { Ok(u16::from_le_bytes(b.get(p..p + 2).context("Truncated PE version")?.try_into()?)) }
    fn u32at(b: &[u8], p: usize) -> Result<u32> { Ok(u32::from_le_bytes(b.get(p..p + 4).context("Truncated PE version")?.try_into()?)) }
    ensure!(bytes.get(..2) == Some(b"MZ"), "Payload has no PE header");
    let pe = u32at(bytes, 0x3c)? as usize; ensure!(bytes.get(pe..pe + 4) == Some(b"PE\0\0"), "Invalid PE header");
    ensure!(u16at(bytes,pe + 4)? == 0x8664, "Payload must target Windows x64");
    let count = u16at(bytes, pe + 6)? as usize; ensure!(count > 0 && count <= 96, "Invalid PE section count");
    let optional = pe + 24; let opt_size = u16at(bytes, pe + 20)? as usize;
    ensure!(u16at(bytes,optional)? == 0x20b, "Windows x64 payload requires PE32+");
    let directory = optional + 112;
    ensure!(directory + 24 <= optional + opt_size, "Missing PE resource directory");
    let resource_rva = u32at(bytes, directory + 16)?;
    let resource_size = u32at(bytes, directory + 20)? as usize;
    ensure!(resource_rva != 0 && resource_size > 0, "Missing payload version resource");
    let map = |rva: u32, size: usize| -> Result<usize> {
        for index in 0..count { let section = optional + opt_size + index * 40; let start = u32at(bytes, section + 12)?; let len = u32at(bytes, section + 16)?; let raw = u32at(bytes, section + 20)?;
            if rva >= start && (rva as u64 + size as u64) <= start as u64 + len as u64 { let offset = raw as usize + (rva - start) as usize; ensure!(offset.checked_add(size).is_some_and(|end| end <= bytes.len()), "Truncated resource section"); return Ok(offset); }
        } bail!("Version resource lies outside PE sections")
    };
    let base = map(resource_rva, resource_size)?;
    let resource = &bytes[base..base + resource_size];
    let entries = |offset: usize| -> Result<Vec<(u32,u32)>> { let count = u16at(resource, offset + 12)? as usize + u16at(resource, offset + 14)? as usize; ensure!(count <= 256, "Oversized PE resource directory"); (0..count).map(|n| Ok((u32at(resource, offset + 16 + n * 8)?, u32at(resource, offset + 20 + n * 8)?))).collect() };
    let mut entry = entries(0)?.into_iter().find(|(id,_)| *id == 16).context("Payload version resource missing")?.1;
    for _ in 0..2 { ensure!(entry & 0x80000000 != 0, "Invalid version resource tree"); entry = entries((entry & 0x7fffffff) as usize)?.first().context("Empty version resource tree")?.1; }
    ensure!(entry & 0x80000000 == 0, "Invalid version data entry");
    let data = entry as usize; let rva = u32at(resource, data)?; let len = u32at(resource, data + 4)? as usize; let offset = map(rva,len)?; let value = &bytes[offset..offset+len];
    ensure!(u16at(value,0)? as usize <= len && u16at(value,2)? >= 52 && u16at(value,4)? == 0, "Invalid fixed version info");
    let mut cursor = 6; for c in "VS_VERSION_INFO".encode_utf16().chain(Some(0)) { ensure!(u16at(value,cursor)? == c, "Invalid version resource key"); cursor += 2; }
    cursor = (cursor + 3) & !3; ensure!(u32at(value,cursor)? == 0xfeef04bd, "Invalid fixed version signature");
    let ms = u32at(value,cursor+8)?; let ls = u32at(value,cursor+12)?; ensure!(ls & 0xffff == 0, "Payload has an unsupported revision");
    Ok(format!("{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn stable_versions_are_strict() { assert!(version("1.0.3").unwrap() > version("1.0.2").unwrap()); for bad in ["v1.0.3", "1.0.3-beta", "1.0", "01.0.3", "1.0.3.0"] { assert!(version(bad).is_err()); } }
    #[test] fn invalid_payload_fails_before_execution() { for bytes in [&b"not a PE"[..], &b"MZ"[..]] { assert!(payload_version(bytes).is_err()); } }
    #[test] fn malformed_health_args_fail_closed() { assert!(HealthProbe::from_args(&["app".into()], "1.0.3").unwrap().is_none()); assert!(HealthProbe::from_args(&["app".into(), "--update-token".into(), "timestamp".into()], "1.0.3").is_err()); }
    fn owned_pe_resource_fixture() -> Vec<u8> {
        fn put16(b:&mut[u8],p:usize,v:u16) { b[p..p+2].copy_from_slice(&v.to_le_bytes()); }
        fn put32(b:&mut[u8],p:usize,v:u32) { b[p..p+4].copy_from_slice(&v.to_le_bytes()); }
        let mut bytes=vec![0u8;1024]; bytes[..2].copy_from_slice(b"MZ"); put32(&mut bytes,0x3c,0x80); bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        put16(&mut bytes,0x84,0x8664); put16(&mut bytes,0x86,1); put16(&mut bytes,0x94,240); put16(&mut bytes,0x98,0x20b); put32(&mut bytes,0x98+112+16,0x1000); put32(&mut bytes,0x98+112+20,512);
        let section=0x98+240; put32(&mut bytes,section+12,0x1000); put32(&mut bytes,section+16,512); put32(&mut bytes,section+20,512);
        let base=512; put16(&mut bytes,base+14,1); put32(&mut bytes,base+16,16); put32(&mut bytes,base+20,0x80000020);
        put16(&mut bytes,base+32+14,1); put32(&mut bytes,base+32+16,1); put32(&mut bytes,base+32+20,0x80000040);
        put16(&mut bytes,base+64+14,1); put32(&mut bytes,base+64+16,0x409); put32(&mut bytes,base+64+20,96);
        put32(&mut bytes,base+96,0x1080); put32(&mut bytes,base+100,92);
        let value=base+128; put16(&mut bytes,value,92); put16(&mut bytes,value+2,52);
        for (i,c) in "VS_VERSION_INFO".encode_utf16().chain(Some(0)).enumerate() {put16(&mut bytes,value+6+i*2,c);}
        put32(&mut bytes,value+40,0xfeef04bd); put32(&mut bytes,value+48,0x00010000); put32(&mut bytes,value+52,0x00030000);
        bytes
    }
    #[test] fn update_payload_version_is_read_from_owned_pe_resource_fixture() {
        let mut bytes=owned_pe_resource_fixture();
        assert_eq!(payload_version(&bytes).unwrap(),"1.0.3"); bytes[512+128+52..512+128+56].copy_from_slice(&0x00020000u32.to_le_bytes()); assert_eq!(payload_version(&bytes).unwrap(),"1.0.2");
        bytes[512+96..512+100].copy_from_slice(&0xffff0000u32.to_le_bytes()); assert!(payload_version(&bytes).is_err());
    }
    #[test] fn update_payload_architecture_rejects_pe32_and_non_x64_machines() {
        let original=owned_pe_resource_fixture();
        for machine in [0x14cu16,0xaa64,0] { let mut bytes=original.clone(); bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes()); assert!(payload_version(&bytes).is_err()); }
        let mut bytes=original; bytes[0x98..0x9a].copy_from_slice(&0x10bu16.to_le_bytes()); assert!(payload_version(&bytes).is_err());
    }
    #[test] fn update_helper_directory_rejects_data_boundary_case_insensitively() {
        let manifest=PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace=manifest.ancestors().find(|p|p.file_name().is_some_and(|s|s=="outputs")).map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
        let root=workspace.join("verification").join(format!("helper-boundary-{}",token().unwrap()));
        fs::create_dir_all(root.join("Data")).unwrap(); fs::write(root.join("Data/personal.bin"),b"unchanged fixture bytes").unwrap();
        assert!(ensure_helper_outside_data(&root.join("Data"),&root).is_err());
        assert!(ensure_helper_outside_data(&root.join("dAtA").join("not-created-helper"),&root).is_err());
        assert!(ensure_helper_outside_data(&root.join("DataBackup").join("not-created-helper"),&root).is_ok());
        assert!(ensure_helper_outside_data(&root.join("helper"),&root).is_ok());
        assert_eq!(fs::read(root.join("Data/personal.bin")).unwrap(),b"unchanged fixture bytes");
        assert_eq!(fs::read_dir(root.join("Data")).unwrap().count(),1);
        fs::remove_dir_all(root).unwrap();
    }
    fn watcher_fixture_root() -> PathBuf {
        let manifest=PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace=manifest.ancestors().find(|p|p.file_name().is_some_and(|s|s=="outputs")).map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
        let root=workspace.join("verification").join(format!("watcher-{}",token().unwrap())); fs::create_dir(&root).unwrap(); root
    }
    #[test] fn update_result_token_and_recorded_result_take_priority_over_helper_exit() {
        let root=watcher_fixture_root(); let token=token().unwrap(); let stage=root.join(format!(".maxi-update-{token}")); fs::create_dir(&stage).unwrap(); let result=root.join("result.json");
        let mut probe=HealthProbe { health:None,result:result.clone(),token:token.clone(),acknowledged:true,consumed:false,helper:HelperProcess::Unavailable("owned fixture helper already exited".into()),stage:stage.clone(),version:"1.0.3".into() };
        let recorded=UpdateResult {status:UpdateStatus::Updated,version:"1.0.3".into(),message:"owned fixture result".into(),backup_path:None};
        fs::write(&result,serde_json::to_vec(&ResultEnvelope {token:"0".repeat(32),result:recorded.clone()}).unwrap()).unwrap();
        assert!(probe.poll_result().is_err()); assert!(!probe.consumed);
        fs::write(&result,serde_json::to_vec(&ResultEnvelope {token:token.clone(),result:recorded}).unwrap()).unwrap();
        assert_eq!(probe.poll_result().unwrap().unwrap().status,UpdateStatus::Updated); assert!(probe.poll_result().unwrap().is_none());
        fs::remove_file(&result).unwrap(); probe.consumed=false;
        let missing=probe.poll_result().unwrap().unwrap(); assert_eq!(missing.status,UpdateStatus::RecoveryRequired); assert_eq!(missing.backup_path,Some(stage));
        fs::remove_dir_all(root).unwrap();
    }
    #[test] fn update_owned_fixture_process_natural_exit_is_terminal_without_result() {
        fn assert_send<T:Send>() {} assert_send::<HealthProbe>();
        let root=watcher_fixture_root(); let token=token().unwrap(); let executable=canonical(&std::env::current_exe().unwrap()).unwrap();
        // Execute only this harness's test runner with an intentionally absent exact
        // test name: it runs zero tests and exits naturally, with no app services.
        let mut child=Command::new(&executable).arg("--exact").arg("OWNED-UPDATE-EMPTY-EXIT-FIXTURE").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let observer=HelperProcess::observe(child.id(),&executable);
        assert!(matches!(&observer,HelperProcess::Handle(_)), "owned fixture process handle was not verified");
        child.wait().unwrap();
        let mut probe=HealthProbe {health:None,result:root.join("missing-result.json"),token:token.clone(),acknowledged:true,consumed:false,helper:observer,stage:root.join(format!(".maxi-update-{token}")),version:"1.0.3".into()};
        let result=probe.poll_result().unwrap().unwrap(); assert_eq!(result.status,UpdateStatus::Failed); assert!(result.backup_path.is_none()); assert!(probe.poll_result().unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
