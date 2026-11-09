#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app_log;
mod app_update;
mod update_handoff;
mod audio_engine;
mod audio_routing;
mod background;
mod components;
mod cable_evidence;
mod official_cable;
mod conflicts;
mod core_audio;
mod dsp;
mod enhancer;
mod loudness;
mod native_tray;
mod osd;
mod settings;
mod startup;
mod window_repaint;
use anyhow::{Context, Result};
use audio_engine::{AudioEngine, AudioEvent};
use settings::Settings;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
slint::include_modules!();

struct DeviceChoice {
    id: String,
    name: String,
    available: bool,
}

#[derive(Clone, Copy)]
enum AudioErrorPresentation {
    DeviceInvalidated { english: bool },
}
impl AudioErrorPresentation {
    fn text(self) -> &'static str {
        match self {
            Self::DeviceInvalidated { english } => device_invalidated_notice(english),
        }
    }
}
fn device_invalidated_notice(english: bool) -> &'static str {
    if english { "Audio device disconnected or changed. Connect or select a device, refresh devices, then Run." }
    else { "دستگاه صوتی قطع شد یا تغییر کرد. دستگاه را وصل یا انتخاب کن؛ سپس «به‌روزرسانی» و «اجرا» را بزن." }
}

struct State {
    background: background::Background,
    devices_pending: bool,
    conflicts_pending: bool,
    exiting: bool,
    settings: Settings,
    data: PathBuf,
    engine: AudioEngine,
    components: components::Manager,
    app_updates: app_update::Manager,
    update_handoff_result: Option<std::sync::mpsc::Receiver<Result<update_handoff::HealthProbe>>>,
    update_health: Option<update_handoff::HealthProbe>,
    outputs: Vec<DeviceChoice>,
    sources: Vec<DeviceChoice>,
    device_inventory: Vec<core_audio::DeviceInfo>,
    history: VecDeque<LevelPoint>,
    save_at: Option<Instant>,
    error: Option<String>,
    audio_error_presentation: Option<AudioErrorPresentation>,
    initialization_error: Option<String>,
    audio_apps: Vec<conflicts::AudioApp>,
    conflict_scan_at: Instant,
    unmute_target: Option<i32>,
    hotkey_errors: Vec<(usize, String)>,
    hotkey_pending_audio: Option<HotkeyPendingAudio>,
}
fn begin_component_check(ui: &AppWindow) {
    ui.set_components_busy(true);
    ui.set_components_installing(false);
    ui.set_component_check_state(1);
    ui.set_component_message("".into());
    ui.set_download_progress(0.0);
    ui.set_latest_version("".into());
    ui.set_update_available(false);
}
fn finish_component_check(ui: &AppWindow, installed: bool, outcome: components::CheckOutcome) {
    ui.set_components_busy(false);
    ui.set_cable_installed(installed);
    ui.set_component_check_state(match outcome {
        components::CheckOutcome::NotInstalled => 6,
        components::CheckOutcome::NewerAvailable => 2,
        components::CheckOutcome::NoNewer => 3,
        components::CheckOutcome::InstalledVersionUnknown => 4,
    });
}
fn component_error(ui: &AppWindow, error: String) {
    ui.set_components_busy(false);
    ui.set_download_progress(0.0);
    if ui.get_component_check_state() == 1 {
        ui.set_component_check_state(5);
        log::error!("VB-CABLE update check: {error}");
    } else {
        ui.set_components_installing(false);
        ui.set_component_message(error.into());
    }
}
fn app_update_error(ui: &AppWindow, error: &str) {
    log::error!("Application update: {error}");
    let kind = if error.contains("UPD_NETWORK") { 1 }
        else if error.contains("UPD_RELEASE") { 2 }
        else if error.contains("UPD_ASSET") { 3 }
        else if error.contains("UPD_HASH") || error.contains("UPD_SIZE") { 4 }
        else if error.contains("UPD_INSTALL_PATH") { 5 }
        else if error.contains("UPD_RECOVERY") { 7 }
        else { 6 };
    ui.set_app_update_error_kind(kind);
    ui.set_app_update_cancelling(false);
    ui.set_app_update_state(4);
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum HotkeyPendingAudio { Start, Stop }
fn strings(values: Vec<String>) -> ModelRc<slint::SharedString> {
    Rc::new(VecModel::from(
        values.into_iter().map(Into::into).collect::<Vec<_>>(),
    ))
    .into()
}
fn graph_path(history: &VecDeque<LevelPoint>, width: f32, height: f32, output: bool) -> String {
    let w = width.max(1.0);
    let h = height.max(1.0);
    if history.is_empty() {
        return format!("M0 {h} L{w} {h} Z");
    }
    let points: Vec<_> = history
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let level = if output { p.output } else { p.input };
            (
                i as f32 * w / (history.len() - 1).max(1) as f32,
                h * (1.0 - 0.85 * level.clamp(0.0, 1.0)),
            )
        })
        .collect();
    let mut path = format!("M0 {h:.2} L0 {:.2}", points[0].1);
    for pair in points.windows(2) {
        let (x, y) = pair[0];
        let (nx, ny) = pair[1];
        path.push_str(&format!(
            " Q{x:.2} {y:.2} {:.2} {:.2}",
            (x + nx) / 2.0,
            (y + ny) / 2.0
        ));
    }
    let (x, y) = points[points.len() - 1];
    path.push_str(&format!(" Q{x:.2} {y:.2} {w:.2} {y:.2} L{w:.2} {h:.2} Z"));
    path
}
fn show(ui: &AppWindow) {
    ui.window().set_minimized(false);
    if let Err(e) = ui.show() {
        log::error!("Show window: {e}");
    } else {
        window_repaint::after_show(ui);
    }
}
fn dashboard_accepts_notice(ui: &AppWindow) -> bool {
    ui.window().is_visible() && !ui.window().is_minimized()
}
fn present_notice(ui: &AppWindow, osd: &Rc<RefCell<Option<osd::Osd>>>, notice: osd::Notice<'_>) {
    log::debug!("Background notification requested: UI visible={}, minimized={}, OSD initialized={}", ui.window().is_visible(), ui.window().is_minimized(), osd.borrow().is_some());
    if dashboard_accepts_notice(ui) {
        let value = notice.value.map(|value| format!(" · {value}%")).unwrap_or_default();
        let text = if notice.detail.is_empty() { format!("{}{value}", notice.title) } else { format!("{}{value} · {}", notice.title, notice.detail) };
        ui.invoke_show_volume_notice(text.into());
    } else {
        if osd.borrow().is_none() {
            match osd::Osd::new() {
                Ok(window) => *osd.borrow_mut() = Some(window),
                Err(error) => {
                    log::error!("Background OSD initialization: {error:#}");
                    return;
                }
            }
        }
        if let Some(window) = osd.borrow().as_ref() {
            window.show_notice(notice);
        }
    }
}
fn notify_action(ui: &AppWindow, osd: &Rc<RefCell<Option<osd::Osd>>>, icon: osd::NoticeIcon, title_fa: &str, title_en: &str, detail_fa: &str, detail_en: &str) {
    let english = ui.get_english();
    present_notice(ui, osd, osd::Notice {
        title: if english { title_en } else { title_fa },
        detail: if english { detail_en } else { detail_fa },
        rtl: !english, icon, value: None, anchor: None,
    });
}
fn notify_audio_failure(ui: &AppWindow, osd: &Rc<RefCell<Option<osd::Osd>>>, title_fa: &str, title_en: &str, error: &str) {
    // Full diagnostics remain in status/log and the visible app. A short OSD
    // must not clip an arbitrary error chain into an apparently complete one.
    if dashboard_accepts_notice(ui) {
        notify_action(ui, osd, osd::NoticeIcon::Warning, title_fa, title_en, error, error);
    } else {
        notify_action(ui, osd, osd::NoticeIcon::Warning, title_fa, title_en,
            "برای جزئیات، گزارش برنامه را باز کن.", "Open the app log for details.");
    }
}
fn clear_audio_error(ui: &AppWindow, state: &mut State) {
    state.error = None;
    state.audio_error_presentation = None;
    ui.set_audio_error("".into());
}
fn refresh_audio_error_presentation(ui: &AppWindow, state: &mut State) {
    let Some(previous) = state.audio_error_presentation else { return; };
    // The kind and previous language identify exactly the text we own. Slint's
    // X clears the card independently; a language change must not revive it or
    // overwrite a newer recovery/exit message.
    if ui.get_audio_error().as_str() != previous.text() {
        state.audio_error_presentation = None;
        return;
    }
    let next = AudioErrorPresentation::DeviceInvalidated { english: ui.get_english() };
    ui.set_audio_error(next.text().into());
    if ui.get_status().as_str() == previous.text() { ui.set_status(next.text().into()); }
    state.audio_error_presentation = Some(next);
}
fn present_audio_failure(ui: &AppWindow, state: &mut State, osd: &Rc<RefCell<Option<osd::Osd>>>, raw: String, presentation: Option<AudioErrorPresentation>) {
    let display = presentation.map(|kind| kind.text().to_owned()).unwrap_or_else(|| raw.clone());
    state.error = Some(raw);
    state.audio_error_presentation = presentation;
    ui.set_audio_error(display.clone().into());
    ui.set_status(display.clone().into());
    if state.hotkey_pending_audio.take().is_some() {
        // The persistent dashboard card already owns the localized message.
        // Avoid a second transient that could resurface in an old language
        // after dismissal/Run. The hidden/minimized OSD keeps its short route.
        if presentation.is_none() || !dashboard_accepts_notice(ui) {
            notify_audio_failure(ui, osd, "عملیات صدا ناموفق بود", "Audio action failed", &display);
        }
    } else { show(ui); }
}
fn notify_target(ui: &AppWindow, osd: &Rc<RefCell<Option<osd::Osd>>>, anchor: Option<(i32, i32)>) {
    let english = ui.get_english();
    let (title, detail) = match (ui.get_running(), ui.get_paused(), english) {
        (false, _, true) => ("Playback stopped", "Saved · Run to apply"),
        (false, _, false) => ("پردازش متوقف است", "ذخیره شد؛ «اجرا» را بزنید"),
        (true, true, true) => ("Playback paused", "Saved · Resume to apply"),
        (true, true, false) => ("پردازش در حالت مکث است", "ذخیره شد؛ «ادامه» را بزنید"),
        (true, false, true) => ("Volume target", ""),
        (true, false, false) => ("سطح صدای هدف", ""),
    };
    present_notice(ui, osd, osd::Notice {
        title, detail, rtl: !english,
        icon: if ui.get_target() == 0 { osd::NoticeIcon::Muted } else { osd::NoticeIcon::Volume },
        value: Some(ui.get_target()), anchor,
    });
}
fn handle_tray_scroll(ui: &AppWindow, state: &Rc<RefCell<State>>, osd: &Rc<RefCell<Option<osd::Osd>>>, windows_mode: bool, steps: i32, anchor: (i32, i32)) {
    // A mode switch can commit before an already emitted native action drains.
    // Keep that old gesture from being reinterpreted with the new mode's range.
    if windows_mode != (ui.get_mode() == 0) || steps == 0 || !tray_scroll_allowed(ui, &state.borrow()) { return; }
    let target = target_after_wheel(ui.get_target(), steps, ui.get_mode());
    if target != ui.get_target() {
        ui.set_target(target);
        let mut state = state.borrow_mut();
        // Wheel-up from zero is deliberately gradual, unlike the restore hotkey.
        state.unmute_target = None;
        read_config(ui, &mut state);
    }
    notify_target(ui, osd, Some(anchor));
}
fn target_after_wheel(current: i32, steps: i32, mode: i32) -> i32 {
    current.saturating_add(steps.saturating_mul(2)).clamp(0, if mode == 0 { 100 } else { 200 })
}
fn tray_scroll_allowed(ui: &AppWindow, state: &State) -> bool {
    !state.exiting && !ui.get_busy() && !state.components.busy && ui.get_hotkey_capture_index() < 0
}
fn audio_toggle_allowed(ui: &AppWindow, state: &State) -> bool {
    !ui.get_busy() && !state.exiting && (ui.get_running()
        || (!state.components.busy && !state.devices_pending && ui.get_devices_ready() && ui.get_recovery_ready()
            && selected_devices_available(ui, state)))
}
fn handle_tray_start(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    // A queued Run is an explicit start request, never a toggle into Stop.
    // Release the State borrow before invoking the callback, which rechecks
    // the same readiness gate and marks an accepted start busy synchronously.
    let allowed = !ui.get_running() && audio_toggle_allowed(ui, &state.borrow());
    if allowed { ui.invoke_toggle_audio(); }
}
fn profile_label(profile: i32, english: bool) -> &'static str {
    const EN: [&str; 8] = ["System default", "Gentle", "Voice / dialogue", "Music", "Cinema", "Warm & relaxed", "Gaming focus", "Custom equalizer"];
    const FA: [&str; 8] = ["پیش‌فرض سیستم", "ملایم", "گفتار و دیالوگ", "موسیقی", "سینما", "گرم و آرام", "تمرکز بازی", "اکولایزر سفارشی"];
    if english { EN[profile.clamp(0, 7) as usize] } else { FA[profile.clamp(0, 7) as usize] }
}
fn unavailable_audio_control_shortcut(ui: &AppWindow, action: usize) -> Option<(&'static str, &'static str)> {
    if (8..=10).contains(&action) && ui.get_mode() != 1 {
        Some(("نیازمند تغییر حالت به پردازش کابل مجازی", "Requires switching to virtual cable processing mode."))
    } else if action == 10 && ui.get_profile() == 0 {
        Some(("برای شنیدن اثر، یک پروفایل بهبود صدا انتخاب کن.", "Select an enhancement profile to hear treble smoothing."))
    } else {
        None
    }
}
fn handle_hotkey(ui: &AppWindow, state: &Rc<RefCell<State>>, osd: &Rc<RefCell<Option<osd::Osd>>>, action: usize) {
    if state.borrow().exiting { return; }
    if ui.get_busy() {
        notify_action(ui, osd, osd::NoticeIcon::Warning, "در حال اجرا", "Audio busy", "درخواست قبلی هنوز کامل نشده است.", "The previous audio action is still pending.");
        return;
    }
    if let Some((detail_fa, detail_en)) = unavailable_audio_control_shortcut(ui, action) {
        let (title_fa, title_en) = match action {
            8 => ("بهبود صدا غیرفعال", "Enhancement unavailable"),
            9 => ("پروفایل غیرفعال", "Profile unavailable"),
            _ => ("نرم‌سازی غیرفعال", "Smoothing unavailable"),
        };
        notify_action(ui, osd, osd::NoticeIcon::Warning, title_fa, title_en, detail_fa, detail_en);
        return;
    }
    match action {
        0 | 1 | 11 | 12 => {
            let delta = if action == 0 || action == 11 { 5 } else { -5 };
            let ceiling = if ui.get_mode() == 0 { 100 } else { 200 };
            let current = if ui.get_target() == 0 && delta > 0 {
                let mut s = state.borrow_mut();
                s.unmute_target.take().unwrap_or(s.settings.default_volume)
            } else { ui.get_target() };
            ui.set_target((current + delta).clamp(0, ceiling));
            read_config(ui, &mut state.borrow_mut());
            notify_target(ui, osd, None);
        }
        2 | 13 => {
            let unmute_target = state.borrow_mut().unmute_target.take();
            if ui.get_target() > 0 {
                state.borrow_mut().unmute_target = Some(ui.get_target());
                ui.set_target(0);
            } else if let Some(target) = unmute_target {
                ui.set_target(target.clamp(0, if ui.get_mode() == 0 { 100 } else { 200 }));
            } else {
                ui.set_target(state.borrow().settings.default_volume.clamp(1, 100));
            }
            read_config(ui, &mut state.borrow_mut());
            notify_target(ui, osd, None);
        }
        3 => {
            if ui.get_running() { notify_action(ui, osd, osd::NoticeIcon::Play, "صدا در حال اجراست", "Audio already running", "", ""); }
            else if !ui.get_devices_ready() || !ui.get_recovery_ready() {
                notify_action(ui, osd, osd::NoticeIcon::Warning, "صدا هنوز آماده نیست", "Audio is not ready", "منتظر آماده‌شدن دستگاه‌ها و بازیابی صدا بمان.", "Wait for audio-device initialization and recovery.");
            }
            else {
                ui.invoke_toggle_audio();
                if ui.get_busy() {
                    state.borrow_mut().hotkey_pending_audio = Some(HotkeyPendingAudio::Start);
                    notify_action(ui, osd, osd::NoticeIcon::Play, "در حال شروع صدا", "Starting audio", "در انتظار آماده‌شدن موتور صدا", "Waiting for the audio engine");
                } else {
                    let detail = state.borrow().error.clone().unwrap_or_else(|| "موتور صدا شروع نشد".into());
                    notify_audio_failure(ui, osd, "شروع صدا ناموفق بود", "Could not start audio", &detail);
                }
            }
        }
        4 => {
            if ui.get_running() {
                ui.invoke_toggle_pause();
                if ui.get_paused() { notify_action(ui, osd, osd::NoticeIcon::Pause, "صدا مکث شد", "Audio paused", "", ""); }
                else { notify_action(ui, osd, osd::NoticeIcon::Play, "پخش ادامه یافت", "Audio resumed", "", ""); }
            } else { notify_action(ui, osd, osd::NoticeIcon::Warning, "صدا اجرا نیست", "Audio is not running", "برای مکث، ابتدا صدا را اجرا کن.", "Start audio before using pause."); }
        }
        5 => {
            if ui.get_running() {
                state.borrow().engine.stop(); ui.set_busy(true); ui.set_status("Stopping and restoring audio…".into());
                state.borrow_mut().hotkey_pending_audio = Some(HotkeyPendingAudio::Stop);
                notify_action(ui, osd, osd::NoticeIcon::Stop, "در حال توقف صدا", "Stopping audio", "در حال بازگردانی دستگاه صدا", "Restoring the audio device");
            } else { notify_action(ui, osd, osd::NoticeIcon::Stop, "صدا متوقف است", "Audio is already stopped", "", ""); }
        }
        6 => {
            ui.set_leveling(!ui.get_leveling()); read_config(ui, &mut state.borrow_mut());
            if ui.get_leveling() { notify_action(ui, osd, osd::NoticeIcon::Leveling, "بلندی پایدار روشن", "Steady loudness on", "", ""); }
            else { notify_action(ui, osd, osd::NoticeIcon::Leveling, "بلندی پایدار خاموش", "Steady loudness off", "", ""); }
        }
        7 => {
            ui.set_intensity(match ui.get_intensity() { 2 => 1, 1 => 0, _ => 2 }); read_config(ui, &mut state.borrow_mut());
            let level = match ui.get_intensity() { 2 => ("کم", "Low"), 1 => ("متوسط", "Balanced"), _ => ("زیاد", "High") };
            if ui.get_leveling() { notify_action(ui, osd, osd::NoticeIcon::Leveling, "شدت بلندی پایدار", "Loudness intensity", level.0, level.1); }
            else { notify_action(ui, osd, osd::NoticeIcon::Leveling, "شدت ذخیره شد", "Intensity saved", "برای اعمال، بلندی پایدار را روشن کن.", "Turn on steady loudness to apply it."); }
        }
        8 => {
            if ui.get_profile() == 0 { ui.set_profile(ui.get_last_profile().clamp(1, 7)); }
            else { ui.set_last_profile(ui.get_profile()); ui.set_profile(0); }
            read_config(ui, &mut state.borrow_mut());
            let profile = profile_label(ui.get_profile(), ui.get_english());
            if ui.get_profile() == 0 { notify_action(ui, osd, osd::NoticeIcon::Profile, "بهبود صدا خاموش", "Enhancement off", profile, profile); }
            else { notify_action(ui, osd, osd::NoticeIcon::Profile, "بهبود صدا روشن", "Enhancement on", profile, profile); }
        }
        9 => {
            let next = (ui.get_profile() + 1) % 8;
            ui.set_profile(next);
            if next != 0 { ui.set_last_profile(next); }
            read_config(ui, &mut state.borrow_mut());
            let label = profile_label(next, ui.get_english());
            let detail_fa = format!("پروفایل: {label}");
            let detail_en = format!("Profile: {label}");
            notify_action(ui, osd, osd::NoticeIcon::Profile, "پروفایل بهبود صدا", "Enhancement profile", &detail_fa, &detail_en);
        }
        10 => {
            ui.invoke_request_treble_smoothing(!ui.get_treble_smoothing());
            if ui.get_treble_smoothing() { notify_action(ui, osd, osd::NoticeIcon::Smoothing, "نرم‌سازی صدای زیر روشن", "Treble smoothing on", "", ""); }
            else { notify_action(ui, osd, osd::NoticeIcon::Smoothing, "نرم‌سازی صدای زیر خاموش", "Treble smoothing off", "", ""); }
        }
        _ => log::warn!("Unknown global shortcut action index {action}"),
    }
}
fn hotkey_names(index: usize, english: bool) -> &'static str {
    const EN: [&str; 14] = ["Volume up", "Volume down", "Mute / restore", "Start audio", "Pause / resume", "Stop audio", "Steady loudness on/off", "Loudness intensity", "Enhancement on/off", "Cycle enhancement profile", "Treble smoothing on/off", "Media volume up", "Media volume down", "Media mute"];
    const FA: [&str; 14] = ["افزایش ولوم", "کاهش ولوم", "قطع/وصل صدا", "اجرای صدا", "مکث/ادامه", "توقف صدا", "روشن/خاموش بلندی پایدار", "شدت بلندی پایدار", "روشن/خاموش بهبود صدا", "چرخش پروفایل بهبود", "نرم‌سازی صدای زیر", "ولوم بالای کیبورد", "ولوم پایین کیبورد", "قطع صدای کیبورد"];
    if english { EN[index] } else { FA[index] }
}
fn virtual_key_name(vk: u32) -> String {
    match vk {
        0x21 => "PageUp".into(), 0x22 => "PageDown".into(), 0x23 => "End".into(),
        0x24 => "Home".into(), 0x2D => "Insert".into(), 0x2E => "Delete".into(),
        0x20 => "Space".into(), 0x0D => "Enter".into(), 0x09 => "Tab".into(),
        0x08 => "Backspace".into(), 0x25 => "Left".into(), 0x26 => "Up".into(),
        0x27 => "Right".into(), 0x28 => "Down".into(), 0xAF => "VolumeUp".into(),
        0xAE => "VolumeDown".into(), 0xAD => "Mute".into(),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).unwrap().to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => format!("VK{:02X}", vk),
    }
}
fn hotkey_chord(binding: crate::settings::HotkeyBinding) -> String {
    if !binding.enabled { return "—".into(); }
    let mut parts: Vec<String> = Vec::new();
    if binding.modifiers & 0x0002 != 0 { parts.push("Ctrl".into()); }
    if binding.modifiers & 0x0001 != 0 { parts.push("Alt".into()); }
    if binding.modifiers & 0x0004 != 0 { parts.push("Shift".into()); }
    parts.push(virtual_key_name(binding.virtual_key));
    parts.join("+")
}
fn sync_hotkey_rows(ui: &AppWindow, state: &State) {
    let rows = state.settings.hotkeys.iter().enumerate().map(|(index, binding)| HotkeyRow {
        name: hotkey_names(index, ui.get_english()).into(),
        chord: hotkey_chord(*binding).into(),
        enabled: binding.enabled,
        error: state.hotkey_errors.iter().find(|(i, _)| *i == index).map(|(_, e)| e.as_str()).unwrap_or("").into(),
    }).collect::<Vec<_>>();
    ui.set_hotkey_rows(Rc::new(VecModel::from(rows)).into());
}
fn capture_validation(index: usize, vk: u32, modifiers: u32) -> std::result::Result<(), &'static str> {
    if index >= crate::settings::HOTKEY_ACTION_COUNT { return Err("Invalid shortcut row"); }
    if matches!(vk, 0x10 | 0x11 | 0x12 | 0xA0..=0xA5) { return Err("Press a non-modifier key"); }
    if modifiers & !0x0007 != 0 { return Err("Windows-key shortcuts are not supported"); }
    if index >= 11 {
        if modifiers != 0 { return Err("Media volume keys cannot use modifiers"); }
        if !matches!(vk, 0xAD | 0xAE | 0xAF) { return Err("Use a media volume key for this row"); }
    } else {
        if vk == 0x1B { return Err("Escape cancels recording"); }
        if modifiers == 0 { return Err("Add Ctrl, Alt, or Shift to the shortcut"); }
        if (modifiers & 0x0001 != 0 && matches!(vk, 0x09 | 0x73 | 0x1B | 0x20))
            || (modifiers & 0x0002 != 0 && vk == 0x1B)
            || (modifiers & 0x0006 == 0x0006 && vk == 0x1B)
            || (modifiers & 0x0003 == 0x0003 && vk == 0x2E)
        { return Err("This key combination is reserved by Windows"); }
    }
    Ok(())
}
fn cancel_capture(ui: &AppWindow, state: &Rc<RefCell<State>>, native: &Rc<RefCell<Option<native_tray::Tray>>>, index: usize) {
    let bindings = state.borrow().settings.hotkeys.clone();
    let errors = native.borrow_mut().as_mut().map(|tray| tray.register_hotkeys(&bindings)).unwrap_or_default();
    state.borrow_mut().hotkey_errors = errors;
    ui.set_hotkey_capture_index(-1);
    sync_hotkey_rows(ui, &state.borrow());
    if index < crate::settings::HOTKEY_ACTION_COUNT {
        ui.set_status(if ui.get_english() { "Shortcut recording cancelled." } else { "ضبط میان‌بر لغو شد." }.into());
    }
}
fn new_registration_failures(
    candidate: &[crate::settings::HotkeyBinding],
    previous: &[crate::settings::HotkeyBinding],
    prior_errors: &[(usize, String)],
    errors: &[(usize, String)],
    changed_index: Option<usize>,
) -> Vec<(usize, String)> {
    let old_failed: std::collections::HashSet<_> = prior_errors.iter().map(|(i, _)| *i).collect();
    errors.iter().filter(|(i, _)| {
        changed_index == Some(*i)
            || !old_failed.contains(i)
            || candidate.get(*i) != previous.get(*i)
    }).cloned().collect()
}
fn accept_captured_hotkey(ui: &AppWindow, state: &Rc<RefCell<State>>, native: &Rc<RefCell<Option<native_tray::Tray>>>, index: usize, vk: u32, modifiers: u32) {
    if let Err(error) = capture_validation(index, vk, modifiers) {
        cancel_capture(ui, state, native, index);
        let mut s = state.borrow_mut();
        s.hotkey_errors.retain(|(i, _)| *i != index);
        s.hotkey_errors.push((index, error.to_string()));
        sync_hotkey_rows(ui, &s);
        ui.set_status(if ui.get_english() { error } else { "این ترکیب قابل استفاده نیست؛ ترکیب دیگری ضبط کن." }.into());
        return;
    }
    let mut candidate = state.borrow().settings.hotkeys.clone();
    candidate[index] = crate::settings::HotkeyBinding { enabled: true, modifiers, virtual_key: vk };
    apply_hotkeys(ui, state, native, candidate, Some(index));
}
fn apply_hotkeys(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    native: &Rc<RefCell<Option<native_tray::Tray>>>,
    candidate: Vec<crate::settings::HotkeyBinding>,
    changed_index: Option<usize>,
) -> bool {
    let previous = state.borrow().settings.hotkeys.clone();
    let prior_errors = state.borrow().hotkey_errors.clone();
    let mut native_ref = native.borrow_mut();
    let Some(tray) = native_ref.as_mut() else {
        ui.set_hotkey_capture_index(-1);
        sync_hotkey_rows(ui, &state.borrow());
        ui.set_status(if ui.get_english() { "Global shortcut window is unavailable." } else { "پنجرهٔ میان‌برها در دسترس نیست." }.into());
        return false;
    };
    let errors = tray.register_hotkeys(&candidate);
    let new_failures = new_registration_failures(&candidate, &previous, &prior_errors, &errors, changed_index);
    if !new_failures.is_empty() {
        let _ = tray.register_hotkeys(&previous);
        let mut display = prior_errors;
        if let Some(index) = changed_index {
            if let Some((_, message)) = new_failures.first() {
                if !display.iter().any(|(i, _)| *i == index) {
                    display.push((index, format!("Shortcut conflict: {message}")));
                }
            }
        }
        for failure in new_failures { if !display.iter().any(|(i, _)| *i == failure.0) { display.push(failure); } }
        state.borrow_mut().hotkey_errors = display;
        ui.set_hotkey_capture_index(-1);
        sync_hotkey_rows(ui, &state.borrow());
        ui.set_status(if ui.get_english() { "Shortcut was not saved because Windows could not register it. Choose a different combination." } else { "میان‌بر ذخیره نشد؛ ویندوز آن را ثبت نکرد. ترکیب دیگری انتخاب کن." }.into());
        return false;
    }
    {
        let mut s = state.borrow_mut();
        s.settings.hotkeys = candidate;
        s.hotkey_errors = errors;
        s.save_at = Some(Instant::now());
    }
    ui.set_hotkey_capture_index(-1);
    sync_hotkey_rows(ui, &state.borrow());
    ui.set_status(if ui.get_english() { "Keyboard shortcuts saved." } else { "کلیدهای میان‌بر ذخیره شدند." }.into());
    true
}
fn open(target: &str) {
    use windows::{
        core::PCWSTR,
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };
    let t = core_audio::wide(target);
    let verb = core_audio::wide("open");
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(t.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        log::error!("Could not open {target}");
    }
}
fn refresh(ui: &AppWindow, state: &mut State) -> Result<bool> {
    // Internal component callbacks share this boundary with the manual button.
    // Pause retains live streams, so it must not permit inventory replacement.
    if state.devices_pending || state.exiting || ui.get_running()
        || ui.get_busy() || state.components.busy {
        return Ok(false);
    }
    let submitted = state.background.submit(if ui.get_recovery_ready() {
        background::Job::Devices
    } else {
        background::Job::Initialize(state.data.clone())
    });
    if let Err(error) = submitted {
        ui.set_devices_ready(false);
        ui.set_device_refresh_state(3);
        return Err(error);
    }
    state.devices_pending = true;
    ui.set_devices_ready(false);
    ui.set_device_refresh_state(0);
    Ok(true)
}
fn request_device_refresh(ui: &AppWindow, state: &mut State) {
    match refresh(ui, state) {
        Ok(true) => {
            log_device_selection("Refresh requested", state);
            ui.set_device_refresh_state(1);
        }
        Ok(false) => (),
        Err(error) => {
            log::error!("Refresh audio devices: {error:#}");
            ui.set_device_refresh_state(3);
        }
    }
}
fn log_device_selection(context: &str, state: &State) {
    let available = |rows: &[DeviceChoice], id: &str| rows.iter()
        .find(|row| row.id == id).map(|row| row.available);
    log::info!("Audio selection ({context}): mode={}, source_id={:?}, source_available={:?}, output_intent={}, output_id={:?}, output_available={:?}, auto_route={}",
        state.settings.mode, state.settings.source_id,
        available(&state.sources, &state.settings.source_id),
        if state.settings.output_id.is_empty() { "System default" } else { "Explicit endpoint" },
        state.settings.output_id, available(&state.outputs, &state.settings.output_id),
        state.settings.auto_route);
}
fn selected_devices_available(ui: &AppWindow, state: &State) -> bool {
    let available = |rows: &[DeviceChoice], index: i32| {
        usize::try_from(index).ok().and_then(|i| rows.get(i)).is_some_and(|row| row.available)
    };
    available(&state.outputs, ui.get_output_index())
        && (ui.get_mode() != 1 || available(&state.sources, ui.get_source_index()))
}
fn sync_device_labels(ui: &AppWindow, state: &State) {
    let english = ui.get_english();
    let label = |row: &DeviceChoice, source: bool| {
        if row.id.is_empty() {
            if source {
                if state.device_inventory.iter().any(|d| d.virtual_input) {
                    if english { "Select a virtual input" } else { "ورودی مجازی را انتخاب کن" }
                } else if english { "Install VB-CABLE first" } else { "ابتدا VB-CABLE را نصب کنید" }
            } else if english { "System default" } else { "خروجی پیش‌فرض ویندوز" }.to_string()
        } else if row.available {
            row.name.clone()
        } else {
            let name = if row.name.is_empty() {
                if source { if english { "Selected virtual input" } else { "ورودی مجازی انتخاب‌شده" } }
                else if english { "Selected output" } else { "خروجی انتخاب‌شده" }
            } else { &row.name };
            format!("{} · {name}", if english { "Unavailable" } else { "در دسترس نیست" })
        }
    };
    let output_index = ui.get_output_index();
    let source_index = ui.get_source_index();
    ui.set_outputs(strings(state.outputs.iter().map(|row| label(row, false)).collect()));
    ui.set_sources(strings(state.sources.iter().map(|row| label(row, true)).collect()));
    ui.set_output_index(output_index);
    ui.set_source_index(source_index);
    sync_device_availability(ui, state);
}
fn sync_device_availability(ui: &AppWindow, state: &State) {
    let unavailable = |rows: &[DeviceChoice], index: i32| {
        usize::try_from(index).ok().and_then(|i| rows.get(i)).is_none_or(|row| !row.available)
    };
    ui.set_output_unavailable(unavailable(&state.outputs, ui.get_output_index()));
    ui.set_source_unavailable(unavailable(&state.sources, ui.get_source_index()));
}
fn apply_devices(ui: &AppWindow, state: &mut State, all: Vec<core_audio::DeviceInfo>) {
    let cable = all
        .iter()
        .any(|d| d.vb_cable);
    let mut outputs = vec![DeviceChoice { id: String::new(), name: String::new(), available: true }];
    let mut sources = Vec::new();
    for d in &all {
        let row = DeviceChoice { id: d.id.clone(), name: d.name.clone(), available: true };
        if d.virtual_input {
            sources.push(row);
        } else {
            outputs.push(row);
        }
    }
    // Only an empty source preference opts into choosing the initial cable.
    if state.settings.source_id.is_empty() {
        if let Some(id) = audio_routing::initial_source(&all) { state.settings.source_id = id; }
    }
    for (rows, previous, selected) in [
        (&mut outputs, &state.outputs, &state.settings.output_id),
        (&mut sources, &state.sources, &state.settings.source_id),
    ] {
        if !selected.is_empty() && !rows.iter().any(|row| &row.id == selected) {
            let name = previous.iter().find(|row| &row.id == selected).map(|row| row.name.clone()).unwrap_or_default();
            rows.push(DeviceChoice { id: selected.clone(), name, available: false });
        }
    }
    if sources.is_empty() || state.settings.source_id.is_empty() {
        sources.insert(0, DeviceChoice { id: String::new(), name: String::new(), available: false });
    }
    state.device_inventory = all.clone();
    state.outputs = outputs;
    state.sources = sources;
    ui.set_output_index(
        state
            .outputs
            .iter()
            .position(|row| row.id == state.settings.output_id)
            .unwrap_or(0) as i32,
    );
    ui.set_source_index(
        state
            .sources
            .iter()
            .position(|row| row.id == state.settings.source_id)
            .unwrap_or(0) as i32,
    );
    sync_device_labels(ui, state);
    ui.set_cable_installed(cable);
    ui.set_component_status(
        if cable {
            "VB-CABLE detected"
        } else {
            "VB-CABLE is required for Full boost"
        }
        .into(),
    );
    let pack = state.components.state.confirmed_pack;
    ui.set_installed_version(
        if pack > 0 && cable {
            format!("Pack{pack} (last confirmed installation)")
        } else if cable {
            "Installed · package version unknown".into()
        } else {
            "Not installed".into()
        }
        .into(),
    );
    log::info!("Refreshed {} audio outputs; VB-CABLE={cable}", all.len());
    log_device_selection("Scan completed", state);
    ui.set_devices_ready(true);
}
fn apply_device_result(ui: &AppWindow, state: &mut State, result: Result<Vec<core_audio::DeviceInfo>>) {
    let manual = ui.get_device_refresh_state() == 1;
    state.devices_pending = false;
    match result {
        Ok(devices) => {
            apply_devices(ui, state, devices);
            ui.set_device_refresh_state(if manual { 2 } else { 0 });
        }
        Err(error) => {
            // Keep the last inventory and audio/recovery error. A scan failure
            // cannot establish that remembered endpoints have disappeared.
            log::error!("Audio devices: {error:#}");
            log_device_selection("Scan failed; prior inventory/preferences retained", state);
            ui.set_devices_ready(false);
            ui.set_device_refresh_state(3);
        }
    }
}
fn apply_initialization_result(ui: &AppWindow, state: &mut State, result: Result<()>) {
    match result {
        Ok(()) => {
            ui.set_recovery_ready(true);
            if let Some(owned) = state.initialization_error.take() {
                // Clear only this operation's previous failure. A newer audio
                // or safe-exit error can replace either channel independently.
                if state.error.as_deref() == Some(owned.as_str()) { state.error = None; }
                if ui.get_audio_error().as_str() == owned {
                    state.audio_error_presentation = None;
                    ui.set_audio_error("".into());
                }
            }
        }
        Err(error) => {
            // Initialize emits no Devices event on failure, so finish the scan here.
            state.devices_pending = false;
            ui.set_devices_ready(false);
            ui.set_recovery_ready(false);
            ui.set_device_refresh_state(3);
            let message = format!("{error:#}");
            log::error!("Startup recovery: {message}");
            state.initialization_error = Some(message.clone());
            state.error = Some(message.clone());
            state.audio_error_presentation = None;
            ui.set_status(message.clone().into());
            ui.set_audio_error(message.into());
        }
    }
}
fn update_conflicts(ui: &AppWindow, state: &mut State) {
    let _ = ui;
    if state.conflicts_pending || state.exiting {
        return;
    }
    state.conflicts_pending = state.background.submit(background::Job::Conflicts).is_ok();
    state.conflict_scan_at = Instant::now();
}
fn apply_conflicts(ui: &AppWindow, state: &mut State, result: Result<Vec<conflicts::AudioApp>>) {
    match result {
        Ok(apps) => {
            let changed = apps != state.audio_apps;
            if changed {
                log::info!("Detected {} other audio processors", apps.len());
            }
            if changed {
                ui.set_conflicts(
                    Rc::new(VecModel::from(
                        apps.iter()
                            .map(|a| ConflictRow {
                                pid: a.pid as i32,
                                name: a.name.clone().into(),
                                exe: a.exe.clone().into(),
                            })
                            .collect::<Vec<_>>(),
                    ))
                    .into(),
                );
            }
            ui.set_conflict_message(
                if ui.get_english() {
                    "Running app detection · refreshes every 5 seconds."
                } else {
                    "بررسی برنامه‌های در حال اجرا؛ هر ۵ ثانیه به‌روز می‌شود."
                }
                .into(),
            );
            state.audio_apps = apps;
        }
        Err(e) => {
            log::error!("Audio processor scan: {e:#}");
            ui.set_conflict_message(format!("{e:#}").into());
        }
    }
}
fn set_eq_model(ui: &AppWindow, gains: [f32; 10]) {
    // Preserve repeater items and pointer/keyboard focus during slider drags.
    // Replacing ModelRc recreates all faders in Slint.
    let existing = ui.get_eq_bands();
    if existing.row_count() == 10 {
        for (i, gain) in gains.iter().copied().enumerate() {
            if let Some(mut row) = existing.row_data(i) {
                if row.gain != gain {
                    row.gain = gain;
                    existing.set_row_data(i, row);
                }
            }
        }
        return;
    }
    let names = [
        "31.5", "63", "125", "250", "500", "1k", "2k", "4k", "8k", "16k",
    ];
    ui.set_eq_bands(
        Rc::new(VecModel::from(
            names
                .iter()
                .zip(gains)
                .map(|(f, gain)| EqBand {
                    frequency: (*f).into(),
                    gain,
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
}
fn read_config(ui: &AppWindow, s: &mut State) {
    if s.exiting {
        return;
    }
    let max_target = if ui.get_mode() == 0 { 100 } else { 200 };
    if ui.get_target() > max_target {
        ui.invoke_show_volume_notice(
            if ui.get_english() {
                "Boost above 100 requires Full boost / virtual cable in Processing mode."
            } else {
                "برای بوست بالاتر از ۱۰۰، گزینهٔ بوست کامل / کابل مجازی را از حالت پردازش انتخاب کن."
            }
            .into(),
        );
    }
    s.settings.target = ui.get_target().clamp(0, max_target);
    ui.set_target(s.settings.target);
    s.settings.default_volume = ui.get_default_volume().clamp(0, 100);
    let language_changed = s.settings.english != ui.get_english();
    s.settings.boost_db = ui.get_boost().clamp(0, 36);
    s.settings.speed = ui.get_reaction().clamp(0, 2);
    s.settings.intensity = ui.get_intensity().clamp(0, 2);
    s.settings.mode = ui.get_mode().clamp(0, 1);
    s.settings.profile = ui.get_profile().clamp(0, 7);
    s.settings.last_profile = ui.get_last_profile().clamp(1, 7);
    s.settings.leveling = ui.get_leveling();
    s.settings.treble_smoothing = ui.get_treble_smoothing();
    s.settings.startup_active = ui.get_startup_active();
    s.settings.auto_route = ui.get_auto_route();
    s.settings.debug_log = ui.get_debug_log();
    s.settings.english = ui.get_english();
    if language_changed {
        sync_hotkey_rows(ui, s);
        refresh_audio_error_presentation(ui, s);
    }
    if let Some(row) = usize::try_from(ui.get_output_index()).ok().and_then(|i| s.outputs.get(i)) {
        s.settings.output_id = row.id.clone();
    }
    if let Some(row) = usize::try_from(ui.get_source_index()).ok().and_then(|i| s.sources.get(i)) {
        s.settings.source_id = row.id.clone();
    }
    if language_changed { sync_device_labels(ui, s); }
    else { sync_device_availability(ui, s); }
    s.engine.update(&s.settings);
    app_log::AppLogger::set_debug(s.settings.debug_log);
    s.save_at = Some(Instant::now());
}
fn prepare_audio_config(ui: &AppWindow, state: &State, resolve_default: impl FnOnce() -> Result<String>) -> Result<Settings> {
    anyhow::ensure!(!ui.get_running() && !ui.get_busy() && !state.exiting
        && !state.components.busy && !state.devices_pending && ui.get_devices_ready() && ui.get_recovery_ready(),
        if ui.get_english() { "Wait for audio devices and recovery before starting audio." }
        else { "پیش از اجرا، منتظر آماده‌شدن دستگاه‌ها و بازیابی صدا بمانید." });
    anyhow::ensure!(selected_devices_available(ui, state),
        if ui.get_english() { "The selected audio device is unavailable. Reconnect it, then refresh devices, or select another available device." }
        else { "دستگاه انتخاب‌شده در دسترس نیست. آن را وصل کنید و دستگاه‌ها را به‌روزرسانی کنید، یا دستگاه دیگری انتخاب کنید." });
    let mut config = state.settings.clone();
    if config.mode == 1 && config.output_id.is_empty() {
        let message = audio_routing::guidance(ui.get_english());
        let id = resolve_default().context(message)?;
        // Resolve only this run: the UI and saved preference remain System default.
        // Full's own routing subsequently makes the cable the Windows default.
        let output = audio_routing::resolve_output(&config, &state.device_inventory, Some(&id))?;
        // A remembered cable fallback is re-resolved from fresh hardware evidence
        // by the worker; it must not masquerade as an explicit user choice.
        if !audio_routing::selected_cable(&config, &state.device_inventory, &id) { config.output_id = output; }
    }
    Ok(config)
}
fn start_audio(ui: &AppWindow, s: &mut State) {
    read_config(ui, s);
    log_device_selection("Run requested", s);
    let config = match prepare_audio_config(ui, s, || {
        let _com = core_audio::ComScope::new()?;
        core_audio::default_id(1)
    }) {
        Ok(config) => config,
        Err(error) => {
            log::error!("Start audio: {error:#}");
            let message = format!("{error:#}");
            s.error = Some(message.clone());
            ui.set_status(message.clone().into());
            ui.set_audio_error(message.into());
            return;
        }
    };
    log::info!("Audio run configuration: mode={}, source_id={:?}, output_id={:?}, output_resolved_for_run={}, auto_route={}",
        config.mode, config.source_id, config.output_id,
        config.output_id != s.settings.output_id, config.auto_route);
    let data = s.data.clone();
    match s.engine.start(config, data) {
        Ok(()) => {
            ui.set_busy(true);
            ui.set_status("Starting audio…".into());
        }
        Err(e) => {
            log::error!("Start audio: {e:#}");
            s.error = Some(format!("{e:#}"));
            ui.set_status(format!("{e:#}").into());
        }
    }
}
fn exit(ui: &AppWindow, state: &Rc<RefCell<State>>, osd: &Rc<RefCell<Option<osd::Osd>>>) {
    let mut s = state.borrow_mut();
    if s.exiting {
        return;
    }
    if s.components.busy {
        log::info!("Exit requested during component operation; an already launched official installer remains independent");
    }
    s.app_updates.cancel();
    s.exiting = true;
    if let Some(window) = osd.borrow().as_ref() { window.hide(); }
    s.settings.was_running = false;
    ui.set_busy(true);
    // Signal restoration immediately even if a scan/save is ahead of Exit.
    s.engine.stop();
    let engine = std::mem::replace(&mut s.engine, AudioEngine::new());
    if let Err(std::sync::mpsc::SendError(background::Job::Exit(engine, _, _))) =
        s.background.jobs.send(background::Job::Exit(
            engine,
            s.settings.clone(),
            s.data.clone(),
        ))
    {
        s.engine = engine;
        s.exiting = false;
        ui.set_busy(false);
        log::error!("Background worker is unavailable; retry Exit app.");
        s.audio_error_presentation = None;
        ui.set_audio_error(if ui.get_english() {
            "Safe exit failed. Please retry Exit app."
        } else {
            "خروج امن انجام نشد. دوباره گزینهٔ خروج را بزنید."
        }.into());
    }
}

fn run() -> Result<()> {
    let preview_args: Vec<String> = std::env::args().collect();
    let popup_preview = preview_args.iter().any(|arg| arg == "--popup-preview");
    let native_preview = popup_preview || preview_args.iter().any(|arg| arg == "--native-window-preview");
    anyhow::ensure!(
        !native_preview || preview_args.iter().any(|arg| arg == "--layout-preview"),
        "--native-window-preview requires --layout-preview"
    );
    // Synthetic preview returns before production services and personal settings.
    if preview_args.iter().any(|arg| arg == "--layout-preview") {
        let ui = AppWindow::new()?;
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        window_repaint::install_focus_reveal(&ui);
        let preview_value = |flag: &str| preview_args.iter().position(|arg| arg == flag).and_then(|index| preview_args.get(index + 1));
        anyhow::ensure!(
            !native_preview || preview_value("--capture-dir").is_none(),
            "--native-window-preview is for interactive --layout-preview"
        );
        ui.set_english(preview_value("--lang").is_some_and(|lang| lang == "en"));
        if let Some(page) = preview_value("--page").and_then(|v| v.parse::<i32>().ok()) { ui.set_page(page.clamp(0, 6)); }
        if let (Some(width), Some(height)) = (preview_value("--width").and_then(|v| v.parse::<f32>().ok()), preview_value("--height").and_then(|v| v.parse::<f32>().ok())) { window_repaint::resize_logically(ui.window(), width.max(640.), height.max(320.)); }
        ui.set_devices_ready(true);
        ui.set_recovery_ready(true);
        ui.set_mode(1);
        ui.set_profile(7);
        ui.set_eq_bands(Rc::new(VecModel::from(["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"].into_iter().map(|frequency| EqBand { frequency: frequency.into(), gain: 0. }).collect::<Vec<_>>())).into());
        ui.set_outputs(Rc::new(VecModel::from(vec![slint::SharedString::from("Speakers (High Definition Audio Device)")])).into());
        ui.set_sources(Rc::new(VecModel::from(vec![slint::SharedString::from("CABLE Output (VB-Audio Virtual Cable)")])).into());
        ui.set_hotkey_rows(Rc::new(VecModel::from((0..8).map(|i| HotkeyRow { name: format!("Shortcut {i} / میان‌بر صدا").into(), chord: "Ctrl + Alt + F12".into(), enabled: true, error: "".into() }).collect::<Vec<_>>())).into());
        ui.on_exit_app(|| { let _ = slint::quit_event_loop(); });
        // An explicit, in-memory popup fixture. It never constructs production
        // State, starts audio/update/network workers, or loads personal settings.
        let popup_trace = if popup_preview {
            let directory = PathBuf::from(preview_value("--popup-trace-dir").context("--popup-preview requires --popup-trace-dir")?);
            fs::create_dir_all(&directory)?;
            ui.set_mode(if preview_value("--preview-mode").is_some_and(|mode| mode == "windows") { 0 } else { 1 });
            ui.set_target(preview_value("--preview-target").and_then(|value| value.parse::<i32>().ok()).unwrap_or(50).clamp(0, if ui.get_mode() == 0 { 100 } else { 200 }));
            ui.set_running(preview_args.iter().any(|arg| arg == "--preview-running"));
            ui.set_paused(ui.get_running() && preview_args.iter().any(|arg| arg == "--preview-paused"));
            Some(fs::OpenOptions::new().write(true).create_new(true).open(directory.join("popup-preview.jsonl"))?)
        } else { None };
        if preview_args.iter().any(|arg| arg == "--app-update-preview") {
            // Isolated visual fixture: this early-return branch never starts update,
            // settings, audio, registry, or network workers.
            ui.set_page(6);
            ui.set_app_latest_version("v1.0.4".into());
            ui.set_app_update_installable(true);
            ui.on_open_app_release_notes(|| {});
            ui.set_app_update_state(preview_value("--update-state").and_then(|v| v.parse::<i32>().ok()).unwrap_or(3).clamp(0, 11));
            ui.set_app_release_notes_ready(![0, 1, 4].contains(&ui.get_app_update_state()));
            ui.set_app_update_error_kind(preview_value("--update-error").and_then(|v| v.parse::<i32>().ok()).unwrap_or(1).clamp(0, 7));
            ui.set_app_update_progress(0.42);
            let weak = ui.as_weak();
            ui.on_check_app_update(move || { if let Some(ui) = weak.upgrade() { ui.set_app_update_state(3); ui.set_app_release_notes_ready(true); ui.set_app_update_cancelling(false); } });
            let weak = ui.as_weak();
            ui.on_install_app_update(move || { if let Some(ui) = weak.upgrade() { ui.set_app_update_state(5); ui.set_app_update_cancelling(false); } });
            let weak = ui.as_weak();
            ui.on_cancel_app_update(move || { if let Some(ui) = weak.upgrade() { ui.set_app_update_state(8); ui.set_app_update_progress(0.0); } });
        }
        if let Some(directory) = preview_value("--update-capture-dir") {
            anyhow::ensure!(preview_args.iter().any(|arg| arg == "--app-update-preview"), "Update captures require the isolated update preview");
            let directory = PathBuf::from(directory);
            fs::create_dir_all(&directory)?;
            let cases = [(3, 0, false), (5, 0, false), (5, 0, true), (8, 0, false), (2, 0, false), (4, 3, false), (4, 4, false), (4, 5, false), (4, 7, false), (7, 0, false), (9, 0, false), (10, 0, false), (11, 0, false)];
            let step = Rc::new(std::cell::Cell::new(0usize));
            let capture_error = Rc::new(RefCell::new(None));
            let timer_error = capture_error.clone();
            let weak = ui.as_weak();
            let capture_timer = slint::Timer::default();
            capture_timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
                let Some(ui) = weak.upgrade() else { return; };
                let index = step.get();
                if index > 0 {
                    let previous = index - 1;
                    let (state, error, cancelling) = cases[previous % cases.len()];
                    let language = if previous < cases.len() { "fa" } else { "en" };
                    let name = format!("{language}-state-{state}-error-{error}-cancel-{cancelling}.png");
                    if let Err(error) = snapshot_png(&ui, &directory.join(name)) { *timer_error.borrow_mut() = Some(error); let _ = slint::quit_event_loop(); return; }
                }
                if index == cases.len() * 2 { let _ = slint::quit_event_loop(); return; }
                let (state, error, cancelling) = cases[index % cases.len()];
                ui.set_english(index >= cases.len());
                ui.set_app_update_state(state);
                ui.set_app_release_notes_ready(![0, 1, 4].contains(&state));
                ui.set_app_update_error_kind(error);
                ui.set_app_update_cancelling(cancelling);
                ui.set_app_update_progress(if state == 5 { 0.42 } else { 0.0 });
                ui.set_app_update_installable(![2, 9, 10, 11].contains(&state));
                step.set(index + 1);
            });
            ui.show()?;
            window_repaint::after_show(&ui);
            slint::run_event_loop()?;
            if let Some(error) = capture_error.borrow_mut().take() { return Err(error); }
            return Ok(());
        }
        if let Some(directory) = preview_value("--capture-dir") {
            let directory = PathBuf::from(directory);
            fs::create_dir_all(&directory)?;
            ui.set_logs(Rc::new(VecModel::from(vec![LogRow { time: "12:34:56".into(), level: "INFO".into(), message: "Synthetic preview event / رویداد آزمایشی".into() }])).into());
            let weak = ui.as_weak();
            let capture_timer = slint::Timer::default();
            let step = Rc::new(std::cell::Cell::new(0usize));
            let capture_error = Rc::new(RefCell::new(None));
            let timer_error = capture_error.clone();
            capture_timer.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
                let Some(ui) = weak.upgrade() else { return; };
                let index = step.get();
                // Preserve the original five captures and add reachable small-window evidence.
                let cases = [(0, true, false, 0.), (0, false, false, 0.), (2, true, false, 0.), (4, true, false, 0.), (6, true, false, 0.), (0, true, true, 0.), (0, false, true, 0.), (4, true, false, 200.), (4, true, false, 400.)];
                if index > 0 {
                    let (_, _, sidebar_end, settings_offset) = cases[index - 1];
                    if sidebar_end { ui.invoke_scroll_sidebar_to_end(); }
                    ui.invoke_scroll_settings_to(settings_offset);
                    if let Err(error) = snapshot_png(&ui, &directory.join(format!("preview-{}.png", index - 1))) {
                        *timer_error.borrow_mut() = Some(error);
                        let _ = slint::quit_event_loop();
                        return;
                    }
                }
                if index < cases.len() {
                    let (page, expanded, _, _) = cases[index];
                    ui.set_page(page);
                    ui.set_sidebar_expanded(expanded);
                }
                if index == cases.len() { let _ = slint::quit_event_loop(); }
                step.set(index + 1);
            });
            ui.show()?;
            window_repaint::after_show(&ui);
            slint::run_event_loop()?;
            if let Some(error) = capture_error.borrow_mut().take() { return Err(error); }
            return Ok(());
        }
        let weak = ui.as_weak();
        ui.on_eq_band_changed(move |index, gain| { if let Some(ui) = weak.upgrade() { let model = ui.get_eq_bands(); if let Some(mut band) = model.row_data(index as usize) { band.gain = gain; model.set_row_data(index as usize, band); } } });
        ui.show()?;
        window_repaint::after_show(&ui);
        if preview_args.iter().any(|arg| arg == "--menu") { ui.invoke_toggle_sidebar(); }
        // Install the same native window notification subclass as production,
        // once Winit has created its HWND. Keep only window lifecycle actions.
        let preview_tray: Rc<RefCell<Option<native_tray::Tray>>> = Rc::new(RefCell::new(None));
        let preview_osd: Rc<RefCell<Option<osd::Osd>>> = Rc::new(RefCell::new(None));
        let preview_error: Rc<RefCell<Option<anyhow::Error>>> = Rc::new(RefCell::new(None));
        let preview_timer = slint::Timer::default();
        if popup_preview {
            let tray = preview_tray.clone();
            ui.window().on_close_requested(move || {
                if tray.borrow().as_ref().is_some_and(|tray| tray.is_registered()) { slint::CloseRequestResponse::HideWindow }
                else { slint::CloseRequestResponse::KeepWindowShown }
            });
        }
        if native_preview {
            let weak = ui.as_weak();
            let tray = preview_tray.clone();
            let osd = preview_osd.clone();
            let error = preview_error.clone();
            let mut trace = popup_trace;
            let mut unmute_target = None;
            let started = Instant::now();
            preview_timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
                let Some(ui) = weak.upgrade() else { return; };
                if tray.borrow().is_none() {
                    match native_tray::Tray::new() {
                        Ok(mut value) => {
                            if popup_preview {
                                // Dedicated temporary test chords; no default media
                                // shortcuts or user-persisted bindings are registered.
                                let bindings: Vec<_> = [0x77, 0x78, 0x79].into_iter().map(|virtual_key| settings::HotkeyBinding { enabled: true, modifiers: 0x0007, virtual_key }).collect();
                                let failures = value.register_hotkeys(&bindings);
                                if !failures.is_empty() {
                                    *error.borrow_mut() = Some(anyhow::anyhow!("Popup preview shortcut registration failed: {failures:?}"));
                                    let _ = slint::quit_event_loop(); return;
                                }
                            }
                            *tray.borrow_mut() = Some(value);
                        }
                        Err(e) => {
                            *error.borrow_mut() = Some(e);
                            let _ = slint::quit_event_loop();
                            return;
                        }
                    }
                }
                if popup_preview {
                    let target = ui.get_target().clamp(0, if ui.get_mode() == 0 { 100 } else { 200 });
                    if target != ui.get_target() { ui.set_target(target); }
                    if let Some(tray) = tray.borrow_mut().as_mut() {
                        tray.update(ui.get_running(), ui.get_paused(), ui.get_target(), ui.get_english(), ui.get_mode() == 0);
                        tray.set_scroll_enabled(!ui.get_busy());
                    }
                }
                let actions: Vec<_> = tray.borrow().as_ref()
                    .and_then(|value| value.events.as_ref())
                    .map(|events| events.try_iter().collect()).unwrap_or_default();
                for action in actions {
                    let action_name = format!("{action:?}");
                    let before = (ui.get_target(), ui.get_running(), ui.get_paused(), ui.window().is_visible());
                    let mut should_exit = false;
                    match action {
                        native_tray::Action::Show => show(&ui),
                        native_tray::Action::Repaint => {
                            if ui.window().is_visible() { window_repaint::full_redraw(ui.window()); }
                        }
                        native_tray::Action::FitWindow => {
                            if ui.window().is_visible() { window_repaint::fit_native_work_area(); }
                        }
                        native_tray::Action::ScrollTarget { steps, x, y, windows_mode } if popup_preview && windows_mode == (ui.get_mode() == 0) => {
                            ui.set_target(target_after_wheel(ui.get_target(), steps, ui.get_mode()));
                            unmute_target = None;
                            notify_target(&ui, &osd, Some((x, y)));
                        }
                        native_tray::Action::Target(target) if popup_preview => {
                            ui.set_target(target.clamp(0, if ui.get_mode() == 0 { 100 } else { 200 }));
                            unmute_target = None;
                        }
                        native_tray::Action::Pause if popup_preview && ui.get_running() => {
                            ui.set_paused(!ui.get_paused());
                        }
                        native_tray::Action::Start if popup_preview && !ui.get_running() => {
                            ui.set_running(true);
                            ui.set_paused(false);
                        }
                        native_tray::Action::Hotkey(index) if popup_preview && index < 3 => {
                            if index == 2 {
                                if ui.get_target() > 0 { unmute_target = Some(ui.get_target()); ui.set_target(0); }
                                else { ui.set_target(unmute_target.take().unwrap_or(50).clamp(0, if ui.get_mode() == 0 { 100 } else { 200 })); }
                            } else {
                                let current = if ui.get_target() == 0 && index == 0 { unmute_target.take().unwrap_or(50) } else { ui.get_target() };
                                ui.set_target((current + if index == 0 { 5 } else { -5 }).clamp(0, if ui.get_mode() == 0 { 100 } else { 200 }));
                            }
                            notify_target(&ui, &osd, None);
                        }
                        native_tray::Action::Refresh if popup_preview => {
                            if let Some(tray) = tray.borrow_mut().as_mut() { let _ = tray.register(); }
                        }
                        native_tray::Action::Exit => { should_exit = true; }
                        _ => {} // No preview action dispatches production services.
                    }
                    if let Some(trace) = trace.as_mut() {
                        use std::io::Write;
                        let after = (ui.get_target(), ui.get_running(), ui.get_paused(), ui.window().is_visible());
                        let record = serde_json::json!({"elapsed_ms":started.elapsed().as_millis(),"action":action_name,"synthetic_audio_state":true,"before":before,"after":after,"english":ui.get_english(),"mode":ui.get_mode(),"notice":ui.get_volume_notice().as_str()});
                        if let Err(failure) = writeln!(trace, "{record}") {
                            *error.borrow_mut() = Some(failure.into()); let _ = slint::quit_event_loop(); return;
                        }
                    }
                    if should_exit { ui.invoke_exit_app(); return; }
                }
            });
        }
        let result = slint::run_event_loop();
        preview_timer.stop();
        preview_osd.borrow_mut().take();
        preview_tray.borrow_mut().take();
        if let Some(error) = preview_error.borrow_mut().take() { return Err(error); }
        result?;
        return Ok(());
    }
    let args: Vec<_> = std::env::args().collect();
    let update_health = update_handoff::HealthProbe::from_args(&args, env!("CARGO_PKG_VERSION"))?;
    if args.get(1).is_some_and(|arg| matches!(arg.as_str(), "--ui-smoke" | "--startup-instance-idle-smoke")) {
        // Keep the diagnostic harness from opening a blocking Windows crash dialog.
        #[link(name = "kernel32")]
        unsafe extern "system" { fn SetErrorMode(mode: u32) -> u32; }
        unsafe { SetErrorMode(0x0002); }
    }
    let data = if args.get(1).is_some_and(|a| {
        [
            "--diagnose",
            "--components-check",
            "--engine-smoke",
            "--ui-smoke",
            "--startup-hotkey-smoke",
            "--startup-osd-smoke",
            "--startup-both-smoke",
            "--startup-instance-smoke",
            "--startup-instance-idle-smoke",
        ]
        .contains(&a.as_str())
    }) {
        args.get(2)
            .map(PathBuf::from)
            .unwrap_or(settings::data_directory()?.join("Verification"))
    } else {
        settings::data_directory()?
    };
    fs::create_dir_all(&data).context("Run Maxi from a writable folder")?;
    let settings = Settings::load(&data);
    app_log::AppLogger::init(&data, settings.debug_log)?;
    std::panic::set_hook(Box::new(|info| {
        log::error!(
            "Panic: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        log::logger().flush();
    }));
    log::info!(
        "MAXI SOUNDSET {} · Rust · Slint 1.18.0 started; embedded Vazir interface font",
        env!("CARGO_PKG_VERSION")
    );
    match args.get(1).map(String::as_str) {
        Some("--configure-startup") => {
            startup::apply(settings.startup, &std::env::current_exe()?)?;
            fs::write(
                data.join("startup-check.txt"),
                format!(
                    "PASS: Windows startup={}\nExecutable: {}\n",
                    settings.startup,
                    std::env::current_exe()?.display()
                ),
            )?;
            return Ok(());
        }
        Some("--diagnose") => {
            let report = core_audio::diagnose()?;
            fs::write(data.join("diagnose.txt"), report)?;
            return Ok(());
        }
        Some("--components-check") => {
            fs::write(
                data.join("components-check.txt"),
                components::check_download(&data)?,
            )?;
            return Ok(());
        }
        Some("--engine-smoke") => {
            engine_smoke(&data)?;
            return Ok(());
        }
        _ => (),
    }
    let smoke = args.get(1).is_some_and(|a| a == "--ui-smoke");
    let startup_hotkey_smoke = args.get(1).is_some_and(|a| matches!(a.as_str(), "--startup-hotkey-smoke" | "--startup-both-smoke"));
    let startup_instance_smoke = args.get(1).is_some_and(|a| a == "--startup-instance-smoke");
    let startup_instance_idle_smoke = args.get(1).is_some_and(|a| a == "--startup-instance-idle-smoke");
    let startup_osd_smoke = args.get(1).is_some_and(|a| matches!(a.as_str(), "--startup-osd-smoke" | "--startup-both-smoke" | "--startup-instance-smoke"));
    let startup_hotkey_smoke = startup_hotkey_smoke || startup_instance_smoke || startup_instance_idle_smoke;
    let startup_smoke = startup_hotkey_smoke || startup_osd_smoke || startup_instance_idle_smoke;
    let startup_safe_probe = cfg!(debug_assertions) && std::env::var_os("MAXI_SOUNDSET_STARTUP_SAFE_PROBE").is_some();
    let startup_probe_mode = startup_smoke || startup_safe_probe;
    let startup_launch = args.get(1).is_some_and(|a| a == "--startup");
    let instance = if smoke || (startup_smoke && !(startup_instance_smoke || startup_instance_idle_smoke)) { None } else { Some(Instance::new()?) };
    if instance.as_ref().is_some_and(|i| !i.owner) {
        return Ok(());
    }
    let ui = AppWindow::new()?;
    window_repaint::install_focus_reveal(&ui);
    set_eq_model(&ui, settings.eq_bands);
    ui.set_target(settings.default_volume);
    ui.set_default_volume(settings.default_volume);
    ui.set_boost(settings.boost_db);
    ui.set_reaction(settings.speed);
    ui.set_intensity(settings.intensity);
    ui.set_mode(settings.mode);
    ui.set_profile(settings.profile);
    ui.set_last_profile(settings.last_profile);
    ui.set_leveling(settings.leveling);
    ui.set_treble_smoothing(settings.treble_smoothing);
    ui.set_startup(settings.startup);
    ui.set_startup_active(settings.startup_active);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_rust_version(
        env!("MAXI_RUST_VERSION")
            .split_whitespace()
            .nth(1)
            .unwrap_or("unknown")
            .into(),
    );
    ui.set_dependency_versions(include_str!(concat!(env!("OUT_DIR"), "/dependencies.txt")).into());
    ui.set_auto_route(settings.auto_route);
    ui.set_debug_log(settings.debug_log);
    ui.set_english(settings.english);
    ui.set_log_folder(data.join("Logs").display().to_string().into());
    if update_health.is_some() {
        // Keep the upgrade outcome visible after the new or restored app starts.
        ui.set_page(6);
        ui.set_app_update_state(7);
    }
    let state = Rc::new(RefCell::new(State {
        background: background::Background::new(),
        devices_pending: true,
        conflicts_pending: false,
        exiting: false,
        components: components::Manager::new(&data),
        app_updates: app_update::Manager::new(),
        update_handoff_result: None,
        update_health,
        settings,
        data: data.clone(),
        engine: AudioEngine::new(),
        outputs: vec![],
        sources: vec![],
        device_inventory: vec![],
        history: VecDeque::new(),
        save_at: None,
        error: None,
        initialization_error: None,
        audio_error_presentation: None,
        audio_apps: vec![],
        conflict_scan_at: Instant::now(),
        unmute_target: None,
        hotkey_errors: Vec::new(),
        hotkey_pending_audio: None,
    }));
    let osd: Rc<RefCell<Option<osd::Osd>>> = Rc::new(RefCell::new(None));

    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_startup_changed(move || {
            if let Some(u) = w.upgrade() {
                if s.borrow().exiting {
                    return;
                }
                let enabled = u.get_startup();
                if smoke {
                    s.borrow_mut().settings.startup = enabled;
                    s.borrow_mut().save_at = Some(Instant::now());
                    u.set_startup_message("Verification: no Windows registration changed.".into());
                    return;
                }
                if let Ok(exe) = std::env::current_exe() {
                    if let Err(e) = s
                        .borrow()
                        .background
                        .submit(background::Job::Startup(enabled, exe))
                    {
                        u.set_startup(!enabled);
                        u.set_startup_message(format!("{e:#}").into());
                    }
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_refresh_conflicts(move || {
            if let Some(u) = w.upgrade() {
                update_conflicts(&u, &mut s.borrow_mut());
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_end_conflict(move |pid| {
            if let Some(u) = w.upgrade() {
                if smoke {
                    u.set_conflict_message(
                        "Verification: ending host applications is disabled.".into(),
                    );
                    return;
                }
                let mut s = s.borrow_mut();

                let result = s
                    .audio_apps
                    .iter()
                    .find(|a| a.pid == pid as u32)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("App is no longer listed; refresh first"))
                    .and_then(|app| conflicts::end(&app));
                match result {
                    Ok(()) => update_conflicts(&u, &mut s),
                    Err(e) => {
                        log::error!("End audio app: {e:#}");
                        u.set_conflict_message(format!("{e:#}").into());
                    }
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_eq_band_changed(move |index, gain| {
            if let Some(u) = w.upgrade() {
                if (0..10).contains(&index) && gain.is_finite() {
                    let mut s = s.borrow_mut();
                    let gain = (gain.clamp(-12., 12.) * 10.).round() / 10.;
                    if s.settings.eq_bands[index as usize] == gain {
                        return;
                    }
                    s.settings.eq_bands[index as usize] = gain;
                    log::info!(
                        "Custom EQ: {} Hz = {:+.1} dB",
                        enhancer::EQ_FREQUENCIES[index as usize],
                        gain
                    );
                    set_eq_model(&u, s.settings.eq_bands);
                    s.engine.update(&s.settings);
                    s.save_at = Some(Instant::now());
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_reset_equalizer(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                s.settings.eq_bands = [0.; 10];
                log::info!("Custom EQ reset to flat");
                set_eq_model(&u, s.settings.eq_bands);
                s.engine.update(&s.settings);
                s.save_at = Some(Instant::now());
            }
        });
    }
    update_conflicts(&ui, &mut state.borrow_mut());
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_config_changed(move || {
            if let Some(u) = w.upgrade() {
                read_config(&u, &mut s.borrow_mut());
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_refresh_devices(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                request_device_refresh(&u, &mut s);
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_toggle_audio(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                if !audio_toggle_allowed(&u, &s) {
                    return;
                }
                clear_audio_error(&u, &mut s);
                if u.get_running() {
                    s.engine.stop();
                    u.set_busy(true);
                    u.set_status("Stopping and restoring audio…".into());
                } else {
                    start_audio(&u, &mut s);
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_toggle_pause(move || {
            if let Some(u) = w.upgrade() {
                if u.get_running() && !u.get_busy() {
                    u.set_paused(s.borrow().engine.pause());
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_install_components(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                if s.components.busy { return; }
                if u.get_running() || u.get_busy() {
                    u.set_status("Stop audio before installing or updating a driver.".into());
                    return;
                }
                let data = s.data.clone();
                s.components.start(data, true);
                u.set_components_busy(true);
                u.set_component_check_state(0);
                u.set_components_installing(true);
                u.set_component_message("".into());
                u.set_download_progress(0.0);
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_download_components_again(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                if s.components.busy || u.get_running() || u.get_busy() { return; }
                let data = s.data.clone();
                s.components.start_action(data, false, true);
                u.set_components_busy(true);
                u.set_components_installing(true);
                u.set_component_check_state(0);
                u.set_download_progress(0.0);
                u.set_component_message(if u.get_english() { "Downloading a fresh official package; setup will not launch." } else { "در حال دریافت دوبارهٔ بستهٔ رسمی؛ نصاب اجرا نمی‌شود." }.into());
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_check_updates(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                if s.components.busy { return; }
                let data = s.data.clone();
                s.components.start(data, false);
                begin_component_check(&u);
            }
        });
    }
    {
        let p = data.join("Logs");
        ui.on_open_log_folder(move || open(&p.to_string_lossy()));
    }
    {
        let p = data.join("Logs/maxi.log");
        ui.on_open_log_file(move || open(&p.to_string_lossy()));
    }
    ui.on_open_link(|url| {
        if [
            "https://vb-audio.com/Cable/",
            "https://slint.dev",
            "https://www.rust-lang.org/",
            "https://MaxiSpace.dev/",
            "https://maxispace.dev/donate",
            "https://github.com/MaxiSpace/MaxiSoundSet/issues",
            app_update::RELEASES_URL,
        ]
        .contains(&url.as_str())
        {
            open(&url);
        }
    });
    ui.on_open_licenses(|| {
        let license = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|folder| folder.join("LICENSE.txt")));
        match license {
            Some(path) if path.is_file() => open(&path.to_string_lossy()),
            Some(path) => log::error!("License file was not found: {}", path.display()),
            None => log::error!("Could not locate the application license file"),
        }
    });
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_open_app_release_notes(move || {
            let Some(u) = w.upgrade() else { return; };
            if smoke || !u.get_app_release_notes_ready() { return; }
            if let Some(url) = s.borrow().app_updates.release_notes_url() { open(&url); }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_check_app_update(move || {
            if let Some(u) = w.upgrade() {
                if smoke {
                    u.set_app_update_state(2);
                    u.set_app_latest_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
                    return;
                }
                let mut s = s.borrow_mut();
                if !s.exiting && !s.app_updates.busy && s.update_handoff_result.is_none() && u.get_app_update_state() != 7 {
                    s.app_updates.start(env!("CARGO_PKG_VERSION"));
                    u.set_app_update_state(1);
                    u.set_app_latest_version("".into());
                    u.set_app_update_progress(0.0);
                    u.set_app_update_installable(false);
                    u.set_app_release_notes_ready(false);
                    u.set_app_update_cancelling(false);
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_install_app_update(move || {
            let Some(u) = w.upgrade() else { return; };
            let mut s = s.borrow_mut();
            if smoke || s.exiting || s.app_updates.busy || s.update_handoff_result.is_some() || !u.get_app_update_installable() || u.get_app_update_state() == 7 { return; }
            if !u.get_recovery_ready() || !u.get_devices_ready() || s.error.is_some() || s.components.busy {
                app_update_error(&u, "UPD_RECOVERY: audio initialization or recovery is not ready");
                return;
            }
            match s.app_updates.download() {
                Ok(()) => {
                    u.set_app_update_progress(0.0);
                    u.set_app_update_cancelling(false);
                    u.set_app_update_state(5);
                }
                Err(error) => app_update_error(&u, &format!("{error:#}")),
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_cancel_app_update(move || {
            let Some(u) = w.upgrade() else { return; };
            let mut s = s.borrow_mut();
            if s.app_updates.busy && s.update_handoff_result.is_none() && u.get_app_update_state() != 7 {
                s.app_updates.cancel();
                u.set_app_update_cancelling(true);
            }
        });
    }
    ui.on_clear_log_view(app_log::AppLogger::clear_view);
    {
        let w = ui.as_weak();
        let s = state.clone();
        let osd = osd.clone();
        ui.on_exit_app(move || {
            if let Some(u) = w.upgrade() {
                exit(&u, &s, &osd);
            }
        });
    }
    ui.show()?;
    window_repaint::after_show(&ui);
    state
        .borrow()
        .background
        .submit(background::Job::Initialize(data.clone()))?;
    if !smoke && !startup_probe_mode && state.borrow().update_health.is_none() {
        state.borrow().background.submit(background::Job::Startup(
            state.borrow().settings.startup,
            std::env::current_exe()?,
        ))?;
    }
    // Winit creates its HWND when the event loop starts.
    let native: Rc<RefCell<Option<native_tray::Tray>>> = Rc::new(RefCell::new(None));
    sync_hotkey_rows(&ui, &state.borrow());
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let native = native.clone();
        ui.on_begin_hotkey_capture(move |index| {
            let Some(u) = weak.upgrade() else { return; };
            if index >= 0 {
                let mut tray_ref = native.borrow_mut();
                let Some(tray) = tray_ref.as_mut() else {
                    u.set_hotkey_capture_index(-1);
                    u.set_status(if u.get_english() { "Global shortcut window is not ready yet." } else { "پنجرهٔ میان‌بر هنوز آماده نیست." }.into());
                    return;
                };
                tray.begin_hotkey_capture(index as usize);
                u.set_status(if u.get_english() { "Press the shortcut now. Escape cancels." } else { "میان‌بر را فشار بده؛ کلید Escape لغو می‌کند." }.into());
            } else {
                let bindings = state.borrow().settings.hotkeys.clone();
                let errors = native.borrow_mut().as_mut().map(|tray| tray.register_hotkeys(&bindings)).unwrap_or_default();
                state.borrow_mut().hotkey_errors = errors;
                u.set_hotkey_capture_index(-1);
                sync_hotkey_rows(&u, &state.borrow());
                u.set_status(if u.get_english() { "Shortcut recording cancelled." } else { "ضبط میان‌بر لغو شد." }.into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let native = native.clone();
        ui.on_set_hotkey_enabled(move |index, enabled| {
            if !(0..crate::settings::HOTKEY_ACTION_COUNT as i32).contains(&index) { return; }
            let Some(u) = weak.upgrade() else { return; };
            let mut candidate = state.borrow().settings.hotkeys.clone();
            candidate[index as usize].enabled = enabled;
            apply_hotkeys(&u, &state, &native, candidate, Some(index as usize));
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let native = native.clone();
        ui.on_clear_hotkey(move |index| {
            if !(0..crate::settings::HOTKEY_ACTION_COUNT as i32).contains(&index) { return; }
            let Some(u) = weak.upgrade() else { return; };
            let mut candidate = state.borrow().settings.hotkeys.clone();
            candidate[index as usize].enabled = false;
            apply_hotkeys(&u, &state, &native, candidate, Some(index as usize));
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let native = native.clone();
        ui.on_reset_hotkeys(move || {
            let Some(u) = weak.upgrade() else { return; };
            apply_hotkeys(&u, &state, &native, crate::settings::default_hotkeys(), None);
        });
    }
    {
        let native = native.clone();
        let smoke = smoke;
        ui.window().on_close_requested(move || {
            if smoke || native
                .borrow()
                .as_ref()
                .is_some_and(|tray| tray.is_registered())
            {
                log::info!("Window closed: continuing in system tray");
                slint::CloseRequestResponse::HideWindow
            } else {
                slint::CloseRequestResponse::KeepWindowShown
            }
        });
    }
    let startup_hidden = Rc::new(std::cell::Cell::new(false));
    let startup_resume = Rc::new(Cell::new(
        startup_launch
            && state.borrow().settings.startup
            && state.borrow().settings.startup_active
            && state.borrow().settings.was_running,
    ));
    let timer = slint::Timer::default();
    let startup_smoke_started = Instant::now();
    let startup_smoke_finished = Rc::new(Cell::new(false));
    let startup_smoke_probe = Rc::new(Cell::new((false, false, false)));
    let startup_instance_active = instance.is_some();
    let logs = Rc::new(VecModel::<LogRow>::default());
    ui.set_logs(logs.clone().into());
    let log_revision = std::cell::Cell::new(u64::MAX);
    let tray_retry = std::cell::Cell::new(Instant::now() - Duration::from_secs(5));
    let audio_diagnostics = RefCell::new(audio_engine::DiagnosticTransitions::default());
    {
        let w = ui.as_weak();
        let native = native.clone();
        let startup_hidden = startup_hidden.clone();
        let startup_resume = startup_resume.clone();
        let s = state.clone();
        let osd = osd.clone();
        let startup_smoke_finished = startup_smoke_finished.clone();
        let startup_smoke_probe = startup_smoke_probe.clone();
        let startup_safe_probe = startup_safe_probe;
        let data = data.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                let Some(u) = w.upgrade() else {
                    return;
                };
                if startup_probe_mode && !startup_smoke_finished.get() && startup_smoke_started.elapsed() >= Duration::from_secs(20) {
                    startup_smoke_finished.set(true);
                    let osd_ready = !startup_osd_smoke || osd.borrow().is_some();
                    let (shell_callback, hotkey_callback, osd_visible) = startup_smoke_probe.get();
                    let passed = shell_callback && hotkey_callback && osd_ready && (!startup_safe_probe || osd_visible);
                    let status = if passed { "PASS" } else { "FAIL" };
                    log::info!("STARTUP SAFE PROBE {status}: instance={startup_instance_active}, hotkeys={}, OSD={}, shell callback={shell_callback}, hotkey callback={hotkey_callback}, OSD visible={osd_visible}, UI timer responsive for 20 seconds", startup_hotkey_smoke || startup_safe_probe, startup_osd_smoke || startup_safe_probe);
                    let _ = fs::write(data.join("startup-smoke.txt"), format!("{status}: instance={startup_instance_active}, hotkeys={}, OSD={}, shell callback={shell_callback}, hotkey callback={hotkey_callback}, OSD visible={osd_visible}, UI timer responsive for 20 seconds\n", startup_hotkey_smoke || startup_safe_probe, startup_osd_smoke || startup_safe_probe));
                    u.invoke_exit_app();
                }
                if native.borrow().is_none() && tray_retry.get().elapsed() >= Duration::from_secs(5) {
                    tray_retry.set(Instant::now());
                    match native_tray::Tray::new() {
                        Ok(mut tray) => {
                            let failures = if (smoke || startup_smoke) && !startup_hotkey_smoke { Vec::new() } else { tray.register_hotkeys(&s.borrow().settings.hotkeys) };
                            for (index, error) in &failures { log::error!("Global shortcut {index} registration failed: {error}"); }
                            if startup_probe_mode {
                                let (shell_callback, hotkey_callback) = tray.probe_callback_messages(startup_hotkey_smoke);
                                startup_smoke_probe.set((shell_callback, hotkey_callback, false));
                                log::info!("Startup callback probe: shell={shell_callback}, hotkey={hotkey_callback}");
                            }
                            s.borrow_mut().hotkey_errors = failures;
                            sync_hotkey_rows(&u, &s.borrow());
                    if startup_osd_smoke || startup_safe_probe {
                                match osd::Osd::new() {
                                    Ok(window) => {
                                        if startup_probe_mode && (startup_osd_smoke || startup_safe_probe) {
                                            window.show("Startup verification", "Temporary isolated launch", false);
                                            let visible = window.query_visibility(Duration::from_secs(1)) == Some(true);
                                            let (shell, hotkey, _) = startup_smoke_probe.get();
                                            startup_smoke_probe.set((shell, hotkey, visible));
                                            log::info!("Startup OSD probe: HWND visible={visible}");
                                        }
                                        log::debug!("Background OSD initialized on its message thread");
                                        *osd.borrow_mut() = Some(window);
                                    },
                                    Err(e) => log::error!("Background OSD initialization: {e:#}"),
                                }
                            }
                            *native.borrow_mut() = Some(tray);
                        },
                        Err(e) => {
                            log::error!("Native tray initialization: {e:#}");
                            u.set_status(format!("{e:#}").into());
                        }
                    }
                }
                if tray_retry.get().elapsed() >= Duration::from_secs(5) {
                    if let Some(tray) = native.borrow_mut().as_mut() {
                        if !tray.is_registered() { tray_retry.set(Instant::now()); if let Err(e) = tray.register() { log::warn!("Retry tray registration: {e:#}"); } }
                    }
                }
                if startup_launch && native.borrow().as_ref().is_some_and(|tray| tray.is_registered()) && !startup_hidden.replace(true) {
                    let _ = u.hide();
                    log::info!("Windows logon launch: running in background");
                }
                if let Some(tray) = native.borrow_mut().as_mut() {
                    tray.set_scroll_enabled(tray_scroll_allowed(&u, &s.borrow()));
                }
                let actions: Vec<_> = native.borrow().as_ref().and_then(|tray| tray.events.as_ref()).map(|events| events.try_iter().collect()).unwrap_or_default();
                for action in actions {
                    match action {
                        native_tray::Action::Show => show(&u),
                        native_tray::Action::Repaint => { if u.window().is_visible() { window_repaint::full_redraw(u.window()); } }
                        native_tray::Action::FitWindow => { if u.window().is_visible() { window_repaint::fit_native_work_area(); } }
                        native_tray::Action::Pause => u.invoke_toggle_pause(),
                        native_tray::Action::Start => handle_tray_start(&u, &s),
                        native_tray::Action::Hotkey(index) => handle_hotkey(&u, &s, &osd, index),
                        native_tray::Action::ScrollTarget { steps, x, y, windows_mode } => handle_tray_scroll(&u, &s, &osd, windows_mode, steps, (x, y)),
                        native_tray::Action::CapturedHotkey(index, vk, modifiers) => accept_captured_hotkey(&u, &s, &native, index, vk, modifiers),
                        native_tray::Action::CaptureCancelled(index) => cancel_capture(&u, &s, &native, index),
                        native_tray::Action::Target(v) => {
                            if tray_scroll_allowed(&u, &s.borrow()) {
                                u.set_target(v);
                                s.borrow_mut().unmute_target = None;
                                u.invoke_config_changed();
                            }
                        }
                        native_tray::Action::Exit => {
                            // The installer may repost Exit while waiting. Keep draining
                            // the background result after the first request starts shutdown.
                            let already_exiting = s.borrow().exiting;
                            if !already_exiting {
                                u.invoke_exit_app();
                                return;
                            }
                        }
                        native_tray::Action::Refresh => {
                            if let Err(e) = native.borrow_mut().as_mut().unwrap().register() {
                                log::error!("Recreating tray: {e:#}");
                            }
                        }
                    }
                }
                let mut s = s.borrow_mut();
                let events: Vec<_> = s.background.events.try_iter().collect();
                for event in events {
                    match event {
                        background::Event::Initialized(result) => {
                            apply_initialization_result(&u, &mut s, result);
                        }
                        background::Event::Devices(result) => {
                            apply_device_result(&u, &mut s, result);
                        }
                        background::Event::Conflicts(result) => { s.conflicts_pending = false; apply_conflicts(&u, &mut s, result); }
                        background::Event::Startup(enabled, result) => { match result { Ok(()) => { u.set_startup(enabled);
                                    if s.settings.startup != enabled { s.settings.startup = enabled; s.save_at = Some(Instant::now()); } u.set_startup_message(if u.get_english() { "Startup preference applied." } else { "تنظیم اجرای استارت‌آپ اعمال شد." }.into()); }, Err(e) => { log::error!("Windows startup: {e:#}"); u.set_startup(s.settings.startup); u.set_startup_message(format!("{e:#}").into()); } } }
                        background::Event::Saved(result) => { if let Err(e) = result { log::error!("Settings: {e:#}"); } }
                        background::Event::Exited(engine, result) => {
                            s.engine = engine;
                            match result {
                                Ok(()) => { let _ = slint::quit_event_loop(); return; }
                                Err(e) => {
                                    log::error!("Safe exit failed: {e:#}");
                                    let message = if u.get_english() {
                                        "Safe exit could not be verified. Check the audio output and access to the data folder, then retry Exit app."
                                    } else {
                                        "خروج امن تأیید نشد. اتصال خروجی صدا و دسترسی به پوشهٔ داده‌ها را بررسی کنید و دوباره گزینهٔ خروج را بزنید."
                                    };
                                    s.exiting = false;
                                    s.error = Some(message.into());
                                    s.audio_error_presentation = None;
                                    u.set_busy(false);
                                    u.set_status(message.into());
                                    u.set_audio_error(message.into());
                                    show(&u);
                                }
                            }
                        }
                    }
                }
                if s.exiting { return; }
                if startup_resume.get()
                    && u.get_devices_ready()
                    && u.get_recovery_ready()
                    && !u.get_running()
                    && !u.get_busy()
                    && !s.components.busy
                {
                    startup_resume.set(false);
                    log::info!("Windows startup: resuming the last active audio configuration");
                    let default_volume = s.settings.default_volume;
                    s.settings.target = default_volume;
                    u.set_target(default_volume);
                    start_audio(&u, &mut s);
                }
                if s.conflict_scan_at.elapsed() >= Duration::from_secs(5) {
                    update_conflicts(&u, &mut s);
                }
                for event in s.engine.poll() {
                    match event {
                        AudioEvent::PhysicalOutput(id) => {
                            s.settings.last_physical_output_id = id;
                            s.save_at = Some(Instant::now());
                        }
                        AudioEvent::Started => {
                            let from_hotkey = s.hotkey_pending_audio == Some(HotkeyPendingAudio::Start);
                            if from_hotkey { s.hotkey_pending_audio = None; }
                            u.set_running(true);
                            u.set_busy(false);
                            u.set_paused(false);
                            if !s.settings.was_running {
                                s.settings.was_running = true;
                                s.save_at = Some(Instant::now());
                            }
                            if from_hotkey { notify_action(&u, &osd, osd::NoticeIcon::Play, "صدا آماده است", "Audio started", "پردازش صدا با موفقیت آغاز شد.", "Audio processing is ready."); }
                        }
                        AudioEvent::Stopped => {
                            let from_hotkey = s.hotkey_pending_audio == Some(HotkeyPendingAudio::Stop);
                            if from_hotkey { s.hotkey_pending_audio = None; }
                            u.set_running(false);
                            u.set_busy(false);
                            u.set_paused(false);
                            if s.settings.was_running {
                                s.settings.was_running = false;
                                s.save_at = Some(Instant::now());
                            }
                            if from_hotkey { notify_action(&u, &osd, osd::NoticeIcon::Stop, "صدا متوقف شد", "Audio stopped", "دستگاه صوتی بازگردانی شد.", "The audio device has been restored."); }
                        }
                        AudioEvent::Error(e) | AudioEvent::RestoreWarning(e) => {
                            present_audio_failure(&u, &mut s, &osd, e, None);
                        }
                        AudioEvent::DeviceInvalidated(e) => {
                            let presentation = AudioErrorPresentation::DeviceInvalidated { english: u.get_english() };
                            present_audio_failure(&u, &mut s, &osd, e, Some(presentation));
                        }
                    }
                }
                for event in s.components.poll() {
                    match event {
                        components::Event::Metadata(m) => {
                            u.set_latest_version(if m.latest > 0 { format!("Pack{}", m.latest) } else { String::new() }.into());
                            u.set_update_available(m.confirmed_pack > 0 && m.latest > m.confirmed_pack);
                        }
                        components::Event::Progress(p) => u.set_download_progress(p),
                        components::Event::Message(m) => u.set_component_message(m.into()),
                        components::Event::CheckFinished { installed, outcome } => {
                            finish_component_check(&u, installed, outcome);
                            if let Err(e) = refresh(&u, &mut s) {
                                log::error!("Refresh after components check: {e:#}");
                            }
                        }
                        components::Event::Done(found) => {
                            u.set_components_busy(false);
                            u.set_components_installing(false);
                            u.set_cable_installed(found);
                            if let Err(e) = refresh(&u, &mut s) {
                                log::error!("Refresh after components check: {e:#}");
                            }
                        }
                        components::Event::Error(e) => component_error(&u, e),
                        components::Event::Downloaded => {
                            u.set_components_busy(false);
                            u.set_components_installing(false);
                            u.set_component_message(if u.get_english() { "Fresh package verified and saved. Choose Install / repair to launch setup." } else { "بستهٔ تازه تأیید و ذخیره شد. برای اجرای نصاب، نصب / ترمیم را بزن." }.into());
                        }
                    }
                }
                for event in s.app_updates.poll() {
                    match event {
                        app_update::Event::Found { version, update_available } => {
                            u.set_app_release_notes_ready(s.app_updates.release_notes_url().is_some());
                            u.set_app_latest_version(version.into());
                            u.set_app_update_installable(update_available);
                            u.set_app_update_cancelling(false);
                            u.set_app_update_state(if update_available { 3 } else { 2 });
                        }
                        app_update::Event::Error(error) => {
                            app_update_error(&u, &error);
                        }
                        app_update::Event::Progress(progress) => {
                            u.set_app_update_progress(progress.clamp(0.0, 1.0));
                            if !u.get_app_update_cancelling() { u.set_app_update_state(if progress >= 1.0 { 6 } else { 5 }); }
                        }
                        app_update::Event::Cancelled => {
                            u.set_app_update_cancelling(false);
                            u.set_app_update_progress(0.0);
                            u.set_app_update_state(8);
                        }
                        app_update::Event::Downloaded(installer) => {
                            if s.exiting {
                                if let Err(error) = app_update::discard_download(&installer) { log::warn!("Discard update on exit: {error:#}"); }
                                continue;
                            }
                            u.set_app_update_cancelling(false);
                            u.set_app_update_state(7);
                            let (tx, rx) = std::sync::mpsc::channel();
                            s.update_handoff_result = Some(rx);
                            std::thread::spawn(move || {
                                let result = update_handoff::launch_verified(&installer, env!("CARGO_PKG_VERSION"));
                                if let Err(error) = app_update::discard_download(&installer) { log::warn!("Discard update download after handoff: {error:#}"); }
                                let _ = tx.send(result);
                            });
                        }
                    }
                }
                let handoff = s.update_handoff_result.as_ref().and_then(|rx| match rx.try_recv() {
                    Ok(result) => Some(result),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("UPD_HANDOFF: update worker disconnected"))),
                });
                if let Some(result) = handoff {
                    s.update_handoff_result = None;
                    match result {
                        Ok(probe) => s.update_health = Some(probe),
                        Err(error) => app_update_error(&u, &format!("{error:#}")),
                    }
                }
                let ready_for_health = u.get_recovery_ready() && u.get_devices_ready() && s.error.is_none() && !s.exiting;
                let mut retire_probe = false;
                if let Some(probe) = s.update_health.as_mut() {
                    if ready_for_health {
                        if let Err(error) = probe.acknowledge(env!("CARGO_PKG_VERSION")) {
                            app_update_error(&u, &format!("UPD_HANDOFF: startup acknowledgement failed: {error:#}"));
                            retire_probe = true;
                        }
                    }
                    match probe.poll_result() {
                        Ok(Some(result)) => {
                            log::info!("Application update outcome {:?}, version {}: {}", result.status, result.version, result.message);
                            if let Some(path) = result.backup_path { log::warn!("Update recovery backups: {}", path.display()); }
                            u.set_app_update_state(match result.status {
                                update_handoff::UpdateStatus::Updated => 9,
                                update_handoff::UpdateStatus::RolledBack => 10,
                                update_handoff::UpdateStatus::RecoveryRequired => 11,
                                update_handoff::UpdateStatus::Failed => 4,
                            });
                            if matches!(result.status, update_handoff::UpdateStatus::Failed) { u.set_app_update_error_kind(6); }
                            u.set_app_update_cancelling(false);
                            retire_probe = true;
                        }
                        Ok(None) => (),
                        Err(error) => {
                            app_update_error(&u, &format!("UPD_HANDOFF: could not read the update result: {error:#}"));
                            retire_probe = true;
                        }
                    }
                }
                if retire_probe { s.update_health = None; }
                if s.save_at
                    .is_some_and(|i| i.elapsed() > Duration::from_millis(600))
                {
                    let _ = s.background.submit(background::Job::Save(s.settings.clone(), s.data.clone()));
                    log::info!(
                        "Settings target={}, mode={}, boost={} dB, speed={}, profile={}, leveling={}, treble_smoothing={}",
                        s.settings.target,
                        s.settings.mode,
                        s.settings.boost_db,
                        s.settings.speed,
                        s.settings.profile,
                        s.settings.leveling,
                        s.settings.treble_smoothing
                    );
                    s.save_at = None;
                }
                let snap = *s.engine.snapshot.lock().unwrap_or_else(|p| p.into_inner());
                audio_diagnostics.borrow_mut().report(startup_smoke_started.elapsed(), &snap,
                    s.engine.controls.paused.load(std::sync::atomic::Ordering::Relaxed));
                let m = snap.meters;
                u.set_measurement_source(match snap.source {
                    audio_engine::MeasurementSource::Unavailable => 0,
                    audio_engine::MeasurementSource::Sampled => 1,
                    audio_engine::MeasurementSource::PeakEstimate => 2,
                });
                let meter_index = |value: f32| dsp::index(value as f64);
                let input = meter_index(m.input);
                let output = if snap.silent {
                    0
                } else {
                    meter_index(m.output)
                };
                u.set_input_text(input.to_string().into());
                u.set_output_text(output.to_string().into());
                u.set_input_level(input as f32 / 200.0);
                u.set_output_level(output as f32 / 200.0);
                u.set_gain_db(m.gain_db);
                u.set_gain_text(format!("{:+.1} dB", m.gain_db).into());
                u.set_limited(m.limited);
                u.set_boost_limited(m.boost_limited);
                u.set_deess_text(format!("\u{2066}−{:.1} dB\u{2069}", m.sibilance_db).into());
                u.set_deess_level(m.sibilance_db / 8.0);
                if s.history.len() >= 120 {
                    s.history.pop_front();
                }
                s.history.push_back(LevelPoint {
                    input: input as f32 / 200.0,
                    output: output as f32 / 200.0,
                });
                if u.window().is_visible() && u.get_page() == 0 { u.set_input_path(
                    graph_path(&s.history, u.get_graph_width(), u.get_graph_height(), false).into(),
                );
                u.set_output_path(
                    graph_path(&s.history, u.get_graph_width(), u.get_graph_height(), true).into(),
                ); }
                let revision = app_log::AppLogger::revision();
                if u.window().is_visible() && u.get_page() == 2 && revision != log_revision.get() { logs.set_vec(
                        app_log::AppLogger::events()
                            .into_iter()
                            .map(|e| LogRow {
                                time: e.time.into(),
                                level: e.level.into(),
                                message: e.message.into(),
                            })
                            .collect::<Vec<_>>(),
                ); log_revision.set(revision); }
                if s.error.is_none() && !u.get_busy() {
                    let en = u.get_english();
                    let status = if !u.get_running() {
                        if en {
                            "Ready · Start to normalize audio"
                        } else {
                            "آماده · برای تنظیم صدا شروع را بزن"
                        }
                    } else if u.get_paused() {
                        if en {
                            "Paused · Original sound restored"
                        } else {
                            "توقف موقت · صدای اصلی"
                        }
                    } else if snap.muted {
                        if en {
                            "Windows is muted"
                        } else {
                            "صدای ویندوز قطع است"
                        }
                    } else if s.settings.leveling && s.settings.target == 0 {
                        if en {
                            "Target 0 · Muted"
                        } else {
                            "سطح ۰ · بی‌صدا"
                        }
                    } else if snap.silent {
                        if en {
                            "Listening · Silence / noise gate"
                        } else {
                            "در حال اجرا · سکوت / حذف بوست نویز"
                        }
                    } else if !s.settings.leveling {
                        if en {
                            if s.settings.mode == 1 {
                                "Manual volume / boost active · Steady loudness off"
                            } else {
                                "Manual volume active · Steady loudness off"
                            }
                        } else if s.settings.mode == 1 {
                            "ولوم / بوست دستی فعال · ثابت ماندن بلندی خاموش"
                        } else {
                            "ولوم دستی فعال · ثابت ماندن بلندی خاموش"
                        }
                    } else if s.settings.mode == 0 && snap.reference == 0.0 {
                        if en {
                            "Learning your usual Windows sound…"
                        } else {
                            "در حال سنجش بلندی معمول ویندوز…"
                        }
                    } else if m.boost_limited {
                        if en {
                            if s.settings.mode==0 {"Native ceiling reached · virtual cable can boost further"} else {"Configured digital boost ceiling reached"}
                        } else {
                            if s.settings.mode==0 {"سقف ولوم دستگاه · کابل مجازی می‌تواند بیشتر تقویت کند"} else {"سقف بوست تنظیم‌شده؛ هدف کامل قابل دستیابی نیست"}
                        }
                    } else if m.limited {
                        if en {
                            "Transient protection active"
                        } else {
                            "محافظت در برابر جهش صدا فعال است"
                        }
                    } else {
                        if en {
                            "Normalizing toward your target"
                        } else {
                            "در حال نزدیک کردن صدا به سطح انتخابی"
                        }
                    };
                    u.set_status(status.into());
                }
                if let Some(tray) = native.borrow_mut().as_mut() { tray.set_scroll_enabled(tray_scroll_allowed(&u, &s)); tray.update(
                    u.get_running(),
                    u.get_paused(),
                    u.get_target(),
                    u.get_english(),
                    u.get_mode() == 0,
                ); }
            },
        );
    }
    let activation_timer = slint::Timer::default();
    if let Some(i) = instance.as_ref() {
        let event = i.event;
        let w = ui.as_weak();
        activation_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                if unsafe { windows::Win32::System::Threading::WaitForSingleObject(event, 0) }.0
                    == 0
                {
                    if let Some(u) = w.upgrade() {
                        show(&u);
                    }
                }
            },
        );
    }
    let smoke_timer = slint::Timer::default();
    if smoke {
        log::info!("UI smoke timer scheduled");
        let w = ui.as_weak();
        let native = native.clone();
        let osd = osd.clone();
        let directory = data.clone();
        let step = Rc::new(RefCell::new(0));
        let cable_before_warning = std::cell::Cell::new(false);
        smoke_timer.start(slint::TimerMode::Repeated, Duration::from_millis(1600), move || { let Some(u) = w.upgrade() else { return; }; let mut step = step.borrow_mut(); log::info!("UI smoke step {step}"); let result = (|| -> Result<()> { match *step {
        0 => { anyhow::ensure!(u.get_devices_ready() && u.get_recovery_ready(), "Startup initialization did not finish"); anyhow::ensure!(u.get_sidebar_brand_text() == "مکسی سَوندسِت" && u.get_log_label() == "لاگ" && u.get_gain_label() == "بهره صدا", "Persian localization labels changed"); cable_before_warning.set(u.get_cable_installed()); u.set_cable_installed(false); anyhow::ensure!(u.get_cable_warning_visible(), "Missing cable warning absent"); snapshot_png(&u, &directory.join("Cable-Warning-FA.png"))?; anyhow::ensure!(u.get_sidebar_on_right(), "Persian sidebar is not on the right"); snapshot_png(&u, &directory.join("Sound-FA.png"))?; },
        1 => { u.set_english(true); u.invoke_config_changed(); anyhow::ensure!(u.get_sidebar_brand_text() == "MAXI\nSOUNDSET" && u.get_log_label() == "Log" && u.get_gain_label() == "GAIN", "English localization labels changed"); anyhow::ensure!(u.get_outputs().row_data(0).unwrap() == "System default", "Default output label did not translate"); u.set_page(1); }, 2 => { anyhow::ensure!(!u.get_sidebar_on_right(), "English sidebar did not move left"); snapshot_png(&u, &directory.join("Components.png"))?; u.set_cable_installed(cable_before_warning.get()); },
        3 => u.set_page(2), 4 => snapshot_png(&u, &directory.join("Log.png"))?,
        5 => { u.set_page(0); u.invoke_enter_target_text("0".into()); anyhow::ensure!(u.get_target()==0 && u.get_exact_target_text()=="0"); },
        6 => { u.invoke_enter_target_text("200".into()); anyhow::ensure!(u.get_exact_target_text()=="100", "Windows exact field retained rejected boost"); anyhow::ensure!(u.get_target() == 100 && !u.get_volume_notice().is_empty(), "Windows boost was not rejected"); u.set_volume_notice("".into()); native_tray::post_own_close()?; },
        7 => { anyhow::ensure!(!u.window().is_visible(), "Native Close did not hide the window"); native_tray::send(native_tray::Action::Show); u.set_target(100); },
        8 => { anyhow::ensure!(u.window().is_visible(), "Native tray action did not restore the window"); snapshot_png(&u, &directory.join("Sound-EN.png"))?; u.invoke_preview_menu(); },
        9 => { snapshot_png(&u, &directory.join("Menu.png"))?; u.invoke_close_preview_menu(); u.set_page(0); },
        10 => { snapshot_png(&u, &directory.join("Sound-Profiles-EN.png"))?; u.set_profile(0); u.invoke_config_changed(); anyhow::ensure!(u.get_profile() == 0); u.set_english(false); },
        11 => { anyhow::ensure!(Settings::load(&directory).profile == 0, "System profile was not saved"); snapshot_png(&u, &directory.join("Sound-Profiles-FA.png"))?; u.set_profile(1); u.invoke_config_changed(); u.set_sidebar_expanded(false); u.set_page(0); },
        12 => { snapshot_png(&u, &directory.join("Collapsed.png"))?; u.set_sidebar_expanded(true); u.set_mode(1); u.invoke_enter_target_text("200".into()); u.set_page(0); u.invoke_config_changed(); anyhow::ensure!(u.get_target()==200, "Full mode cannot select 200"); },
        13 => { anyhow::ensure!(!u.get_enhancement_hint_visible(), "Virtual cable mode retained the switch-mode hint"); anyhow::ensure!(u.get_exact_target_text()=="200","Full exact field differs from target"); snapshot_png(&u, &directory.join("Sound-Profiles-Full-FA.png"))?; anyhow::ensure!(Settings::load(&directory).profile == 1, "Gentle profile was not saved"); u.invoke_preview_routing_help(); },

        14 => { snapshot_png(&u, &directory.join("Routing-Help-FA.png"))?; u.invoke_close_routing_help(); window_repaint::resize_logically(u.window(), 1120., 760.); u.set_page(4); u.set_leveling(false); u.set_intensity(2); u.set_treble_smoothing(false); u.set_target(0); u.invoke_config_changed(); u.set_startup(false); u.invoke_startup_changed(); u.set_startup(true); u.invoke_startup_changed(); u.set_startup_active(true); u.invoke_config_changed(); },
        15 => { let saved = Settings::load(&directory); anyhow::ensure!(!saved.leveling && saved.intensity == 2 && !saved.treble_smoothing && saved.target == 0 && saved.profile == 1 && saved.startup && saved.startup_active, "Independent features/intensity/smoothing/startup settings did not persist"); u.invoke_scroll_settings_to(0.); snapshot_png(&u, &directory.join("Settings-FA-Top-Min.png"))?; u.set_english(true); u.invoke_config_changed(); snapshot_png(&u, &directory.join("Settings-EN-Top-Min.png"))?; u.set_english(false); u.invoke_config_changed(); u.invoke_scroll_settings_to(500.); },
        16 => { snapshot_png(&u, &directory.join("Settings-Shortcuts-FA-Min.png"))?; u.set_english(true); u.invoke_config_changed(); snapshot_png(&u, &directory.join("Settings-Shortcuts-EN-Min.png"))?; u.set_english(false); u.invoke_config_changed(); u.invoke_scroll_settings_to(0.); anyhow::ensure!(Settings::load(&directory).profile == 1 && !u.get_leveling(), "Settings changed during shortcut visual verification"); u.invoke_choose_profile(6); u.set_page(6); },
        17 => { anyhow::ensure!(u.get_app_version() == env!("CARGO_PKG_VERSION"), "About version mismatch"); u.invoke_check_app_update(); anyhow::ensure!(u.get_app_update_state() == 2 && u.get_app_latest_version() == format!("v{}", env!("CARGO_PKG_VERSION")), "About current-version state failed"); snapshot_png(&u, &directory.join("About-FA.png"))?; u.set_app_update_state(4); u.set_app_latest_version("verification network error".into()); snapshot_png(&u, &directory.join("About-Update-Error-FA.png"))?; u.set_english(true); u.set_app_update_state(3); u.set_app_latest_version("v1.0.3".into()); },
        18 => { snapshot_png(&u, &directory.join("About-EN.png"))?; u.set_english(false); u.set_page(5); u.invoke_refresh_conflicts(); },
        19 => { snapshot_png(&u, &directory.join("Conflicts-FA.png"))?; u.set_conflicts(Rc::new(VecModel::from(vec![ConflictRow { pid: 12345, name: "FxSound · verification preview".into(), exe: "FxSound.exe".into() }])).into()); },
        20 => { snapshot_png(&u, &directory.join("Conflicts-Warning.png"))?; u.invoke_end_conflict(12345); u.set_leveling(true); u.invoke_set_enhancement(false); u.invoke_config_changed(); u.set_page(0); },
        21 => { let saved = Settings::load(&directory); anyhow::ensure!(saved.profile == 0 && saved.leveling, "Leveling depends on enhancement"); u.invoke_set_enhancement(true); anyhow::ensure!(u.get_profile() == 6, "Enhancement did not remember selected profile"); u.set_target(100); u.invoke_config_changed(); u.set_page(0); window_repaint::resize_logically(u.window(), 1120., 760.); },
        22 => { let playback=u.get_playback_device_width(); let processing=u.get_processing_mode_width(); anyhow::ensure!(u.get_minimum_size_valid() && u.get_sound_layout_fits() && (playback*1.25-processing).abs()<0.2, "DPI minimum size/controls clipped or header proportions changed: physical={:?}, scale={}, fits={}, playback={}, processing={}", u.window().size(), u.window().scale_factor(), u.get_sound_layout_fits(), playback, processing); snapshot_png(&u, &directory.join("Sound-Minimum-FA.png"))?; u.invoke_choose_profile(7); let eq_model=u.get_eq_bands(); u.invoke_eq_band_changed(2, 4.5); anyhow::ensure!(eq_model==u.get_eq_bands(),"EQ drag replaced its fader model"); u.invoke_eq_band_changed(6, -3.); u.invoke_eq_band_changed(2, 5.); u.invoke_eq_band_changed(2, 4.5); anyhow::ensure!(eq_model==u.get_eq_bands(),"Continuous EQ changes recreate faders"); u.set_leveling(false); u.invoke_config_changed(); },
        23 => { anyhow::ensure!(u.get_minimum_size_valid() && u.get_sound_layout_fits(), "DPI minimum size/controls clipped: physical={:?}, scale={}, fits={}", u.window().size(), u.window().scale_factor(), u.get_sound_layout_fits()); let saved=Settings::load(&directory); anyhow::ensure!(saved.profile == 7 && !saved.leveling && saved.eq_bands[2] == 4.5 && saved.eq_bands[6] == -3., "Custom EQ did not persist independently"); snapshot_png(&u, &directory.join("Equalizer-Minimum-FA.png"))?; window_repaint::resize_logically(u.window(), 1280., 860.); },
        24 => { snapshot_png(&u, &directory.join("Equalizer-FA.png"))?; u.set_english(true); u.invoke_config_changed(); },
        25 => { snapshot_png(&u, &directory.join("Equalizer-EN.png"))?; u.invoke_reset_equalizer(); },
        26 => { anyhow::ensure!(Settings::load(&directory).eq_bands == [0.;10], "EQ reset did not persist"); u.invoke_set_enhancement(false); },
        27 => { anyhow::ensure!(u.get_profile()==0, "Custom enhancement bypass failed"); u.invoke_set_enhancement(true); anyhow::ensure!(u.get_profile()==7, "Custom profile was not remembered"); u.set_mode(0); u.set_target(140); u.invoke_config_changed(); },
        28 => { let playback=u.get_playback_device_width(); let processing=u.get_processing_mode_width(); anyhow::ensure!(u.get_target()==100 && Settings::load(&directory).target ==100, "Mode switch did not clamp target"); anyhow::ensure!((playback*1.25-processing).abs()<0.2, "Windows-mode header proportions changed: playback={}, processing={}", playback, processing); snapshot_png(&u,&directory.join("Windows-Boost-Notice.png"))?; },
        29 => { u.set_volume_notice("".into()); u.set_mode(1); u.set_target(200); u.invoke_config_changed(); u.invoke_preview_profile_menu(); },
        30 => { snapshot_png(&u,&directory.join("Profile-Menu-EN.png"))?; u.invoke_close_profile_menu(); u.set_english(false); u.invoke_config_changed(); },
        31 => { anyhow::ensure!(u.get_target()==200 && Settings::load(&directory).target==200,"Full target200 not retained"); snapshot_png(&u,&directory.join("Sound-200-FA.png"))?; },
        32 => { let _ = u.hide(); },
        33 => { anyhow::ensure!(!u.window().is_visible(), "UI smoke window remained visible after hide"); notify_action(&u, &osd, osd::NoticeIcon::Warning, "اعلان آزمایشی", "Synthetic test notice", "اعلان پس‌زمینه", "Synthetic background notice"); },
        34 => { anyhow::ensure!(osd.borrow().as_ref().is_some_and(|window| window.is_visible() && window.query_visibility(Duration::from_millis(800)) == Some(true)), "Lazy OSD HWND was not visible or its worker did not answer within 800 ms"); },
        35 => { anyhow::ensure!(osd.borrow().as_ref().is_some_and(|window| !window.is_visible() && window.query_visibility(Duration::from_millis(800)) == Some(false)), "OSD did not auto-hide or its worker did not answer within 800 ms"); let registered = native.borrow().as_ref().is_some_and(|tray| tray.is_registered()); fs::write(directory.join("ui-smoke.txt"), format!("PASS: Slint 1.18.0 FA/EN RTL sidebar and row ordering; Sound/profiles/settings/conflicts/About/components/log/dropdown/collapsed/minimum-window rendering; profile 0/1/6/7, steadying intensity and optional treble smoothing persist; routing help popup with explicit close control and treble-reduction meter render; Persian/English sidebar icons remain anchored throughout collapse/expand animation; Donate action and startup audio-resume preference render; custom 10-band EQ and reset persist independently; English System default updates on language switch; manual volume/boost, leveling and enhancement independently enabled; remembered profile; startup preferences persist without touching host registry; About Rust/Slint logo credits, v{} branding, localized update states including error/log access, and compact website/report/license actions; aligned Log controls with regular spacing; proportional header fully fills available width with Playback 20% narrower than Processing; no host app termination; Windows mode max100 and full mode 0/200; custom EQ FA/EN/minimum size; native Close hides; native tray Show/Exit dispatch; lazy synthetic layered OSD appeared, remained visible, then auto-hid\nNotification-area registration: {}\n", env!("CARGO_PKG_VERSION"), if registered { "PASS" } else { "UNVERIFIED: rejected by this sandboxed Windows session" }))?; u.invoke_exit_app(); }, _ => () } Ok(()) })(); if let Err(e) = result { log::error!("UI smoke: {e:#}"); let _ = fs::write(directory.join("ui-smoke.txt"), format!("FAIL {e:#}")); u.invoke_exit_app(); } *step += 1; });
    }
    slint::run_event_loop_until_quit()?;
    state.borrow_mut().engine.shutdown()?;
    Ok(())
}
fn snapshot_png(ui: &AppWindow, path: &Path) -> Result<()> {
    let buffer = ui.window().take_snapshot()?;
    let mut encoder = png::Encoder::new(fs::File::create(path)?, buffer.width(), buffer.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(buffer.as_bytes())?;
    Ok(())
}
// Emits a controlled signal only into a virtual cable, never directly into speakers.
struct TestTone {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
}
impl TestTone {
    fn start(id: String) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || -> Result<()> {
            let _com = core_audio::ComScope::new()?;
            let fmt = core_audio::CaptureStream::new(&id)?;
            let rate = fmt.format.rate;
            let channels = fmt.format.channels;
            let mut render = core_audio::RenderStream::new(&id, &fmt.format)?;
            render.start()?;
            let mut frame = 0usize;
            let mut samples = vec![0_f32; render.frames * channels];
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                let count = render.available()?;
                for i in 0..count {
                    let value = (0.4
                        * (2. * std::f64::consts::PI * 440. * frame as f64 / rate as f64).sin())
                        as f32;
                    for c in 0..channels {
                        samples[i * channels + c] = value;
                    }
                    frame += 1;
                }
                render.write(&samples[..count * channels])?;
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for TestTone {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => (),
                result => log::error!("Test tone worker: {result:?}"),
            }
        }
    }
}
fn engine_smoke(data: &Path) -> Result<()> {
    let _com = core_audio::ComScope::new()?;
    let initial_defaults: Vec<_> = (0..3).map(core_audio::default_id).collect::<Result<_>>()?;
    let devices = core_audio::devices()?;
    let id = devices
        .iter()
        .find(|d| d.vb_cable)
        .or_else(|| devices.iter().find(|d| d.virtual_input))
        .context("An installed virtual cable is required for routing verification")?
        .id
        .clone();
    anyhow::ensure!(!data.join("recovery.json").exists(), "Finish pending audio recovery before the smoke check");
    let ep = core_audio::Endpoint::new(&id)?;
    let original = ep.scalar()?;
    let muted = ep.muted()?;
    let mut e = AudioEngine::new();
    e.start(Settings { target: 0, output_id: id.clone(), auto_route: false, ..Default::default() }, data.to_path_buf())?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut events = Vec::new();
    loop {
        events.extend(e.poll());
        if events.iter().any(|event| matches!(event, AudioEvent::Stopped)) { break; }
        anyhow::ensure!(Instant::now() < deadline, "Virtual-output rejection did not finish: {events:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::ensure!(!events.iter().any(|event| matches!(event, AudioEvent::Started))
        && events.iter().any(|event| matches!(event, AudioEvent::Error(_))),
        "Windows must reject a virtual actuator before Started: {events:?}");
    anyhow::ensure!((ep.scalar()? - original).abs() < 0.001 && ep.muted()? == muted,
        "Rejected virtual actuator changed volume or mute");
    for (role, before) in initial_defaults.iter().enumerate() {
        anyhow::ensure!(*before == core_audio::default_id(role as i32)?, "Rejected virtual actuator changed a default");
    }
    anyhow::ensure!(!data.join("recovery.json").exists(), "Rejected virtual actuator left recovery state");
    e.shutdown()?;
    // Exercise the real capture/render worker with independently enabled profiles.
    // Manual routing avoids touching the user's Windows default device assignments.
    let devices = core_audio::devices()?;
    let source = devices
        .iter()
        .find(|d| d.vb_cable)
        .context("Full processing smoke requires installed VB-CABLE")?;
    let output = devices
        .iter()
        .find(|d| !d.virtual_input && d.verified_physical && d.name.contains("Realtek"))
        .or_else(|| devices.iter().find(|d| !d.virtual_input && d.verified_physical))
        .context("No physical playback output")?;
    let defaults: Vec<_> = (0..3).map(core_audio::default_id).collect::<Result<_>>()?;
    let physical_ep = core_audio::Endpoint::new(&output.id)?;
    let physical_original = physical_ep.scalar()?;
    let physical_muted = physical_ep.muted()?;
    e.start(
        Settings {
            mode: 1,
            target: 0,
            leveling: false,
            profile: 1,
            source_id: source.id.clone(),
            output_id: output.id.clone(),
            auto_route: false,
            ..Default::default()
        },
        data.to_path_buf(),
    )?;
    std::thread::sleep(Duration::from_millis(600));
    let events = e.poll();
    anyhow::ensure!(
        events.iter().any(|v| matches!(v, AudioEvent::Started))
            && !events.iter().any(|v| matches!(v, AudioEvent::Error(_) | AudioEvent::DeviceInvalidated(_))),
        "Full capture/render worker failed: {events:?}"
    );
    anyhow::ensure!(
        (physical_ep.scalar()? - physical_original).abs() < 0.001,
        "Profile-only playback changed physical master volume"
    );
    // Feed only the cable while Full is active at target zero. Stop and drain
    // the tone before pause (which bypasses DSP), target changes or headroom.
    let nonzero_before_tone = e.snapshot.lock().unwrap().captured_nonzero_packets();
    let tone = TestTone::start(source.id.clone());
    let deadline = Instant::now() + Duration::from_secs(3);
    let captured = loop {
        let value = *e.snapshot.lock().unwrap();
        if value.source == audio_engine::MeasurementSource::Sampled && value.captured_nonzero_packets() > nonzero_before_tone { break value; }
        anyhow::ensure!(Instant::now() < deadline, "Full did not observe nonzero cable packets");
        std::thread::sleep(Duration::from_millis(20));
    };
    anyhow::ensure!(captured.meters.output == 0., "Target zero must silence the captured tone");
    drop(tone);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if e.snapshot.lock().unwrap().silent { break; }
        anyhow::ensure!(Instant::now() < deadline, "Cable is not quiet after the isolated tone; refusing headroom checks");
        std::thread::sleep(Duration::from_millis(20));
    }
    e.controls
        .profile
        .store(7, std::sync::atomic::Ordering::Relaxed);
    e.controls.eq_bands[5].store(45, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    e.pause();
    std::thread::sleep(Duration::from_millis(100));
    e.pause();
    e.controls
        .profile
        .store(0, std::sync::atomic::Ordering::Relaxed);
    e.controls
        .leveling
        .store(true, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    anyhow::ensure!(
        !e.poll().iter().any(|v| matches!(v, AudioEvent::Error(_) | AudioEvent::DeviceInvalidated(_))),
        "Full worker errored during feature switching"
    );
    anyhow::ensure!(
        (physical_ep.scalar()? - physical_original).abs() < 0.001,
        "Zero target unexpectedly reserved physical headroom"
    );
    e.controls
        .target
        .store(100, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(120));
    anyhow::ensure!(
        (physical_ep.scalar()? - 1.).abs() < 0.001,
        "Full leveling failed to reserve output headroom"
    );
    physical_ep.set_scalar(0.55)?;
    std::thread::sleep(Duration::from_millis(220));
    anyhow::ensure!(
        (physical_ep.scalar()? - 1.).abs() < 0.001,
        "Full headroom lost after external slider change"
    );
    e.pause();
    std::thread::sleep(Duration::from_millis(120));
    anyhow::ensure!(
        (physical_ep.scalar()? - physical_original).abs() < 0.001,
        "Full pause failed to restore physical master volume"
    );
    e.pause();
    std::thread::sleep(Duration::from_millis(120));
    anyhow::ensure!(
        (physical_ep.scalar()? - 1.).abs() < 0.001,
        "Full resume failed to reserve output headroom"
    );
    e.controls
        .leveling
        .store(false, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(120));
    anyhow::ensure!(
        (physical_ep.scalar()? - physical_original).abs() < 0.001,
        "Disabling full leveling failed to restore physical master volume"
    );
    e.shutdown()?;
    anyhow::ensure!(
        (physical_ep.scalar()? - physical_original).abs() < 0.001,
        "Full stop did not restore physical master volume"
    );
    for (role, before) in defaults.iter().enumerate() {
        anyhow::ensure!(
            *before == core_audio::default_id(role as i32)?,
            "Full manual routing changed a Windows default"
        );
    }
    let automatic_evidence = if initial_defaults.iter().all(|id| audio_routing::playback(&devices, id).is_some_and(|d| d.verified_physical)) {
        for (role, before) in initial_defaults.iter().enumerate() {
            anyhow::ensure!(*before == core_audio::default_id(role as i32)?, "Host defaults changed before automatic routing check");
        }
        // No tone is active. Target zero keeps this automatic route check silent.
        e.start(Settings { mode: 1, target: 0, leveling: false, profile: 0,
            source_id: source.id.clone(), output_id: output.id.clone(), auto_route: true,
            ..Settings::default() }, data.to_path_buf())?;
        std::thread::sleep(Duration::from_millis(600));
        let events = e.poll();
        anyhow::ensure!(events.iter().any(|event| matches!(event, AudioEvent::Started))
            && !events.iter().any(|event| matches!(event, AudioEvent::Error(_) | AudioEvent::DeviceInvalidated(_))),
            "Automatic Full routing failed: {events:?}");
        for role in 0..3 { anyhow::ensure!(core_audio::default_id(role)? == source.id, "Automatic Full did not bind role {role}"); }
        anyhow::ensure!((physical_ep.scalar()? - physical_original).abs() < 0.001, "Automatic target zero changed output volume");
        let quiet = e.snapshot.lock().unwrap().silent;
        if quiet {
            e.pause();
            std::thread::sleep(Duration::from_millis(100));
            for role in 0..3 { anyhow::ensure!(core_audio::default_id(role)? == source.id, "Full pause changed route {role}"); }
            e.pause();
            std::thread::sleep(Duration::from_millis(100));
        }
        e.shutdown()?;
        for (role, before) in initial_defaults.iter().enumerate() {
            anyhow::ensure!(*before == core_audio::default_id(role as i32)?, "Automatic Full did not restore role {role}");
        }
        anyhow::ensure!((physical_ep.scalar()? - physical_original).abs() < 0.001 && physical_ep.muted()? == physical_muted
            && !data.join("recovery.json").exists(), "Automatic Full did not restore volume/recovery state");
        if quiet { "Automatic Full target-zero start, pause/resume and exact three-role restoration passed from a playback baseline." }
        else { "Automatic Full target-zero start and exact three-role restoration passed; pause/resume skipped because the cable received other audio." }
    } else {
        "Automatic Full cycle skipped: initial defaults include a cable or another virtual/unknown playback route; host baseline was preserved."
    };
    anyhow::ensure!(
        (ep.scalar()? - original).abs() < 0.001
            && ep.muted()? == muted
            && physical_ep.muted()? == physical_muted
            && !data.join("recovery.json").exists(),
        "Final audio state was not restored"
    );
    fs::write(data.join("engine-smoke.txt"), format!("PASS: Windows mode rejects a virtual actuator before Started, without changes to volume, mute or any default role and without leaving recovery state. Actual VB-CABLE capture and physical render worker starts at target 0 with leveling off/profile on; sampled nonzero packets are observed during a cable-only 440Hz test signal while this app's rendered output remains zero; packet counts do not uniquely identify the signal and external forwarders are outside this check. Tone is stopped and cable becomes quiet before pause, nonzero targets or headroom checks. Profiles 1/7/0 and custom EQ +4.5dB, feature switching, pause and shutdown run without errors; manual routing preserves all three Windows defaults. Physical headroom is reserved for active nonzero leveling, maintained after a slider change, and released on pause/off/stop; profile-only and target zero leave master volume unchanged. {automatic_evidence} Native target100/50 loudness-scale measurements from earlier smoke versions are not repeated by this harness. No listening assessment or measured hardware frequency response.\n"))?;
    Ok(())
}

#[cfg(test)]
mod tray_scroll_tests {
    use super::*;
    use slint::platform::{software_renderer::{MinimalSoftwareWindow, RepaintBufferType}, Platform, PlatformError, WindowAdapter};
    use std::sync::atomic::Ordering;

    struct Headless(Rc<MinimalSoftwareWindow>);
    impl Platform for Headless {
        fn create_window_adapter(&self) -> std::result::Result<Rc<dyn WindowAdapter>, PlatformError> { Ok(self.0.clone()) }
    }

    fn inert_state(data: PathBuf, settings: Settings) -> Rc<RefCell<State>> {
        Rc::new(RefCell::new(State {
            background: background::Background::new(), devices_pending: false, conflicts_pending: false,
            exiting: false, components: components::Manager::new(&data), app_updates: app_update::Manager::new(),
            update_handoff_result: None, update_health: None, settings, data, engine: AudioEngine::new(),
            outputs: vec![], sources: vec![], device_inventory: vec![], history: VecDeque::new(), save_at: None, error: None,
            initialization_error: None,
            audio_error_presentation: None,
            audio_apps: vec![], conflict_scan_at: Instant::now(), unmute_target: Some(88),
            hotkey_errors: vec![], hotkey_pending_audio: None,
        }))
    }

    fn reconnect_device(id: &str, name: &str, virtual_input: bool) -> core_audio::DeviceInfo {
        core_audio::DeviceInfo { id: id.into(), name: name.into(), virtual_input, vb_cable: virtual_input, verified_physical: !virtual_input, system_default: false }
    }

    fn reconnect_ui() -> AppWindow {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Headless(adapter))).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.set_recovery_ready(true);
        ui
    }

    #[test]
    fn device_invalidation_preserves_raw_error_and_retranslates_owned_dashboard_text() {
        let ui = reconnect_ui();
        ui.show().unwrap();
        ui.set_english(false);
        let state = inert_state(std::env::temp_dir().join("maxi-invalidated-presentation"), Settings::default());
        let osd = Rc::new(RefCell::new(None));
        let raw = "capture or render operation: device invalidated (0x88890004)";
        state.borrow_mut().hotkey_pending_audio = Some(HotkeyPendingAudio::Start);
        present_audio_failure(&ui, &mut state.borrow_mut(), &osd, raw.into(),
            Some(AudioErrorPresentation::DeviceInvalidated { english: false }));
        assert_eq!(state.borrow().error.as_deref(), Some(raw));
        assert_eq!(ui.get_audio_error(), device_invalidated_notice(false));
        assert_eq!(ui.get_status(), device_invalidated_notice(false));
        assert!(ui.get_volume_notice().is_empty(), "A second transient must not retain raw or old-language text");
        assert!(osd.borrow().is_none());
        ui.set_english(true);
        read_config(&ui, &mut state.borrow_mut());
        assert_eq!(ui.get_audio_error(), device_invalidated_notice(true));
        assert_eq!(ui.get_status(), device_invalidated_notice(true));
        assert_eq!(state.borrow().error.as_deref(), Some(raw));
        // A newer status channel is not owned by this error presentation.
        ui.set_status("Newer independent status".into());
        ui.set_english(false);
        read_config(&ui, &mut state.borrow_mut());
        assert_eq!(ui.get_audio_error(), device_invalidated_notice(false));
        assert_eq!(ui.get_status(), "Newer independent status");
    }

    #[test]
    fn dismissed_or_cleared_invalidation_does_not_return_on_volume_or_language_change() {
        let ui = reconnect_ui();
        ui.set_english(false);
        let state = inert_state(std::env::temp_dir().join("maxi-invalidated-dismiss"), Settings::default());
        let osd = Rc::new(RefCell::new(None));
        present_audio_failure(&ui, &mut state.borrow_mut(), &osd, "raw diagnostic".into(),
            Some(AudioErrorPresentation::DeviceInvalidated { english: false }));
        ui.invoke_dismiss_notice();
        ui.invoke_show_volume_notice("Saved volume".into());
        read_config(&ui, &mut state.borrow_mut());
        assert!(ui.get_audio_error().is_empty());
        ui.set_english(true);
        read_config(&ui, &mut state.borrow_mut());
        assert!(ui.get_audio_error().is_empty());
        assert!(state.borrow().audio_error_presentation.is_none());
        assert_eq!(state.borrow().error.as_deref(), Some("raw diagnostic"), "Dismissal preserves existing safety/error state");
        clear_audio_error(&ui, &mut state.borrow_mut()); // same boundary used by Run
        ui.set_english(false);
        read_config(&ui, &mut state.borrow_mut());
        assert!(ui.get_audio_error().is_empty());
        assert!(state.borrow().error.is_none());
        assert!(state.borrow().audio_error_presentation.is_none());
    }

    #[test]
    fn recovery_ownership_and_restore_warning_take_priority_over_friendly_invalidation() {
        let ui = reconnect_ui();
        ui.set_english(false);
        let state = inert_state(std::env::temp_dir().join("maxi-invalidated-recovery"), Settings::default());
        let osd = Rc::new(RefCell::new(None));
        state.borrow_mut().initialization_error = Some("Earlier recovery error".into());
        present_audio_failure(&ui, &mut state.borrow_mut(), &osd, "New invalidation diagnostic".into(),
            Some(AudioErrorPresentation::DeviceInvalidated { english: false }));
        apply_initialization_result(&ui, &mut state.borrow_mut(), Ok(()));
        assert_eq!(state.borrow().error.as_deref(), Some("New invalidation diagnostic"));
        assert_eq!(ui.get_audio_error(), device_invalidated_notice(false));
        assert!(state.borrow().audio_error_presentation.is_some());
        // RestoreWarning uses the same unclassified failure path as Error.
        present_audio_failure(&ui, &mut state.borrow_mut(), &osd, "Audio restoration is incomplete. Reconnect the previous device.".into(), None);
        ui.set_english(true);
        read_config(&ui, &mut state.borrow_mut());
        assert_eq!(ui.get_audio_error(), "Audio restoration is incomplete. Reconnect the previous device.");
        assert_eq!(state.borrow().error.as_deref(), Some(ui.get_audio_error().as_str()));
        assert!(state.borrow().audio_error_presentation.is_none());
        present_audio_failure(&ui, &mut state.borrow_mut(), &osd, "Another raw invalidation".into(),
            Some(AudioErrorPresentation::DeviceInvalidated { english: true }));
        apply_initialization_result(&ui, &mut state.borrow_mut(), Err(anyhow::anyhow!("Recovery blocked")));
        assert_eq!(ui.get_audio_error(), "Recovery blocked");
        assert!(state.borrow().audio_error_presentation.is_none());
        apply_initialization_result(&ui, &mut state.borrow_mut(), Ok(()));
        assert!(ui.get_audio_error().is_empty());
        assert!(state.borrow().error.is_none());
        ui.set_english(false);
        read_config(&ui, &mut state.borrow_mut());
        assert!(ui.get_audio_error().is_empty());
    }

    #[test]
    fn device_reconnect_missing_output_survives_config_save_and_exact_return() {
        let ui = reconnect_ui();
        let data = std::env::temp_dir().join(format!("maxi-104-reconnect-output-{}", std::process::id()));
        fs::create_dir_all(&data).unwrap();
        let mut settings = Settings::default();
        settings.output_id = "bluetooth-original".into();
        settings.source_id = "cable-original".into();
        let state = inert_state(data.clone(), settings);
        let cable = reconnect_device("cable-original", "CABLE Input", true);
        let speakers = reconnect_device("speakers", "Speakers", false);
        let headphones = reconnect_device("bluetooth-original", "Headphones", false);
        apply_devices(&ui, &mut state.borrow_mut(), vec![cable.clone(), speakers.clone(), headphones.clone()]);
        apply_devices(&ui, &mut state.borrow_mut(), vec![cable.clone(), speakers.clone(), reconnect_device("bluetooth-other", "Headphones", false)]);
        assert_ne!(ui.get_output_index(), 0, "A missing explicit output must not masquerade as intentional System default");
        assert_ne!(ui.get_outputs().row_data(ui.get_output_index() as usize).unwrap().as_str(), "Headphones", "A different endpoint with the same name must not replace the remembered device");
        for (english, mode) in [(true, 1), (false, 0), (true, 0), (false, 1)] {
            ui.set_english(english);
            ui.set_mode(mode);
            ui.set_target(42);
            read_config(&ui, &mut state.borrow_mut());
            assert_eq!(state.borrow().settings.output_id, "bluetooth-original");
            state.borrow().settings.save(&data).unwrap();
            assert_eq!(Settings::load(&data).output_id, "bluetooth-original");
        }
        apply_devices(&ui, &mut state.borrow_mut(), vec![headphones, cable, speakers]);
        assert_eq!(ui.get_outputs().row_data(ui.get_output_index() as usize).unwrap().as_str(), "Headphones");
        assert_eq!(state.borrow().settings.output_id, "bluetooth-original");
        drop(state);
        fs::remove_file(data.join("settings.json")).unwrap();
        fs::remove_dir(data).unwrap();
    }

    #[test]
    fn device_reconnect_missing_source_survives_empty_inventory_and_config_changes() {
        let ui = reconnect_ui();
        let data = std::env::temp_dir().join(format!("maxi-104-reconnect-source-{}", std::process::id()));
        fs::create_dir_all(&data).unwrap();
        let mut settings = Settings::default();
        settings.source_id = "cable-original".into();
        let state = inert_state(data.clone(), settings);
        let source = reconnect_device("cable-original", "CABLE Input", true);
        apply_devices(&ui, &mut state.borrow_mut(), vec![source.clone()]);
        for inventory in [vec![reconnect_device("cable-other", "CABLE Input", true)], vec![]] {
            apply_devices(&ui, &mut state.borrow_mut(), inventory);
            assert_eq!(state.borrow().settings.source_id, "cable-original", "Inventory changes must not replace an explicit source preference");
            for (english, mode) in [(true, 0), (false, 1)] {
                ui.set_english(english);
                ui.set_mode(mode);
                read_config(&ui, &mut state.borrow_mut());
                assert_eq!(state.borrow().settings.source_id, "cable-original");
                state.borrow().settings.save(&data).unwrap();
                assert_eq!(Settings::load(&data).source_id, "cable-original");
            }
        }
        let reloaded = inert_state(data.clone(), Settings::load(&data));
        apply_devices(&ui, &mut reloaded.borrow_mut(), vec![]);
        assert!(ui.get_source_unavailable(), "A saved unavailable ID must survive a new UI with no cached friendly name");
        assert_eq!(reloaded.borrow().settings.source_id, "cable-original");
        apply_devices(&ui, &mut reloaded.borrow_mut(), vec![source]);
        assert_eq!(ui.get_sources().row_data(ui.get_source_index() as usize).unwrap().as_str(), "CABLE Input");
        assert!(!ui.get_source_unavailable());
        drop(reloaded);
        drop(state);
        fs::remove_file(data.join("settings.json")).unwrap();
        fs::remove_dir(data).unwrap();
    }

    #[test]
    fn device_reconnect_intentional_system_default_remains_distinct() {
        let ui = reconnect_ui();
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-default-inert"), Settings::default());
        for inventory in [vec![], vec![reconnect_device("speakers", "Speakers", false)]] {
            apply_devices(&ui, &mut state.borrow_mut(), inventory);
            for english in [true, false] {
                ui.set_english(english);
                read_config(&ui, &mut state.borrow_mut());
                assert_eq!(ui.get_output_index(), 0);
                assert!(state.borrow().settings.output_id.is_empty());
                assert_eq!(ui.get_outputs().row_data(0).unwrap().as_str(), if english { "System default" } else { "خروجی پیش‌فرض ویندوز" });
            }
        }
    }

    #[test]
    fn device_reconnect_start_rejects_missing_selection_but_stop_remains_available() {
        let ui = reconnect_ui();
        let mut settings = Settings::default();
        settings.output_id = "bluetooth-original".into();
        settings.source_id = "cable-original".into();
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-readiness-inert"), settings);
        ui.set_mode(1);
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("cable-original", "CABLE Input", true), reconnect_device("speakers", "Speakers", false)]);
        assert!(!audio_toggle_allowed(&ui, &state.borrow()), "Stale UI/tray/hotkey Start must reject a missing explicit output");
        ui.set_running(true);
        ui.set_devices_ready(false);
        for paused in [false, true] {
            ui.set_paused(paused);
            assert!(audio_toggle_allowed(&ui, &state.borrow()), "Missing inventory must never prevent Stop, including while paused");
        }
        ui.set_recovery_ready(false);
        state.borrow_mut().components.busy = true;
        assert!(audio_toggle_allowed(&ui, &state.borrow()), "Stop must bypass recovery and component gates too");
        ui.set_busy(true);
        assert!(!audio_toggle_allowed(&ui, &state.borrow()), "An already pending Stop must remain deduplicated");
        ui.set_busy(false);
        state.borrow_mut().exiting = true;
        assert!(!audio_toggle_allowed(&ui, &state.borrow()));
    }

    #[test]
    fn device_reconnect_availability_tracks_explicit_selection_and_localized_rows() {
        let ui = reconnect_ui();
        let mut settings = Settings::default();
        settings.output_id = "headphones".into();
        settings.source_id = "cable-original".into();
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-choices-inert"), settings);
        ui.set_mode(1);
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("headphones", "Headphones", false), reconnect_device("cable-original", "CABLE Input", true)]);
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("speakers", "Speakers", false), reconnect_device("cable-other", "CABLE Input", true)]);
        assert!(ui.get_output_unavailable() && ui.get_source_unavailable());
        assert!(!audio_toggle_allowed(&ui, &state.borrow()));
        let missing_output = ui.get_output_index();
        let missing_source = ui.get_source_index();
        for english in [true, false, true, false] {
            ui.set_english(english);
            read_config(&ui, &mut state.borrow_mut());
            let output = ui.get_outputs().row_data(missing_output as usize).unwrap();
            assert_eq!(output.as_str(), if english { "Unavailable · Headphones" } else { "در دسترس نیست · Headphones" });
            assert!(ui.get_output_unavailable() && ui.get_source_unavailable());
        }
        ui.set_output_index(0); // Intentional default is an available preference, not a missing ID.
        ui.set_mode(0);
        read_config(&ui, &mut state.borrow_mut());
        assert!(!ui.get_output_unavailable() && ui.get_source_unavailable());
        assert!(audio_toggle_allowed(&ui, &state.borrow()), "Windows mode does not require a virtual source");
        ui.set_mode(1);
        read_config(&ui, &mut state.borrow_mut());
        assert!(!audio_toggle_allowed(&ui, &state.borrow()), "Full mode does require its selected virtual source");
        ui.set_source_index(0); // Explicitly choose the available alternative cable.
        read_config(&ui, &mut state.borrow_mut());
        assert!(!ui.get_output_unavailable() && !ui.get_source_unavailable());
        assert!(audio_toggle_allowed(&ui, &state.borrow()));
        assert!(state.borrow().settings.output_id.is_empty());
        assert_eq!(state.borrow().settings.source_id, "cable-other");
        ui.set_output_index(missing_output);
        ui.set_source_index(missing_source);
        read_config(&ui, &mut state.borrow_mut());
        assert!(ui.get_output_unavailable() && ui.get_source_unavailable());
        assert_eq!(state.borrow().settings.output_id, "headphones");
        assert_eq!(state.borrow().settings.source_id, "cable-original");
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("cable-original", "CABLE renamed", true), reconnect_device("headphones", "Headphones renamed", false)]);
        assert!(!ui.get_output_unavailable() && !ui.get_source_unavailable());
        assert!(audio_toggle_allowed(&ui, &state.borrow()));
        ui.set_output_index(-1);
        read_config(&ui, &mut state.borrow_mut());
        assert!(ui.get_output_unavailable());
        assert!(!audio_toggle_allowed(&ui, &state.borrow()));
        assert_eq!(state.borrow().settings.output_id, "headphones", "An invalid index must not replace the saved preference");
    }

    #[test]
    fn device_reconnect_full_default_resolves_only_runtime_config_without_guessing() {
        let ui = reconnect_ui();
        ui.set_mode(1);
        ui.set_english(true);
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-resolver-inert"), Settings::default());
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("cable", "CABLE Input", true), reconnect_device("speakers", "Speakers", false)]);
        read_config(&ui, &mut state.borrow_mut());
        let config = prepare_audio_config(&ui, &state.borrow(), || Ok("speakers".into())).unwrap();
        assert_eq!(config.output_id, "speakers");
        assert!(state.borrow().settings.output_id.is_empty());
        assert_eq!(ui.get_output_index(), 0);
        for default in ["cable", "missing-headphones", ""] {
            assert!(prepare_audio_config(&ui, &state.borrow(), || Ok(default.into())).is_err(), "Full must not guess speakers when default={default:?}");
            assert!(state.borrow().settings.output_id.is_empty());
            assert_eq!(ui.get_output_index(), 0);
        }
        assert!(prepare_audio_config(&ui, &state.borrow(), || Err(anyhow::anyhow!("default query failed"))).is_err());
        ui.set_running(true); // A successful run binds config, while later controls retain default intent.
        ui.set_target(37);
        ui.set_english(false);
        read_config(&ui, &mut state.borrow_mut());
        assert!(state.borrow().settings.output_id.is_empty());
        assert_eq!(config.output_id, "speakers");
        assert_eq!(ui.get_output_index(), 0);
        ui.set_running(false);
        ui.set_mode(0);
        read_config(&ui, &mut state.borrow_mut());
        let windows = prepare_audio_config(&ui, &state.borrow(), || panic!("Windows default must retain its existing engine semantics")).unwrap();
        assert!(windows.output_id.is_empty());
        ui.set_mode(1);
        ui.set_output_index(1);
        read_config(&ui, &mut state.borrow_mut());
        let explicit = prepare_audio_config(&ui, &state.borrow(), || panic!("Explicit output must not query or follow Windows default")).unwrap();
        assert_eq!(explicit.output_id, "speakers");
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("cable", "CABLE Input", true)]);
        assert!(prepare_audio_config(&ui, &state.borrow(), || panic!("A missing selected row is not an active endpoint")).is_err());
    }

    #[test]
    fn automatic_routing_remembered_fallback_stays_runtime_only_and_rechecks_hardware() {
        let ui = reconnect_ui();
        ui.set_mode(1);
        ui.set_english(true);
        let settings = Settings { last_physical_output_id: "speakers".into(), ..Settings::default() };
        let state = inert_state(std::env::temp_dir().join("maxi-routing-hint-inert"), settings);
        let all = vec![reconnect_device("cable", "Renamed cable", true), reconnect_device("speakers", "Speakers", false), reconnect_device("headphones", "Headphones", false)];
        apply_devices(&ui, &mut state.borrow_mut(), all.clone());
        read_config(&ui, &mut state.borrow_mut());
        let config = prepare_audio_config(&ui, &state.borrow(), || Ok("cable".into())).unwrap();
        assert!(config.output_id.is_empty(), "The worker must freshly verify the remembered fallback");
        assert_eq!(audio_routing::resolve_output(&config, &all, Some("cable")).unwrap(), "speakers");
        assert!(state.borrow().settings.output_id.is_empty());
        assert_eq!(ui.get_output_index(), 0);
        let mut changed = all;
        changed[1].verified_physical = false;
        assert!(audio_routing::resolve_output(&config, &changed, Some("cable")).is_err());
        ui.set_output_index(2);
        read_config(&ui, &mut state.borrow_mut());
        let explicit = prepare_audio_config(&ui, &state.borrow(), || panic!("Explicit selection wins over the remembered hint")).unwrap();
        assert_eq!(explicit.output_id, "headphones");
        assert_eq!(state.borrow().settings.last_physical_output_id, "speakers");
    }

    #[test]
    fn automatic_routing_source_ambiguity_keeps_selection_explicit_and_localized() {
        let ui = reconnect_ui();
        ui.set_mode(1);
        ui.set_english(true);
        let state = inert_state(std::env::temp_dir().join("maxi-routing-source-inert"), Settings::default());
        let first = reconnect_device("cable-one", "First cable", true);
        let mut second = reconnect_device("cable-two", "Second cable", true);
        apply_devices(&ui, &mut state.borrow_mut(), vec![first.clone(), second.clone()]);
        assert!(state.borrow().settings.source_id.is_empty());
        assert!(ui.get_source_unavailable());
        assert_eq!(ui.get_sources().row_data(0).unwrap().as_str(), "Select a virtual input");
        assert!(!audio_toggle_allowed(&ui, &state.borrow()));
        second.system_default = true;
        apply_devices(&ui, &mut state.borrow_mut(), vec![first, second]);
        assert_eq!(state.borrow().settings.source_id, "cable-two");
        assert!(!ui.get_source_unavailable());
        // Once selected, a disconnected exact source remains selected.
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("cable-other", "Other cable", true)]);
        assert_eq!(state.borrow().settings.source_id, "cable-two");
        assert!(ui.get_source_unavailable());
    }

    #[test]
    fn device_reconnect_refresh_rechecks_shared_boundary_and_deduplicates_requests() {
        let ui = reconnect_ui();
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-refresh-inert"), Settings::default());
        let (jobs, requests) = std::sync::mpsc::channel();
        let (_results, events) = std::sync::mpsc::channel();
        state.borrow_mut().background = background::Background { jobs, events };
        // These channels capture requests without executing scans or native audio calls.
        for blocked in 0..6 {
            ui.set_running(blocked == 0 || blocked == 1);
            ui.set_paused(blocked == 1);
            ui.set_busy(blocked == 2);
            state.borrow_mut().components.busy = blocked == 3;
            state.borrow_mut().exiting = blocked == 4;
            state.borrow_mut().devices_pending = blocked == 5;
            assert!(!refresh(&ui, &mut state.borrow_mut()).unwrap());
            request_device_refresh(&ui, &mut state.borrow_mut());
            assert!(requests.try_recv().is_err(), "Refresh guard {blocked} submitted a scan");
            assert_eq!(ui.get_device_refresh_state(), 0);
        }
        state.borrow_mut().devices_pending = false;
        state.borrow_mut().exiting = false;
        state.borrow_mut().components.busy = false;
        ui.set_running(false); ui.set_paused(false); ui.set_busy(false);
        request_device_refresh(&ui, &mut state.borrow_mut());
        assert!(matches!(requests.try_recv().unwrap(), background::Job::Devices));
        assert!(state.borrow().devices_pending && !ui.get_devices_ready());
        assert_eq!(ui.get_device_refresh_state(), 1);
        request_device_refresh(&ui, &mut state.borrow_mut());
        assert!(requests.try_recv().is_err());
        apply_device_result(&ui, &mut state.borrow_mut(), Ok(vec![reconnect_device("speakers", "Speakers", false)]));
        assert_eq!(ui.get_device_refresh_state(), 2);
        assert!(ui.get_devices_ready() && !state.borrow().devices_pending);
        assert!(refresh(&ui, &mut state.borrow_mut()).unwrap()); // Internal component refresh stays quiet.
        assert!(matches!(requests.try_recv().unwrap(), background::Job::Devices));
        assert_eq!(ui.get_device_refresh_state(), 0);
        apply_device_result(&ui, &mut state.borrow_mut(), Ok(vec![]));
        assert_eq!(ui.get_device_refresh_state(), 0);
        drop(requests); // A failed submission also stays retryable and blocks stale Start.
        request_device_refresh(&ui, &mut state.borrow_mut());
        assert_eq!(ui.get_device_refresh_state(), 3);
        assert!(!ui.get_devices_ready() && !state.borrow().devices_pending);
    }

    #[test]
    fn device_reconnect_failed_inventory_retains_choices_errors_and_allows_retry() {
        let ui = reconnect_ui();
        let mut settings = Settings::default();
        settings.output_id = "headphones".into();
        settings.source_id = "cable".into();
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-scan-error-inert"), settings);
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("headphones", "Headphones", false), reconnect_device("cable", "CABLE Input", true)]);
        state.borrow_mut().error = Some("Earlier restoration is incomplete".into());
        ui.set_audio_error("Earlier restoration is incomplete".into());
        ui.set_status("Earlier restoration is incomplete".into());
        state.borrow_mut().devices_pending = true;
        ui.set_device_refresh_state(1);
        apply_device_result(&ui, &mut state.borrow_mut(), Err(anyhow::anyhow!("Inventory unavailable")));
        assert_eq!(ui.get_device_refresh_state(), 3);
        assert!(!state.borrow().devices_pending && !ui.get_devices_ready());
        assert!(!ui.get_output_unavailable() && !ui.get_source_unavailable(), "A failed scan must not falsely mark the old inventory disconnected");
        assert_eq!(ui.get_outputs().row_data(ui.get_output_index() as usize).unwrap().as_str(), "Headphones");
        assert_eq!(state.borrow().settings.output_id, "headphones");
        assert_eq!(state.borrow().settings.source_id, "cable");
        assert_eq!(ui.get_audio_error().as_str(), "Earlier restoration is incomplete");
        let (jobs, requests) = std::sync::mpsc::channel();
        let (_results, events) = std::sync::mpsc::channel();
        state.borrow_mut().background = background::Background { jobs, events };
        request_device_refresh(&ui, &mut state.borrow_mut());
        assert!(matches!(requests.try_recv().unwrap(), background::Job::Devices));
        assert_eq!(ui.get_device_refresh_state(), 1);
        apply_device_result(&ui, &mut state.borrow_mut(), Ok(vec![reconnect_device("headphones", "Headphones", false), reconnect_device("cable", "CABLE Input", true)]));
        assert_eq!(ui.get_device_refresh_state(), 2);
        assert!(ui.get_devices_ready());
        assert_eq!(state.borrow().error.as_deref(), Some("Earlier restoration is incomplete"));
        assert_eq!(ui.get_audio_error().as_str(), "Earlier restoration is incomplete");
        assert_eq!(ui.get_status().as_str(), "Earlier restoration is incomplete");
    }

    #[test]
    fn device_reconnect_recovery_retry_clears_only_its_owned_failure() {
        let ui = reconnect_ui();
        ui.set_recovery_ready(false);
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-recovery-error-inert"), Settings::default());
        let (jobs, requests) = std::sync::mpsc::channel();
        let (_results, events) = std::sync::mpsc::channel();
        state.borrow_mut().background = background::Background { jobs, events };
        request_device_refresh(&ui, &mut state.borrow_mut());
        assert!(matches!(requests.try_recv().unwrap(), background::Job::Initialize(_)));
        assert_eq!(ui.get_device_refresh_state(), 1);
        apply_initialization_result(&ui, &mut state.borrow_mut(), Err(anyhow::anyhow!("Reconnect previous output for recovery")));
        assert_eq!(ui.get_device_refresh_state(), 3);
        assert!(!state.borrow().devices_pending && !ui.get_recovery_ready() && !ui.get_devices_ready());
        assert_eq!(ui.get_audio_error().as_str(), "Reconnect previous output for recovery");
        request_device_refresh(&ui, &mut state.borrow_mut());
        assert!(matches!(requests.try_recv().unwrap(), background::Job::Initialize(_)));
        apply_initialization_result(&ui, &mut state.borrow_mut(), Ok(()));
        assert!(ui.get_recovery_ready());
        assert!(state.borrow().error.is_none());
        assert!(state.borrow().initialization_error.is_none());
        assert!(ui.get_audio_error().is_empty());
        assert_eq!(ui.get_device_refresh_state(), 1, "Initialization success still waits for inventory completion");
        assert!(state.borrow().devices_pending);
        apply_device_result(&ui, &mut state.borrow_mut(), Ok(vec![]));
        assert_eq!(ui.get_device_refresh_state(), 2);
        assert!(ui.get_audio_error().is_empty());
        assert!(state.borrow().error.is_none());
    }

    #[test]
    fn device_reconnect_recovery_retry_preserves_newer_errors_in_each_channel() {
        let ui = reconnect_ui();
        let state = inert_state(std::env::temp_dir().join("maxi-104-reconnect-owned-error-inert"), Settings::default());
        for (state_replaced, ui_replaced) in [(true, true), (true, false), (false, true)] {
            apply_initialization_result(&ui, &mut state.borrow_mut(), Err(anyhow::anyhow!("Original recovery failure")));
            if state_replaced { state.borrow_mut().error = Some("Newer safe-exit failure".into()); }
            if ui_replaced { ui.set_audio_error("Newer audio failure".into()); }
            apply_initialization_result(&ui, &mut state.borrow_mut(), Ok(()));
            let expected_state = if state_replaced { Some("Newer safe-exit failure") } else { None };
            let expected_ui = if ui_replaced { "Newer audio failure" } else { "" };
            assert_eq!(state.borrow().error.as_deref(), expected_state);
            assert_eq!(ui.get_audio_error().as_str(), expected_ui);
            assert!(state.borrow().initialization_error.is_none());
            apply_device_result(&ui, &mut state.borrow_mut(), Ok(vec![]));
            assert_eq!(state.borrow().error.as_deref(), expected_state, "Inventory success does not own audio/recovery errors");
            assert_eq!(ui.get_audio_error().as_str(), expected_ui);
        }
        // Ownership is consumed once; a later equal message has no stale owner.
        state.borrow_mut().error = Some("Original recovery failure".into());
        ui.set_audio_error("Original recovery failure".into());
        apply_initialization_result(&ui, &mut state.borrow_mut(), Ok(()));
        assert_eq!(state.borrow().error.as_deref(), Some("Original recovery failure"));
        assert_eq!(ui.get_audio_error().as_str(), "Original recovery failure");
    }

    #[test]
    fn tray_start_rechecks_readiness_and_never_toggles_a_running_engine() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Headless(adapter))).unwrap();
        let ui = AppWindow::new().unwrap();
        let state = inert_state(std::env::temp_dir().join("maxi-tray-start-inert"), Settings::default());
        let accepted = Rc::new(Cell::new(0));
        let count = accepted.clone(); let weak = ui.as_weak(); let callback_state = state.clone();
        // Replace only the service boundary: the shared production readiness
        // gate and adapter run; no initialization or audio jobs are submitted.
        ui.on_toggle_audio(move || {
            let ui = weak.upgrade().unwrap();
            let state = callback_state.borrow_mut();
            assert!(audio_toggle_allowed(&ui, &state));
            assert!(!ui.get_running(), "A stale Run must never reach the Stop branch");
            drop(state); // Match the production callback's mutable State access.
            count.set(count.get() + 1);
            ui.set_busy(true); // Accepted production start marks busy before returning.
        });
        apply_devices(&ui, &mut state.borrow_mut(), vec![reconnect_device("speakers", "Speakers", false)]);
        for blocked in 0..5 {
            ui.set_running(false);
            ui.set_busy(blocked == 0);
            state.borrow_mut().components.busy = blocked == 1;
            ui.set_devices_ready(blocked != 2);
            ui.set_recovery_ready(blocked != 3);
            state.borrow_mut().exiting = blocked == 4;
            handle_tray_start(&ui, &state);
            assert_eq!(accepted.get(), 0, "Readiness guard {blocked} was bypassed");
        }
        state.borrow_mut().exiting = false;
        ui.set_busy(false); ui.set_devices_ready(true); ui.set_recovery_ready(true);
        handle_tray_start(&ui, &state);
        handle_tray_start(&ui, &state); // Duplicate while the first start is pending.
        assert_eq!(accepted.get(), 1);
        ui.set_busy(false); ui.set_running(true);
        for paused in [false, true] {
            ui.set_paused(paused);
            handle_tray_start(&ui, &state); // Queued old Run after start completes.
            assert!(ui.get_running()); assert_eq!(ui.get_paused(), paused);
            assert_eq!(accepted.get(), 1);
        }
    }

    #[test]
    fn queued_wheel_cannot_cross_processing_modes() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Headless(adapter))).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.set_english(true);
        ui.set_hotkey_capture_index(-1);
        ui.show().unwrap();
        let data = std::env::temp_dir().join(format!("maxi-104-wheel-mode-{}", std::process::id()));
        fs::create_dir_all(&data).unwrap();
        let state = inert_state(data.clone(), Settings::default());
        let osd = Rc::new(RefCell::new(None));
        state.borrow().engine.stop();
        let (sender, receiver) = std::sync::mpsc::channel();
        for (origin, destination, initial, steps, expected) in [(0, 1, 100, 1, 100), (1, 0, 200, -1, 100)] {
            ui.set_mode(origin);
            ui.set_target(initial);
            read_config(&ui, &mut state.borrow_mut());
            sender.send(native_tray::Action::ScrollTarget { steps, x: 20, y: 30, windows_mode: origin == 0 }).unwrap();
            // The real UI commits config changes synchronously, before the
            // application timer next drains its native action channel.
            ui.set_mode(destination);
            read_config(&ui, &mut state.borrow_mut());
            ui.set_volume_notice("mode change complete".into());
            state.borrow_mut().save_at = None;
            state.borrow_mut().unmute_target = Some(77);
            let native_tray::Action::ScrollTarget { steps, x, y, windows_mode } = receiver.try_recv().unwrap() else { panic!("Expected queued wheel action"); };
            handle_tray_scroll(&ui, &state, &osd, windows_mode, steps, (x, y));
            assert_eq!(ui.get_target(), expected, "Old-context wheel action was reinterpreted after a mode change");
            let saved = state.borrow();
            assert_eq!(saved.settings.target, expected);
            assert_eq!(saved.engine.controls.target.load(Ordering::Relaxed), expected);
            assert_eq!(saved.unmute_target, Some(77));
            assert!(saved.save_at.is_none(), "Rejected action must not schedule another settings save");
            assert!(saved.engine.controls.stop.load(Ordering::Relaxed));
            assert_eq!(ui.get_volume_notice().as_str(), "mode change complete");
            drop(saved);
            // A fresh gesture for the new mode still takes the normal path.
            handle_tray_scroll(&ui, &state, &osd, destination == 0, -1, (x, y));
            assert_eq!(ui.get_target(), expected - 2);
            assert!(state.borrow().save_at.is_some());
            assert_eq!(state.borrow().unmute_target, None);
        }
        assert!(osd.borrow().is_none());
        drop(state);
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn target_notices_explain_inactive_playback_without_changing_audio_state() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Headless(adapter))).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.show().unwrap();
        let osd = Rc::new(RefCell::new(None));
        for (english, running, paused, leveling, title, detail) in [
            (true, false, false, true, "Playback stopped", "Saved · Run to apply"),
            (false, false, false, true, "پردازش متوقف است", "ذخیره شد؛ «اجرا» را بزنید"),
            (true, false, true, false, "Playback stopped", "Saved · Run to apply"),
            (false, false, true, false, "پردازش متوقف است", "ذخیره شد؛ «اجرا» را بزنید"),
            (true, true, true, true, "Playback paused", "Saved · Resume to apply"),
            (false, true, true, true, "پردازش در حالت مکث است", "ذخیره شد؛ «ادامه» را بزنید"),
            (true, true, false, true, "Volume target", ""),
            (false, true, false, true, "سطح صدای هدف", ""),
            (true, true, false, false, "Volume target", ""),
            (false, true, false, false, "سطح صدای هدف", ""),
        ] {
            ui.set_english(english);
            ui.set_running(running);
            ui.set_paused(paused);
            ui.set_leveling(leveling);
            for target in [0, 150] {
                ui.set_target(target);
                notify_target(&ui, &osd, Some((-1200, 30)));
                let expected = if detail.is_empty() { format!("{title} · {target}%") } else { format!("{title} · {target}% · {detail}") };
                assert_eq!(ui.get_volume_notice().as_str(), expected);
                assert_eq!((ui.get_running(), ui.get_paused(), ui.get_leveling(), ui.get_target()),
                    (running, paused, leveling, target));
                assert!(osd.borrow().is_none(), "Visible dashboard must retain its existing notification route");
            }
        }
        ui.hide().unwrap();
        assert!(!dashboard_accepts_notice(&ui), "Hidden dashboard must leave notices to the native OSD route");
    }

    #[test]
    fn wheel_uses_shared_target_and_preserves_paused_stopped_and_busy_states() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Headless(adapter))).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.set_english(true);
        ui.set_hotkey_capture_index(-1);
        ui.show().unwrap(); // Exercise the in-app notice route, without native windows.
        let data = std::env::temp_dir().join(format!("maxi-104-wheel-{}", std::process::id()));
        fs::create_dir_all(&data).unwrap();
        let mut settings = Settings::default();
        settings.output_id = "selected-output".into();
        settings.source_id = "selected-source".into();
        let state = inert_state(data.clone(), settings);
        let osd = Rc::new(RefCell::new(None));
        state.borrow().engine.stop();
        for (mode, ceiling) in [(0, 100), (1, 200)] {
            ui.set_mode(mode);
            ui.set_running(false);
            ui.set_paused(false);
            ui.set_target(0);
            handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, 1, (-1200, 30));
            assert_eq!(ui.get_target(), 2, "Wheel-up must not restore the hotkey's remembered target");
            assert_eq!(state.borrow().unmute_target, None);
            assert!(!ui.get_running() && !ui.get_paused());
            assert_eq!(ui.get_volume_notice().as_str(), "Playback stopped · 2% · Saved · Run to apply");
            assert!(state.borrow().engine.controls.stop.load(Ordering::Relaxed));
            handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, i32::MAX, (20, 30));
            assert_eq!(ui.get_target(), ceiling);
            handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, i32::MIN, (20, 30));
            assert_eq!(ui.get_target(), 0);
            // Simulated paused UI/engine controls verify the input adapter; no audio worker is started.
            ui.set_running(true);
            ui.set_paused(true);
            state.borrow().engine.controls.paused.store(true, Ordering::Relaxed);
            handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, 3, (20, 30));
            assert_eq!(ui.get_target(), 6);
            assert!(ui.get_paused() && state.borrow().engine.controls.paused.load(Ordering::Relaxed));
            assert_eq!(ui.get_volume_notice().as_str(), "Playback paused · 6% · Saved · Resume to apply");
            assert_eq!(state.borrow().settings.target, 6);
            assert_eq!(state.borrow().engine.controls.target.load(Ordering::Relaxed), 6);
        }
        for blocked in 0..4 {
            ui.set_busy(blocked == 0);
            state.borrow_mut().components.busy = blocked == 1;
            ui.set_hotkey_capture_index(if blocked == 2 { 0 } else { -1 });
            state.borrow_mut().exiting = blocked == 3;
            state.borrow_mut().save_at = None;
            handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, 2, (20, 30));
            assert_eq!(ui.get_target(), 6);
            assert!(state.borrow().save_at.is_none(), "Rejected wheel input must not save or update the engine");
        }
        state.borrow_mut().exiting = false;
        ui.set_busy(false);
        ui.set_hotkey_capture_index(-1);
        ui.set_paused(false);
        ui.set_leveling(false);
        handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, 1, (20, 30));
        assert_eq!(ui.get_volume_notice(), "Volume target · 8%");
        state.borrow().settings.save(&data).unwrap();
        let saved = Settings::load(&data);
        assert_eq!(saved.target, 8);
        assert_eq!((&*saved.output_id, &*saved.source_id), ("selected-output", "selected-source"));
        assert!(osd.borrow().is_none(), "Visible main UI must retain its existing in-app notification route");
        assert!(!data.join("recovery.json").exists());
        ui.set_mode(0);
        ui.set_target(50);
        handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, 1, (20, 30));
        assert_eq!(ui.get_target(), 52);
        handle_hotkey(&ui, &state, &osd, 2); // Actual mute handler, following accepted wheel input.
        assert_eq!(ui.get_target(), 0);
        assert_eq!(state.borrow().unmute_target, Some(52));
        handle_hotkey(&ui, &state, &osd, 2);
        assert_eq!(ui.get_target(), 52, "Mute restore must retain the target reached before mute");
        ui.set_target(100);
        handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, 1, (20, 30));
        handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, -1, (20, 30));
        assert_eq!(ui.get_target(), 98);
        ui.set_target(0);
        handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, -1, (20, 30));
        handle_tray_scroll(&ui, &state, &osd, ui.get_mode() == 0, 1, (20, 30));
        assert_eq!(ui.get_target(), 2);
        drop(state);
        fs::remove_dir_all(data).unwrap();
    }
}

