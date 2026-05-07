use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x08000000;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::log::clilog;

use crate::audio::AudioFormat;
use crate::audio_ring::AudioRing;
use crate::segment_ring::SegmentRing;

pub struct SaveConfig {
    pub clips_dir: PathBuf,
    pub ffmpeg_path: PathBuf,
    pub clip_secs: Arc<AtomicU32>,
    pub fps: Arc<AtomicU32>,
    pub audio_fmt: Option<AudioFormat>,
    pub mic_fmt: Option<AudioFormat>,
}

pub fn start_saver(
    save_rx: Receiver<()>,
    config: SaveConfig,
    video_ring: Arc<Mutex<SegmentRing>>,
    audio_ring: Arc<Mutex<AudioRing>>,
    mic_ring: Arc<Mutex<AudioRing>>,
) -> Result<(std::thread::JoinHandle<()>, Receiver<String>)> {
    let (notify_tx, notify_rx) = crossbeam_channel::unbounded::<String>();
    let handle = std::thread::Builder::new()
        .name("saver".into())
        .spawn(move || {
            run_saver(save_rx, config, video_ring, audio_ring, mic_ring, notify_tx);
        })?;
    Ok((handle, notify_rx))
}

fn run_saver(
    save_rx: Receiver<()>,
    config: SaveConfig,
    video_ring: Arc<Mutex<SegmentRing>>,
    audio_ring: Arc<Mutex<AudioRing>>,
    mic_ring: Arc<Mutex<AudioRing>>,
    notify_tx: Sender<String>,
) {
    while save_rx.recv().is_ok() {
        let t0 = Instant::now();
        match save_clip(&config, &video_ring, &audio_ring, &mic_ring) {
            Ok(path) => {
                let elapsed = t0.elapsed().as_secs_f32();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                clilog!("[saver] saved: {}  ({:.1}s)", path.display(), elapsed);
                let _ = notify_tx.send(name);
            }
            Err(e) => clilog!("[saver] clip save failed: {e:#}"),
        }
    }
}

