//! Recover the whole software-rendered surface when a native window is shown.
//! A redraw request alone retains Slint's partial-rendering cache: an erased
//! native buffer can therefore remain white outside the changing controls.
use i_slint_core::{
    lengths::{LogicalPoint, LogicalRect, LogicalSize},
    partial_renderer::DirtyRegion,
    window::WindowInner,
};
use slint::ComponentHandle;
use std::time::Duration;

pub fn full_redraw(window: &slint::Window) {
    let size = window.size().to_logical(window.scale_factor());
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }
    let mut dirty = DirtyRegion::default();
    dirty.add_rect(LogicalRect::new(
        LogicalPoint::new(0.0, 0.0),
        LogicalSize::new(size.width, size.height),
    ));
    // This internal API is deliberately isolated. i-slint-core is pinned to
    // the same exact version as Slint; review this bridge when upgrading Slint.
    WindowInner::from_pub(window)
        .window_adapter()
        .renderer()
        .mark_dirty_region(dirty);
    window.request_redraw();
}

pub fn resize_logically(window: &slint::Window, width: f32, height: f32) {
    // Winit's runtime LogicalSize uses the OS DPI, whereas SLINT_SCALE_FACTOR
    // overrides Slint's DPI. Convert explicitly so smoke tests use the same
    // logical viewport under both real DPI and a test scale override.
    window.set_size(slint::LogicalSize::new(width, height).to_physical(window.scale_factor()));
}

pub fn install_focus_reveal(ui: &crate::AppWindow) {
    use slint::ComponentHandle;
    let weak = ui.as_weak();
    ui.global::<crate::Viewport>().on_reveal(move |x, y, height| {
        if let Some(ui) = weak.upgrade() { ui.invoke_reveal_control(x, y, height); }
    });
}

pub fn after_show(ui: &crate::AppWindow) {
    fit_native_work_area();
    full_redraw(ui.window());
    // Native creation/presentation is asynchronous. Cover the first settled
    // frames as well, without resizing, changing UI properties, or continuously
    // disabling partial rendering. Hidden startup windows need no repaint.
    for delay in [100, 250] {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            if let Some(ui) = weak.upgrade() {
                if ui.window().is_visible() {
                    fit_native_work_area();
                    full_redraw(ui.window());
                }
            }
        });
    }
}

/// Work-area and window bounds are both physical outer rectangles, including
/// the title bar and resize borders. Negative monitor origins are valid.
fn bounded_rect(rect: [i32; 4], work: [i32; 4]) -> [i32; 4] {
    if work[2] <= work[0] || work[3] <= work[1] { return rect; }
    let width = (rect[2] - rect[0]).min(work[2] - work[0]).max(1);
    let height = (rect[3] - rect[1]).min(work[3] - work[1]).max(1);
    let left = rect[0].clamp(work[0], work[2] - width);
    let top = rect[1].clamp(work[1], work[3] - height);
    [left, top, left + width, top + height]
}

