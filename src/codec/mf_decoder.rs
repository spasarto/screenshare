//! H.264 decoding through Media Foundation's H.264 decoder MFT, on the GPU
//! (DXVA) where it can be. Phase 5 of `docs/hardware-encoding-plan.md`.
//!
//! Unlike the hardware encoders, Microsoft's decoder is a synchronous MFT:
//! `ProcessInput`, then `ProcessOutput` until it wants more input. Given a
//! D3D11 device it decodes into GPU textures, which we copy to a staging
//! texture and read back as NV12 — about 5 MB at 1440p, the same planes the
//! viewer uploads. Keeping the picture on the GPU all the way to the screen
//! would take D3D11 → wgpu texture sharing, which isn't worth it yet.
//!
//! Without a usable device it decodes in software into system memory, which
//! is still multithreaded and so faster than OpenH264.

use super::mf_encoder::{MtaGuard, ensure_mf_started, stream_ids};
use super::{Nv12Frame, VideoDecoder};
use anyhow::{Context as _, Result, anyhow, bail};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFDXGIBuffer,
    IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoArea,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

/// Nominal time between samples, in 100 ns units. The decoder only needs
/// timestamps to increase; nothing downstream reads them.
const SAMPLE_DURATION: i64 = 10_000_000 / 60;

/// Media Foundation's H.264 decoder, driven synchronously on the decoder
/// thread.
pub struct MfDecoder {
    name: String,
    transform: IMFTransform,
    input_stream: u32,
    output_stream: u32,
    /// Re-read on every output type change: whether the MFT hands out its
    /// own samples (it does with a D3D device) and, if not, how big ours
    /// must be.
    output_provides_samples: bool,
    output_buffer_size: u32,
    format: Option<OutputFormat>,
    gpu: Option<GpuReadback>,
    next_time: i64,
    _device_manager: Option<IMFDXGIDeviceManager>,
    _mta: MtaGuard,
}

