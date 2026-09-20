//! Our own Windows Graphics Capture session on a video-capable D3D11 device.
//!
//! `windows-capture` (used for the picker and thumbnails) creates its D3D11
//! device with `D3D11_CREATE_DEVICE_BGRA_SUPPORT` only, and gives no way to
//! hand in our own device. Media Foundation's video processor and hardware
//! encoder MFT (phases 2-3) need `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` on that
//! same device, so this module builds the capture pipeline directly on top
//! of the `windows` crate instead.
//!
//! This phase keeps the CPU readback identical to the old `windows-capture`
//! path (same per-frame cost, same [`Frame`] shape) so it's pure plumbing:
//! everything from GPU scaling onward still happens on the encoder thread.

use super::{CaptureTarget, Frame};
use crate::diag;
use anyhow::{Context as _, Result};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_9_2,
    D3D_FEATURE_LEVEL_9_3, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Com::{CO_MTA_USAGE_COOKIE, CoDecrementMTAUsage, CoIncrementMTAUsage};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{IInspectable, Interface};

/// How much earlier than the frame interval a frame may arrive and still be
/// kept. WGC delivers on the display's refresh, so at 30 fps on a 60 Hz
/// screen every other frame lands a hair either side of the interval; a
/// strict cut-off would drop most of them and deliver 20.
const FRAME_INTERVAL_SLACK: f64 = 0.85;

/// A running capture. Dropping it without calling [`CaptureSession::stop`]
/// still signals the capture thread to tear down (dropping `stop_tx` closes
/// its stop channel), but doesn't wait for that to finish — call `stop` to
/// block until the thread, and the capture session it owns, are gone.
pub struct CaptureSession {
    stop_tx: Sender<()>,
    thread: JoinHandle<()>,
}

impl CaptureSession {
    /// Stops capturing and waits for the capture thread to exit. The frame
    /// sender drops with the handler, which in turn ends the encoder thread.
    pub fn stop(self) {
        let _ = self.stop_tx.send(());
        if self.thread.join().is_err() {
            tracing::warn!("Capture thread panicked while stopping");
        }
    }
}

/// Starts capturing `target` on its own thread, at up to `fps` frames a
/// second. While `paused` is set, frames are dropped at the source.
pub fn start_capture(
    target: CaptureTarget,
    frame_tx: SyncSender<Frame>,
    paused: Arc<AtomicBool>,
    fps: u32,
) -> Result<CaptureSession> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let min_interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);

    let thread = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run_capture_thread(target, frame_tx, paused, min_interval, stop_rx, ready_tx))
        .context("Failed to spawn capture thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(CaptureSession { stop_tx, thread }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow::anyhow!("Capture thread exited before starting"))
        }
    }
}

/// Everything the capture thread has to close down cleanly on stop. Held for
/// the lifetime of the thread so the WinRT/COM state it depends on
/// (`_winrt`) outlives every call in the teardown sequence.
struct CaptureHandles {
    item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_arrived_token: i64,
    closed_token: i64,
    halt: Arc<AtomicBool>,
    _winrt: WinRtGuard,
}

fn run_capture_thread(
    target: CaptureTarget,
    tx: SyncSender<Frame>,
    paused: Arc<AtomicBool>,
    min_interval: Duration,
    stop_rx: Receiver<()>,
    ready_tx: Sender<Result<()>>,
) {
    let handles = match setup_capture(target, tx, paused, min_interval) {
        Ok(handles) => handles,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        // GUI-side receiver is already gone; fall through to teardown.
    }

    let _ = stop_rx.recv();

    handles.halt.store(true, Ordering::Relaxed);
    let _ = handles.frame_pool.RemoveFrameArrived(handles.frame_arrived_token);
    let _ = handles.item.RemoveClosed(handles.closed_token);
    let _ = handles.session.Close();
    let _ = handles.frame_pool.Close();
}