#[cfg(test)]
mod dashboard_notice_tests {
    use super::*;
    use slint::platform::{software_renderer::{MinimalSoftwareWindow, RepaintBufferType}, Platform, PlatformError, WindowAdapter, WindowEvent, PointerEventButton};

    struct Headless(Rc<MinimalSoftwareWindow>, Rc<Cell<Duration>>);
    impl Platform for Headless {
        fn create_window_adapter(&self) -> std::result::Result<Rc<dyn WindowAdapter>, PlatformError> { Ok(self.0.clone()) }
        fn duration_since_start(&self) -> Duration { self.1.get() }
    }

    fn assert_notice_text_layout(ui: &AppWindow, pixels: &[slint::Rgb8Pixel]) {
        let card = ui.get_notice_bounds();
        let close = ui.get_notice_close_bounds();
        let text = ui.get_notice_text_bounds();
        let (usable_left, usable_right) = if ui.get_english() {
            (card.x + 18., close.x - 11.)
        } else {
            (close.x + close.width + 11., card.x + card.width - 18.)
        };
        assert!(text.x >= usable_left - 0.1 && text.x + text.width <= usable_right + 0.1,
            "Text must stay outside the close circle with an 11px gap");
        assert!((text.x + text.width / 2. - (usable_left + usable_right) / 2.).abs() < 0.1,
            "Text must be centered in the usable area excluding the close circle: text={text:?}, card={card:?}, close={close:?}");
        assert!(text.y >= card.y + 10.9 && text.y + text.height <= card.y + card.height - 10.9);
        // Check actual ink, too: geometry alone would miss a left/right-aligned
        // Text item occupying the correct rectangle. Union the ink rows so
        // Persian diacritics/dots do not get mistaken for independent lines.
        let stride = ui.window().size().width as usize;
        let mut ink_bounds: Option<(usize, usize)> = None;
        for y in text.y.ceil() as usize..(text.y + text.height).floor() as usize {
            let ink: Vec<_> = (usable_left.ceil() as usize..usable_right.floor() as usize)
                .filter(|&x| { let p = pixels[y * stride + x]; p.r > 180 && p.g > 190 && p.b > 205 }).collect();
            if let (Some(&left), Some(&right)) = (ink.first(), ink.last()) {
                ink_bounds = Some(match ink_bounds { Some((old_left, old_right)) => (old_left.min(left), old_right.max(right)), None => (left, right) });
            }
        }
        let (left, right) = ink_bounds.expect("Notification text must actually be painted");
        assert!(((left + right + 1) as f32 / 2. - (usable_left + usable_right) / 2.).abs() <= 3.,
            "Painted text must center in usable area, ink={left}..{right}, usable={usable_left}..{usable_right}");
    }

