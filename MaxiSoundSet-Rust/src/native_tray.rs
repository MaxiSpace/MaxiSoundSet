//! The tray uses the existing Slint/Winit HWND, avoiding a second hidden window.
use crate::core_audio::wide;
use anyhow::{Context, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
    mpsc::{self, Receiver, Sender},
    Mutex,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NIN_SELECT, NOTIFYICONDATAW,
            },
            WindowsAndMessaging::*,
        },
    },
};
const MESSAGE: u32 = WM_APP + 741;
static PRIOR: AtomicIsize = AtomicIsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ENGLISH: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);
static PRESENT_PENDING: AtomicBool = AtomicBool::new(true);
static TX: Mutex<Option<Sender<Action>>> = Mutex::new(None);
static EXIT_MESSAGE: AtomicU32 = AtomicU32::new(0);
#[derive(Debug)]
pub enum Action {
    Show,
    Pause,
    Target(i32),
    Exit,
    Refresh,
    Repaint,
}
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    previous: isize,
    pub events: Option<Receiver<Action>>,
    registered: bool,
    tooltip: String,
}
impl Tray {
    pub fn new() -> Result<Self> {
        EXIT_MESSAGE.store(
            unsafe {
                RegisterWindowMessageW(PCWSTR(wide("MAXISOUNDSET.RequestSafeExit.v1").as_ptr()))
            },
            Ordering::Relaxed,
        );
        let hwnd =
            own_window().context("Slint native window is unavailable for tray registration")?;
        let icon = unsafe {
            LoadIconW(
                Some(GetModuleHandleW(None)?.into()),
                PCWSTR(1usize as *const u16),
            )?
        };
        let (tx, events) = mpsc::channel();
        *TX.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        let previous =
            unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, procedure as *const () as isize) };
        anyhow::ensure!(
            previous != 0,
            "Native tray window procedure could not be installed"
        );
        PRIOR.store(previous, Ordering::Relaxed);
        let mut tray = Self {
            hwnd,
            icon,
            previous,
            events: Some(events),
            registered: false,
            tooltip: "MAXI SOUNDSET · Standby".into(),
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
    pub fn update(&mut self, active: bool, paused: bool, target: i32, english: bool) {
        ENGLISH.store(english, Ordering::Relaxed);
        ACTIVE.store(active, Ordering::Relaxed);
        PAUSED.store(paused, Ordering::Relaxed);
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
        unsafe {
            if self.registered {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
            SetWindowLongPtrW(self.hwnd, GWLP_WNDPROC, self.previous);
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
    if message == MESSAGE {
        match l.0 as u32 & 0xffff {
            WM_LBUTTONUP | NIN_SELECT => send(Action::Show),
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                menu(hwnd);
            },
            _ => (),
        }
        return LRESULT(0);
    }
    // Explorer broadcasts TaskbarCreated to top-level windows after restarting.
    let taskbar = unsafe { RegisterWindowMessageW(PCWSTR(wide("TaskbarCreated").as_ptr())) };
    if taskbar != 0 && message == taskbar {
        send(Action::Refresh);
    }
    let previous = PRIOR.load(Ordering::Relaxed);
    unsafe {
        CallWindowProcW(
            Some(std::mem::transmute::<
                isize,
                unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            >(previous)),
            hwnd,
            message,
            w,
            l,
        )
    }
}
unsafe fn menu(hwnd: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let en = ENGLISH.load(Ordering::Relaxed);
    let mut items = vec![
        (
            1,
            if en {
                "Show MAXI SOUNDSET"
            } else {
                "نمایش مکسی سوند ست"
            },
        ),
        (
            2,
            if PAUSED.load(Ordering::Relaxed) {
                if en {
                    "Resume"
                } else {
                    "ادامه"
                }
            } else {
                if en {
                    "Pause"
                } else {
                    "مکث"
                }
            },
        ),
        (
            3,
            if en {
                "Quiet · 65"
            } else {
                "آرام · ۶۵"
            },
        ),
        (
            4,
            if en {
                "Daily · 100"
            } else {
                "روزمره · ۱۰۰"
            },
        ),
        (
            5,
            if en {
                "Loud · 140"
            } else {
                "بلند · ۱۴۰"
            },
        ),
        (
            6,
            if en {
                "Exit app"
            } else {
                "خروج از برنامه"
            },
        ),
    ];
    for (id, text) in items.drain(..) {
        let text = wide(text);
        let flags = MF_STRING
            | if id == 2 && !ACTIVE.load(Ordering::Relaxed) {
                MF_GRAYED
            } else {
                MENU_ITEM_FLAGS(0)
            };
        let _ = unsafe { AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr())) };
    }
    let mut point = POINT::default();
    let _ = unsafe { GetCursorPos(&mut point) };
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let command = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
    }
    .0;
    unsafe {
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    }
    match command {
        1 => send(Action::Show),
        2 => send(Action::Pause),
        3 => send(Action::Target(65)),
        4 => send(Action::Target(100)),
        5 => send(Action::Target(140)),
        6 => send(Action::Exit),
        _ => (),
    }
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
