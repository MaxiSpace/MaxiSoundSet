//! Non-activating, click-through Win32 toast for successful background actions.
use anyhow::{Context, Result};
use std::{ffi::c_void, sync::{Mutex, OnceLock}};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CreateFontW, CreatePen, CreateRoundRectRgn, CreateSolidBrush,
            DeleteObject, DrawTextW, EndPaint, GetMonitorInfoW, MonitorFromWindow, RoundRect,
            SelectObject, SetBkMode, SetTextColor, SetWindowRgn, BACKGROUND_MODE, DEFAULT_CHARSET,
            DEFAULT_PITCH, DT_CENTER, DT_END_ELLIPSIS, DT_RTLREADING, DT_SINGLELINE, DT_VCENTER, FW_SEMIBOLD,
            MONITOR_DEFAULTTONEAREST, MONITORINFO, PS_SOLID,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow,
            IsWindowVisible, KillTimer, RegisterClassW, SetLayeredWindowAttributes, SetTimer,
            SetWindowPos, ShowWindow, UnregisterClassW, WNDCLASSW, CS_HREDRAW, CS_VREDRAW,
            HWND_TOPMOST, HTTRANSPARENT, LWA_ALPHA, MA_NOACTIVATE, SW_HIDE, SWP_NOACTIVATE,
            SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST,
            WM_PAINT, WM_TIMER, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        },
    },
};

const WIDTH: i32 = 390;
const HEIGHT: i32 = 74;
const HIDE_AFTER_MS: u32 = 2200;
const TIMER_ID: usize = 1;
static TEXT: Mutex<(String, String, bool)> = Mutex::new((String::new(), String::new(), false));
static PRIVATE_FONTS: OnceLock<Vec<usize>> = OnceLock::new();

#[link(name = "gdi32")]
unsafe extern "system" {
    fn AddFontMemResourceEx(data: *const c_void, size: u32, reserved: *mut c_void, count: *mut u32) -> *mut c_void;
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
    log::debug!("Registered both embedded Vazir faces for GDI OSD text");
    Ok(())
}

/// The Slint/Winit UI thread owns and dispatches this HWND. Keeping window
/// creation, show/hide, timer, and painting on that thread avoids a second
/// Win32 message pump and cross-thread window callbacks.
pub struct Osd {
    hwnd: HWND,
    module: HINSTANCE,
    class_name: Vec<u16>,
}

impl Osd {
    pub fn new() -> Result<Self> {
        ensure_vazir_registered()?;
        let class_name = wide("MaxiSoundSet.BackgroundNotice");
        let module: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(procedure),
            hInstance: module,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        anyhow::ensure!(unsafe { RegisterClassW(&class) } != 0, "Could not register the background notice window class");

        let title = wide("MAXI SOUNDSET");
        let hwnd = match unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                PCWSTR(class_name.as_ptr()), PCWSTR(title.as_ptr()), WS_POPUP,
                0, 0, WIDTH, HEIGHT, None, None, Some(module), None,
            )
        }.context("Could not create the background notice window") {
            Ok(hwnd) => hwnd,
            Err(error) => {
                unsafe { let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(module)); }
                return Err(error);
            }
        };

        if let Err(error) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 236, LWA_ALPHA) } {
            unsafe { let _ = DestroyWindow(hwnd); let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(module)); }
            return Err(error.into());
        }
        let region = unsafe { CreateRoundRectRgn(0, 0, WIDTH + 1, HEIGHT + 1, 40, 40) };
        if !region.is_invalid() {
            let assigned = unsafe { SetWindowRgn(hwnd, Some(region), true) };
            if assigned == 0 { unsafe { let _ = DeleteObject(region.into()); } }
        }
        log::debug!("Background OSD HWND created on UI thread: {:#x}", hwnd.0 as usize);
        Ok(Self { hwnd, module, class_name })
    }

    pub fn show(&self, title: &str, detail: &str, rtl: bool) {
        if let Ok(mut text) = TEXT.lock() { *text = (title.into(), detail.into(), rtl); }
        let foreground = unsafe { GetForegroundWindow() };
        let monitor = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (left, top, right) = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            (info.rcWork.left, info.rcWork.top, info.rcWork.right)
        } else { (0, 0, WIDTH) };
        let positioned = unsafe {
            SetWindowPos(
                self.hwnd, Some(HWND_TOPMOST), left + ((right - left - WIDTH) / 2), top + 26,
                WIDTH, HEIGHT, SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        }.is_ok();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, true);
            let _ = KillTimer(Some(self.hwnd), TIMER_ID);
            let timer = SetTimer(Some(self.hwnd), TIMER_ID, HIDE_AFTER_MS, None);
            let visible = IsWindowVisible(self.hwnd).as_bool();
            if !positioned || timer == 0 || !visible {
                log::error!("OSD show failed: position_ok={positioned}, timer_id={timer}, visible={visible}");
            } else {
                log::debug!("OSD visible; hide delay={HIDE_AFTER_MS}ms, timer_id={timer}");
            }
        }
    }

    pub fn is_visible(&self) -> bool { unsafe { IsWindowVisible(self.hwnd).as_bool() } }

    pub fn query_visibility(&self, _timeout: std::time::Duration) -> Option<bool> {
        Some(self.is_visible())
    }

    pub fn hide(&self) {
        unsafe { let _ = KillTimer(Some(self.hwnd), TIMER_ID); let _ = ShowWindow(self.hwnd, SW_HIDE); }
    }
}

