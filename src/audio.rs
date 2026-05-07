use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::audio_ring::{AudioChunk, AudioRing};
use crate::log::clilog;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Clone, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub block_align: u16,
    pub is_float: bool,
}

impl AudioFormat {
    pub fn ffmpeg_format(&self) -> &'static str {
        match (self.is_float, self.bits_per_sample) {
            (true, 32) => "f32le",
            (false, 16) => "s16le",
            (false, 32) => "s32le",
            _ => "f32le",
        }
    }
}

pub fn start_audio_capture(
    ring: Arc<Mutex<AudioRing>>,
) -> Result<(std::thread::JoinHandle<()>, Option<AudioFormat>)> {
    start_capture_thread(ring, true, "audio")
}

pub fn start_mic_capture(
    ring: Arc<Mutex<AudioRing>>,
) -> Result<(std::thread::JoinHandle<()>, Option<AudioFormat>)> {
    start_capture_thread(ring, false, "mic")
}

fn start_capture_thread(
    ring: Arc<Mutex<AudioRing>>,
    loopback: bool,
    label: &'static str,
) -> Result<(std::thread::JoinHandle<()>, Option<AudioFormat>)> {
    let (fmt_tx, fmt_rx) = std::sync::mpsc::sync_channel::<Result<AudioFormat>>(0);

    let handle = std::thread::Builder::new()
        .name(label.into())
        .spawn(move || {
            if let Err(e) = audio_loop(&ring, &fmt_tx, loopback, label) {
                clilog!("[{label}] error: {e:#}");
                let _ = fmt_tx.try_send(Err(e));
            }
        })?;

    let fmt = match fmt_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(f)) => Some(f),
        Ok(Err(e)) => {
            clilog!("[{label}] capture unavailable: {e:#}");
            None
        }
        Err(_) => {
            clilog!("[{label}] timed out waiting for audio init");
            None
        }
    };

    Ok((handle, fmt))
}

fn audio_loop(
    ring: &Arc<Mutex<AudioRing>>,
    fmt_tx: &std::sync::mpsc::SyncSender<Result<AudioFormat>>,
    loopback: bool,
    label: &'static str,
) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("CoCreateInstance(MMDeviceEnumerator) failed")?;

        let endpoint_dir = if loopback { eRender } else { eCapture };
        let device = enumerator
            .GetDefaultAudioEndpoint(endpoint_dir, eConsole)
            .context("no default audio endpoint")?;

        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("IMMDevice::Activate(IAudioClient) failed")?;

        let fmt_ptr = client.GetMixFormat().context("GetMixFormat failed")?;
        let fmt = &*fmt_ptr;

        let sample_rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels;
        let bits_per_sample = fmt.wBitsPerSample;
        let block_align = fmt.nBlockAlign;
        let format_tag = fmt.wFormatTag;

        let is_float = if format_tag == WAVE_FORMAT_IEEE_FLOAT {
            true
        } else if format_tag == WAVE_FORMAT_EXTENSIBLE {
            let ext_bytes =
                std::slice::from_raw_parts(fmt_ptr as *const u8, 18 + fmt.cbSize as usize);
            let sub_tag = u16::from_le_bytes([ext_bytes[24], ext_bytes[25]]);
            sub_tag == WAVE_FORMAT_IEEE_FLOAT
        } else {
            format_tag != WAVE_FORMAT_PCM
        };

        const REFTIMES_PER_SEC: i64 = 10_000_000;
        // Loopback capture requires AUDCLNT_STREAMFLAGS_LOOPBACK; mic capture uses no flags.
        let stream_flags: u32 = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                REFTIMES_PER_SEC / 5,
                0,
                fmt_ptr,
                None,
            )
            .context("IAudioClient::Initialize failed")?;

        CoTaskMemFree(Some(fmt_ptr as *mut _));

        let audio_fmt = AudioFormat {
            sample_rate,
            channels,
            bits_per_sample,
            block_align,
            is_float,
        };

        let _ = fmt_tx.try_send(Ok(audio_fmt.clone()));

        let capture_client: IAudioCaptureClient = client
            .GetService()
            .context("GetService(IAudioCaptureClient) failed")?;

        client.Start().context("IAudioClient::Start failed")?;

        clilog!(
            "[{label}] WASAPI {}: {}Hz  {}ch  {}bit  {}",
            if loopback { "loopback" } else { "capture" },
            sample_rate,
            channels,
            bits_per_sample,
            audio_fmt.ffmpeg_format(),
        );

        let mut next_chunk_end = Instant::now();

        loop {
            std::thread::sleep(Duration::from_millis(10));

            let mut packet_size = capture_client.GetNextPacketSize()?;

            while packet_size > 0 {
                let mut data_ptr: *mut u8 = std::ptr::null_mut();
                let mut num_frames: u32 = 0;
                let mut flags: u32 = 0;

                capture_client.GetBuffer(
                    &mut data_ptr,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )?;

                let byte_count = num_frames as usize * block_align as usize;
                // AUDCLNT_BUFFERFLAGS_SILENT = 2
                let data = if flags & 2 != 0 {
                    vec![0u8; byte_count]
                } else {
                    std::slice::from_raw_parts(data_ptr, byte_count).to_vec()
                };

                capture_client.ReleaseBuffer(num_frames)?;

                let chunk_dur = Duration::from_secs_f64(num_frames as f64 / sample_rate as f64);
                let now = Instant::now();
                if now > next_chunk_end + Duration::from_millis(50) {
                    next_chunk_end = now - chunk_dur;
                }
                next_chunk_end += chunk_dur;

                ring.lock().push(AudioChunk {
                    data,
                    num_frames,
                    captured_at: next_chunk_end,
                });

                packet_size = capture_client.GetNextPacketSize()?;
            }
        }
    }
}
