//! Compact non-activating, click-through Win32 result card.
use anyhow::{Context, Result};
use std::{cell::RefCell, ffi::c_void, sync::OnceLock, time::{Duration, Instant}};
use windows::{core::{BOOL, PCWSTR}, Win32::{
    Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
}};

const ENTER: Duration = Duration::from_millis(200);
const HOLD: Duration = Duration::from_millis(1800);
const EXIT: Duration = Duration::from_millis(160);
const MOTION_TIMER: usize = 1;
const HOLD_TIMER: usize = 2;
static PRIVATE_FONTS: OnceLock<Vec<usize>> = OnceLock::new();

#[link(name = "gdi32")]
unsafe extern "system" {
    fn AddFontMemResourceEx(data: *const c_void, size: u32, reserved: *mut c_void, count: *mut u32) -> *mut c_void;
}
#[link(name = "shcore")]
unsafe extern "system" {
    fn GetDpiForMonitor(monitor: *mut c_void, kind: i32, x: *mut u32, y: *mut u32) -> i32;
}

pub(crate) fn ensure_vazir_registered() -> Result<()> {
    let handles = PRIVATE_FONTS.get_or_init(|| {
        let mut handles = Vec::with_capacity(2);
        for bytes in [include_bytes!("../assets/Vazir.ttf").as_slice(), include_bytes!("../assets/Vazir-Bold.ttf").as_slice()] {
            let mut count = 0;
            let handle = unsafe { AddFontMemResourceEx(bytes.as_ptr().cast(), bytes.len() as u32, std::ptr::null_mut(), &mut count) };
            if !handle.is_null() && count > 0 { handles.push(handle as usize); }
        }
        handles
    });
    anyhow::ensure!(handles.len() == 2, "Could not register the embedded Vazir fonts for GDI");
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub enum NoticeIcon { Volume, Muted, Play, Pause, Stop, Leveling, Profile, Smoothing, Warning }
pub struct Notice<'a> {
    pub title: &'a str,
    pub detail: &'a str,
    pub rtl: bool,
    pub icon: NoticeIcon,
    /// The application's target; this is never labelled as Windows master volume.
    pub value: Option<i32>,
    pub anchor: Option<(i32, i32)>,
}
#[derive(Clone)]
struct Content { title: String, detail: String, rtl: bool, icon: NoticeIcon, value: Option<i32> }
impl Default for Content {
    fn default() -> Self { Self { title: String::new(), detail: String::new(), rtl: false, icon: NoticeIcon::Volume, value: None } }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase { Hidden, Entering, Visible, Exiting }
/// Pure monotonic-clock state. A delayed WM_TIMER consumes all elapsed phases;
/// it cannot extend the hold indefinitely after suspension or a busy UI thread.
struct Motion {
    phase: Phase,
    started: Instant,
    duration: Duration,
    from: f64,
    hold_until: Instant,
    reduced: bool,
}
impl Motion {
    fn new(now: Instant, reduced: bool) -> Self {
        Self { phase: Phase::Hidden, started: now, duration: ENTER, from: 0.0, hold_until: now, reduced }
    }
    fn progress(&self, now: Instant) -> f64 {
        let t = (now.saturating_duration_since(self.started).as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        match self.phase {
            Phase::Hidden => 0.0,
            Phase::Visible => 1.0,
            Phase::Entering => self.from + (1.0 - self.from) * (1.0 - (1.0 - t).powi(3)),
            Phase::Exiting => self.from * (1.0 - t * t),
        }
    }
    fn advance(&mut self, now: Instant) {
        if self.phase == Phase::Entering && now >= self.started + self.duration { self.phase = Phase::Visible; }
        if self.phase == Phase::Visible && now >= self.hold_until {
            self.phase = if self.reduced { Phase::Hidden } else { Phase::Exiting };
            self.started = self.hold_until;
            self.duration = EXIT;
            self.from = 1.0;
        }
        if self.phase == Phase::Exiting && now >= self.started + self.duration { self.phase = Phase::Hidden; }
    }
    fn renew(&mut self, now: Instant) {
        self.advance(now);
        let current = self.progress(now);
        match self.phase {
            Phase::Hidden | Phase::Exiting => {
                self.phase = if self.reduced { Phase::Visible } else { Phase::Entering };
                self.started = now;
                self.from = current;
                // Reversal starts at the exact current position and opacity.
                self.duration = ENTER.mul_f64((1.0 - current).max(0.005));
            }
            _ => {}
        }
        let entry_end = if self.phase == Phase::Entering { self.started + self.duration } else { now };
        self.hold_until = entry_end.max(now) + HOLD;
    }
    fn set_reduced(&mut self, now: Instant, reduced: bool) {
        self.advance(now);
        self.reduced = reduced;
        if reduced {
            match self.phase {
                Phase::Entering => { self.phase = Phase::Visible; self.hold_until = now + HOLD; }
                Phase::Exiting => self.phase = Phase::Hidden,
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Geometry { work: RECT, dpi: u32, width: i32, height: i32 }
impl Geometry {
    fn px(&self, dip: i32) -> i32 { ((dip as i64 * self.dpi as i64 + 48) / 96) as i32 }
    fn position(&self, progress: f64) -> (i32, i32) {
        let x = self.work.left + (self.work.right - self.work.left - self.width) / 2;
        let hidden = self.work.top - self.px(72) - 1;
        let shown = self.work.top + self.px(16);
        (x, (hidden as f64 + (shown - hidden) as f64 * progress).round() as i32)
    }
    fn resize(&mut self, content: &Content) {
        let expanded = content.value.is_none() && (content.detail.chars().count() > 44 || content.title.chars().count() > 34);
        self.width = self.px(if expanded { 360 } else { 328 }).min(self.work.right - self.work.left);
        self.height = self.px(if expanded { 72 } else { 64 }).min(self.work.bottom - self.work.top);
    }
}
struct State { content: Content, motion: Motion, geometry: Option<Geometry>, positioning: bool }

/// The UI thread owns both the HWND and its boxed state. The pointer is cleared
/// at WM_NCDESTROY before the Box drops. No RefCell borrow crosses a Win32 call
/// which can synchronously reenter the window procedure.
pub struct Osd { hwnd: HWND, module: HINSTANCE, class_name: Vec<u16>, _state: Box<RefCell<State>> }
impl Osd {
    pub fn new() -> Result<Self> {
        ensure_vazir_registered()?;
        let class_name = wide("MaxiSoundSet.BackgroundNotice");
        let module: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
        let class = WNDCLASSW { style: CS_HREDRAW | CS_VREDRAW, lpfnWndProc: Some(procedure), hInstance: module, lpszClassName: PCWSTR(class_name.as_ptr()), ..Default::default() };
        anyhow::ensure!(unsafe { RegisterClassW(&class) } != 0, "Could not register the background notice window class");
        let state = Box::new(RefCell::new(State { content: Content::default(), motion: Motion::new(Instant::now(), !animations_enabled()), geometry: None, positioning: false }));
        // Main-window discovery uses its exact title. A topmost notice must
        // never be mistaken for the main app during show/close/tray recovery.
        let title = wide("MAXI SOUNDSET · Background notice");
        let hwnd = match unsafe {
            CreateWindowExW(WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                PCWSTR(class_name.as_ptr()), PCWSTR(title.as_ptr()), WS_POPUP, 0, 0, 328, 64,
                None, None, Some(module), Some((&*state as *const RefCell<State>).cast()))
        }.context("Could not create the background notice window") {
            Ok(hwnd) => hwnd,
            Err(error) => { unsafe { let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(module)); } return Err(error); }
        };
        Ok(Self { hwnd, module, class_name, _state: state })
    }
    pub fn show(&self, title: &str, detail: &str, rtl: bool) {
        self.show_notice(Notice { title, detail, rtl, icon: NoticeIcon::Volume, value: None, anchor: None });
    }
    pub fn show_notice(&self, notice: Notice<'_>) {
        let now = Instant::now();
        // Expiry may happen before a queued timer is delivered. A fresh display
        // cycle is selected from the new input's origin only when truly hidden.
        let needs_destination = {
            let mut state = self._state.borrow_mut();
            state.motion.advance(now);
            state.motion.phase == Phase::Hidden || state.geometry.is_none()
        };
        let destination = if needs_destination { select_geometry(notice.anchor) } else { None };
        {
            let mut state = self._state.borrow_mut();
            if needs_destination {
                state.geometry = destination;
                if state.geometry.is_none() { state.motion.phase = Phase::Hidden; }
            }
            if state.geometry.is_some() {
                state.content = Content { title: notice.title.into(), detail: notice.detail.into(), rtl: notice.rtl, icon: notice.icon, value: notice.value };
                let content = state.content.clone();
                // The offscreen start is height-independent, so content can
                // expand while moving without jumping vertically.
                state.geometry.as_mut().unwrap().resize(&content);
                state.motion.renew(now);
            }
        }
        present(self.hwnd, now);
    }
    pub fn is_visible(&self) -> bool { unsafe { IsWindowVisible(self.hwnd).as_bool() } }
    pub fn query_visibility(&self, _timeout: Duration) -> Option<bool> { Some(self.is_visible()) }
    pub fn hide(&self) { hide(self.hwnd); }
}
impl Drop for Osd {
    fn drop(&mut self) {
        self.hide();
        unsafe { let _ = DestroyWindow(self.hwnd); let _ = UnregisterClassW(PCWSTR(self.class_name.as_ptr()), Some(self.module)); }
    }
}

fn animations_enabled() -> bool {
    let mut enabled = BOOL(1);
    unsafe { let _ = SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some((&mut enabled as *mut BOOL).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)); }
    enabled.as_bool()
}
fn select_geometry(anchor: Option<(i32, i32)>) -> Option<Geometry> {
    let monitor = unsafe {
        if let Some((x, y)) = anchor { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) }
        else { MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONEAREST) }
    };
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() { return None; }
    let mut x = 96; let mut y = 96;
    if unsafe { GetDpiForMonitor(monitor.0, 0, &mut x, &mut y) } < 0 { x = 96; }
    Some(Geometry { work: info.rcWork, dpi: x.max(96), width: 0, height: 0 })
}
unsafe fn state<'a>(hwnd: HWND) -> Option<&'a RefCell<State>> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<State>;
    unsafe { pointer.as_ref() }
}
fn hide(hwnd: HWND) {
    if let Some(cell) = unsafe { state(hwnd) } {
        let mut state = cell.borrow_mut(); state.motion.phase = Phase::Hidden; state.geometry = None;
    }
    unsafe { let _ = KillTimer(Some(hwnd), MOTION_TIMER); let _ = KillTimer(Some(hwnd), HOLD_TIMER); let _ = ShowWindow(hwnd, SW_HIDE); }
}
fn present(hwnd: HWND, now: Instant) {
    let Some(cell) = (unsafe { state(hwnd) }) else { return; };
    let snapshot = {
        let mut state = cell.borrow_mut();
        state.motion.advance(now);
        state.geometry.map(|geometry| (geometry, state.motion.phase, state.motion.progress(now), state.motion.hold_until))
    };
    let Some((geometry, phase, progress, deadline)) = snapshot else { hide(hwnd); return; };
    unsafe { let _ = KillTimer(Some(hwnd), MOTION_TIMER); let _ = KillTimer(Some(hwnd), HOLD_TIMER); }
    if phase == Phase::Hidden { hide(hwnd); return; }
    let (x, y) = geometry.position(progress);
    // Intersect the rounded HWND region with the destination's work area. This
    // prevents entering/exiting pixels appearing on a monitor above or taskbar.
    let region = unsafe { CreateRoundRectRgn(0, 0, geometry.width + 1, geometry.height + 1, geometry.px(18), geometry.px(18)) };
    let clip = unsafe { CreateRectRgn(0, (geometry.work.top - y).max(0), geometry.width, geometry.height.min(geometry.work.bottom - y)) };
    if region.is_invalid() || clip.is_invalid() {
        unsafe { if !region.is_invalid() { let _ = DeleteObject(region.into()); } if !clip.is_invalid() { let _ = DeleteObject(clip.into()); } }
        hide(hwnd); return;
    }
    unsafe {
        let _ = CombineRgn(Some(region), Some(region), Some(clip), RGN_AND);
        let _ = DeleteObject(clip.into());
        if SetWindowRgn(hwnd, Some(region), true) == 0 { let _ = DeleteObject(region.into()); hide(hwnd); return; }
        cell.borrow_mut().positioning = true;
        let positioned = SetLayeredWindowAttributes(hwnd, COLORREF(0), (progress * 248.0).round() as u8, LWA_ALPHA).is_ok()
            && SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, geometry.width, geometry.height, SWP_NOACTIVATE | SWP_SHOWWINDOW).is_ok();
        cell.borrow_mut().positioning = false;
        if !positioned || cell.borrow().motion.phase == Phase::Hidden { hide(hwnd); return; }
        let _ = InvalidateRect(Some(hwnd), None, false);
        let timer = if matches!(phase, Phase::Entering | Phase::Exiting) {
            SetTimer(Some(hwnd), MOTION_TIMER, 15, None)
        } else {
            SetTimer(Some(hwnd), HOLD_TIMER, deadline.saturating_duration_since(now).as_millis().clamp(1, u32::MAX as u128) as u32, None)
        };
        if timer == 0 { hide(hwnd); }
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = unsafe { &*(l.0 as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize); }
            LRESULT(1)
        }
        WM_NCDESTROY => { unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0); DefWindowProcW(hwnd, message, w, l) } }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER if w.0 == MOTION_TIMER || w.0 == HOLD_TIMER => { present(hwnd, Instant::now()); LRESULT(0) }
        WM_DISPLAYCHANGE => { hide(hwnd); LRESULT(0) }
        WM_DPICHANGED => {
            let positioning = unsafe { state(hwnd) }.is_some_and(|cell| cell.borrow().positioning);
            // Our own placement already uses the selected monitor DPI.
            if !positioning { hide(hwnd); }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            if w.0 == SPI_SETWORKAREA.0 as usize { hide(hwnd); }
            else {
                let reduced = !animations_enabled(); let now = Instant::now();
                if let Some(cell) = unsafe { state(hwnd) } { cell.borrow_mut().motion.set_reduced(now, reduced); }
                present(hwnd, now);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = unsafe { BeginPaint(hwnd, &mut ps) };
            let snapshot = unsafe { state(hwnd) }.and_then(|cell| { let state = cell.borrow(); state.geometry.map(|g| (g, state.content.clone())) });
            if let Some((geometry, content)) = snapshot { unsafe { render_card(dc, geometry, &content); } }
            unsafe { let _ = EndPaint(hwnd, &ps); }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

fn rgb(value: u32) -> COLORREF { COLORREF(((value & 0xff) << 16) | (value & 0xff00) | ((value >> 16) & 0xff)) }
unsafe fn font(height: i32, bold: bool) -> HFONT {
    let face = wide("Vazir");
    unsafe { CreateFontW(-height, 0, 0, 0, if bold { 600 } else { 400 }, 0, 0, 0, DEFAULT_CHARSET, Default::default(), Default::default(), ANTIALIASED_QUALITY, DEFAULT_PITCH.0 as u32, PCWSTR(face.as_ptr())) }
}
fn target_number(value: i32, rtl: bool) -> String {
    value.to_string().chars().map(|ch| match ch {
        '0'..='9' if rtl => char::from_u32('۰' as u32 + ch as u32 - '0' as u32).unwrap(),
        other => other,
    }).collect()
}
fn target_alignment(rtl: bool) -> DRAW_TEXT_FORMAT {
    // The number has its own isolated lane, anchored to the card's outer padding.
    // Do not apply paragraph RTL flags to this digit-only run.
    (if rtl { DT_LEFT } else { DT_RIGHT }) | DT_VCENTER | DT_SINGLELINE
}
unsafe fn text(dc: HDC, value: &str, rect: RECT, flags: DRAW_TEXT_FORMAT, color: u32) {
    let mut buffer: Vec<u16> = value.encode_utf16().collect(); let mut rect = rect;
    unsafe { let _ = SetTextColor(dc, rgb(color)); let _ = DrawTextW(dc, &mut buffer, &mut rect, flags | DT_NOPREFIX); }
}
struct TextLayout { title: RECT, detail: RECT, value: Option<RECT>, align: DRAW_TEXT_FORMAT }
fn text_layout(geometry: Geometry, content: &Content) -> TextLayout {
    let p = |value| geometry.px(value); let width = geometry.width; let height = geometry.height;
    let (mut left, mut right) = if content.rtl { (p(12), width - p(48)) } else { (p(48), width - p(12)) };
    let value = content.value.map(|_| {
        let rect = if content.rtl { RECT { left, top: 0, right: left + p(62), bottom: height } }
            else { RECT { left: right - p(62), top: 0, right, bottom: height } };
        if content.rtl { left += p(66); } else { right -= p(66); }
        rect
    });
    // Vazir 12 has a 19px line cell at 96 DPI. Reserve 40 DIP for two
    // detail lines, including fractional-DPI font/rectangle rounding.
    let expanded = height > p(64);
    TextLayout {
        title: RECT { left, top: p(if expanded { 4 } else { 8 }), right, bottom: if content.detail.is_empty() { height - p(8) } else { p(if expanded { 26 } else { 30 }) } },
        detail: RECT { left, top: p(if expanded { 28 } else { 31 }), right, bottom: height - p(if expanded { 4 } else { 6 }) },
        value,
        align: if content.rtl { DT_RIGHT | DT_RTLREADING } else { DT_LEFT },
    }
}
/// Opaque GDI depth shared with the tray popup. Ellipse dimensions preserve
/// the existing RoundRect corners; this paints a gradient, not backdrop blur.
pub(crate) unsafe fn draw_depth_surface(dc: HDC, width: i32, height: i32, ellipse: i32, border_width: i32) {
    let vertex = |x, y, color: u32| TRIVERTEX {
        x, y, Red: ((color >> 16) as u16 & 0xff) << 8,
        Green: ((color >> 8) as u16 & 0xff) << 8, Blue: (color as u16 & 0xff) << 8,
        Alpha: u16::MAX,
    };
    let middle = height * 45 / 100;
    let vertices = [vertex(0, 0, 0x1a2942), vertex(width, middle, 0x142039),
        vertex(0, middle, 0x142039), vertex(width, height, 0x0d162b)];
    let meshes = [GRADIENT_RECT { UpperLeft: 0, LowerRight: 1 }, GRADIENT_RECT { UpperLeft: 2, LowerRight: 3 }];
    let saved = unsafe { SaveDC(dc) };
    let mut filled = false;
    if saved != 0 {
        let clip = unsafe { CreateRoundRectRgn(0, 0, width, height, ellipse, ellipse) };
        if !clip.is_invalid() {
            if unsafe { ExtSelectClipRgn(dc, Some(clip), RGN_AND) }.0 != 0 {
                filled = unsafe { GradientFill(dc, &vertices, meshes.as_ptr().cast(), meshes.len() as u32, GRADIENT_FILL_RECT_V) }.as_bool();
            }
            unsafe { let _ = DeleteObject(clip.into()); }
        }
        unsafe { let _ = RestoreDC(dc, saved); }
    }
    // Stock brushes avoid allocating a brush for every gradient band or frame.
    // Keep the card legible if a DC/region/gradient operation is unavailable.
    let fallback_color = if filled { None } else { Some(unsafe { SetDCBrushColor(dc, rgb(0x142039)) }) };
    let brush = unsafe { GetStockObject(if filled { NULL_BRUSH } else { DC_BRUSH }) };
    let border = unsafe { CreatePen(PS_SOLID, border_width, rgb(0x344b69)) };
    let previous_brush = unsafe { SelectObject(dc, brush) };
    let previous_pen = unsafe { SelectObject(dc, border.into()) };
    unsafe {
        let _ = RoundRect(dc, 0, 0, width, height, ellipse, ellipse);
        let _ = SelectObject(dc, previous_brush); let _ = SelectObject(dc, previous_pen);
        let _ = DeleteObject(border.into());
        if let Some(color) = fallback_color { let _ = SetDCBrushColor(dc, color); }
    }
}

#[cfg(test)]
pub(crate) fn assert_depth_surface_pixels(bgra: &[u8], width: i32, height: i32, dpi: u32) {
    // These clear surface lanes avoid labels, icons and inset hover fills.
    // A small tolerance covers raster interpolation near the endpoint stops.
    let margin = (3 * dpi / 96).max(3) as i32;
    for (name, x, y, expected, tolerance) in [
        ("top", width / 2, margin, [0x1a, 0x29, 0x42], 3),
        ("middle", margin, height * 45 / 100, [0x14, 0x20, 0x39], 3),
        ("bottom", width / 2, height - margin, [0x0d, 0x16, 0x2b], 3),
        ("border", width / 2, 0, [0x34, 0x4b, 0x69], 0),
        ("rounded corner", 0, 0, [0x09, 0x0e, 0x22], 0),
    ] {
        let offset = ((y * width + x) * 4) as usize;
        let actual = [bgra[offset + 2], bgra[offset + 1], bgra[offset]];
        assert!(actual.iter().zip(expected).all(|(&a, b)| (a as i32 - b).abs() <= tolerance),
            "Depth surface {name} at {dpi} DPI: {actual:?}, expected {expected:?}");
    }
}

/// Shared by WM_PAINT and the opt-in native raster diagnostic.
unsafe fn render_card(dc: HDC, geometry: Geometry, content: &Content) {
    let p = |value| geometry.px(value);
    let bg = unsafe { CreateSolidBrush(rgb(0x090e22)) };
    unsafe { let _ = FillRect(dc, &RECT { left: 0, top: 0, right: geometry.width, bottom: geometry.height }, bg); }
    unsafe { draw_depth_surface(dc, geometry.width, geometry.height, p(18), p(1).max(1));
        let _ = DeleteObject(bg.into()); let _ = SetBkMode(dc, BACKGROUND_MODE(1)); }
    let label_font = unsafe { font(p(13), true) };
    let detail_font = unsafe { font(p(12), false) };
    let value_font = unsafe { font(p(20), true) };
    let previous_font = unsafe { SelectObject(dc, label_font.into()) };
    let width = geometry.width; let height = geometry.height;
    let layout = text_layout(geometry, content);
    if let (Some(value), Some(rect)) = (content.value, layout.value) {
        unsafe { let _ = SelectObject(dc, value_font.into()); text(dc, &target_number(value, content.rtl), rect, target_alignment(content.rtl), 0xeff5ff); let _ = SelectObject(dc, label_font.into()); }
    }
    unsafe { text(dc, &content.title, layout.title, layout.align | DT_VCENTER | DT_SINGLELINE, 0xeff5ff); let _ = SelectObject(dc, detail_font.into()); }
    if !content.detail.is_empty() {
        unsafe { text(dc, &content.detail, layout.detail, layout.align | DT_WORDBREAK, 0xa4b6d2); }
    }
    let icon_x = if content.rtl { width - p(36) } else { p(12) };
    unsafe { draw_icon(dc, content.icon, icon_x, (height - p(24)) / 2, geometry.dpi); let _ = SelectObject(dc, previous_font); let _ = DeleteObject(label_font.into()); let _ = DeleteObject(detail_font.into()); let _ = DeleteObject(value_font.into()); }
}
unsafe fn draw_icon(dc: HDC, icon: NoticeIcon, x: i32, y: i32, dpi: u32) {
    let p = |v: i32| ((v as i64 * dpi as i64 + 48) / 96) as i32;
    let pen = unsafe { CreatePen(PS_SOLID, p(2).max(1), rgb(0x74aaf1)) };
    let previous = unsafe { SelectObject(dc, pen.into()) };
    let previous_brush = unsafe { SelectObject(dc, GetStockObject(NULL_BRUSH)) };
    let line = |points: &[(i32, i32)]| unsafe {
        if let Some(&(a, b)) = points.first() { let _ = MoveToEx(dc, x + p(a), y + p(b), None); }
        for &(a, b) in points.iter().skip(1) { let _ = LineTo(dc, x + p(a), y + p(b)); }
    };
    let curve = |points: &[(i32, i32)]| unsafe {
        let points: Vec<POINT> = points.iter().map(|&(a, b)| POINT { x: x + p(a), y: y + p(b) }).collect();
        let _ = PolyBezier(dc, &points);
    };
    match icon {
        NoticeIcon::Volume | NoticeIcon::Muted => {
            line(&[(2, 9), (6, 9), (11, 5), (11, 19), (6, 15), (2, 15), (2, 9)]);
            if matches!(icon, NoticeIcon::Muted) { line(&[(16, 9), (22, 15)]); line(&[(22, 9), (16, 15)]); }
            else { curve(&[(15, 8), (19, 10), (19, 14), (15, 16)]); curve(&[(19, 4), (24, 8), (24, 16), (19, 20)]); }
        }
        NoticeIcon::Play => line(&[(6, 4), (20, 12), (6, 20), (6, 4)]),
        NoticeIcon::Pause => { line(&[(7, 4), (7, 20)]); line(&[(17, 4), (17, 20)]); }
        NoticeIcon::Stop => line(&[(5, 5), (19, 5), (19, 19), (5, 19), (5, 5)]),
        NoticeIcon::Leveling => { line(&[(4, 8), (4, 16)]); line(&[(9, 4), (9, 20)]); line(&[(14, 7), (14, 17)]); line(&[(19, 2), (19, 22)]); }
        NoticeIcon::Profile => {
            line(&[(2, 7), (5, 7)]); line(&[(11, 7), (22, 7)]);
            line(&[(2, 17), (13, 17)]); line(&[(19, 17), (22, 17)]);
            unsafe { let _ = Ellipse(dc, x + p(5), y + p(4), x + p(11), y + p(10));
                let _ = Ellipse(dc, x + p(13), y + p(14), x + p(19), y + p(20)); }
        }
        NoticeIcon::Smoothing => curve(&[(2, 16), (4, 16), (4, 8), (8, 8), (12, 8), (12, 16), (16, 16), (20, 16), (20, 8), (22, 8)]),
        NoticeIcon::Warning => { line(&[(12, 3), (22, 21), (2, 21), (12, 3)]); line(&[(12, 9), (12, 14)]); line(&[(12, 17), (12, 19)]); }
    }
    unsafe { let _ = SelectObject(dc, previous); let _ = SelectObject(dc, previous_brush); let _ = DeleteObject(pen.into()); }
}
fn wide(value: &str) -> Vec<u16> { value.encode_utf16().chain(Some(0)).collect() }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_digits_follow_language_and_keep_numeric_order() {
        for (value, latin, persian) in [(0, "0", "۰"), (52, "52", "۵۲"), (150, "150", "۱۵۰"), (200, "200", "۲۰۰")] {
            assert_eq!(target_number(value, false), latin);
            assert_eq!(target_number(value, true), persian);
        }
    }
    #[test]
    fn repeated_input_does_not_restart_entry_and_renews_hold() {
        let t = Instant::now(); let mut motion = Motion::new(t, false); motion.renew(t);
        let position = motion.progress(t + Duration::from_millis(80));
        motion.renew(t + Duration::from_millis(80));
        assert_eq!(motion.started, t); assert_eq!(motion.progress(t + Duration::from_millis(80)), position);
        motion.renew(t + Duration::from_millis(1500));
        motion.advance(t + Duration::from_millis(3000)); assert_eq!(motion.phase, Phase::Visible);
        motion.advance(t + Duration::from_millis(3300)); assert_eq!(motion.phase, Phase::Exiting);
    }
    #[test]
    fn exit_reversal_is_continuous_and_uses_same_cycle() {
        let t = Instant::now(); let mut motion = Motion::new(t, false); motion.renew(t);
        let reverse = t + ENTER + HOLD + Duration::from_millis(80);
        motion.advance(reverse); let progress = motion.progress(reverse); assert!(progress > 0.0 && progress < 1.0);
        motion.renew(reverse); assert_eq!(motion.phase, Phase::Entering); assert!((motion.progress(reverse) - progress).abs() < 1e-9);
        motion.advance(reverse + ENTER); assert_eq!(motion.phase, Phase::Visible);
    }
    #[test]
    fn delayed_timer_consumes_all_phases_and_reduced_motion_has_no_frames() {
        let t = Instant::now(); let mut motion = Motion::new(t, false); motion.renew(t);
        motion.advance(t + Duration::from_secs(60)); assert_eq!(motion.phase, Phase::Hidden);
        motion.set_reduced(t + Duration::from_secs(60), true); motion.renew(t + Duration::from_secs(60));
        assert_eq!(motion.phase, Phase::Visible); assert_eq!(motion.progress(t + Duration::from_secs(60)), 1.0);
        motion.advance(t + Duration::from_secs(62)); assert_eq!(motion.phase, Phase::Hidden);
    }
    #[test]
    fn changing_reduced_motion_settles_entry_and_hides_exit() {
        let t = Instant::now(); let mut motion = Motion::new(t, false); motion.renew(t);
        motion.set_reduced(t + Duration::from_millis(50), true); assert_eq!(motion.phase, Phase::Visible);
        motion.set_reduced(t + Duration::from_millis(60), false);
        motion.advance(t + Duration::from_millis(1900)); assert_eq!(motion.phase, Phase::Exiting);
        motion.set_reduced(t + Duration::from_millis(1910), true); assert_eq!(motion.phase, Phase::Hidden);
    }
    #[test]
    fn negative_monitor_coordinates_and_dpi_keep_destination_inside_work_area() {
        for dpi in [96, 120, 144, 192] {
            let mut g = Geometry { work: RECT { left: -2560, top: -900, right: -640, bottom: 180 }, dpi, width: 0, height: 0 };
            g.resize(&Content::default()); let (x, y) = g.position(1.0);
            assert_eq!(g.width, 328 * dpi as i32 / 96); assert_eq!(g.height, 64 * dpi as i32 / 96);
            assert!(x >= g.work.left && x + g.width <= g.work.right); assert!(y >= g.work.top && y + g.height <= g.work.bottom);
            assert!(g.position(0.0).1 + g.height < g.work.top);
        }
    }
    #[test]
    fn long_notice_expands_without_a_vertical_jump_during_entry() {
        let mut geometry = Geometry { work: RECT { left: -1920, top: 40, right: 0, bottom: 1080 }, dpi: 144, width: 0, height: 0 };
        geometry.resize(&Content::default());
        let before = geometry.position(0.65).1;
        let content = Content { detail: "Start audio processing before using this control.".into(), ..Content::default() };
        geometry.resize(&content);
        assert_eq!(geometry.width, 540); assert_eq!(geometry.height, 108);
        assert_eq!(geometry.position(0.65).1, before);
    }
    unsafe fn assert_native_text_fits(dc: HDC, geometry: Geometry, content: &Content, sample: &str) -> String {
        let layout = text_layout(geometry, content);
        let mut report = format!("{sample} {}dpi:", geometry.dpi);
        let number = content.value.map(|value| target_number(value, content.rtl)).unwrap_or_default();
        let runs = [
            ("title", content.title.as_str(), layout.title, layout.align | DT_VCENTER | DT_SINGLELINE, 13, true),
            ("detail", content.detail.as_str(), layout.detail, layout.align | DT_WORDBREAK, 12, false),
            ("value", number.as_str(), layout.value.unwrap_or_default(), target_alignment(content.rtl), 20, true),
        ];
        for (name, value, available, flags, size, bold) in runs {
            if value.is_empty() { continue; }
            let selected_font = unsafe { font(geometry.px(size), bold) };
            let old = unsafe { SelectObject(dc, selected_font.into()) };
            let mut resolved_face = [0u16; 64];
            let face_length = unsafe { GetTextFaceW(dc, Some(&mut resolved_face)) };
            assert!(face_length > 0, "{sample} {}dpi {name}: GDI did not resolve a font face", geometry.dpi);
            let end = resolved_face.iter().position(|&unit| unit == 0).unwrap_or(resolved_face.len());
            let resolved_face = String::from_utf16_lossy(&resolved_face[..end]);
            assert!(resolved_face.eq_ignore_ascii_case("Vazir"), "{sample} {}dpi {name}: expected embedded Vazir, resolved {resolved_face:?}", geometry.dpi);
            let mut measured = available;
            let mut buffer: Vec<u16> = value.encode_utf16().collect();
            unsafe { let _ = DrawTextW(dc, &mut buffer, &mut measured, flags | DT_CALCRECT | DT_NOPREFIX); let _ = SelectObject(dc, old); let _ = DeleteObject(selected_font.into()); }
            let measured_width = measured.right - measured.left;
            let measured_height = measured.bottom - measured.top;
            let available_width = available.right - available.left;
            let available_height = available.bottom - available.top;
            assert!(measured_width <= available_width && measured_height <= available_height,
                "{sample} {}dpi {name} clipped: text={value:?}; measured={measured_width}x{measured_height}; available={available_width}x{available_height}", geometry.dpi);
            report.push_str(&format!(" {name}={measured_width}x{measured_height}/{available_width}x{available_height} face={resolved_face}"));
        }
        report.push('\n'); report
    }
    /// Opt-in native GDI raster evidence, using the exact WM_PAINT draw path.
    /// This does not establish physical mixed-DPI, focus, or mouse behavior.
    #[test]
    #[ignore = "writes native GDI evidence only to MAXI_POPUP_EVIDENCE_DIR"]
    fn capture_native_osd_cards() -> Result<()> {
        let directory = std::path::PathBuf::from(std::env::var_os("MAXI_POPUP_EVIDENCE_DIR").context("Set MAXI_POPUP_EVIDENCE_DIR explicitly")?);
        std::fs::create_dir_all(&directory)?;
        ensure_vazir_registered()?;
        let samples = [
            ("volume-en", "Volume target", "", false, NoticeIcon::Volume, Some(150)),
            ("volume-fa", "سطح صدای هدف", "", true, NoticeIcon::Volume, Some(150)),
            ("saved-stopped-en", "Playback stopped", "Saved · Run to apply", false, NoticeIcon::Volume, Some(150)),
            ("saved-stopped-fa", "پردازش متوقف است", "ذخیره شد؛ «اجرا» را بزنید", true, NoticeIcon::Volume, Some(150)),
            ("saved-paused-en", "Playback paused", "Saved · Resume to apply", false, NoticeIcon::Volume, Some(150)),
            ("saved-paused-fa", "پردازش در حالت مکث است", "ذخیره شد؛ «ادامه» را بزنید", true, NoticeIcon::Volume, Some(150)),
            ("saved-paused-muted-en", "Playback paused", "Saved · Resume to apply", false, NoticeIcon::Muted, Some(0)),
            ("saved-paused-muted-fa", "پردازش در حالت مکث است", "ذخیره شد؛ «ادامه» را بزنید", true, NoticeIcon::Muted, Some(0)),
            ("unavailable-mode-en", "Smoothing unavailable", "Requires switching to virtual cable processing mode.", false, NoticeIcon::Warning, None),
            ("unavailable-mode-fa", "نرم‌سازی غیرفعال", "نیازمند تغییر حالت به پردازش کابل مجازی", true, NoticeIcon::Warning, None),
            ("unavailable-enhancement-en", "Enhancement unavailable", "Requires switching to virtual cable processing mode.", false, NoticeIcon::Warning, None),
            ("unavailable-enhancement-fa", "بهبود صدا غیرفعال", "نیازمند تغییر حالت به پردازش کابل مجازی", true, NoticeIcon::Warning, None),
            ("unavailable-cycle-profile-en", "Profile unavailable", "Requires switching to virtual cable processing mode.", false, NoticeIcon::Warning, None),
            ("unavailable-cycle-profile-fa", "پروفایل غیرفعال", "نیازمند تغییر حالت به پردازش کابل مجازی", true, NoticeIcon::Warning, None),
            ("unavailable-profile-en", "Smoothing unavailable", "Select an enhancement profile to hear treble smoothing.", false, NoticeIcon::Warning, None),
            ("unavailable-profile-fa", "نرم‌سازی غیرفعال", "برای شنیدن اثر، یک پروفایل بهبود صدا انتخاب کن.", true, NoticeIcon::Warning, None),
            ("not-ready-en", "Audio is not ready", "Wait for audio-device initialization and recovery.", false, NoticeIcon::Warning, None),
            ("not-ready-fa", "صدا هنوز آماده نیست", "منتظر آماده‌شدن دستگاه‌ها و بازیابی صدا بمان.", true, NoticeIcon::Warning, None),
            ("audio-failed-en", "Audio action failed", "Open the app log for details.", false, NoticeIcon::Warning, None),
            ("audio-failed-fa", "عملیات صدا ناموفق بود", "برای جزئیات، گزارش برنامه را باز کن.", true, NoticeIcon::Warning, None),
            ("muted-en", "Playback stopped", "Saved · Run to apply", false, NoticeIcon::Muted, Some(0)),
            ("muted-fa", "پردازش متوقف است", "ذخیره شد؛ «اجرا» را بزنید", true, NoticeIcon::Muted, Some(0)),
            ("play-en", "Audio resumed", "", false, NoticeIcon::Play, None),
            ("play-fa", "پخش ادامه یافت", "", true, NoticeIcon::Play, None),
            ("stop-en", "Audio stopped", "The audio device has been restored.", false, NoticeIcon::Stop, None),
            ("stop-fa", "صدا متوقف شد", "دستگاه صوتی بازگردانی شد.", true, NoticeIcon::Stop, None),
            ("leveling-en", "Steady loudness on", "", false, NoticeIcon::Leveling, None),
            ("leveling-fa", "بلندی پایدار روشن", "", true, NoticeIcon::Leveling, None),
            ("smoothing-en", "Treble smoothing on", "", false, NoticeIcon::Smoothing, None),
            ("smoothing-fa", "نرم‌سازی صدای زیر روشن", "", true, NoticeIcon::Smoothing, None),
            ("pause-en", "Audio paused", "Processing is paused", false, NoticeIcon::Pause, None),
            ("pause-fa", "پردازش مکث شد", "پردازش صدا در حالت مکث است", true, NoticeIcon::Pause, None),
            ("profile-en", "Sound profile", "Voice / dialogue", false, NoticeIcon::Profile, None),
            ("profile-fa", "پروفایل صدا", "گفتار و دیالوگ", true, NoticeIcon::Profile, None),
            ("warning-en", "Audio control unavailable", "Start audio processing before using this control.", false, NoticeIcon::Warning, None),
            ("warning-fa", "کنترل صدا در دسترس نیست", "برای استفاده از این کنترل، ابتدا پردازش صدا را شروع کن.", true, NoticeIcon::Warning, None),
        ];
        let mut fit_report = String::from("Native GDI DT_CALCRECT measurements against the production text layout. Synthetic fixed messages; not physical DPI validation.\n");
        for dpi in [96, 120, 144, 192] {
            for &(name, title, detail, rtl, icon, value) in &samples {
                let content = Content { title: title.into(), detail: detail.into(), rtl, icon, value };
                let mut geometry = Geometry { work: RECT { left: 0, top: 0, right: 3840, bottom: 2160 }, dpi, width: 0, height: 0 };
                geometry.resize(&content);
                let dc = unsafe { CreateCompatibleDC(None) };
                anyhow::ensure!(!dc.is_invalid(), "CreateCompatibleDC failed");
                fit_report.push_str(&unsafe { assert_native_text_fits(dc, geometry, &content, name) });
                let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: geometry.width, biHeight: -geometry.height,
                    biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0,
                    ..Default::default()
                }, ..Default::default() };
                let mut bits = std::ptr::null_mut();
                let bitmap = match unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) } {
                    Ok(bitmap) => bitmap,
                    Err(error) => { unsafe { let _ = DeleteDC(dc); } return Err(error.into()); }
                };
                let old = unsafe { SelectObject(dc, bitmap.into()) };
                unsafe { render_card(dc, geometry, &content); let _ = GdiFlush(); }
                let source = unsafe { std::slice::from_raw_parts(bits as *const u8, (geometry.width * geometry.height * 4) as usize) };
                assert_depth_surface_pixels(source, geometry.width, geometry.height, dpi);
                let mut rgba = Vec::with_capacity(source.len());
                for pixel in source.chunks_exact(4) { rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]); }
                unsafe { let _ = SelectObject(dc, old); let _ = DeleteObject(bitmap.into()); let _ = DeleteDC(dc); }
                let file = std::fs::File::create(directory.join(format!("osd-{name}-{dpi}dpi.png")))?;
                let mut encoder = png::Encoder::new(file, geometry.width as u32, geometry.height as u32);
                encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header()?.write_image_data(&rgba)?;
            }
        }
        std::fs::write(directory.join("osd-native-text-fit.txt"), fit_report)?;
        Ok(())
    }
    #[test]
    #[ignore = "bounded actual HWND probe with synthetic notices and window messages"]
    fn native_osd_lifecycle_probe() -> Result<()> {
        use std::io::Write;
        let directory = std::path::PathBuf::from(std::env::var_os("MAXI_POPUP_EVIDENCE_DIR").context("Set MAXI_POPUP_EVIDENCE_DIR explicitly")?);
        // Resolve existing paths before opening evidence files, including junctions.
        // Requiring a precreated directory prevents writes through an unchecked parent.
        let directory = directory.canonicalize().context("Precreate MAXI_POPUP_EVIDENCE_DIR inside this workspace")?;
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
        anyhow::ensure!(directory.is_dir() && directory != workspace && directory.starts_with(&workspace), "Native probe evidence must be strictly inside this workspace");
        std::fs::write(directory.join("osd-native-lifecycle.txt"), "INCOMPLETE: native motion probe started; only a final PASS confirms completion. See osd-native-motion.csv for observations.\n")?;
        #[derive(Clone, Copy)]
        struct Sample { at: Instant, phase: Phase, visible: bool, rect: RECT, alpha: u8 }
        struct Probe { start: Instant, file: std::fs::File, samples: Vec<Sample>, motion_timers: usize, hold_timers: usize }
        impl Probe {
            fn sample(&mut self, window: &Osd, label: &str) -> Result<Sample> {
                let mut rect = RECT::default(); let mut alpha = 0; let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
                unsafe {
                    GetWindowRect(window.hwnd, &mut rect)?;
                    GetLayeredWindowAttributes(window.hwnd, None, Some(&mut alpha), Some(&mut flags))?;
                }
                assert_eq!(flags.0 & LWA_ALPHA.0, LWA_ALPHA.0);
                let visible = window.is_visible();
                let state = window._state.borrow();
                let sample = Sample { at: Instant::now(), phase: state.motion.phase, visible, rect, alpha };
                if sample.visible {
                    let geometry = state.geometry.context("Visible native OSD lost its geometry")?;
                    assert_eq!(rect.right - rect.left, geometry.width);
                    assert_eq!(rect.bottom - rect.top, geometry.height);
                    assert_eq!(rect.left, geometry.position(1.0).0);
                    // Both values come from native queries, not Motion::progress.
                    // Alpha quantization permits one pixel at these desktop DPIs.
                    let alpha_y = geometry.position(alpha as f64 / 248.0).1;
                    assert!((rect.top - alpha_y).abs() <= 2, "Native position and layered alpha diverged");
                }
                writeln!(self.file, "{},{label},{:?},{},{},{},{},{},{}", sample.at.duration_since(self.start).as_micros(), sample.phase, sample.visible, rect.left, rect.top, rect.right, rect.bottom, alpha)?;
                self.samples.push(sample);
                Ok(sample)
            }
            fn wait(&mut self, window: &Osd, label: &str, timeout: Duration, ready: impl Fn(Sample) -> bool) -> Result<Sample> {
                let deadline = Instant::now() + timeout;
                loop {
                    let mut message = MSG::default();
                    while unsafe { PeekMessageW(&mut message, Some(window.hwnd), 0, 0, PM_REMOVE) }.as_bool() {
                        if message.message == WM_TIMER {
                            match message.wParam.0 { MOTION_TIMER => self.motion_timers += 1, HOLD_TIMER => self.hold_timers += 1, _ => {} }
                        }
                        unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
                    }
                    let sample = self.sample(window, label)?;
                    if ready(sample) { return Ok(sample); }
                    anyhow::ensure!(Instant::now() < deadline, "Timed out during {label}: phase={:?}, visible={}, alpha={}, top={}", sample.phase, sample.visible, sample.alpha, sample.rect.top);
                    // Yield this same UI thread; no worker or fabricated clock/timer.
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            fn pump(&mut self, window: &Osd, label: &str, duration: Duration) -> Result<()> {
                let until = Instant::now() + duration;
                self.wait(window, label, duration + Duration::from_secs(1), |sample| sample.at >= until)?;
                Ok(())
            }
            fn timers(&self) -> usize { self.motion_timers + self.hold_timers }
        }
        let window = Osd::new()?;
        let hwnd = window.hwnd;
        let reduced = window._state.borrow().motion.reduced;
        let branch = if reduced { "system reduced motion: instant show/hide; exit reversal unavailable" } else { "system animations enabled: native entry, hold, exit and exit reversal" };
        let mut probe = Probe { start: Instant::now(), file: std::fs::File::create(directory.join("osd-native-motion.csv"))?, samples: Vec::new(), motion_timers: 0, hold_timers: 0 };
        writeln!(probe.file, "# {branch}; synthetic notices; real Instant and OS WM_TIMER; native GetWindowRect/GetLayeredWindowAttributes")?;
        writeln!(probe.file, "elapsed_us,stage,phase,visible,left,top,right,bottom,alpha")?;
        let foreground = unsafe { GetForegroundWindow() };
        window.show_notice(Notice { title: "Synthetic native probe", detail: "App target", rtl: false, icon: NoticeIcon::Volume, value: Some(150), anchor: None });
        assert!(window.is_visible());
        assert_eq!(crate::native_tray::own_window(), None, "Standalone OSD was mistaken for the main app");
        // A hidden synthetic main window avoids opening the host app or audio.
        // The shown topmost OSD must not hide this exact-title HWND from discovery.
        struct SyntheticMain(HWND);
        impl Drop for SyntheticMain {
            fn drop(&mut self) { unsafe { let _ = DestroyWindow(self.0); } }
        }
        let class = wide("STATIC"); let main_title = wide("MAXI SOUNDSET");
        let main_window = SyntheticMain(unsafe { CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()), PCWSTR(main_title.as_ptr()), WS_POPUP, 0, 0, 16, 16,
            None, None, Some(window.module), None) }?);
        assert_eq!(crate::native_tray::own_window(), Some(main_window.0), "Shown OSD interfered with exact main-window discovery");
        assert_eq!(unsafe { GetForegroundWindow() }, foreground, "OSD activated the foreground window");
        let styles = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
        for flag in [WS_EX_NOACTIVATE, WS_EX_TRANSPARENT, WS_EX_TOOLWINDOW, WS_EX_LAYERED, WS_EX_TOPMOST] { assert_eq!(styles & flag.0, flag.0); }
        assert_eq!(unsafe { SendMessageW(hwnd, WM_NCHITTEST, None, None) }.0, HTTRANSPARENT as isize);
        assert_eq!(unsafe { SendMessageW(hwnd, WM_MOUSEACTIVATE, None, None) }.0, MA_NOACTIVATE as isize);
        probe.pump(&window, "explicit-hide-setup", Duration::from_millis(70))?;
        window.hide(); assert!(!window.is_visible());
        // Drain a potentially already-queued WM_TIMER before checking shutdown.
        probe.pump(&window, "explicit-hide-drain", Duration::from_millis(30))?;
        let stopped = probe.timers();
        // Cover the maximum HOLD period as well as the 15 ms motion timer.
        probe.pump(&window, "explicit-hide-idle", HOLD + Duration::from_millis(150))?;
        assert_eq!(probe.timers(), stopped, "Hidden OSD kept generating timer messages");

        window.show_notice(Notice { title: "Synthetic native motion", detail: "App target", rtl: false, icon: NoticeIcon::Volume, value: Some(150), anchor: None });
        let geometry = window._state.borrow().geometry.context("No monitor geometry")?;
        let entry_begin = probe.samples.len();
        let initial = probe.sample(&window, "entry-start")?;
        assert_eq!(initial.alpha, if reduced { 248 } else { 0 });
        assert_eq!(initial.rect.top, geometry.position(if reduced { 1.0 } else { 0.0 }).1);
        probe.wait(&window, "entry", Duration::from_secs(3), |sample| sample.phase == Phase::Visible && sample.alpha == 248)?;
        let entry_end = probe.samples.len();
        if !reduced {
            assert!(probe.samples[entry_begin..entry_end].iter().any(|s| s.alpha > 0 && s.alpha < 248), "No actual intermediate entry frame observed");
            assert!(probe.samples[entry_begin..entry_end].windows(2).all(|pair| pair[1].alpha >= pair[0].alpha && pair[1].rect.top >= pair[0].rect.top), "Native entry reversed direction");
        }
        let hold_begin = probe.samples.len();
        let motion_before_hold = probe.motion_timers;
        probe.pump(&window, "hold", Duration::from_millis(120))?;
        assert!(probe.samples[hold_begin..].iter().all(|s| s.visible && s.phase == Phase::Visible && s.alpha == 248 && s.rect.top == geometry.position(1.0).1), "Native hold was not settled");
        assert_eq!(probe.motion_timers, motion_before_hold, "Settled hold continued animation timers");

        if reduced {
            // Renewal remains measurable without changing the user's animation setting.
            let original_deadline = window._state.borrow().motion.hold_until;
            window.show_notice(Notice { title: "Synthetic renewed motion", detail: "App target", rtl: false, icon: NoticeIcon::Volume, value: Some(150), anchor: None });
            let renewed = probe.sample(&window, "reduced-renew")?;
            assert_eq!(renewed.alpha, 248); assert_eq!(renewed.rect.top, geometry.position(1.0).1);
            probe.wait(&window, "reduced-renewed-hold", HOLD + Duration::from_secs(1), |s| s.at >= original_deadline + Duration::from_millis(30))?;
            assert!(window.is_visible(), "Reduced-motion renewal failed to extend the real hold");
            assert_eq!(probe.motion_timers, 0, "Reduced-motion path generated animation frames");
        } else {
            let exit_begin = probe.samples.len();
            let before = probe.wait(&window, "exit-before-renew", HOLD + EXIT + Duration::from_secs(2), |s| s.phase == Phase::Exiting && s.alpha < 224 && s.alpha > 32)?;
            assert!(probe.samples[exit_begin..].windows(2).all(|pair| pair[1].alpha <= pair[0].alpha && pair[1].rect.top <= pair[0].rect.top), "Native exit reversed before renewal");
            let (old_start, old_duration, old_from) = { let state = window._state.borrow(); (state.motion.started, state.motion.duration, state.motion.from) };
            window.show_notice(Notice { title: "Synthetic renewed motion", detail: "App target", rtl: false, icon: NoticeIcon::Volume, value: Some(150), anchor: None });
            let after = probe.sample(&window, "exit-renew")?;
            let state = window._state.borrow();
            assert_eq!(state.motion.phase, Phase::Entering);
            let t = (state.motion.started.saturating_duration_since(old_start).as_secs_f64() / old_duration.as_secs_f64()).clamp(0.0, 1.0);
            let continuation = old_from * (1.0 - t * t);
            assert!((state.motion.from - continuation).abs() < 1e-9);
            assert_eq!(after.alpha, (continuation * 248.0).round() as u8, "Native renewal restarted opacity");
            assert_eq!(after.rect.top, geometry.position(continuation).1, "Native renewal restarted position");
            // Bound the observed native jump by actual elapsed time and exit's
            // maximum speed, allowing 5 ms for the last dispatched frame/query.
            let elapsed = state.motion.started.saturating_duration_since(before.at) + Duration::from_millis(5);
            let allowed_progress = 2.0 * elapsed.as_secs_f64() / EXIT.as_secs_f64();
            assert!((after.alpha as i32 - before.alpha as i32).abs() <= (248.0 * allowed_progress).ceil() as i32 + 1, "Native renewal had an unexplained opacity jump");
            let travel = geometry.position(1.0).1 - geometry.position(0.0).1;
            assert!((after.rect.top - before.rect.top).abs() <= (travel as f64 * allowed_progress).ceil() as i32 + 1, "Native renewal had an unexplained position jump");
            let renewed_geometry = state.geometry.unwrap();
            assert_eq!((renewed_geometry.work.left, renewed_geometry.work.top, renewed_geometry.dpi), (geometry.work.left, geometry.work.top, geometry.dpi), "Renewal changed destination");
            drop(state);
            let reverse_begin = probe.samples.len() - 1;
            probe.wait(&window, "renewed-entry", Duration::from_secs(3), |s| s.phase == Phase::Visible && s.alpha == 248)?;
            assert!(probe.samples[reverse_begin..].windows(2).all(|pair| pair[1].visible && pair[1].alpha >= pair[0].alpha && pair[1].rect.top >= pair[0].rect.top), "Renewed native entry flashed or reversed");
        }
        let final_exit_begin = probe.samples.len();
        probe.wait(&window, "final-expiry", HOLD + EXIT + Duration::from_secs(2), |s| !s.visible && s.phase == Phase::Hidden)?;
        if !reduced {
            assert!(probe.samples[final_exit_begin..].iter().any(|s| s.visible && s.phase == Phase::Exiting && s.alpha > 0 && s.alpha < 248), "No actual intermediate final exit frame observed");
            let final_visible: Vec<_> = probe.samples[final_exit_begin..].iter().filter(|s| s.visible).collect();
            assert!(final_visible.windows(2).all(|pair| pair[1].alpha <= pair[0].alpha && pair[1].rect.top <= pair[0].rect.top), "Final native exit reversed direction");
        }
        assert!(!window.is_visible(), "OSD did not auto-hide");
        assert_eq!(window._state.borrow().motion.phase, Phase::Hidden);
        assert!(window._state.borrow().geometry.is_none());
        probe.pump(&window, "expiry-drain", Duration::from_millis(30))?;
        let stopped = probe.timers();
        // A leaked HOLD timer must have time to arrive before accepting idle.
        probe.pump(&window, "expiry-idle", HOLD + Duration::from_millis(150))?;
        assert_eq!(probe.timers(), stopped, "Expired OSD kept generating timer messages");
        assert!(probe.hold_timers > 0, "Real hold timer was never delivered");
        assert_eq!(!animations_enabled(), reduced, "System animation preference changed during this run; rerun the probe");
        assert_eq!(window._state.borrow().motion.reduced, reduced);
        drop(window);
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool(), "OSD HWND survived Drop");
        drop(main_window);
        assert_eq!(crate::native_tray::own_window(), None, "Synthetic main HWND survived cleanup");
        std::fs::write(directory.join("osd-native-lifecycle.txt"), format!("PASS: {branch}. Actual HWND rectangle and layered-alpha samples: {}; real motion timers: {}; real hold timers: {}. Native settled hold, renewal continuity, eventual hide/geometry clear, explicit/automatic timer shutdown and HWND Drop verified. Existing system animation preference was read, never changed. Standalone OSD excluded from discovery; synthetic exact-title main HWND identified and cleaned up; foreground unchanged at show; required window styles checked. Notices and WM_NCHITTEST/WM_MOUSEACTIVATE calls are synthetic. CSV records native API observations, not animation video or physical click-through/mixed-monitor/DPI validation.\n", probe.samples.len(), probe.motion_timers, probe.hold_timers))?;
        Ok(())
    }
    // Opt-in cross-process fixture. Official CU screenshot-backed pointer clicks
    // are the only intended input source. A tool-rejected click is inconclusive;
    // no test code forwards, injects, invokes, or replays input.
    #[link(name = "kernel32")]
    unsafe extern "system" { fn GetTickCount64() -> u64; }
    fn fixture_tick() -> u64 { unsafe { GetTickCount64() } }
    fn fixture_message_tick(low: u32, now: u64) -> Option<u64> {
        let candidate = (now & !0xffff_ffff) | low as u64;
        if candidate <= now { Some(candidate) } else { candidate.checked_sub(1u64 << 32) }
    }
    fn fixture_directory() -> Result<std::path::PathBuf> {
        let directory = std::path::PathBuf::from(std::env::var_os("MAXI_POPUP_EVIDENCE_DIR").context("Set MAXI_POPUP_EVIDENCE_DIR to a precreated workspace directory")?).canonicalize()?;
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
        anyhow::ensure!(directory.is_dir() && directory != workspace && directory.starts_with(&workspace), "Fixture evidence must be strictly inside this workspace");
        Ok(directory)
    }
    fn fixture_values(line: &str) -> Option<Vec<i64>> { line.trim().split(',').map(str::parse).collect::<std::result::Result<Vec<_>, _>>().ok() }
    fn fixture_native_sample(window: &Osd, generation: u32) -> Result<String> {
        let begin = fixture_tick();
        let mut rect = RECT::default(); let mut alpha = 0; let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0); let mut pid = 0;
        unsafe {
            GetWindowRect(window.hwnd, &mut rect)?;
            GetLayeredWindowAttributes(window.hwnd, None, Some(&mut alpha), Some(&mut flags))?;
            GetWindowThreadProcessId(window.hwnd, Some(&mut pid));
        }
        // Before the first show there are no layered attributes yet; callers
        // initialize a hidden frame once before collecting observations.
        let styles = unsafe { GetWindowLongPtrW(window.hwnd, GWL_EXSTYLE) } as u32;
        let visible = window.is_visible();
        Ok(format!("{},{},{},{},{},{},{},{},{},{},{},{}", fixture_tick(), pid, window.hwnd.0 as usize, visible as u8, alpha, rect.left, rect.top, rect.right, rect.bottom, styles, generation, begin))
    }
    #[test]
    #[ignore = "internal child of native_osd_clickthrough_fixture; requires its fresh nonce contract"]
    fn native_osd_clickthrough_worker() -> Result<()> {
        use std::io::Write;
        let directory = fixture_directory()?;
        let nonce = std::env::var("MAXI_OSD_FIXTURE_NONCE")?;
        let parent: u32 = std::env::var("MAXI_OSD_FIXTURE_PARENT")?.parse()?;
        anyhow::ensure!(directory.file_name().and_then(|s| s.to_str()) == Some(nonce.as_str()), "Worker nonce directory mismatch");
        anyhow::ensure!(std::fs::read_to_string(directory.join("contract.txt"))? == format!("{nonce}\n{parent}\n"), "Worker nonce contract mismatch");
        let window = Osd::new()?;
        // Initialize native alpha without showing a notice or a visible frame.
        unsafe { SetLayeredWindowAttributes(window.hwnd, COLORREF(0), 0, LWA_ALPHA)?; }
        let mut samples = std::fs::File::create(directory.join("worker-native.csv"))?;
        writeln!(samples, "sample_end_ms,pid,hwnd,visible,alpha,left,top,right,bottom,exstyle,generation,sample_begin_ms")?;
        let deadline = fixture_tick() + 95_000;
        let mut generation = 0; let mut armed_until = 0; let mut last_renew = 0;
        while fixture_tick() < deadline && !directory.join("stop.txt").exists() {
            let now = fixture_tick();
            if let Ok(request) = std::fs::read_to_string(directory.join("show.txt")) {
                if let Some(values) = fixture_values(&request).filter(|v| v.len() == 3) {
                    if values[0] > generation as i64 {
                        generation = values[0] as u32; armed_until = (now + 20_000).min(deadline);
                        window.show_notice(Notice { title: "Synthetic click-through fixture", detail: "Renewed for at most 20 seconds per Show", rtl: false, icon: NoticeIcon::Volume, value: Some(150), anchor: Some((values[1] as i32, values[2] as i32)) });
                        last_renew = now;
                    }
                }
            }
            if now < armed_until && now.saturating_sub(last_renew) >= 100 {
                window.show_notice(Notice { title: "Synthetic click-through fixture", detail: "Renewed for at most 20 seconds per Show", rtl: false, icon: NoticeIcon::Volume, value: Some(150), anchor: None });
                last_renew = now;
            }
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, Some(window.hwnd), 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
            }
            let row = fixture_native_sample(&window, generation)?;
            writeln!(samples, "{row}")?;
            std::fs::write(directory.join("latest.txt"), format!("{row}\n"))?;
            std::thread::sleep(Duration::from_millis(5));
        }
        let hwnd = window.hwnd; window.hide();
        writeln!(samples, "{}", fixture_native_sample(&window, generation)?)?;
        drop(window);
        anyhow::ensure!(!unsafe { IsWindow(Some(hwnd)) }.as_bool(), "Worker HWND survived cleanup");
        std::fs::write(directory.join("worker-stop.txt"), format!("natural stop; pid={}; tick={}; own HWND destroyed; reduced_motion={}; synthetic renewal (not normal hold) <=20s per Show\n", std::process::id(), fixture_tick(), !animations_enabled()))?;
        Ok(())
    }
    #[test]
    #[ignore = "owned two-process fixture requiring real official CU pointer clicks; bounded to 100 seconds"]
    fn native_osd_clickthrough_fixture() -> Result<()> {
        use std::{io::Write, os::windows::process::CommandExt};
        #[derive(Clone)]
        struct Click { tick: u64, dispatch: u64, kind: &'static str, point: POINT }
        struct Underlay { events: Vec<Click>, queued_pointer: Option<(u32, u64, POINT)>, target: HWND, original: WNDPROC, requested: u32, finish: Option<u64> }
        unsafe extern "system" fn target_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Underlay;
            if !pointer.is_null() {
                if message == WM_LBUTTONDOWN || message == WM_LBUTTONUP {
                    let dispatch = fixture_tick();
                    // MSG.time/pt retain original OS queue time/screen position;
                    // a synchronous sent pointer message has no qualifying record.
                    let queued = unsafe { (*pointer).queued_pointer.take() };
                    if let Some((_, tick, point)) = queued.filter(|(kind, _, _)| *kind == message) {
                        unsafe { (*pointer).events.push(Click { tick, dispatch, kind: if message == WM_LBUTTONDOWN { "down" } else { "up" }, point }); }
                    } else {
                        unsafe { (*pointer).events.push(Click { tick: dispatch, dispatch, kind: "unqualified-pointer", point: POINT::default() }); }
                    }
                }
                let original = unsafe { (*pointer).original };
                if message == WM_NCDESTROY {
                    // Restore only this owned BUTTON; state still outlives destruction.
                    unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, original.map(|proc| proc as *const () as isize).unwrap_or(0)); SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0); }
                }
                return unsafe { CallWindowProcW(original, hwnd, message, w, l) };
            }
            unsafe { DefWindowProcW(hwnd, message, w, l) }
        }
        unsafe extern "system" fn underlay_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if message == WM_NCCREATE {
                let create = unsafe { &*(l.0 as *const CREATESTRUCTW) };
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize); }
            }
            let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Underlay;
            if !pointer.is_null() && (message == WM_COMMAND || message == WM_CLOSE) {
                let dispatch = fixture_tick(); let mut tick = dispatch;
                let mut point = POINT::default(); let _ = unsafe { GetCursorPos(&mut point) };
                let data = unsafe { &mut *pointer };
                let kind = if message == WM_CLOSE { "close" } else {
                    match w.0 & 0xffff {
                        101 => { data.requested += 1; "show" },
                        102 if l.0 == data.target.0 as isize && w.0 >> 16 == BN_CLICKED as usize => "click",
                        103 => "finish", _ => "other",
                    }
                };
                if kind == "click" {
                    if let Some(up) = data.events.last().filter(|event| event.kind == "up") { tick = up.tick; point = up.point; }
                }
                data.events.push(Click { tick, dispatch, kind, point });
                if kind == "finish" || kind == "close" { data.finish = Some(fixture_tick()); }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, message, w, l) }
        }
        struct Cleanup { hwnd: HWND, module: HINSTANCE, class: Vec<u16>, child: Option<std::process::Child>, directory: std::path::PathBuf }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::write(self.directory.join("stop.txt"), "owned parent cleanup");
                if let Some(child) = self.child.as_mut() {
                    let until = fixture_tick() + 2_000;
                    while fixture_tick() < until && matches!(child.try_wait(), Ok(None)) { std::thread::sleep(Duration::from_millis(10)); }
                    if matches!(child.try_wait(), Ok(None)) { let _ = child.kill(); let _ = std::fs::write(self.directory.join("forced-own-child-stop.txt"), "Owned fixture child did not stop naturally within 2 seconds"); }
                    let _ = child.wait();
                }
                unsafe { let _ = DestroyWindow(self.hwnd); let _ = UnregisterClassW(PCWSTR(self.class.as_ptr()), Some(self.module)); }
            }
        }
        let root = fixture_directory()?;
        let nonce = format!("clickthrough-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
        let directory = root.join(&nonce); std::fs::create_dir(&directory)?;
        std::fs::write(directory.join("contract.txt"), format!("{nonce}\n{}\n", std::process::id()))?;
        std::fs::write(directory.join("result.txt"), "INCOMPLETE: awaiting baseline pointer click, Show overlay, target pointer click while child visible, then Finish. A rejected CU click is INCONCLUSIVE.\n")?;
        let mut data = Box::new(Underlay { events: Vec::new(), queued_pointer: None, target: HWND::default(), original: None, requested: 0, finish: None });
        let module: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
        let class = wide(&format!("MaxiSoundSet.ClickThroughFixture.{}", std::process::id()));
        let descriptor = WNDCLASSW { lpfnWndProc: Some(underlay_proc), hInstance: module, lpszClassName: PCWSTR(class.as_ptr()), hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut c_void), ..Default::default() };
        anyhow::ensure!(unsafe { RegisterClassW(&descriptor) } != 0, "Could not register fixture underlay");
        let title = wide("MAXI SOUNDSET - Click-through fixture");
        let hwnd = unsafe { CreateWindowExW(WS_EX_APPWINDOW, PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr()), WS_OVERLAPPEDWINDOW,
            300, 0, 520, 260, None, None, Some(module), Some((&mut *data as *mut Underlay).cast())) }?;
        let mut cleanup = Cleanup { hwnd, module, class, child: None, directory: directory.clone() };
        let button = |id: usize, label: &str, y: i32| -> Result<HWND> {
            let class = wide("BUTTON"); let label = wide(label);
            Ok(unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(label.as_ptr()), WS_CHILD | WS_VISIBLE,
                12, y, 300, 34, Some(hwnd), Some(HMENU(id as *mut c_void)), Some(module), None) }?)
        };
        data.target = button(102, "Target clicks: 0", 0)?;
        let _show = button(101, "Show overlay (renew for <=20 seconds)", 70)?;
        let _finish = button(103, "Finish", 120)?;
        unsafe { SetWindowLongPtrW(data.target, GWLP_USERDATA, (&mut *data as *mut Underlay) as isize); }
        let original = unsafe { SetWindowLongPtrW(data.target, GWLP_WNDPROC, target_proc as *const () as isize) };
        anyhow::ensure!(original != 0, "Could not subclass own fixture target");
        data.original = unsafe { std::mem::transmute::<isize, WNDPROC>(original) };
        let log = std::fs::File::create(directory.join("worker-output.txt"))?;
        let child = std::process::Command::new(std::env::current_exe()?).args(["--ignored", "--exact", "osd::tests::native_osd_clickthrough_worker", "--nocapture", "--test-threads=1"])
            .env("MAXI_POPUP_EVIDENCE_DIR", &directory).env("MAXI_OSD_FIXTURE_NONCE", &nonce).env("MAXI_OSD_FIXTURE_PARENT", std::process::id().to_string())
            .creation_flags(0x08000000).stdout(log.try_clone()?).stderr(log).spawn()?;
        let child_pid = child.id(); cleanup.child = Some(child);
        anyhow::ensure!(child_pid != std::process::id(), "Worker must own a different process");
        unsafe { let _ = ShowWindow(hwnd, SW_SHOWNORMAL); }
        println!("Click-through fixture: {} ; parent PID={} ; child PID={} ; lifetime <=100s. Click Target once, Show overlay, Target again, Finish. Only screenshot-backed coordinate clicks qualify.", directory.display(), std::process::id(), child_pid);
        let mut events_file = std::fs::File::create(directory.join("underlay-events.csv"))?;
        writeln!(events_file, "original_os_tick_ms,dispatch_tick_ms,kind,x,y,parent_pid")?;
        let deadline = fixture_tick() + 100_000; let mut requested = 0; let mut logged = 0; let mut aligned = 0; let mut displayed_count = 0;
        while fixture_tick() < deadline && !data.finish.is_some_and(|tick| fixture_tick() >= tick + 200) {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { let _ = TranslateMessage(&message); }
                data.queued_pointer = if message.hwnd == data.target && (message.message == WM_LBUTTONDOWN || message.message == WM_LBUTTONUP) {
                    fixture_message_tick(message.time, fixture_tick()).map(|tick| (message.message, tick, message.pt))
                } else { None };
                unsafe { DispatchMessageW(&message); }
                data.queued_pointer = None;
            }
            for event in &data.events[logged..] { writeln!(events_file, "{},{},{},{},{},{}", event.tick, event.dispatch, event.kind, event.point.x, event.point.y, std::process::id())?; }
            logged = data.events.len();
            let count = data.events.iter().filter(|e| e.kind == "click").count();
            if count != displayed_count {
                let label = wide(&format!("Target clicks: {count}")); unsafe { SetWindowTextW(data.target, PCWSTR(label.as_ptr()))?; }
                displayed_count = count;
            }
            if data.requested != requested {
                requested = data.requested;
                let mut rect = RECT::default(); unsafe { GetWindowRect(hwnd, &mut rect)?; }
                std::fs::write(directory.join("show.txt"), format!("{requested},{},{}", (rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2))?;
            }
            if let Some(values) = std::fs::read_to_string(directory.join("latest.txt")).ok().and_then(|s| s.strip_suffix('\n').and_then(fixture_values)).filter(|v| v.len() == 12) {
                anyhow::ensure!(values[1] == child_pid as i64, "Native sample PID differs from owned child");
                if values[3] == 1 && values[4] == 248 && values[10] > aligned && fixture_tick().saturating_sub(values[0] as u64) < 100 {
                    let mut target = RECT::default(); let mut parent = RECT::default();
                    unsafe { GetWindowRect(data.target, &mut target)?; GetWindowRect(hwnd, &mut parent)?; }
                    let dx = (values[5] + values[7]) as i32 / 2 - (target.left + target.right) / 2;
                    let dy = (values[6] + values[8]) as i32 / 2 - (target.top + target.bottom) / 2;
                    unsafe { SetWindowPos(hwnd, None, parent.left + dx, parent.top + dy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)?; }
                    aligned = values[10];
                    std::fs::write(directory.join("alignment.txt"), format!("generation={aligned}; tick={}; target center={},{}; settled child rect={},{},{},{}; this is placement telemetry, not click evidence\n", fixture_tick(), (values[5] + values[7]) / 2, (values[6] + values[8]) / 2, values[5], values[6], values[7], values[8]))?;
                }
            }
            if cleanup.child.as_mut().unwrap().try_wait()?.is_some() { break; }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(directory.join("stop.txt"), "owned parent finished")?;
        let stop_deadline = fixture_tick() + 2_000;
        let exit = loop {
            if let Some(exit) = cleanup.child.as_mut().unwrap().try_wait()? { break exit; }
            anyhow::ensure!(fixture_tick() < stop_deadline, "Owned child failed bounded natural stop");
            std::thread::sleep(Duration::from_millis(10));
        };
        anyhow::ensure!(exit.success() && directory.join("worker-stop.txt").exists(), "Worker failed; inspect worker-output.txt");
        let samples: Vec<_> = std::fs::read_to_string(directory.join("worker-native.csv"))?.lines().filter_map(fixture_values).filter(|v| v.len() == 12).collect();
        let bracket = |event: &Click, visible: bool| -> bool {
            // Exclude same-tick observations even on a coarse shared clock.
            // Entire API-read intervals must precede/follow the receipt, so
            // preemption between a native query and its timestamp cannot PASS.
            let before = samples.iter().rev().find(|s| s[0] < event.tick as i64);
            let after = samples.iter().find(|s| s[11] > event.tick as i64);
            let valid = |s: &Vec<i64>| {
                let flags = s[9] as u32;
                let required = WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;
                s[1] == child_pid as i64 && s[2] != 0 && s[3] == visible as i64 && (!visible || (s[4] > 0 &&
                    flags & required == required &&
                    (event.point.x as i64 - (s[5] + s[7]) / 2).abs() <= (s[7] - s[5]) / 4 &&
                    (event.point.y as i64 - (s[6] + s[8]) / 2).abs() <= (s[8] - s[6]) / 4))
            };
            matches!((before, after), (Some(a), Some(b)) if a[11] <= a[0] && b[11] <= b[0] && event.tick - a[11] as u64 <= 100 && b[0] as u64 - event.tick <= 100 && valid(a) && valid(b))
        };
        let qualifying = |index: usize, visible: bool| -> bool {
            if index < 2 { return false; }
            let event = &data.events[index]; let down = &data.events[index - 2]; let up = &data.events[index - 1];
            down.kind == "down" && up.kind == "up" && down.tick <= up.tick && up.tick <= event.tick && event.tick - down.tick <= 1_000 &&
                down.dispatch <= up.dispatch && up.dispatch <= event.dispatch && event.dispatch - up.dispatch <= 100 &&
                (event.point.x - up.point.x).abs() <= 8 && (event.point.y - up.point.y).abs() <= 8 &&
                bracket(event, visible) && bracket(down, visible) && bracket(up, visible)
        };
        let baseline = data.events.iter().enumerate().filter(|(index, e)| e.kind == "click" && qualifying(*index, false)).count();
        let overlay = data.events.iter().enumerate().filter(|(index, e)| e.kind == "click" && qualifying(*index, true)).count();
        let finish = data.events.iter().any(|e| e.kind == "finish" && samples.iter().any(|s| s[0] <= e.tick as i64 && e.tick - s[0] as u64 <= 100 &&
            (e.point.x as i64 <= s[5] || e.point.x as i64 >= s[7] || e.point.y as i64 <= s[6] || e.point.y as i64 >= s[8])));
        let result = if baseline > 0 && overlay > 0 && finish { "PASS" } else { "INCONCLUSIVE" };
        std::fs::write(directory.join("result.txt"), format!("{result}: baseline pointer clicks={baseline}; child-overlay pointer clicks={overlay}; Finish={finish}; parent PID={}; worker PID={child_pid}; child exited naturally and destroyed own HWND. Queued MSG.time/pt identify actual target DOWN/UP; synchronous BN_CLICKED inherits its immediately preceding UP's original time/point. Dispatch ticks are recorded separately. Native API-read intervals wholly bracket original OS event time on the shared tick clock, <=100ms on each side; same-tick observations cannot qualify. Overlay clicks must lie inside the central half of its actual native rectangle with alpha>0 and layered/transparent/topmost/noactivate/toolwindow styles. Synthetic notices are renewed <=20s per Show; this is not normal hold evidence. Official CU coordinate clicks must be established separately; rejected clicks are inconclusive. Physical mixed-monitor/DPI behavior remains untested.\n", std::process::id()))?;
        println!("{result}: {}", directory.join("result.txt").display());
        drop(cleanup);
        Ok(())
    }
}