fn setup_capture(
    target: CaptureTarget,
    tx: SyncSender<Frame>,
    paused: Arc<AtomicBool>,
    min_interval: Duration,
) -> Result<CaptureHandles> {
    let _winrt = WinRtGuard::new().context("Failed to initialize WinRT")?;

    let (device, context) = create_device().context("Failed to create D3D11 device")?;
    let direct3d_device = wrap_for_winrt(&device).context("Failed to wrap D3D11 device for WinRT")?;
    let item = capture_item(target).context("Failed to create capture item")?;

    let pixel_format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
    let size = item.Size().context("Failed to get capture item size")?;
    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&direct3d_device, pixel_format, 1, size)
        .context("Failed to create frame pool")?;
    let session = frame_pool
        .CreateCaptureSession(&item)
        .context("Failed to create capture session")?;

    if let Err(e) = session.SetIsCursorCaptureEnabled(true) {
        tracing::debug!("Cursor capture toggle not supported: {e}");
    }
    if let Err(e) = session.SetIsBorderRequired(false) {
        tracing::debug!("Border toggle not supported: {e}");
    }
    if let Err(e) = session.SetMinUpdateInterval(duration_to_timespan(min_interval)) {
        tracing::debug!("Minimum update interval not supported: {e}");
    }

    let halt = Arc::new(AtomicBool::new(false));
    let last_size = (AtomicI32::new(size.Width), AtomicI32::new(size.Height));
    let last_kept: Mutex<Option<Instant>> = Mutex::new(None);

    let closure_frame_pool = frame_pool.clone();
    let closure_direct3d_device = SendDirectX(direct3d_device.clone());
    let closure_device = device.clone();
    let closure_context = context.clone();
    let closure_halt = halt.clone();
    let closure_paused = paused;
    let closure_tx = tx;

    let frame_arrived_token = frame_pool
        .FrameArrived(&windows::Foundation::TypedEventHandler::<
            Direct3D11CaptureFramePool,
            IInspectable,
        >::new(move |sender, _args| {
            // Force capture of the whole `SendDirectX` wrapper rather than
            // Rust 2021 disjoint closure capture reaching straight in and
            // grabbing the non-`Send` `IDirect3DDevice` field, which would
            // make this closure `!Send`.
            let closure_direct3d_device = &closure_direct3d_device;

            if closure_halt.load(Ordering::Relaxed) {
                return Ok(());
            }

            let Some(pool) = sender.as_ref() else {
                return Ok(());
            };
            let capture_frame = pool.TryGetNextFrame()?;
            diag::CAP_ARRIVED.inc();

            let content_size = capture_frame.ContentSize()?;
            if content_size.Width != last_size.0.load(Ordering::Relaxed)
                || content_size.Height != last_size.1.load(Ordering::Relaxed)
            {
                // The current frame's surface still has the old pool
                // dimensions; release it and wait for a frame backed by a
                // correctly sized surface before doing any real work.
                drop(capture_frame);
                closure_frame_pool.Recreate(&closure_direct3d_device.0, pixel_format, 1, content_size)?;
                last_size.0.store(content_size.Width, Ordering::Relaxed);
                last_size.1.store(content_size.Height, Ordering::Relaxed);
                return Ok(());
            }

            // Paused: drop the frame before paying for the GPU readback, so
            // nothing reaches the encoder and it goes idle.
            if closure_paused.load(Ordering::Relaxed) {
                diag::CAP_SKIP_PAUSED.inc();
                return Ok(());
            }

            // Frame-rate cap, applied here rather than relying solely on
            // WGC's MinUpdateInterval, which older Windows 10 builds don't
            // have. Also before the readback, so a dropped frame costs
            // nothing.
            let now = Instant::now();
            {
                let mut last = last_kept.lock().unwrap();
                if last.is_some_and(|t| now.duration_since(t) < min_interval.mul_f64(FRAME_INTERVAL_SLACK)) {
                    diag::CAP_SKIP_RATE.inc();
                    return Ok(());
                }
                *last = Some(now);
            }

            let surface = capture_frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };

            let readback_start = Instant::now();
            let data = read_back(&closure_device, &closure_context, &texture, &desc)?;
            let readback = readback_start.elapsed();
            diag::CAP_READBACK_US.add_time(readback);
            diag::CAP_READBACK_MAX_US.max_time(readback);

            let frame = Frame {
                width: desc.Width,
                height: desc.Height,
                data,
                timestamp: Instant::now(),
            };

            match closure_tx.try_send(frame) {
                Ok(()) => diag::CAP_KEPT.inc(),
                Err(_) => diag::CAP_DROP_FULL.inc(),
            }

            Ok(())
        }))
        .context("Failed to register FrameArrived handler")?;

    let closed_token = item
        .Closed(&windows::Foundation::TypedEventHandler::<
            GraphicsCaptureItem,
            IInspectable,
        >::new(move |_, _| {
            tracing::info!("Capture source closed");
            Ok(())
        }))
        .context("Failed to register Closed handler")?;

    session.StartCapture().context("Failed to start capture")?;

    Ok(CaptureHandles {
        item,
        frame_pool,
        session,
        frame_arrived_token,
        closed_token,
        halt,
        _winrt,
    })
}

