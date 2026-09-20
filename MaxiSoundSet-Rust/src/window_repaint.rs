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

pub fn after_show(ui: &crate::AppWindow) {
    full_redraw(ui.window());
    // Native creation/presentation is asynchronous. Cover the first settled
    // frames as well, without resizing, changing UI properties, or continuously
    // disabling partial rendering. Hidden startup windows need no repaint.
    for delay in [100, 250] {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            if let Some(ui) = weak.upgrade() {
                if ui.window().is_visible() {
                    full_redraw(ui.window());
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::{
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        Platform, PlatformError, WindowAdapter,
    };
    use std::{cell::Cell, rc::Rc};

    struct Headless(Rc<MinimalSoftwareWindow>, Rc<Cell<Duration>>);
    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
        fn duration_since_start(&self) -> Duration {
            self.1.get()
        }
    }

    #[test]
    fn erased_surface_is_fully_restored_without_resize_at_both_dpi_scales() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let clock = Rc::new(Cell::new(Duration::ZERO));
        slint::platform::set_platform(Box::new(Headless(adapter.clone(), clock.clone()))).unwrap();
        let ui = crate::AppWindow::new().unwrap();
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