    #[test]
    fn notice_route_tracks_window_visibility_separately_from_minimization() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter, clock))).unwrap();
        let ui = AppWindow::new().unwrap();
        let osd = Rc::new(RefCell::new(None));
        assert!(!ui.window().is_visible());
        assert!(!dashboard_accepts_notice(&ui), "A hidden app must use the native OSD route");

        ui.show().unwrap();
        assert!(dashboard_accepts_notice(&ui));
        ui.set_english(true);
        notify_audio_failure(&ui, &osd, "خطا", "Audio failure", "Full diagnostic chain");
        assert!(ui.get_volume_notice().contains("Full diagnostic chain"));
        assert!(osd.borrow().is_none(), "The visible dashboard route must not create a native OSD");

        ui.window().set_minimized(true);
        // Slint keeps the shown component alive while the independent public
        // minimized property changes. This is the state a truth-table mock
        // would miss, and Winit synchronizes it from native window events.
        assert!(ui.window().is_visible(), "Minimizing must not be mistaken for hiding in this regression");
        assert!(ui.window().is_minimized());
        assert!(!dashboard_accepts_notice(&ui), "A shown but minimized app must use the native OSD route");

        ui.window().set_minimized(false);
        assert!(ui.window().is_visible());
        assert!(!ui.window().is_minimized());
        assert!(dashboard_accepts_notice(&ui));
        notify_action(&ui, &osd, osd::NoticeIcon::Volume, "ذخیره شد", "Restored target", "", "Dashboard restored");
        assert_eq!(ui.get_volume_notice(), "Restored target · Dashboard restored");

        ui.hide().unwrap();
        assert!(!ui.window().is_visible());
        assert!(!ui.window().is_minimized());
        assert!(!dashboard_accepts_notice(&ui), "Hiding must independently select the native OSD route");
        assert!(osd.borrow().is_none(), "The regression must not create native windows");
    }

    #[test]
    fn notice_lifecycle_renews_reverses_and_preserves_engine_errors() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        adapter.set_size(slint::LogicalSize::new(1280., 860.));
        let ui = AppWindow::new().unwrap();
        ui.show().unwrap();
        let draw = |millis: u64, capture: Option<&str>| {
            clock.set(clock.get() + Duration::from_millis(millis));
            slint::platform::update_timers_and_animations();
            let size = ui.window().size();
            let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
            window_repaint::full_redraw(ui.window());
            adapter.draw_if_needed(|renderer| { renderer.render(&mut pixels, size.width as usize); });
            if let (Some(name), Some(folder)) = (capture, std::env::var_os("MAXI_NOTICE_CAPTURE_DIR")) {
                let folder = PathBuf::from(folder);
                fs::create_dir_all(&folder).unwrap();
                let mut encoder = png::Encoder::new(fs::File::create(folder.join(format!("{name}.png"))).unwrap(), size.width, size.height);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect::<Vec<_>>()).unwrap();
            }
            pixels
        };
        draw(0, None);
        assert_eq!(ui.get_notice_reveal(), 0.);
        ui.invoke_show_volume_notice("ولوم هدف · ۱۰۰٪".into());
        draw(0, None);
        draw(100, Some("FA-entering"));
        let entering = ui.get_notice_reveal();
        let entering_y = ui.get_notice_bounds().y;
        assert!(entering > 0. && entering < 1., "Entry must animate: {entering}");
        draw(100, Some("FA-held"));
        assert_eq!(ui.get_notice_reveal(), 1.);
        assert!(ui.get_notice_bounds().y < entering_y);
        draw(2600, None);
        ui.invoke_show_volume_notice("ولوم هدف · ۱۰۰٪".into()); // Identical clamped input renews the deadline.
        draw(0, None);
        draw(400, None); // Past the original deadline.
        assert_eq!(ui.get_notice_reveal(), 1.);
        assert!(!ui.get_volume_notice().is_empty());
        draw(2600, None);
        draw(100, Some("FA-exiting"));
        let exiting = ui.get_notice_reveal();
        assert!(exiting > 0. && exiting < 1.);
        assert_eq!(ui.get_notice_display_text(), "ولوم هدف · ۱۰۰٪");
        ui.set_english(true);
        ui.invoke_show_volume_notice("New target · 90%".into());
        draw(0, None);
        assert!((ui.get_notice_reveal() - exiting).abs() < 0.01, "Renewing during exit must not jump");
        draw(200, Some("EN-renewed"));
        assert_eq!(ui.get_notice_reveal(), 1.);
        draw(200, None); // Past any old exit cleanup.
        assert_eq!(ui.get_volume_notice(), "New target · 90%");
        ui.invoke_dismiss_notice();
        draw(0, None);
        draw(100, None);
        let before_resize = ui.get_notice_bounds();
        let long_notice = "Saved target · 90%\nVirtual cable processing mode\nAudio configuration retained\nOpen the app log for details.";
        ui.invoke_show_volume_notice(long_notice.into());
        draw(0, None);
        let after_resize = ui.get_notice_bounds();
        assert!((after_resize.y - before_resize.y).abs() < 0.1, "Long replacement must not jump vertically");
        assert!((after_resize.height - before_resize.height).abs() < 0.1, "Card height must resize continuously");
        draw(50, Some("EN-long-renewing"));
        assert!(ui.get_notice_bounds().height > before_resize.height);
        ui.set_audio_error("Engine unavailable\nPersistent diagnostic\nCheck the output device\nOpen the app log\nTry again after recovery".into());
        draw(0, None);
        draw(50, None);
        assert!(ui.get_notice_display_text().starts_with("Engine unavailable"));
        ui.set_audio_error("".into());
        draw(0, None);
        assert_eq!(ui.get_notice_display_text(), long_notice);
        // Dismiss while the resized card is still growing, then renew again.
        ui.invoke_dismiss_notice();
        draw(0, None);
        draw(100, None);
        assert!(ui.get_notice_reveal() > 0. && ui.get_notice_reveal() < 1.);
        ui.invoke_show_volume_notice("New target · 90%".into());
        draw(0, None);
        draw(200, None);
        ui.set_audio_error("Persistent engine failure".into());
        draw(0, None);
        draw(3100, None);
        assert!(ui.get_volume_notice().is_empty());
        assert_eq!(ui.get_notice_reveal(), 1.);
        assert_eq!(ui.get_notice_display_text(), "Persistent engine failure");
        ui.invoke_show_volume_notice("Latest transient".into());
        ui.set_audio_error("".into()); // Recovery exposes a still-active newer transient.
        draw(0, None);
        assert_eq!(ui.get_notice_display_text(), "Latest transient");
        ui.invoke_dismiss_notice();
        draw(0, None);
        draw(100, None);
        ui.set_audio_error("Error arrived during exit".into());
        draw(0, None);
        draw(200, None);
        assert_eq!(ui.get_notice_reveal(), 1.);
        assert_eq!(ui.get_notice_display_text(), "Error arrived during exit");
        let close = ui.get_notice_close_bounds();
        let position = slint::LogicalPosition::new(close.x + close.width / 2., close.y + close.height / 2.);
        ui.window().dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
        ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
        draw(0, None);
        draw(100, None);
        assert!(ui.get_audio_error().is_empty());
        assert_eq!(ui.get_notice_display_text(), "Error arrived during exit");
        assert!(ui.get_notice_reveal() > 0. && ui.get_notice_reveal() < 1.);
        draw(100, Some("dismissed"));
        assert_eq!(ui.get_notice_reveal(), 0.);

        let osd = Rc::new(RefCell::new(None));
        for (english, label) in [(false, "FA"), (true, "EN")] {
            ui.set_english(english);
            for (width, height, size_label) in [(1280., 860., "normal"), (640., 320., "small")] {
                adapter.set_size(slint::LogicalSize::new(width, height));
                for (text, sample) in if english {
                    [("Volume target · 95%", "short"),
                     ("Playback stopped · 95% · Saved · Run to apply", "long-single"),
                     ("Saved target · 95%\nVirtual cable processing mode\nOpen the app log for details.", "wrapped")]
                } else {
                    [("سطح صدای هدف · 95%", "short"),
                     ("پردازش متوقف است · 95% · ذخیره شد؛ «اجرا» را بزنید", "long-single"),
                     ("سطح صدای هدف · 95%\nپردازش کابل مجازی\nبرای جزئیات، گزارش برنامه را باز کن.", "wrapped")]
                } {
                    ui.invoke_show_volume_notice(text.into());
                    draw(0, None);
                    let pixels = draw(400, Some(&format!("{label}-{size_label}-layout-{sample}")));
                    assert_notice_text_layout(&ui, &pixels);
                    if sample == "wrapped" { assert!(ui.get_notice_bounds().height > 54.); }
                }
                ui.invoke_show_volume_notice(if english { "Boost above 100 requires Full boost / virtual cable in Processing mode." } else { "برای بوست بالاتر از ۱۰۰، گزینهٔ بوست کامل / کابل مجازی را از حالت پردازش انتخاب کن." }.into());
                draw(0, None);
                draw(400, Some(&format!("{label}-{size_label}-long")));
                let bounds = ui.get_notice_bounds();
                assert!(bounds.width <= 520. && bounds.height >= 54.);
                assert!((bounds.x + bounds.width / 2. - ui.get_body_x() - ui.get_body_width() / 2.).abs() < 1.);
                assert!(bounds.y >= 0. && bounds.y + bounds.height <= height - 22.);
                let close = ui.get_notice_close_bounds();
                assert!(close.x >= bounds.x && close.x + close.width <= bounds.x + bounds.width);
                assert_eq!(close.height, 36.);
                assert!(close.y >= bounds.y + 9. && close.y + close.height <= bounds.y + bounds.height - 9.);
                let saved_audio_state = (ui.get_running(), ui.get_paused(), ui.get_target());
                for (running, paused, state_label) in [(true, false, "active"), (false, false, "stopped"), (true, true, "paused")] {
                    ui.set_running(running);
                    ui.set_paused(paused);
                    ui.set_target(95);
                    notify_target(&ui, &osd, None);
                    draw(0, None);
                    let pixels = draw(400, Some(&format!("{label}-{size_label}-target-{state_label}")));
                    assert_notice_text_layout(&ui, &pixels);
                    let bounds = ui.get_notice_bounds();
                    assert!(bounds.width <= 520. && bounds.height >= 54.);
                    assert!((bounds.x + bounds.width / 2. - ui.get_body_x() - ui.get_body_width() / 2.).abs() < 1., "Inactive notice must remain centered");
                    assert!(bounds.x >= 0. && bounds.x + bounds.width <= width);
                    assert!(bounds.y >= 0. && bounds.y + bounds.height <= height - 22.);
                    assert_eq!((ui.get_running(), ui.get_paused(), ui.get_target()), (running, paused, 95));
                    assert!(osd.borrow().is_none(), "Visible target notices must use the dashboard route");
                }
                ui.set_running(saved_audio_state.0);
                ui.set_paused(saved_audio_state.1);
                ui.set_target(saved_audio_state.2);
            }
        }
        draw(3000, None);
        draw(200, None);
        assert_eq!(ui.get_notice_reveal(), 0.);
        if std::env::var_os("MAXI_NOTICE_CAPTURE_DIR").is_some() {
            for (english, label) in [(false, "FA"), (true, "EN")] {
                adapter.set_size(slint::LogicalSize::new(1280., 760.));
                ui.set_english(english);
                ui.invoke_scroll_page(1.);
                draw(400, None);
                ui.invoke_preview_routing_help();
                draw(0, None);
                draw(400, Some(&format!("{label}-depth-help")));
                ui.invoke_close_routing_help();
                ui.set_mode(1);
                ui.invoke_preview_profile_menu();
                draw(0, None);
                draw(400, Some(&format!("{label}-depth-dropdown")));
                ui.invoke_close_profile_menu();
                ui.set_mode(0);
                let before_modal = draw(0, None);
                ui.invoke_preview_confirmation();
                let modal_pixels = draw(400, Some(&format!("{label}-depth-modal")));
                // Rounded fill must leave all four bounding-box corner pixels
                // showing only the modal scrim over the original dashboard.
                // This catches the software renderer's square-gradient bug.
                for (x, y) in [(408, 266), (871, 266), (408, 493), (871, 493)] {
                    let before = before_modal[y * 1280 + x];
                    let after = modal_pixels[y * 1280 + x];
                    for (background, actual, scrim) in [(before.r, after.r, 2), (before.g, after.g, 7), (before.b, after.b, 22)] {
                        let expected = (u32::from(background) * 79 + scrim * 176) / 255;
                        assert!((i32::from(actual) - expected as i32).abs() <= 3, "Rounded modal leaked fill at ({x}, {y}): {actual} vs {expected}");
                    }
                }
                ui.invoke_close_preview_confirmation();
                adapter.set_size(slint::LogicalSize::new(820., 760.));
                draw(400, None);
                ui.invoke_toggle_sidebar();
                draw(0, None);
                draw(400, Some(&format!("{label}-depth-compact-nav")));
                ui.invoke_dismiss_sidebar_menu();
                adapter.set_size(slint::LogicalSize::new(1280., 360.));
                ui.invoke_scroll_page(0.);
                draw(400, None);
                // Synthetic dense cyan/blue graph exercises the translucent
                // notice over active-looking content without starting audio.
                let history: VecDeque<LevelPoint> = (0..64).map(|i| LevelPoint { input: if i % 2 == 0 { 1. } else { 0.15 }, output: if i % 2 == 0 { 0.1 } else { 1. } }).collect();
                ui.set_input_path(graph_path(&history, ui.get_graph_width(), ui.get_graph_height(), false).into());
                ui.set_output_path(graph_path(&history, ui.get_graph_width(), ui.get_graph_height(), true).into());
                ui.set_running(true);
                ui.set_paused(false);
                ui.set_target(100);
                notify_target(&ui, &osd, None);
                draw(0, None);
                draw(400, Some(&format!("{label}-depth-graph-contrast")));
                ui.invoke_dismiss_notice();
                draw(200, None);
            }
        }
    }
}

