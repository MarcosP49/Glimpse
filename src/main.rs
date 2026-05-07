#![windows_subsystem = "windows"]

mod audio;
mod audio_ring;
mod capture;
mod encode;
mod hotkey;
mod log;
mod mux;
mod overlay;
mod segment_ring;
mod settings;
mod tray;

use anyhow::Result;
use audio_ring::AudioRing;
use capture::CaptureConfig;
use encode::EncodeConfig;
use log::clilog;
use mux::SaveConfig;
use parking_lot::Mutex;
use segment_ring::SegmentRing;
use settings::Settings;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};

const FFMPEG_EXE: &str = "ffmpeg.exe";

fn resolve_ffmpeg(name: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.parent().unwrap_or(std::path::Path::new(".")).join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(name)
}

fn clips_dir() -> PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("Videos").join("Glimpse")
}

fn main() -> Result<()> {
    // Single-instance guard — if our tray window class already exists, another
    // instance is running. Exit silently rather than spawning a duplicate.
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
        use windows::core::PCWSTR;
        let cls: Vec<u16> = "GlimpseTray\0".encode_utf16().collect();
        if let Ok(hwnd) = FindWindowW(PCWSTR(cls.as_ptr()), None) {
            if hwnd.0 != std::ptr::null_mut() {
                return Ok(());
            }
        }
    }

    unsafe {
        use windows::Win32::UI::HiDpi::*;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    log::init();

    let cfg = Settings::load();
    clilog!(
        "[glimpse] starting — monitor={} clip={}s fps={} bitrate={}Mbps",
        cfg.monitor_index, cfg.clip_secs, cfg.fps, cfg.bitrate_mbps
    );

    let segments_dir = std::env::temp_dir().join("glimpse_segments");
    std::fs::create_dir_all(&segments_dir)?;
    for entry in std::fs::read_dir(&segments_dir)?.flatten() {
        let _ = std::fs::remove_file(entry.path());
    }

    let ffmpeg_path = resolve_ffmpeg(FFMPEG_EXE);

    let clip_secs    = Arc::new(AtomicU32::new(cfg.clip_secs));
    let fps          = Arc::new(AtomicU32::new(cfg.fps));
    let bitrate_mbps = Arc::new(AtomicU32::new(cfg.bitrate_mbps));
    let monitor_index = Arc::new(AtomicU32::new(cfg.monitor_index));

    // Shared width/height — written by capture on (re)init, read by encode on restart.
    let width  = Arc::new(AtomicU32::new(0));
    let height = Arc::new(AtomicU32::new(0));

    let (frame_tx, frame_rx) = crossbeam_channel::bounded::<capture::Frame>(4);
    let (restart_tx, restart_rx) = crossbeam_channel::bounded::<()>(1);

    let capture_config = CaptureConfig {
        monitor_index: monitor_index.clone(),
        fps: fps.clone(),
        width: width.clone(),
        height: height.clone(),
        encode_restart_tx: restart_tx.clone(),
    };
    let (_capture_handle, (init_w, init_h)) = capture::start_capture(capture_config, frame_tx)?;
    clilog!("[glimpse] video: {init_w}x{init_h} @ {}fps  buffer: {}s", cfg.fps, cfg.clip_secs);

    let video_ring = Arc::new(Mutex::new(SegmentRing::new(segments_dir.clone(), cfg.clip_secs)));

    encode::start_encode(
        EncodeConfig {
            width: width.clone(),
            height: height.clone(),
            fps: fps.clone(),
            bitrate_mbps: bitrate_mbps.clone(),
            segments_dir,
            ffmpeg_path: ffmpeg_path.clone(),
            segment_ring: video_ring.clone(),
        },
        frame_rx,
        restart_rx,
    )?;

    let audio_ring = Arc::new(Mutex::new(AudioRing::new(clip_secs.clone())));
    let (_audio_handle, audio_fmt) = audio::start_audio_capture(audio_ring.clone())?;
    if audio_fmt.is_none() {
        clilog!("[glimpse] audio: unavailable, clips will be video-only");
    }

    let mic_ring = Arc::new(Mutex::new(AudioRing::new(clip_secs.clone())));
    let (_mic_handle, mic_fmt) = audio::start_mic_capture(mic_ring.clone())?;
    if mic_fmt.is_none() {
        clilog!("[glimpse] mic: unavailable");
    }

    let hotkey_packed_init = ((cfg.hotkey_mods as u64) << 32) | cfg.hotkey_vk as u64;
    let settings = Arc::new(Mutex::new(cfg));

    let (save_tx, save_rx) = crossbeam_channel::bounded::<()>(1);

    let (_saver_handle, notify_rx) = mux::start_saver(
        save_rx,
        SaveConfig {
            clips_dir: clips_dir(),
            ffmpeg_path,
            clip_secs: clip_secs.clone(),
            fps: fps.clone(),
            audio_fmt,
            mic_fmt,
        },
        video_ring.clone(),
        audio_ring,
        mic_ring,
    )?;

    let hotkey_packed = Arc::new(AtomicU64::new(hotkey_packed_init));
    let hk_thread_id = Arc::new(AtomicU32::new(0));

    let overlay = overlay::start_overlay(
        settings.clone(),
        clip_secs.clone(),
        fps.clone(),
        bitrate_mbps.clone(),
        restart_tx.clone(),
        save_tx.clone(),
        monitor_index.clone(),
        hotkey_packed.clone(),
        hk_thread_id.clone(),
    )?;

    let tray_handle = tray::start_tray(
        save_tx.clone(),
        overlay.clone(),
        video_ring,
        notify_rx,
        clip_secs,
        clips_dir(),
    )?;

    hotkey::start_hotkey_listener(save_tx, overlay, hotkey_packed, hk_thread_id)?;
    clilog!("[glimpse] ready — clips → {}", clips_dir().display());

    tray_handle.join().ok();
    clilog!("[glimpse] exiting");
    Ok(())
}
