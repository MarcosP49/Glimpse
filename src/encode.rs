use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use crate::log::clilog;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

const CREATE_NO_WINDOW: u32 = 0x08000000;

use crate::capture::Frame;
use crate::segment_ring::SegmentRing;

pub struct EncodeConfig {
    pub width:        Arc<AtomicU32>,
    pub height:       Arc<AtomicU32>,
    pub fps:          Arc<AtomicU32>,
    pub bitrate_mbps: Arc<AtomicU32>,
    pub segments_dir: PathBuf,
    pub ffmpeg_path:  PathBuf,
    pub segment_ring: Arc<Mutex<SegmentRing>>,
}

pub fn start_encode(
    config: EncodeConfig,
    rx: Receiver<Frame>,
    restart_rx: Receiver<()>,
) -> Result<std::thread::JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || {
            if let Err(e) = encode_loop(&config, &rx, &restart_rx) {
                clilog!("[encode] fatal: {e:#}");
            }
        })?;
    Ok(handle)
}

fn spawn_ffmpeg(config: &EncodeConfig) -> Result<Child> {
    let fps          = config.fps.load(Ordering::Relaxed);
    let bitrate_mbps = config.bitrate_mbps.load(Ordering::Relaxed);
    let width        = config.width.load(Ordering::Relaxed);
    let height       = config.height.load(Ordering::Relaxed);
    let segment_pattern = config
        .segments_dir
        .join("seg%06d.ts")
        .to_string_lossy()
        .into_owned();

    // Lookahead: 4 frames is enough for real-time screen recording.
    // surfaces must be >= lookahead + pipeline overhead; 12 gives headroom
    // at <=60fps and 16 at >60fps without hitting the default 32.
    let lookahead = 4u32;
    let surfaces  = if fps > 60 { 16 } else { 12 };

    Command::new(&config.ffmpeg_path)
        .args([
            "-f", "rawvideo",
            "-pixel_format", "bgra",
            "-video_size", &format!("{width}x{height}"),
            "-framerate", &fps.to_string(),
            "-i", "pipe:0",
            "-c:v", "h264_nvenc",
            "-preset", "p4",
            "-surfaces", &surfaces.to_string(),
            "-rc-lookahead", &lookahead.to_string(),
            "-bf", "0",
            "-b:v", &format!("{bitrate_mbps}M"),
            "-g", &fps.to_string(),
            "-f", "segment",
            "-segment_time", "1",
            "-segment_format", "mpegts",
            &segment_pattern,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("failed to spawn ffmpeg — is ffmpeg.exe present?")
}

fn spawn_ffmpeg_software(config: &EncodeConfig) -> Result<Child> {
    let fps          = config.fps.load(Ordering::Relaxed);
    let bitrate_mbps = config.bitrate_mbps.load(Ordering::Relaxed);
    let width        = config.width.load(Ordering::Relaxed);
    let height       = config.height.load(Ordering::Relaxed);
    let segment_pattern = config
        .segments_dir
        .join("seg%06d.ts")
        .to_string_lossy()
        .into_owned();

    Command::new(&config.ffmpeg_path)
        .args([
            "-f", "rawvideo",
            "-pixel_format", "bgra",
            "-video_size", &format!("{width}x{height}"),
            "-framerate", &fps.to_string(),
            "-i", "pipe:0",
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-b:v", &format!("{bitrate_mbps}M"),
            "-g", &fps.to_string(),
            "-f", "segment",
            "-segment_time", "1",
            "-segment_format", "mpegts",
            &segment_pattern,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("failed to spawn ffmpeg (software fallback)")
}

fn encode_loop(config: &EncodeConfig, rx: &Receiver<Frame>, restart_rx: &Receiver<()>) -> Result<()> {
    'outer: loop {
        let mut child = match spawn_ffmpeg(config) {
            Ok(c) => {
                clilog!("[encode] using h264_nvenc ({}x{})",
                    config.width.load(Ordering::Relaxed),
                    config.height.load(Ordering::Relaxed));
                c
            }
            Err(e) => {
                clilog!("[encode] NVENC unavailable ({e:#}), falling back to software x264");
                match spawn_ffmpeg_software(config) {
                    Ok(c) => c,
                    Err(e2) => return Err(e2),
                }
            }
        };

        let stdin = match child.stdin.take() {
            Some(s) => s,
            None => return Err(anyhow::anyhow!("ffmpeg stdin not available")),
        };

        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stderr).lines() {
                    if let Ok(line) = line {
                        if !line.starts_with("frame=") {
                            clilog!("[ffmpeg] {line}");
                        }
                    }
                }
            });
        }

        // Dedicated stdin-writing thread: decouples blocking write_all from the
        // select loop so the capture channel drains at full speed regardless of
        // pipe write latency.  Without this, a slow write_all stalls the select,
        // the capture channel fills to capacity, and try_send silently drops
        // frames — causing actual fps to fall below the declared rate and making
        // the video cover more real-world seconds than its PTS claims.
        let (write_tx, write_rx) = crossbeam_channel::bounded::<Arc<Vec<u8>>>(8);
        let write_handle = std::thread::Builder::new()
            .name("stdin-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                for data in write_rx {
                    if stdin.write_all(&data).is_err() {
                        break;
                    }
                }
                // stdin drops here, closing ffmpeg's input pipe.
            })?;

        // Returns true to restart the encoder, false to exit.
        let restart = loop {
            crossbeam_channel::select! {
                recv(rx) -> frame_res => match frame_res {
                    Ok(frame) => {
                        match write_tx.try_send(frame.data) {
                            Ok(()) => {}
                            // Writer is momentarily behind; drop this frame rather
                            // than blocking and propagating backpressure upstream.
                            Err(crossbeam_channel::TrySendError::Full(_)) => {
                                clilog!("[encode] writer backlogged, frame dropped");
                            }
                            // Writer thread exited (stdin error); restart ffmpeg.
                            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                                clilog!("[encode] stdin-writer exited unexpectedly, restarting");
                                break true;
                            }
                        }
                    }
                    Err(_) => break false,
                },
                recv(restart_rx) -> _ => {
                    clilog!("[encode] restarting (settings changed)");
                    break true;
                },
            }
        };

        // Drop write_tx first so the writer thread sees disconnection and drains
        // any queued frames, then wait for it and for ffmpeg to exit cleanly.
        drop(write_tx);
        let _ = write_handle.join();
        let _ = child.wait();

        if restart {
            config.segment_ring.lock().clear();
            continue 'outer;
        }
        break 'outer;
    }
    Ok(())
}