#[cfg(test)]
mod hotkey_capture_tests {
    use super::*;

    #[test]
    fn capture_rejects_modifier_only_and_windows_reserved_chords() {
        assert!(capture_validation(0, 0x11, 0x0002).is_err());
        assert!(capture_validation(0, 0x09, 0x0001).is_err()); // Alt+Tab
        assert!(capture_validation(3, 0x73, 0x0001).is_err()); // Alt+F4
        assert!(capture_validation(3, 0x2E, 0x0003).is_err()); // Ctrl+Alt+Delete
        assert!(capture_validation(3, 0x51, 0x0003).is_ok()); // Ctrl+Alt+Q
    }

    #[test]
    fn media_rows_accept_only_unmodified_volume_keys() {
        assert!(capture_validation(11, 0xAF, 0).is_ok());
        assert!(capture_validation(12, 0x41, 0).is_err());
        assert!(capture_validation(13, 0xAD, 0x0002).is_err());
        assert!(capture_validation(14, 0xAD, 0).is_err());
    }

    #[test]
    fn rollback_policy_allows_old_failures_but_rejects_new_or_changed_binding_failures() {
        let previous = crate::settings::default_hotkeys();
        let old_error = vec![(2, "external conflict".to_string())];
        let candidate = previous.clone();
        assert!(new_registration_failures(&candidate, &previous, &old_error, &old_error, Some(4)).is_empty());

        let mut changed = previous.clone();
        changed[2].virtual_key = b'X' as u32;
        assert_eq!(new_registration_failures(&changed, &previous, &old_error, &old_error, None).len(), 1);

        let fresh = vec![(4, "Windows rejected it".to_string())];
        assert_eq!(new_registration_failures(&candidate, &previous, &[], &fresh, None).len(), 1);
        assert_eq!(new_registration_failures(&candidate, &previous, &[], &fresh, Some(4)).len(), 1);
    }
}
#[cfg(test)]
mod audio_control_mode_tests {
    use super::*;
    use slint::platform::{software_renderer::{MinimalSoftwareWindow, RepaintBufferType}, Platform, PlatformError, WindowAdapter, WindowEvent, PointerEventButton};
    use std::{cell::Cell, rc::Rc, time::Duration};

