//! System tray icon, its context menu, and the "start with Windows" option.

use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Callback message the tray icon sends to the overlay window.
pub const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;

pub const CMD_TOGGLE: usize = 100;
pub const CMD_AUTOSTART: usize = 101;
pub const CMD_QUIT: usize = 102;
pub const CMD_CLOCK: usize = 103;

const SETTINGS_KEY: &str = r"Software\FpsOverlay";
pub const SETTING_CLOCK: &str = "ShowClock";

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "FpsOverlay";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

// ------------------------------------------------------------------ icon

/// Draws a small rounded tile with a green frametime-style line.
pub fn make_icon() -> HICON {
    const N: i32 = 32;
    unsafe {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let color = CreateCompatibleBitmap(screen, N, N);
        let mask = CreateBitmap(N, N, 1, 1, null());

        // Mask: white = transparent, black = opaque (the rounded tile).
        let old = SelectObject(dc, mask);
        PatBlt(dc, 0, 0, N, N, WHITENESS);
        SelectObject(dc, GetStockObject(BLACK_BRUSH));
        SelectObject(dc, GetStockObject(BLACK_PEN));
        RoundRect(dc, 0, 0, N, N, 10, 10);

        // Color: black outside the tile (required for transparency), dark tile, green line.
        SelectObject(dc, color);
        PatBlt(dc, 0, 0, N, N, BLACKNESS);
        let tile = CreateSolidBrush(rgb(30, 33, 44));
        SelectObject(dc, tile);
        SelectObject(dc, GetStockObject(NULL_PEN));
        RoundRect(dc, 0, 0, N + 1, N + 1, 10, 10);
        let pen = CreatePen(PS_SOLID, 3, rgb(110, 231, 140));
        SelectObject(dc, pen);
        let pts = [
            POINT { x: 5, y: 22 },
            POINT { x: 11, y: 16 },
            POINT { x: 16, y: 20 },
            POINT { x: 21, y: 9 },
            POINT { x: 27, y: 13 },
        ];
        Polyline(dc, pts.as_ptr(), pts.len() as i32);

        SelectObject(dc, old);
        DeleteObject(pen);
        DeleteObject(tile);
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);

        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(mask);
        DeleteObject(color);
        icon
    }
}

// ------------------------------------------------------------------ tray icon

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { zeroed() };
    n.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = TRAY_ID;
    n
}

pub fn add(hwnd: HWND, icon: HICON) {
    let mut n = nid(hwnd);
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = icon;
    let tip: Vec<u16> = "FPS Overlay".encode_utf16().collect();
    n.szTip[..tip.len()].copy_from_slice(&tip);
    unsafe { Shell_NotifyIconW(NIM_ADD, &n) };
}

pub fn remove(hwnd: HWND) {
    let n = nid(hwnd);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &n) };
}

/// Message Explorer broadcasts after it restarts; the icon must be re-added then.
pub fn taskbar_created_msg() -> u32 {
    unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }
}

/// Shows the context menu at the cursor and returns the chosen command (0 = none).
pub fn show_menu(hwnd: HWND, overlay_visible: bool, clock: bool) -> usize {
    unsafe {
        let menu = CreatePopupMenu();
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide("FPS Overlay").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(
            menu,
            MF_STRING | check(overlay_visible),
            CMD_TOGGLE,
            wide("Show overlay\tCtrl+Shift+F11").as_ptr(),
        );
        AppendMenuW(menu, MF_STRING | check(clock), CMD_CLOCK, wide("Show clock").as_ptr());
        AppendMenuW(
            menu,
            MF_STRING | check(autostart_enabled()),
            CMD_AUTOSTART,
            wide("Start with Windows").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Exit\tCtrl+Shift+F12").as_ptr());

        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        cmd.max(0) as usize
    }
}

// ------------------------------------------------------------------ settings

/// Boolean setting stored as a DWORD under HKCU\Software\FpsOverlay.
pub fn get_flag(name: &str) -> Option<bool> {
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(SETTINGS_KEY).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut v as *mut u32 as *mut _,
            &mut size,
        )
    };
    (r == 0).then_some(v != 0)
}

pub fn set_flag(name: &str, on: bool) {
    unsafe {
        let mut key: HKEY = null_mut();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(SETTINGS_KEY).as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE,
            null(),
            &mut key,
            null_mut(),
        ) != 0
        {
            return;
        }
        let v = on as u32;
        RegSetValueExW(key, wide(name).as_ptr(), 0, REG_DWORD, &v as *const u32 as *const u8, 4);
        RegCloseKey(key);
    }
}

// ------------------------------------------------------------------ autostart

pub fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(RUN_VALUE).as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            null_mut(),
        ) == 0
    }
}

pub fn set_autostart(on: bool) {
    unsafe {
        let mut key: HKEY = null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key) != 0 {
            return;
        }
        let name = wide(RUN_VALUE);
        if on {
            let Ok(exe) = std::env::current_exe() else {
                RegCloseKey(key);
                return;
            };
            let cmd = wide(&format!("\"{}\"", exe.display()));
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                cmd.as_ptr() as *const u8,
                (cmd.len() * 2) as u32,
            );
        } else {
            RegDeleteValueW(key, name.as_ptr());
        }
        RegCloseKey(key);
    }
}

/// If autostart is on, rewrite the entry so it follows the exe if it was moved.
pub fn refresh_autostart() {
    if autostart_enabled() {
        set_autostart(true);
    }
}
