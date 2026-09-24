use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(default = "default_hotkeys")]
    pub hotkeys: Vec<HotkeyBinding>,
    pub target: i32,
    pub default_volume: i32,
    pub boost_db: i32,
    pub speed: i32,
    pub intensity: i32,
    pub mode: i32,
    pub profile: i32,
    pub last_profile: i32,
    pub leveling: bool,
    pub treble_smoothing: bool,
    pub eq_bands: [f32; 10],
    pub startup: bool,
    pub startup_active: bool,
    pub was_running: bool,
    pub output_id: String,
    pub source_id: String,
    pub auto_route: bool,
    pub debug_log: bool,
    pub english: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkeys: default_hotkeys(),
            target: 50,
            default_volume: 50,
            boost_db: 24,
            speed: 1,
            intensity: 1,
            mode: 0,
            profile: 1,
            last_profile: 1,
            leveling: true,
            treble_smoothing: true,
            eq_bands: [0.; 10],
            startup: true,
            startup_active: false,
            was_running: false,
            output_id: String::new(),
            source_id: String::new(),
            auto_route: true,
            debug_log: false,
            english: false,
        }
    }
}

/// A persisted Win32 virtual-key chord. Modifier bits use the MOD_* values from
/// Windows; keeping this independent of UI types makes settings backward compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyBinding {
    pub enabled: bool,
    pub modifiers: u32,
    pub virtual_key: u32,
}

pub const HOTKEY_ACTION_COUNT: usize = 14;
pub fn default_hotkeys() -> Vec<HotkeyBinding> {
    const ALT: u32 = 0x0001;
    const CTRL: u32 = 0x0002;
    const SHIFT: u32 = 0x0004;
    let chords = [
        (ALT, 0x21), // PageUp: target up
        (ALT, 0x22), // PageDown: target down
        (ALT, 0x23), // End: mute
        (ALT, 0x2D), // Insert: run
        (ALT, 0x24), // Home: pause/resume
        (ALT, 0x2E), // Delete: stop
        (CTRL | ALT, b'L' as u32),
        (CTRL | SHIFT, b'L' as u32),
        (CTRL | ALT, b'E' as u32),
        (CTRL | SHIFT, b'E' as u32),
        (CTRL | ALT, b'T' as u32),
        (0, 0xAF), // VK_VOLUME_UP
        (0, 0xAE), // VK_VOLUME_DOWN
        (0, 0xAD), // VK_VOLUME_MUTE
    ];
    chords.into_iter().map(|(modifiers, virtual_key)| HotkeyBinding {
        enabled: true, modifiers, virtual_key,
    }).collect()
}
impl Settings {
    pub fn load(data: &Path) -> Self {
        let mut s: Self = fs::read(data.join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if s.hotkeys.len() != HOTKEY_ACTION_COUNT {
            // A malformed/older partial list must not silently shift action IDs.
            s.hotkeys = default_hotkeys();
        }
        s.mode = s.mode.clamp(0, 1);
        s.target = s.target.clamp(0, if s.mode == 0 { 100 } else { 200 });
        s.default_volume = s.default_volume.clamp(0, 100);
        for gain in &mut s.eq_bands {
            *gain = if gain.is_finite() {
                gain.clamp(-12., 12.)
            } else {
                0.
            };
        }
        s.boost_db = s.boost_db.clamp(0, 36);
        s.speed = s.speed.clamp(0, 2);
        s.intensity = s.intensity.clamp(0, 2);
        s.mode = s.mode.clamp(0, 1);
        s.profile = s.profile.clamp(0, 7);
        s.last_profile = s.last_profile.clamp(1, 7);
        s
    }
    pub fn save(&self, data: &Path) -> Result<()> {
        write_json(&data.join("settings.json"), self)
    }
}
pub fn data_directory() -> Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .context("Executable directory unavailable")?
        .join("Data"))
}
pub fn write_json<T: Serialize>(file: &Path, value: &T) -> Result<()> {
    fs::create_dir_all(file.parent().context("Invalid data path")?)?;
    let temporary = file.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&temporary)?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.sync_all()?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            },
        };
        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )?;
        }
    }
    #[cfg(not(windows))]
    fs::rename(temporary, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_target_is_capped_while_full_boost_and_custom_eq_survive_reload() {
        let data = std::env::temp_dir().join(format!("maxi-eq-settings-{}", std::process::id()));
        let s = Settings {
            target: 200,
            mode: 0,
            profile: 7,
            last_profile: 7,
            eq_bands: [15.; 10],
            ..Settings::default()
        };
        s.save(&data).unwrap();
        let loaded = Settings::load(&data);
        assert_eq!(loaded.target, 100);
        assert_eq!(loaded.default_volume, 50);
        assert_eq!(loaded.profile, 7);
        assert_eq!(loaded.eq_bands, [12.; 10]);
        let s = Settings {
            mode: 1,
            eq_bands: [-4.5; 10],
            ..s
        };
        s.save(&data).unwrap();
        let loaded = Settings::load(&data);
        assert_eq!(loaded.target, 200);
        assert_eq!(loaded.eq_bands, [-4.5; 10]);
        std::fs::remove_file(data.join("settings.json")).unwrap();
        std::fs::remove_dir(data).unwrap();
    }

    #[test]
    fn existing_settings_get_gentle_profile_and_explicit_bypass_persists() {
        let old: Settings =
            serde_json::from_str(r#"{"target":123,"mode":1,"boost_db":25}"#).unwrap();
        assert_eq!(
            (
                old.target,
                old.mode,
                old.boost_db,
                old.profile,
                old.intensity,
                old.treble_smoothing,
                old.startup_active,
                old.was_running,
            ),
            (123, 1, 25, 1, 1, true, false, false)
        );
        let bypass = Settings { profile: 0, ..old };
        let saved = serde_json::to_string(&bypass).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&saved).unwrap().profile, 0);
        let old_without_hotkeys: Settings = serde_json::from_str(r#"{"target":44}"#).unwrap();
        assert_eq!(old_without_hotkeys.hotkeys, default_hotkeys());
    }

    #[test]
    fn global_hotkey_defaults_are_stable_and_round_trip() {
        let defaults = default_hotkeys();
        assert_eq!(defaults.len(), HOTKEY_ACTION_COUNT);
        assert_eq!((defaults[0].modifiers, defaults[0].virtual_key), (1, 0x21));
        assert_eq!((defaults[10].modifiers, defaults[10].virtual_key), (3, b'T' as u32));
        assert_eq!((defaults[11].modifiers, defaults[11].virtual_key), (0, 0xAF));
        let settings = Settings { hotkeys: defaults.clone(), ..Settings::default() };
        let restored: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.hotkeys, defaults);
    }

    #[test]
    fn default_volume_is_independent_clamped_and_persisted() {
        let data = std::env::temp_dir().join(format!("maxi-default-volume-{}", std::process::id()));
        let s = Settings { target: 20, default_volume: 20, ..Settings::default() };
        s.save(&data).unwrap();
        let loaded = Settings::load(&data);
        assert_eq!(loaded.target, 20);
        assert_eq!(loaded.default_volume, 20);

        let invalid = Settings { default_volume: 150, ..loaded };
        invalid.save(&data).unwrap();
        assert_eq!(Settings::load(&data).default_volume, 100);
        std::fs::remove_file(data.join("settings.json")).unwrap();
        std::fs::remove_dir(data).unwrap();
    }
}