#[cfg(test)]
mod environment_notification_tests {
    use super::*;
    use serde_json::{json, Value};
    use std::io::{Seek, SeekFrom, Write};

    fn snapshot(window: &Osd, stage: &str, started: Instant) -> Result<Value> {
        let mut rect = RECT::default(); let mut alpha = 0;
        let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
        unsafe {
            GetWindowRect(window.hwnd, &mut rect)?;
            GetLayeredWindowAttributes(window.hwnd, None, Some(&mut alpha), Some(&mut flags))?;
        }
        assert_eq!(flags.0 & LWA_ALPHA.0, LWA_ALPHA.0);
        let state = window._state.borrow();
        let geometry = state.geometry.map(|g| json!({"work": [g.work.left, g.work.top, g.work.right, g.work.bottom], "dpi": g.dpi, "size": [g.width, g.height]}));
        Ok(json!({"stage": stage, "elapsed_ms": started.elapsed().as_millis(), "hwnd": window.hwnd.0 as usize,
            "visible": window.is_visible(), "rect": [rect.left, rect.top, rect.right, rect.bottom], "alpha": alpha,
            "phase": format!("{:?}", state.motion.phase), "geometry": geometry, "reduced": state.motion.reduced,
            "exstyles": unsafe { GetWindowLongPtrW(window.hwnd, GWL_EXSTYLE) } as u32}))
    }

