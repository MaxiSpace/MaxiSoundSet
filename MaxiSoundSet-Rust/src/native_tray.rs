//! The tray uses the existing Slint/Winit HWND, avoiding a second hidden window.
use crate::core_audio::wide;
use anyhow::{Context, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
    mpsc::{self, Receiver, Sender},
    Mutex, OnceLock,
};
use std::collections::HashMap;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{BeginPaint, CreateFontW, CreatePen, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, EndPaint, HDC, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MoveToEx, Rectangle, RoundRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, BACKGROUND_MODE, DEFAULT_CHARSET, DEFAULT_PITCH, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_RTLREADING, DT_SINGLELINE, DT_VCENTER, FW_SEMIBOLD, PS_SOLID},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NIN_SELECT, NOTIFYICONDATAW,
            },
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};
const MESSAGE: u32 = WM_APP + 741;
const APP_SUBCLASS_ID: usize = 0x4D415849;
#[link(name = "user32")]
unsafe extern "system" {
    fn GetDpiForWindow(hwnd: *mut std::ffi::c_void) -> u32;
    fn UpdateWindow(hwnd: *mut std::ffi::c_void) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ENGLISH: AtomicBool = AtomicBool::new(false);
static WINDOWS_MODE: AtomicBool = AtomicBool::new(true);
static PAUSED: AtomicBool = AtomicBool::new(false);
static TARGET: AtomicU32 = AtomicU32::new(50);
static MENU_VISIBLE: AtomicBool = AtomicBool::new(false);
static MENU_HOVER: AtomicIsize = AtomicIsize::new(-1);
static MENU_DPI: AtomicU32 = AtomicU32::new(96);
static MENU_KEYS_READY: AtomicBool = AtomicBool::new(false);
static PRESENT_PENDING: AtomicBool = AtomicBool::new(true);
static TX: Mutex<Option<Sender<Action>>> = Mutex::new(None);
static EXIT_MESSAGE: AtomicU32 = AtomicU32::new(0);
static HOTKEY_IDS: OnceLock<Mutex<HashMap<i32, usize>>> = OnceLock::new();
static CAPTURE_INDEX: AtomicIsize = AtomicIsize::new(-1);
const MOD_NOREPEAT_VALUE: u32 = 0x4000;
#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterHotKey(hwnd: *mut std::ffi::c_void, id: i32, modifiers: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hwnd: *mut std::ffi::c_void, id: i32) -> i32;
    fn GetKeyState(key: i32) -> i16;
}
#[derive(Debug)]
pub enum Action {
    Show,
    Pause,
    Target(i32),
    Exit,
    Refresh,
    Repaint,
    Hotkey(usize),
    CapturedHotkey(usize, u32, u32),
    CaptureCancelled(usize),
}
pub struct Tray {
    hwnd: HWND,
    capture_hwnd: HWND,
    icon: HICON,
    pub events: Option<Receiver<Action>>,
    registered: bool,
    tooltip: String,
    hotkey_ids: Vec<i32>,
    pub hotkey_errors: Vec<(usize, String)>,
}
impl Tray {
    pub fn new() -> Result<Self> {
        crate::osd::ensure_vazir_registered().context("Could not register embedded Vazir fonts for the tray popup")?;
        EXIT_MESSAGE.store(
            unsafe {
                RegisterWindowMessageW(PCWSTR(wide("MAXISOUNDSET.RequestSafeExit.v1").as_ptr()))
            },
            Ordering::Relaxed,
        );
        // Keep shell callbacks and WM_HOTKEY off Winit's HWND. Subclassing Winit's
        // procedure made two independent window lifetimes share one WndProc chain.
        let class_name = wide("MaxiSoundSet.NativeTrayWindow");
        let instance = unsafe { GetModuleHandleW(None)? };
        let capture_hwnd = own_window().context("Slint application window is unavailable")?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        anyhow::ensure!(atom != 0, "Native tray window class could not be registered");
        let title = wide("MAXI SOUNDSET native notifications");
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0, 0, 0, 0,
                None, None, Some(instance.into()), None,
            )
        }.context("Native tray window could not be created")?;
        unsafe { let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 244, LWA_ALPHA); }
        if !unsafe { SetWindowSubclass(capture_hwnd, Some(app_window_subclass), APP_SUBCLASS_ID, 0) }.as_bool() {
            unsafe {
                let _ = DestroyWindow(hwnd);
                let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(instance.into()));
            }
            anyhow::bail!("Safe Slint window notification subclass could not be installed");
        }
        let icon = unsafe {
            LoadIconW(
                Some(GetModuleHandleW(None)?.into()),
                PCWSTR(1usize as *const u16),
            )?
        };
        let (tx, events) = mpsc::channel();
        *TX.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        let mut tray = Self {
            hwnd,
            capture_hwnd,
            icon,
            events: Some(events),
            registered: false,
            tooltip: "MAXI SOUNDSET · Standby".into(),
            hotkey_ids: Vec::new(),
            hotkey_errors: Vec::new(),
        };
        match tray.register() {
            Ok(()) => log::info!("Native Windows system tray icon registered"),
            Err(e) => log::warn!("Native tray is unavailable in this Windows session: {e:#}. Reopen the EXE to show Maxi."),
        }
        Ok(tray)
    }
    pub fn is_registered(&self) -> bool {
        self.registered
    }
    pub fn probe_callback_messages(&self, probe_hotkey: bool) -> (bool, bool) {
        let Some(events) = self.events.as_ref() else { return (false, false); };
        unsafe { let _ = SendMessageW(self.hwnd, MESSAGE, Some(WPARAM(0)), Some(LPARAM(WM_LBUTTONUP as isize))); }
        let shell_callback = matches!(events.try_recv(), Ok(Action::Show));
        let menu_ok = self.probe_custom_menu();
        if !menu_ok { log::error!("Custom tray popup interaction probe failed"); }
        if !probe_hotkey { return (shell_callback && menu_ok, true); }
        let Some(id) = self.hotkey_ids.first().copied() else { return (shell_callback, false); };
        let expected = HOTKEY_IDS.get().and_then(|map| map.lock().ok()?.get(&id).copied());
        unsafe { let _ = SendMessageW(self.hwnd, WM_HOTKEY, Some(WPARAM(id as usize)), Some(LPARAM(0))); }
        let hotkey_callback = matches!((expected, events.try_recv()), (Some(index), Ok(Action::Hotkey(actual))) if index == actual);
        (shell_callback && menu_ok, hotkey_callback)
    }
    /// Exercise the custom popup through its actual HWND procedure. This is
    /// used only by the isolated startup diagnostic modes.
    fn probe_custom_menu(&self) -> bool {
        let Some(events) = self.events.as_ref() else { return false; };
        while events.try_recv().is_ok() {}
        ACTIVE.store(true, Ordering::Relaxed);
        PAUSED.store(false, Ordering::Relaxed);
        WINDOWS_MODE.store(true, Ordering::Relaxed);
        ENGLISH.store(false, Ordering::Relaxed);
        TARGET.store(50, Ordering::Relaxed);
        unsafe {
            let foreground_before = GetForegroundWindow();
            let _ = SendMessageW(self.hwnd, MESSAGE, Some(WPARAM(0)), Some(LPARAM(WM_RBUTTONUP as isize)));
            let opened = MENU_VISIBLE.load(Ordering::Relaxed) && IsWindowVisible(self.hwnd).as_bool();
            let stayed_nonactivating = GetForegroundWindow() == foreground_before;
            let _ = UpdateWindow(self.hwnd.0);
            let _ = SendMessageW(self.hwnd, WM_HOTKEY, Some(WPARAM(MENU_KEYS[6].0 as usize)), Some(LPARAM(0)));
            let escaped = !MENU_VISIBLE.load(Ordering::Relaxed) && !IsWindowVisible(self.hwnd).as_bool();
            let _ = SendMessageW(self.hwnd, MESSAGE, Some(WPARAM(0)), Some(LPARAM(WM_RBUTTONUP as isize)));
            let dpi = MENU_DPI.load(Ordering::Relaxed).max(96) as i32;
            let x = MENU_WIDTH * dpi / 192;
            let y = (MENU_HEADER + MENU_ROW * 3 + MENU_GAP + MENU_ROW / 2) * dpi / 96;
            let packed = (((y as u32) << 16) | (x as u16 as u32)) as isize;
            let _ = SendMessageW(self.hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(LPARAM(packed)));
            let target = matches!(events.try_recv(), Ok(Action::Target(50)));
            opened && stayed_nonactivating && escaped && target && MENU_KEYS_READY.load(Ordering::Relaxed)
        }
    }
    /// Register shortcuts on the dedicated native tray HWND. It remains alive
    /// while the Slint window is hidden and keeps WM_HOTKEY out of Winit's WndProc.
    pub fn register_hotkeys(&mut self, bindings: &[crate::settings::HotkeyBinding]) -> Vec<(usize, String)> {
        CAPTURE_INDEX.store(-1, Ordering::SeqCst);
        self.unregister_hotkeys();
        let mut failures = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let map = HOTKEY_IDS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registered = map.lock().unwrap_or_else(|p| p.into_inner());
        for (index, binding) in bindings.iter().enumerate() {
            if !binding.enabled { continue; }
            let chord = (binding.modifiers, binding.virtual_key);
            if !seen.insert(chord) {
                failures.push((index, "Duplicate shortcut in MAXI SOUNDSET settings".to_string()));
                continue;
            }
            let id = 0x5100 + index as i32;
            let ok = unsafe { RegisterHotKey(self.hwnd.0, id, binding.modifiers | MOD_NOREPEAT_VALUE, binding.virtual_key) };
            if ok != 0 { registered.insert(id, index); self.hotkey_ids.push(id); }
            else {
                let code = unsafe { windows::Win32::Foundation::GetLastError().0 };
                failures.push((index, format!("Windows could not register shortcut (error {code})")));
            }
        }
        self.hotkey_errors = failures;
        log::info!(
            "Global shortcuts registered: {} active, {} failed",
            self.hotkey_ids.len(),
            self.hotkey_errors.len()
        );
        self.hotkey_errors.clone()
    }
    pub fn begin_hotkey_capture(&mut self, index: usize) {
        self.unregister_hotkeys();
        CAPTURE_INDEX.store(index as isize, Ordering::SeqCst);
    }
    fn unregister_hotkeys(&mut self) {
        let map = HOTKEY_IDS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registered = map.lock().unwrap_or_else(|p| p.into_inner());
        for id in self.hotkey_ids.drain(..) {
            unsafe { let _ = UnregisterHotKey(self.hwnd.0, id); }
            registered.remove(&id);
        }
    }
    fn data(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 741,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: MESSAGE,
            hIcon: self.icon,
            ..Default::default()
        };
        for (to, from) in data
            .szTip
            .iter_mut()
            .take(127)
            .zip(self.tooltip.encode_utf16())
        {
            *to = from;
        }
        data
    }
    pub fn register(&mut self) -> Result<()> {
        self.registered = unsafe { Shell_NotifyIconW(NIM_ADD, &self.data()) }.as_bool();
        anyhow::ensure!(
            self.registered,
            "Windows notification area rejected the tray icon"
        );
        Ok(())
    }
    pub fn update(&mut self, active: bool, paused: bool, target: i32, english: bool, windows_mode: bool) {
        ENGLISH.store(english, Ordering::Relaxed);
        WINDOWS_MODE.store(windows_mode, Ordering::Relaxed);
        ACTIVE.store(active, Ordering::Relaxed);
        PAUSED.store(paused, Ordering::Relaxed);
        TARGET.store(target.max(0) as u32, Ordering::Relaxed);
        let tooltip = format!(
            "MAXI SOUNDSET · {} · Target {target}",
            if active {
                if paused {
                    "Paused"
                } else {
                    "Active"
                }
            } else {
                "Standby"
            }
        );
        if tooltip != self.tooltip {
            self.tooltip = tooltip;
            if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.data()) }.as_bool() {
                let _ = self.register();
            }
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        self.unregister_hotkeys();
        unsafe {
            // Winit may already have destroyed its HWND when the Slint event
            // loop returns; never pass a stale handle into comctl32 cleanup.
            if IsWindow(Some(self.capture_hwnd)).as_bool() {
                let _ = RemoveWindowSubclass(self.capture_hwnd, Some(app_window_subclass), APP_SUBCLASS_ID);
            }
            if self.registered {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
            let _ = DestroyWindow(self.hwnd);
            let class_name = wide("MaxiSoundSet.NativeTrayWindow");
            if let Ok(instance) = GetModuleHandleW(None) {
                let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(instance.into()));
            }
        }
        *TX.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}