fn save_clip(
    config: &SaveConfig,
    video_ring: &Arc<Mutex<SegmentRing>>,
    audio_ring: &Arc<Mutex<AudioRing>>,
    mic_ring: &Arc<Mutex<AudioRing>>,
) -> Result<PathBuf> {
    let clip_secs = config.clip_secs.load(Ordering::Relaxed);
    let segments = video_ring.lock().get_clip_segments(clip_secs);

    // Use the newest completed segment's mtime as the audio_end anchor.
    // The mtime is when ffmpeg finished writing that segment — the real-world
    // endpoint of the captured video — on the same wall-clock as audio
    // captured_at timestamps (bridged via a simultaneous SystemTime/Instant
    // snapshot).  audio_start is exactly n_segs seconds earlier, matching the
    // video PTS span one-for-one.
    let seg_newest_mtime: Option<SystemTime> = segments.last()
        .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());

    let snap_sys = SystemTime::now();
    let snap_ins = Instant::now();

    // Compensate for the encode pipeline delay: newest_mtime is when ffmpeg
    // *finished* writing the last segment, which is ~2 frame-durations after
    // the last frame in that segment was captured.  Subtracting that offset
    // pulls audio_end back to align with actual video content time.  The factor
    // is dynamic so it scales correctly at any user-configured fps.
    let fps = config.fps.load(Ordering::Relaxed).max(1);
    let pipeline_delay = Duration::from_secs_f64(2.0 / fps as f64);

    let n_segs = segments.len();
    let audio_end = seg_newest_mtime
        .and_then(|st| snap_sys.duration_since(st).ok())
        .and_then(|age| snap_ins.checked_sub(age))
        .unwrap_or_else(|| snap_ins - Duration::from_secs(3))
        .checked_sub(pipeline_delay)
        .unwrap_or(snap_ins - Duration::from_secs(3));
    let audio_start = audio_end
        .checked_sub(Duration::from_secs(n_segs as u64))
        .unwrap_or(audio_end);

    clilog!(
        "[saver] audio window — start: {:.2?} ago  end: {:.2?} ago  n_segs: {}",
        snap_ins.saturating_duration_since(audio_start),
        snap_ins.saturating_duration_since(audio_end),
        n_segs,
    );

    let desktop = config.audio_fmt.as_ref().and_then(|fmt| {
        let data = audio_ring
            .lock()
            .get_clip_data_for_range(audio_start, audio_end, fmt.sample_rate, fmt.block_align);
        if data.is_empty() { None } else { Some((fmt.clone(), data)) }
    });

    let mic = config.mic_fmt.as_ref().and_then(|fmt| {
        let data = mic_ring
            .lock()
            .get_clip_data_for_range(audio_start, audio_end, fmt.sample_rate, fmt.block_align);
        if data.is_empty() { None } else { Some((fmt.clone(), data)) }
    });

    // Clamp both audio tracks to exactly n_segs seconds of PTS.
    // audio_start is already n_segs seconds before audio_end, so this is a
    // belt-and-suspenders guard against floating-point rounding in get_clip_data_for_range.
    let desktop = desktop.map(|(fmt, mut data)| {
        data.truncate(n_segs * fmt.sample_rate as usize * fmt.block_align as usize);
        (fmt, data)
    });
    let mic = mic.map(|(fmt, mut data)| {
        data.truncate(n_segs * fmt.sample_rate as usize * fmt.block_align as usize);
        (fmt, data)
    });

    if segments.is_empty() {
        anyhow::bail!("no video segments buffered — wait a moment and try again");
    }

    std::fs::create_dir_all(&config.clips_dir)?;
    let output_path = config.clips_dir.join(clip_filename());

    let tmp = std::env::temp_dir();
    let concat_path = tmp.join("glimpse_concat.txt");
    let audio_path = tmp.join("glimpse_audio.raw");
    let mic_path = tmp.join("glimpse_mic.raw");

    {
        let mut f = std::fs::File::create(&concat_path)
            .context("failed to write concat list")?;
        writeln!(f, "ffconcat version 1.0")?;
        for seg in &segments {
            let p = seg.to_string_lossy().replace('\\', "/");
            writeln!(f, "file '{p}'")?;
        }
    }

    let mut cmd = Command::new(&config.ffmpeg_path);
    cmd.args(["-y", "-f", "concat", "-safe", "0", "-i"]);
    cmd.arg(&concat_path);

    if let Some((ref fmt, ref data)) = desktop {
        std::fs::write(&audio_path, data).context("failed to write audio temp file")?;
        cmd.args([
            "-f", fmt.ffmpeg_format(),
            "-ar", &fmt.sample_rate.to_string(),
            "-ac", &fmt.channels.to_string(),
            "-i",
        ]);
        cmd.arg(&audio_path);
    }

    if let Some((ref fmt, ref data)) = mic {
        std::fs::write(&mic_path, data).context("failed to write mic temp file")?;
        cmd.args([
            "-f", fmt.ffmpeg_format(),
            "-ar", &fmt.sample_rate.to_string(),
            "-ac", &fmt.channels.to_string(),
            "-i",
        ]);
        cmd.arg(&mic_path);
    }

    match (desktop.is_some(), mic.is_some()) {
        (true, true) => {
            // Mix desktop + mic. Reset video PTS to 0 so it aligns with the PCM audio stream
            // (which ffmpeg always assigns PTS 0), preventing the accumulated segment PTS
            // from appearing as an A/V offset in the output file.
            cmd.args([
                "-filter_complex",
                "[0:v]setpts=PTS-STARTPTS[v];[1:a][2:a]amix=inputs=2:duration=first:dropout_transition=3:normalize=0[aout]",
                "-map", "[v]",
                "-map", "[aout]",
                "-c:v", "libx264", "-preset", "ultrafast", "-crf", "18",
                "-c:a", "aac", "-b:a", "192k",
            ]);
        }
        (true, false) | (false, true) => {
            cmd.args([
                "-vf", "setpts=PTS-STARTPTS",
                "-c:v", "libx264", "-preset", "ultrafast", "-crf", "18",
                "-c:a", "aac", "-b:a", "192k",
            ]);
        }
        (false, false) => {
            cmd.args([
                "-vf", "setpts=PTS-STARTPTS",
                "-c:v", "libx264", "-preset", "ultrafast", "-crf", "18",
            ]);
        }
    }

    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(&output_path);

    let output = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("failed to launch ffmpeg for mux")?;

    let _ = std::fs::remove_file(&concat_path);
    let _ = std::fs::remove_file(&audio_path);
    let _ = std::fs::remove_file(&mic_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg mux exited with {}:\n{stderr}", output.status);
    }

    Ok(output_path)
}

fn clip_filename() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!(
        "clip_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.mp4",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}