    struct Headless(Rc<MinimalSoftwareWindow>, Rc<Cell<Duration>>);
    impl Platform for Headless {
        fn create_window_adapter(&self) -> std::result::Result<Rc<dyn WindowAdapter>, PlatformError> { Ok(self.0.clone()) }
        fn duration_since_start(&self) -> Duration { self.1.get() }
    }

    fn click(ui: &AppWindow, bounds: ControlBounds) {
        let position = slint::LogicalPosition::new(bounds.x + bounds.width / 2., bounds.y + bounds.height / 2.);
        ui.window().dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
        ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
    }

    #[test]
    fn sound_controls_require_full_mode_without_losing_saved_preferences() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        adapter.set_size(slint::LogicalSize::new(1280., 860.));
        let ui = AppWindow::new().unwrap();
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        ui.set_page(0);
        ui.set_mode(1);
        ui.set_profile(3);
        ui.set_last_profile(3);
        ui.set_treble_smoothing(true);
        let saved = Rc::new(RefCell::new(Settings::default()));
        let changes = Rc::new(Cell::new(0usize));
        let weak = ui.as_weak();
        let saved_for_callback = saved.clone();
        let changes_for_callback = changes.clone();
        ui.on_config_changed(move || {
            let ui = weak.upgrade().unwrap();
            let mut settings = saved_for_callback.borrow_mut();
            settings.mode = ui.get_mode();
            settings.profile = ui.get_profile();
            settings.last_profile = ui.get_last_profile();
            settings.treble_smoothing = ui.get_treble_smoothing();
            changes_for_callback.set(changes_for_callback.get() + 1);
        });
        ui.invoke_config_changed();
        ui.show().unwrap();
        let settle = || {
            clock.set(clock.get() + Duration::from_millis(500));
            slint::platform::update_timers_and_animations();
            let size = ui.window().size();
            let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
            adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
        };
        settle();
        assert!(ui.get_enhancement_available() && ui.get_enhancement_checked_effective());
        assert!(ui.get_treble_available() && ui.get_treble_checked_effective());
        click(&ui, ui.get_enhancement_toggle_bounds());
        assert_eq!(ui.get_profile(), 0, "Full-mode mouse click must turn enhancement off");
        assert!(!ui.get_treble_available() && !ui.get_treble_checked_effective());
        ui.invoke_request_treble_smoothing(false);
        assert!(ui.get_treble_smoothing(), "Profile-off must preserve the treble preference");
        click(&ui, ui.get_enhancement_toggle_bounds());
        assert_eq!(ui.get_profile(), 3, "Full-mode toggle must restore the remembered profile");
        click(&ui, ui.get_treble_toggle_bounds());
        assert!(!ui.get_treble_smoothing(), "Full-mode mouse click must change treble preference");
        click(&ui, ui.get_treble_toggle_bounds());
        assert!(ui.get_treble_smoothing());
        ui.invoke_request_treble_smoothing(false); // Same request path as shortcut action 10.
        assert!(!saved.borrow().treble_smoothing);
        ui.invoke_request_treble_smoothing(true);
        assert!(saved.borrow().treble_smoothing);

