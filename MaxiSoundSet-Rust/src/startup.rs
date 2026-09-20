//! Per-user Windows logon registration. Only Maxi's own Run value is changed.
use anyhow::{ensure, Result};
use std::path::Path;
use windows::{
    core::PCWSTR,
    Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Registry::*},
};

const KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const NAME: &str = "MaxiSoundSet";
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn command(exe: &Path) -> Result<String> {
    let path = exe
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Executable path is not Unicode"))?;
    ensure!(
        exe.is_absolute() && !path.contains('"'),
        "Invalid startup executable path"
    );
    let command = format!("\"{path}\" --startup");
    ensure!(
        command.encode_utf16().count() < 260,
        "Startup path exceeds Windows Run command limit"
    );
    Ok(command)
}
pub fn apply(enabled: bool, exe: &Path) -> Result<()> {
    let path = crate::core_audio::wide(KEY);
    let name = crate::core_audio::wide(NAME);
    let mut handle = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut handle,
            None,
        )
        .ok()?;
    }
    let key = Key(handle);
    if enabled {
        let text = command(exe)?;
        let data: Vec<u8> = crate::core_audio::wide(&text)
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        unsafe {
            RegSetValueExW(key.0, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&data)).ok()?;
        }
    } else {
        let result = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
        if result != ERROR_FILE_NOT_FOUND {
            result.ok()?;
        }
    }
    ensure!(
        matches(exe, enabled)?,
        "Windows did not retain the startup preference"
    );
    log::info!(
        "Windows startup {} for {}",
        if enabled { "enabled" } else { "disabled" },
        exe.display()
    );
    Ok(())
}
pub fn matches(exe: &Path, enabled: bool) -> Result<bool> {
    let path = crate::core_audio::wide(KEY);
    let name = crate::core_audio::wide(NAME);
    let mut handle = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut handle,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(!enabled);
    }
    result.ok()?;
    let key = Key(handle);
    let mut bytes = vec![0u8; 4096];
    let mut size = bytes.len() as u32;
    let mut kind = REG_VALUE_TYPE::default();
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(!enabled);
    }
    result.ok()?;
    if !enabled {
        return Ok(false);
    }
    if kind != REG_SZ || size % 2 != 0 {
        return Ok(false);
    }
    let units: Vec<u16> = bytes[..size as usize]
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|u| *u != 0)
        .collect();
    Ok(String::from_utf16(&units)? == command(exe)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logon_command_quotes_spaces_and_never_uses_a_shell() {
        assert_eq!(
            command(Path::new("C:\\Music Apps\\MaxiSoundSet.exe")).unwrap(),
            "\"C:\\Music Apps\\MaxiSoundSet.exe\" --startup"
        );
        assert!(command(Path::new("relative.exe")).is_err());
        assert!(command(Path::new("C:\\bad\"name.exe")).is_err());
    }
}
