# Glimpse

A lightweight ShadowPlay-style screen and audio clip tool for Windows. Runs silently in the system tray, keeps a rolling buffer of the last N seconds of your screen, and saves a clip instantly when you press a hotkey.

---

## Features

- Continuous ring buffer recording — always captures, saves only when you want
- Hardware-accelerated encoding via NVENC (falls back to libx264 automatically)
- WASAPI loopback audio capture — records desktop audio in sync with video
- Configurable clip length, frame rate, and bitrate
- Settings overlay with live preview — toggle with the hotkey or tray icon
- Saves timestamped `.mp4` files to `%USERPROFILE%\Videos\Glimpse`
- Optional start-with-Windows support

---

## Download

Go to the [Releases](../../releases) page and download the latest `glimpse-x.x.x-setup.exe`. Run it, follow the installer, and you're done.

---

## Usage

1. After install, Glimpse starts automatically and sits in your system tray.
2. Press **Ctrl + Shift + F8** (default) at any time to save the last 30 seconds as a clip.
3. Right-click the tray icon to open Settings or quit.
4. In Settings you can change:
   - **Clip length** (10 – 120 s)
   - **Frame rate** (1 – 120 fps)
   - **Bitrate** (1 – 100 Mbps)
   - **Hotkey** — click the hotkey card and press any key combination
   - **Output directory** — clips always go to `%USERPROFILE%\Videos\Glimpse`
   - **Start with Windows** toggle
   - **Accent colour** and **background image** in the Advanced tab

---

## Building from source

### Prerequisites

- [Rust](https://rustup.rs) (stable, MSVC toolchain)
- `ffmpeg.exe` placed in the project root (or on your PATH)

```
git clone https://github.com/your-username/glimpse
cd glimpse
cargo build --release
```

The binary is at `target\release\glimpse.exe`. Copy it alongside `ffmpeg.exe` and run.

### Building the installer

Requires [NSIS](https://nsis.sourceforge.io) installed.

```
cargo build --release
copy target\release\glimpse.exe installer\
copy ffmpeg.exe installer\
makensis /DVERSION="1.0.0" installer\glimpse.nsi
```

---

## ffmpeg

Glimpse uses `ffmpeg.exe` for encoding and remuxing. The installer bundles the [gyan.dev essentials build](https://www.gyan.dev/ffmpeg/builds/) (LGPL licence). If you build from source, download your own copy and place it next to the exe.

---

## Licence

MIT