impl Drop for Osd {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ID);
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(PCWSTR(self.class_name.as_ptr()), Some(self.module));
        }
    }
}

unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER if w.0 == TIMER_ID => {
            unsafe { let _ = KillTimer(Some(hwnd), TIMER_ID); let _ = ShowWindow(hwnd, SW_HIDE); }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => { unsafe { paint(hwnd); } LRESULT(0) }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    // COLORREF is 0x00BBGGRR. Keep a deep navy glass-like fill under the
    // existing layered alpha, with a bright cool border like the reference.
    let brush = unsafe { CreateSolidBrush(COLORREF(0x00301b12)) };
    let border = unsafe { CreatePen(PS_SOLID, 1, COLORREF(0x00e5a758)) };
    let old_brush = unsafe { SelectObject(dc, brush.into()) };
    let old_pen = unsafe { SelectObject(dc, border.into()) };
    unsafe { let _ = RoundRect(dc, 1, 1, WIDTH - 1, HEIGHT - 1, 26, 26); }
    unsafe {
        let _ = SelectObject(dc, old_brush);
        let _ = SelectObject(dc, old_pen);
        let _ = DeleteObject(brush.into());
        let _ = DeleteObject(border.into());
        let _ = SetBkMode(dc, BACKGROUND_MODE(1));
    }
    let (title, detail, rtl) = TEXT.lock().map(|text| text.clone()).unwrap_or_default();
    let face = wide("Vazir");
    let title_font = unsafe { CreateFontW(-16, 0, 0, 0, FW_SEMIBOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET, Default::default(), Default::default(), Default::default(), DEFAULT_PITCH.0 as u32, PCWSTR(face.as_ptr())) };
    let detail_font = unsafe { CreateFontW(-13, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, Default::default(), Default::default(), Default::default(), DEFAULT_PITCH.0 as u32, PCWSTR(face.as_ptr())) };
    let old_font = unsafe { SelectObject(dc, title_font.into()) };
    unsafe { let _ = SetTextColor(dc, COLORREF(0x00ffffff)); }
    let mut title_wide: Vec<u16> = title.encode_utf16().collect();
    let flags = (if rtl { DT_RTLREADING | DT_CENTER } else { DT_CENTER }) | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
    let (content_left, content_right) = if rtl { (18, WIDTH - 60) } else { (60, WIDTH - 18) };
    let title_rect = if detail.is_empty() {
        RECT { left: content_left, top: 6, right: content_right, bottom: HEIGHT - 6 }
    } else {
        RECT { left: content_left, top: 6, right: content_right, bottom: 34 }
    };
    let mut title_rect = title_rect;
    if !title_wide.is_empty() { unsafe { let _ = DrawTextW(dc, &mut title_wide, &mut title_rect, flags); } }
    let _ = unsafe { SelectObject(dc, detail_font.into()) };
    unsafe { let _ = SetTextColor(dc, COLORREF(0x00f4f7fb)); }
    let mut detail_wide: Vec<u16> = detail.encode_utf16().collect();
    let mut detail_rect = RECT { left: content_left, top: 35, right: content_right, bottom: HEIGHT - 6 };
    if !detail_wide.is_empty() { unsafe { let _ = DrawTextW(dc, &mut detail_wide, &mut detail_rect, flags); } }
    // A small speaker mark anchors the notification with a relevant icon.
    let icon_pen = unsafe { CreatePen(PS_SOLID, 2, COLORREF(0x00f4b76e)) };
    let previous_pen = unsafe { SelectObject(dc, icon_pen.into()) };
    let icon_x = if rtl { WIDTH - 48 } else { 25 };
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::MoveToEx(dc, icon_x, 30, None);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, icon_x + 8, 30);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, icon_x + 19, 21);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, icon_x + 19, 53);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, icon_x + 8, 44);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, icon_x, 44);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, icon_x, 30);
        let _ = SelectObject(dc, previous_pen);
        let _ = DeleteObject(icon_pen.into());
    }
    unsafe {
        let _ = SelectObject(dc, old_font);
        let _ = DeleteObject(title_font.into());
        let _ = DeleteObject(detail_font.into());
        let _ = EndPaint(hwnd, &paint);
    }
}

fn wide(value: &str) -> Vec<u16> { value.encode_utf16().chain(Some(0)).collect() }