        // Keep keyboard focus on a control while the mode changes.
        ui.set_mode(0);
        ui.invoke_config_changed();
        settle();
        let changes_before_blocked_inputs = changes.get();
        for english in [false, true] {
            ui.set_english(english);
            settle();
            assert_eq!(ui.get_processing_mode_warning().as_str(), if english { "Requires switching to virtual cable processing mode." } else { "نیازمند تغییر حالت به پردازش کابل مجازی" });
            assert!(ui.get_enhancement_hint_visible());
            assert!(!ui.get_enhancement_available() && !ui.get_enhancement_checked_effective());
            assert!(!ui.get_treble_available() && !ui.get_treble_checked_effective());
            click(&ui, ui.get_enhancement_toggle_bounds());
            click(&ui, ui.get_treble_toggle_bounds());
            let space: slint::SharedString = " ".into();
            ui.window().dispatch_event(WindowEvent::KeyPressed { text: space.clone() });
            ui.window().dispatch_event(WindowEvent::KeyReleased { text: space });
            ui.invoke_set_enhancement(false);
            ui.invoke_choose_profile(6);
            ui.invoke_request_treble_smoothing(false);
            assert_eq!((ui.get_profile(), ui.get_last_profile(), ui.get_treble_smoothing()), (3, 3, true));
            for action in 8..=10 {
                let blocked = unavailable_audio_control_shortcut(&ui, action).unwrap();
                assert_eq!(blocked.1, "Requires switching to virtual cable processing mode.");
            }
        }
        assert_eq!(changes.get(), changes_before_blocked_inputs, "Disabled controls must not persist changes");
        let persisted: Settings = serde_json::from_str(&serde_json::to_string(&*saved.borrow()).unwrap()).unwrap();
        assert_eq!((persisted.profile, persisted.last_profile, persisted.treble_smoothing), (3, 3, true));
        ui.set_mode(1);
        ui.invoke_config_changed();
        settle();
        assert!(ui.get_enhancement_checked_effective() && ui.get_treble_checked_effective());
        assert!(!ui.get_enhancement_hint_visible());
        assert!(unavailable_audio_control_shortcut(&ui, 8).is_none());
        assert!(unavailable_audio_control_shortcut(&ui, 9).is_none());
        assert!(unavailable_audio_control_shortcut(&ui, 10).is_none());
        if let Some(folder) = std::env::var_os("MAXI_AUDIO_CONTROLS_DIR") {
            let folder = std::path::PathBuf::from(folder);
            std::fs::create_dir_all(&folder).unwrap();
            for (mode, english, label) in [(0, false, "Windows-FA"), (0, true, "Windows-EN"), (1, true, "Full-EN")] {
                adapter.set_size(slint::LogicalSize::new(820., if mode == 0 && !english { 1000. } else { 760. }));
                ui.set_mode(mode);
                ui.set_english(english);
                ui.invoke_scroll_page(if mode == 0 && !english { 0.85 } else { 0.5 });
                settle();
                let size = ui.window().size();
                let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
                window_repaint::full_redraw(ui.window());
                adapter.draw_if_needed(|renderer| { renderer.render(&mut pixels, size.width as usize); });
                let path = folder.join(format!("{label}.png"));
                let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), size.width, size.height);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect::<Vec<_>>()).unwrap();
            }
        }
    }
}
struct Instance {
    mutex: windows::Win32::Foundation::HANDLE,
    event: windows::Win32::Foundation::HANDLE,
    owner: bool,
}
impl Instance {
    fn new() -> Result<Self> {
        use windows::{
            core::PCWSTR,
            Win32::{
                Foundation::{GetLastError, ERROR_ALREADY_EXISTS},
                System::Threading::{CreateEventW, CreateMutexW, SetEvent},
            },
        };
        let name = core_audio::wide("Local\\MaxiSoundSet.Rust.v2");
        let activation = core_audio::wide("Local\\MaxiSoundSet.Rust.v2.Activate");
        unsafe {
            let mutex = CreateMutexW(None, false, PCWSTR(name.as_ptr()))?;
            let owner = GetLastError() != ERROR_ALREADY_EXISTS;
            let event = CreateEventW(None, false, false, PCWSTR(activation.as_ptr()))?;
            if !owner {
                SetEvent(event)?;
            }
            Ok(Self {
                mutex,
                event,
                owner,
            })
        }
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
            let _ = windows::Win32::Foundation::CloseHandle(self.mutex);
        }
    }
}
#[cfg(test)]
mod audio_measurement_ui_tests {
    use super::*;
    use slint::platform::{
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        Platform, PlatformError, WindowAdapter,
    };
    use std::{cell::Cell, rc::Rc};