/// The layout of the pictures the MFT outputs, from its current output type.
#[derive(Clone, Copy, Debug)]
struct OutputFormat {
    /// The allocated picture size, 16-aligned (e.g. 1920x1088).
    coded_height: u32,
    /// Row pitch of system-memory output, when the type says.
    default_stride: Option<u32>,
    /// The part of the picture to show, from the SPS cropping.
    visible: Visible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Visible {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// The D3D11 device the MFT decodes on, plus a staging texture to read its
/// output back through.
struct GpuReadback {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl MfDecoder {
    /// Starts Microsoft's H.264 decoder MFT. With `use_gpu` it's given a D3D11
    /// device to decode on (DXVA); if that's refused it carries on in software
    /// rather than failing. Fails only when the MFT itself won't start
    /// (N editions of Windows without the Media Feature Pack, say): the
    /// caller's fallback is OpenH264.
    pub fn new(use_gpu: bool) -> Result<Self> {
        let mta = MtaGuard::new()?;
        ensure_mf_started()?;

        let transform: IMFTransform = unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
            .context("Failed to create the Media Foundation H.264 decoder")?;
        let attributes = unsafe { transform.GetAttributes() }.context("MFT has no attributes")?;
        if let Err(e) = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) } {
            tracing::debug!("H.264 decoder: MF_LOW_LATENCY not accepted: {e}");
        }
        // Without this the decoder holds several frames back for reordering,
        // which a stream without B-frames never needs.
        if let Ok(api) = transform.cast::<ICodecAPI>()
            && let Err(e) = unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true)) }
        {
            tracing::debug!("H.264 decoder: low-latency mode not accepted: {e}");
        }

        let (gpu, device_manager) = if use_gpu {
            match attach_device(&transform) {
                Ok((gpu, manager)) => (Some(gpu), Some(manager)),
                Err(e) => {
                    tracing::info!("H.264 decoder: no GPU decoding, decoding in software: {e:#}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        let name = format!(
            "Microsoft H.264 decoder ({})",
            if gpu.is_some() { "DXVA" } else { "software" }
        );

        let (input_stream, output_stream) = stream_ids(&transform)?;
        let input_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType failed")?;
        unsafe {
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        }
        unsafe { transform.SetInputType(input_stream, &input_type, 0) }.context("SetInputType failed")?;

        let mut decoder = Self {
            name,
            transform,
            input_stream,
            output_stream,
            output_provides_samples: false,
            output_buffer_size: 0,
            format: None,
            gpu,
            next_time: 0,
            _device_manager: device_manager,
            _mta: mta,
        };
        decoder.set_nv12_output()?;

        unsafe {
            let t = &decoder.transform;
            t.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).context("MFT_MESSAGE_COMMAND_FLUSH failed")?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING failed")?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM failed")?;
        }
        Ok(decoder)
    }

    /// Whether it decodes on the GPU (the D3D11 device was accepted).
    pub fn is_hardware(&self) -> bool {
        self.gpu.is_some()
    }

    /// Picks NV12 from the output types the MFT offers, and records the
    /// layout that comes with it. Done at start and again whenever the
    /// stream's format changes (the first SPS, or a resolution change).
    fn set_nv12_output(&mut self) -> Result<()> {
        let mut index = 0;
        let media_type = loop {
            let candidate = unsafe { self.transform.GetOutputAvailableType(self.output_stream, index) }
                .context("The decoder offers no NV12 output")?;
            if unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.is_ok_and(|s| s == MFVideoFormat_NV12) {
                break candidate;
            }
            index += 1;
        };
        unsafe { self.transform.SetOutputType(self.output_stream, &media_type, 0) }
            .context("SetOutputType (NV12) failed")?;

        let current = unsafe { self.transform.GetOutputCurrentType(self.output_stream) }.unwrap_or(media_type);
        self.format = output_format(&current);
        let info = unsafe { self.transform.GetOutputStreamInfo(self.output_stream) }
            .context("GetOutputStreamInfo failed")?;
        self.output_provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        self.output_buffer_size = info.cbSize;
        if let Some(f) = self.format {
            tracing::debug!("{}: output {:?}, MFT samples {}", self.name, f, self.output_provides_samples);
        }
        Ok(())
    }

    fn submit(&mut self, h264: &[u8]) -> windows::core::Result<()> {
        let buffer = unsafe { MFCreateMemoryBuffer(h264.len() as u32) }?;
        let mut ptr = std::ptr::null_mut();
        unsafe {
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(h264.as_ptr(), ptr, h264.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(h264.len() as u32)?;
        }
        let sample = unsafe { MFCreateSample() }?;
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.next_time)?;
            sample.SetSampleDuration(SAMPLE_DURATION)?;
        }
        self.next_time += SAMPLE_DURATION;
        unsafe { self.transform.ProcessInput(self.input_stream, &sample, 0) }
    }

    /// Takes every picture the MFT has ready and returns the newest; older
    /// ones would only be overwritten before the GUI got to them anyway.
    fn drain(&mut self) -> Result<Option<Nv12Frame>> {
        let mut latest = None;
        while let Some(output) = self.process_output()? {
            if let Some(frame) = output {
                if latest.is_some() {
                    crate::diag::DEC_OVERWRITTEN.inc();
                }
                latest = Some(frame);
            }
        }
        Ok(latest)
    }

    /// One `ProcessOutput`. `None` when the MFT needs more input; `Some(None)`
    /// when it renegotiated its output format instead of producing a picture.
    fn process_output(&mut self) -> Result<Option<Option<Nv12Frame>>> {
        let our_sample = if self.output_provides_samples {
            None
        } else {
            let sample = unsafe { MFCreateSample() }?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size.max(1)) }?;
            unsafe { sample.AddBuffer(&buffer) }?;
            Some(sample)
        };

        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_stream,
            pSample: std::mem::ManuallyDrop::new(our_sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        // Reclaim what the MFT filled in so their references are released.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
        let _events = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pEvents) };

        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.set_nv12_output().context("Renegotiating output after a stream change")?;
                return Ok(Some(None));
            }
            Err(e) => return Err(e).context("ProcessOutput failed"),
        }

        let sample = sample.context("ProcessOutput succeeded without a sample")?;
        let format = self.format.context("Output before the output format was known")?;
        Ok(Some(Some(self.read_sample(&sample, format)?)))
    }

    fn read_sample(&mut self, sample: &IMFSample, format: OutputFormat) -> Result<Nv12Frame> {
        let buffer = unsafe { sample.GetBufferByIndex(0) }.context("Output sample has no buffer")?;
        if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>()
            && let Some(gpu) = &mut self.gpu
        {
            return gpu.read(&dxgi, format.visible);
        }
        read_memory_buffer(&buffer, format)
    }
}

