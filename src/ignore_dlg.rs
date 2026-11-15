//! "Hidden apps" dialog: pick apps the overlay must never show for.
//! Apps can be added from the list of running apps, by typing an exe name,
//! or by browsing for an exe. The list is stored in HKCU\Software\FpsOverlay.

use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::tray;

/// Posted to the overlay window whenever the list changed.
pub const WM_IGNORE_CHANGED: u32 = WM_APP + 2;

const IDC_IGN: i32 = 201;
const IDC_RUN: i32 = 202;
const IDC_REMOVE: i32 = 203;
const IDC_ADDSEL: i32 = 204;
const IDC_REFRESH: i32 = 205;
const IDC_EDIT: i32 = 206;
const IDC_ADD: i32 = 207;
const IDC_BROWSE: i32 = 208;
const IDC_CLOSE: i32 = 209;

const SS_LEFT: i32 = 0;
const CLASS: &str = "RustFpsOverlayIgnoreDlg";

static DLG: AtomicIsize = AtomicIsize::new(0);
static OWNER: AtomicIsize = AtomicIsize::new(0);
static FONT: AtomicIsize = AtomicIsize::new(0);
static LAST_FOCUS: AtomicIsize = AtomicIsize::new(0);

// Dialog-manager constants (WM_USER + 0, "has a default id" marker).
const DM_GETDEFID: u32 = 0x0400;
const DC_HASDEFID: u32 = 0x534B;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The dialog window, or null when it is not open (used by the message loop).
pub fn hwnd() -> HWND {
    DLG.load(Ordering::Relaxed) as HWND
}

// ------------------------------------------------------------------ storage