/// A wrapper to send a WinRT DirectX device across threads. The Win32 D3D11
/// interfaces (`ID3D11Device`, `ID3D11DeviceContext`, ...) are `Send`, but
/// the WinRT `IDirect3DDevice` projection is not, so it needs an explicit
/// (and sound, since D3D11 objects created with multithread protection are
/// safe to call from any thread) opt-in.
struct SendDirectX<T>(T);
unsafe impl<T> Send for SendDirectX<T> {}

/// Keeps the current thread in the process-wide MTA for the lifetime of the
/// capture session, mirroring what `windows-capture` does per capture
/// thread.
struct WinRtGuard {
    mta_cookie: CO_MTA_USAGE_COOKIE,
}

impl WinRtGuard {
    fn new() -> Result<Self> {
        let mta_cookie = unsafe { CoIncrementMTAUsage() }.context("CoIncrementMTAUsage failed")?;
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("RoInitialize failed")?;
        Ok(Self { mta_cookie })
    }
}

impl Drop for WinRtGuard {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
        let _ = unsafe { CoDecrementMTAUsage(self.mta_cookie) };
    }
}

/// Creates the D3D11 device the capture, GPU scaling (phase 2) and hardware
/// encoder MFT (phase 3) will all share. `VIDEO_SUPPORT` is what
/// `windows-capture`'s own device lacks; `SetMultithreadProtected` is needed
/// because the capture thread and a future encoder thread both touch the
/// immediate context.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let feature_levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_9_3,
        D3D_FEATURE_LEVEL_9_2,
        D3D_FEATURE_LEVEL_9_1,
    ];
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;

    let mut device = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            flags,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )?;
    }

    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = context.context("D3D11CreateDevice returned no context")?;

    if feature_level.0 < D3D_FEATURE_LEVEL_11_0.0 {
        anyhow::bail!("D3D11 device does not support feature level 11.0");
    }

    let multithread: ID3D11Multithread = context.cast().context("Device context has no ID3D11Multithread")?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    Ok((device, context))
}

fn wrap_for_winrt(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi_device: IDXGIDevice = device.cast()?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
    Ok(inspectable.cast()?)
}

fn capture_item(target: CaptureTarget) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .context("Failed to get GraphicsCaptureItem interop factory")?;
    let item: GraphicsCaptureItem = match target {
        CaptureTarget::Window(window) => unsafe { interop.CreateForWindow(HWND(window.as_raw_hwnd()))? },
        CaptureTarget::Monitor(monitor) => unsafe {
            interop.CreateForMonitor(HMONITOR(monitor.as_raw_hmonitor()))?
        },
    };
    Ok(item)
}

fn duration_to_timespan(d: Duration) -> TimeSpan {
    TimeSpan {
        Duration: (d.as_nanos() / 100) as i64,
    }
}

/// Copies a captured D3D11 texture into a tightly packed BGRA8 buffer via a
/// CPU staging texture. Same cost profile as the readback `windows-capture`
/// did (one GPU-side copy, one map, one row-by-row memcpy) — phase 2 is what
/// removes this step for the hardware-encoder path.
fn read_back(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    desc: &D3D11_TEXTURE2D_DESC,
) -> windows::core::Result<Vec<u8>> {
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };

    let mut staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.expect("CreateTexture2D succeeded without producing a texture");

    unsafe { context.CopyResource(&staging, texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

    let width = desc.Width as usize;
    let height = desc.Height as usize;
    let row_bytes = width * 4;
    let mut out = vec![0u8; row_bytes * height];
    unsafe {
        let src = mapped.pData as *const u8;
        for y in 0..height {
            let src_row = src.add(y * mapped.RowPitch as usize);
            let dst_row = out.as_mut_ptr().add(y * row_bytes);
            std::ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
        }
        context.Unmap(&staging, 0);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures the primary monitor for real and checks that frames with
    /// sane BGRA8 dimensions come out the other end. Needs an interactive
    /// desktop session, so it's not part of the default `cargo test` run:
    /// `cargo test --package screenshare --lib capture::session::tests -- --ignored`.
    #[test]
    #[ignore]
    fn captures_real_frames_from_primary_monitor() {
        let monitor = windows_capture::monitor::Monitor::primary().expect("primary monitor");
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let paused = Arc::new(AtomicBool::new(false));

        let session = start_capture(CaptureTarget::Monitor(monitor), tx, paused, 30)
            .expect("start_capture should succeed on a real desktop session");

        let mut seen = 0;
        for _ in 0..3 {
            let frame = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("expected a frame within 5s");
            assert!(frame.width > 0 && frame.height > 0, "frame has zero dimension");
            assert_eq!(
                frame.data.len(),
                (frame.width * frame.height * 4) as usize,
                "frame data isn't tightly packed BGRA8"
            );
            seen += 1;
        }
        assert_eq!(seen, 3);

        session.stop();
    }
}