impl VideoDecoder for MfDecoder {
    fn decode(&mut self, h264: &[u8]) -> Result<Option<Nv12Frame>> {
        let mut latest = None;
        match self.submit(h264) {
            Ok(()) => {}
            // Output still waiting to be collected; collect it, then retry.
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                latest = self.drain()?;
                self.submit(h264).context("ProcessInput failed after draining")?;
            }
            Err(e) => return Err(e).context("ProcessInput failed"),
        }
        match self.drain()? {
            Some(frame) => {
                if latest.is_some() {
                    crate::diag::DEC_OVERWRITTEN.inc();
                }
                Ok(Some(frame))
            }
            None => Ok(latest),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
    }
}

/// Gives the MFT a D3D11 device of its own to decode on.
fn attach_device(transform: &IMFTransform) -> Result<(GpuReadback, IMFDXGIDeviceManager)> {
    let attributes = unsafe { transform.GetAttributes() }.context("MFT has no attributes")?;
    if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
        bail!("decoder isn't D3D11-aware");
    }
    let (device, context) = crate::capture::session::create_device()?;
    let mut reset_token = 0;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
        .context("MFCreateDXGIDeviceManager failed")?;
    let manager = manager.context("MFCreateDXGIDeviceManager returned nothing")?;
    unsafe { manager.ResetDevice(&device, reset_token) }.context("IMFDXGIDeviceManager::ResetDevice failed")?;
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .context("MFT_MESSAGE_SET_D3D_MANAGER failed")?;
    Ok((GpuReadback { device, context, staging: None }, manager))
}

fn output_format(media_type: &IMFMediaType) -> Option<OutputFormat> {
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    let (coded_width, coded_height) = ((size >> 32) as u32, size as u32);
    if coded_width == 0 || coded_height == 0 {
        return None;
    }
    let default_stride = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .ok()
        .map(|s| s as i32)
        .filter(|&s| s > 0)
        .map(|s| s as u32);
    let aperture = display_aperture(media_type);
    Some(OutputFormat {
        coded_height,
        default_stride,
        visible: visible_area(coded_width, coded_height, aperture),
    })
}

fn display_aperture(media_type: &IMFMediaType) -> Option<MFVideoArea> {
    let mut area = MFVideoArea::default();
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(&mut area as *mut _ as *mut u8, std::mem::size_of::<MFVideoArea>())
    };
    unsafe { media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None) }.ok()?;
    Some(area)
}

/// The visible part of a `coded_width`x`coded_height` picture: the display
/// aperture if the decoder gave a sane one, else all of it. Rounded to even
/// sizes and offsets, since NV12 chroma covers 2x2 pixels.
fn visible_area(coded_width: u32, coded_height: u32, aperture: Option<MFVideoArea>) -> Visible {
    let full = Visible { x: 0, y: 0, width: coded_width & !1, height: coded_height & !1 };
    let Some(a) = aperture else { return full };
    let (x, y) = (a.OffsetX.value.max(0) as u32 & !1, a.OffsetY.value.max(0) as u32 & !1);
    let (w, h) = (a.Area.cx.max(0) as u32 & !1, a.Area.cy.max(0) as u32 & !1);
    if w == 0 || h == 0 || x + w > coded_width || y + h > coded_height {
        return full;
    }
    Visible { x, y, width: w, height: h }
}

