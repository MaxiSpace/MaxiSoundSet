//! Shell callbacks and the nonactivating popup share the existing native tray HWND.
use crate::core_audio::wide;
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
    mpsc::{self, Receiver, Sender},
    Mutex, OnceLock,
};
use std::time::{Duration, Instant};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CreateFontW, CreatePen, CreateRoundRectRgn, CreateSolidBrush, DeleteObject,
            DrawTextW, EndPaint, InvalidateRect, MoveToEx, RoundRect, SelectObject, SetBkMode,
            SetTextColor, SetWindowRgn, ANTIALIASED_QUALITY, BACKGROUND_MODE, DEFAULT_CHARSET,
            DEFAULT_PITCH, DT_LEFT, DT_RIGHT, DT_RTLREADING, DT_SINGLELINE, DT_VCENTER,
            FW_SEMIBOLD, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST, PS_SOLID,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            Shell::{
                Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP,
                NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
            },
            WindowsAndMessaging::*,
        },
    },
};
const MESSAGE: u32 = WM_APP + 741;
const SCROLL_WAKE: u32 = WM_APP + 742;
const MENU_CANCEL: u32 = WM_APP + 743;
const MENU_KEY: u32 = WM_APP + 744;
const ICON_CACHE_TIMER: usize = 0x5300;
const SCROLL_FLUSH_TIMER: usize = 0x5301;
const ICON_CACHE_INTERVAL_MS: u32 = 100;
const ICON_CACHE_MAX_AGE: Duration = Duration::from_millis(250);
static SCROLL_ENABLED: AtomicBool = AtomicBool::new(false);
static TASKBAR_MESSAGE: AtomicU32 = AtomicU32::new(0);
const APP_SUBCLASS_ID: usize = 0x4D415849;
#[link(name = "user32")]
unsafe extern "system" {
    fn UpdateWindow(hwnd: *mut std::ffi::c_void) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
}
#[link(name = "shcore")]
unsafe extern "system" {
    fn GetDpiForMonitor(monitor: *mut std::ffi::c_void, kind: i32, x: *mut u32, y: *mut u32)
        -> i32;
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ENGLISH: AtomicBool = AtomicBool::new(false);
static WINDOWS_MODE: AtomicBool = AtomicBool::new(true);
static PAUSED: AtomicBool = AtomicBool::new(false);
static TARGET: AtomicU32 = AtomicU32::new(50);
static MENU_VISIBLE: AtomicBool = AtomicBool::new(false);
static MENU_HOVER: AtomicIsize = AtomicIsize::new(-1);
static MENU_DPI: AtomicU32 = AtomicU32::new(96);
static MENU_MOUSE_HOVER: AtomicBool = AtomicBool::new(false);
static MENU_KEYS_READY: AtomicBool = AtomicBool::new(false);
static MENU_KEY_SINK: AtomicIsize = AtomicIsize::new(0);
static MENU_KEY_OWNER: AtomicIsize = AtomicIsize::new(0);
static MENU_KEYBOARD_ARMED: AtomicBool = AtomicBool::new(false);
static MENU_SHORTCUT_KEYS: AtomicU32 = AtomicU32::new(0);
static MENU_FOREGROUND: AtomicIsize = AtomicIsize::new(0);
static MENU_ORIGIN_OVERFLOW: AtomicIsize = AtomicIsize::new(0);
static MENU_GENERATION: AtomicU32 = AtomicU32::new(0);
static MENU_CANCEL_PENDING: AtomicU32 = AtomicU32::new(0);
static WINDOW_MOVING: AtomicBool = AtomicBool::new(false);
static WINDOW_MINIMIZED: AtomicBool = AtomicBool::new(false);
pub(crate) fn window_is_moving() -> bool {
    WINDOW_MOVING.load(Ordering::Relaxed)
}
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
    Start,
    Pause,
    Target(i32),
    /// Signed standard wheel notches; the application alone scales/clamps/saves.
    ScrollTarget {
        steps: i32,
        x: i32,
        y: i32,
        /// Processing mode when the native hook accepted this wheel run.
        windows_mode: bool,
    },
    Exit,
    Refresh,
    Repaint,
    FitWindow,
    Hotkey(usize),
    CapturedHotkey(usize, u32, u32),
    CaptureCancelled(usize),
}
pub struct Tray {
    hwnd: HWND,
    capture_hwnd: HWND,
    icon: HICON,
    mouse_hook: Option<HHOOK>,
    pub events: Option<Receiver<Action>>,
    registered: bool,
    tooltip: String,
    hotkey_ids: Vec<i32>,
    pub hotkey_errors: Vec<(usize, String)>,
}
impl Tray {
    pub fn new() -> Result<Self> {
        crate::osd::ensure_vazir_registered()
            .context("Could not register embedded Vazir fonts for the tray popup")?;
        EXIT_MESSAGE.store(
            unsafe {
                RegisterWindowMessageW(PCWSTR(wide("MAXISOUNDSET.RequestSafeExit.v1").as_ptr()))
            },
            Ordering::Relaxed,
        );
        TASKBAR_MESSAGE.store(
            unsafe { RegisterWindowMessageW(PCWSTR(wide("TaskbarCreated").as_ptr())) },
            Ordering::Relaxed,
        );
        SCROLL_ENABLED.store(false, Ordering::Relaxed);
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
        anyhow::ensure!(
            atom != 0,
            "Native tray window class could not be registered"
        );
        let title = wide("MAXI SOUNDSET native notifications");
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .context("Native tray window could not be created")?;
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 244, LWA_ALPHA);
        }
        if !unsafe {
            SetWindowSubclass(capture_hwnd, Some(app_window_subclass), APP_SUBCLASS_ID, 0)
        }
        .as_bool()
        {
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
            mouse_hook: None,
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
        SCROLL_STATE.with(|state| state.borrow_mut().hwnd = hwnd);
        // A low-level hook runs on this UI thread; no audio, disk, shell IPC, or
        // cross-thread locks are permitted in mouse_hook. Hook failure leaves
        // ordinary tray/menu behavior available.
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(instance.into()), 0) }
        {
            Ok(hook) => tray.mouse_hook = Some(hook),
            Err(error) => log::warn!("Tray wheel input unavailable: {error}"),
        }
        unsafe {
            let _ = SetTimer(Some(hwnd), ICON_CACHE_TIMER, ICON_CACHE_INTERVAL_MS, None);
        }
        Ok(tray)
    }
    /// Called from the application timer. Stopped/paused processing is allowed;
    /// busy, capture and exit states must reject target changes.
    pub fn set_scroll_enabled(&mut self, enabled: bool) {
        let enabled = enabled
            && self.registered
            && self.mouse_hook.is_some()
            && CAPTURE_INDEX.load(Ordering::Relaxed) < 0;
        if SCROLL_ENABLED.swap(enabled, Ordering::Relaxed) != enabled {
            invalidate_scroll(true);
            if enabled {
                refresh_icon_cache(self.hwnd);
            }
        }
    }
    pub fn is_registered(&self) -> bool {
        self.registered
    }
    pub fn probe_callback_messages(&self, probe_hotkey: bool) -> (bool, bool) {
        let Some(events) = self.events.as_ref() else {
            return (false, false);
        };
        unsafe {
            let _ = SendMessageW(
                self.hwnd,
                MESSAGE,
                Some(WPARAM(0)),
                Some(LPARAM(WM_LBUTTONUP as isize)),
            );
        }
        let shell_callback = matches!(events.try_recv(), Ok(Action::Show));
        let menu_ok = self.probe_custom_menu();
        if !menu_ok {
            log::error!("Custom tray popup interaction probe failed");
        }
        if !probe_hotkey {
            return (shell_callback && menu_ok, true);
        }
        let Some(id) = self.hotkey_ids.first().copied() else {
            return (shell_callback, false);
        };
        let expected = HOTKEY_IDS
            .get()
            .and_then(|map| map.lock().ok()?.get(&id).copied());
        unsafe {
            let _ = SendMessageW(
                self.hwnd,
                WM_HOTKEY,
                Some(WPARAM(id as usize)),
                Some(LPARAM(0)),
            );
        }
        let hotkey_callback = matches!((expected, events.try_recv()), (Some(index), Ok(Action::Hotkey(actual))) if index == actual);
        (shell_callback && menu_ok, hotkey_callback)
    }
    /// Exercise the custom popup through its actual HWND procedure. This is
    /// used only by the isolated startup diagnostic modes.
    fn probe_custom_menu(&self) -> bool {
        let Some(events) = self.events.as_ref() else {
            return false;
        };
        while events.try_recv().is_ok() {}
        ACTIVE.store(true, Ordering::Relaxed);
        PAUSED.store(false, Ordering::Relaxed);
        WINDOWS_MODE.store(true, Ordering::Relaxed);
        ENGLISH.store(false, Ordering::Relaxed);
        TARGET.store(50, Ordering::Relaxed);
        unsafe {
            let foreground_before = GetForegroundWindow();
            let _ = SendMessageW(
                self.hwnd,
                MESSAGE,
                Some(WPARAM(0)),
                Some(LPARAM(WM_RBUTTONUP as isize)),
            );
            let opened =
                MENU_VISIBLE.load(Ordering::Relaxed) && IsWindowVisible(self.hwnd).as_bool();
            let keys_ready = MENU_KEYS_READY.load(Ordering::Relaxed);
            let stayed_nonactivating = GetForegroundWindow() == foreground_before;
            let _ = UpdateWindow(self.hwnd.0);
            let _ = SendMessageW(
                HWND(MENU_KEY_SINK.load(Ordering::Relaxed) as *mut std::ffi::c_void),
                MENU_KEY,
                Some(WPARAM(MENU_KEYS[6].1 as usize)),
                Some(LPARAM(MENU_GENERATION.load(Ordering::Relaxed) as isize)),
            );
            let escaped =
                !MENU_VISIBLE.load(Ordering::Relaxed) && !IsWindowVisible(self.hwnd).as_bool();
            let _ = SendMessageW(
                self.hwnd,
                MESSAGE,
                Some(WPARAM(0)),
                Some(LPARAM(WM_RBUTTONUP as isize)),
            );
            let dpi = MENU_DPI.load(Ordering::Relaxed).max(96) as i32;
            let x = MENU_WIDTH * dpi / 192;
            let y = (MENU_HEADER + MENU_ROW * 3 + MENU_GAP + MENU_ROW / 2) * dpi / 96;
            let packed = (((y as u32) << 16) | (x as u16 as u32)) as isize;
            let _ = SendMessageW(
                self.hwnd,
                WM_LBUTTONUP,
                Some(WPARAM(0)),
                Some(LPARAM(packed)),
            );
            let target = matches!(events.try_recv(), Ok(Action::Target(50)));
            opened
                && stayed_nonactivating
                && escaped
                && target
                && keys_ready
        }
    }
    /// Register shortcuts on the dedicated native tray HWND. It remains alive
    /// while the Slint window is hidden and keeps WM_HOTKEY out of Winit's WndProc.
    pub fn register_hotkeys(
        &mut self,
        bindings: &[crate::settings::HotkeyBinding],
    ) -> Vec<(usize, String)> {
        CAPTURE_INDEX.store(-1, Ordering::SeqCst);
        self.unregister_hotkeys();
        let mut failures = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let map = HOTKEY_IDS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registered = map.lock().unwrap_or_else(|p| p.into_inner());
        for (index, binding) in bindings.iter().enumerate() {
            if !binding.enabled {
                continue;
            }
            let chord = (binding.modifiers, binding.virtual_key);
            if !seen.insert(chord) {
                failures.push((
                    index,
                    "Duplicate shortcut in MAXI SOUNDSET settings".to_string(),
                ));
                continue;
            }
            let id = 0x5100 + index as i32;
            let ok = unsafe {
                RegisterHotKey(
                    self.hwnd.0,
                    id,
                    binding.modifiers | MOD_NOREPEAT_VALUE,
                    binding.virtual_key,
                )
            };
            if ok != 0 {
                registered.insert(id, index);
                self.hotkey_ids.push(id);
                MENU_SHORTCUT_KEYS.fetch_or(
                    menu_shortcut_key_bit(binding.modifiers, binding.virtual_key), Ordering::Relaxed);
            } else {
                let code = unsafe { windows::Win32::Foundation::GetLastError().0 };
                failures.push((
                    index,
                    format!("Windows could not register shortcut (error {code})"),
                ));
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
        flush_scroll(self.hwnd);
        self.unregister_hotkeys();
        CAPTURE_INDEX.store(index as isize, Ordering::SeqCst);
        self.set_scroll_enabled(false);
    }
    fn unregister_hotkeys(&mut self) {
        let map = HOTKEY_IDS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registered = map.lock().unwrap_or_else(|p| p.into_inner());
        for id in self.hotkey_ids.drain(..) {
            unsafe {
                let _ = UnregisterHotKey(self.hwnd.0, id);
            }
            registered.remove(&id);
        }
        MENU_SHORTCUT_KEYS.store(0, Ordering::Relaxed);
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
        invalidate_scroll(true);
        self.registered = unsafe { Shell_NotifyIconW(NIM_ADD, &self.data()) }.as_bool();
        anyhow::ensure!(
            self.registered,
            "Windows notification area rejected the tray icon"
        );
        Ok(())
    }
    pub fn update(
        &mut self,
        active: bool,
        paused: bool,
        target: i32,
        english: bool,
        windows_mode: bool,
    ) {
        let menu_changed = ENGLISH.load(Ordering::Relaxed) != english
            || WINDOWS_MODE.load(Ordering::Relaxed) != windows_mode
            || ACTIVE.load(Ordering::Relaxed) != active
            || PAUSED.load(Ordering::Relaxed) != paused
            || TARGET.load(Ordering::Relaxed) != target.max(0) as u32;
        if WINDOWS_MODE.load(Ordering::Relaxed) != windows_mode {
            invalidate_scroll(true);
        }
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
        if menu_changed && MENU_VISIBLE.load(Ordering::Relaxed) {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
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
        SCROLL_ENABLED.store(false, Ordering::Relaxed);
        invalidate_scroll(true);
        unsafe {
            if let Some(hook) = self.mouse_hook.take() {
                let _ = UnhookWindowsHookEx(hook);
            }
            let _ = KillTimer(Some(self.hwnd), ICON_CACHE_TIMER);
            let _ = KillTimer(Some(self.hwnd), SCROLL_FLUSH_TIMER);
            dismiss_menu(self.hwnd);
        }
        SCROLL_STATE.with(|state| *state.borrow_mut() = ScrollState::default());
        self.unregister_hotkeys();
        unsafe {
            // Winit may already have destroyed its HWND when the Slint event
            // loop returns; never pass a stale handle into comctl32 cleanup.
            if IsWindow(Some(self.capture_hwnd)).as_bool() {
                let _ = RemoveWindowSubclass(
                    self.capture_hwnd,
                    Some(app_window_subclass),
                    APP_SUBCLASS_ID,
                );
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

#[derive(Clone, Copy)]
struct IconHit {
    rect: RECT,
    host: HWND,
    leaf: HWND,
    host_rect: RECT,
    refreshed: Instant,
}
#[derive(Clone, Copy, Default)]
struct WheelAccumulator {
    remainder: i32,
}
impl WheelAccumulator {
    fn reset(&mut self) {
        *self = Self::default();
    }
    fn push(&mut self, delta: i32) -> i32 {
        let total = self.remainder + delta;
        self.remainder = total % 120;
        total / 120
    }
}
#[derive(Clone, Copy)]
struct ScrollBatch {
    steps: i64,
    point: POINT,
    hit: IconHit,
    windows_mode: bool,
}
fn append_scroll_batch(queue: &mut VecDeque<ScrollBatch>, steps: i32, point: POINT, hit: IconHit, windows_mode: bool) {
    if steps == 0 {
        return;
    }
    if let Some(last) = queue.back_mut() {
        if last.steps.signum() == (steps as i64).signum()
            && same_rect(last.hit.rect, hit.rect)
            && last.hit.host == hit.host
            && last.hit.leaf == hit.leaf
            && last.windows_mode == windows_mode
        {
            last.steps += steps as i64;
            last.point = point;
            return;
        }
    }
    queue.push_back(ScrollBatch {
        steps: steps as i64,
        point,
        hit,
        windows_mode,
    });
}
#[derive(Default)]
struct ScrollState {
    hwnd: HWND,
    hit: Option<IconHit>,
    wheel: WheelAccumulator,
    pending: VecDeque<ScrollBatch>,
    wake_pending: bool,
}
thread_local! {
    // The hook, its owner window and the application timer all use the same
    // thread. Never retain this borrow across a call which may pump messages.
    static SCROLL_STATE: RefCell<ScrollState> = RefCell::new(ScrollState::default());
}
fn point_in(rect: RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}
fn same_rect(a: RECT, b: RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}
fn invalidate_scroll(discard_pending: bool) {
    SCROLL_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.hit = None;
        state.wheel.reset();
        if discard_pending {
            state.pending.clear();
            state.wake_pending = false;
        }
    });
}
/// Shell IPC belongs here, on the ordinary timer, never in the hook callback.
fn refresh_icon_cache(hwnd: HWND) {
    if !SCROLL_ENABLED.load(Ordering::Relaxed) || MENU_VISIBLE.load(Ordering::Relaxed) {
        invalidate_scroll(false);
        return;
    }
    let identifier = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: 741,
        ..Default::default()
    };
    let hit = (|| {
        let rect = unsafe { Shell_NotifyIconGetRect(&identifier) }.ok()?;
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return None;
        }
        let center = POINT {
            x: rect.left + (rect.right - rect.left) / 2,
            y: rect.top + (rect.bottom - rect.top) / 2,
        };
        let leaf = unsafe { WindowFromPoint(center) };
        let host = unsafe { GetAncestor(leaf, GA_ROOT) };
        if host.0.is_null()
            || !unsafe { IsWindowVisible(host) }.as_bool()
            || !unsafe { IsWindowVisible(leaf) }.as_bool()
        {
            return None;
        }
        let mut class = [0u16; 96];
        let len = unsafe { GetClassNameW(host, &mut class) };
        let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
        // Explorer's taskbars and the Windows 10/11 visible overflow hosts.
        // A hidden overflow's stale rect can point at an ordinary app instead.
        if !matches!(
            class.as_str(),
            "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "NotifyIconOverflowWindow"
                | "TopLevelWindowForOverflowXamlIsland"
        ) {
            return None;
        }
        let mut host_rect = RECT::default();
        unsafe { GetWindowRect(host, &mut host_rect) }.ok()?;
        if !point_in(host_rect, center) {
            return None;
        }
        Some(IconHit {
            rect,
            host,
            leaf,
            host_rect,
            refreshed: Instant::now(),
        })
    })();
    SCROLL_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let unchanged = match (state.hit, hit) {
            (Some(a), Some(b)) => {
                same_rect(a.rect, b.rect)
                    && a.host == b.host
                    && a.leaf == b.leaf
                    && same_rect(a.host_rect, b.host_rect)
            }
            _ => false,
        };
        if !unchanged {
            state.wheel.reset();
        }
        state.hit = hit;
    });
}
/// user32 visibility/hit checks are bounded; no Explorer messages are sent.
fn visible_icon_at(hit: IconHit, point: POINT) -> bool {
    if hit.refreshed.elapsed() > ICON_CACHE_MAX_AGE || !point_in(hit.rect, point) {
        return false;
    }
    unsafe {
        if !IsWindowVisible(hit.host).as_bool() || !IsWindowVisible(hit.leaf).as_bool() {
            return false;
        }
        let leaf = WindowFromPoint(point);
        if leaf != hit.leaf || GetAncestor(leaf, GA_ROOT) != hit.host {
            return false;
        }
        let mut rect = RECT::default();
        GetWindowRect(hit.host, &mut rect).is_ok() && same_rect(rect, hit.host_rect)
    }
}
unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, w, l) };
    }
    let input = unsafe { &*(l.0 as *const MSLLHOOKSTRUCT) };
    observe_menu_button(w.0 as u32, input.pt);
    // Button input always continues to its original recipient, including when
    // it has just queued cancellation of this nonactivating popup.
    if !matches!(w.0 as u32, WM_MOUSEMOVE | WM_MOUSEWHEEL) {
        return unsafe { CallNextHookEx(None, code, w, l) };
    }
    let accepted = SCROLL_STATE.with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return false;
        };
        let valid = SCROLL_ENABLED.load(Ordering::Relaxed)
            && CAPTURE_INDEX.load(Ordering::Relaxed) < 0
            && !MENU_VISIBLE.load(Ordering::Relaxed)
            && state.hit.is_some_and(|hit| visible_icon_at(hit, input.pt));
        if !valid {
            state.wheel.reset();
            return false;
        }
        if w.0 as u32 != WM_MOUSEWHEEL {
            return false;
        }
        let delta = (input.mouseData >> 16) as u16 as i16 as i32;
        if delta == 0 {
            return false;
        }
        let previous = state.wheel;
        let steps = state.wheel.push(delta);
        if steps != 0 && !state.wake_pending {
            // One wake arms a short batch timer. Subsequent physical deltas
            // accumulate in memory without queuing one application action each.
            if unsafe { PostMessageW(Some(state.hwnd), SCROLL_WAKE, WPARAM(0), LPARAM(0)) }.is_err()
            {
                state.wheel = previous;
                return false;
            }
            state.wake_pending = true;
        }
        let hit = state.hit.unwrap(); // Validated above; this borrow never yields.
        // Keep acceptance context through deferred delivery. Reading this only
        // when emitting would relabel old input after a processing-mode change.
        let windows_mode = WINDOWS_MODE.load(Ordering::Relaxed);
        append_scroll_batch(&mut state.pending, steps, input.pt, hit, windows_mode);
        true // Fine deltas belong to this gesture even before a full notch.
    });
    if accepted {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, w, l) }
    }
}
fn batch_actions(mut batches: VecDeque<ScrollBatch>, following: Option<Action>) -> Vec<Action> {
    let mut actions = Vec::with_capacity(batches.len());
    while let Some(mut batch) = batches.pop_front() {
        while batch.steps != 0 {
            let emitted = batch.steps.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            batch.steps -= emitted as i64;
            actions.push(Action::ScrollTarget {
                steps: emitted,
                x: batch.point.x,
                y: batch.point.y,
                windows_mode: batch.windows_mode,
            });
        }
    }
    if let Some(action) = following {
        actions.push(action);
    }
    actions
}
fn take_scroll_batches(hwnd: HWND) -> VecDeque<ScrollBatch> {
    unsafe {
        let _ = KillTimer(Some(hwnd), SCROLL_FLUSH_TIMER);
    }
    let mut batches = SCROLL_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.wake_pending = false;
        std::mem::take(&mut state.pending)
    });
    if batches.is_empty()
        || !SCROLL_ENABLED.load(Ordering::Relaxed)
        || CAPTURE_INDEX.load(Ordering::Relaxed) >= 0
    {
        return VecDeque::new();
    }
    // Rects for neighboring tray icons can share the same user32 leaf/root.
    // Re-query the exact Shell icon identity before applying any queued input,
    // outside the hook. A same-host reorder therefore cannot mutate the target
    // from the obsolete rect during the periodic-cache race.
    refresh_icon_cache(hwnd);
    let current = SCROLL_STATE.with(|state| state.borrow().hit);
    batches.retain(|batch| {
        current.is_some_and(|hit| {
            same_rect(hit.rect, batch.hit.rect)
                && hit.host == batch.hit.host
                && hit.leaf == batch.hit.leaf
                && visible_icon_at(hit, batch.point)
        })
    });
    batches
}
fn flush_scroll(hwnd: HWND) {
    for action in batch_actions(take_scroll_batches(hwnd), None) {
        send(action);
    }
}
/// Target-affecting commands must follow physical wheel input already accepted
/// by the native hook, even if its 20ms coalescing timer has not fired yet.
fn dispatch_after_scroll(hwnd: HWND, action: Action) {
    let batches = take_scroll_batches(hwnd);
    SCROLL_STATE.with(|state| state.borrow_mut().wheel.reset());
    for action in batch_actions(batches, Some(action)) {
        send(action);
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
    // The same owned class also hosts a message-only keyboard receiver. Its
    // owner is a native HWND value, not a borrowed Rust allocation. Main tray
    // creation has a null lpCreateParams and keeps its default zero userdata.
    if message == WM_NCCREATE && l.0 != 0 {
        let create = unsafe { &*(l.0 as *const CREATESTRUCTW) };
        if !create.lpCreateParams.is_null() {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize); }
        }
    }
    let keyboard_owner = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if keyboard_owner != 0 {
        return unsafe { menu_keyboard_message(hwnd, message, w, l,
            HWND(keyboard_owner as *mut std::ffi::c_void)) };
    }
    if message == MENU_CANCEL {
        let generation = w.0 as u32;
        if generation != 0
            && generation == MENU_GENERATION.load(Ordering::Relaxed)
            && generation == MENU_CANCEL_PENDING.load(Ordering::Relaxed)
        {
            unsafe { dismiss_menu(hwnd); }
        }
        return LRESULT(0);
    }
    if message != 0 && message == EXIT_MESSAGE.load(Ordering::Relaxed) {
        // Use the same shutdown path as Exit app, including audio restoration.
        dispatch_after_scroll(hwnd, Action::Exit);
        return LRESULT(0);
    }
    if message == SCROLL_WAKE {
        if !SCROLL_STATE.with(|state| state.borrow().wake_pending) {
            return LRESULT(0);
        }
        if unsafe { SetTimer(Some(hwnd), SCROLL_FLUSH_TIMER, 20, None) } == 0 {
            flush_scroll(hwnd);
        }
        return LRESULT(0);
    }
    if message == WM_TIMER && w.0 == ICON_CACHE_TIMER {
        refresh_icon_cache(hwnd);
        return LRESULT(0);
    }
    if message == WM_TIMER && w.0 == SCROLL_FLUSH_TIMER {
        flush_scroll(hwnd);
        return LRESULT(0);
    }
    if matches!(message, WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED) {
        invalidate_scroll(false);
        if MENU_VISIBLE.load(Ordering::Relaxed) {
            unsafe {
                dismiss_menu(hwnd);
            }
        }
    }
    if message == MESSAGE {
        flush_scroll(hwnd);
        match l.0 as u32 & 0xffff {
            WM_LBUTTONUP | NIN_SELECT => send(Action::Show),
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                menu(hwnd);
            },
            _ => (),
        }
        return LRESULT(0);
    }
    // Menu input is received only by its opening's keyboard HWND. Legacy
    // reserved hotkey IDs on the tray must never reach the shortcut map.
    if message == WM_HOTKEY {
        if MENU_KEYS.iter().any(|(id, _)| *id == w.0 as i32) {
            return LRESULT(0);
        }
    }
    if MENU_VISIBLE.load(Ordering::Relaxed) {
        match message {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => {
                unsafe {
                    paint_menu(hwnd);
                }
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_TIMER if w.0 == MENU_TIMER_ID => {
                if !retain_menu_context(hwnd) {
                    return LRESULT(0);
                }
                let mut cursor = POINT::default();
                let mut rect = RECT::default();
                unsafe {
                    let _ = GetCursorPos(&mut cursor);
                    let _ = GetWindowRect(hwnd, &mut rect);
                }
                let inside = cursor.x >= rect.left
                    && cursor.x < rect.right
                    && cursor.y >= rect.top
                    && cursor.y < rect.bottom;
                let pressed = unsafe { GetAsyncKeyState(0x01) < 0 || GetAsyncKeyState(0x02) < 0 };
                if pressed && !inside {
                    unsafe {
                        dismiss_menu(hwnd);
                    }
                } else if !inside && MENU_MOUSE_HOVER.swap(false, Ordering::Relaxed) {
                    MENU_HOVER.store(-1, Ordering::Relaxed);
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                MENU_MOUSE_HOVER.store(true, Ordering::Relaxed);
                let next = menu_row_at(client_x(l), client_y(l));
                if MENU_HOVER.swap(next as isize, Ordering::Relaxed) != next as isize {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                if !retain_menu_context(hwnd) {
                    return LRESULT(0);
                }
                let row = menu_row_at(client_x(l), client_y(l));
                unsafe {
                    dismiss_menu(hwnd);
                }
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
        if let Some(index) = HOTKEY_IDS
            .get()
            .and_then(|ids| ids.lock().ok()?.get(&(w.0 as i32)).copied())
        {
            log::debug!("WM_HOTKEY id={} action={index}", w.0);
            dispatch_after_scroll(hwnd, Action::Hotkey(index));
        }
        return LRESULT(0);
    }
    // Explorer broadcasts TaskbarCreated to top-level windows after restarting.
    let taskbar = TASKBAR_MESSAGE.load(Ordering::Relaxed);
    if taskbar != 0 && message == taskbar {
        invalidate_scroll(true);
        unsafe {
            dismiss_menu(hwnd);
        }
        send(Action::Refresh);
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

unsafe extern "system" fn app_window_subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let capture = CAPTURE_INDEX.load(Ordering::SeqCst);
    if capture >= 0 && (message == WM_KILLFOCUS || (message == WM_SHOWWINDOW && w.0 == 0)) {
        CAPTURE_INDEX.store(-1, Ordering::SeqCst);
        send(Action::CaptureCancelled(capture as usize));
    }
    if capture >= 0 && (matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) || message == WM_APPCOMMAND) {
        let key = if message == WM_APPCOMMAND {
            match ((l.0 as usize >> 16) & 0x0fff) as u32 {
                8 => 0xAD,
                9 => 0xAE,
                10 => 0xAF,
                _ => 0,
            }
        } else {
            w.0 as u32
        };
        let call_previous = || unsafe { DefSubclassProc(hwnd, message, w, l) };
        if key == 0 {
            return call_previous();
        }
        if matches!(key, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5) {
            return call_previous();
        }
        if key == 0x1B {
            CAPTURE_INDEX.store(-1, Ordering::SeqCst);
            send(Action::CaptureCancelled(capture as usize));
        } else {
            let mut modifiers = 0u32;
            if unsafe { GetKeyState(0x11) } < 0 {
                modifiers |= 0x0002;
            }
            if unsafe { GetKeyState(0x12) } < 0 {
                modifiers |= 0x0001;
            }
            if unsafe { GetKeyState(0x10) } < 0 {
                modifiers |= 0x0004;
            }
            if unsafe { GetKeyState(0x5B) } < 0 || unsafe { GetKeyState(0x5C) } < 0 {
                modifiers |= 0x0008;
            }
            let reserved = modifiers & 0x0008 != 0
                || (modifiers & 0x0001 != 0 && matches!(key, 0x09 | 0x73 | 0x1B | 0x20))
                || (modifiers & 0x0002 != 0 && key == 0x1B)
                || (modifiers & 0x0006 == 0x0006 && key == 0x1B)
                || (modifiers & 0x0003 == 0x0003 && key == 0x2E);
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
    if message == WM_ENTERSIZEMOVE {
        WINDOW_MOVING.store(true, Ordering::Relaxed);
    }
    if message == WM_EXITSIZEMOVE {
        WINDOW_MOVING.store(false, Ordering::Relaxed);
    }
    let restored = message == WM_SIZE
        && !matches!(w.0 as u32, SIZE_MINIMIZED)
        && WINDOW_MINIMIZED.swap(false, Ordering::Relaxed);
    if message == WM_SIZE && w.0 == SIZE_MINIMIZED as usize {
        WINDOW_MINIMIZED.store(true, Ordering::Relaxed);
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
    let result = unsafe { DefSubclassProc(hwnd, message, w, l) };
    // Queue after Winit applies the suggested DPI rectangle; no correction
    // during interactive move/resize or a stream of WM_SIZE notifications.
    if restored
        || matches!(
            message,
            WM_DPICHANGED | WM_EXITSIZEMOVE | WM_DISPLAYCHANGE | WM_SETTINGCHANGE
        )
        || (message == WM_SHOWWINDOW && w.0 != 0)
    {
        send(Action::FitWindow);
    }
    result
}
const MENU_WIDTH: i32 = 236;
// Embedded Vazir's native GDI line boxes at 96 DPI are 22px for the 14-DIP
// title and 18px for the 11-DIP status. Reserve complete line cells plus
// breathing room above the title and before the first command.
const MENU_HEADER: i32 = 50;
const MENU_ROW: i32 = 30;
const MENU_GAP: i32 = 5;
const MENU_HEIGHT: i32 = MENU_HEADER + MENU_ROW * 6 + MENU_GAP * 2 + 2;
const MENU_TIMER_ID: usize = 0x5200;
const MENU_KEYS: [(i32, u32); 7] = [
    (0x5201, 0x26),
    (0x5202, 0x28),
    (0x5203, 0x24),
    (0x5204, 0x23),
    (0x5205, 0x0D),
    (0x5206, 0x20),
    (0x5207, 0x1B),
];

fn menu_key_bit(key: u32) -> u32 {
    MENU_KEYS.iter().position(|(_, candidate)| *candidate == key)
        .map_or(0, |index| 1 << index)
}

fn menu_shortcut_key_bit(modifiers: u32, key: u32) -> u32 {
    // MOD_NOREPEAT controls notification frequency, not a physical modifier.
    // Do not rewrite persisted settings or ignore other unknown flag bits.
    if modifiers & !MOD_NOREPEAT_VALUE == 0 { menu_key_bit(key) } else { 0 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuKeyDecision { Pass, Consume, Post }

#[derive(Default)]
struct MenuKeyPolicy {
    held: u32,
    owned: u32,
}
impl MenuKeyPolicy {
    fn input(&mut self, message: u32, key: u32, modified: bool, excluded: u32) -> MenuKeyDecision {
        let bit = menu_key_bit(key);
        if bit == 0 { return MenuKeyDecision::Pass; }
        if matches!(message, WM_KEYUP | WM_SYSKEYUP) {
            let owned = self.owned & bit != 0;
            self.held &= !bit;
            self.owned &= !bit;
            return if owned && !modified && message == WM_KEYUP {
                MenuKeyDecision::Consume
            } else { MenuKeyDecision::Pass };
        }
        if !matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) { return MenuKeyDecision::Pass; }
        let held = self.held & bit != 0;
        // Remember passed downs too: releasing a modifier while keeping Down
        // pressed must not turn its auto-repeat into a fresh menu command.
        self.held |= bit;
        if modified || message == WM_SYSKEYDOWN || excluded & bit != 0 {
            return MenuKeyDecision::Pass;
        }
        if held {
            return if self.owned & bit != 0 { MenuKeyDecision::Consume } else { MenuKeyDecision::Pass };
        }
        MenuKeyDecision::Post
    }
    fn posted(&mut self, key: u32, success: bool) {
        if success { self.owned |= menu_key_bit(key); }
        // Failure leaves held set: a passed down cannot be recaptured on repeat.
    }
}

#[derive(Default)]
struct MenuKeyboard {
    hook: Option<HHOOK>,
    policy: MenuKeyPolicy,
}
thread_local! {
    static MENU_KEYBOARD: RefCell<MenuKeyboard> = RefCell::new(MenuKeyboard::default());
}

fn keyboard_hook_result(
    code: i32, accept: impl FnOnce() -> bool, next: impl FnOnce() -> LRESULT,
) -> LRESULT {
    // In particular, a negative hook code never reads lParam.
    if code < 0 || !accept() { next() } else { LRESULT(1) }
}

unsafe extern "system" fn menu_keyboard_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    keyboard_hook_result(code, || {
        let input = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
        accept_menu_keyboard(w.0 as u32, input)
    }, || unsafe { CallNextHookEx(None, code, w, l) })
}

fn request_menu_cancel() {
    let generation = MENU_GENERATION.load(Ordering::Relaxed);
    if generation == 0 || !MENU_VISIBLE.load(Ordering::Relaxed) { return; }
    MENU_CANCEL_PENDING.store(generation, Ordering::Relaxed);
    let owner = HWND(MENU_KEY_OWNER.load(Ordering::Relaxed) as *mut std::ffi::c_void);
    let _ = unsafe { PostMessageW(Some(owner), MENU_CANCEL, WPARAM(generation as usize), LPARAM(0)) };
}

fn accept_menu_keyboard(message: u32, input: &KBDLLHOOKSTRUCT) -> bool {
    if !MENU_KEYBOARD_ARMED.load(Ordering::Relaxed) { return false; }
    let valid = menu_context_valid();
    // Only query OTHER keys during a navigation callback. Its own async state
    // has not yet been updated; repeat ownership is tracked explicitly below.
    let modified = valid && menu_key_bit(input.vkCode) != 0
        && menu_key_modified(input.flags.0, |key| unsafe { GetAsyncKeyState(key) } < 0);
    accept_menu_keyboard_with(message, input.vkCode, valid, modified,
        |sink, generation, key| unsafe { PostMessageW(Some(sink), MENU_KEY,
            WPARAM(key as usize), LPARAM(generation as isize)) }.is_ok())
}

fn menu_key_modified(flags: u32, mut down: impl FnMut(i32) -> bool) -> bool {
    flags & LLKHF_ALTDOWN.0 != 0 || [0x10, 0x11, 0x12, 0x5B, 0x5C].into_iter().any(&mut down)
}

fn accept_menu_keyboard_with(
    message: u32, key: u32, context_valid: bool, modified: bool,
    post: impl FnOnce(HWND, u32, u32) -> bool,
) -> bool {
    if !MENU_KEYBOARD_ARMED.load(Ordering::Relaxed) { return false; }
    // This predicate contains only bounded user32 queries, and does not hide
    // windows, send Explorer messages, dispatch actions, or take a lock.
    if !context_valid {
        MENU_KEYBOARD_ARMED.store(false, Ordering::Relaxed);
        request_menu_cancel();
        return false;
    }
    if menu_key_bit(key) == 0 { return false; }
    let decision = MENU_KEYBOARD.with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else { return None; };
        Some(state.policy.input(message, key, modified,
            MENU_SHORTCUT_KEYS.load(Ordering::Relaxed)))
    });
    match decision {
        Some(MenuKeyDecision::Consume) => true,
        Some(MenuKeyDecision::Pass) => false,
        Some(MenuKeyDecision::Post) => {
            let sink = HWND(MENU_KEY_SINK.load(Ordering::Relaxed) as *mut std::ffi::c_void);
            let generation = MENU_GENERATION.load(Ordering::Relaxed);
            let posted = !sink.0.is_null() && generation != 0 && post(sink, generation, key);
            let recorded = MENU_KEYBOARD.with(|state| {
                let Ok(mut state) = state.try_borrow_mut() else { return false; };
                state.policy.posted(key, posted);
                true
            });
            if !posted || !recorded {
                MENU_KEYBOARD_ARMED.store(false, Ordering::Relaxed);
                request_menu_cancel();
                return false;
            }
            true
        }
        None => {
            // Once an event passed, this opening must never recapture a repeat.
            MENU_KEYBOARD_ARMED.store(false, Ordering::Relaxed);
            request_menu_cancel();
            false
        }
    }
}

fn retire_menu_keyboard_with(unhook: impl FnOnce(HHOOK) -> bool) -> bool {
    MENU_KEYBOARD_ARMED.store(false, Ordering::Relaxed);
    let hook = MENU_KEYBOARD.with(|state| state.borrow().hook);
    let Some(hook) = hook else { return true; };
    if !unhook(hook) {
        // Keep this disabled handle. Installing the same static callback again
        // could otherwise let two hooks process the next opening's input.
        return false;
    }
    MENU_KEYBOARD.with(|state| state.borrow_mut().hook = None);
    true
}

fn retire_menu_keyboard() -> bool {
    retire_menu_keyboard_with(|hook| unsafe { UnhookWindowsHookEx(hook) }.is_ok())
}

fn install_menu_keyboard() -> bool {
    install_menu_keyboard_with(
        |hook| unsafe { UnhookWindowsHookEx(hook) }.is_ok(),
        || {
            let instance = unsafe { GetModuleHandleW(None) }.ok()?;
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL,
                Some(menu_keyboard_hook), Some(instance.into()), 0) }.ok()
        },
        || MENU_KEYS.iter().fold(0, |held, (_, key)| held
            | if unsafe { GetAsyncKeyState(*key as i32) } < 0 { menu_key_bit(*key) } else { 0 }),
    )
}

fn install_menu_keyboard_with(
    unhook: impl FnOnce(HHOOK) -> bool,
    install: impl FnOnce() -> Option<HHOOK>,
    held: impl FnOnce() -> u32,
) -> bool {
    if !retire_menu_keyboard_with(unhook) { return false; }
    MENU_KEYBOARD.with(|state| state.borrow_mut().policy = MenuKeyPolicy::default());
    let Some(hook) = install() else { return false; };
    MENU_KEYBOARD.with(|state| state.borrow_mut().hook = Some(hook));
    // A key held before the menu opened never becomes a fresh command just
    // because the next event observed by this hook is an auto-repeat.
    let held = held();
    MENU_KEYBOARD.with(|state| state.borrow_mut().policy.held |= held);
    MENU_KEYBOARD_ARMED.store(true, Ordering::Relaxed);
    true
}

unsafe fn menu_keyboard_message(
    hwnd: HWND, message: u32, w: WPARAM, l: LPARAM, owner: HWND,
) -> LRESULT {
    if message == MENU_KEY {
        if MENU_KEY_SINK.load(Ordering::Relaxed) == hwnd.0 as isize
            && MENU_VISIBLE.load(Ordering::Relaxed)
            && l.0 as u32 == MENU_GENERATION.load(Ordering::Relaxed)
        {
            if menu_key_bit(w.0 as u32) != 0 {
                handle_menu_key(owner, w.0 as u32);
            }
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY
        && MENU_KEY_SINK.compare_exchange(hwnd.0 as isize, 0,
            Ordering::Relaxed, Ordering::Relaxed).is_ok()
    {
        // An unexpected destruction retires this opening too. Clear identity
        // before dismissing: cleanup must not destroy this HWND recursively,
        // and teardown of an older receiver cannot dismiss a newer opening.
        unsafe {
            // No key can route after retiring the receiver identity.
            MENU_KEYS_READY.store(false, Ordering::Relaxed);
            retire_menu_keyboard();
            dismiss_menu(owner);
        }
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

unsafe fn create_menu_keyboard(owner: HWND) -> Result<HWND> {
    let class = wide("MaxiSoundSet.NativeTrayWindow");
    let instance = unsafe { GetModuleHandleW(None)? };
    let sink = unsafe {
        CreateWindowExW(WINDOW_EX_STYLE::default(), PCWSTR(class.as_ptr()), PCWSTR::null(),
            WINDOW_STYLE::default(), 0, 0, 0, 0, Some(HWND_MESSAGE), None,
            Some(instance.into()), Some(owner.0.cast_const()))?
    };
    // Do not publish an HWND whose owner tag failed to initialize.
    // It would otherwise be mistaken for the long-lived tray procedure.
    if unsafe { GetWindowLongPtrW(sink, GWLP_USERDATA) } != owner.0 as isize {
        unsafe { let _ = DestroyWindow(sink); }
        anyhow::bail!("Tray menu keyboard receiver owner could not be initialized");
    }
    Ok(sink)
}

unsafe fn menu(hwnd: HWND) {
    if MENU_VISIBLE.load(Ordering::Relaxed) {
        return;
    }
    let mut point = POINT::default();
    let _ = unsafe { GetCursorPos(&mut point) };
    // Snapshot the launching context before positioning can cover the cursor
    // or synchronously pump messages. This popup never takes foreground.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null()
        || !unsafe { IsWindow(Some(foreground)) }.as_bool()
        || !unsafe { IsWindowVisible(foreground) }.as_bool()
    {
        return;
    }
    let origin = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    let mut origin_class = [0u16; 96];
    let origin_len = unsafe { GetClassNameW(origin, &mut origin_class) }.max(0) as usize;
    let overflow = if matches!(
        String::from_utf16_lossy(&origin_class[..origin_len]).as_str(),
        "NotifyIconOverflowWindow" | "TopLevelWindowForOverflowXamlIsland"
    ) && unsafe { IsWindowVisible(origin) }.as_bool() {
        origin
    } else {
        HWND::default()
    };
    let monitor =
        unsafe { windows::Win32::Graphics::Gdi::MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info) }
        .as_bool()
    {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    };
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    // The tray HWND may still belong to the previous monitor. Size from the
    // destination monitor before moving, including negative desktop positions.
    let dpi = if unsafe { GetDpiForMonitor(monitor.0, 0, &mut dpi_x, &mut dpi_y) } >= 0 {
        dpi_x.max(96) as i32
    } else {
        96
    };
    MENU_DPI.store(dpi as u32, Ordering::Relaxed);
    let width = MENU_WIDTH * dpi / 96;
    let height = MENU_HEIGHT * dpi / 96;
    let x = (point.x - width + 16 * dpi / 96).clamp(work.left, (work.right - width).max(work.left));
    // Tray icons sit on the bottom edge in the common layout; flip above when
    // there is not enough room below on a side/top taskbar.
    let y = (if point.y + height <= work.bottom {
        point.y
    } else {
        point.y - height
    })
    .clamp(work.top, (work.bottom - height).max(work.top));
    invalidate_scroll(false);
    MENU_HOVER.store(-1, Ordering::Relaxed);
    MENU_MOUSE_HOVER.store(false, Ordering::Relaxed);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE,
        );
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, 18 * dpi / 96, 18 * dpi / 96);
        if !region.is_invalid() {
            let _ = SetWindowRgn(hwnd, Some(region), true);
        }
        // Positioning can synchronously deliver WM_DPICHANGED. Mark the menu
        // visible only after its final destination geometry has been applied.
        MENU_FOREGROUND.store(foreground.0 as isize, Ordering::Relaxed);
        MENU_ORIGIN_OVERFLOW.store(overflow.0 as isize, Ordering::Relaxed);
        // Zero means no pending cancellation. All menu/hook calls run on this
        // same UI thread; a posted cancellation is bound to one opening only.
        let generation = MENU_GENERATION.load(Ordering::Relaxed).wrapping_add(1).max(1);
        MENU_GENERATION.store(generation, Ordering::Relaxed);
        MENU_CANCEL_PENDING.store(0, Ordering::Relaxed);
        MENU_VISIBLE.store(true, Ordering::Relaxed);
        if !retain_menu_context(hwnd) {
            return;
        }
        // Avoid activating this popup: doing so takes foreground away from
        // Explorer and can collapse the hidden-icons flyout that launched it.
        if SetTimer(Some(hwnd), MENU_TIMER_ID, 35, None) == 0 {
            dismiss_menu(hwnd);
            return;
        }
        let sink = match create_menu_keyboard(hwnd) {
            Ok(sink) => sink,
            Err(error) => {
                log::debug!("Could not create tray menu keyboard receiver: {error:#}");
                dismiss_menu(hwnd);
                return;
            }
        };
        MENU_KEY_SINK.store(sink.0 as isize, Ordering::Relaxed);
        MENU_KEY_OWNER.store(hwnd.0 as isize, Ordering::Relaxed);
        MENU_KEYS_READY.store(install_menu_keyboard(), Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if !retain_menu_context(hwnd) {
            return;
        }
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

unsafe fn dismiss_menu(hwnd: HWND) {
    MENU_CANCEL_PENDING.store(0, Ordering::Relaxed);
    MENU_FOREGROUND.store(0, Ordering::Relaxed);
    MENU_ORIGIN_OVERFLOW.store(0, Ordering::Relaxed);
    MENU_KEYS_READY.store(false, Ordering::Relaxed);
    MENU_KEY_OWNER.store(0, Ordering::Relaxed);
    let was_visible = MENU_VISIBLE.swap(false, Ordering::Relaxed);
    let sink = HWND(MENU_KEY_SINK.swap(0, Ordering::Relaxed) as *mut std::ffi::c_void);
    // Retire both flags before any user32 call can synchronously reenter.
    // WM_NCDESTROY from this retired receiver cannot close the next opening.
    retire_menu_keyboard();
    if was_visible {
        unsafe {
            let _ = KillTimer(Some(hwnd), MENU_TIMER_ID);
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    if !sink.0.is_null() {
        unsafe { let _ = DestroyWindow(sink); }
    }
}

/// Observe the physical edge rather than sampling button state on a timer.
/// Only bounded user32 calls and a posted wake are allowed on this hook path.
fn observe_menu_button(message: u32, point: POINT) {
    if !matches!(message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN)
        || !MENU_VISIBLE.load(Ordering::Relaxed)
    {
        return;
    }
    let hwnd = SCROLL_STATE.with(|state| state.try_borrow().ok().map(|state| state.hwnd));
    let Some(hwnd) = hwnd.filter(|hwnd| !hwnd.0.is_null()) else { return; };
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() && point_in(rect, point) {
        return;
    }
    let generation = MENU_GENERATION.load(Ordering::Relaxed);
    if generation == 0 || MENU_CANCEL_PENDING.load(Ordering::Relaxed) == generation {
        return;
    }
    MENU_CANCEL_PENDING.store(generation, Ordering::Relaxed);
    // If posting fails, keep cancellation latched: the already-running menu
    // timer or next key/click handler performs cleanup and rejects commands.
    // The hook still passes this mouse event through in either case.
    let _ = unsafe { PostMessageW(Some(hwnd), MENU_CANCEL, WPARAM(generation as usize), LPARAM(0)) };
}

fn menu_context_valid() -> bool {
    let owner = HWND(MENU_FOREGROUND.load(Ordering::Relaxed) as *mut std::ffi::c_void);
    let overflow = HWND(MENU_ORIGIN_OVERFLOW.load(Ordering::Relaxed) as *mut std::ffi::c_void);
    MENU_VISIBLE.load(Ordering::Relaxed)
        && MENU_CANCEL_PENDING.load(Ordering::Relaxed) == 0
        && !owner.0.is_null()
        && unsafe { GetForegroundWindow() } == owner
        && unsafe { IsWindow(Some(owner)) }.as_bool()
        && unsafe { IsWindowVisible(owner) }.as_bool()
        && (overflow.0.is_null()
            || (unsafe { IsWindow(Some(overflow)) }.as_bool()
                && unsafe { IsWindowVisible(overflow) }.as_bool()))
}

fn retain_menu_context(hwnd: HWND) -> bool {
    let valid = menu_context_valid();
    if !valid {
        unsafe { dismiss_menu(hwnd) };
    }
    valid
}

fn handle_menu_key(hwnd: HWND, key: u32) {
    if !retain_menu_context(hwnd) {
        return;
    }
    MENU_MOUSE_HOVER.store(false, Ordering::Relaxed);
    let current = MENU_HOVER.load(Ordering::Relaxed);
    match key {
        0x1B => unsafe {
            dismiss_menu(hwnd);
        },
        0x26 | 0x28 | 0x24 | 0x23 => {
            let next = match key {
                0x26 => menu_step(current, false),
                0x28 => menu_step(current, true),
                0x24 => 0,
                _ => 5,
            };
            MENU_HOVER.store(next, Ordering::Relaxed);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
        }
        0x0D | 0x20 => {
            unsafe {
                dismiss_menu(hwnd);
            }
            dispatch_menu_action(current);
        }
        _ => (),
    }
}

/// Paint layout and hit testing use these same scaled row edges; only the
/// decorative fill is inset. Scaling edges (rather than unscaling pointer
/// coordinates) avoids a 1px drift at 125/150%.
fn menu_row_rect(row: i32, dpi: i32, width: i32) -> RECT {
    let scale = |value: i32| value * dpi / 96;
    let top = MENU_HEADER
        + row * MENU_ROW
        + if row >= 2 { MENU_GAP } else { 0 }
        + if row >= 5 { MENU_GAP } else { 0 };
    RECT {
        left: scale(1),
        top: scale(top),
        right: width - scale(1),
        bottom: scale(top + MENU_ROW),
    }
}
fn menu_row_at(x: i32, y: i32) -> isize {
    let dpi = MENU_DPI.load(Ordering::Relaxed).max(96) as i32;
    menu_hit_test(x, y, dpi)
}
fn menu_hit_test(x: i32, y: i32, dpi: i32) -> isize {
    (0..6)
        .find(|row| {
            point_in(
                menu_row_rect(*row, dpi, MENU_WIDTH * dpi / 96),
                POINT { x, y },
            )
        })
        .map(|row| row as isize)
        .unwrap_or(-1)
}
fn client_x(point: LPARAM) -> i32 {
    point.0 as i16 as i32
}

fn client_y(point: LPARAM) -> i32 {
    ((point.0 >> 16) as i16) as i32
}

fn menu_step(current: isize, forward: bool) -> isize {
    let direction = if forward { 1 } else { -1 };
    if current < 0 {
        if forward {
            0
        } else {
            5
        }
    } else {
        (current + direction + 6) % 6
    }
}

fn playback_action() -> Action {
    if ACTIVE.load(Ordering::Relaxed) { Action::Pause } else { Action::Start }
}

fn dispatch_menu_action(row: isize) {
    match row {
        0 => send(Action::Show),
        1 => send(playback_action()),
        2 => send(Action::Target(if WINDOWS_MODE.load(Ordering::Relaxed) {
            20
        } else {
            65
        })),
        3 => send(Action::Target(if WINDOWS_MODE.load(Ordering::Relaxed) {
            50
        } else {
            100
        })),
        4 => send(Action::Target(if WINDOWS_MODE.load(Ordering::Relaxed) {
            100
        } else {
            140
        })),
        5 => send(Action::Exit),
        _ => (),
    }
}

const fn rgb(value: u32) -> COLORREF {
    COLORREF(((value & 0xff) << 16) | (value & 0xff00) | ((value >> 16) & 0xff))
}
const MENU_BG: COLORREF = rgb(0x090e22);
const MENU_SELECTED_FILL: COLORREF = rgb(0x1b2c46);
const MENU_ACCENT: COLORREF = rgb(0x74aaf1);
const MENU_TEXT: COLORREF = rgb(0xeff5ff);
const MENU_SECONDARY: COLORREF = rgb(0xa4b6d2);
const MENU_HOVER_FILL: COLORREF = rgb(0x233853);
unsafe fn paint_menu(hwnd: HWND) {
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut ps) };
    let mut client = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
    }
    let dpi = MENU_DPI.load(Ordering::Relaxed).max(96) as i32;
    unsafe {
        draw_menu(
            dc,
            client.right - client.left,
            client.bottom - client.top,
            dpi,
        );
        let _ = EndPaint(hwnd, &ps);
    }
}
/// Shared by the native HWND and the explicitly requested offscreen diagnostic.
unsafe fn draw_menu(dc: HDC, width: i32, height: i32, dpi: i32) {
    let scale = |v: i32| v * dpi / 96;
    unsafe {
        crate::osd::draw_depth_surface(dc, width, height, scale(18), scale(1).max(1));
        let _ = SetBkMode(dc, BACKGROUND_MODE(1));
    }
    let en = ENGLISH.load(Ordering::Relaxed);
    let face = wide("Vazir");
    let title_font = unsafe {
        CreateFontW(
            -scale(14),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            Default::default(),
            Default::default(),
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    };
    let normal_font = unsafe {
        CreateFontW(
            -scale(12),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            Default::default(),
            Default::default(),
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    };
    let status_font = unsafe {
        CreateFontW(
            -scale(11),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            Default::default(),
            Default::default(),
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    };
    let prior = unsafe { SelectObject(dc, title_font.into()) };
    unsafe {
        let _ = SetTextColor(dc, MENU_TEXT);
    }
    let title = if en {
        "MAXI SOUNDSET"
    } else {
        "مکسی سوندست"
    };
    draw_menu_text(
        dc,
        title,
        RECT {
            left: scale(12),
            top: scale(6),
            right: width - scale(12),
            bottom: scale(28),
        },
        en,
    );
    unsafe {
        let _ = SelectObject(dc, status_font.into());
        let _ = SetTextColor(dc, MENU_SECONDARY);
    }
    let active = ACTIVE.load(Ordering::Relaxed);
    let paused = PAUSED.load(Ordering::Relaxed);
    let target = TARGET.load(Ordering::Relaxed) as i32;
    let status = match (en, active, paused) {
        (true, false, _) => "Stopped",
        (true, true, true) => "Paused",
        (true, true, false) => "Active",
        (false, false, _) => "متوقف",
        (false, true, true) => "مکث",
        (false, true, false) => "فعال",
    };
    let subtitle = if en {
        format!("{status} · Target {target}")
    } else {
        format!("{status} · هدف {}", persian_number(target))
    };
    draw_menu_text(
        dc,
        &subtitle,
        RECT {
            left: scale(12),
            top: scale(29),
            right: width - scale(12),
            bottom: scale(47),
        },
        en,
    );
    let presets = if WINDOWS_MODE.load(Ordering::Relaxed) {
        [20, 50, 100]
    } else {
        [65, 100, 140]
    };
    let names: [String; 6] = if en {
        [
            "Show dashboard".into(),
            if !active { "Run" } else if paused { "Resume" } else { "Pause" }.into(),
            format!("Quiet · {}", presets[0]),
            format!("Daily · {}", presets[1]),
            format!("Loud · {}", presets[2]),
            "Exit app".into(),
        ]
    } else {
        [
            "نمایش داشبورد".into(),
            if !active { "اجرا" } else if paused { "ادامه" } else { "مکث" }.into(),
            format!("آرام · {}", persian_number(presets[0])),
            format!("روزمره · {}", persian_number(presets[1])),
            format!("بلند · {}", persian_number(presets[2])),
            "خروج از برنامه".into(),
        ]
    };
    unsafe {
        let _ = SelectObject(dc, normal_font.into());
    }
    let selected = presets
        .iter()
        .position(|value| *value == target)
        .map(|i| i + 2);
    for row in 0..6 {
        let rect = menu_row_rect(row, dpi, width);
        let selected_row = selected == Some(row as usize);
        let hover = MENU_HOVER.load(Ordering::Relaxed) == row as isize;
        if hover || selected_row {
            let fill = CreateSolidBrush(if hover { MENU_HOVER_FILL } else { MENU_SELECTED_FILL });
            // Paint is inset for breathing room; the shared full row rectangle
            // deliberately remains the accessible mouse/keyboard hit target.
            unsafe {
                let old_fill = SelectObject(dc, fill.into());
                let old_outline = SelectObject(dc, windows::Win32::Graphics::Gdi::GetStockObject(windows::Win32::Graphics::Gdi::NULL_PEN));
                let _ = RoundRect(dc, scale(6), rect.top + scale(2), width - scale(6), rect.bottom - scale(2), scale(10), scale(10));
                let _ = SelectObject(dc, old_fill);
                let _ = SelectObject(dc, old_outline);
                let _ = DeleteObject(fill.into());
            }
        }
        unsafe {
            let _ = SetTextColor(dc, MENU_TEXT);
        }
        let icon_x = if en {
            rect.left + scale(20)
        } else {
            rect.right - scale(20)
        };
        draw_row_icon(
            dc,
            row,
            icon_x,
            (rect.top + rect.bottom) / 2,
            scale,
            if hover || selected_row { MENU_ACCENT } else { MENU_SECONDARY },
        );
        draw_menu_text(
            dc,
            &names[row as usize],
            RECT {
                left: rect.left + scale(36),
                top: rect.top,
                right: rect.right - scale(36),
                bottom: rect.bottom,
            },
            en,
        );
        if selected_row {
            let check_x = if en {
                rect.right - scale(20)
            } else {
                rect.left + scale(20)
            };
            let check_y = (rect.top + rect.bottom) / 2;
            let pen = CreatePen(PS_SOLID, ((scale(3) + 1) / 2).max(1), MENU_ACCENT);
            let previous = unsafe { SelectObject(dc, pen.into()) };
            let points = [
                POINT {
                    x: check_x - scale(4),
                    y: check_y,
                },
                POINT {
                    x: check_x - scale(1),
                    y: check_y + scale(3),
                },
                POINT {
                    x: check_x + scale(5),
                    y: check_y - scale(4),
                },
            ];
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::Polyline(dc, &points);
                let _ = SelectObject(dc, previous);
                let _ = DeleteObject(pen.into());
            }
        }
    }
    let sep_y = scale(MENU_HEADER + MENU_ROW * 5 + MENU_GAP + MENU_GAP / 2);
    let divider = CreatePen(PS_SOLID, scale(1).max(1), rgb(0x1c2c46));
    let oldp = unsafe { SelectObject(dc, divider.into()) };
    unsafe {
        let _ = MoveToEx(dc, scale(12), sep_y, None);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, width - scale(12), sep_y);
        let _ = SelectObject(dc, oldp);
        let _ = DeleteObject(divider.into());
        let _ = SelectObject(dc, prior);
        let _ = DeleteObject(title_font.into());
        let _ = DeleteObject(normal_font.into());
        let _ = DeleteObject(status_font.into());
    }
}
fn persian_number(value: i32) -> String {
    value
        .to_string()
        .chars()
        .map(|ch| match ch {
            '0'..='9' => char::from_u32('۰' as u32 + ch as u32 - '0' as u32).unwrap(),
            other => other,
        })
        .collect()
}

unsafe fn draw_menu_text(dc: HDC, value: &str, mut rect: RECT, en: bool) {
    let mut text: Vec<u16> = value.encode_utf16().collect();
    if text.is_empty() {
        return;
    }
    #[cfg(test)]
    {
        let mut extent = windows::Win32::Foundation::SIZE::default();
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::GetTextExtentPoint32W(dc, &text, &mut extent);
        }
        assert!(
            extent.cx <= rect.right - rect.left && extent.cy <= rect.bottom - rect.top,
            "Tray text would clip: {value} ({}×{} in {}×{})",
            extent.cx,
            extent.cy,
            rect.right - rect.left,
            rect.bottom - rect.top
        );
    }
    let flags = DT_SINGLELINE
        | DT_VCENTER
        | if en {
            DT_LEFT
        } else {
            DT_RIGHT | DT_RTLREADING
        };
    unsafe {
        let _ = DrawTextW(dc, &mut text, &mut rect, flags);
    }
}

/// All glyphs occupy a 16-DIP leading lane. Small open paths and consistent
/// strokes retain clear silhouettes without font fallback or overlapping rings.
unsafe fn draw_row_icon(
    dc: HDC,
    row: i32,
    cx: i32,
    cy: i32,
    scale: impl Fn(i32) -> i32,
    color: COLORREF,
) {
    let pen = CreatePen(PS_SOLID, ((scale(3) + 1) / 2).max(1), color);
    let old = unsafe { SelectObject(dc, pen.into()) };
    let point = |x: i32, y: i32| POINT {
        x: cx + x.signum() * scale(x.abs()),
        y: cy + y.signum() * scale(y.abs()),
    };
    let path = |values: &[(i32, i32)]| {
        let points: Vec<POINT> = values.iter().map(|&(x, y)| point(x, y)).collect();
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::Polyline(dc, &points);
        }
    };
    match row {
        0 => {
            path(&[(-7, -6), (7, -6), (7, 6), (-7, 6), (-7, -6)]);
            path(&[(-7, -2), (7, -2)]);
        }
        1 if ACTIVE.load(Ordering::Relaxed) && !PAUSED.load(Ordering::Relaxed) => {
            path(&[(-3, -6), (-3, 6)]);
            path(&[(3, -6), (3, 6)]);
        }
        1 => path(&[(-4, -6), (5, 0), (-4, 6), (-4, -6)]),
        2..=4 => {
            path(&[
                (-8, -3),
                (-5, -3),
                (-2, -6),
                (-2, 6),
                (-5, 3),
                (-8, 3),
                (-8, -3),
            ]);
            let near = [point(1, -3), point(4, -2), point(4, 2), point(1, 3)];
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::PolyBezier(dc, &near);
            }
            if row >= 3 {
                let far = [point(4, -6), point(8, -4), point(8, 4), point(4, 6)];
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::PolyBezier(dc, &far);
                }
            }
        }
        _ => {
            let direction = if ENGLISH.load(Ordering::Relaxed) {
                1
            } else {
                -1
            };
            path(&[
                (-direction, -7),
                (-6 * direction, -7),
                (-6 * direction, 7),
                (-direction, 7),
            ]);
            path(&[(-2 * direction, 0), (7 * direction, 0)]);
            path(&[(4 * direction, -3), (7 * direction, 0), (4 * direction, 3)]);
        }
    }
    unsafe {
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(pen.into());
    }
}

pub(crate) fn own_window() -> Option<HWND> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_keyboard_policy_preserves_edges_modifiers_and_shortcuts() {
        for (_, key) in MENU_KEYS {
            for physical_modifiers in 0..16 {
                let expected = if physical_modifiers == 0 { menu_key_bit(key) } else { 0 };
                assert_eq!(menu_shortcut_key_bit(physical_modifiers, key), expected);
                assert_eq!(menu_shortcut_key_bit(physical_modifiers | MOD_NOREPEAT_VALUE, key), expected);
            }
            assert_eq!(menu_shortcut_key_bit(0x8000, key), 0);
            let mut policy = MenuKeyPolicy::default();
            assert_eq!(policy.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Post);
            policy.posted(key, true);
            assert_eq!(policy.input(WM_KEYDOWN, key, true, 0), MenuKeyDecision::Pass);
            assert_eq!(policy.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Consume);
            assert_eq!(policy.input(WM_SYSKEYUP, key, false, 0), MenuKeyDecision::Pass);
            assert_eq!(policy.held & menu_key_bit(key), 0);
            assert_eq!(policy.owned & menu_key_bit(key), 0);
            assert_eq!(policy.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Post);
            policy.posted(key, true);
            assert_eq!(policy.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Consume);
            assert_eq!(policy.input(WM_KEYUP, key, false, 0), MenuKeyDecision::Consume);
            assert_eq!(policy.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Post);
            let mut passed = MenuKeyPolicy::default();
            assert_eq!(passed.input(WM_KEYDOWN, key, true, 0), MenuKeyDecision::Pass);
            assert_eq!(passed.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Pass);
            assert_eq!(passed.input(WM_KEYUP, key, false, 0), MenuKeyDecision::Pass);
            assert_eq!(passed.input(WM_SYSKEYDOWN, key, false, 0), MenuKeyDecision::Pass);
            assert_eq!(passed.input(WM_SYSKEYUP, key, false, 0), MenuKeyDecision::Pass);
            let bit = menu_key_bit(key);
            let mut held = MenuKeyPolicy { held: bit, owned: 0 };
            assert_eq!(held.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Pass);
            assert_eq!(held.input(WM_KEYUP, key, false, 0), MenuKeyDecision::Pass);
            assert_eq!(held.input(WM_KEYDOWN, key, false, bit), MenuKeyDecision::Pass);
            assert_eq!(held.input(WM_KEYUP, key, false, bit), MenuKeyDecision::Pass);
            let mut failed = MenuKeyPolicy::default();
            assert_eq!(failed.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Post);
            failed.posted(key, false);
            assert_eq!(failed.input(WM_KEYDOWN, key, false, 0), MenuKeyDecision::Pass);
            assert_eq!(failed.input(WM_KEYUP, key, false, 0), MenuKeyDecision::Pass);
        }
        // Letters, OEM dead-key candidates, IME, modifiers, Windows and function
        // keys never belong to this policy; it never translates text input.
        for key in [0x41, 0xDE, 0xC0, 0xE5, 0x10, 0x11, 0x12, 0xA0, 0xA5, 0x5B, 0x5C, 0x70] {
            assert_eq!(menu_shortcut_key_bit(0, key), 0);
            assert_eq!(menu_shortcut_key_bit(MOD_NOREPEAT_VALUE, key), 0);
            for message in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
                assert_eq!(MenuKeyPolicy::default().input(message, key, false, 0), MenuKeyDecision::Pass);
            }
        }
    }

    #[test]
    fn menu_keyboard_hook_propagates_next_without_reading_negative_payload() {
        assert_eq!(keyboard_hook_result(-1, || panic!("negative code read payload"), || LRESULT(9321)).0, 9321);
        assert_eq!(keyboard_hook_result(0, || false, || LRESULT(-71)).0, -71);
        assert_eq!(keyboard_hook_result(0, || true, || panic!("consumed input reached next hook")).0, 1);
    }

    #[test]
    fn menu_keyboard_modifier_combinations_pass_without_querying_navigation_state() {
        let modifiers = [0x10, 0x11, 0x12, 0x5B, 0x5C];
        for mask in 0..32 {
            let modified = menu_key_modified(0, |key| {
                let index = modifiers.iter().position(|candidate| *candidate == key)
                    .expect("Navigation key state queried inside callback");
                mask & (1 << index) != 0
            });
            assert_eq!(modified, mask != 0);
        }
        assert!(menu_key_modified(LLKHF_ALTDOWN.0, |_| panic!("Alt context requires no state query")));
    }

    /// Hidden native receivers and synthetic callback decisions. No real popup,
    /// common-key registrations, input injection, tray icon, or audio is used.
    /// The one real keyboard hook is installed only while the menu is invisible
    /// and has no foreground owner, so it cannot consume actual desktop input.
    #[test]
    #[ignore = "hidden native menu receivers and invalid-context keyboard hook lifecycle"]
    fn native_menu_context_lifecycle_probe() -> Result<()> {
        struct Probe { owner: HWND, instance: windows::Win32::Foundation::HINSTANCE, old_tx: Option<Sender<Action>> }
        impl Drop for Probe {
            fn drop(&mut self) {
                unsafe {
                    dismiss_menu(self.owner);
                    let _ = DestroyWindow(self.owner);
                    let class = wide("MaxiSoundSet.NativeTrayWindow");
                    let _ = UnregisterClassW(PCWSTR(class.as_ptr()), Some(self.instance));
                }
                *TX.lock().unwrap_or_else(|p| p.into_inner()) = self.old_tx.take();
            }
        }
        let instance = unsafe { GetModuleHandleW(None)? };
        let class = wide("MaxiSoundSet.NativeTrayWindow");
        let atom = unsafe { RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(procedure),
            hInstance: instance.into(), lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() }) };
        anyhow::ensure!(atom != 0, "Probe window class already exists or cannot be registered");
        let owner = unsafe { CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()), PCWSTR::null(), WS_POPUP,
            -32000, -32000, 16, 16, None, None, Some(instance.into()), None)? };
        let (tx, events) = mpsc::channel();
        let old_tx = TX.lock().unwrap_or_else(|p| p.into_inner()).replace(tx);
        let probe = Probe { owner, instance: instance.into(), old_tx };
        unsafe { dismiss_menu(owner); }
        assert!(!MENU_VISIBLE.load(Ordering::Relaxed));
        assert_eq!(MENU_FOREGROUND.load(Ordering::Relaxed), 0);
        if install_menu_keyboard() {
            let input = KBDLLHOOKSTRUCT { vkCode: 0x0D, ..Default::default() };
            assert!(!accept_menu_keyboard(WM_KEYDOWN, &input));
            assert!(!MENU_KEYBOARD_ARMED.load(Ordering::Relaxed));
            anyhow::ensure!(retire_menu_keyboard(), "Invalid-context native hook could not be retired");
        } else {
            eprintln!("Native keyboard hook unavailable; synthetic failure and policy checks still run");
        }
        // Exercise real install/remove failure control flow without registering
        // an actual consuming hook. The fake handle never reaches user32.
        let fake = HHOOK(1usize as *mut std::ffi::c_void);
        assert!(!install_menu_keyboard_with(|_| panic!("unexpected old hook"), || None, || 0));
        assert!(!MENU_KEYBOARD_ARMED.load(Ordering::Relaxed));
        assert!(install_menu_keyboard_with(|_| true, || Some(fake), || menu_key_bit(0x28)));
        assert!(!retire_menu_keyboard_with(|hook| { assert_eq!(hook, fake); false }));
        assert!(!MENU_KEYBOARD_ARMED.load(Ordering::Relaxed));
        assert!(!install_menu_keyboard_with(|_| false, || panic!("duplicate hook installed"), || 0));
        assert!(retire_menu_keyboard_with(|_| true));
        anyhow::ensure!(!unsafe { GetForegroundWindow() }.0.is_null(), "Probe requires an existing foreground window");
        let open = || -> Result<HWND> {
            assert!(MENU_KEYBOARD.with(|state| state.borrow().hook.is_none()));
            let sink = unsafe { create_menu_keyboard(owner)? };
            let generation = MENU_GENERATION.load(Ordering::Relaxed).wrapping_add(1).max(1);
            MENU_GENERATION.store(generation, Ordering::Relaxed);
            MENU_CANCEL_PENDING.store(0, Ordering::Relaxed);
            MENU_FOREGROUND.store(unsafe { GetForegroundWindow() }.0 as isize, Ordering::Relaxed);
            MENU_ORIGIN_OVERFLOW.store(0, Ordering::Relaxed);
            MENU_KEY_SINK.store(sink.0 as isize, Ordering::Relaxed);
            MENU_KEY_OWNER.store(owner.0 as isize, Ordering::Relaxed);
            MENU_VISIBLE.store(true, Ordering::Relaxed);
            // No native hook is present: only this test invokes the facade.
            MENU_KEYBOARD.with(|state| state.borrow_mut().policy = MenuKeyPolicy::default());
            MENU_KEYBOARD_ARMED.store(true, Ordering::Relaxed);
            Ok(sink)
        };
        let closed = || {
            assert!(!MENU_VISIBLE.load(Ordering::Relaxed));
            assert!(!MENU_KEYBOARD_ARMED.load(Ordering::Relaxed));
            assert_eq!(MENU_KEY_SINK.load(Ordering::Relaxed), 0);
            assert_eq!(MENU_KEY_OWNER.load(Ordering::Relaxed), 0);
            assert_eq!(MENU_FOREGROUND.load(Ordering::Relaxed), 0);
            assert_eq!(MENU_ORIGIN_OVERFLOW.load(Ordering::Relaxed), 0);
            assert_eq!(MENU_CANCEL_PENDING.load(Ordering::Relaxed), 0);
            assert!(events.try_recv().is_err(), "Stale callback dispatched an action");
        };
        let deliver = |sink, key, generation| unsafe {
            SendMessageW(sink, MENU_KEY, Some(WPARAM(key)), Some(LPARAM(generation as isize)))
        };
        let a = open()?;
        let generation_a = MENU_GENERATION.load(Ordering::Relaxed);
        unsafe { PostMessageW(Some(a), MENU_KEY, WPARAM(0x0D), LPARAM(generation_a as isize))?; }
        let mut held = MSG::default();
        assert!(unsafe { PeekMessageW(&mut held, Some(a), MENU_KEY, MENU_KEY, PM_REMOVE) }.as_bool());
        unsafe { dismiss_menu(owner); }
        assert!(!unsafe { IsWindow(Some(a)) }.as_bool());
        let b = open()?;
        let generation_b = MENU_GENERATION.load(Ordering::Relaxed);
        MENU_HOVER.store(3, Ordering::Relaxed);
        deliver(b, 0x0D, generation_a); // Also protects hypothetical HWND reuse.
        unsafe { DispatchMessageW(&held); }
        assert!(MENU_VISIBLE.load(Ordering::Relaxed));
        assert!(events.try_recv().is_err());
        WINDOWS_MODE.store(true, Ordering::Relaxed);
        for key in [0x24, 0x28, 0x28, 0x28] { deliver(b, key, generation_b); }
        assert_eq!(MENU_HOVER.load(Ordering::Relaxed), 3);
        deliver(b, 0x0D, generation_b);
        assert!(matches!(events.try_recv(), Ok(Action::Target(50))));
        closed();
        for owner_value in [0, -1, owner.0 as isize] {
            let sink = open()?;
            MENU_FOREGROUND.store(owner_value, Ordering::Relaxed);
            assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x0D, menu_context_valid(), false,
                |_, _, _| panic!("invalid context posted navigation")));
            deliver(sink, 0x0D, MENU_GENERATION.load(Ordering::Relaxed));
            closed();
        }
        // Hidden or destroyed synthetic overflow invalidates the current key
        // before any timer callback has been dispatched.
        for destroy in [false, true] {
            let overflow = unsafe { CreateWindowExW(WS_EX_NOACTIVATE,
                PCWSTR(class.as_ptr()), PCWSTR::null(), WS_POPUP,
                -32000, -32000, 16, 16, None, None, Some(instance.into()), None)? };
            let sink = open()?;
            MENU_ORIGIN_OVERFLOW.store(overflow.0 as isize, Ordering::Relaxed);
            if destroy { unsafe { DestroyWindow(overflow)?; } }
            assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x20, menu_context_valid(), false, |_, _, _| false));
            deliver(sink, 0x20, MENU_GENERATION.load(Ordering::Relaxed));
            closed();
            if !destroy { unsafe { DestroyWindow(overflow)?; } }
        }
        let sink = open()?;
        MENU_CANCEL_PENDING.store(MENU_GENERATION.load(Ordering::Relaxed), Ordering::Relaxed);
        assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x0D, menu_context_valid(), false, |_, _, _| false));
        deliver(sink, 0x0D, MENU_GENERATION.load(Ordering::Relaxed));
        closed();
        // Cached app-owned bare shortcuts pass through the actual callback
        // facade. No real common-key registration is necessary for this check.
        let _sink = open()?;
        let excluded = MENU_SHORTCUT_KEYS.swap(menu_key_bit(0x0D), Ordering::Relaxed);
        assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x0D, true, false,
            |_, _, _| panic!("permanent bare shortcut was intercepted")));
        MENU_SHORTCUT_KEYS.store(excluded, Ordering::Relaxed);
        unsafe { dismiss_menu(owner); }
        closed();
        // Preserve the existing physical-edge observer regression without
        // installing a mouse hook or generating any desktop mouse events.
        for (down, up) in [(WM_LBUTTONDOWN, WM_LBUTTONUP), (WM_RBUTTONDOWN, WM_RBUTTONUP),
            (WM_MBUTTONDOWN, WM_MBUTTONUP), (WM_XBUTTONDOWN, WM_XBUTTONUP)] {
            let sink = open()?;
            let generation = MENU_GENERATION.load(Ordering::Relaxed);
            let saved = SCROLL_STATE.with(|state| std::mem::replace(&mut state.borrow_mut().hwnd, owner));
            observe_menu_button(down, POINT { x: 0, y: 0 });
            observe_menu_button(up, POINT { x: 0, y: 0 });
            SCROLL_STATE.with(|state| state.borrow_mut().hwnd = saved);
            assert_eq!(MENU_CANCEL_PENDING.load(Ordering::Relaxed), generation);
            assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x0D, menu_context_valid(), false, |_, _, _| false));
            deliver(sink, 0x0D, generation);
            closed();
            open()?;
            unsafe { SendMessageW(owner, MENU_CANCEL, Some(WPARAM(generation as usize)), Some(LPARAM(0))); }
            assert!(MENU_VISIBLE.load(Ordering::Relaxed));
            unsafe { dismiss_menu(owner); }
            closed();
        }
        let sink = open()?;
        MENU_FOREGROUND.store(0, Ordering::Relaxed);
        unsafe { SendMessageW(owner, WM_TIMER, Some(WPARAM(MENU_TIMER_ID)), Some(LPARAM(0))); }
        assert!(!unsafe { IsWindow(Some(sink)) }.as_bool());
        closed();
        let sink = open()?;
        assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x28, true, false, |_, _, _| false));
        assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x28, true, false, |_, _, _| panic!("failed post repeat recaptured")));
        assert!(!MENU_KEYBOARD_ARMED.load(Ordering::Relaxed));
        deliver(sink, 0x0D, MENU_GENERATION.load(Ordering::Relaxed));
        closed();
        let sink = open()?;
        MENU_KEYBOARD.with(|state| {
            let _borrow = state.borrow_mut();
            assert!(!accept_menu_keyboard_with(WM_KEYDOWN, 0x28, true, false, |_, _, _| false));
        });
        assert!(!MENU_KEYBOARD_ARMED.load(Ordering::Relaxed));
        deliver(sink, 0x0D, MENU_GENERATION.load(Ordering::Relaxed));
        closed();
        let sink = open()?;
        unsafe { DestroyWindow(sink)?; }
        closed();
        // A reserved legacy message must not become a persistent shortcut.
        let map = HOTKEY_IDS.get_or_init(|| Mutex::new(HashMap::new()));
        let previous = map.lock().unwrap().insert(MENU_KEYS[4].0, 999);
        unsafe { SendMessageW(owner, WM_HOTKEY, Some(WPARAM(MENU_KEYS[4].0 as usize)), Some(LPARAM(0))); }
        let mut map = map.lock().unwrap();
        if let Some(previous) = previous { map.insert(MENU_KEYS[4].0, previous); }
        else { map.remove(&MENU_KEYS[4].0); }
        drop(map);
        closed();
        assert!(!unsafe { IsWindowVisible(owner) }.as_bool());
        let exit_sink = open()?;
        drop(probe);
        assert!(!unsafe { IsWindow(Some(exit_sink)) }.as_bool());
        closed();
        Ok(())
    }
    #[test]
    fn playback_row_remains_keyboard_reachable_in_all_three_states() {
        for (active, paused) in [(false, false), (true, false), (true, true)] {
            ACTIVE.store(active, Ordering::Relaxed);
            PAUSED.store(paused, Ordering::Relaxed);
            assert_eq!(menu_step(0, true), 1);
            assert_eq!(menu_step(2, false), 1);
            assert_eq!(menu_step(5, true), 0);
            assert_eq!(menu_step(0, false), 5);
            let action = playback_action();
            assert!(if active { matches!(action, Action::Pause) } else { matches!(action, Action::Start) });
            for dpi in [96, 120, 144, 192] {
                let row = menu_row_rect(1, dpi, MENU_WIDTH * dpi / 96);
                assert_eq!(menu_hit_test((row.left + row.right) / 2, (row.top + row.bottom) / 2, dpi), 1);
            }
        }
        ACTIVE.store(false, Ordering::Relaxed);
        PAUSED.store(false, Ordering::Relaxed);
    }
    #[test]
    fn fine_wheel_deltas_preserve_signed_remainder_until_context_reset() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            [30, 30, 30, 30].map(|delta| wheel.push(delta)),
            [0, 0, 0, 1]
        );
        assert_eq!(wheel.push(300), 2);
        assert_eq!(wheel.remainder, 60);
        assert_eq!(wheel.push(-30), 0);
        assert_eq!(wheel.push(90), 1); // +60 -30 +90 = one standard notch.
        assert_eq!(wheel.push(-360), -3);
        wheel.push(90);
        wheel.reset(); // Leaving icon, disabling controls or changing context.
        assert_eq!(wheel.push(30), 0);
        assert_eq!(wheel.push(90), 1);
    }
    fn test_hit() -> IconHit {
        IconHit {
            rect: RECT {
                left: -20,
                top: 40,
                right: 0,
                bottom: 60,
            },
            host: HWND::default(),
            leaf: HWND::default(),
            host_rect: RECT::default(),
            refreshed: Instant::now(),
        }
    }
    #[test]
    fn directional_batches_preserve_sequential_clamps_at_both_boundaries() {
        let hit = test_hit();
        for (initial, notches, expected) in [(0, [-1, 1], 2), (100, [1, -1], 98)] {
            let mut queue = VecDeque::new();
            for steps in notches {
                append_scroll_batch(&mut queue, steps, POINT { x: -10, y: 50 }, hit, true);
            }
            assert_eq!(queue.len(), 2);
            let result = batch_actions(queue, None)
                .into_iter()
                .fold(initial, |target, action| {
                    let Action::ScrollTarget { steps, .. } = action else {
                        panic!("Unexpected command");
                    };
                    (target + steps * 2).clamp(0, 100)
                });
            assert_eq!(result, expected);
        }
        let mut queue = VecDeque::new();
        for steps in [1, 2, 3] {
            append_scroll_batch(&mut queue, steps, POINT::default(), hit, true);
        }
        assert_eq!(queue.len(), 1); // Repeated notches still produce one run.
        assert!(matches!(
            batch_actions(queue, None).as_slice(),
            [Action::ScrollTarget { steps: 6, .. }]
        ));
    }
    #[test]
    fn same_direction_wheel_runs_keep_origin_mode_through_emission() {
        let mut queue = VecDeque::new();
        for (steps, windows_mode) in [(1, true), (2, true), (4, false), (5, false)] {
            append_scroll_batch(&mut queue, steps, POINT { x: -10, y: 50 }, test_hit(), windows_mode);
        }
        assert_eq!(queue.len(), 2, "Different origin modes must not coalesce");
        assert!(matches!(batch_actions(queue, None).as_slice(), [
            Action::ScrollTarget { steps: 3, windows_mode: true, .. },
            Action::ScrollTarget { steps: 9, windows_mode: false, .. },
        ]), "Deferred actions must retain their accepted mode");
    }
    #[test]
    fn accepted_wheel_runs_precede_the_following_hotkey_command() {
        let mut queue = VecDeque::new();
        append_scroll_batch(&mut queue, 1, POINT { x: -10, y: 50 }, test_hit(), true);
        let actions = batch_actions(queue, Some(Action::Hotkey(2)));
        assert!(matches!(
            actions.as_slice(),
            [
                Action::ScrollTarget {
                    steps: 1,
                    x: -10,
                    y: 50,
                    windows_mode: true,
                },
                Action::Hotkey(2)
            ]
        ));
        let mut target = 50;
        for action in actions {
            match action {
                Action::ScrollTarget { steps, .. } => target = (target + steps * 2).clamp(0, 100),
                Action::Hotkey(2) => target = 0, // e.g. the following mute command.
                _ => unreachable!(),
            }
        }
        assert_eq!(target, 0); // Reversing wheel/hotkey delivery would leave 2.
    }
    #[test]
    fn menu_hit_testing_preserves_full_command_rows_at_fractional_dpi() {
        assert_eq!(MENU_HEIGHT, 242);
        for dpi in [96, 120, 144, 192] {
            let width = MENU_WIDTH * dpi / 96;
            for y in 0..MENU_HEIGHT * dpi / 96 {
                let expected = (0..6)
                    .find(|row| {
                        let rect = menu_row_rect(*row, dpi, width);
                        y >= rect.top && y < rect.bottom
                    })
                    .map(|row| row as isize)
                    .unwrap_or(-1);
                for x in [dpi / 96, width / 2, width - dpi / 96 - 1] {
                    assert_eq!(menu_hit_test(x, y, dpi), expected, "DPI {dpi}, ({x},{y})");
                }
                assert_eq!(menu_hit_test(-1, y, dpi), -1);
                assert_eq!(menu_hit_test(width, y, dpi), -1);
            }
        }
    }
    /// Offscreen native GDI pixels, not a physical Shell/focus/multimonitor test.
    /// Explicitly opted into by the root's validation command.
    #[test]
    #[ignore = "writes native popup evidence only when MAXI_POPUP_EVIDENCE_DIR is set"]
    fn capture_tray_menu_evidence() {
        use windows::Win32::Graphics::Gdi::*;
        let output = std::path::PathBuf::from(
            std::env::var_os("MAXI_POPUP_EVIDENCE_DIR")
                .expect("MAXI_POPUP_EVIDENCE_DIR is required"),
        );
        std::fs::create_dir_all(&output).unwrap();
        crate::osd::ensure_vazir_registered().unwrap();
        WINDOWS_MODE.store(true, Ordering::Relaxed);
        for (active, paused, suffix) in [(false, false, "-stopped"), (true, false, "-running"), (true, true, "-paused")] {
            ACTIVE.store(active, Ordering::Relaxed);
            PAUSED.store(paused, Ordering::Relaxed);
            TARGET.store(if active { 50 } else { 52 }, Ordering::Relaxed);
            MENU_HOVER.store(1, Ordering::Relaxed);
            for english in [false, true] {
                ENGLISH.store(english, Ordering::Relaxed);
                for dpi in [96, 120, 144, 192] {
                    let width = MENU_WIDTH * dpi / 96;
                    let height = MENU_HEIGHT * dpi / 96;
                    unsafe {
                        let dc = CreateCompatibleDC(None);
                        assert!(!dc.0.is_null());
                        let info = BITMAPINFO {
                            bmiHeader: BITMAPINFOHEADER {
                                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                                biWidth: width,
                                biHeight: -height,
                                biPlanes: 1,
                                biBitCount: 32,
                                biCompression: BI_RGB.0,
                                ..Default::default()
                            },
                            ..Default::default()
                        };
                        let mut bits = std::ptr::null_mut();
                        let bitmap =
                            CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                                .unwrap();
                        let old = SelectObject(dc, bitmap.into());
                        // The HWND's region rounds corners. Fill capture corners in
                        // app navy so the rectangular evidence remains easy to read.
                        let brush = CreateSolidBrush(MENU_BG);
                        let _ = FillRect(
                            dc,
                            &RECT {
                                left: 0,
                                top: 0,
                                right: width,
                                bottom: height,
                            },
                            brush,
                        );
                        let _ = DeleteObject(brush.into());
                        draw_menu(dc, width, height, dpi);
                        let _ = GdiFlush();
                        let pixels = std::slice::from_raw_parts(
                            bits as *const u8,
                            (width * height * 4) as usize,
                        );
                        crate::osd::assert_depth_surface_pixels(pixels, width, height, dpi as u32);
                        let mut rgba = Vec::with_capacity(pixels.len());
                        for pixel in pixels.chunks_exact(4) {
                            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
                        }
                        let path = output.join(format!(
                            "tray-{}-{dpi}{suffix}.png",
                            if english { "en" } else { "fa" }
                        ));
                        let mut encoder = png::Encoder::new(
                            std::io::BufWriter::new(std::fs::File::create(path).unwrap()),
                            width as u32,
                            height as u32,
                        );
                        encoder.set_color(png::ColorType::Rgba);
                        encoder.set_depth(png::BitDepth::Eight);
                        encoder
                            .write_header()
                            .unwrap()
                            .write_image_data(&rgba)
                            .unwrap();
                        let _ = SelectObject(dc, old);
                        let _ = DeleteObject(bitmap.into());
                        let _ = DeleteDC(dc);
                    }
                }
            }
        }
    }
}
