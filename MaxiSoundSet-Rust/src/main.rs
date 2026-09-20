#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app_log;
mod app_update;
mod audio_engine;
mod background;
mod components;
mod conflicts;
mod core_audio;
mod dsp;
mod enhancer;
mod loudness;
mod native_tray;
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
    outputs: Vec<String>,
    sources: Vec<String>,
    history: VecDeque<LevelPoint>,
    save_at: Option<Instant>,
    error: Option<String>,
    audio_apps: Vec<conflicts::AudioApp>,
    conflict_scan_at: Instant,
}
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
fn refresh(ui: &AppWindow, state: &mut State) -> Result<()> {
    if !state.devices_pending && !state.exiting {
        state.background.submit(if ui.get_recovery_ready() {
            background::Job::Devices
        } else {
            background::Job::Initialize(state.data.clone())
        })?;
        state.devices_pending = true;
        ui.set_devices_ready(false);
    }
    Ok(())
}
fn apply_devices(ui: &AppWindow, state: &mut State, all: Vec<core_audio::DeviceInfo>) {
    let cable = all
        .iter()
        .any(|d| d.name.to_ascii_lowercase().contains("cable input"));
    let mut names = vec![if ui.get_english() {
        "System default"
    } else {
        "خروجی پیش‌فرض ویندوز"
    }
    .to_string()];
    state.outputs = vec![String::new()];
    let mut sources = Vec::new();
    state.sources.clear();
    for d in &all {
        if d.virtual_input {
            sources.push(d.name.clone());
            state.sources.push(d.id.clone());
        } else {
            names.push(d.name.clone());
            state.outputs.push(d.id.clone());
        }
    }
    if sources.is_empty() {
        sources.push("Install VB-CABLE first".into());
        state.sources.push(String::new());
    }
    ui.set_outputs(strings(names));
    ui.set_sources(strings(sources));
    ui.set_output_index(
        state
            .outputs
            .iter()
            .position(|id| *id == state.settings.output_id)
            .unwrap_or(0) as i32,
    );
    ui.set_source_index(
        state
            .sources
            .iter()
            .position(|id| *id == state.settings.source_id)
            .unwrap_or(0) as i32,
    );
    state.settings.source_id = state.sources[ui.get_source_index() as usize].clone();
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
    ui.set_devices_ready(true);
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
        ui.set_volume_notice(
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
    if s.settings.english != ui.get_english() {
        let mut labels: Vec<String> = ui.get_outputs().iter().map(|v| v.to_string()).collect();
        if let Some(first) = labels.first_mut() {
            *first = if ui.get_english() {
                "System default"
            } else {
                "خروجی پیش‌فرض ویندوز"
            }
            .into();
        }
        let selected = ui.get_output_index();
        ui.set_outputs(strings(labels));
        ui.set_output_index(selected);
    }
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
    if let Some(id) = s.outputs.get(ui.get_output_index().max(0) as usize) {
        s.settings.output_id = id.clone();
    }
    if let Some(id) = s.sources.get(ui.get_source_index().max(0) as usize) {
        s.settings.source_id = id.clone();
    }
    s.engine.update(&s.settings);
    app_log::AppLogger::set_debug(s.settings.debug_log);
    s.save_at = Some(Instant::now());
}
fn start_audio(ui: &AppWindow, s: &mut State) {
    read_config(ui, s);
    if s.settings.mode == 1 && s.settings.output_id.is_empty() {
        let id = core_audio::ComScope::new()
            .and_then(|_com| core_audio::default_id(1))
            .unwrap_or_default();
        if let Some(i) = s
            .outputs
            .iter()
            .position(|d| !d.is_empty() && *d == id)
            .or_else(|| s.outputs.iter().position(|d| !d.is_empty()))
        {
            ui.set_output_index(i as i32);
            s.settings.output_id = s.outputs[i].clone();
        }
    }
    let config = s.settings.clone();
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
fn exit(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let mut s = state.borrow_mut();
    if s.exiting {
        return;
    }
    if s.components.busy {
        log::info!("Exit requested during component operation; an already launched official installer remains independent");
    }
    s.exiting = true;
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
        ui.set_audio_error("Background worker is unavailable; retry Exit app.".into());
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let data = if args.get(1).is_some_and(|a| {
        [
            "--diagnose",
            "--components-check",
            "--engine-smoke",
            "--ui-smoke",
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
    let startup_launch = args.get(1).is_some_and(|a| a == "--startup");
    let instance = if smoke { None } else { Some(Instance::new()?) };
    if instance.as_ref().is_some_and(|i| !i.owner) {
        return Ok(());
    }
    let ui = AppWindow::new()?;
    set_eq_model(&ui, settings.eq_bands);
    ui.set_target(settings.target);
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
    let state = Rc::new(RefCell::new(State {
        background: background::Background::new(),
        devices_pending: true,
        conflicts_pending: false,
        exiting: false,
        components: components::Manager::new(&data),
        app_updates: app_update::Manager::new(),
        settings,
        data: data.clone(),
        engine: AudioEngine::new(),
        outputs: vec![],
        sources: vec![],
        history: VecDeque::new(),
        save_at: None,
        error: None,
        audio_apps: vec![],
        conflict_scan_at: Instant::now(),
    }));

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
                if !u.get_running() && !s.components.busy {
                    if let Err(e) = refresh(&u, &mut s) {
                        u.set_status(format!("{e:#}").into());
                        u.set_audio_error(format!("{e:#}").into());
                    }
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_toggle_audio(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                if u.get_busy()
                    || s.components.busy
                    || !u.get_devices_ready()
                    || !u.get_recovery_ready()
                    || s.exiting
                {
                    return;
                }
                s.error = None;
                u.set_audio_error("".into());
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
                if u.get_running() || u.get_busy() {
                    u.set_status("Stop audio before installing or updating a driver.".into());
                    return;
                }
                let data = s.data.clone();
                s.components.start(data, true);
                u.set_components_busy(true);
                u.set_download_progress(0.0);
            }
        });
    }
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_check_updates(move || {
            if let Some(u) = w.upgrade() {
                let mut s = s.borrow_mut();
                let data = s.data.clone();
                s.components.start(data, false);
                u.set_components_busy(true);
                u.set_component_message("Checking the official VB-Audio website…".into());
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
        ui.on_check_app_update(move || {
            if let Some(u) = w.upgrade() {
                if smoke {
                    u.set_app_update_state(2);
                    u.set_app_latest_version("v1.0.0".into());
                    return;
                }
                let mut s = s.borrow_mut();
                if !s.app_updates.busy {
                    s.app_updates.start(env!("CARGO_PKG_VERSION"));
                    u.set_app_update_state(1);
                    u.set_app_latest_version("".into());
                }
            }
        });
    }
    ui.on_clear_log_view(app_log::AppLogger::clear_view);
    {
        let w = ui.as_weak();
        let s = state.clone();
        ui.on_exit_app(move || {
            if let Some(u) = w.upgrade() {
                exit(&u, &s);
            }
        });
    }
    ui.show()?;
    window_repaint::after_show(&ui);
    state
        .borrow()
        .background
        .submit(background::Job::Initialize(data.clone()))?;
    if !smoke {
        state.borrow().background.submit(background::Job::Startup(
            state.borrow().settings.startup,
            std::env::current_exe()?,
        ))?;
    }
    // Winit creates its HWND when the event loop starts.
    let native: Rc<RefCell<Option<native_tray::Tray>>> = Rc::new(RefCell::new(None));
    {
        let native = native.clone();
        ui.window().on_close_requested(move || {
            if native
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
    let logs = Rc::new(VecModel::<LogRow>::default());
    ui.set_logs(logs.clone().into());
    let log_revision = std::cell::Cell::new(u64::MAX);
    let tray_retry = std::cell::Cell::new(Instant::now() - Duration::from_secs(5));
    {
        let w = ui.as_weak();
        let native = native.clone();
        let startup_hidden = startup_hidden.clone();
        let startup_resume = startup_resume.clone();
        let s = state.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                let Some(u) = w.upgrade() else {
                    return;
                };
                if native.borrow().is_none() && tray_retry.get().elapsed() >= Duration::from_secs(5) {
                    tray_retry.set(Instant::now());
                    match native_tray::Tray::new() {
                        Ok(tray) => *native.borrow_mut() = Some(tray),
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
                let actions: Vec<_> = native.borrow().as_ref().and_then(|tray| tray.events.as_ref()).map(|events| events.try_iter().collect()).unwrap_or_default();
                for action in actions {
                    match action {
                        native_tray::Action::Show => show(&u),
                        native_tray::Action::Repaint => { if u.window().is_visible() { window_repaint::full_redraw(u.window()); } }
                        native_tray::Action::Pause => u.invoke_toggle_pause(),
                        native_tray::Action::Target(v) => {
                            u.set_target(v);
                            u.invoke_config_changed();
                        }
                        native_tray::Action::Exit => {
                            u.invoke_exit_app();
                            return;
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
                            match result {
                                Ok(()) => { u.set_recovery_ready(true); s.error = None; u.set_audio_error("".into()); }
                                Err(e) => { s.devices_pending = false; let message = format!("{e:#}"); log::error!("Startup recovery: {message}"); s.error = Some(message.clone()); u.set_status(message.clone().into()); u.set_audio_error(message.into()); }
                            }
                        }
                        background::Event::Devices(result) => {
                            s.devices_pending = false;
                            match result {
                                Ok(devices) => apply_devices(&u, &mut s, devices),
                                Err(e) => { log::error!("Audio devices: {e:#}"); u.set_audio_error(format!("{e:#}").into()); }
                            }
                        }
                        background::Event::Conflicts(result) => { s.conflicts_pending = false; apply_conflicts(&u, &mut s, result); }
                        background::Event::Startup(enabled, result) => { match result { Ok(()) => { u.set_startup(enabled);
                                    if s.settings.startup != enabled { s.settings.startup = enabled; s.save_at = Some(Instant::now()); } u.set_startup_message(if u.get_english() { "Startup preference applied." } else { "تنظیم اجرای استارت‌آپ اعمال شد." }.into()); }, Err(e) => { log::error!("Windows startup: {e:#}"); u.set_startup(s.settings.startup); u.set_startup_message(format!("{e:#}").into()); } } }
                        background::Event::Saved(result) => { if let Err(e) = result { log::error!("Settings: {e:#}"); } }
                        background::Event::Exited(engine, result) => {
                            s.engine = engine;
                            match result {
                                Ok(()) => { let _ = slint::quit_event_loop(); return; }
                                Err(e) => { s.exiting = false; s.error = Some(format!("{e:#}")); u.set_busy(false); u.set_status(format!("{e:#}").into()); u.set_audio_error(format!("{e:#}").into()); show(&u); }
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
                    start_audio(&u, &mut s);
                }
                if s.conflict_scan_at.elapsed() >= Duration::from_secs(5) {
                    update_conflicts(&u, &mut s);
                }
                for event in s.engine.poll() {
                    match event {
                        AudioEvent::Started => {
                            u.set_running(true);
                            u.set_busy(false);
                            u.set_paused(false);
                            if !s.settings.was_running {
                                s.settings.was_running = true;
                                s.save_at = Some(Instant::now());
                            }
                        }
                        AudioEvent::Stopped => {
                            u.set_running(false);
                            u.set_busy(false);
                            u.set_paused(false);
                            if s.settings.was_running {
                                s.settings.was_running = false;
                                s.save_at = Some(Instant::now());
                            }
                        }
                        AudioEvent::Error(e) | AudioEvent::RestoreWarning(e) => {
                            u.set_audio_error(e.clone().into());
                            s.error = Some(e.clone());
                            u.set_status(e.into());
                            show(&u);
                        }
                    }
                }
                for event in s.components.poll() {
                    match event {
                        components::Event::Metadata(m) => {
                            u.set_latest_version(format!("Pack{}", m.latest).into());
                            u.set_update_available(m.latest > m.confirmed_pack);
                        }
                        components::Event::Progress(p) => u.set_download_progress(p),
                        components::Event::Message(m) => u.set_component_message(m.into()),
                        components::Event::Done(found) => {
                            u.set_components_busy(false);
                            u.set_cable_installed(found);
                            if let Err(e) = refresh(&u, &mut s) {
                                log::error!("Refresh after components check: {e:#}");
                            }
                        }
                        components::Event::Error(e) => {
                            u.set_components_busy(false);
                            u.set_component_message(e.into());
                        }
                    }
                }
                for event in s.app_updates.poll() {
                    match event {
                        app_update::Event::Found { version, update_available } => {
                            u.set_app_latest_version(version.into());
                            u.set_app_update_state(if update_available { 3 } else { 2 });
                        }
                        app_update::Event::Error(error) => {
                            u.set_app_update_state(4);
                            u.set_app_latest_version(error.into());
                        }
                    }
                }
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
                let m = snap.meters;
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
                if let Some(tray) = native.borrow_mut().as_mut() { tray.update(
                    u.get_running(),
                    u.get_paused(),
                    u.get_target(),
                    u.get_english(),
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
        let w = ui.as_weak();
        let native = native.clone();
        let directory = data.clone();
        let step = Rc::new(RefCell::new(0));
        let cable_before_warning = std::cell::Cell::new(false);
        smoke_timer.start(slint::TimerMode::Repeated, Duration::from_millis(1600), move || { let Some(u) = w.upgrade() else { return; }; let mut step = step.borrow_mut(); let result = (|| -> Result<()> { match *step {
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

        14 => { snapshot_png(&u, &directory.join("Routing-Help-FA.png"))?; u.invoke_close_routing_help(); u.set_page(4); u.set_leveling(false); u.set_intensity(2); u.set_treble_smoothing(false); u.set_target(0); u.invoke_config_changed(); u.set_startup(false); u.invoke_startup_changed(); u.set_startup(true); u.invoke_startup_changed(); u.set_startup_active(true); u.invoke_config_changed(); },
        15 => { let saved = Settings::load(&directory); anyhow::ensure!(!saved.leveling && saved.intensity == 2 && !saved.treble_smoothing && saved.target == 0 && saved.profile == 1 && saved.startup && saved.startup_active, "Independent features/intensity/smoothing/startup settings did not persist"); snapshot_png(&u, &directory.join("Settings-FA.png"))?; u.invoke_choose_profile(6); },
        16 => { anyhow::ensure!(Settings::load(&directory).profile == 6 && !u.get_leveling(), "Gaming profile depends on leveling"); u.set_page(6); },
        17 => { anyhow::ensure!(u.get_app_version() == "1.0.0", "About version mismatch"); u.invoke_check_app_update(); anyhow::ensure!(u.get_app_update_state() == 2 && u.get_app_latest_version() == "v1.0.0", "About current-version state failed"); snapshot_png(&u, &directory.join("About-FA.png"))?; u.set_app_update_state(4); u.set_app_latest_version("verification network error".into()); snapshot_png(&u, &directory.join("About-Update-Error-FA.png"))?; u.set_english(true); u.set_app_update_state(3); u.set_app_latest_version("v1.0.1".into()); },
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
        32 => { let registered = native.borrow().as_ref().is_some_and(|tray| tray.is_registered()); fs::write(directory.join("ui-smoke.txt"), format!("PASS: Slint 1.18.0 FA/EN RTL sidebar and row ordering; Sound/profiles/settings/conflicts/About/components/log/dropdown/collapsed/minimum-window rendering; profile 0/1/6/7, steadying intensity and optional treble smoothing persist; routing help popup with explicit close control and treble-reduction meter render; Persian/English sidebar icons remain anchored throughout collapse/expand animation; Donate action and startup audio-resume preference render; custom 10-band EQ and reset persist independently; English System default updates on language switch; manual volume/boost, leveling and enhancement independently enabled; remembered profile; startup preferences persist without touching host registry; About Rust/Slint logo credits, v1.0.0 branding, localized update states including error/log access, and compact website/report/license actions; aligned Log controls with regular spacing; proportional header fully fills available width with Playback 20% narrower than Processing; no host app termination; Windows mode max100 and full mode 0/200; custom EQ FA/EN/minimum size; native Close hides; native tray Show/Exit dispatch\nNotification-area registration: {}\n", if registered { "PASS" } else { "UNVERIFIED: rejected by this sandboxed Windows session" }))?; u.invoke_exit_app(); }, _ => () } Ok(()) })(); if let Err(e) = result { log::error!("UI smoke: {e:#}"); let _ = fs::write(directory.join("ui-smoke.txt"), format!("FAIL {e:#}")); let _ = slint::quit_event_loop(); } *step += 1; });
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
        .find(|d| d.virtual_input && d.name.contains("CABLE Input"))
        .or_else(|| devices.iter().find(|d| d.virtual_input))
        .context("An installed virtual cable is required for isolated leveler verification")?
        .id
        .clone();
    let tone = TestTone::start(id.clone());
    let ep = core_audio::Endpoint::new(&id)?;
    let original = ep.scalar()?;
    let muted = ep.muted()?;
    let mut e = AudioEngine::new();
    e.start(
        Settings {
            target: 100,
            output_id: id.clone(),
            ..Default::default()
        },
        data.to_path_buf(),
    )?;
    std::thread::sleep(Duration::from_millis(800));
    let events = e.poll();
    anyhow::ensure!(
        events.iter().any(|v| matches!(v, AudioEvent::Started)),
        "Engine failed to start: {events:?}"
    );
    anyhow::ensure!(
        ep.scalar()? <= 1.0 && ep.scalar()? > 0.0,
        "Windows actuator must remain within its native range"
    );
    let snap = *e.snapshot.lock().unwrap();
    anyhow::ensure!(
        !snap.silent && (dsp::db(snap.meters.output as f64) - dsp::target_db(100)).abs() < 1.0,
        "Native leveler did not normalize the isolated tone: {:?}",
        (snap.meters.input, snap.meters.output, ep.db()?)
    );
    let source_rms = snap.meters.input;
    e.controls
        .target
        .store(50, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(400));
    anyhow::ensure!(
        ep.scalar()? <= 1.0 && ep.scalar()? > 0.0,
        "Target 50 must remain audible while using the shared loudness scale"
    );
    let snap = *e.snapshot.lock().unwrap();
    anyhow::ensure!(
        (dsp::db(snap.meters.output as f64) - dsp::target_db(50)).abs() < 1.0,
        "Native target50 not reached"
    );
    anyhow::ensure!(
        (dsp::db(snap.meters.input as f64 / source_rms as f64)).abs() < 0.3,
        "Loopback is not pre-volume on this device"
    );
    e.controls
        .target
        .store(0, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(150));
    if !muted {
        anyhow::ensure!(ep.scalar()? < 0.001, "Target 0 did not mute");
    }
    e.controls
        .target
        .store(100, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(120));
    anyhow::ensure!(
        ep.scalar()? > 0.001,
        "Returning from target0 must restore audible Windows gain"
    );
    e.controls
        .target
        .store(0, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    e.controls
        .leveling
        .store(false, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(120));
    anyhow::ensure!(
        (ep.scalar()? - original).abs() < 0.001,
        "Disabling leveling at target 0 did not restore Windows volume"
    );
    e.controls
        .leveling
        .store(true, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    e.pause();
    std::thread::sleep(Duration::from_millis(150));
    anyhow::ensure!(
        (ep.scalar()? - original).abs() < 0.001,
        "Pause did not restore original volume"
    );
    e.pause();
    std::thread::sleep(Duration::from_millis(100));
    e.shutdown()?;
    anyhow::ensure!(
        (ep.scalar()? - original).abs() < 0.001
            && ep.muted()? == muted
            && core_audio::default_id(1)? == initial_defaults[1],
        "Audio state was not restored"
    );
    anyhow::ensure!(
        !data.join("recovery.json").exists(),
        "Recovery state remains"
    );
    drop(tone);
    // Exercise the real capture/render worker with independently enabled profiles.
    // Manual routing avoids touching the user's Windows default device assignments.
    let devices = core_audio::devices()?;
    let source = devices
        .iter()
        .find(|d| d.name.to_ascii_lowercase().starts_with("cable input"))
        .context("Full processing smoke requires installed VB-CABLE")?;
    let output = devices
        .iter()
        .find(|d| !d.virtual_input && d.name.contains("Realtek"))
        .or_else(|| devices.iter().find(|d| !d.virtual_input))
        .context("No physical playback output")?;
    let defaults: Vec<_> = (0..3).map(core_audio::default_id).collect::<Result<_>>()?;
    let physical_ep = core_audio::Endpoint::new(&output.id)?;
    let physical_original = physical_ep.scalar()?;
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
            && !events.iter().any(|v| matches!(v, AudioEvent::Error(_))),
        "Full capture/render worker failed: {events:?}"
    );
    anyhow::ensure!(
        (physical_ep.scalar()? - physical_original).abs() < 0.001,
        "Profile-only playback changed physical master volume"
    );
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
        !e.poll().iter().any(|v| matches!(v, AudioEvent::Error(_))),
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
    anyhow::ensure!(
        (ep.scalar()? - original).abs() < 0.001
            && ep.muted()? == muted
            && !data.join("recovery.json").exists(),
        "Final audio state was not restored"
    );
    fs::write(data.join("engine-smoke.txt"), "PASS: native Windows endpoint control; targets100/50 use the common loudness scale and native actuator stays in range; target 0 mutes; returning to100 restores native gain; pause/resume; leveling off at target 0 restores volume; stop restores volume, mute and default output; actual VB-CABLE capture and physical render worker starts at target 0 with leveling off/profile on; profiles 1/7/0 and custom EQ +4.5dB, independent feature switching, pause and shutdown run without errors; manual routing preserves all three Windows defaults. Isolated440Hz tone verifies native target100/50 within1dB and pre-volume loopback remains stable within0.3dB. Physical output headroom is reserved only for active nonzero leveling and released on pause/off/stop; profile-only and zero target leave master volume unchanged. No listening assessment or measured hardware frequency response.\n")?;
    Ok(())
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