impl GpuReadback {
    /// Copies a decoded picture (one slice of the decoder's texture array)
    /// into our staging texture and reads the visible part back.
    fn read(&mut self, buffer: &IMFDXGIBuffer, visible: Visible) -> Result<Nv12Frame> {
        let mut raw = std::ptr::null_mut();
        unsafe { buffer.GetResource(&ID3D11Texture2D::IID, &mut raw) }.context("IMFDXGIBuffer::GetResource failed")?;
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource = unsafe { buffer.GetSubresourceIndex() }.context("GetSubresourceIndex failed")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if visible.x + visible.width > desc.Width || visible.y + visible.height > desc.Height {
            bail!("decoded texture {}x{} is smaller than the picture {visible:?}", desc.Width, desc.Height);
        }

        let staging = match &self.staging {
            Some((t, w, h)) if (*w, *h) == (desc.Width, desc.Height) => t.clone(),
            _ => {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t = None;
                unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut t)) }
                    .context("Failed to create the decoder's staging texture")?;
                let t = t.context("CreateTexture2D returned nothing")?;
                self.staging = Some((t.clone(), desc.Width, desc.Height));
                t
            }
        };

        unsafe { self.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, subresource, None) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .context("Failed to map the decoder's staging texture")?;
        // The chroma plane follows the luma plane's `Height` rows, at the same
        // pitch — the layout every D3D11 NV12 implementation uses.
        let frame = unsafe {
            copy_nv12(mapped.pData as *const u8, mapped.RowPitch as usize, desc.Height as usize, visible)
        };
        unsafe { self.context.Unmap(&staging, 0) };
        Ok(frame)
    }
}

/// Reads a picture the MFT decoded into system memory (software decoding).
fn read_memory_buffer(buffer: &IMFMediaBuffer, format: OutputFormat) -> Result<Nv12Frame> {
    if let Ok(buffer_2d) = buffer.cast::<IMF2DBuffer>() {
        let mut base = std::ptr::null_mut();
        let mut pitch = 0;
        unsafe { buffer_2d.Lock2D(&mut base, &mut pitch) }.context("IMF2DBuffer::Lock2D failed")?;
        let frame = if pitch > 0 {
            Ok(unsafe { copy_nv12(base, pitch as usize, format.coded_height as usize, format.visible) })
        } else {
            Err(anyhow!("bottom-up NV12 output (pitch {pitch})"))
        };
        let _ = unsafe { buffer_2d.Unlock2D() };
        return frame;
    }

    let mut base = std::ptr::null_mut();
    let mut len = 0;
    unsafe { buffer.Lock(&mut base, None, Some(&mut len)) }.context("IMFMediaBuffer::Lock failed")?;
    let pitch = format.default_stride.unwrap_or(format.visible.x + format.visible.width) as usize;
    let needed = pitch * format.coded_height as usize * 3 / 2;
    let frame = if (len as usize) >= needed {
        Ok(unsafe { copy_nv12(base, pitch, format.coded_height as usize, format.visible) })
    } else {
        Err(anyhow!("output buffer is {len} bytes, expected at least {needed}"))
    };
    let _ = unsafe { buffer.Unlock() };
    frame
}

