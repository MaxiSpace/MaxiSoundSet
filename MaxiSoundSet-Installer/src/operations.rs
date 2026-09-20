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
    let root = safe_root(request)?;
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
    let root = local_canonical(&safe_root(root)?)?;
    let m = manifest(&root)?;
    ensure!(
        !crate::running_app::is_running(&root)?,
        "MAXI SOUNDSET is running. Close the software before uninstalling."
    );
    ensure!(
        !root.join("Data/recovery.json").exists(),
        "Restore audio by opening the app and using Stop / Exit app before removing it"
    );
    for n in FILES.into_iter().chain([MARKER]) {
        writable(&root.join(n))?
    }
    let data = root.join("Data");
    if delete_data && data.exists() {
        checked_tree(&root, &data)?
    }
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
