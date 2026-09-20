#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod operations;
mod platform;
mod running_app;
use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};
slint::include_modules!();
fn terms(english: bool) -> ModelRc<Term> {
    let text = if english {
        include_str!("../assets/EULA_EN.md")
    } else {
        include_str!("../assets/EULA_FA.md")
    };
    let mut items = text
        .split("\n\n")
        .filter(|s| !s.is_empty())
        .flat_map(|paragraph| {
            paragraph.lines().map(|line| {
                let line = line.trim_start_matches("- ");
                if let Some(rest) = line.strip_prefix("**") {
                    if let Some((heading, body)) = rest.split_once("**") {
                        return Term {
                            heading: heading.into(),
                            body: body.trim().replace("**", "").into(),
                        };
                    }
                }
                Term {
                    heading: "".into(),
                    body: line.replace("**", "").into(),
                }
            })
        })
        .collect::<Vec<_>>();
    items.push(Term {
        heading: if english {
            "End of agreement".into()
        } else {
            "پایان توافق‌نامه".into()
        },
        body: "".into(),
    });
    Rc::new(VecModel::from(items)).into()
}
#[cfg(test)]
mod agreement_tests {
    use super::*;
    use slint::Model;

    #[test]
    fn both_agreements_include_every_section_and_the_final_paragraph() {
        for english in [false, true] {
            let agreement = terms(english);
            assert!(agreement.row_count() >= 12);
            let rows = (0..agreement.row_count())
                .filter_map(|index| agreement.row_data(index))
                .map(|row| format!("{} {}", row.heading, row.body))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(rows.contains(if english {
                "6. Changes and Updates to This Agreement"
            } else {
                "۶. تغییرات و به‌روزرسانی توافق‌نامه"
            }));
            assert!(rows.contains(if english {
                "Continued use of any version"
            } else {
                "تداوم استفاده کاربر از هر یک از نسخه‌های نرم‌افزار"
            }));
        }
        assert!(!include_str!("../assets/EULA_EN.md").contains('—'));
    }
}
fn version_label(english: bool) -> String {
    let _ = english;
    format!("v{}", operations::VERSION)
}
fn bootstrap() -> Result<Option<PathBuf>> {
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_some_and(|s| s == "--uninstall-root") {
        return Ok(Some(operations::local_canonical(Path::new(
            args.get(2).context("Missing uninstall location")?,
        ))?));
    }
    let exe = std::env::current_exe()?;
    if exe
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Uninstall.exe"))
    {
        let root = operations::local_canonical(exe.parent().unwrap())?;
        operations::manifest(&root)?;
        let helper_dir =
            std::env::temp_dir().join(format!("MAXI-SOUNDSET-Uninstall-{}", operations::nonce()));
        fs::create_dir(&helper_dir)?;
        let helper = helper_dir.join("Uninstall.exe");
        fs::copy(&exe, &helper)?;
        let mut command = std::process::Command::new(helper);
        command.arg("--uninstall-root").arg(root);
        if args.get(1).is_some_and(|s| s == "--uninstall-self-test") {
            command
                .arg("--uninstall-self-test")
                .arg(args.get(2).context("Missing helper report")?);
        }
        command.spawn()?;
        std::process::exit(0)
    }
    Ok(None)
}
fn run() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|s| s == "--self-test") {
        return self_test(Path::new(args.get(2).context("Missing fixture directory")?));
    }
    let _com = platform::Com::new()?;
    let uninstall_root = bootstrap()?;
    let _helper_guard = TempHelperGuard(args.get(1).is_some_and(|s| s == "--uninstall-root"));
    if let Some(index) = args.iter().position(|s| s == "--uninstall-self-test") {
        let root = uninstall_root.context("Missing uninstall root")?;
        std::thread::sleep(Duration::from_millis(250));
        operations::uninstall(&root, true, |_| {})?;
        fs::write(
            args.get(index + 1).context("Missing helper report")?,
            serde_json::to_vec(
                &serde_json::json!({"root":root,"helper":std::env::current_exe()?}),
            )?,
        )?;
        return Ok(());
    }
    let smoke = args.get(1).is_some_and(|s| s == "--ui-smoke");
    let ui = InstallerWindow::new()?;
    ui.set_version(operations::VERSION.into());
    ui.set_version_label(version_label(false).into());
    ui.set_terms(terms(false));
    ui.set_uninstall(uninstall_root.is_some());
    let path = if let Some(p) = uninstall_root {
        p
    } else if let Some(p) = platform::text(platform::UNINSTALL_KEY, "InstallLocation")? {
        PathBuf::from(p)
    } else {
        platform::default_path()?
    };
    if let Ok(m) = operations::manifest(&path) {
        ui.set_desktop_shortcut(m.desktop);
        ui.set_startup(m.startup);
    }
    if !ui.get_uninstall() {
        if let Ok(data) = fs::read(path.join("Data/settings.json")) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data) {
                if let Some(v) = value["startup"].as_bool() {
                    ui.set_startup(v);
                }
                if let Some(v) = value["english"].as_bool() {
                    ui.set_english(v);
                    ui.set_terms(terms(v));
                    ui.set_version_label(version_label(v).into());
                }
            }
        }
    }
    ui.set_install_path(path.display().to_string().into());
    let w = ui.as_weak();
    ui.on_language_changed(move || {
        if let Some(u) = w.upgrade() {
            u.set_terms(terms(u.get_english()));
            u.set_version_label(version_label(u.get_english()).into());
        }
    });
    let w = ui.as_weak();
    ui.on_browse(move || {
        if let Some(u) = w.upgrade() {
            match platform::browse() {
                Ok(Some(p)) => u.set_install_path(p.display().to_string().into()),
                Ok(None) => (),
                Err(e) => u.set_error(format!("{e:#}").into()),
            }
        }
    });
    let w = ui.as_weak();
    ui.on_back(move || {
        if let Some(u) = w.upgrade() {
            u.set_error("".into());
            u.set_step((u.get_step() - 1).max(0));
        }
    });
    ui.on_cancel(|| {
        let _ = slint::quit_event_loop();
    });
    let w = ui.as_weak();
    ui.on_close_app(move || {
        let Some(u) = w.upgrade() else { return };
        if u.get_busy() || !u.get_uninstall() { return; }
        let root = PathBuf::from(u.get_install_path().as_str());
        u.set_busy(true);
        u.set_closing_app(true);
        u.set_error("".into());
        let weak = u.as_weak();
        std::thread::spawn(move || {
            let result = running_app::close(&root);
            let running = running_app::is_running(&root).unwrap_or(true);
            let _ = weak.upgrade_in_event_loop(move |u| {
                u.set_busy(false);
                u.set_closing_app(false);
                u.set_app_running(running);
                if let Err(e) = result {
                    u.set_error(format!("{}: {e:#}", if u.get_english() { "Safe shutdown did not finish. Use Exit app and retry" } else { "خروج امن کامل نشد. از گزینهٔ خروج از برنامه استفاده کن و دوباره تلاش کن" }).into());
                }
            });
        });
    });
    let w = ui.as_weak();
    ui.window().on_close_requested(move || {
        if w.upgrade().is_some_and(|u| u.get_busy()) {
            slint::CloseRequestResponse::KeepWindowShown
        } else {
            slint::CloseRequestResponse::HideWindow
        }
    });
    let w = ui.as_weak();
    ui.on_next(move || {
        let Some(u) = w.upgrade() else { return };
        u.set_error("".into());
        match u.get_step() {
            0 => u.set_step(1),
            1 if !u.get_uninstall() => {
                if u.get_accepted() {
                    u.set_step(2)
                }
            }
            4 => {
                if !u.get_uninstall() && u.get_launch_app() {
                    let root = PathBuf::from(u.get_install_path().as_str());
                    if let Err(e) = std::process::Command::new(root.join("MaxiSoundSet.exe"))
                        .current_dir(root)
                        .spawn()
                    {
                        u.set_error(format!("{e}").into());
                        return;
                    }
                }
                let _ = slint::quit_event_loop();
            }
            1 | 2 => {
                let root = PathBuf::from(u.get_install_path().as_str());
                let uninstall = u.get_uninstall();
                if uninstall {
                    match running_app::is_running(&root) {
                        Ok(true) => {
                            u.set_app_running(true);
                            return;
                        }
                        Ok(false) => u.set_app_running(false),
                        Err(e) => {
                            u.set_error(format!("{e:#}").into());
                            return;
                        }
                    }
                }
                let delete = u.get_delete_data();
                let options = operations::Options {
                    accepted: u.get_accepted(),
                    desktop: u.get_desktop_shortcut(),
                    startup: u.get_startup(),
                    integrated: true,
                    english: u.get_english(),
                };
                let weak = u.as_weak();
                u.set_busy(true);
                u.set_progress(0.);
                u.set_step(3);
                std::thread::spawn(move || {
                    let notifier = weak.clone();
                    let callback = move |progress| {
                        let _ = notifier.upgrade_in_event_loop(move |u| u.set_progress(progress));
                    };
                    let result = if uninstall {
                        operations::uninstall(&root, delete, callback).map(|_| root)
                    } else {
                        std::env::current_exe()
                            .context("Missing installer executable")
                            .and_then(|exe| operations::install(&root, &exe, options, callback))
                    };
                    let _ = weak.upgrade_in_event_loop(move |u| {
                        u.set_busy(false);
                        match result {
                            Ok(p) => {
                                u.set_install_path(p.display().to_string().into());
                                u.set_progress(1.);
                                u.set_step(4)
                            }
                            Err(e) => {
                                u.set_step(if uninstall { 1 } else { 2 });
                                u.set_error(
                                    format!(
                                        "{}: {e:#}",
                                        if u.get_english() {
                                            "Operation failed"
                                        } else {
                                            "انجام عملیات ناموفق بود"
                                        }
                                    )
                                    .into(),
                                );
                            }
                        }
                    });
                });
            }
            _ => (),
        }
    });
    let timer = slint::Timer::default();
    let running_timer = slint::Timer::default();
    if ui.get_uninstall() && !smoke {
        let weak = ui.as_weak();
        let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        running_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(500),
            move || {
                let Some(u) = weak.upgrade() else { return };
                if u.get_busy()
                    || u.get_step() >= 3
                    || pending.swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    return;
                }
                let root = PathBuf::from(u.get_install_path().as_str());
                let weak = u.as_weak();
                let pending = pending.clone();
                std::thread::spawn(move || {
                    let result = running_app::is_running(&root);
                    let _ = weak.upgrade_in_event_loop(move |u| match result {
                        Ok(running) => u.set_app_running(running),
                        Err(e) => {
                            u.set_app_running(true);
                            u.set_error(format!("{e:#}").into());
                        }
                    });
                    pending.store(false, std::sync::atomic::Ordering::Relaxed);
                });
            },
        );
    }
    let motion_timer = slint::Timer::default();
    if smoke {
        let dir = PathBuf::from(args.get(2).context("Missing screenshot folder")?);
        fs::create_dir_all(&dir)?;
        let w = ui.as_weak();
        let index = Rc::new(std::cell::Cell::new(0));
        let motion_seen = Rc::new(std::cell::Cell::new(false));
        let observed = motion_seen.clone();
        let motion_window = ui.as_weak();
        motion_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                if let Some(u) = motion_window.upgrade() {
                    let p = u.get_page_motion_progress();
                    if p > 0.02 && p < 0.98 {
                        observed.set(true);
                    }
                }
            },
        );
        timer.start(slint::TimerMode::Repeated,Duration::from_millis(700),move||{let Some(u)=w.upgrade()else{return};let i=index.get();let result=(||->Result<()>{match i{0=>snapshot(&u,&dir.join("Welcome-FA.png"))?,1=>u.set_step(1),2=>{snapshot(&u,&dir.join("Agreement-FA.png"))?;anyhow::ensure!(u.get_agreement_content_height()>u.get_agreement_visible_height(),"Persian agreement is not scrollable");u.set_agreement_scroll_y(-(u.get_agreement_content_height()-u.get_agreement_visible_height()));},3=>{snapshot(&u,&dir.join("Agreement-End-FA.png"))?;u.set_agreement_scroll_y(0.);u.set_step(2);u.set_accepted(true)},4=>snapshot(&u,&dir.join("Location-FA.png"))?,5=>{u.set_english(true);u.invoke_language_changed();u.set_step(0)},6=>snapshot(&u,&dir.join("Welcome-EN.png"))?,7=>u.set_step(1),8=>{snapshot(&u,&dir.join("Agreement-EN.png"))?;anyhow::ensure!(u.get_agreement_content_height()>u.get_agreement_visible_height(),"English agreement is not scrollable");u.set_agreement_scroll_y(-(u.get_agreement_content_height()-u.get_agreement_visible_height()));},9=>{snapshot(&u,&dir.join("Agreement-End-EN.png"))?;u.set_agreement_scroll_y(0.);u.set_step(3);u.set_busy(true);u.set_progress(0.56)},10=>snapshot(&u,&dir.join("Installing-EN.png"))?,11=>{u.set_busy(false);u.set_step(4)},12=>snapshot(&u,&dir.join("Finish-EN.png"))?,13=>{u.set_uninstall(true);u.set_step(1);u.set_english(false);u.invoke_language_changed()},14=>{u.set_app_running(true);snapshot(&u,&dir.join("Uninstall-Running-FA.png"))?;u.set_english(true);u.invoke_language_changed()},15=>{snapshot(&u,&dir.join("Uninstall-Running-EN.png"))?;u.set_closing_app(true);u.set_busy(true)},16=>{snapshot(&u,&dir.join("Uninstall-Closing-EN.png"))?;u.set_closing_app(false);u.set_busy(false);u.set_app_running(false)},17=>snapshot(&u,&dir.join("Uninstall-EN.png"))?,18=>{anyhow::ensure!(motion_seen.get(),"No animated intermediate page state observed");fs::write(dir.join("ui-smoke.txt"),"PASS: bilingual FA/EN installer; complete Persian and English agreements scroll to their final section and end marker; RTL step placement; Vazir; welcome/agreement/location/progress/finish/uninstall screens; app version v1.0.0; updated GPLv3 terms; MaxiSpace.dev / 2026 and brand rights; animated intermediate page states observed; isolated screenshots only; no host installation performed\n")?;let _=slint::quit_event_loop();},_=>()}Ok(())})();if let Err(e)=result{let _=fs::write(dir.join("ui-smoke.txt"),format!("FAIL {e:#}"));let _=slint::quit_event_loop();}index.set(i+1)});
    }
    ui.run()?;
    Ok(())
}
fn snapshot(ui: &InstallerWindow, path: &Path) -> Result<()> {
    let pixels = ui.window().take_snapshot()?;
    let file = fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, pixels.width(), pixels.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(pixels.as_bytes())?;
    Ok(())
}
fn self_test(dir: &Path) -> Result<()> {
    let _com = platform::Com::new()?;
    fs::create_dir_all(dir)?;
    let _scope = platform::TestScope::new(&dir.join("integration"))?;
    let root = dir.join(format!("MAXI تست {}", operations::nonce()));
    let exe = std::env::current_exe()?;
    let options = operations::Options {
        accepted: true,
        desktop: true,
        startup: true,
        integrated: true,
        english: false,
    };
    let installed = operations::install(&root, &exe, options.clone(), |_| {})?;
    anyhow::ensure!(
        fs::read(installed.join("MaxiSoundSet.exe"))? == operations::APP,
        "Payload mismatch"
    );
    anyhow::ensure!(
        platform::text(platform::UNINSTALL_KEY, "DisplayVersion")?.as_deref()
            == Some(operations::VERSION),
        "Uninstall registration version failed"
    );
    for (p, t) in platform::shortcut_paths(true)? {
        anyhow::ensure!(
            platform::same(&platform::shortcut_target(&p)?, &installed.join(t)),
            "Shortcut target mismatch"
        );
    }
    fs::create_dir_all(installed.join("Data"))?;
    fs::write(
        installed.join("Data/settings.json"),
        r#"{"startup":true,"target":77,"eq_bands":[1,2,3,4,5,6,7,8,9,10]}"#,
    )?;
    fs::write(installed.join("personal.txt"), "unrelated file")?;
    operations::install(&installed, &exe, options.clone(), |_| {})?;
    anyhow::ensure!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(
            installed.join("Data/settings.json")
        )?)?["target"]
            == 77,
        "Upgrade lost settings"
    );
    let saved_settings = fs::read(installed.join("Data/settings.json"))?;
    let saved_marker = fs::read(installed.join(".maxi-install.json"))?;
    let saved_startup = platform::text(platform::RUN_KEY, "MaxiSoundSet")?;
    let mut rollback_options = options.clone();
    rollback_options.startup = false;
    platform::test_fail_registration_once();
    anyhow::ensure!(
        operations::install(&installed, &exe, rollback_options, |_| {}).is_err(),
        "Late failure unexpectedly succeeded"
    );
    anyhow::ensure!(
        fs::read(installed.join("Data/settings.json"))? == saved_settings
            && fs::read(installed.join(".maxi-install.json"))? == saved_marker
            && fs::read(installed.join("MaxiSoundSet.exe"))? == operations::APP,
        "Failed upgrade did not restore original files and settings"
    );
    anyhow::ensure!(
        platform::text(platform::RUN_KEY, "MaxiSoundSet")? == saved_startup,
        "Rollback lost startup registration"
    );
    for (p, t) in platform::shortcut_paths(true)? {
        anyhow::ensure!(
            platform::same(&platform::shortcut_target(&p)?, &installed.join(t)),
            "Rollback lost shortcut"
        );
    }
    anyhow::ensure!(
        !fs::read_dir(&installed)?
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with(".maxi-stage-")),
        "Rollback left staging files"
    );
    let mut unsafe_marker: serde_json::Value = serde_json::from_slice(&saved_marker)?;
    unsafe_marker["files"] = serde_json::json!(["../personal.txt"]);
    fs::write(
        installed.join(".maxi-install.json"),
        serde_json::to_vec(&unsafe_marker)?,
    )?;
    anyhow::ensure!(
        operations::uninstall(&installed, true, |_| {}).is_err()
            && installed.join("personal.txt").exists()
            && installed.join("MaxiSoundSet.exe").exists(),
        "Unsafe ownership manifest accepted"
    );
    fs::write(installed.join(".maxi-install.json"), &saved_marker)?;
    operations::uninstall(&installed, false, |_| {})?;
    anyhow::ensure!(
        installed.join("Data/settings.json").exists()
            && installed.join("personal.txt").exists()
            && !installed.join("Uninstall.exe").exists(),
        "Removal ownership failed"
    );
    let mut no_start = options.clone();
    no_start.startup = false;
    platform::test_set_startup("external portable command")?;
    operations::install(&installed, &exe, no_start, |_| {})?;
    operations::uninstall(&installed, true, |_| {})?;
    anyhow::ensure!(
        !installed.join("Data").exists() && installed.join("personal.txt").exists(),
        "Optional data removal failed"
    );
    anyhow::ensure!(
        platform::text(platform::RUN_KEY, "MaxiSoundSet")?.as_deref()
            == Some("external portable command"),
        "Other startup command was removed"
    );
    anyhow::ensure!(
        platform::text(platform::UNINSTALL_KEY, "InstallLocation")?.is_none(),
        "Uninstall registration remains"
    );
    let bootstrap_root = dir.join(format!("bootstrap-{}", operations::nonce()));
    operations::install(
        &bootstrap_root,
        &exe,
        operations::Options {
            accepted: true,
            desktop: false,
            startup: false,
            integrated: false,
            english: false,
        },
        |_| {},
    )?;
    let helper_report = dir.join(format!("helper-{}.json", operations::nonce()));
    // Run only the installed fixture in smoke mode: no host startup changes or audio playback.
    let mut app = std::process::Command::new(bootstrap_root.join("MaxiSoundSet.exe"))
        .arg("--ui-smoke")
        .arg(dir.join("running-app-ui"))
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !running_app::is_running(&bootstrap_root)? {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "Fixture app did not start"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    fs::create_dir_all(bootstrap_root.join("Data"))?;
    fs::write(
        bootstrap_root.join("Data/preserve-until-exit.txt"),
        "private fixture",
    )?;
    anyhow::ensure!(
        operations::uninstall(&bootstrap_root, true, |_| {}).is_err(),
        "Running app did not block removal"
    );
    anyhow::ensure!(
        bootstrap_root.join("Data/preserve-until-exit.txt").exists()
            && bootstrap_root.join("MaxiSoundSet.exe").exists(),
        "Blocked removal changed files/data"
    );
    anyhow::ensure!(
        !running_app::is_running(&installed)?,
        "Another installation was incorrectly marked running"
    );
    running_app::close(&bootstrap_root)?;
    anyhow::ensure!(
        app.wait()?.success() && !running_app::is_running(&bootstrap_root)?,
        "Cooperative app shutdown failed"
    );
    anyhow::ensure!(
        bootstrap_root.join("Data/preserve-until-exit.txt").exists(),
        "Close request deleted data before uninstall"
    );
    let status = std::process::Command::new(bootstrap_root.join("Uninstall.exe"))
        .arg("--uninstall-self-test")
        .arg(&helper_report)
        .status()?;
    anyhow::ensure!(status.success(), "Uninstall bootstrap failed");
    for _ in 0..100 {
        if helper_report.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let helper_result: serde_json::Value = serde_json::from_slice(
        &fs::read(&helper_report).context("Temporary uninstall helper did not finish")?,
    )?;
    anyhow::ensure!(
        !bootstrap_root.exists(),
        "Uninstall bootstrap left installation files"
    );
    let helper = PathBuf::from(helper_result["helper"].as_str().unwrap());
    for _ in 0..30 {
        if !helper.exists() {
            break;
        }
        if fs::remove_file(&helper).is_ok() {
            let _ = fs::remove_dir(helper.parent().unwrap());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    fs::write(dir.join("self-test.txt"),"PASS: running private app blocks uninstall without modifying files/data; process detection is scoped to installation; Close software requests cooperative Exit and waits for process completion; data stays intact until removal; installed Uninstall.exe launches temporary Rust helper and removes full installation including requested Data; native ShellLink targets and Start-menu/desktop links; registry version and uninstall entry; isolated HKCU test keys cleaned; unrelated startup preserved; single-file embedded payload exact; Unicode/spaces install path; upgrade preserves settings; late failed upgrade restores files/settings/registry/shortcuts; malicious ownership manifest rejected; uninstall preserves user data by default and unrelated files; explicit data deletion; owned marker; no host registry or shortcuts changed\n")?;
    Ok(())
}
struct TempHelperGuard(bool);
impl Drop for TempHelperGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = platform::cleanup_temp_helper();
        }
    }
}
fn main() {
    if let Err(e) = run() {
        let message = format!("MAXI SOUNDSET: {e:#}");
        let _ = fs::write(
            std::env::temp_dir().join("MaxiSoundSet-Setup-error.txt"),
            &message,
        );
        if std::env::args()
            .any(|s| s == "--self-test" || s == "--ui-smoke" || s == "--uninstall-self-test")
        {
            eprintln!("{message}");
            std::process::exit(1);
        }
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::*;
            let _ = MessageBoxW(
                None,
                windows::core::PCWSTR(platform::wide(&message).as_ptr()),
                windows::core::PCWSTR(platform::wide("MAXI SOUNDSET").as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}