/// Copies the `visible` part of an NV12 picture whose Y plane starts at
/// `base`, rows `pitch` bytes apart, with the UV plane after `plane_rows`
/// rows of Y.
///
/// # Safety
/// `base` must point to at least `pitch * plane_rows * 3 / 2` readable bytes,
/// and `visible` must lie within `pitch` x `plane_rows`.
unsafe fn copy_nv12(base: *const u8, pitch: usize, plane_rows: usize, visible: Visible) -> Nv12Frame {
    let (x, y0) = (visible.x as usize, visible.y as usize);
    let (w, h) = (visible.width as usize, visible.height as usize);
    let mut y = vec![0u8; w * h];
    let mut uv = vec![0u8; w * h / 2];
    unsafe {
        for (row, dst) in y.chunks_exact_mut(w).enumerate() {
            let src = base.add((y0 + row) * pitch + x);
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w);
        }
        let uv_base = base.add(plane_rows * pitch);
        for (row, dst) in uv.chunks_exact_mut(w).enumerate() {
            // `x` is even, so it's also the byte offset of its U/V pair.
            let src = uv_base.add((y0 / 2 + row) * pitch + x);
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w);
        }
    }
    Nv12Frame { width: w as u32, height: h as u32, y, uv }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Media::MediaFoundation::MFOffset;

    fn area(x: i16, y: i16, w: i32, h: i32) -> MFVideoArea {
        MFVideoArea {
            OffsetX: MFOffset { fract: 0, value: x },
            OffsetY: MFOffset { fract: 0, value: y },
            Area: SIZE { cx: w, cy: h },
        }
    }

    #[test]
    fn visible_area_crops_to_the_aperture() {
        assert_eq!(
            visible_area(1920, 1088, Some(area(0, 0, 1920, 1080))),
            Visible { x: 0, y: 0, width: 1920, height: 1080 }
        );
        assert_eq!(visible_area(1920, 1088, None), Visible { x: 0, y: 0, width: 1920, height: 1088 });
    }

    #[test]
    fn visible_area_ignores_apertures_outside_the_picture() {
        let full = Visible { x: 0, y: 0, width: 1280, height: 720 };
        assert_eq!(visible_area(1280, 720, Some(area(0, 0, 1920, 1080))), full);
        assert_eq!(visible_area(1280, 720, Some(area(0, 0, 0, 0))), full);
    }

    #[test]
    fn copy_nv12_crops_both_planes() {
        // 4x4 picture in an 8-byte pitch, Y then UV after 4 rows.
        let pitch = 8;
        let mut data = vec![0u8; pitch * 6];
        for row in 0..4 {
            for col in 0..pitch {
                data[row * pitch + col] = (row * 10 + col) as u8;
            }
        }
        for row in 0..2 {
            for col in 0..pitch {
                data[(4 + row) * pitch + col] = (100 + row * 10 + col) as u8;
            }
        }
        let visible = Visible { x: 2, y: 2, width: 2, height: 2 };
        let frame = unsafe { copy_nv12(data.as_ptr(), pitch, 4, visible) };
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frame.y, [22, 23, 32, 33]);
        assert_eq!(frame.uv, [112, 113]);
    }

    /// Encodes the primary monitor with OpenH264 and decodes it with the
    /// Media Foundation decoder on the GPU. Needs an interactive desktop:
    /// `cargo test --bin screenshare codec::mf_decoder::tests -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn decodes_real_frames_on_the_gpu() {
        use crate::capture::{CaptureTarget, session::start_capture};
        use crate::codec::{H264Encoder, VideoEncoder};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let monitor = windows_capture::monitor::Monitor::primary().expect("primary monitor");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let paused = Arc::new(AtomicBool::new(false));
        let session = start_capture(CaptureTarget::Monitor(monitor), tx, paused, 30, 1920, 1080)
            .expect("start_capture should succeed on a real desktop session");

        let mut encoder: Option<H264Encoder> = None;
        let mut decoder = MfDecoder::new(true).expect("the Media Foundation decoder should start");
        println!("decoder: {}", decoder.name());
        let mut decoded = 0;
        for i in 0..30 {
            let frame = rx.recv_timeout(Duration::from_secs(5)).expect("expected a frame within 5s");
            let enc = encoder.get_or_insert_with(|| H264Encoder::new(frame.width, frame.height, 30).unwrap());
            let (y_ref, _) = frame.read_back().unwrap();
            let h264 = enc.encode(&frame).expect("encode");
            if h264.is_empty() {
                continue;
            }
            let start = Instant::now();
            let out = decoder.decode(&h264).expect("decode");
            println!("frame {i}: {} bytes decoded in {:?}", h264.len(), start.elapsed());
            if let Some(out) = out {
                assert_eq!((out.width, out.height), (frame.width, frame.height));
                assert_eq!(out.y.len(), y_ref.len());
                // Lossy, but a decoded screen should be close to what went in.
                let diff: u64 = out.y.iter().zip(&y_ref).map(|(a, b)| a.abs_diff(*b) as u64).sum();
                let mean = diff as f64 / y_ref.len() as f64;
                assert!(mean < 8.0, "decoded luma differs by {mean:.1} on average");
                decoded += 1;
            }
        }
        session.stop();
        assert!(decoded >= 25, "only {decoded} of 30 frames decoded");
    }
}
