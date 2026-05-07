use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use crate::log::clilog;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIResource,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

const E_WAIT_TIMEOUT:             i32 = 0x887A0027u32 as i32;
const E_ACCESS_LOST:              i32 = 0x887A0026u32 as i32;
// Returned by Map(D3D11_MAP_FLAG_DO_NOT_WAIT) when the GPU copy is still in flight.
const DXGI_ERROR_WAS_STILL_DRAWING: i32 = 0x887A000Au32 as i32;
// D3D11_MAP_FLAG_DO_NOT_WAIT — makes Map return immediately instead of stalling
// the CPU until all pending GPU work (including unrelated game rendering) finishes.
const D3D11_MAP_FLAG_DO_NOT_WAIT: u32 = 0x100000;

pub struct Frame {
    pub data: Arc<Vec<u8>>,
}

pub struct CaptureConfig {
    pub monitor_index:     Arc<AtomicU32>,
    pub fps:               Arc<AtomicU32>,
    /// Written by capture when it (re)initialises; read by encode on restart.
    pub width:             Arc<AtomicU32>,
    pub height:            Arc<AtomicU32>,
    /// Sent to the encode thread whenever capture restarts on a new monitor.
    pub encode_restart_tx: Sender<()>,
}

pub fn start_capture(
    config: CaptureConfig,
    frame_tx: Sender<Frame>,
) -> Result<(std::thread::JoinHandle<()>, (u32, u32))> {
    let (dim_tx, dim_rx) = std::sync::mpsc::sync_channel::<Result<(u32, u32)>>(0);

    let handle = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            let mut instant = false;
            match capture_loop(&config, &frame_tx, &dim_tx, &mut instant) {
                Ok(()) => break,
                Err(e) => {
                    if instant {
                        clilog!("[capture] restarting on new monitor");
                    } else {
                        clilog!("[capture] error: {e:#}, restarting in 2s");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        })?;

    let dims = dim_rx
        .recv()
        .context("capture thread exited before sending dimensions")??;

    Ok((handle, dims))
}

fn capture_loop(
    config: &CaptureConfig,
    tx: &Sender<Frame>,
    dim_tx: &std::sync::mpsc::SyncSender<Result<(u32, u32)>>,
    instant_restart: &mut bool,
) -> Result<()> {
    unsafe {
        let target_monitor = config.monitor_index.load(Ordering::Relaxed);

        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter = factory.EnumAdapters(0)?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device  = device.context("D3D11 device is null")?;
        let context = context.context("D3D11 context is null")?;

        let raw_output = adapter
            .EnumOutputs(target_monitor)
            .with_context(|| format!("monitor {} not found", target_monitor))?;
        let output1: IDXGIOutput1 = raw_output.cast()?;
        let duplication = output1.DuplicateOutput(&device)?;

        let dupl_desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();
        let width  = dupl_desc.ModeDesc.Width;
        let height = dupl_desc.ModeDesc.Height;

        config.width.store(width,   Ordering::Relaxed);
        config.height.store(height, Ordering::Relaxed);
        if dim_tx.try_send(Ok((width, height))).is_err() {
            let _ = config.encode_restart_tx.try_send(());
        }

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: dupl_desc.ModeDesc.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut s0: Option<ID3D11Texture2D> = None;
        let mut s1: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut s0))?;
        device.CreateTexture2D(&staging_desc, None, Some(&mut s1))?;
        let staging = [
            s0.context("staging texture 0 is null")?,
            s1.context("staging texture 1 is null")?,
        ];

        // staging_idx  — which texture the NEXT CopyResource will write into.
        // has_pending  — whether staging[1 - staging_idx] holds a queued GPU copy
        //                that we should attempt to read this iteration.
        let mut staging_idx: usize = 0;
        let mut has_pending: bool  = false;

        let bytes_per_frame = (width * height * 4) as usize;
        let mut last_frame_data: Option<Arc<Vec<u8>>> = None;
        let mut last_send = Instant::now();

        loop {
            if config.monitor_index.load(Ordering::Relaxed) != target_monitor {
                *instant_restart = true;
                return Err(anyhow::anyhow!("monitor index changed"));
            }

            let fps = config.fps.load(Ordering::Relaxed).max(1);
            let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
            // Truncate (not ceil): at 60 fps this is 16 ms. On a 60 Hz desktop real
            // frames always arrive at 16.67 ms so AcquireNextFrame returns on the
            // frame event, not the timeout, and fps is correct.
            let timeout_ms = frame_interval.as_millis() as u32;

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            let acquire: windows::core::Result<()> =
                duplication.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource);

            match acquire {
                Err(ref e) if e.code().0 == E_WAIT_TIMEOUT => {
                    let now = Instant::now();
                    if now.duration_since(last_send) >= frame_interval {
                        if let Some(ref data) = last_frame_data {
                            let _ = tx.try_send(Frame { data: data.clone() });
                            last_send += frame_interval;
                        }
                    }
                    continue;
                }
                Err(ref e) if e.code().0 == E_ACCESS_LOST => {
                    return Err(anyhow::anyhow!("DXGI access lost (display mode changed?)"));
                }
                Err(e) => return Err(e.into()),
                Ok(()) => {}
            }

            let resource = resource.context("AcquireNextFrame returned no resource")?;
            let desktop_tex: ID3D11Texture2D = resource.cast()?;

            let now = Instant::now();
            if now.duration_since(last_send) >= frame_interval {
                if has_pending {
                    let prev = 1 - staging_idx;
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();

                    // Map the PREVIOUS staging texture with DO_NOT_WAIT.
                    //
                    // Critical ordering: we call Map BEFORE issuing this iteration's
                    // CopyResource.  A Map with MapFlags=0 (blocking) does a full
                    // D3D11 pipeline flush — it waits for every GPU command queued
                    // before the call, including one we just pushed.  By mapping
                    // first, the flush only covers work from prior iterations, giving
                    // the GPU a full frame interval to have finished that copy.
                    //
                    // DO_NOT_WAIT means: if the GPU copy still isn't done, return
                    // DXGI_ERROR_WAS_STILL_DRAWING immediately instead of stalling.
                    // We then send a duplicate frame to keep the fps cadence and retry
                    // on the next iteration (another 16 ms for the GPU to catch up).
                    let map_result: windows::core::Result<()> = context.Map(
                        &staging[prev], 0, D3D11_MAP_READ,
                        D3D11_MAP_FLAG_DO_NOT_WAIT,
                        Some(&mut mapped),
                    );

                    match map_result {
                        Err(ref e) if e.code().0 == DXGI_ERROR_WAS_STILL_DRAWING => {
                            // GPU isn't done yet. Capture this desktop frame into the
                            // current slot for a future read, release the DXGI frame,
                            // and send a duplicate to hold the declared fps.
                            context.CopyResource(&staging[staging_idx], &desktop_tex);
                            duplication.ReleaseFrame()?;
                            if let Some(ref data) = last_frame_data {
                                let _ = tx.try_send(Frame { data: data.clone() });
                                last_send += frame_interval;
                            }
                            // Don't flip staging_idx: next iteration retries Map on
                            // the same prev texture, which now has one more frame
                            // interval of GPU time to finish.
                        }
                        Err(e) => return Err(e.into()),
                        Ok(()) => {
                            // GPU copy is done — read pixels out of staging[prev].
                            let pitch = mapped.RowPitch as usize;
                            let mut data = vec![0u8; bytes_per_frame];
                            let src = std::slice::from_raw_parts(
                                mapped.pData as *const u8,
                                pitch * height as usize,
                            );
                            let row_bytes = width as usize * 4;
                            if pitch == row_bytes {
                                data.copy_from_slice(&src[..bytes_per_frame]);
                            } else {
                                for row in 0..height as usize {
                                    let dst = row * row_bytes;
                                    data[dst..dst + row_bytes]
                                        .copy_from_slice(&src[row * pitch..row * pitch + row_bytes]);
                                }
                            }
                            context.Unmap(&staging[prev], 0);

                            // Queue CopyResource for THIS frame only after Map has
                            // returned.  This ensures the next iteration's Map call
                            // never has to wait for the copy we just issued.
                            context.CopyResource(&staging[staging_idx], &desktop_tex);
                            duplication.ReleaseFrame()?;

                            let data = Arc::new(data);
                            last_frame_data = Some(data.clone());
                            let _ = tx.try_send(Frame { data });
                            last_send += frame_interval;

                            has_pending = true;
                            staging_idx ^= 1;
                        }
                    }
                } else {
                    // First iteration: prime the pipeline by queuing the first GPU
                    // copy.  Don't advance last_send so the next iteration also fires
                    // the rate check immediately and reads this texture.
                    context.CopyResource(&staging[staging_idx], &desktop_tex);
                    duplication.ReleaseFrame()?;
                    has_pending = true;
                    staging_idx ^= 1;
                }
            } else {
                // Frame arrived but rate limiter says it's too soon.
                duplication.ReleaseFrame()?;
            }
        }
    }
}