    struct Headless(Rc<MinimalSoftwareWindow>, Rc<Cell<Duration>>);
    impl Platform for Headless {
        fn create_window_adapter(&self) -> std::result::Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
        fn duration_since_start(&self) -> Duration { self.1.get() }
    }

    #[test]
    #[ignore = "Opt-in headless AUD-03 source-label captures"]
    fn capture_source_labels() {
        let directory = PathBuf::from(std::env::var_os("MAXI_AUD03_CAPTURE_DIR").expect("capture directory"));
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        ui.set_page(0);
        ui.set_running(true);
        ui.show().unwrap();
        adapter.set_size(slint::LogicalSize::new(640., 320.));
        ui.set_sidebar_expanded(false);
        for (language, english) in [("FA", false), ("EN", true)] {
            ui.set_english(english);
            ui.set_measurement_source(2);
            let mut pixels = Vec::new();
            for pass in 0..8 {
                clock.set(clock.get() + Duration::from_millis(250));
                slint::platform::update_timers_and_animations();
                if pass == 3 { ui.invoke_scroll_page(0.8); }
                let size = ui.window().size();
                pixels.resize((size.width * size.height) as usize, slint::Rgb8Pixel::default());
                window_repaint::full_redraw(ui.window());
                adapter.draw_if_needed(|renderer| { renderer.render(&mut pixels, size.width as usize); });
            }
            let size = ui.window().size();
            assert_eq!((size.width, size.height), (640, 320));
            assert!(ui.get_minimum_size_valid() && ui.get_sound_layout_fits(), "Sound layout overflow at minimum/{language}");
            fs::write(directory.join(format!("layout-minimum-{language}.txt")), format!("viewport={}x{}; minimum_size_valid={}; sound_layout_fits={}; sound_scroll_range={:?}; sound_scroll_offset={:?}\n", size.width, size.height, ui.get_minimum_size_valid(), ui.get_sound_layout_fits(), ui.get_sound_scroll_range(), ui.get_sound_scroll_offset())).unwrap();
            let path = directory.join(format!("source-minimum-{language}-peak.png"));
            let mut encoder = png::Encoder::new(fs::File::create(path).unwrap(), size.width, size.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect::<Vec<_>>()).unwrap();
        }
    }
}

fn main() {
    if let Err(e) = run() {
        log::error!("Application error: {e:#}");
        log::logger().flush();
        eprintln!("MAXI SOUNDSET: {e:#}");
        if std::env::args().nth(1).is_some_and(|a| a.starts_with("--")) {
            std::process::exit(1);
        }
        use windows::{
            core::PCWSTR,
            Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR},
        };
        let text = core_audio::wide(&format!("{e:#}\n\nSee Data/Logs/maxi.log."));
        let title = core_audio::wide("MAXI SOUNDSET");
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_ICONERROR,
            );
        }
    }
    // Drain queued records after the UI loop (or a diagnostic command) exits.
    log::logger().flush();
}
