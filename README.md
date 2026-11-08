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
- `simple-overlay.exe --probe`: print the processes currently presenting frames plus sensor readings (for debugging)

Settings are stored in `HKCU\Software\FpsOverlay`; "Start with Windows" uses `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

## How it works

- **FPS / frametime**: a real-time ETW session listening to Present events from DXGI (D3D10–12), D3D9, and DxgKrnl (fallback that also covers Vulkan/OpenGL), similar in spirit to PresentMon.
  Requires admin rights or membership in the *Performance Log Users* group; if access is denied the app asks for elevation (UAC).
- **Game detection**: the foreground process is treated as the game when it is presenting frames, is not on the ignore list or under `C:\Windows`, and its window covers at least 50% of the monitor at 15+ FPS. Once detected it stays detected even if FPS drops (loading screens).
- **GPU temperature**: NVIDIA NVML (`nvml.dll`, ships with the driver). Only queried while a game is running, so an idle Optimus dGPU is not woken up.
- **CPU temperature**: hottest ACPI thermal zone via PDH. On most laptops this tracks the CPU package; some desktops don't expose a useful zone and show `--`.

## Ignoring apps

Create `ignore.txt` next to the exe with one executable name per line (e.g. `myapp.exe`).

## Building

```
cargo build --release
```

The binary is written to `target\release\overlay.exe`.

## Limitations

- Cannot draw over games running in true **exclusive fullscreen** (most modern games use borderless / flip model, where it works fine).
- GPU temperature is NVIDIA-only for now.