/// User-ignored exe names (lowercase), stored `|`-separated (illegal in file names).
pub fn load() -> Vec<String> {
    tray::get_string(tray::SETTING_IGNORED)
        .unwrap_or_default()
        .split('|')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn save(list: &[String]) {
    tray::set_string(tray::SETTING_IGNORED, &list.join("|"));
    unsafe { PostMessageW(OWNER.load(Ordering::Relaxed) as HWND, WM_IGNORE_CHANGED, 0, 0) };
}

/// "C:\Games\Foo.EXE" / "foo" -> "foo.exe". None if unusable.
fn normalize(raw: &str) -> Option<String> {
    let name = raw.trim().trim_matches('"');
    let name = name.rsplit(['\\', '/']).next().unwrap_or(name).trim().to_ascii_lowercase();
    if name.is_empty() || name.contains('|') {
        return None;
    }
    // "my.game" is the file "my.game.exe", not a complete name: only ".exe" counts as an extension.
    Some(if name.ends_with(".exe") { name } else { format!("{name}.exe") })
}

// ------------------------------------------------------------------ list helpers

unsafe fn item(dlg: HWND, id: i32) -> HWND {
    GetDlgItem(dlg, id)
}

unsafe fn fill(list: HWND, entries: &[String]) {
    SendMessageW(list, LB_RESETCONTENT, 0, 0);
    for e in entries {
        SendMessageW(list, LB_ADDSTRING, 0, wide(e).as_ptr() as LPARAM);
    }
}

unsafe fn selected(list: HWND) -> Option<String> {
    let i = SendMessageW(list, LB_GETCURSEL, 0, 0);
    if i == LB_ERR as LRESULT {
        return None;
    }
    let len = SendMessageW(list, LB_GETTEXTLEN, i as WPARAM, 0);
    if len <= 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    SendMessageW(list, LB_GETTEXT, i as WPARAM, buf.as_mut_ptr() as LPARAM);
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

unsafe fn refresh_ignored(dlg: HWND) {
    fill(item(dlg, IDC_IGN), &load());
}

/// Names of apps that currently have a normal visible top-level window.
unsafe fn refresh_running(dlg: HWND) {
    unsafe extern "system" fn cb(w: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp as *mut Vec<String>);
        if IsWindowVisible(w) == 0
            || !GetWindow(w, GW_OWNER).is_null()
            || GetWindowTextLengthW(w) == 0
            || (GetWindowLongW(w, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW != 0
        {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(w, &mut pid);
        if pid != 0 && pid != GetCurrentProcessId() {
            if let Some(n) = crate::process_name(pid) {
                out.push(n.to_ascii_lowercase());
            }
        }
        1
    }
    let mut names: Vec<String> = Vec::new();
    EnumWindows(Some(cb), &mut names as *mut Vec<String> as LPARAM);
    names.sort();
    names.dedup();
    fill(item(dlg, IDC_RUN), &names);
}

unsafe fn add_name(dlg: HWND, raw: &str) {
    let Some(name) = normalize(raw) else { return };
    let mut list = load();
    if !list.contains(&name) {
        list.push(name);
        list.sort();
        save(&list);
        refresh_ignored(dlg);
    }
}

unsafe fn browse(dlg: HWND) {
    let filter = wide("Executables (*.exe)\0*.exe\0All files (*.*)\0*.*\0");
    let title = wide("Choose the app to hide the overlay for");
    let mut file = [0u16; 1024];
    let mut ofn: OPENFILENAMEW = zeroed();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = dlg;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
    if GetOpenFileNameW(&mut ofn) != 0 {
        let n = file.iter().position(|&c| c == 0).unwrap_or(file.len());
        add_name(dlg, &String::from_utf16_lossy(&file[..n]));
    }
}

// ------------------------------------------------------------------ window

unsafe extern "system" fn dlg_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = ((wp >> 16) & 0xFFFF) as u32;
            match id {
                IDC_REMOVE => {
                    if let Some(sel) = selected(item(h, IDC_IGN)) {
                        let list: Vec<String> = load().into_iter().filter(|n| *n != sel).collect();
                        save(&list);
                        refresh_ignored(h);
                    }
                }
                IDC_ADDSEL => {
                    if let Some(sel) = selected(item(h, IDC_RUN)) {
                        add_name(h, &sel);
                    }
                }
                IDC_RUN if code == LBN_DBLCLK => {
                    if let Some(sel) = selected(item(h, IDC_RUN)) {
                        add_name(h, &sel);
                    }
                }
                IDC_REFRESH => refresh_running(h),
                IDC_ADD | 1 /* IDOK: Enter */ => {
                    // Enter acts on what has the focus: a list entry, or the typed name.
                    let focus = GetFocus();
                    let focus_id = if focus.is_null() { 0 } else { GetDlgCtrlID(focus) };
                    if focus_id == IDC_RUN {
                        if let Some(sel) = selected(item(h, IDC_RUN)) {
                            add_name(h, &sel);
                        }
                    } else if focus_id != IDC_IGN {
                        let edit = item(h, IDC_EDIT);
                        let mut buf = [0u16; 512];
                        let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
                        add_name(h, &String::from_utf16_lossy(&buf[..n]));
                        SetWindowTextW(edit, wide("").as_ptr());
                    }
                }
                IDC_BROWSE => browse(h),
                IDC_CLOSE | 2 /* IDCANCEL: Esc */ => {
                    DestroyWindow(h);
                }
                _ => {}
            }
            0
        }
        // Remember which control had the focus when the dialog is deactivated, and give it
        // back on re-activation (DefWindowProc would put the focus on the frame).
        WM_ACTIVATE => {
            if wp & 0xFFFF == 0 {
                let f = GetFocus();
                if !f.is_null() && IsChild(h, f) != 0 {
                    LAST_FOCUS.store(f as isize, Ordering::Relaxed);
                }
            }
            DefWindowProcW(h, msg, wp, lp)
        }
        WM_SETFOCUS => {
            let last = LAST_FOCUS.load(Ordering::Relaxed) as HWND;
            let target = if !last.is_null() && IsWindow(last) != 0 { last } else { item(h, IDC_EDIT) };
            SetFocus(target);
            0
        }
        // "Add" is the default button (Enter), whichever button has the focus.
        DM_GETDEFID => ((DC_HASDEFID as isize) << 16) | IDC_ADD as isize,
        WM_DPICHANGED => {
            let dpi = ((wp >> 16) & 0xFFFF) as i32;
            let r = &*(lp as *const RECT);
            SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            apply_layout(h, dpi);
            0
        }
        WM_CLOSE => {
            DestroyWindow(h);
            0
        }
        WM_DESTROY => {
            DLG.store(0, Ordering::Relaxed);
            LAST_FOCUS.store(0, Ordering::Relaxed);
            0
        }
        WM_NCDESTROY => {
            // The font may only go once all child controls are gone.
            let f = FONT.swap(0, Ordering::Relaxed);
            if f != 0 {
                DeleteObject(f as HGDIOBJ);
            }
            DefWindowProcW(h, msg, wp, lp)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}


/// One child control, in 96-DPI units.
struct Ctl {
    class: &'static str,
    text: &'static str,
    style: u32,
    ex: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
}

fn controls() -> Vec<Ctl> {
    let tab = WS_TABSTOP;
    let list = WS_VSCROLL | tab | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32;
    let push = BS_PUSHBUTTON as u32 | tab;
    let label = SS_LEFT as u32;
    let c = |class, text, style, ex, x, y, w, h, id| Ctl { class, text, style, ex, x, y, w, h, id };
    vec![
        c("STATIC", "Hidden apps (no overlay for these):", label, 0, 12, 10, 240, 18, 301),
        c("LISTBOX", "", list, WS_EX_CLIENTEDGE, 12, 30, 240, 230, IDC_IGN),
        c("BUTTON", "Remove", push, 0, 12, 266, 100, 26, IDC_REMOVE),
        c("STATIC", "Running apps (double-click to hide):", label, 0, 268, 10, 240, 18, 302),
        c("LISTBOX", "", list, WS_EX_CLIENTEDGE, 268, 30, 240, 230, IDC_RUN),
        c("BUTTON", "Hide selected", push, 0, 268, 266, 110, 26, IDC_ADDSEL),
        c("BUTTON", "Refresh", push, 0, 386, 266, 80, 26, IDC_REFRESH),
        c("STATIC", "Or type an exe name, or browse for the exe:", label, 0, 12, 306, 400, 18, 303),
        c("EDIT", "", ES_AUTOHSCROLL as u32 | tab, WS_EX_CLIENTEDGE, 12, 328, 300, 24, IDC_EDIT),
        c("BUTTON", "Add", BS_DEFPUSHBUTTON as u32 | tab, 0, 320, 327, 70, 26, IDC_ADD),
        c("BUTTON", "Browse...", push, 0, 398, 327, 110, 26, IDC_BROWSE),
        c("BUTTON", "Close", push, 0, 408, 362, 100, 26, IDC_CLOSE),
    ]
}

/// Client size in 96-DPI units.
const CLIENT_W: i32 = 520;
const CLIENT_H: i32 = 398;

/// Creates the UI font for `dpi`, installs it on the dialog and all children, and
/// frees the previous one.
unsafe fn set_font(dlg: HWND, dpi: i32) {
    let font = CreateFontW(-(12 * dpi / 96), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, wide("Segoe UI").as_ptr());
    for c in controls() {
        SendMessageW(item(dlg, c.id), WM_SETFONT, font as WPARAM, 1);
    }
    let old = FONT.swap(font as isize, Ordering::Relaxed);
    if old != 0 {
        DeleteObject(old as HGDIOBJ);
    }
}

/// Positions all children for `dpi` and refreshes the font (also used on WM_DPICHANGED).
unsafe fn apply_layout(dlg: HWND, dpi: i32) {
    let s = |v: i32| v * dpi / 96;
    for c in controls() {
        MoveWindow(item(dlg, c.id), s(c.x), s(c.y), s(c.w), s(c.h), 1);
    }
    set_font(dlg, dpi);
}

/// Opens the dialog (or brings the open one to the front).
pub fn open(owner: HWND) {
    unsafe {
        let existing = hwnd();
        if !existing.is_null() {
            if IsIconic(existing) != 0 {
                ShowWindow(existing, SW_RESTORE);
            }
            SetForegroundWindow(existing);
            return;
        }
        OWNER.store(owner as isize, Ordering::Relaxed);

        let hinst = GetModuleHandleW(null());
        let class = wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(dlg_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: LoadIconW(hinst, 1usize as *const u16), // exe icon (resource id 1)
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: LoadIconW(hinst, 1usize as *const u16),
        };
        RegisterClassExW(&wc); // fails harmlessly if already registered

        // The dialog opens on the primary monitor; WM_DPICHANGED re-scales it if it is
        // dragged to a monitor with a different DPI.
        let dpi = GetDpiForSystem().max(96) as i32;
        let s = |v: i32| v * dpi / 96;
        let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: s(CLIENT_W), bottom: s(CLIENT_H) };
        AdjustWindowRectExForDpi(&mut r, style, 0, 0, dpi as u32);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let x = (GetSystemMetrics(SM_CXSCREEN) - w) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - h) / 2;

        let dlg = CreateWindowExW(
            0, class.as_ptr(), wide("Hidden apps").as_ptr(), style,
            x, y, w, h, null_mut(), null_mut(), hinst, null(),
        );
        if dlg.is_null() {
            return;
        }
        DLG.store(dlg as isize, Ordering::Relaxed);
        LAST_FOCUS.store(0, Ordering::Relaxed);

        for c in controls() {
            CreateWindowExW(
                c.ex, wide(c.class).as_ptr(), wide(c.text).as_ptr(), WS_CHILD | WS_VISIBLE | c.style,
                s(c.x), s(c.y), s(c.w), s(c.h), dlg, c.id as isize as HMENU, hinst, null(),
            );
        }
        set_font(dlg, dpi);

        refresh_ignored(dlg);
        refresh_running(dlg);

        ShowWindow(dlg, SW_SHOW);
        SetForegroundWindow(dlg);
        SetFocus(item(dlg, IDC_EDIT));
    }
}
