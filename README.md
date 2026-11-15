# simple-overlay

A tiny, single-line performance overlay for Windows games, written in Rust with plain Win32 (no runtime, no driver, ~350 KB portable exe).

```
120 FPS  8.3 ms  [frametime chart]  CPU 68°  GPU 64°  23:04
```

It sits flush in the top-left corner of the screen (20 px tall at 100% scaling), detects the game you are playing automatically, and only shows up while a game is running.

## Download

Grab `simple-overlay.exe` from the [latest release](../../releases/latest) and run it. No installation needed.

## Usage

- **Tray icon** (right-click): Show overlay · Show clock · Start with Windows · Exit. Double-click toggles the overlay.
  Windows 11 puts new tray icons in the hidden area (^); drag it onto the taskbar if you want it always visible.
- `Ctrl+Shift+F11`: show/hide the overlay
- `Ctrl+Shift+F12`: exit
- `simple-overlay.exe --probe`: print the processes currently presenting frames plus sensor readings (for debugging); run it from a terminal, it attaches to that console. It uses its own ETW session, so it is safe while the overlay is running.

"Start with Windows" creates a per-user scheduled task (`FpsOverlay-<your SID>`, run at logon with highest privileges), so the overlay starts as admin without a UAC prompt at every login. Turn the option on while the overlay is running elevated (it asks for UAC once at launch); if the task cannot be created it falls back to a normal Run entry (UAC prompt at login).

Because the task runs elevated, it never launches an exe from a folder you can write to. Turning the option on copies the exe to `C:\Program Files\SimpleOverlay\overlay.exe` and the task starts that copy.
- **Updating:** exit the overlay, then run the new exe with autostart on; if it is newer and different it replaces the installed copy. An older exe never replaces a newer installed one.
- **Turning it off** removes the task and the installed copy. If the installed copy is the running exe it cannot be deleted while running; it is left behind and can be deleted after exiting.
- If the overlay is already on at startup and runs as admin, it also converts an old Run entry into the task and refreshes the installed copy.
- `ignore.txt` is read from next to the exe that is running; for the installed copy use the **Hidden apps...** dialog instead.
- Rare NVIDIA driver glitches are logged to `C:\Program Files\SimpleOverlay\overlay.log` (when installed), otherwise to the debugger output (OutputDebugString).

Settings are stored in `HKCU\Software\FpsOverlay`; the Run entry fallback uses `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

## How it works

- **FPS / frametime**: a real-time ETW session listening to Present events from DXGI (D3D10–12), D3D9, and DxgKrnl (fallback that also covers Vulkan/OpenGL), similar in spirit to PresentMon.
  Requires admin rights or membership in the *Performance Log Users* group; if access is denied the app asks for elevation (UAC).
- **Game detection**: the foreground process is treated as the game when it is presenting frames, is not on the ignore list or under `C:\Windows`, and its window covers at least 50% of the monitor at 15+ FPS. Once detected it stays detected even if FPS drops (loading screens).
- **GPU temperature**: NVIDIA NVML (`nvml.dll`, ships with the driver). Only queried while a game is running, so an idle Optimus dGPU is not woken up.
- **CPU temperature**: hottest ACPI thermal zone via PDH. On most laptops this tracks the CPU package; some desktops don't expose a useful zone and show `--`.

## Ignoring apps

Right-click the tray icon and choose **Hidden apps...** to open a dialog where you can pick a running app, type an exe name, or browse for an exe. The list is saved in `HKCU\Software\FpsOverlay` (`IgnoredApps`) and applies immediately.

You can also create `ignore.txt` next to the exe with one executable name per line (e.g. `myapp.exe`).

## Icon

The app icon (a frametime line on a dark tile) is `assets/icon.ico`, embedded in the exe and used for the tray and the dialog. `assets/make_icon.ps1` regenerates it (and `assets/icon.png`); `build.rs` embeds it without needing a resource compiler.

## Building

```
cargo build --release
```

The binary is written to `target\release\overlay.exe`.

## Limitations

- Cannot draw over games running in true **exclusive fullscreen** (most modern games use borderless / flip model, where it works fine).
- GPU temperature is NVIDIA-only for now.