#[cfg(windows)]
pub(crate) fn fit_native_work_area() {
    use windows::Win32::{Foundation::RECT, Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST}, UI::WindowsAndMessaging::{GetWindowRect, IsIconic, IsZoomed, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER}};
    #[link(name = "user32")]
    extern "system" { fn GetAsyncKeyState(key: i32) -> i16; }
    let Some(hwnd) = crate::native_tray::own_window() else { return; };
    unsafe {
        // Never fight an active mouse drag or Windows' maximized sizing.
        if crate::native_tray::window_is_moving() || GetAsyncKeyState(1) < 0 || IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() { return; }
        let mut rect = RECT::default();
        let mut monitor = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetWindowRect(hwnd, &mut rect).is_err() || !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut monitor).as_bool() { return; }
        let original = [rect.left, rect.top, rect.right, rect.bottom];
        let work = monitor.rcWork;
        let fitted = bounded_rect(original, [work.left, work.top, work.right, work.bottom]);
        if fitted != original {
            let _ = SetWindowPos(hwnd, None, fitted[0], fitted[1], fitted[2] - fitted[0], fitted[3] - fitted[1], SWP_NOACTIVATE | SWP_NOZORDER);
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn fit_native_work_area() {}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::{
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        Platform, PlatformError, WindowAdapter,
    };
    use std::{cell::Cell, rc::Rc};

    struct Headless(Rc<MinimalSoftwareWindow>, Rc<Cell<Duration>>);
    #[test]
    #[ignore = "Opt-in software render timing, writes verification evidence"]
    fn animation_render_timing() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        let ui = crate::AppWindow::new().unwrap();
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        adapter.set_size(slint::LogicalSize::new(1280., 860.));
        ui.show().unwrap();
        let size = ui.window().size();
        let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
        let mut timings = Vec::new();
        for transition in 0..12 {
            ui.set_sidebar_expanded(transition % 2 == 0);
            ui.set_page([0, 4, 2, 6][transition % 4]);
            for _ in 0..20 {
                clock.set(clock.get() + Duration::from_millis(16));
                slint::platform::update_timers_and_animations();
                let started = std::time::Instant::now();
                if adapter.draw_if_needed(|renderer| { renderer.render(&mut pixels, size.width as usize); }) {
                    timings.push(started.elapsed().as_secs_f64() * 1000.);
                }
            }
        }
        timings.sort_by(f64::total_cmp);
        let result = format!("Software rendering, 1280x860, sidebar/page transitions, {} frames: median {:.2} ms, p95 {:.2} ms, max {:.2} ms; optimized={}\n", timings.len(), timings[timings.len()/2], timings[timings.len()*95/100], timings[timings.len()-1], !cfg!(debug_assertions));
        if let Some(path) = std::env::var_os("MAXI_RENDER_TIMING_FILE") { std::fs::write(path, &result).unwrap(); }
        eprintln!("{result}");
    }
    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
        fn duration_since_start(&self) -> Duration {
            self.1.get()
        }
    }

    #[test]
    fn outer_bounds_fit_work_area_and_negative_monitor_origins() {
        assert_eq!(bounded_rect([40, 40, 1320, 900], [0, 0, 1280, 680]), [0, 0, 1280, 680]);
        assert_eq!(bounded_rect([-2400, 50, -1300, 810], [-1920, 0, 0, 1040]), [-1920, 50, -820, 810]);
        assert_eq!(bounded_rect([60, 30, 1100, 600], [0, 0, 1280, 680]), [60, 30, 1100, 600]);
        let fitted = bounded_rect([1000, 650, 1640, 970], [0, 0, 1280, 680]);
        assert_eq!(fitted, [640, 360, 1280, 680]);
        assert_eq!(bounded_rect(fitted, [0, 0, 1280, 680]), fitted);
    }

    fn press(ui: &crate::AppWindow, key: impl Into<slint::SharedString>) {
        let text = key.into();
        ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    }

    fn click(ui: &crate::AppWindow, bounds: crate::ControlBounds) {
        let position = slint::LogicalPosition::new(bounds.x + bounds.width / 2., bounds.y + bounds.height / 2.);
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button: slint::platform::PointerEventButton::Left });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button: slint::platform::PointerEventButton::Left });
    }

    #[test]
    fn components_check_result_and_header_geometry_r3() {
        use slint::platform::WindowEvent;
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        let ui = crate::AppWindow::new().unwrap();
        ui.set_devices_ready(true);
        ui.set_recovery_ready(true);
        ui.set_page(1);
        let checks = Rc::new(Cell::new(0));
        let count = checks.clone();
        ui.on_check_updates(move || count.set(count.get() + 1));
        let installs = Rc::new(Cell::new(0));
        let count = installs.clone();
        ui.on_install_components(move || count.set(count.get() + 1));
        ui.show().unwrap();
        let settle = || {
            clock.set(clock.get() + Duration::from_millis(500));
            slint::platform::update_timers_and_animations();
            let size = ui.window().size();
            let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
            adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
        };
        let capture = |label: &str| {
            if let Some(folder) = std::env::var_os("MAXI_COMPONENTS_R3_DIR") {
                let folder = std::path::PathBuf::from(folder);
                std::fs::create_dir_all(&folder).unwrap();
                let size = ui.window().size();
                let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
                full_redraw(ui.window());
                adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
                let mut encoder = png::Encoder::new(std::fs::File::create(folder.join(format!("{label}.png"))).unwrap(), size.width, size.height);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r,p.g,p.b]).collect::<Vec<_>>()).unwrap();
            }
        };
        for english in [false, true] {
            ui.set_english(english);
            for scale in [1., 1.25, 1.5] {
                ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
                for (width, height) in [(640., 600.), (837., 650.), (1099., 720.)] {
                    adapter.set_size(slint::LogicalSize::new(width, height));
                    settle();
                    settle(); // Complete the sidebar width animation after a size change.
                    let logical = ui.window().size().to_logical(ui.window().scale_factor());
                    assert!((logical.width - width).abs() < 1., "Unexpected logical viewport at {width}/{scale}: {}", logical.width);
                    assert!((ui.get_body_width() - (width - 84.)).abs() < 1., "Compact sidebar has not settled at {width}/{scale}: body={}", ui.get_body_width());
                    let heading = ui.get_component_heading_bounds();
                    let button = ui.invoke_component_control_bounds(0);
                    assert!(!ui.get_component_header_stacked(), "Header unexpectedly stacked at logical {width}/{scale}");
                    if ui.get_component_header_stacked() {
                        assert!(button.y >= heading.y + heading.height, "Fallback row overlaps heading at {width}/{scale}");
                        assert!((if english {button.x - heading.x} else {button.x + button.width - heading.x - heading.width}).abs() < 1., "Fallback button lost header edge at {width}/{scale}");
                    } else {
                        assert!((heading.y - button.y).abs() < 20., "Header button left its row at {width}/{scale}");
                        if english {
                            assert!(heading.x + heading.width + 7. <= button.x + 0.1, "English header overlap at {width}/{scale}");
                        } else {
                            assert!(button.x + button.width + 7. <= heading.x + 0.1, "Persian header overlap at {width}/{scale}");
                        }
                    }
                    if width == 640. {
                        assert!(ui.get_components_actions_centered(), "Card actions shifted at {width}/{scale}");
                        if scale == 1.25 { capture(if english {"en-min"} else {"fa-min"}); }
                    }
                    if width == 1099. && scale == 1.5 { capture(if english {"en-wide"} else {"fa-wide"}); }
                }
            }
            adapter.set_size(slint::LogicalSize::new(480., 650.));
            settle();
            if ui.get_component_header_stacked() {
                let heading = ui.get_component_heading_bounds();
                let button = ui.invoke_component_control_bounds(0);
                assert!(button.y >= heading.y + heading.height, "Fallback row overlaps heading");
                assert!((if english {button.x - heading.x} else {button.x + button.width - heading.x - heading.width}).abs() < 1., "Fallback button lost header edge");
            }
        }
        adapter.set_size(slint::LogicalSize::new(640., 600.));
        ui.set_english(true);
        settle();
        click(&ui, ui.invoke_component_control_bounds(0));
        assert_eq!(checks.get(), 1);
        ui.set_latest_version("Pack45".into());
        ui.set_download_progress(0.75);
        crate::begin_component_check(&ui);
        assert!(ui.get_components_busy());
        assert_eq!(ui.get_download_progress(), 0.);
        assert_eq!(ui.get_latest_version(), "");
        assert!(!ui.get_component_progress_visible());
        assert_eq!(ui.get_component_display_message(), "Checking the official VB-Audio website…");
        click(&ui, ui.invoke_component_control_bounds(0));
        assert_eq!(checks.get(), 1, "Concurrent check was accepted");
        crate::finish_component_check(&ui, true, crate::components::CheckOutcome::NewerAvailable);
        assert!(!ui.get_components_busy());
        assert_eq!(ui.get_component_display_message(), "A newer VB-CABLE package is available.");
        capture("en-newer");
        crate::begin_component_check(&ui);
        crate::finish_component_check(&ui, true, crate::components::CheckOutcome::NoNewer);
        assert_eq!(ui.get_component_display_message(), "No newer VB-CABLE package is available.");
        crate::begin_component_check(&ui);
        crate::finish_component_check(&ui, true, crate::components::CheckOutcome::InstalledVersionUnknown);
        assert_eq!(ui.get_component_display_message(), "VB-CABLE is installed; its package version is unknown.");
        crate::begin_component_check(&ui);
        crate::finish_component_check(&ui, false, crate::components::CheckOutcome::NotInstalled);
        assert_eq!(ui.get_component_display_message(), "VB-CABLE is not installed. The latest package is available.");
        crate::begin_component_check(&ui);
        crate::component_error(&ui, "synthetic timeout".into());
        assert_eq!(ui.get_component_display_message(), "Could not check VB-CABLE updates. Try again.");
        assert!(!ui.get_components_busy());
        assert!(!ui.get_component_progress_visible());
        ui.set_english(false);
        assert_eq!(ui.get_component_display_message(), "بررسی آپدیت VB-CABLE انجام نشد؛ دوباره تلاش کنید.");
        settle();
        capture("fa-timeout");
        ui.set_english(true);
        settle();
        click(&ui, ui.invoke_component_control_bounds(0));
        assert_eq!(checks.get(), 2, "Error did not allow retry");
        ui.set_component_check_state(0);
        ui.set_component_message("Installing driver".into());
        ui.set_components_busy(true);
        ui.set_components_installing(true);
        ui.set_download_progress(0.5);
        assert_eq!(ui.get_component_display_message(), "Installing driver");
        assert!(ui.get_component_progress_visible());
        ui.set_components_busy(false);
        ui.set_components_installing(false);
        assert!(!ui.get_component_progress_visible());
        click(&ui, ui.invoke_component_control_bounds(1));
        assert_eq!(installs.get(), 1, "Install action changed");
    }

    #[test]
    fn compact_menu_open_animation() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        let ui = crate::AppWindow::new().unwrap();
        install_focus_reveal(&ui);
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        adapter.set_size(slint::LogicalSize::new(800., 600.));
        ui.set_page(2);
        let log_opens = Rc::new(Cell::new(0));
        let captured_opens = log_opens.clone();
        ui.on_open_log_file(move || captured_opens.set(captured_opens.get() + 1));
        ui.show().unwrap();
        let draw = |label: &str| {
            slint::platform::update_timers_and_animations();
            let size = ui.window().size();
            let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
            full_redraw(ui.window());
            adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
            let capture_dir = if ["fa-midpoint", "fa-settled", "en-midpoint", "en-settled"].contains(&label) {
                std::env::var_os("MAXI_MENU_ANIMATION_DIR")
            } else if ["fa-closing-midpoint", "fa-closing-settled", "en-closing-midpoint", "en-closing-settled"].contains(&label) {
                std::env::var_os("MAXI_MENU_CLOSING_DIR")
            } else { None };
            if let Some(folder) = capture_dir {
                let folder = std::path::PathBuf::from(folder);
                std::fs::create_dir_all(&folder).unwrap();
                let mut encoder = png::Encoder::new(std::fs::File::create(folder.join(format!("{label}.png"))).unwrap(), size.width, size.height);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect::<Vec<_>>()).unwrap();
            }
        };
        draw("initial");
        clock.set(Duration::from_millis(500));
        draw("layout-settled");
        for (english, label) in [(false, "fa"), (true, "en")] {
            ui.set_english(english);
            clock.set(clock.get() + Duration::from_millis(250));
            draw("locale-ready");
            let edge = if english { 0. } else { 800. - 216. };
            let baseline = (ui.get_body_x(), ui.get_body_width(), ui.get_sound_scroll_offset());
            let log_file = ui.get_log_file_control_bounds();
            let opens_before = log_opens.get();
            ui.invoke_toggle_sidebar();
            draw("opening");
            assert!(ui.get_sidebar_menu_open());
            assert_eq!(ui.get_sidebar_focus_index(), 0);
            assert_eq!(baseline, (ui.get_body_x(), ui.get_body_width(), ui.get_sound_scroll_offset()));
            let first = ui.get_sidebar_menu_x();
            assert!((first - edge).abs() > 100., "Menu did not begin beyond its {label} edge");
            click(&ui, log_file.clone());
            assert!(!ui.get_sidebar_menu_open(), "Opening backdrop missed the first outside click");
            assert!(ui.get_sidebar_menu_presented(), "Menu vanished before its closing animation");
            assert_eq!(log_opens.get(), opens_before, "Outside click reached the page beneath the opening backdrop");
            ui.invoke_toggle_sidebar();
            draw("reopening");
            assert_eq!(ui.get_sidebar_focus_index(), 0);
            clock.set(clock.get() + Duration::from_millis(70));
            draw(&format!("{label}-midpoint"));
            let midpoint = ui.get_sidebar_menu_x();
            assert!((midpoint - edge).abs() > 1. && (midpoint - edge).abs() < (first - edge).abs(), "Menu did not move toward its {label} edge");
            assert_eq!(baseline, (ui.get_body_x(), ui.get_body_width(), ui.get_sound_scroll_offset()));
            clock.set(clock.get() + Duration::from_millis(200));
            draw(&format!("{label}-settled"));
            assert!((ui.get_sidebar_menu_x() - edge).abs() < 0.1, "Menu did not settle at its {label} edge");
            assert_eq!(baseline, (ui.get_body_x(), ui.get_body_width(), ui.get_sound_scroll_offset()));
            ui.invoke_toggle_sidebar();
            assert!(!ui.get_sidebar_menu_open());
            ui.invoke_toggle_sidebar();
            draw("rapid-reopen");
            assert!(ui.get_sidebar_menu_open());
            clock.set(clock.get() + Duration::from_millis(250));
            draw("rapid-settled");
            assert!((ui.get_sidebar_menu_x() - edge).abs() < 0.1, "Rapid reopen moved the {label} menu off its edge");
            ui.invoke_close_sidebar_menu();
            assert!(!ui.get_sidebar_menu_open());
            assert!(ui.get_sidebar_menu_presented());
            assert_eq!(ui.get_sidebar_focus_index(), 0);
            click(&ui, log_file.clone());
            assert_eq!(log_opens.get(), opens_before, "Closing backdrop let a page click through");
            click(&ui, ui.invoke_sidebar_control_bounds(2));
            assert_eq!(ui.get_page(), 2, "Closing menu accepted a navigation click");
            clock.set(clock.get() + Duration::from_millis(70));
            draw(&format!("{label}-closing-midpoint"));
            let closing_midpoint = ui.get_sidebar_menu_x();
            assert!((closing_midpoint - edge).abs() > 1. && (closing_midpoint - edge).abs() < 132., "Menu did not retreat toward its {label} edge");
            assert!(ui.get_sidebar_menu_presented());
            assert_eq!(baseline, (ui.get_body_x(), ui.get_body_width(), ui.get_sound_scroll_offset()));
            clock.set(clock.get() + Duration::from_millis(200));
            draw(&format!("{label}-closing-settled"));
            assert!(!ui.get_sidebar_menu_presented(), "Closing menu remained after its timer");
            assert_eq!(ui.get_sidebar_focus_index(), 0);
            assert_eq!(baseline, (ui.get_body_x(), ui.get_body_width(), ui.get_sound_scroll_offset()));
            click(&ui, log_file);
            assert_eq!(log_opens.get(), opens_before + 1, "Page input did not resume after close");
            ui.invoke_toggle_sidebar();
            clock.set(clock.get() + Duration::from_millis(250));
            draw("reversal-ready");
            ui.invoke_close_sidebar_menu();
            clock.set(clock.get() + Duration::from_millis(70));
            draw("reversing-close");
            ui.invoke_toggle_sidebar();
            clock.set(clock.get() + Duration::from_millis(250));
            draw("reversed-open");
            assert!(ui.get_sidebar_menu_open() && ui.get_sidebar_menu_presented(), "Stale close timer hid the reopened menu");
            assert!((ui.get_sidebar_menu_x() - edge).abs() < 0.1);
            ui.invoke_close_sidebar_menu();
            ui.set_english(!english);
            draw("locale-cleanup");
            assert!(!ui.get_sidebar_menu_presented(), "Locale change left the closing overlay visible");
            ui.set_english(english);
            ui.invoke_toggle_sidebar();
            ui.invoke_close_sidebar_menu();
            adapter.set_size(slint::LogicalSize::new(1100., 600.));
            draw("threshold-cleanup");
            assert!(!ui.get_sidebar_menu_presented(), "Wide threshold left the closing overlay visible");
            adapter.set_size(slint::LogicalSize::new(800., 600.));
            draw("compact-again");
            ui.invoke_toggle_sidebar();
            ui.invoke_close_sidebar_menu();
            ui.invoke_preview_confirmation();
            draw("dialog-cleanup");
            assert!(!ui.get_sidebar_menu_presented(), "Higher dialog left the closing overlay visible");
            press(&ui, slint::platform::Key::Escape);
        }
        ui.hide().unwrap();
    }

    #[test]
    fn compact_menu_input_geometry_and_credits_regression() {
        use slint::platform::{Key, WindowEvent};
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        let ui = crate::AppWindow::new().unwrap();
        install_focus_reveal(&ui);
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        ui.set_devices_ready(true);
        ui.set_recovery_ready(true);
        let links = Rc::new(std::cell::RefCell::new(Vec::new()));
        let captured_links = links.clone();
        ui.on_open_link(move |url| captured_links.borrow_mut().push(url.to_string()));
        let exits = Rc::new(Cell::new(0));
        let captured_exits = exits.clone();
        ui.on_exit_app(move || captured_exits.set(captured_exits.get() + 1));
        let log_opens = Rc::new(Cell::new(0));
        let captured_opens = log_opens.clone();
        ui.on_open_log_file(move || captured_opens.set(captured_opens.get() + 1));
        let settle = || {
            clock.set(clock.get() + Duration::from_millis(500));
            slint::platform::update_timers_and_animations();
            let size = ui.window().size();
            let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
            adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
        };
        let capture = |label: &str| {
            if let Some(folder) = std::env::var_os("MAXI_UI_IMPROVEMENTS_DIR") {
                let folder = std::path::PathBuf::from(folder);
                std::fs::create_dir_all(&folder).unwrap();
                let size = ui.window().size();
                let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
                full_redraw(ui.window());
                adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
                let mut encoder = png::Encoder::new(std::fs::File::create(folder.join(format!("{label}.png"))).unwrap(), size.width, size.height);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r,p.g,p.b]).collect::<Vec<_>>()).unwrap();
            }
        };
        for english in [false, true] {
            ui.set_english(english);
            for scale in [1., 1.25, 1.5] {
                ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
                for (width, height) in [(640., 320.), (837., 414.), (1099., 720.)] {
                    adapter.set_size(slint::LogicalSize::new(width, height));
                    ui.set_page(0);
                    ui.set_sidebar_expanded(true);
                    ui.show().unwrap();
                    settle();
                    ui.invoke_scroll_page(0.5);
                    let baseline = (ui.get_body_width(), ui.get_body_x(), ui.get_sound_scroll_offset());
                    click(&ui, ui.invoke_sidebar_control_bounds(0));
                    settle();
                    assert!(ui.get_sidebar_menu_open(), "Pointer must open compact menu");
                    assert_eq!(baseline, (ui.get_body_width(), ui.get_body_x(), ui.get_sound_scroll_offset()), "Menu changed page width/scroll");
                    assert!((ui.get_sidebar_menu_width() - 216.).abs() < 0.1);
                    let logical_size = ui.window().size().to_logical(ui.window().scale_factor());
                    assert!((ui.get_sidebar_menu_x() - if english {0.} else {logical_size.width - 216.}).abs() < 0.1);
                    assert_eq!(ui.get_sidebar_focus_index(), 0);
                    ui.window().dispatch_event(WindowEvent::PointerScrolled { position: slint::LogicalPosition::new(if english {width - 30.} else {30.}, height - 80.), delta_x: 0., delta_y: -120. });
                    assert_eq!(ui.get_sound_scroll_offset(), baseline.2, "Dimmer allowed underlying wheel scrolling");
                    // Blank panel space belongs to the menu, not the outside dimmer.
                    click(&ui, crate::ControlBounds { x: ui.get_sidebar_menu_x() + 90., y: 75., width: 5., height: 5. });
                    assert!(ui.get_sidebar_menu_open());
                    ui.invoke_focus_sidebar_item(0);
                    for index in 1..=20 {
                        press(&ui, Key::Tab);
                        settle();
                        assert_eq!(ui.get_sidebar_focus_index(), index % 10, "Tab escaped compact menu");
                        let bounds = ui.invoke_sidebar_control_bounds(index % 10);
                        assert!(bounds.y >= 0. && bounds.y + bounds.height <= height + 0.1, "Focused menu action clipped at {width}x{height}/{scale}, EN={english}, index={index}");
                    }
                    for index in (0..10).rev() {
                        press(&ui, Key::Backtab);
                        settle();
                        assert_eq!(ui.get_sidebar_focus_index(), index, "Backtab escaped menu");
                    }
                    // Actual Shift+Tab modifier path, in addition to Backtab.
                    ui.window().dispatch_event(WindowEvent::KeyPressed { text: Key::Shift.into() });
                    press(&ui, Key::Tab);
                    ui.window().dispatch_event(WindowEvent::KeyReleased { text: Key::Shift.into() });
                    assert_eq!(ui.get_sidebar_focus_index(), 9);
                    press(&ui, Key::Escape);
                    assert!(!ui.get_sidebar_menu_open());
                    assert_eq!(ui.get_sidebar_focus_index(), 0);
                    assert!(ui.get_sidebar_expanded(), "Compact state changed wide preference");
                    ui.set_page(1);
                    settle();
                    if width < 904. { assert!(ui.get_components_actions_centered(), "Components actions off center EN={english}/{width}"); }
                    else {
                        let update = ui.invoke_component_control_bounds(0);
                        let install = ui.invoke_component_control_bounds(1);
                        let website = ui.invoke_component_control_bounds(2);
                        assert_eq!((update.width, install.width, website.width), (165., 247., 137.), "Wide button sizes changed");
                        assert!((install.y - website.y).abs() < 0.1, "Wide actions must stay on one row");
                        assert!((if english {website.x - install.x - install.width} else {install.x - website.x - website.width} - 12.).abs() < 0.1, "Wide actions spacing changed");
                    }
                    if scale == 1. && width == 640. { capture(if english {"en-components"} else {"fa-components"}); }
                    ui.set_page(2);
                    settle();
                    let log_file = ui.get_log_file_control_bounds();
                    let before = log_opens.get();
                    click(&ui, ui.invoke_sidebar_control_bounds(0));
                    settle();
                    click(&ui, log_file.clone());
                    assert_eq!(log_opens.get(), before, "Dimmer allowed underlying click");
                    assert!(!ui.get_sidebar_menu_open(), "Outside pointer must close menu");
                    assert_eq!(ui.get_sidebar_focus_index(), 0);
                    assert!(ui.get_sidebar_menu_presented(), "Closing overlay vanished before animation");
                    // Same pointer succeeds once the dimmer is gone.
                    settle();
                    assert!(!ui.get_sidebar_menu_presented());
                    click(&ui, log_file);
                    assert_eq!(log_opens.get(), before + 1);
                    if scale == 1. && width == 640. { capture(if english {"en-log"} else {"fa-log"}); }
                    ui.invoke_toggle_sidebar();
                    ui.invoke_focus_sidebar_item(2);
                    press(&ui, Key::Return);
                    assert_eq!(ui.get_page(), 1);
                    assert!(!ui.get_sidebar_menu_open());
                    ui.invoke_toggle_sidebar();
                    settle();
                    if scale == 1. && width == 640. { capture(if english {"en-menu-top"} else {"fa-menu-top"}); }
                    ui.invoke_focus_sidebar_item(7);
                    press(&ui, Key::Return);
                    assert_eq!(links.borrow().last().unwrap(), "https://maxispace.dev/donate");
                    ui.invoke_focus_sidebar_item(9);
                    settle();
                    if scale == 1. && width == 640. { capture(if english {"en-menu-footer"} else {"fa-menu-footer"}); }
                    let before_exit = exits.get();
                    press(&ui, Key::Return);
                    assert_eq!(exits.get(), before_exit + 1);
                    ui.invoke_focus_sidebar_item(8);
                    press(&ui, Key::Return);
                    assert_eq!(ui.get_english(), !english);
                    assert!(!ui.get_sidebar_menu_open(), "Language switch left stale overlay");
                    assert_eq!(ui.get_sidebar_focus_index(), 0);
                    ui.set_english(english);
                    ui.invoke_toggle_sidebar();
                    ui.invoke_preview_confirmation();
                    settle();
                    assert!(!ui.get_sidebar_menu_open(), "Higher dialog must close compact menu");
                    ui.invoke_toggle_sidebar();
                    assert!(!ui.get_sidebar_menu_open(), "Menu opened above higher dialog");
                    press(&ui, Key::Tab);
                    press(&ui, Key::Escape);
                    assert_eq!(ui.get_sidebar_focus_index(), 0);
                    ui.set_page(6);
                    settle();
                    ui.invoke_focus_rust_credit();
                    settle();
                    press(&ui, Key::Return);
                    assert_eq!(links.borrow().last().unwrap(), "https://www.rust-lang.org/");
                    press(&ui, " ");
                    click(&ui, ui.get_rust_control_bounds());
                    assert!(links.borrow().iter().rev().take(3).all(|s| s == "https://www.rust-lang.org/"));
                    if scale == 1. && width == 640. { capture(if english {"en-rust-credit"} else {"fa-rust-credit"}); }
                    ui.invoke_toggle_sidebar();
                    settle();
                    click(&ui, ui.invoke_sidebar_control_bounds(0));
                    assert!(!ui.get_sidebar_menu_open(), "Close button did not dismiss menu");
                }
            }
            ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1. });
            adapter.set_size(slint::LogicalSize::new(640., 860.));
            ui.set_page(1);
            settle();
            assert!(ui.get_components_actions_centered());
            capture(if english {"en-components-tall"} else {"fa-components-tall"});
            adapter.set_size(slint::LogicalSize::new(837., 720.));
            ui.set_page(2);
            ui.set_logs(Rc::new(slint::VecModel::from(vec![crate::LogRow { time: "12:34:56".into(), level: "INFO".into(), message: "Synthetic preview event / رویداد آزمایشی".into() }])).into());
            settle();
            capture(if english {"en-log-rows"} else {"fa-log-rows"});
            // Cross the breakpoint in both directions, preserving the wide preference.
            for expanded in [false, true] {
                ui.set_sidebar_expanded(expanded);
                adapter.set_size(slint::LogicalSize::new(1099., 720.));
                settle();
                ui.invoke_toggle_sidebar();
                assert!(ui.get_sidebar_menu_open());
                adapter.set_size(slint::LogicalSize::new(1100., 720.));
                settle();
                settle(); // The first layout evaluation starts the reserved-slot animation.
                assert!(!ui.get_sidebar_menu_open());
                assert_eq!(ui.get_sidebar_expanded(), expanded);
                assert!((ui.get_sidebar_menu_width() - if expanded {216.} else {84.}).abs() < 0.1);
            }
        }
        ui.hide().unwrap();
    }

    #[test]
    fn erased_surface_is_fully_restored_without_resize_at_both_dpi_scales() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        let ui = crate::AppWindow::new().unwrap();
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        install_focus_reveal(&ui);
        assert!(
            !ui.get_cable_warning_visible(),
            "Unknown devices must not show a missing-driver warning"
        );
        ui.set_devices_ready(true);
        ui.set_recovery_ready(true);
        ui.set_cable_installed(false);
        assert!(ui.get_cable_warning_visible());
        ui.set_cable_installed(true);
        assert!(!ui.get_cable_warning_visible());
        ui.set_profile(1);
        ui.set_mode(0);
        assert!(ui.get_enhancement_hint_visible());
        ui.set_mode(1);
        assert!(!ui.get_enhancement_hint_visible());
        for scale in [1.0, 1.25] {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                    scale_factor: scale,
                });
            adapter.set_size(slint::LogicalSize::new(1280.0, 860.0));
            ui.show().unwrap();
            let size = ui.window().size();
            let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
            full_redraw(ui.window());
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, size.width as usize);
            }));
            let expected = pixels.clone();
            // Simulate native loss of pixels while Slint still believes its
            // reused buffer is valid, as seen in the supplied white screenshot.
            let white = slint::Rgb8Pixel {
                r: 255,
                g: 255,
                b: 255,
            };
            for _ in 0..2 {
                pixels.fill(white);
                ui.window().request_redraw();
                adapter.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, size.width as usize);
                });
                assert_ne!(
                    pixels, expected,
                    "A plain redraw unexpectedly fixed the erased cache"
                );
                full_redraw(ui.window());
                assert!(adapter.draw_if_needed(|renderer| {
                    let dirty = renderer.render(&mut pixels, size.width as usize);
                    assert_eq!(dirty.bounding_box_size(), size);
                }));
                assert_eq!(
                    ui.window().size(),
                    size,
                    "Recovery must not resize the window"
                );
                assert_eq!(
                    pixels, expected,
                    "Full repaint failed to restore the entire app"
                );
            }
            ui.hide().unwrap();
        }
        stress_sound_meters(&ui, &adapter, &clock);
        small_screen_matrix(&ui, &adapter, &clock);
    }

    fn small_screen_matrix(ui: &crate::AppWindow, adapter: &Rc<MinimalSoftwareWindow>, clock: &Cell<Duration>) {
        let folder = std::env::var_os("MAXI_UI_STRESS_DIR").map(std::path::PathBuf::from);
        ui.set_eq_bands(std::rc::Rc::new(slint::VecModel::from(["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"].into_iter().map(|frequency| crate::EqBand { frequency: frequency.into(), gain: 0. }).collect::<Vec<_>>())).into());
        ui.set_outputs(std::rc::Rc::new(slint::VecModel::from(vec!["Speakers (High Definition Audio Device)".into(), "Headphones — USB audio".into()])).into());
        ui.set_sources(std::rc::Rc::new(slint::VecModel::from(vec!["CABLE Output (VB-Audio Virtual Cable)".into()])).into());
        ui.set_hotkey_rows(std::rc::Rc::new(slint::VecModel::from((0..8).map(|i| crate::HotkeyRow { name: format!("Shortcut {i} / میانبر صدا").into(), chord: "Ctrl + Alt + F12".into(), enabled: true, error: "".into() }).collect::<Vec<_>>())).into());
        for (width, height) in [(1280., 720.), (1366., 768.), (1920., 1080.)] {
            for scale in [1., 1.25, 1.5] {
                ui.window().dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: scale });
                // Reserve a 40-physical-pixel taskbar and scaled title/borders.
                adapter.set_size(slint::LogicalSize::new((width - 16. * scale) / scale, (height - 40. - 39. * scale) / scale));
                ui.set_sidebar_expanded(true);
                ui.set_cable_installed(false);
                ui.set_mode(1);
                ui.set_profile(7);
                for english in [false, true] {
                    ui.set_english(english);
                    for page in [0, 1, 4, 6] {
                        ui.set_page(page);
                        for (position, fraction) in [("top", 0.), ("middle", 0.5), ("equalizer", 0.8), ("bottom", 1.)] {
                        ui.invoke_scroll_page(fraction);
                        ui.show().unwrap();
                        clock.set(clock.get() + Duration::from_millis(500));
                        slint::platform::update_timers_and_animations();
                        let size = ui.window().size();
                        let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
                        full_redraw(ui.window());
                        adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
                        assert!(ui.get_minimum_size_valid(), "Window exceeds minimum at {width}x{height}/{scale}");
                        assert!(ui.get_scroll_widths_fit(), "Horizontal page overflow");
                        if page == 4 && height == 720. && scale == 1.5 { assert!(ui.get_settings_scroll_range() > 400., "Hotkey rows must remain scrollable"); }
                        if page == 0 && fraction == 1. { assert!(ui.get_sound_bottom_visible(), "Sound bottom not reachable at {width}x{height}/{scale}, EN={english}"); }
                        if page == 0 { assert!(ui.get_sound_layout_fits(), "Sound bounds at {width}x{height}/{scale}, EN={english}"); }
                        if let Some(folder) = &folder {
                            let path = folder.join(format!("Matrix-{width}x{height}-{scale}-{}-page{page}-{}.png", if english { "EN" } else { "FA" }, position));
                            let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), size.width, size.height);
                            encoder.set_color(png::ColorType::Rgb);
                            encoder.set_depth(png::BitDepth::Eight);
                            encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r,p.g,p.b]).collect::<Vec<_>>()).unwrap();
                        }
                        }
                    }
                }
            }
        }
        for english in [false, true] {
            ui.set_english(english);
            ui.window().dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: 1. });
            adapter.set_size(slint::LogicalSize::new(640., 320.));
            ui.set_page(4);
            ui.show().unwrap();
            clock.set(clock.get() + Duration::from_millis(500));
            slint::platform::update_timers_and_animations();
            ui.invoke_scroll_page(0.);
            ui.invoke_focus_page();
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::PageDown.into() });
            assert!(ui.get_settings_scroll_offset() < -20., "PageDown did not scroll");
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: slint::platform::Key::PageDown.into() });
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::PageUp.into() });
            assert!(ui.get_settings_scroll_offset().abs() < 1., "PageUp did not return to top");
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: slint::platform::Key::PageUp.into() });
            let mut revealed = false;
            for _ in 0..80 {
                ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::Tab.into() });
                ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: slint::platform::Key::Tab.into() });
                revealed |= ui.get_settings_scroll_offset() < -20.;
            }
            assert!(revealed, "Tab did not reveal lower Settings controls");
            for (width, height) in [(640., 320.), (837., 414.)] {
                adapter.set_size(slint::LogicalSize::new(width, height));
                ui.set_sidebar_expanded(true);
                ui.set_page(2);
                clock.set(clock.get() + Duration::from_millis(500));
                slint::platform::update_timers_and_animations();
                let log_size = ui.window().size();
                let mut log_pixels = vec![slint::Rgb8Pixel::default(); (log_size.width * log_size.height) as usize];
                full_redraw(ui.window());
                adapter.draw_if_needed(|r| { r.render(&mut log_pixels, log_size.width as usize); });
                assert!(ui.get_log_header_fits(), "Log header controls clipped at {width}x{height}, EN={english}");
                for mode in [0, 1] {
                    ui.set_mode(mode);
                    for surface in ["sound", "help", "profile", "reaction", "confirmation", "message"] {
                        ui.set_page(0);
                        clock.set(clock.get() + Duration::from_millis(500));
                        slint::platform::update_timers_and_animations();
                        ui.invoke_scroll_page(if matches!(surface, "help" | "reaction") { 1. } else { 0. });
                        match surface {
                            "help" => ui.invoke_preview_routing_help(),
                            "profile" => { ui.invoke_scroll_page(0.3); ui.invoke_preview_profile_menu(); },
                            "reaction" => ui.invoke_preview_menu(),
                            "confirmation" => ui.invoke_preview_confirmation(),
                            "message" => ui.set_audio_error(if english { "Audio device unavailable. Check the selected output and try again." } else { "دستگاه صوتی در دسترس نیست. خروجی انتخاب‌شده را بررسی کن و دوباره تلاش کن." }.into()),
                            _ => {},
                        }
                        ui.invoke_scroll_sidebar_to_end();
                        clock.set(clock.get() + Duration::from_millis(500));
                        slint::platform::update_timers_and_animations();
                        let size = ui.window().size();
                        let mut pixels = vec![slint::Rgb8Pixel::default(); (size.width * size.height) as usize];
                        full_redraw(ui.window());
                        adapter.draw_if_needed(|r| { r.render(&mut pixels, size.width as usize); });
                        assert!(ui.get_minimum_size_valid() && ui.get_scroll_widths_fit(), "Minimum viewport overflow");
                        assert!(ui.get_sidebar_footer_visible(), "Sidebar Exit control clipped after scrolling at {width}x{height}, EN={english}, mode={mode}, surface={surface}");
                        if let Some(folder) = &folder {
                            let path = folder.join(format!("Surface-{width}x{height}-{}-mode{mode}-{surface}.png", if english { "EN" } else { "FA" }));
                            let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), size.width, size.height);
                            encoder.set_color(png::ColorType::Rgb);
                            encoder.set_depth(png::BitDepth::Eight);
                            encoder.write_header().unwrap().write_image_data(&pixels.iter().flat_map(|p| [p.r,p.g,p.b]).collect::<Vec<_>>()).unwrap();
                        }
                        ui.invoke_close_routing_help();
                        ui.invoke_close_profile_menu();
                        ui.invoke_close_preview_menu();
                        ui.invoke_close_preview_confirmation();
                        ui.set_audio_error("".into());
                    }
                }
            }
        }
        ui.hide().unwrap();
    }

    fn stress_sound_meters(
        ui: &crate::AppWindow,
        adapter: &Rc<MinimalSoftwareWindow>,
        clock: &Cell<Duration>,
    ) {
        let folder = std::env::var_os("MAXI_UI_STRESS_DIR").map(std::path::PathBuf::from);
        if let Some(path) = &folder {
            std::fs::create_dir_all(path).unwrap();
        }
        let mut states = 0;
        for english in [false, true] {
            ui.set_english(english);
            ui.set_sidebar_expanded(true);
            clock.set(clock.get() + Duration::from_millis(400));
            slint::platform::update_timers_and_animations();
            for expanded in [false, true] {
                ui.set_sidebar_expanded(expanded);
                for _ in 0..9 {
                    clock.set(clock.get() + Duration::from_millis(40));
                    slint::platform::update_timers_and_animations();
                    assert!(ui.get_sidebar_icon_aligned(), "Sidebar icon lagged its intended position during animation, en={english}, expanded={expanded}");
                }
            }
        }
        for scale in [1.0, 1.25, 1.5, 2.0] {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                    scale_factor: scale,
                });
            for (width, height) in [(1120., 760.), (1600., 1000.)] {
                adapter.set_size(slint::LogicalSize::new(width, height));
                for english in [false, true] {
                    for expanded in [false, true] {
                        for profile in [0, 7] {
                            for mode in [0, 1] {
                                ui.set_page(0);
                                ui.set_english(english);
                                ui.set_sidebar_expanded(expanded);
                                ui.set_profile(profile);
                                ui.set_mode(mode);
                                ui.set_input_level(0.25);
                                ui.set_output_level(0.75);
                                ui.show().unwrap();
                                for gain in [-96., -24., -12., -0.1, 0., 0.1, 12., 24., 96.] {
                                    ui.set_gain_db(gain);
                                    ui.set_gain_text(format!("{gain:+.1} dB").into());
                                    ui.set_input_text("0".into());
                                    ui.set_output_text("0".into());
                                    clock.set(clock.get() + Duration::from_millis(300));
                                    slint::platform::update_timers_and_animations();
                                    let baseline = ui.get_input_meter_width();
                                    ui.set_input_text("999999999999.9".into());
                                    ui.set_output_text("−123456789.9 dB".into());
                                    ui.set_boost_limited(gain > 0.);
                                    ui.set_limited(gain < 0.);
                                    clock.set(clock.get() + Duration::from_millis(300));
                                    slint::platform::update_timers_and_animations();
                                    assert!(
                                        (ui.get_input_meter_width() - baseline).abs() < 0.1,
                                        "Values changed card width"
                                    );
                                    assert!(
                                        (ui.get_output_meter_width() - baseline).abs() < 0.1
                                            && (ui.get_gain_meter_width() - baseline).abs() < 0.1,
                                        "Unequal card widths"
                                    );
                                    assert!(baseline > 100. && ui.get_sound_layout_fits(), "Clipped Sound layout at scale {scale}, {width}x{height}, en={english}, sidebar={expanded}, profile={profile}");
                                    let track = ui.get_gain_track_width();
                                    let fill = ui.get_gain_fill_width();
                                    let x = ui.get_gain_fill_x();
                                    assert!(
                                        fill >= 0.
                                            && fill <= track / 2. + 0.1
                                            && x >= 0.
                                            && x + fill <= track + 0.1,
                                        "Gain fill exceeds bounds"
                                    );
                                    let cut_on_left = (gain < 0.) == english;
                                    let expected_x =
                                        track / 2. - if cut_on_left { fill } else { 0. };
                                    assert!(
                                        (x - expected_x).abs() < 0.1,
                                        "Gain direction disagrees with language"
                                    );
                                    assert!(
                                        (fill - track / 2. * (gain.abs() / 24.).min(1.)).abs()
                                            < 0.1,
                                        "Gain amplitude/zero incorrect"
                                    );
                                    let input_track = ui.get_input_track_width();
                                    let input_fill = ui.get_input_fill_width();
                                    let input_x = ui.get_input_fill_x();
                                    let output_track = ui.get_output_track_width();
                                    let output_fill = ui.get_output_fill_width();
                                    let output_x = ui.get_output_fill_x();
                                    assert!(
                                        (input_fill - input_track * 0.25).abs() < 0.1
                                            && (output_fill - output_track * 0.75).abs() < 0.1,
                                        "Input/output level amplitude incorrect"
                                    );
                                    let expected_input_x = if english {
                                        0.
                                    } else {
                                        input_track - input_fill
                                    };
                                    let expected_output_x = if english {
                                        0.
                                    } else {
                                        output_track - output_fill
                                    };
                                    assert!(
                                        (input_x - expected_input_x).abs() < 0.1
                                            && (output_x - expected_output_x).abs() < 0.1,
                                        "Input/output fill direction disagrees with language"
                                    );
                                    // Render selected extremes and representative half fills; geometry is checked for every state.
                                    if expanded
                                        && profile == 0
                                        && mode == 1
                                        && [-96., -12., 0., 12., 96.].contains(&gain)
                                    {
                                        let size = ui.window().size();
                                        let mut pixels = vec![
                                            slint::Rgb8Pixel::default();
                                            (size.width * size.height) as usize
                                        ];
                                        full_redraw(ui.window());
                                        assert!(adapter.draw_if_needed(|r| {
                                            r.render(&mut pixels, size.width as usize);
                                        }));
                                        if let Some(folder) = &folder {
                                            if width == 1120.
                                                && expanded
                                                && profile == 0
                                                && mode == 1
                                                && [-12., 0., 12.].contains(&gain)
                                            {
                                                let path = folder.join(format!(
                                                    "Sound-{scale}-{}-{gain}.png",
                                                    if english { "EN" } else { "FA" }
                                                ));
                                                let mut encoder = png::Encoder::new(
                                                    std::fs::File::create(path).unwrap(),
                                                    size.width,
                                                    size.height,
                                                );
                                                encoder.set_color(png::ColorType::Rgb);
                                                encoder.set_depth(png::BitDepth::Eight);
                                                let bytes: Vec<u8> = pixels
                                                    .iter()
                                                    .flat_map(|p| [p.r, p.g, p.b])
                                                    .collect();
                                                encoder
                                                    .write_header()
                                                    .unwrap()
                                                    .write_image_data(&bytes)
                                                    .unwrap();
                                            }
                                        }
                                    }
                                    states += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        ui.set_page(6);
        for scale in [1., 1.25, 1.5, 2.] {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                    scale_factor: scale,
                });
            adapter.set_size(slint::LogicalSize::new(1120., 760.));
            for english in [false, true] {
                ui.set_english(english);
                clock.set(clock.get() + Duration::from_millis(300));
                slint::platform::update_timers_and_animations();
                assert!(ui.get_about_layout_fits(), "About content clipped at minimum size, scale {scale}, en={english}: rights={}, cards={}, notices={}, credits={}", ui.get_about_rights_fits(), ui.get_about_cards_separated(), ui.get_about_notices_fit(), ui.get_about_credits_fit());
            }
        }
        ui.hide().unwrap();
        if let Some(folder) = folder {
            std::fs::write(folder.join("stress.txt"),format!("PASS: {states} Sound meter states; 100/125/150/200% scale; minimum/large viewport; FA/EN; expanded/collapsed sidebar with icon anchoring checked throughout both animations; default/custom EQ; Windows/cable modes; zero/tiny/half/full/over-range gain; long values and limiter labels; stable equal widths, bounds and mirrored center fills. About credit and notices also fit the minimum window in 8 scale/language states. Representative software-rendered screenshots saved.\n")).unwrap();
        }
    }
}
