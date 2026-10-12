use crate::platform;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
pub const APP: &[u8] = include_bytes!("../../MaxiSoundSet-Rust/MaxiSoundSet.exe");
pub const VERSION: &str = env!("MAXI_APP_VERSION");
const MAGIC: &str = "MAXI-SOUNDSET-INSTALL-v1";
const MARKER: &str = ".maxi-install.json";
const FILES: [&str; 7] = [
    "MaxiSoundSet.exe",
    "Uninstall.exe",
    "EULA_FA.md",
    "EULA_EN.md",
    "LICENSE.txt",
    "THIRD-PARTY-NOTICES.md",
    "ThirdParty-Licenses.txt",
];
#[derive(Serialize, Deserialize, Clone)]
pub struct Manifest {
    identity: String,
    root: PathBuf,
    version: String,
    files: Vec<String>,
    pub desktop: bool,
    pub startup: bool,
    integrated: bool,
    sha256: String,
}
#[derive(Clone)]
pub struct Options {
    pub accepted: bool,
    pub desktop: bool,
    pub startup: bool,
    pub integrated: bool,
    pub english: bool,
}
pub fn local_canonical(path: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        fs::canonicalize(path)?
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .to_owned(),
    ))
}
pub fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
pub fn no_links(path: &Path) -> Result<()> {
    for p in path.ancestors() {
        if let Ok(m) = fs::symlink_metadata(p) {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                !m.is_symlink() && m.file_attributes() & 0x400 == 0,
                "Symbolic links/junctions are not accepted: {}",
                p.display()
            )
        }
    }
    Ok(())
}
fn validate_data_directory(root: &Path) -> Result<()> {
    let path=root.join("Data");no_links(&path)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata)=>{use std::os::windows::fs::MetadataExt;ensure!(metadata.is_dir() && !metadata.is_symlink() && metadata.file_attributes()&0x400==0,"Data must be a regular directory without links");}
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>(),
        Err(error)=>return Err(error).context("Data directory could not be verified"),
    }
    Ok(())
}
pub fn safe_root(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Choose an absolute installation path");
    ensure!(
        !path
            .components()
            .any(|p| matches!(p, Component::ParentDir | Component::CurDir)),
        "Relative path components are not accepted"
    );
    let s = path.to_string_lossy();
    ensure!(
        !s.contains('"') && !s.contains(['\r', '\n']) && !s.starts_with("\\\\"),
        "Choose a local drive path without quotes"
    );
    ensure!(
        s.encode_utf16().count() < 220,
        "Installation path is too long"
    );
    ensure!(
        path.file_name().is_some() && path.components().count() >= 3,
        "Do not install at a drive root"
    );
    no_links(path)?;
    for n in [
        "USERPROFILE",
        "WINDIR",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
        "APPDATA",
    ] {
        if let Some(v) = std::env::var_os(n) {
            let p = PathBuf::from(v);
            ensure!(
                !platform::same(path, &p),
                "Choose a dedicated application folder"
            );
            if n == "WINDIR" {
                ensure!(
                    !path.starts_with(&p),
                    "Do not install in Windows system folders"
                )
            }
        }
    }
    Ok(path.to_path_buf())
}
pub fn manifest(root: &Path) -> Result<Manifest> {
    no_links(root)?;
    no_links(&root.join(MARKER))?;
    let m: Manifest = serde_json::from_slice(
        &fs::read(root.join(MARKER))
            .context("This folder is not a registered MAXI SOUNDSET installation")?,
    )?;
    ensure!(
        m.identity == MAGIC
            && platform::same(root, &m.root)
            && m.files == FILES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "Invalid installation ownership record"
    );
    Ok(m)
}
fn writable(file: &Path) -> Result<()> {
    no_links(file)?;
    if file.exists() {
        OpenOptions::new().write(true).open(file).with_context(|| {
            format!(
                "Close MAXI SOUNDSET with Exit app, or check write permissions: {}",
                file.display()
            )
        })?;
    }
    Ok(())
}
fn payload() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("MaxiSoundSet.exe", APP),
        ("EULA_FA.md", include_bytes!("../assets/EULA_FA.md")),
        ("EULA_EN.md", include_bytes!("../assets/EULA_EN.md")),
        (
            "LICENSE.txt",
            include_bytes!("../../MaxiSoundSet-Rust/LICENSE.txt"),
        ),
        (
            "THIRD-PARTY-NOTICES.md",
            include_bytes!("../assets/THIRD-PARTY-NOTICES.md"),
        ),
        (
            "ThirdParty-Licenses.txt",
            include_bytes!("../assets/ThirdParty-Licenses.txt"),
        ),
    ]
}
pub fn install(
    request: &Path,
    setup: &Path,
    options: Options,
    mut progress: impl FnMut(f32),
) -> Result<PathBuf> {
    ensure!(options.accepted, "Accept the agreement before installation");
    let _root_lease = platform::acquire_root_lease(request)?;
    let root = safe_root(request)?;
    validate_data_directory(&root)?;
    if root.exists() {
        ensure!(root.is_dir(), "Install location is not a folder");
        if root.join(MARKER).exists() {
            manifest(&root)?;
        } else {
            for n in FILES {
                ensure!(
                    !root.join(n).exists(),
                    "Existing files are not owned by the installer: {}",
                    root.join(n).display()
                )
            }
        }
    }
    ensure!(
        !root.join("Data/recovery.json").exists(),
        "Open the app and use Stop / Exit app to restore audio before updating"
    );
    for n in FILES.into_iter().chain([MARKER]) {
        writable(&root.join(n))?
    }
    let settings_path = root.join("Data/settings.json");
    writable(&settings_path)?;
    let mut settings: serde_json::Value = if settings_path.exists() {
        serde_json::from_slice(&fs::read(&settings_path)?)
            .context("Existing settings are invalid; preserve them and choose another folder")?
    } else {
        serde_json::json!({"english": options.english})
    };
    ensure!(
        settings.is_object(),
        "Existing settings must be a JSON object"
    );
    settings["startup"] = serde_json::json!(options.startup);
    let _com = if options.integrated {
        Some(platform::Com::new()?)
    } else {
        None
    };
    if options.integrated {
        if let Some(p) = platform::text(platform::UNINSTALL_KEY, "InstallLocation")? {
            ensure!(platform::same(Path::new(&p),&root),"A different installation is already registered at {p}. Uninstall it before choosing another folder.")
        }
    }
    let links = if options.integrated {
        platform::shortcut_paths(options.desktop)?
    } else {
        vec![]
    };
    platform::preflight_shortcuts(&links, &root)?;
    let snapshot = if options.integrated {
        Some(platform::Snapshot::capture()?)
    } else {
        None
    };
    let mut old_links = Vec::new();
    for (p, _) in &links {
        old_links.push((
            p.clone(),
            if p.exists() { Some(fs::read(p)?) } else { None },
        ))
    }
    fs::create_dir_all(&root)?;
    let root = local_canonical(&root)?;
    no_links(&root)?;
    let stage = root.join(format!(".maxi-stage-{}", nonce()));
    fs::create_dir(&stage)?;
    let mut backups: Vec<String> = Vec::new();
    let mut installed: Vec<String> = Vec::new();
    let mut old_settings_taken = false;
    let mut new_settings_installed = false;
    let result = (|| -> Result<()> {
        progress(0.04);
        fs::write(
            stage.join("new-settings.json"),
            serde_json::to_vec_pretty(&settings)?,
        )?;
        for (n, bytes) in payload() {
            let mut f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(stage.join(n))?;
            for (i, chunk) in bytes.chunks(1024 * 1024).enumerate() {
                f.write_all(chunk)?;
                if n == "MaxiSoundSet.exe" {
                    progress(
                        0.05 + 0.5 * ((i + 1) * 1024 * 1024).min(bytes.len()) as f32
                            / bytes.len() as f32,
                    )
                }
            }
            f.sync_all()?
        }
        fs::copy(setup, stage.join("Uninstall.exe"))?;
        let m = Manifest {
            identity: MAGIC.into(),
            root: root.clone(),
            version: VERSION.into(),
            files: FILES.iter().map(|s| s.to_string()).collect(),
            desktop: options.desktop,
            startup: options.startup,
            integrated: options.integrated,
            sha256: format!("{:x}", Sha256::digest(APP)),
        };
        fs::write(stage.join(MARKER), serde_json::to_vec_pretty(&m)?)?;
        for n in FILES.into_iter().chain([MARKER]) {
            let dst = root.join(n);
            if dst.exists() {
                fs::rename(&dst, stage.join(format!("old-{n}")))?;
                backups.push(n.into())
            }
            fs::rename(stage.join(n), dst)?;
            installed.push(n.into())
        }
        fs::create_dir_all(settings_path.parent().unwrap())?;
        if settings_path.exists() {
            fs::rename(&settings_path, stage.join("old-settings.json"))?;
            old_settings_taken = true;
        }
        fs::rename(stage.join("new-settings.json"), &settings_path)?;
        new_settings_installed = true;
        progress(0.8);
        if options.integrated {
            for (p, t) in &links {
                fs::create_dir_all(p.parent().unwrap())?;
                platform::shortcut(p, &root.join(t))?
            }
            platform::register(
                &root,
                VERSION,
                options.startup,
                ((APP.len() + fs::metadata(setup)?.len() as usize) / 1024) as u32,
            )?;
        }
        progress(1.0);
        Ok(())
    })();
    if let Err(error) = result {
        let mut failed = Vec::new();
        if new_settings_installed {
            if let Err(e) = fs::remove_file(&settings_path) {
                failed.push(e.to_string());
            }
        }
        if old_settings_taken {
            if let Err(e) = fs::rename(stage.join("old-settings.json"), &settings_path) {
                failed.push(e.to_string());
            }
        }
        if let Some(s) = snapshot {
            if let Err(e) = s.restore() {
                failed.push(e.to_string())
            }
        }
        for (p, data) in old_links {
            let r = if let Some(d) = data {
                fs::write(&p, d)
            } else if p.exists() {
                fs::remove_file(&p)
            } else {
                Ok(())
            };
            if let Err(e) = r {
                failed.push(e.to_string())
            }
        }
        for n in installed.iter().rev() {
            if let Err(e) = fs::remove_file(root.join(n)) {
                failed.push(e.to_string())
            }
        }
        for n in &backups {
            if let Err(e) = fs::rename(stage.join(format!("old-{n}")), root.join(n)) {
                failed.push(e.to_string())
            }
        }
        if failed.is_empty() {
            let _ = fs::remove_dir_all(&stage);
        } else {
            bail!(
                "{error:#}; rollback incomplete, backups kept at {}: {}",
                stage.display(),
                failed.join("; ")
            )
        }
        return Err(error);
    }
    no_links(&stage)?;
    fs::remove_dir_all(&stage)?;
    Ok(root)
}
/// A bounded upgrade owns only FILES/MARKER, never Data or Shell Links.
pub struct PendingUpgrade {
    pub root: PathBuf,
    pub stage: PathBuf,
    pub previous_version: String,
    metadata: platform::UpdateMetadataSnapshot,
    backups: Vec<String>,
    installed: Vec<String>,
}
impl PendingUpgrade {
    pub fn commit(self) -> Result<()> {
        remove_update_stage(&self.stage).context("Update succeeded but its backup stage could not be removed")
    }
    pub fn rollback(&mut self) -> Result<()> {
        ensure!(!crate::running_app::is_running(&self.root)?, "The application is still running; update backups were kept");
        validate_data_directory(&self.root)?;
        ensure!(crate::cable_evidence::recovery(&self.root.join("Data")) == crate::cable_evidence::Recovery::Clear, "Audio recovery is not clear; update backups were kept");
        self.rollback_files()
    }
    fn rollback_files(&mut self) -> Result<()> {
        no_links(&self.stage)?; no_links(&self.root)?;
        let mut failures = Vec::new();
        for name in self.installed.iter().rev() {
            let path = self.root.join(name);
            if let Err(e) = no_links(&path).and_then(|_| { if path.exists() { fs::remove_file(&path)?; } Ok(()) }) { failures.push(e.to_string()); }
        }
        for name in &self.backups {
            let from = self.stage.join(format!("old-{name}")); let to = self.root.join(name);
            if let Err(e) = no_links(&from).and_then(|_| no_links(&to)).and_then(|_| { fs::rename(from,to)?; Ok(()) }) { failures.push(e.to_string()); }
        }
        if let Err(e) = self.metadata.restore() { failures.push(e.to_string()); }
        ensure!(failures.is_empty(), "Rollback incomplete; backups retained at {}: {}", self.stage.display(), failures.join("; "));
        remove_update_stage(&self.stage)?; self.backups.clear(); self.installed.clear(); Ok(())
    }
}
pub fn preflight_update(root: &Path, setup: &Path, target: &str) -> Result<()> {
    ensure!(target == VERSION && crate::update_handoff::payload_version(APP)? == target, "Embedded application version does not match this installer; no update files were changed");
    let root = safe_root(root)?; let m = manifest(&root)?; validate_data_directory(&root)?;
    ensure!(crate::update_handoff::version(target)? > crate::update_handoff::version(&m.version)?, "Update version must be newer than the installed version");
    ensure!(platform::text(platform::UNINSTALL_KEY,"InstallLocation")?.is_some_and(|p| platform::same(Path::new(&p), &root)), "Registered installation differs from update root");
    no_links(setup)?;
    for entry in fs::read_dir(&root)? { let entry = entry?; ensure!(!entry.file_name().to_string_lossy().starts_with(".maxi-update-"), "An earlier update has retained backups. Resolve that update before retrying."); }
    for name in FILES.into_iter().chain([MARKER]) { no_links(&root.join(name))?; ensure!(!fs::metadata(root.join(name))?.permissions().readonly(), "Owned installation file is read-only"); }
    // Probe dedicated-directory permissions without reading or writing Data.
    let probe = root.join(format!(".maxi-write-probe-{}",crate::update_handoff::token()?));
    let file = OpenOptions::new().write(true).create_new(true).open(&probe).context("Installation folder is not writable by this user. Update cannot request elevation.")?;
    drop(file); fs::remove_file(probe)?; Ok(())
}
fn remove_update_stage(stage: &Path) -> Result<()> {
    no_links(stage)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(stage)? {
        let entry = entry?; let path = entry.path(); no_links(&path)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let old_name = name.strip_prefix("old-");
        ensure!(entry.file_type()?.is_file() && (FILES.contains(&name.as_str()) || name == MARKER || old_name.is_some_and(|n| FILES.contains(&n) || n == MARKER) || ["transaction.json","health.json","health.writing"].contains(&name.as_str())), "Unexpected update stage member; retained for recovery: {}",path.display());
        paths.push(path);
    }
    for path in paths { fs::remove_file(path)?; }
    fs::remove_dir(stage)?; Ok(())
}
pub fn begin_update(root: &Path, setup: &Path, target: &str, token: &str) -> Result<PendingUpgrade> {
    preflight_update(root,setup,target)?;
    ensure!(!crate::running_app::is_running(root)?, "Close the application before replacing its files");
    ensure!(crate::cable_evidence::recovery(&root.join("Data")) == crate::cable_evidence::Recovery::Clear, "Restore audio before replacing application files");
    begin_update_payload(root, setup, target, token, payload())
}
fn begin_update_payload(root: &Path, setup: &Path, target: &str, token: &str, contents: Vec<(&str,&[u8])>) -> Result<PendingUpgrade> {
    ensure!(crate::update_handoff::valid_token(token), "Invalid update token");
    let root = local_canonical(root)?; let previous = manifest(&root)?;
    validate_data_directory(&root)?;
    let stage = root.join(format!(".maxi-update-{token}")); no_links(&stage)?; fs::create_dir(&stage).context("Creating upgrade stage")?;
    let mut transaction = PendingUpgrade { root: root.clone(), stage: stage.clone(), previous_version: previous.version.clone(), metadata: platform::UpdateMetadataSnapshot::capture()?, backups: vec![], installed: vec![] };
    let result = (|| -> Result<()> {
        crate::update_handoff::write_json(&stage.join("transaction.json"), &serde_json::json!({"token":token,"root":root,"previous_version":previous.version,"target_version":target,"metadata":transaction.metadata.record(),"owned_files":FILES,"state":"staging"}))?;
        for (name,bytes) in &contents { ensure!(FILES.contains(name) && *name != "Uninstall.exe", "Unexpected update payload member"); let mut file = OpenOptions::new().write(true).create_new(true).open(stage.join(name)).with_context(||format!("Creating staged {name}"))?; file.write_all(bytes).with_context(||format!("Writing staged {name}"))?; file.sync_all().with_context(||format!("Flushing staged {name}"))?; }
        fs::copy(setup,stage.join("Uninstall.exe")).context("Staging update setup")?;
        let new_app = contents.iter().find(|(name,_)| *name == "MaxiSoundSet.exe").context("Missing update app")?.1;
        let updated = Manifest { version: target.into(), sha256: format!("{:x}",Sha256::digest(new_app)), ..previous };
        crate::update_handoff::write_json(&stage.join(MARKER), &updated)?;
        for name in FILES.into_iter().chain([MARKER]) {
            no_links(&root.join(name))?; no_links(&stage.join(name))?;
            fs::rename(root.join(name),stage.join(format!("old-{name}"))).with_context(||format!("Backing up {name}"))?; transaction.backups.push(name.into());
            fs::rename(stage.join(name),root.join(name)).with_context(||format!("Replacing {name}"))?; transaction.installed.push(name.into());
        }
        platform::update_registration_metadata(&root,target,((new_app.len() as u64 + fs::metadata(setup)?.len()) / 1024).min(u32::MAX as u64) as u32).context("Updating emulated or HKCU metadata")?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback) = transaction.rollback_files() { bail!("{error:#}; {rollback:#}"); }
        return Err(error);
    }
    Ok(transaction)
}

