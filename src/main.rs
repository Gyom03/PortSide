#![windows_subsystem = "windows"]

mod scan;
mod ui;

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCSTR, PCWSTR, PWSTR, w};

use scan::{Cmd, Snapshot, WM_SNAPSHOT};
use ui::{Hit, Ui};

const CLASS: PCWSTR = w!("PortSide.Popup");
const WM_TRAY: u32 = WM_APP + 1;
const WM_SHOW_POPUP: u32 = WM_APP + 3;
const NIN_KEYSELECT: u32 = NIN_SELECT + 1;
const TIMER_PROMOTE: usize = 1;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("PortSide");

struct App {
    ui: Ui,
    snap: Snapshot,
    selected: HashSet<u32>,
    hover: Hit,
    pressed: Hit,
    scroll: f32,
    anchor: POINT,
    visible: bool,
    last_hide: Instant,
    tracking: bool,
    note: Option<String>,
    scanner: Sender<Cmd>,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state. Win32 calls that synchronously re-enter the window
/// procedure (ShowWindow, SetWindowPos, menus…) must happen outside of it.
fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _mutex = CreateMutexW(None, true, w!("PortSide.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(h) = FindWindowW(CLASS, None) {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let _ = PostMessageW(Some(h), WM_SHOW_POPUP, WPARAM(0), LPARAM(0));
            }
            return;
        }
        allow_dark_menus();

        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("PortSide"),
            WS_POPUP,
            0,
            0,
            ui::WIDTH as i32,
            400,
            None,
            None,
            Some(hinst),
            None,
        )
        .unwrap();

        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as _, 4);
        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let border: u32 = 0x00_36_33_30; // COLORREF 0x00BBGGRR
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as _, 4);
        // A 1px extended frame gives the borderless popup the native DWM shadow.
        let _ = DwmExtendFrameIntoClientArea(hwnd, &MARGINS { cxLeftWidth: 1, cxRightWidth: 1, cyTopHeight: 1, cyBottomHeight: 1 });

        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let app = App {
            ui: Ui::new().expect("Direct2D init"),
            snap: Snapshot::default(),
            selected: HashSet::new(),
            hover: Hit::None,
            pressed: Hit::None,
            scroll: 0.0,
            anchor: POINT::default(),
            visible: false,
            last_hide: Instant::now() - Duration::from_secs(10),
            tracking: false,
            note: None,
            scanner: scan::spawn(hwnd.0 as isize),
            taskbar_created,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        add_tray_icon(hwnd, hinst);
        SetTimer(Some(hwnd), TIMER_PROMOTE, 2000, None);

        if !std::env::args().any(|a| a == "--background") {
            let _ = PostMessageW(Some(hwnd), WM_SHOW_POPUP, WPARAM(0), LPARAM(0));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------- tray

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    for (d, s) in nid.szTip.iter_mut().zip(tip.encode_utf16().chain([0]).take(127)) {
        *d = s;
    }
}

unsafe fn add_tray_icon(hwnd: HWND, hinst: HINSTANCE) {
    unsafe {
        let dpi = GetDpiForSystem();
        let cx = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
        let cy = GetSystemMetricsForDpi(SM_CYSMICON, dpi);
        let icon = LoadImageW(Some(hinst), PCWSTR(1 as _), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icon;
        set_tip(&mut nid, "PortSide");
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
}

fn update_tray_tip(hwnd: HWND, n: usize) {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_TIP | NIF_SHOWTIP;
    let tip = match n {
        0 => "PortSide · no servers".to_string(),
        1 => "PortSide · 1 server".to_string(),
        n => format!("PortSide · {n} servers"),
    };
    set_tip(&mut nid, &tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn remove_tray_icon(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
    }
}

/// Center of our tray icon, or the bottom-right corner of the primary work area.
fn tray_anchor(hwnd: HWND) -> POINT {
    let id = NOTIFYICONIDENTIFIER { cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
    unsafe {
        if let Ok(r) = Shell_NotifyIconGetRect(&id) {
            return POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
        }
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut _ as _), Default::default());
        POINT { x: wa.right, y: wa.bottom }
    }
}

unsafe fn tray_menu(hwnd: HWND, pt: POINT) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let auto = autostart_enabled();
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("Open"));
        let _ = AppendMenuW(menu, MF_STRING | if auto { MF_CHECKED } else { MF_UNCHECKED }, 2, w!("Start with Windows"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, 3, w!("Quit"));
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        match cmd.0 {
            1 => show(hwnd, pt),
            2 => set_autostart(!auto),
            3 => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

/// Lets the tray context menu follow the system dark mode (undocumented uxtheme ordinals,
/// silently skipped when unavailable).
fn allow_dark_menus() {
    unsafe {
        let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(ux, PCSTR(135 as _)) {
            let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(1);
        }
        if let Some(f) = GetProcAddress(ux, PCSTR(136 as _)) {
            let flush_menu_themes: extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

/// Windows 11 parks new tray icons in the overflow menu. Show ours on the taskbar,
/// unless the user already made a choice for it (IsPromoted present).
fn promote_tray_icon() {
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!("Control Panel\\NotifyIconSettings"), None, KEY_READ | KEY_SET_VALUE, &mut root) != ERROR_SUCCESS {
            return;
        }
        let mut name = [0u16; 128];
        for i in 0.. {
            let mut len = name.len() as u32;
            if RegEnumKeyExW(root, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) != ERROR_SUCCESS {
                break;
            }
            let sub = PCWSTR(name.as_ptr());
            let mut path = [0u16; 1024];
            let mut cb = (path.len() * 2) as u32;
            if RegGetValueW(root, sub, w!("ExecutablePath"), RRF_RT_REG_SZ, None, Some(path.as_mut_ptr() as _), Some(&mut cb)) != ERROR_SUCCESS {
                continue;
            }
            let path = String::from_utf16_lossy(&path[..(cb as usize / 2).saturating_sub(1)]).to_lowercase();
            if !path.ends_with("\\portside.exe") {
                continue;
            }
            if RegGetValueW(root, sub, w!("IsPromoted"), RRF_RT_REG_DWORD, None, None, None) != ERROR_SUCCESS {
                let one = 1u32;
                let _ = RegSetKeyValueW(root, sub, w!("IsPromoted"), REG_DWORD.0, Some(&one as *const u32 as _), 4);
            }
        }
        let _ = RegCloseKey(root);
    }
}

fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

fn set_autostart(on: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyW(HKEY_CURRENT_USER, RUN_KEY, &mut key) != ERROR_SUCCESS {
            return;
        }
        if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = format!("\"{}\" --background", exe.display());
            let bytes: Vec<u8> = cmd.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect();
            let _ = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(&bytes));
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

// ---------------------------------------------------------------- popup

/// Sizes the popup for the current content and places it next to the anchor, inside
/// the work area of the anchor's monitor.
fn place(hwnd: HWND) {
    let Some((anchor, dip_h)) = with(|a| (a.anchor, a.ui.height(a.snap.servers.len()))) else { return };
    unsafe {
        let mon = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96, 96);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let s = dx as f32 / 96.0;
        let w = (ui::WIDTH * s).round() as i32;
        let h = (dip_h * s).round() as i32;
        let m = (12.0 * s).round() as i32;
        let wa = mi.rcWork;
        let x = (anchor.x - w / 2).min(wa.right - w - m).max(wa.left + m);
        let y = (anchor.y - h / 2).min(wa.bottom - h - m).max(wa.top + m);
        with(|a| a.ui.set_dpi(dx as f32));
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
    }
}

fn show(hwnd: HWND, anchor: POINT) {
    with(|a| {
        a.anchor = anchor;
        a.visible = true;
        a.scroll = 0.0;
        a.hover = Hit::None;
        let _ = a.scanner.send(Cmd::Visible(true));
    });
    place(hwnd);
    unsafe {
        // Cloak while the first frame is rendered so the popup never flashes.
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &on as *const _ as _, 4);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = UpdateWindow(hwnd);
        let off = BOOL(0);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &off as *const _ as _, 4);
    }
}

fn hide(hwnd: HWND) {
    let was = with(|a| {
        let was = a.visible;
        a.visible = false;
        a.last_hide = Instant::now();
        a.selected.clear();
        a.note = None;
        a.hover = Hit::None;
        a.pressed = Hit::None;
        let _ = a.scanner.send(Cmd::Visible(false));
        was
    });
    if was == Some(true) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            // Hand the pages back to the OS while idling in the tray.
            let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        }
    }
}