pub fn post_own_close() -> Result<()> {
    let hwnd = own_window().context("Own Slint window unavailable")?;
    unsafe {
        PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))?;
    }
    Ok(())
}
pub fn send(action: Action) {
    if let Some(tx) = TX.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send(action);
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message != 0 && message == EXIT_MESSAGE.load(Ordering::Relaxed) {
        // Use the same shutdown path as Exit app, including audio restoration.
        send(Action::Exit);
        return LRESULT(0);
    }
    if message == MESSAGE {
        match l.0 as u32 & 0xffff {
            WM_LBUTTONUP | NIN_SELECT => send(Action::Show),
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe { menu(hwnd); },
            _ => (),
        }
        return LRESULT(0);
    }
    if MENU_VISIBLE.load(Ordering::Relaxed) {
        match message {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => { unsafe { paint_menu(hwnd); } return LRESULT(0); }
            WM_ERASEBKGND => return LRESULT(1),
            WM_HOTKEY => {
                if let Some((_, key)) = MENU_KEYS.iter().find(|(id, _)| *id == w.0 as i32) { handle_menu_key(hwnd, *key); }
                return LRESULT(0);
            }
            WM_TIMER if w.0 == MENU_TIMER_ID => {
                let mut cursor = POINT::default();
                let mut rect = RECT::default();
                unsafe { let _ = GetCursorPos(&mut cursor); let _ = GetWindowRect(hwnd, &mut rect); }
                let inside = cursor.x >= rect.left && cursor.x < rect.right && cursor.y >= rect.top && cursor.y < rect.bottom;
                let pressed = unsafe { GetAsyncKeyState(0x01) < 0 || GetAsyncKeyState(0x02) < 0 };
                if pressed && !inside { unsafe { dismiss_menu(hwnd); } }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let y = client_y(l).max(0);
                let next = menu_row_at(y);
                if MENU_HOVER.swap(next as isize, Ordering::Relaxed) != next as isize {
                    unsafe { let _ = InvalidateRect(Some(hwnd), None, true); }
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let y = client_y(l).max(0);
                let row = menu_row_at(y);
                unsafe { dismiss_menu(hwnd); }
                dispatch_menu_action(row);
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                handle_menu_key(hwnd, w.0 as u32);
                return LRESULT(0);
            }
            _ => (),
        }
    }
    if message == WM_HOTKEY {
        if let Some(index) = HOTKEY_IDS.get().and_then(|ids| ids.lock().ok()?.get(&(w.0 as i32)).copied()) {
            log::debug!("WM_HOTKEY id={} action={index}", w.0);
            send(Action::Hotkey(index));
        }
        return LRESULT(0);
    }
    // Explorer broadcasts TaskbarCreated to top-level windows after restarting.
    let taskbar = unsafe { RegisterWindowMessageW(PCWSTR(wide("TaskbarCreated").as_ptr())) };
    if taskbar != 0 && message == taskbar {
        send(Action::Refresh);
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

unsafe extern "system" fn app_window_subclass(
    hwnd: HWND, message: u32, w: WPARAM, l: LPARAM, _id: usize, _data: usize,
) -> LRESULT {
    let capture = CAPTURE_INDEX.load(Ordering::SeqCst);
    if capture >= 0 && (message == WM_KILLFOCUS || (message == WM_SHOWWINDOW && w.0 == 0)) {
        CAPTURE_INDEX.store(-1, Ordering::SeqCst);
        send(Action::CaptureCancelled(capture as usize));
    }
    if capture >= 0 && (matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) || message == WM_APPCOMMAND) {
        let key = if message == WM_APPCOMMAND {
            match ((l.0 as usize >> 16) & 0x0fff) as u32 { 8 => 0xAD, 9 => 0xAE, 10 => 0xAF, _ => 0 }
        } else { w.0 as u32 };
        let call_previous = || unsafe { DefSubclassProc(hwnd, message, w, l) };
        if key == 0 { return call_previous(); }
        if matches!(key, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5) {
            return call_previous();
        }
        if key == 0x1B {
            CAPTURE_INDEX.store(-1, Ordering::SeqCst);
            send(Action::CaptureCancelled(capture as usize));
        } else {
            let mut modifiers = 0u32;
            if unsafe { GetKeyState(0x11) } < 0 { modifiers |= 0x0002; }
            if unsafe { GetKeyState(0x12) } < 0 { modifiers |= 0x0001; }
            if unsafe { GetKeyState(0x10) } < 0 { modifiers |= 0x0004; }
            if unsafe { GetKeyState(0x5B) } < 0 || unsafe { GetKeyState(0x5C) } < 0 { modifiers |= 0x0008; }
            let reserved = modifiers & 0x0008 != 0
                || (modifiers & 0x0001 != 0 && matches!(key, 0x09 | 0x73 | 0x1B | 0x20))
                || (modifiers & 0x0002 != 0 && key == 0x1B)
                || (modifiers & 0x0006 == 0x0006 && key == 0x1B)
                || (modifiers & 0x0003 == 0x0003 && key == 0x2E)
                ;
            if reserved {
                CAPTURE_INDEX.store(-1, Ordering::SeqCst);
                send(Action::CaptureCancelled(capture as usize));
                return call_previous();
            }
            CAPTURE_INDEX.store(-1, Ordering::SeqCst);
            send(Action::CapturedHotkey(capture as usize, key, modifiers));
        }
        return LRESULT(0);
    }
    if message == WM_DPICHANGED
        || (message == WM_SHOWWINDOW && w.0 != 0)
        || (message == WM_SIZE && w.0 != SIZE_MINIMIZED as usize)
    {
        PRESENT_PENDING.store(true, Ordering::Relaxed);
        send(Action::Repaint);
    }
    // Follow the actual native paint as well as timed startup retries, once
    // per show/restore/DPI transition. Repaint itself cannot rearm this flag.
    if message == WM_PAINT && PRESENT_PENDING.swap(false, Ordering::Relaxed) {
        send(Action::Repaint);
    }
    unsafe { DefSubclassProc(hwnd, message, w, l) }
}
const MENU_WIDTH: i32 = 252;
const MENU_HEADER: i32 = 48;
const MENU_ROW: i32 = 34;
const MENU_GAP: i32 = 7;
const MENU_HEIGHT: i32 = MENU_HEADER + MENU_ROW * 6 + MENU_GAP * 2 + 2;
const MENU_TIMER_ID: usize = 0x5200;
const MENU_KEYS: [(i32, u32); 7] = [
    (0x5201, 0x26), (0x5202, 0x28), (0x5203, 0x24), (0x5204, 0x23),
    (0x5205, 0x0D), (0x5206, 0x20), (0x5207, 0x1B),
];

unsafe fn menu(hwnd: HWND) {
    if MENU_VISIBLE.swap(true, Ordering::Relaxed) { return; }
    let mut point = POINT::default();
    let _ = unsafe { GetCursorPos(&mut point) };
    let monitor = unsafe { windows::Win32::Graphics::Gdi::MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let work = if unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info) }.as_bool() { info.rcWork } else { RECT { left: 0, top: 0, right: 1920, bottom: 1080 } };
    let dpi = unsafe { GetDpiForWindow(hwnd.0) }.max(96) as i32;
    MENU_DPI.store(dpi as u32, Ordering::Relaxed);
    let width = MENU_WIDTH * dpi / 96;
    let height = MENU_HEIGHT * dpi / 96;
    let x = (point.x - width + 16).clamp(work.left, (work.right - width).max(work.left));
    // Tray icons sit on the bottom edge in the common layout; flip above when
    // there is not enough room below on a side/top taskbar.
    let y = (if point.y + height <= work.bottom { point.y } else { point.y - height })
        .clamp(work.top, (work.bottom - height).max(work.top));
    MENU_HOVER.store(-1, Ordering::Relaxed);
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE);
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, 14 * dpi / 96, 14 * dpi / 96);
        if !region.is_invalid() {
            let _ = SetWindowRgn(hwnd, Some(region), true);
        }
        // Avoid activating this popup: doing so takes foreground away from
        // Explorer and can collapse the hidden-icons flyout that launched it.
        let _ = SetTimer(Some(hwnd), MENU_TIMER_ID, 35, None);
        let mut all_keys_registered = true;
        for (id, key) in MENU_KEYS { if RegisterHotKey(hwnd.0, id, MOD_NOREPEAT_VALUE, key) == 0 { all_keys_registered = false; log::debug!("Could not register temporary tray menu key {key:#x}"); } }
        MENU_KEYS_READY.store(all_keys_registered, Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

unsafe fn dismiss_menu(hwnd: HWND) {
    if MENU_VISIBLE.swap(false, Ordering::Relaxed) {
        unsafe {
            let _ = KillTimer(Some(hwnd), MENU_TIMER_ID);
            for (id, _) in MENU_KEYS { let _ = UnregisterHotKey(hwnd.0, id); }
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn handle_menu_key(hwnd: HWND, key: u32) {
    let current = MENU_HOVER.load(Ordering::Relaxed);
    match key {
        0x1B => unsafe { dismiss_menu(hwnd); },
        0x26 | 0x28 | 0x24 | 0x23 => {
            let next = match key { 0x26 => menu_step(current, false), 0x28 => menu_step(current, true), 0x24 => 0, _ => 5 };
            MENU_HOVER.store(next, Ordering::Relaxed);
            unsafe { let _ = InvalidateRect(Some(hwnd), None, true); }
        }
        0x0D | 0x20 => { unsafe { dismiss_menu(hwnd); } dispatch_menu_action(current); }
        _ => (),
    }
}

fn menu_row_at(y: i32) -> isize {
    let dpi = MENU_DPI.load(Ordering::Relaxed).max(96) as i32;
    let y = y * 96 / dpi - MENU_HEADER;
    if y < 0 { return -1; }
    if y < MENU_ROW * 2 { return (y / MENU_ROW) as isize; }
    if y < MENU_ROW * 2 + MENU_GAP { return -1; }
    let after_first_gap = y - MENU_GAP;
    if after_first_gap >= MENU_ROW * 5 && after_first_gap < MENU_ROW * 5 + MENU_GAP { return -1; }
    let row = if after_first_gap >= MENU_ROW * 5 + MENU_GAP { (after_first_gap - MENU_GAP) / MENU_ROW } else { after_first_gap / MENU_ROW };
    if (0..6).contains(&row) { row as isize } else { -1 }
}

fn client_y(point: LPARAM) -> i32 { ((point.0 >> 16) as i16) as i32 }

fn menu_step(current: isize, forward: bool) -> isize {
    let direction = if forward { 1 } else { -1 };
    let mut next = if current < 0 { if forward { 0 } else { 5 } } else { (current + direction + 6) % 6 };
    if next == 1 && !ACTIVE.load(Ordering::Relaxed) { next = (next + direction + 6) % 6; }
    next
}

fn dispatch_menu_action(row: isize) {
    match row {
        0 => send(Action::Show),
        1 if ACTIVE.load(Ordering::Relaxed) => send(Action::Pause),
        2 => send(Action::Target(if WINDOWS_MODE.load(Ordering::Relaxed) { 20 } else { 65 })),
        3 => send(Action::Target(if WINDOWS_MODE.load(Ordering::Relaxed) { 50 } else { 100 })),
        4 => send(Action::Target(if WINDOWS_MODE.load(Ordering::Relaxed) { 100 } else { 140 })),
        5 => send(Action::Exit),
        _ => (),
    }
}

unsafe fn paint_menu(hwnd: HWND) {
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut ps) };
    let mut client = RECT::default();
    unsafe { let _ = GetClientRect(hwnd, &mut client); }
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    let dpi = unsafe { GetDpiForWindow(hwnd.0) }.max(96) as i32;
    let scale = |v: i32| v * dpi / 96;
    let bg = CreateSolidBrush(COLORREF(0x00301b12));
    let edge = CreatePen(PS_SOLID, scale(1), COLORREF(0x00e5a758));
    let old_brush = unsafe { SelectObject(dc, bg.into()) };
    let old_pen = unsafe { SelectObject(dc, edge.into()) };
    unsafe { let _ = RoundRect(dc, 1, 1, width - 1, height - 1, scale(14), scale(14)); }
    unsafe {
        let _ = SelectObject(dc, old_brush);
        let _ = SelectObject(dc, old_pen);
        let _ = DeleteObject(bg.into());
        let _ = DeleteObject(edge.into());
        let _ = SetBkMode(dc, BACKGROUND_MODE(1));
    }
    let en = ENGLISH.load(Ordering::Relaxed);
    let bold = wide("Vazir");
    let title_font = unsafe { CreateFontW(-scale(14), 0, 0, 0, FW_SEMIBOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET, Default::default(), Default::default(), Default::default(), DEFAULT_PITCH.0 as u32, PCWSTR(bold.as_ptr())) };
    let normal_font = unsafe { CreateFontW(-scale(11), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, Default::default(), Default::default(), Default::default(), DEFAULT_PITCH.0 as u32, PCWSTR(bold.as_ptr())) };
    let prior = unsafe { SelectObject(dc, title_font.into()) };
    unsafe { let _ = SetTextColor(dc, COLORREF(0x00ffffff)); }
    let title = if en { "MAXI SOUNDSET" } else { "مکسی سوندست" };
    draw_menu_text(dc, title, RECT { left: scale(14), top: scale(3), right: width-scale(14), bottom: scale(24) }, en);
    unsafe { let _ = SelectObject(dc, normal_font.into()); let _ = SetTextColor(dc, COLORREF(0x00a6bbcf)); }
    let subtitle = if en { "Audio control" } else { "کنترل صدا" };
    draw_menu_text(dc, subtitle, RECT { left: scale(14), top: scale(22), right: width-scale(14), bottom: scale(43) }, en);
    let divider_y = scale(MENU_HEADER);
    unsafe { let _ = MoveToEx(dc, scale(14), divider_y, None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, width-scale(14), divider_y); }
    let names: [String; 6] = if en {
        ["Show MAXI SOUNDSET".into(), if PAUSED.load(Ordering::Relaxed) { "Resume" } else { "Pause" }.into(),
         format!("Quiet · {}", if WINDOWS_MODE.load(Ordering::Relaxed) { 20 } else { 65 }),
         format!("Daily · {}", if WINDOWS_MODE.load(Ordering::Relaxed) { 50 } else { 100 }),
         format!("Loud · {}", if WINDOWS_MODE.load(Ordering::Relaxed) { 100 } else { 140 }), "Exit app".into()]
    } else {
        ["نمایش مکسی سوندست".into(), if PAUSED.load(Ordering::Relaxed) { "ادامه" } else { "مکث" }.into(),
         format!("آرام · {}", if WINDOWS_MODE.load(Ordering::Relaxed) { "۲۰" } else { "۶۵" }),
         format!("روزمره · {}", if WINDOWS_MODE.load(Ordering::Relaxed) { "۵۰" } else { "۱۰۰" }),
         format!("بلند · {}", if WINDOWS_MODE.load(Ordering::Relaxed) { "۱۰۰" } else { "۱۴۰" }), "خروج از برنامه".into()]
    };
    let target = TARGET.load(Ordering::Relaxed) as i32;
    let presets = [if WINDOWS_MODE.load(Ordering::Relaxed) { 20 } else { 65 }, if WINDOWS_MODE.load(Ordering::Relaxed) { 50 } else { 100 }, if WINDOWS_MODE.load(Ordering::Relaxed) { 100 } else { 140 }];
    let selected = presets.iter().position(|value| *value == target).map(|i| i + 2);
    unsafe { let _ = SelectObject(dc, normal_font.into()); }
    for row in 0..6 {
        let y = scale(MENU_HEADER + row * MENU_ROW + if row >= 2 { MENU_GAP } else { 0 } + if row == 5 { MENU_GAP } else { 0 });
        let rect = RECT { left: scale(8), top: y + scale(2), right: width - scale(8), bottom: y + scale(MENU_ROW - 2) };
        let disabled = row == 1 && !ACTIVE.load(Ordering::Relaxed);
        let selected_row = selected == Some(row as usize) && (2..=4).contains(&row);
        let hover = MENU_HOVER.load(Ordering::Relaxed) == row as isize && !disabled;
        if hover || selected_row {
            let fill = CreateSolidBrush(if hover { COLORREF(0x00653e25) } else { COLORREF(0x004a321f) });
            let pen = CreatePen(PS_SOLID, 1, if hover { COLORREF(0x008d6539) } else { COLORREF(0x006c5942) });
            let oldb = unsafe { SelectObject(dc, fill.into()) }; let oldp = unsafe { SelectObject(dc, pen.into()) };
            unsafe { let _ = RoundRect(dc, rect.left, rect.top, rect.right, rect.bottom, scale(10), scale(10)); let _ = SelectObject(dc, oldb); let _ = SelectObject(dc, oldp); let _ = DeleteObject(fill.into()); let _ = DeleteObject(pen.into()); }
        }
        unsafe { let _ = SetTextColor(dc, if disabled { COLORREF(0x007b8b99) } else if selected_row { COLORREF(0x00ffffff) } else { COLORREF(0x00e9f1f7) }); }
        let icon_x = if en { rect.left + scale(17) } else { rect.right - scale(17) };
        draw_row_icon(dc, row, icon_x, (rect.top + rect.bottom) / 2, scale, if disabled { COLORREF(0x007b8b99) } else { COLORREF(0x00a6d5f3) });
        draw_menu_text(dc, &names[row as usize], RECT { left: rect.left + scale(32), top: rect.top, right: rect.right - scale(32), bottom: rect.bottom }, en);
        if selected_row {
            let check = wide("✓");
            let mut check = check;
            let (left, right) = if en { (rect.right-scale(27), rect.right-scale(8)) } else { (rect.left+scale(8), rect.left+scale(27)) };
            let mut check_rect = RECT { left, top: rect.top, right, bottom: rect.bottom };
            unsafe { let _ = SetTextColor(dc, COLORREF(0x00efb86e)); let _ = DrawTextW(dc, &mut check, &mut check_rect, DT_SINGLELINE | DT_VCENTER | DT_LEFT); }
        }
    }
    let sep_y = scale(MENU_HEADER + MENU_ROW * 5 + MENU_GAP + MENU_GAP / 2);
    unsafe { let _ = MoveToEx(dc, scale(16), sep_y, None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, width-scale(16), sep_y); }
    unsafe { let _ = SelectObject(dc, prior); let _ = DeleteObject(title_font.into()); let _ = DeleteObject(normal_font.into()); let _ = EndPaint(hwnd, &ps); }
}

unsafe fn draw_menu_text(dc: HDC, value: &str, mut rect: RECT, en: bool) {
    let mut text: Vec<u16> = value.encode_utf16().collect();
    if text.is_empty() { return; }
    let flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | if en { DT_LEFT } else { DT_RIGHT | DT_RTLREADING };
    unsafe { let _ = DrawTextW(dc, &mut text, &mut rect, flags); }
}

unsafe fn draw_row_icon(dc: HDC, row: i32, cx: i32, cy: i32, scale: impl Fn(i32) -> i32, color: COLORREF) {
    let pen = CreatePen(PS_SOLID, scale(2).max(1), color);
    let old = unsafe { SelectObject(dc, pen.into()) };
    let (l, r, t, b) = (cx-scale(7), cx+scale(7), cy-scale(7), cy+scale(7));
    match row {
        0 => unsafe { let _ = Rectangle(dc, l, t, r, b); let _ = MoveToEx(dc, l, cy-scale(3), None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, r, cy-scale(3)); },
        1 if PAUSED.load(Ordering::Relaxed) => unsafe { let _ = MoveToEx(dc, cx-scale(3), t, None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx-scale(3), b); let _ = MoveToEx(dc, cx+scale(3), t, None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx+scale(3), b); },
        1 => unsafe { let _ = MoveToEx(dc, cx-scale(4), t, None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx+scale(5), cy); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx-scale(4), b); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx-scale(4), t); },
        2..=4 => unsafe { let _ = MoveToEx(dc, l, cy-scale(3), None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx-scale(3), cy-scale(3)); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx+scale(2), cy-scale(7)); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx+scale(2), cy+scale(7)); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx-scale(3), cy+scale(3)); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, l, cy+scale(3)); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, l, cy-scale(3)); let _ = Ellipse(dc, cx+scale(3), cy-scale(5), cx+scale(10), cy+scale(5)); },
        _ => unsafe { let _ = MoveToEx(dc, cx-scale(6), cy, None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx+scale(6), cy); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx+scale(2), cy-scale(4)); let _ = MoveToEx(dc, cx+scale(6), cy, None); let _ = windows::Win32::Graphics::Gdi::LineTo(dc, cx+scale(2), cy+scale(4)); },
    }
    unsafe { let _ = SelectObject(dc, old); let _ = DeleteObject(pen.into()); }
}
fn own_window() -> Option<HWND> {
    unsafe extern "system" fn enumerate(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == unsafe { GetCurrentProcessId() } {
            let mut text = [0u16; 256];
            let length = unsafe { GetWindowTextW(hwnd, &mut text) };
            if String::from_utf16_lossy(&text[..length.max(0) as usize]) == "MAXI SOUNDSET" {
                unsafe {
                    *(data.0 as *mut Option<HWND>) = Some(hwnd);
                }
                return false.into();
            }
        }
        true.into()
    }
    let mut hwnd = None;
    unsafe {
        let _ = EnumWindows(
            Some(enumerate),
            LPARAM((&mut hwnd as *mut Option<HWND>) as isize),
        );
    }
    hwnd
}