#[cfg(test)]
mod update_tests {
    use super::*;
    fn fixture() -> (PathBuf,PathBuf,PathBuf,platform::TestScope) {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().find(|p|p.file_name().is_some_and(|s|s == "outputs")).map(Path::to_path_buf).unwrap_or_else(||std::env::temp_dir());
        let sandbox = workspace.join("verification").join(format!("mu-{}",crate::update_handoff::token().unwrap()));
        let root = sandbox.join("i"); fs::create_dir_all(root.join("Data")).unwrap();
        let scope = platform::TestScope::new(&sandbox,false).unwrap();
        for name in FILES { fs::write(root.join(name),format!("old fixture {name}")).unwrap(); }
        fs::write(root.join("Data/settings.json"),b"\xef\xbb\xbf{ personal arbitrary bytes }\r\n").unwrap();
        fs::write(root.join("Data/other.bin"),[0,1,2,255]).unwrap();
        let root = local_canonical(&root).unwrap();
        let m = Manifest { identity:MAGIC.into(),root:root.clone(),version:"1.0.2".into(),files:FILES.iter().map(|v|v.to_string()).collect(),desktop:true,startup:true,integrated:true,sha256:format!("{:x}",Sha256::digest(fs::read(root.join("MaxiSoundSet.exe")).unwrap())) };
        fs::write(root.join(MARKER),serde_json::to_vec(&m).unwrap()).unwrap();
        platform::register(&root,"1.0.2",true,42).unwrap(); platform::test_set_startup("unrelated startup bytes --keep").unwrap();
        let setup = sandbox.join("source-owned-setup-fixture"); fs::write(&setup,b"fixture setup source").unwrap();
        (sandbox,root,setup,scope)
    }
    fn contents() -> Vec<(&'static str,&'static [u8])> { FILES.into_iter().filter(|n|*n!="Uninstall.exe").map(|n|(n,b"new owned fixture".as_slice())).collect() }
    fn files(root:&Path) -> Vec<(String,Vec<u8>)> { FILES.into_iter().chain([MARKER,"Data/settings.json","Data/other.bin"]).map(|n|(n.into(),fs::read(root.join(n)).unwrap())).collect() }
    #[test] fn update_keeps_data_and_startup_bytes_until_commit() {
        let (sandbox,root,setup,scope) = fixture(); let before = files(&root); let startup = platform::text(platform::RUN_KEY,"MaxiSoundSet").unwrap();
        let links = platform::shortcut_paths(true).unwrap();
        for (path,_) in &links { fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path,b"owned shortcut exact bytes\0\xff").unwrap(); }
        let link_bytes: Vec<_> = links.iter().map(|(path,_)|(path.clone(),fs::read(path).unwrap())).collect();
        let transaction = begin_update_payload(&root,&setup,"1.0.3",&crate::update_handoff::token().unwrap(),contents()).unwrap();
        assert!(transaction.stage.join("old-MaxiSoundSet.exe").exists());
        for (name,bytes) in before.iter().filter(|(n,_)|n.starts_with("Data/")) { assert_eq!(&fs::read(root.join(name)).unwrap(),bytes); }
        assert_eq!(platform::text(platform::RUN_KEY,"MaxiSoundSet").unwrap(),startup); assert_eq!(platform::text(platform::UNINSTALL_KEY,"DisplayVersion").unwrap(),Some("1.0.3".into()));
        for (path,bytes) in link_bytes { assert_eq!(fs::read(path).unwrap(),bytes); }
        let stage = transaction.stage.clone(); transaction.commit().unwrap(); assert!(!stage.exists()); drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn late_metadata_failure_rolls_back_exact_owned_files_and_data() {
        let (sandbox,root,setup,scope) = fixture(); let before = files(&root); let startup = platform::text(platform::RUN_KEY,"MaxiSoundSet").unwrap();
        platform::test_fail_registration_once();
        let error = begin_update_payload(&root,&setup,"1.0.3",&crate::update_handoff::token().unwrap(),contents()).err().unwrap();
        assert!(format!("{error:#}").contains("Controlled isolated-test metadata failure"));
        assert_eq!(files(&root),before); assert_eq!(platform::text(platform::RUN_KEY,"MaxiSoundSet").unwrap(),startup); assert_eq!(platform::text(platform::UNINSTALL_KEY,"DisplayVersion").unwrap(),Some("1.0.2".into()));
        drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn missing_staged_member_rolls_back_partial_replacement() {
        let (sandbox,root,setup,scope) = fixture(); let before = files(&root); let mut input = contents(); input.retain(|(n,_)|*n!="LICENSE.txt");
        assert!(begin_update_payload(&root,&setup,"1.0.3",&crate::update_handoff::token().unwrap(),input).is_err()); assert_eq!(files(&root),before); drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn update_rollback_restores_metadata_and_payload() {
        let (sandbox,root,setup,scope) = fixture(); let before = files(&root);
        let mut transaction = begin_update_payload(&root,&setup,"1.0.3",&crate::update_handoff::token().unwrap(),contents()).unwrap();
        transaction.rollback_files().unwrap(); assert_eq!(files(&root),before); assert_eq!(platform::text(platform::UNINSTALL_KEY,"DisplayVersion").unwrap(),Some("1.0.2".into())); drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn update_target_rejects_other_root_hash_and_manifest_before_replacement() {
        let (sandbox,root,_,scope) = fixture(); let exe = root.join("MaxiSoundSet.exe");
        assert!(crate::update_handoff::validate_target(&root,&exe).is_ok()); assert!(crate::update_handoff::validate_target(&sandbox,&exe).is_err());
        fs::write(&exe,b"tampered own source fixture").unwrap(); assert!(crate::update_handoff::validate_target(&root,&exe).is_err()); drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn update_preflight_wrong_payload_version_never_changes_owned_files() {
        let (sandbox,root,setup,scope) = fixture(); let before = files(&root);
        assert!(preflight_update(&root,&setup,"9.9.9").is_err()); assert_eq!(files(&root),before);
        assert_eq!(platform::text(platform::UNINSTALL_KEY,"DisplayVersion").unwrap(),Some("1.0.2".into())); drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn update_cleanup_rejects_unowned_stage_member_and_keeps_backups() {
        let (sandbox,root,setup,scope) = fixture();
        let transaction = begin_update_payload(&root,&setup,"1.0.3",&crate::update_handoff::token().unwrap(),contents()).unwrap();
        let stage = transaction.stage.clone(); fs::write(stage.join("unowned.bin"),b"must keep").unwrap();
        assert!(transaction.commit().is_err()); assert!(stage.join("old-MaxiSoundSet.exe").exists()); assert_eq!(fs::read(stage.join("unowned.bin")).unwrap(),b"must keep");
        drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn update_pending_recovery_retains_backups_without_file_replacement() {
        let (sandbox,root,setup,scope) = fixture();
        let mut transaction = begin_update_payload(&root,&setup,"1.0.3",&crate::update_handoff::token().unwrap(),contents()).unwrap(); let before = files(&root);
        fs::write(root.join("Data/recovery.json"),b"invalid fixture recovery").unwrap();
        assert!(transaction.rollback().is_err()); assert_eq!(files(&root),before); assert!(transaction.stage.join("old-MaxiSoundSet.exe").exists());
        drop(scope); fs::remove_dir_all(sandbox).unwrap();
    }
}

fn checked_tree(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.starts_with(root) && path != root,
        "Invalid data removal path"
    );
    no_links(path)?;
    for entry in fs::read_dir(path)? {
        let p = entry?.path();
        no_links(&p)?;
        if p.is_dir() {
            checked_tree(root, &p)?
        }
    }
    Ok(())
}
pub fn uninstall(root: &Path, delete_data: bool, mut progress: impl FnMut(f32)) -> Result<()> {
    uninstall_with_options(root, crate::cable::UninstallOptions { delete_data, ..Default::default() }, &mut progress)
}
pub fn uninstall_with_options(root: &Path, options: crate::cable::UninstallOptions, progress: impl FnMut(f32)) -> Result<()> {
    uninstall_with_cable_report(root, options, progress, |_|{})
}
pub fn uninstall_with_cable_report(root: &Path, options: crate::cable::UninstallOptions, progress: impl FnMut(f32), report: impl FnMut(crate::cable::Outcome)) -> Result<()> {
    uninstall_coordinated(root, options, progress, report, |root, selected, report| {
        crate::cable::run_removal(&mut crate::cable::NativeRemoval::new(root), selected, report)
    })
}
pub fn uninstall_with_output_report(root: &Path, options: crate::cable::UninstallOptions, destination: &str, progress: impl FnMut(f32), report: impl FnMut(crate::cable::Outcome)) -> Result<()> {
    let mut detail = None;
    let result = uninstall_coordinated(root, options, progress, report, |root,selected,report| {
        let (outcome,reason) = crate::cable::run_removal_to_output(&mut crate::cable::NativeRemoval::new(root),selected,destination,report);
        detail = reason;
        outcome
    });
    match detail {Some(detail) => result.context(detail),None => result}
}
pub(crate) fn validate_removal_root(root: &Path) -> Result<()> {
    let canonical = local_canonical(&safe_root(root)?)?;
    manifest(&canonical)?;
    validate_data_directory(&canonical)?;
    ensure!(crate::cable_evidence::recovery(&canonical.join("Data")) == crate::cable_evidence::Recovery::Clear, "Audio recovery could not be verified. Open the app and use Stop / Exit before uninstalling.");
    Ok(())
}
fn uninstall_coordinated(root: &Path, options: crate::cable::UninstallOptions, mut progress: impl FnMut(f32), mut report: impl FnMut(crate::cable::Outcome), mut cable_stage: impl FnMut(&Path, bool, &mut dyn FnMut(crate::cable::Outcome)) -> crate::cable::Outcome) -> Result<()> {
    let _root_lease = platform::acquire_root_lease(root)?;
    let delete_data = options.delete_data;
    let root = local_canonical(&safe_root(root)?)?;
    manifest(&root)?;
    validate_data_directory(&root)?;
    ensure!(
        !crate::running_app::is_running(&root)?,
        "MAXI SOUNDSET is running. Close the software before uninstalling."
    );
    ensure!(
        crate::cable_evidence::recovery(&root.join("Data")) == crate::cable_evidence::Recovery::Clear,
        "Restore audio by opening the app and using Stop / Exit app before removing it"
    );
    for n in FILES.into_iter().chain([MARKER]) {
        writable(&root.join(n))?
    }
    let data = root.join("Data");
    if delete_data && data.exists() {
        checked_tree(&root, &data)?
    }
    // All cable preparation/official action is before app/data/registry mutations.
    let cable_outcome = cable_stage(&root, options.remove_cable, &mut report);
    ensure!(cable_outcome.permits_app_removal(), "{} {}", cable_outcome.message(options.english), crate::cable::retry_message(options.english));
    // Official setup may have taken time. Revalidate app/recovery and ownership.
    let m = manifest(&root)?;
    validate_data_directory(&root)?;
    ensure!(!crate::running_app::is_running(&root)?, "MAXI SOUNDSET is running. Close it before uninstalling.");
    ensure!(crate::cable_evidence::recovery(&data) == crate::cable_evidence::Recovery::Clear, "Audio recovery could not be verified. Open the app and use Stop / Exit before uninstalling.");
    for n in FILES.into_iter().chain([MARKER]) { writable(&root.join(n))?; }
    if delete_data && data.exists() { checked_tree(&root, &data)?; }
    let _com = if m.integrated {
        Some(platform::Com::new()?)
    } else {
        None
    };
    let links = if m.integrated {
        platform::shortcut_paths(m.desktop)?
    } else {
        vec![]
    };
    // Detach known files atomically before removing integration; unknown files are untouched.
    let trash = root.join(format!(".maxi-remove-{}", nonce()));
    fs::create_dir(&trash)?;
    let mut moved = Vec::new();
    for n in FILES.into_iter().chain([MARKER]) {
        if root.join(n).exists() {
            if let Err(e) = fs::rename(root.join(n), trash.join(n)) {
                for old in moved.iter().rev() {
                    let _ = fs::rename(trash.join(old), root.join(old));
                }
                let _ = fs::remove_dir(&trash);
                return Err(e.into());
            }
            moved.push(n)
        }
    }
    let result = (|| -> Result<()> {
        if m.integrated {
            for (p, t) in &links {
                if p.exists()
                    && platform::shortcut_target(p)
                        .is_ok_and(|target| platform::same(&target, &root.join(t)))
                {
                    fs::remove_file(p)?
                }
            }
            platform::unregister(&root)?;
        }
        if delete_data && data.exists() {
            fs::remove_dir_all(&data)?;
        }
        for (i, n) in moved.iter().enumerate() {
            fs::remove_file(trash.join(n))?;
            progress((i + 1) as f32 / moved.len() as f32)
        }
        fs::remove_dir(&trash)?;
        let _ = fs::remove_dir(&root);
        Ok(())
    })();
    if let Err(e) = result {
        for n in &moved {
            if trash.join(n).exists() {
                let _ = fs::rename(trash.join(n), root.join(n));
            }
        }
        return Err(e);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_and_drive_root_rejected() {
        assert!(safe_root(Path::new("C:\\")).is_err());
        assert!(safe_root(Path::new("relative")).is_err());
        assert!(safe_root(Path::new("C:\\Temp\\..\\Other")).is_err());
    }
    #[test]
    fn agreement_is_required() {
        let r = install(
            Path::new("C:\\Temp\\MAXI"),
            Path::new("none"),
            Options {
                accepted: false,
                desktop: false,
                startup: false,
                integrated: false,
                english: false,
            },
            |_| {},
        );
        assert!(r.is_err())
    }
}

#[cfg(test)]
mod un04_tests {
    use super::*;
    use crate::cable::{Outcome, UninstallOptions};
    fn snapshot(root: &Path) -> Vec<(PathBuf,Vec<u8>)> {
        let mut result=vec![];for entry in fs::read_dir(root).unwrap(){let p=entry.unwrap().path();if p.is_dir(){result.extend(snapshot(&p));}else{result.push((p.clone(),fs::read(p).unwrap()));}}
        result.sort_by(|a,b|a.0.cmp(&b.0));result
    }
    fn fixture() -> (PathBuf,PathBuf,crate::platform::TestScope) {
        let sandbox=std::env::current_dir().unwrap().join(format!("un04-fixture-{}",nonce()));let root=sandbox.join("App");fs::create_dir_all(root.join("Data")).unwrap();
        let scope=crate::platform::TestScope::new(&sandbox,false).unwrap();
        for name in FILES {fs::write(root.join(name),"fixture").unwrap();}
        fs::write(root.join("Data/settings.json"),"preserve personal settings").unwrap();fs::write(root.join("Data/log.txt"),"preserve personal log").unwrap();fs::write(root.join("unrelated.txt"),"unrelated").unwrap();
        fs::write(root.join(MARKER),serde_json::to_vec(&serde_json::json!({"identity":MAGIC,"root":root,"version":"1.0.3","files":FILES,"desktop":true,"startup":true,"integrated":true,"sha256":"fixture"})).unwrap()).unwrap();
        crate::platform::register(&root,"1.0.3",true,1).unwrap();(sandbox,root,scope)
    }
    #[test] fn uninstall_data_file_is_not_treated_as_missing_recovery() {
        let (sandbox,root,scope)=fixture();let data=root.join("Data");let saved=root.join("SavedData");
        fs::rename(&data,&saved).unwrap();fs::write(&data,b"Data file fixture").unwrap();let before=snapshot(&sandbox);
        assert!(validate_data_directory(&root).is_err());assert!(validate_removal_root(&root).is_err());
        for remove_cable in [false,true] {assert!(uninstall_coordinated(&root,UninstallOptions{remove_cable,..Default::default()}, |_|panic!(), |_|panic!(), |_,_,_|panic!("official stage")).is_err());}
        assert_eq!(snapshot(&sandbox),before);fs::remove_file(&data).unwrap();assert!(validate_data_directory(&root).is_ok());fs::rename(&saved,&data).unwrap();assert!(validate_data_directory(&root).is_ok());
        drop(scope);fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn uninstall_recovery_read_error_blocks_both_cable_options_and_late_mutation() {
        use std::os::windows::fs::OpenOptionsExt;
        for remove_cable in [false,true] {
            let (sandbox,root,scope)=fixture();let marker=root.join("Data/recovery.json");
            fs::write(&marker,b"owned unreadable marker").unwrap();let before=snapshot(&sandbox);
            let registry=crate::platform::text(crate::platform::UNINSTALL_KEY,"InstallLocation").unwrap();
            let locked=fs::OpenOptions::new().read(true).share_mode(0).open(&marker).unwrap();
            assert_eq!(crate::cable_evidence::recovery(&root.join("Data")),crate::cable_evidence::Recovery::Unknown);
            assert!(validate_removal_root(&root).is_err());
            assert!(uninstall_coordinated(&root,UninstallOptions{remove_cable,..Default::default()}, |_|panic!("mutation"), |_|panic!("report"), |_,_,_|panic!("cable stage")).is_err());
            drop(locked);assert_eq!(snapshot(&sandbox),before);fs::remove_file(&marker).unwrap();
            let mut stage=false;let mut locked=None;
            assert!(uninstall_coordinated(&root,UninstallOptions{remove_cable,..Default::default()}, |_|panic!("mutation"), |_|{}, |_,_,_|{stage=true;fs::write(&marker,b"owned unreadable marker").unwrap();locked=Some(fs::OpenOptions::new().read(true).share_mode(0).open(&marker).unwrap());Outcome::ConfirmedAbsent}).is_err());
            drop(locked);assert!(stage);assert_eq!(snapshot(&sandbox),before);assert_eq!(crate::platform::text(crate::platform::UNINSTALL_KEY,"InstallLocation").unwrap(),registry);
            drop(scope);fs::remove_dir_all(sandbox).unwrap();
        }
    }
    #[test] fn every_non_success_cable_state_preserves_app_data_registry_and_progress() {
        let (sandbox,root,scope)=fixture();let before=snapshot(&sandbox);let registry=crate::platform::text(crate::platform::UNINSTALL_KEY,"InstallLocation").unwrap();let startup=crate::platform::text(crate::platform::RUN_KEY,"MaxiSoundSet").unwrap();
        for outcome in [Outcome::RestartRequired,Outcome::Cancelled,Outcome::LaunchFailed,Outcome::ExitFailed,Outcome::StillPresent,Outcome::UnknownAfter,Outcome::PreflightBlocked,Outcome::PackageFailed,Outcome::SetupStillRunning] {
            for english in [false,true] {for delete_data in [false,true] {
                let mut progressed=false;let mut reported=vec![];
                let result=uninstall_coordinated(&root,UninstallOptions{delete_data,remove_cable:true,english}, |_|progressed=true, |event|reported.push(event), |_,selected,report|{assert!(selected);report(outcome);outcome});
                assert!(result.is_err());assert!(!progressed);assert_eq!(reported,[outcome]);assert_eq!(snapshot(&sandbox),before);assert_eq!(crate::platform::text(crate::platform::UNINSTALL_KEY,"InstallLocation").unwrap(),registry);assert_eq!(crate::platform::text(crate::platform::RUN_KEY,"MaxiSoundSet").unwrap(),startup);
            }}
        }
        // Clearing cable selection preserves ordinary real fixture uninstall semantics.
        uninstall(&root,false, |_|{}).unwrap();assert!(root.join("Data/settings.json").exists());assert!(root.join("unrelated.txt").exists());assert!(!root.join("MaxiSoundSet.exe").exists());
        drop(scope);fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn confirmed_cable_result_survives_app_failure_without_app_mutations() {
        let (sandbox,root,scope)=fixture();let before=snapshot(&sandbox);let mut reports=vec![];
        // A new recovery marker simulates a concurrent audio state change after
        // the cable coordinator confirms absence, before the app mutation stage.
        let result=uninstall_coordinated(&root,UninstallOptions{remove_cable:true,delete_data:true,..Default::default()}, |_|panic!("unexpected app progress"), |event|reports.push(event), |root,_,report|{report(Outcome::ConfirmedAbsent);fs::write(root.join("Data/recovery.json"),"{}").unwrap();Outcome::ConfirmedAbsent});
        assert!(result.is_err());assert_eq!(reports,[Outcome::ConfirmedAbsent]);fs::remove_file(root.join("Data/recovery.json")).unwrap();assert_eq!(snapshot(&sandbox),before);
        drop(scope);fs::remove_dir_all(sandbox).unwrap();
    }
    #[test] fn accepted_absence_then_app_uninstall_keeps_or_deletes_data_independently() {
        for delete_data in [false,true] {let (sandbox,root,scope)=fixture();let mut reports=vec![];
            uninstall_coordinated(&root,UninstallOptions{remove_cable:true,delete_data,..Default::default()}, |_|{},|event|reports.push(event), |_,_,report|{report(Outcome::ConfirmedAbsent);Outcome::ConfirmedAbsent}).unwrap();
            assert_eq!(reports,[Outcome::ConfirmedAbsent]);assert!(!root.join("MaxiSoundSet.exe").exists());assert_eq!(root.join("Data/settings.json").exists(),!delete_data);assert!(root.join("unrelated.txt").exists());assert!(crate::platform::text(crate::platform::UNINSTALL_KEY,"InstallLocation").unwrap().is_none());drop(scope);fs::remove_dir_all(sandbox).unwrap();
        }
    }
    #[test] fn invalid_root_or_recovery_never_enters_official_stage() {
        assert!(uninstall_coordinated(Path::new("relative"),UninstallOptions{remove_cable:true,..Default::default()}, |_|panic!(), |_|panic!(), |_,_,_|panic!("official stage entered")).is_err());
        let (sandbox,root,scope)=fixture();fs::write(root.join("Data/recovery.json"),"pending").unwrap();
        assert!(uninstall_coordinated(&root,UninstallOptions{remove_cable:true,..Default::default()}, |_|panic!(), |_|panic!(), |_,_,_|panic!("official stage entered")).is_err());
        drop(scope);fs::remove_dir_all(sandbox).unwrap();
    }
}