    // Filter every retrieval to our HWND, including the initial stale-message drain.
    // Real OS timers are counted; no timer callback, timestamp, or input is injected.
    fn pump(window: &Osd, duration: Duration, require_hidden: bool) -> Result<usize> {
        let until = Instant::now() + duration; let mut timers = 0;
        loop {
            let mut message = MSG::default();
            for _ in 0..128 {
                if !unsafe { PeekMessageW(&mut message, Some(window.hwnd), 0, 0, PM_REMOVE) }.as_bool() { break; }
                assert_eq!(message.hwnd, window.hwnd);
                if message.message == WM_TIMER && matches!(message.wParam.0, MOTION_TIMER | HOLD_TIMER) { timers += 1; }
                unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
            }
            if require_hidden {
                assert!(!window.is_visible(), "Hidden OSD reappeared while observing timer shutdown");
                let state = window._state.borrow();
                assert_eq!(state.motion.phase, Phase::Hidden);
                assert!(state.geometry.is_none());
            }
            if Instant::now() >= until { return Ok(timers); }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn show_settled(window: &Osd) -> Result<()> {
        window.show_notice(Notice { title: "Synthetic environment probe", detail: "Owned HWND only",
            rtl: false, icon: NoticeIcon::Volume, value: Some(50), anchor: Some((0, 0)) });
        let deadline = Instant::now() + Duration::from_secs(3);
        while window._state.borrow().motion.phase != Phase::Visible {
            anyhow::ensure!(Instant::now() < deadline, "Native OSD did not settle");
            pump(window, Duration::from_millis(5), false)?;
        }
        assert!(window.is_visible());
        let geometry = window._state.borrow().geometry.context("Recovered show has no geometry")?;
        let mut rect = RECT::default(); let mut alpha = 0;
        unsafe {
            GetWindowRect(window.hwnd, &mut rect)?;
            GetLayeredWindowAttributes(window.hwnd, None, Some(&mut alpha), None)?;
        }
        let (x, y) = geometry.position(1.0);
        assert_eq!((rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top), (x, y, geometry.width, geometry.height));
        assert_eq!(alpha, 248);
        let styles = unsafe { GetWindowLongPtrW(window.hwnd, GWL_EXSTYLE) } as u32;
        for flag in [WS_EX_NOACTIVATE, WS_EX_TRANSPARENT, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST] {
            assert_eq!(styles & flag.0, flag.0);
        }
        Ok(())
    }

    #[test]
    #[ignore = "owned native HWND synthetic environment messages; requires MAXI_OSD_ENV_EVIDENCE_DIR"]
    fn native_osd_environment_notifications() -> Result<()> {
        let raw = std::path::PathBuf::from(std::env::var_os("MAXI_OSD_ENV_EVIDENCE_DIR")
            .context("Set MAXI_OSD_ENV_EVIDENCE_DIR to an existing absolute workspace directory")?);
        anyhow::ensure!(raw.is_absolute(), "Evidence directory must be absolute");
        let directory = raw.canonicalize()?;
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
        anyhow::ensure!(directory.is_dir() && directory != workspace && directory.starts_with(&workspace), "Evidence directory must be strictly inside this workspace");
        // Refuse existing files and links; only this newly opened file handle is rewritten.
        let mut evidence = std::fs::OpenOptions::new().write(true).create_new(true).open(directory.join("osd-environment.json"))?;
        evidence.write_all(b"{\"status\":\"incomplete\",\"synthetic_notifications\":true}\n")?;
        evidence.flush()?;
        let started = Instant::now(); let mut samples = Vec::new(); let mut protocols = Vec::new();
        let preference = animations_enabled();
        let window = Osd::new()?; let hwnd = window.hwnd;
        assert_eq!(window._state.borrow().motion.reduced, !preference);
        for (name, message, parameter) in [("display-change", WM_DISPLAYCHANGE, 0),
            ("external-dpi-change", WM_DPICHANGED, 0), ("workarea-setting-change", WM_SETTINGCHANGE, SPI_SETWORKAREA.0 as usize)] {
            show_settled(&window)?;
            samples.push(snapshot(&window, &format!("{name}-before"), started)?);
            let geometry = window._state.borrow().geometry.unwrap();
            let suggested = RECT { left: geometry.work.left, top: geometry.work.top, right: geometry.work.left + geometry.width, bottom: geometry.work.top + geometry.height };
            let w = if message == WM_DPICHANGED { geometry.dpi as usize | ((geometry.dpi as usize) << 16) } else { parameter };
            let l = if message == WM_DPICHANGED { LPARAM((&suggested as *const RECT) as isize) } else { LPARAM(0) };
            unsafe { SendMessageW(hwnd, message, Some(WPARAM(w)), Some(l)); }
            assert!(!window.is_visible());
            assert_eq!(window._state.borrow().motion.phase, Phase::Hidden);
            assert!(window._state.borrow().geometry.is_none());
            samples.push(snapshot(&window, &format!("{name}-hidden"), started)?);
            // KillTimer does not remove already queued messages: permit a bounded drain,
            // then observe longer than nominal HOLD for any newly generated timer.
            let drained_timers = pump(&window, Duration::from_millis(30), true)?;
            let idle_started = Instant::now();
            let idle_timers = pump(&window, HOLD + Duration::from_millis(150), true)?;
            assert_eq!(idle_timers, 0, "{name} leaked an owned OSD timer after drain");
            protocols.push(json!({"notification": name, "message": message, "wparam": w,
                "drained_timers": drained_timers, "hidden_observation_ms": idle_started.elapsed().as_millis(), "new_timers": idle_timers}));
            show_settled(&window)?;
            samples.push(snapshot(&window, &format!("{name}-recovered"), started)?);
        }
        let before = snapshot(&window, "simulated-positioning-before", started)?;
        let dpi = window._state.borrow().geometry.unwrap().dpi as usize;
        let suggested = RECT { left: -100, top: -100, right: 100, bottom: 100 };
        // Simulate only the reentrancy gate. This is not real SetWindowPos/DPI traffic.
        window._state.borrow_mut().positioning = true;
        unsafe { SendMessageW(hwnd, WM_DPICHANGED, Some(WPARAM(dpi | (dpi << 16))), Some(LPARAM((&suggested as *const RECT) as isize))); }
        window._state.borrow_mut().positioning = false;
        let after = snapshot(&window, "simulated-positioning-after", started)?;
        for key in ["visible", "rect", "alpha", "phase", "geometry", "reduced", "exstyles"] { assert_eq!(before[key], after[key], "Positioning DPI gate changed {key}"); }
        samples.extend([before, after]);
        assert!(animations_enabled() == preference, "Animation preference changed externally; rerun");
        // Seed a stale cache only in owned test state; never change the OS preference.
        window._state.borrow_mut().motion.reduced = preference;
        samples.push(snapshot(&window, "simulated-stale-animation-cache", started)?);
        unsafe { SendMessageW(hwnd, WM_SETTINGCHANGE, Some(WPARAM(0)), Some(LPARAM(0))); }
        assert!(animations_enabled() == preference, "Animation preference changed externally; rerun");
        assert_eq!(window._state.borrow().motion.reduced, !preference);
        assert!(window.is_visible());
        samples.push(snapshot(&window, "unrelated-setting-reread", started)?);
        show_settled(&window)?;
        samples.push(snapshot(&window, "final-recovered", started)?);
        drop(window);
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool(), "Owned OSD HWND survived Drop");
        let report = json!({"status": "pass", "synthetic_notifications": true, "synthetic_positioning_gate": true,
            "animation_preference_before_and_after": preference, "synthetic_stale_animation_cache": true, "owned_hwnd_destroyed": true,
            "scope": "One native Osd HWND; real HWND rectangles, alpha, visibility and queued timer delivery; settings only read",
            "limitations": ["Not physical display or mixed-DPI change evidence", "Not a real reduced-motion preference toggle", "Not real SetWindowPos reentrant DPI evidence", "Not pointer click-through or focus-transition evidence"],
            "protocols": protocols, "samples": samples});
        evidence.seek(SeekFrom::Start(0))?; evidence.set_len(0)?;
        serde_json::to_writer_pretty(&mut evidence, &report)?; evidence.write_all(b"\n")?; evidence.flush()?;
        Ok(())
    }
}
