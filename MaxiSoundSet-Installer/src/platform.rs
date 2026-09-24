use anyhow::{ensure, Context, Result};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    ptr,
};
use windows::{
    core::{Interface, PCWSTR},
    Win32::{
        Foundation::ERROR_FILE_NOT_FOUND,
        System::{Com::*, Registry::*},
        UI::Shell::*,
    },
};
pub const UNINSTALL_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\MAXISOUNDSET";
pub const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain(Some(0)).collect()
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn open(key: &str, write: bool) -> Result<Option<Key>> {
    let k = wide(key_path(key));
    let mut h = HKEY::default();
    let result = unsafe {
        if write {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(k.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_QUERY_VALUE,
                None,
                &mut h,
                None,
            )
        } else {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(k.as_ptr()),
                None,
                KEY_QUERY_VALUE,
                &mut h,
            )
        }
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    result.ok()?;
    Ok(Some(Key(h)))
}
#[derive(Clone)]
pub struct Value {
    kind: REG_VALUE_TYPE,
    bytes: Vec<u8>,
}
fn raw(key: &str, name: &str) -> Result<Option<Value>> {
    if emulated_registry() {
        let key = key_path(key);
        return Ok(TEST_REGISTRY.with(|r| r.borrow().get(&(key, name.to_owned())).cloned()));
    }
    let Some(k) = open(key, false)? else {
        return Ok(None);
    };
    let n = wide(name);
    let mut size = 0;
    let mut kind = REG_VALUE_TYPE::default();
    let result = unsafe {
        RegQueryValueExW(
            k.0,
            PCWSTR(n.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    result.ok()?;
    let mut bytes = vec![0; size as usize];
    unsafe {
        RegQueryValueExW(
            k.0,
            PCWSTR(n.as_ptr()),
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
        .ok()?
    };
    bytes.truncate(size as usize);
    Ok(Some(Value { kind, bytes }))
}
fn put(key: &str, name: &str, value: Option<&Value>) -> Result<()> {
    if emulated_registry() {
        let key = key_path(key);
        TEST_REGISTRY.with(|r| {
            let mut registry = r.borrow_mut();
            if let Some(value) = value {
                registry.insert((key, name.to_owned()), value.clone());
            } else {
                registry.remove(&(key, name.to_owned()));
            }
        });
        return Ok(());
    }
    let Some(k) = open(key, true)? else {
        return Ok(());
    };
    let n = wide(name);
    let result = unsafe {
        if let Some(v) = value {
            RegSetValueExW(k.0, PCWSTR(n.as_ptr()), None, v.kind, Some(&v.bytes))
        } else {
            RegDeleteValueW(k.0, PCWSTR(n.as_ptr()))
        }
    };
    if value.is_none() && result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    result.ok()?;
    Ok(())
}
pub fn text(key: &str, name: &str) -> Result<Option<String>> {
    Ok(raw(key, name)?.map(|v| {
        String::from_utf16_lossy(
            &v.bytes
                .chunks_exact(2)
                .map(|x| u16::from_le_bytes([x[0], x[1]]))
                .collect::<Vec<_>>(),
        )
        .trim_end_matches('\0')
        .to_owned()
    }))
}
fn string(s: &str) -> Value {
    Value {
        kind: REG_SZ,
        bytes: wide(s).into_iter().flat_map(u16::to_le_bytes).collect(),
    }
}
fn number(n: u32) -> Value {
    Value {
        kind: REG_DWORD,
        bytes: n.to_le_bytes().to_vec(),
    }
}
pub struct Snapshot {
    values: Vec<(String, String, Option<Value>)>,
    existed: bool,
}
const NAMES: [&str; 9] = [
    "DisplayName",
    "DisplayVersion",
    "InstallLocation",
    "UninstallString",
    "DisplayIcon",
    "Publisher",
    "NoModify",
    "NoRepair",
    "EstimatedSize",
];
impl Snapshot {
    pub fn capture() -> Result<Self> {
        let existed = open(UNINSTALL_KEY, false)?.is_some();
        let mut values = Vec::new();
        for name in NAMES {
            values.push((UNINSTALL_KEY.into(), name.into(), raw(UNINSTALL_KEY, name)?))
        }
        values.push((
            RUN_KEY.into(),
            "MaxiSoundSet".into(),
            raw(RUN_KEY, "MaxiSoundSet")?,
        ));
        Ok(Self { values, existed })
    }
    pub fn restore(&self) -> Result<()> {
        for (k, n, v) in &self.values {
            put(k, n, v.as_ref())?
        }
        if !self.existed {
            delete_key(UNINSTALL_KEY)?
        }
        Ok(())
    }
}
pub fn same(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    norm(a) == norm(b)
}
pub fn register(root: &Path, version: &str, startup: bool, size: u32) -> Result<()> {
    if TEST_SCOPE.with(|s| s.borrow().is_some()) && FAIL_REGISTER.with(|f| f.replace(false)) {
        anyhow::bail!("Controlled isolated-test registration failure");
    }
    let app = root.join("MaxiSoundSet.exe");
    let values = [
        ("DisplayName", "MAXI SOUNDSET".into()),
        ("DisplayVersion", version.into()),
        ("InstallLocation", root.display().to_string()),
        (
            "UninstallString",
            format!("\"{}\"", root.join("Uninstall.exe").display()),
        ),
        ("DisplayIcon", format!("{},0", app.display())),
        ("Publisher", "MAXI SOUNDSET".into()),
    ];
    for (n, v) in values {
        put(UNINSTALL_KEY, n, Some(&string(&v)))?
    }
    for (n, v) in [("NoModify", 1), ("NoRepair", 1), ("EstimatedSize", size)] {
        put(UNINSTALL_KEY, n, Some(&number(v)))?
    }
    if startup {
        put(
            RUN_KEY,
            "MaxiSoundSet",
            Some(&string(&format!("\"{}\" --startup", app.display()))),
        )?
    } else if text(RUN_KEY, "MaxiSoundSet")?
        .is_some_and(|v| v.eq_ignore_ascii_case(&format!("\"{}\" --startup", app.display())))
    {
        put(RUN_KEY, "MaxiSoundSet", None)?
    }
    Ok(())
}
pub fn unregister(root: &Path) -> Result<()> {
    if text(UNINSTALL_KEY, "InstallLocation")?.is_some_and(|s| same(Path::new(&s), root)) {
        if emulated_registry() {
            delete_key(UNINSTALL_KEY)?;
        } else {
            unsafe {
                RegDeleteTreeW(
                    HKEY_CURRENT_USER,
                    PCWSTR(wide(key_path(UNINSTALL_KEY)).as_ptr()),
                )
                .ok()?
            }
        }
    }
    if text(RUN_KEY, "MaxiSoundSet")?.is_some_and(|s| {
        s.eq_ignore_ascii_case(&format!(
            "\"{}\" --startup",
            root.join("MaxiSoundSet.exe").display()
        ))
    }) {
        put(RUN_KEY, "MaxiSoundSet", None)?
    }
    Ok(())
}
pub struct Com;
impl Com {
    pub fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
pub fn folder(id: &windows::core::GUID) -> Result<PathBuf> {
    let p = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)? };
    let path = PathBuf::from(unsafe { p.to_string()? });
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    Ok(path)
}
pub fn default_path() -> Result<PathBuf> {
    Ok(folder(&FOLDERID_LocalAppData)?
        .join("Programs")
        .join("MAXI SOUNDSET"))
}
pub fn shortcut_paths(desktop: bool) -> Result<Vec<(PathBuf, String)>> {
    let programs = test_folder("Programs").unwrap_or(folder(&FOLDERID_Programs)?);
    let desktop_path = test_folder("Desktop").unwrap_or(folder(&FOLDERID_Desktop)?);
    let mut v = vec![
        (
            programs
                .clone()
                .join("MAXI SOUNDSET")
                .join("MAXI SOUNDSET.lnk"),
            "MaxiSoundSet.exe".into(),
        ),
        (
            programs.join("MAXI SOUNDSET").join("Uninstall.lnk"),
            "Uninstall.exe".into(),
        ),
    ];
    if desktop {
        v.push((
            desktop_path.join("MAXI SOUNDSET.lnk"),
            "MaxiSoundSet.exe".into(),
        ))
    }
    Ok(v)
}
pub fn shortcut(path: &Path, target: &Path) -> Result<()> {
    // Keep the integration harness inside its private filesystem scope. Shell Link COM can be
    // blocked by restricted Windows sessions, while the test only needs to verify ownership
    // and target selection. Production installs continue through the native COM implementation.
    if is_test_path(path) {
        fs::write(path, target.to_string_lossy().as_bytes())?;
        return Ok(());
    }
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    unsafe {
        link.SetPath(PCWSTR(wide(target).as_ptr()))?;
        link.SetWorkingDirectory(PCWSTR(
            wide(target.parent().context("Missing shortcut folder")?).as_ptr(),
        ))?;
        link.SetIconLocation(PCWSTR(wide(target).as_ptr()), 0)?;
        let persist: IPersistFile = link.cast()?;
        persist.Save(PCWSTR(wide(path).as_ptr()), true)?
    }
    Ok(())
}
pub fn shortcut_target(path: &Path) -> Result<PathBuf> {
    if is_test_path(path) {
        return Ok(PathBuf::from(fs::read_to_string(path)?));
    }
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    let persist: IPersistFile = link.cast()?;
    unsafe { persist.Load(PCWSTR(wide(path).as_ptr()), STGM_READ)? };
    let mut value = [0u16; 32768];
    unsafe { link.GetPath(&mut value, ptr::null_mut(), SLGP_RAWPATH.0 as u32)? };
    let len = value.iter().position(|v| *v == 0).unwrap_or(value.len());
    Ok(PathBuf::from(String::from_utf16_lossy(&value[..len])))
}
pub fn browse() -> Result<Option<PathBuf>> {
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)? };
    unsafe {
        dialog.SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
        if dialog.Show(None).is_err() {
            return Ok(None);
        }
        let item = dialog.GetResult()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let value = PathBuf::from(p.to_string()?);
        CoTaskMemFree(Some(p.0.cast()));
        Ok(Some(value))
    }
}
pub fn preflight_shortcuts(paths: &[(PathBuf, String)], root: &Path) -> Result<()> {
    for (p, t) in paths {
        if p.exists() {
            ensure!(
                same(&shortcut_target(p)?, &root.join(t)),
                "An unrelated shortcut already uses {}",
                p.display()
            )
        }
    }
    Ok(())
}

thread_local! {
    static TEST_SCOPE: std::cell::RefCell<Option<(String, PathBuf)>> = const { std::cell::RefCell::new(None) };
    static TEST_REGISTRY: std::cell::RefCell<HashMap<(String, String), Value>> = std::cell::RefCell::new(HashMap::new());
    static TEST_NATIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn emulated_registry() -> bool {
    TEST_SCOPE.with(|s| s.borrow().is_some()) && !TEST_NATIVE.with(std::cell::Cell::get)
}
fn key_path(key: &str) -> String {
    TEST_SCOPE.with(|s| {
        if let Some((prefix, _)) = &*s.borrow() {
            if key == UNINSTALL_KEY {
                format!("{prefix}\\Uninstall")
            } else if key == RUN_KEY {
                format!("{prefix}\\Run")
            } else {
                key.into()
            }
        } else {
            key.into()
        }
    })
}
fn test_folder(name: &str) -> Option<PathBuf> {
    TEST_SCOPE.with(|s| s.borrow().as_ref().map(|(_, p)| p.join(name)))
}
fn is_test_path(path: &Path) -> bool {
    !TEST_NATIVE.with(std::cell::Cell::get) && TEST_SCOPE.with(|s| {
        s.borrow()
            .as_ref()
            .is_some_and(|(_, root)| path.starts_with(root))
    })
}
fn delete_key(key: &str) -> Result<()> {
    if emulated_registry() {
        let key = key_path(key);
        let prefix = format!("{}\\", key.to_ascii_lowercase());
        TEST_REGISTRY.with(|r| {
            r.borrow_mut().retain(|(candidate, _), _| {
                let candidate = candidate.to_ascii_lowercase();
                candidate != key.to_ascii_lowercase() && !candidate.starts_with(&prefix)
            });
        });
        return Ok(());
    }
    let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(key_path(key)).as_ptr())) };
    if result != ERROR_FILE_NOT_FOUND {
        result.ok()?
    }
    Ok(())
}
pub struct TestScope {
    prefix: String,
    path: PathBuf,
    native: bool,
}
impl TestScope {
    pub fn new(path: &Path, native: bool) -> Result<Self> {
        let prefix = format!(
            "Software\\MAXISOUNDSETInstallerTests\\{}",
            crate::operations::nonce()
        );
        TEST_SCOPE.with(|s| *s.borrow_mut() = Some((prefix.clone(), path.into())));
        TEST_REGISTRY.with(|r| r.borrow_mut().clear());
        TEST_NATIVE.with(|value| value.set(native));
        Ok(Self {
            prefix,
            path: path.into(),
            native,
        })
    }
}
impl Drop for TestScope {
    fn drop(&mut self) {
        if self.native {
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(&self.prefix).as_ptr()));
            }
        }
        TEST_SCOPE.with(|s| *s.borrow_mut() = None);
        TEST_REGISTRY.with(|r| r.borrow_mut().clear());
        TEST_NATIVE.with(|value| value.set(false));
        for folder in ["Desktop", "Programs"] {
            let _ = fs::remove_dir_all(self.path.join(folder));
        }
    }
}
pub fn test_set_startup(s: &str) -> Result<()> {
    put(RUN_KEY, "MaxiSoundSet", Some(&string(s)))
}

pub fn cleanup_temp_helper() -> Result<()> {
    use windows::Win32::{Foundation::CloseHandle, Storage::FileSystem::*};
    let exe = std::env::current_exe()?;
    let parent = exe.parent().context("Missing temp folder")?;
    let temp = crate::operations::local_canonical(&std::env::temp_dir())?;
    let local = crate::operations::local_canonical(parent)?;
    ensure!(
        local.parent().is_some_and(|p| same(p, &temp))
            && local
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("MAXI-SOUNDSET-Uninstall-"))
            && exe.file_name().is_some_and(|n| n == "Uninstall.exe"),
        "Not an owned temporary uninstall helper"
    );
    crate::operations::no_links(&exe)?;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide(&exe).as_ptr()),
            DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    };
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
            FILE_DISPOSITION_FLAG_DELETE.0 | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0,
        ),
    };
    let result = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfoEx,
            (&info as *const FILE_DISPOSITION_INFO_EX).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    result?;
    let _ = fs::remove_dir(parent);
    Ok(())
}

thread_local! { static FAIL_REGISTER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub fn test_fail_registration_once() {
    assert!(TEST_SCOPE.with(|s| s.borrow().is_some()));
    FAIL_REGISTER.with(|f| f.set(true));
}