fn toggle(hwnd: HWND, anchor: POINT) {
    let Some((visible, since_hide)) = with(|a| (a.visible, a.last_hide.elapsed())) else { return };
    if visible {
        hide(hwnd);
    } else if since_hide > Duration::from_millis(300) {
        // (a click on the icon while open first deactivates, hence hides, the popup)
        show(hwnd, anchor);
    }
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn mouse_dip(lparam: LPARAM) -> (f32, f32) {
    let x = (lparam.0 & 0xffff) as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as f32;
    let s = with(|a| a.ui.scale()).unwrap_or(1.0);
    (x / s, y / s)
}

fn cursor_dip(hwnd: HWND) -> (f32, f32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
        let _ = ScreenToClient(hwnd, &mut p);
    }
    let s = with(|a| a.ui.scale()).unwrap_or(1.0);
    (p.x as f32 / s, p.y as f32 / s)
}

fn set_hover(hwnd: HWND, x: f32, y: f32) {
    let changed = with(|a| {
        let h = a.ui.hit(x, y, a.snap.servers.len(), a.scroll);
        let changed = h != a.hover;
        a.hover = h;
        changed
    });
    if changed == Some(true) {
        invalidate(hwnd);
    }
}

fn open_in_browser(hwnd: HWND, i: usize) {
    let Some(Some(url)) = with(|a| {
        a.snap.servers.get(i).map(|s| {
            let p = s.ports[0];
            format!("{}://localhost:{}", if p.tls { "https" } else { "http" }, p.port)
        })
    }) else {
        return;
    };
    let wide: Vec<u16> = url.encode_utf16().chain([0]).collect();
    hide(hwnd);
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

fn stop_selected(hwnd: HWND) {
    let Some(targets) = with(|a| {
        a.snap.servers.iter().filter(|s| a.selected.contains(&s.pid)).map(|s| (s.pid, s.created, s.name.clone())).collect::<Vec<_>>()
    }) else {
        return;
    };
    if targets.is_empty() {
        return;
    }
    let failed: Vec<&(u32, u64, String)> = targets.iter().filter(|(pid, created, _)| !scan::kill(*pid, *created)).collect();
    with(|a| {
        a.snap.servers.retain(|s| !a.selected.contains(&s.pid) || failed.iter().any(|f| f.0 == s.pid));
        a.selected.clear();
        a.scroll = 0.0;
        a.note = match failed.len() {
            0 => None,
            _ => {
                let names: Vec<&str> = failed.iter().map(|f| f.2.as_str()).collect();
                Some(format!("Access denied: {} (needs admin?)", names.join(", ")))
            }
        };
        let _ = a.scanner.send(Cmd::RefreshSoon);
    });
    place(hwnd);
    invalidate(hwnd);
}

fn click(hwnd: HWND, hit: Hit) {
    match hit {
        Hit::Row(i) => {
            with(|a| {
                if let Some(s) = a.snap.servers.get(i) {
                    if !a.selected.remove(&s.pid) {
                        a.selected.insert(s.pid);
                    }
                }
                a.note = None;
            });
            invalidate(hwnd);
        }
        Hit::Port(i) => open_in_browser(hwnd, i),
        Hit::Cancel => hide(hwnd),
        Hit::Stop => stop_selected(hwnd),
        Hit::None => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                let event = (lparam.0 & 0xffff) as u32;
                let pt = POINT { x: (wparam.0 & 0xffff) as i16 as i32, y: ((wparam.0 >> 16) & 0xffff) as i16 as i32 };
                match event {
                    NIN_SELECT | NIN_KEYSELECT => toggle(hwnd, pt),
                    WM_CONTEXTMENU => tray_menu(hwnd, pt),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_SHOW_POPUP => {
                if with(|a| a.visible) == Some(false) {
                    show(hwnd, tray_anchor(hwnd));
                }
                LRESULT(0)
            }
            WM_SNAPSHOT => {
                let snap = *Box::from_raw(lparam.0 as *mut Snapshot);
                let Some((visible, resized, n)) = with(|a| {
                    let before = a.ui.height(a.snap.servers.len());
                    a.snap = snap;
                    let pids: HashSet<u32> = a.snap.servers.iter().map(|s| s.pid).collect();
                    a.selected.retain(|p| pids.contains(p));
                    let n = a.snap.servers.len();
                    a.scroll = a.scroll.min(a.ui.max_scroll(n));
                    (a.visible, a.ui.height(n) != before, n)
                }) else {
                    return LRESULT(0);
                };
                update_tray_tip(hwnd, n);
                if visible {
                    if resized {
                        place(hwnd);
                    }
                    let (x, y) = cursor_dip(hwnd);
                    set_hover(hwnd, x, y);
                    invalidate(hwnd);
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                if (wparam.0 & 0xffff) as u32 == WA_INACTIVE {
                    hide(hwnd);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                with(|a| {
                    let state = ui::State {
                        snap: &a.snap,
                        selected: &a.selected,
                        hover: a.hover,
                        pressed: a.pressed,
                        scroll: a.scroll,
                        note: a.note.as_deref(),
                    };
                    a.ui.render(hwnd, &state);
                });
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_TIMER if wparam.0 == TIMER_PROMOTE => {
                let _ = KillTimer(Some(hwnd), TIMER_PROMOTE);
                promote_tray_icon();
                if with(|a| a.visible) == Some(false) {
                    let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let (w, h) = ((lparam.0 & 0xffff) as u32, ((lparam.0 >> 16) & 0xffff) as u32);
                with(|a| a.ui.resize(w, h));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                with(|a| a.ui.set_dpi((wparam.0 & 0xffff) as f32));
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let start = with(|a| !std::mem::replace(&mut a.tracking, true));
                if start == Some(true) {
                    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut tme);
                }
                let (x, y) = mouse_dip(lparam);
                set_hover(hwnd, x, y);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                with(|a| {
                    a.tracking = false;
                    a.hover = Hit::None;
                    a.pressed = Hit::None;
                });
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = mouse_dip(lparam);
                with(|a| a.pressed = a.ui.hit(x, y, a.snap.servers.len(), a.scroll));
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = mouse_dip(lparam);
                let hit = with(|a| {
                    let h = a.ui.hit(x, y, a.snap.servers.len(), a.scroll);
                    let pressed = std::mem::replace(&mut a.pressed, Hit::None);
                    let enabled = !matches!(h, Hit::Stop) || !a.selected.is_empty();
                    if h == pressed && enabled { h } else { Hit::None }
                });
                click(hwnd, hit.unwrap_or(Hit::None));
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xffff) as i16 as f32;
                with(|a| {
                    let max = a.ui.max_scroll(a.snap.servers.len());
                    a.scroll = (a.scroll - delta / 120.0 * ui::ROW_H).clamp(0.0, max);
                });
                let (x, y) = cursor_dip(hwnd);
                set_hover(hwnd, x, y);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_SETCURSOR => {
                if (lparam.0 & 0xffff) as u32 == HTCLIENT {
                    let hand = with(|a| match a.hover {
                        Hit::Port(_) | Hit::Cancel => true,
                        Hit::Stop => !a.selected.is_empty(),
                        _ => false,
                    });
                    let id = if hand == Some(true) { IDC_HAND } else { IDC_ARROW };
                    SetCursor(LoadCursorW(None, id).ok());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_KEYDOWN => {
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                match VIRTUAL_KEY(wparam.0 as u16) {
                    VK_ESCAPE => hide(hwnd),
                    VK_RETURN => stop_selected(hwnd),
                    VK_A if ctrl => {
                        with(|a| {
                            if a.selected.len() == a.snap.servers.len() {
                                a.selected.clear();
                            } else {
                                a.selected = a.snap.servers.iter().map(|s| s.pid).collect();
                            }
                        });
                        invalidate(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                remove_tray_icon(hwnd);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => {
                if msg != 0 && Some(msg) == with(|a| a.taskbar_created) {
                    // Explorer restarted: the tray icon must be registered again.
                    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
                    add_tray_icon(hwnd, hinst);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}
