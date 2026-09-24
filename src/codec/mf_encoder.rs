//! H.264 encoding on the GPU through Media Foundation's hardware encoder
//! MFTs (NVENC, Quick Sync, AMF — whichever the capture adapter has). Phase
//! 3 of `docs/hardware-encoding-plan.md`.
//!
//! Frames arrive as NV12 textures on the capture device (see
//! `capture::video_processor`) and go straight into the MFT as DXGI surface
//! buffers, so nothing is read back to the CPU until it's a few tens of KB
//! of H.264.
//!
//! Hardware MFTs are asynchronous: they announce when they want input and
//! when they have output through `IMFMediaEventGenerator` events. A small
//! pump thread blocks on those events and forwards them over a channel, so
//! `encode` can wait on them with a timeout and present the usual one frame
//! in, (usually) one frame out interface.

use super::{EncoderInput, VideoEncoder, bitrate_for};
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_NOTIMPL, LUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFActivate, IMFAttributes,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    MEError, METransformHaveOutput, METransformNeedInput, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NONE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_LEVEL,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFNominalRange_16_235, MFSTARTUP_FULL, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnum2,
    MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, eAVEncH264VProfile_ConstrainedBase,
};
use windows::Win32::System::Com::{CO_MTA_USAGE_COOKIE, CoDecrementMTAUsage, CoIncrementMTAUsage, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, HRESULT, Interface};

/// How long to wait for the MFT to ask for input, or to produce output it
/// owes, before calling it stuck.
const STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait for a frame's output while the MFT is already asking
/// for the next input. In low-latency mode the output normally follows
/// within a few ms; if it doesn't come within this, the encoder is holding
/// frames back, and `encode` stops waiting for them from then on (see
/// `MfEncoder::max_lag`). Generous, so a slow first keyframe isn't mistaken
/// for that.
const LAG_PROBE: Duration = Duration::from_millis(250);

/// A GOP longer than the keyframe timer in `app.rs`, so keyframes are
/// normally the ones it forces through `CODECAPI_AVEncVideoForceKeyFrame`.
/// Still finite, as a backstop for an encoder that ignores that (0 would
/// mean "encoder's choice", which varies by vendor).
const GOP_SECONDS: u32 = 4;

/// What the event pump forwards: each event's type and status.
type EventResult = Result<(u32, HRESULT)>;

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

/// A hardware H.264 encoder MFT, driven asynchronously on the calling
/// thread (plus a pump thread for its events). Create it on the encoder
/// thread, with a texture from the capture device.
pub struct MfEncoder {
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    input_stream: u32,
    output_stream: u32,
    /// Whether the MFT allocates its own output samples (hardware MFTs
    /// normally do); if not, we hand it one of `output_buffer_size` bytes.
    output_provides_samples: bool,
    output_buffer_size: u32,
    events: Receiver<EventResult>,
    pump: Option<JoinHandle<()>>,
    /// `METransformNeedInput` events not yet answered with `ProcessInput`.
    need_input: u32,
    /// Frames given to the MFT whose output hasn't come out yet. Holding
    /// them keeps their NV12 textures leased, so the capture thread can't
    /// overwrite a texture the GPU may still be encoding from.
    in_flight: VecDeque<EncoderInput>,
    /// How many frames the MFT holds back before producing output. 0 in
    /// low-latency mode, which every vendor we've seen honours; raised if an
    /// encoder turns out to pipeline anyway, so `encode` doesn't sit waiting
    /// for output that won't come until the next input.
    max_lag: usize,
    first_timestamp: Option<Instant>,
    keyframe_requested: bool,
    /// The last SPS + PPS the encoder emitted (Annex-B, start codes
    /// included), for IDRs that come without them.
    parameter_sets: Option<Vec<u8>>,
    _device_manager: IMFDXGIDeviceManager,
    _mta: MtaGuard,
}

impl MfEncoder {
    /// Finds a hardware H.264 encoder on the adapter `texture` lives on and
    /// sets it up for `width`x`height` NV12 at `fps`. Fails, rather than
    /// falling back to a software MFT, when there's none or none will start
    /// (session limits, remote desktop, driver trouble): the caller's
    /// fallback is OpenH264.
    pub fn new(texture: &ID3D11Texture2D, width: u32, height: u32, fps: u32) -> Result<Self> {
        let _mta = MtaGuard::new()?;
        ensure_mf_started()?;

        let device = unsafe { texture.GetDevice() }.context("Texture has no device")?;
        let luid = adapter_luid(&device)?;
        let activates = enumerate_encoders(luid)?;
        if activates.is_empty() {
            bail!("No hardware H.264 encoder on the capture adapter");
        }

        let mut errors = Vec::new();
        for activate in activates {
            let name = friendly_name(&activate).unwrap_or_else(|| "Unnamed H.264 encoder MFT".into());
            match Self::with_activate(&activate, &name, &device, width, height, fps) {
                Ok(encoder) => {
                    tracing::info!("Hardware encoder {name}: {width}x{height} @ {fps} fps");
                    return Ok(encoder);
                }
                Err(e) => {
                    tracing::info!("Hardware encoder {name} failed to start: {e:#}");
                    let _ = unsafe { activate.ShutdownObject() };
                    errors.push(format!("{name}: {e:#}"));
                }
            }
        }
        Err(anyhow!("No hardware H.264 encoder would start ({})", errors.join("; ")))
    }

    fn with_activate(
        activate: &IMFActivate,
        name: &str,
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self> {
        let mta = MtaGuard::new()?;
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.context("ActivateObject failed")?;
        let attributes = unsafe { transform.GetAttributes() }.context("MFT has no attributes")?;

        // We only know how to drive asynchronous MFTs, which every hardware
        // encoder is; they refuse all calls until explicitly unlocked.
        if unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 0 {
            bail!("MFT is synchronous");
        }
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .context("Failed to unlock async MFT")?;
        if let Err(e) = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) } {
            tracing::debug!("{name}: MF_LOW_LATENCY not accepted: {e}");
        }

        if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
            bail!("MFT isn't D3D11-aware");
        }
        let mut reset_token = 0;
        let mut device_manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager) }
            .context("MFCreateDXGIDeviceManager failed")?;
        let device_manager = device_manager.context("MFCreateDXGIDeviceManager returned nothing")?;
        unsafe { device_manager.ResetDevice(device, reset_token) }
            .context("IMFDXGIDeviceManager::ResetDevice failed")?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, device_manager.as_raw() as usize) }
            .context("MFT_MESSAGE_SET_D3D_MANAGER failed")?;

        let (input_stream, output_stream) = stream_ids(&transform)?;

        // Output type first, then input: encoders only offer input types
        // once they know what they're producing.
        let bitrate = bitrate_for(width, height, fps);
        let profiles = [eAVEncH264VProfile_ConstrainedBase.0, eAVEncH264VProfile_Base.0];
        let mut set_output = Err(anyhow!("no profile tried"));
        for profile in profiles {
            let output_type = output_media_type(width, height, fps, bitrate, profile as u32)?;
            set_output = unsafe { transform.SetOutputType(output_stream, &output_type, 0) }
                .with_context(|| format!("SetOutputType (profile {profile}) failed"));
            if set_output.is_ok() {
                break;
            }
        }
        set_output?;
        let input_type = input_media_type(width, height, fps)?;
        unsafe { transform.SetInputType(input_stream, &input_type, 0) }.context("SetInputType failed")?;

        let codec_api = transform.cast::<ICodecAPI>().ok();
        match &codec_api {
            Some(api) => configure_codec_api(api, name, bitrate, fps),
            None => tracing::info!("{name}: no ICodecAPI; using encoder defaults"),
        }

        let stream_info = unsafe { transform.GetOutputStreamInfo(output_stream) }
            .context("GetOutputStreamInfo failed")?;
        let output_provides_samples =
            stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        let (events, pump) = start_event_pump(&transform)?;

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .context("MFT_MESSAGE_COMMAND_FLUSH failed")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING failed")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM failed")?;
        }

        Ok(Self {
            name: name.to_owned(),
            width,
            height,
            fps: fps.max(1),
            transform,
            codec_api,
            input_stream,
            output_stream,
            output_provides_samples,
            output_buffer_size: stream_info.cbSize.max(width * height),
            events,
            pump: Some(pump),
            need_input: 0,
            in_flight: VecDeque::new(),
            max_lag: 0,
            first_timestamp: None,
            keyframe_requested: false,
            parameter_sets: None,
            _device_manager: device_manager,
            _mta: mta,
        })
    }

    /// Waits for the next MFT event until `deadline`. `None` on timeout.
    fn next_event(&self, deadline: Instant) -> Result<Option<(u32, HRESULT)>> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(timeout) {
            Ok(event) => event.map(Some),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("{}: event pump stopped", self.name),
        }
    }

    /// Acts on one MFT event, appending any output to `out`.
    fn handle_event(&mut self, (event_type, status): (u32, HRESULT), out: &mut Vec<u8>) -> Result<()> {
        if event_type == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if event_type == METransformHaveOutput.0 as u32 {
            if let Some(data) = self.process_output()? {
                out.extend_from_slice(&data);
                self.in_flight.pop_front();
            }
        } else if event_type == MEError.0 as u32 {
            bail!("{}: MFT reported an error: {}", self.name, windows::core::Error::from_hresult(status));
        }
        // Anything else (drain complete, markers, ...) we never asked for.
        Ok(())
    }

    fn submit(&mut self, frame: &EncoderInput) -> Result<()> {
        let buffer = unsafe {
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, frame.surface.texture(), 0, false)
        }
        .context("MFCreateDXGISurfaceBuffer failed")?;
        // Some encoders reject a buffer whose current length is 0, which is
        // what a fresh DXGI surface buffer reports.
        if let Ok(buffer_2d) = buffer.cast::<IMF2DBuffer>() {
            let length = unsafe { buffer_2d.GetContiguousLength() }.context("GetContiguousLength failed")?;
            unsafe { buffer.SetCurrentLength(length) }.context("SetCurrentLength failed")?;
        }

        let sample = unsafe { MFCreateSample() }.context("MFCreateSample failed")?;
        unsafe { sample.AddBuffer(&buffer) }.context("IMFSample::AddBuffer failed")?;
        let first = *self.first_timestamp.get_or_insert(frame.timestamp);
        let time = frame.timestamp.saturating_duration_since(first).as_nanos() / 100;
        unsafe {
            sample.SetSampleTime(time as i64).context("SetSampleTime failed")?;
            sample
                .SetSampleDuration(10_000_000 / self.fps as i64)
                .context("SetSampleDuration failed")?;
        }

        if std::mem::take(&mut self.keyframe_requested)
            && let Some(api) = &self.codec_api
            && let Err(e) = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) }
        {
            tracing::debug!("{}: force keyframe failed: {e}", self.name);
        }

        unsafe { self.transform.ProcessInput(self.input_stream, &sample, 0) }
            .context("ProcessInput failed")?;
        self.in_flight.push_back(frame.clone());
        Ok(())
    }

    /// Collects one output sample. `None` when the MFT renegotiated its
    /// output type instead of producing data.
    fn process_output(&mut self) -> Result<Option<Vec<u8>>> {
        let our_sample = if self.output_provides_samples {
            None
        } else {
            let sample = unsafe { MFCreateSample() }?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size) }?;
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
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // Take whatever output type the encoder now proposes (it
                // may have filled in the sequence header, say).
                let new_type = unsafe { self.transform.GetOutputAvailableType(self.output_stream, 0) }
                    .context("GetOutputAvailableType after stream change failed")?;
                unsafe { self.transform.SetOutputType(self.output_stream, &new_type, 0) }
                    .context("SetOutputType after stream change failed")?;
                tracing::debug!("{}: output type renegotiated", self.name);
                return Ok(None);
            }
            Err(e) => return Err(e).context("ProcessOutput failed"),
        }

        let sample = sample.context("ProcessOutput succeeded without a sample")?;
        Ok(Some(sample_bytes(&sample)?))
    }

    /// Puts SPS/PPS in front of an IDR that came without them, so a viewer
    /// joining or recovering mid-stream can decode from it. Remembers the
    /// latest ones the encoder did send, falling back to the output type's
    /// sequence header when it hasn't sent any inline.
    fn with_parameter_sets(&mut self, access_unit: Vec<u8>) -> Vec<u8> {
        let nals = split_nals(&access_unit);
        let types: Vec<u8> = nals.iter().filter_map(|n| nal_type(n)).collect();
        if types.contains(&NAL_SPS) && types.contains(&NAL_PPS) {
            self.parameter_sets = Some(
                nals.iter()
                    .filter(|n| matches!(nal_type(n), Some(NAL_SPS | NAL_PPS)))
                    .flat_map(|n| n.iter().copied())
                    .collect(),
            );
            return access_unit;
        }
        if !types.contains(&NAL_IDR) {
            return access_unit;
        }
        if self.parameter_sets.is_none() {
            self.parameter_sets = self.sequence_header();
        }
        match &self.parameter_sets {
            Some(sets) => {
                let mut out = Vec::with_capacity(sets.len() + access_unit.len());
                out.extend_from_slice(sets);
                out.extend_from_slice(&access_unit);
                out
            }
            None => {
                tracing::warn!("{}: IDR without SPS/PPS, and none to add", self.name);
                access_unit
            }
        }
    }

    fn sequence_header(&self) -> Option<Vec<u8>> {
        let output_type = unsafe { self.transform.GetOutputCurrentType(self.output_stream) }.ok()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0;
        unsafe { output_type.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut len) }.ok()?;
        let header = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
        unsafe { CoTaskMemFree(Some(ptr as *const _)) };
        Some(header)
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, frame: &EncoderInput) -> Result<Vec<u8>> {
        let mut out = Vec::new();

        // Catch up on anything that arrived since the last call (a NeedInput
        // sent after the last output, or output an encoder held back).
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(event?, &mut out)?;
        }

        let stall_deadline = Instant::now() + STALL_TIMEOUT;
        while self.need_input == 0 {
            match self.next_event(stall_deadline)? {
                Some(event) => self.handle_event(event, &mut out)?,
                None => bail!("{}: stalled waiting to accept input", self.name),
            }
        }
        self.submit(frame)?;
        self.need_input -= 1;

        let stall_deadline = Instant::now() + STALL_TIMEOUT;
        let lag_deadline = Instant::now() + LAG_PROBE;
        while self.in_flight.len() > self.max_lag {
            // Asking for more input while still owing output is normal for
            // a moment (some MFTs ask right away), but past `LAG_PROBE` it
            // means the encoder buffers frames regardless of low-latency
            // mode. Waiting would then cost a stall per frame; accept the
            // lag instead.
            let deadline = if self.need_input > 0 { lag_deadline } else { stall_deadline };
            match self.next_event(deadline)? {
                Some(event) => self.handle_event(event, &mut out)?,
                None if self.need_input > 0 => {
                    self.max_lag = self.in_flight.len();
                    tracing::info!("{}: holds {} frame(s) before output", self.name, self.max_lag);
                    break;
                }
                None => bail!("{}: stalled waiting for output", self.name),
            }
        }

        Ok(if out.is_empty() { out } else { self.with_parameter_sets(out) })
    }

    fn force_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
        // Shutting the MFT down is what wakes the pump thread out of its
        // blocking GetEvent (with MF_E_SHUTDOWN). Async MFTs are required to
        // support it; if one somehow doesn't, leave the pump thread parked
        // rather than hang here.
        match self.transform.cast::<IMFShutdown>() {
            Ok(shutdown) => {
                let _ = unsafe { shutdown.Shutdown() };
                if let Some(pump) = self.pump.take() {
                    let _ = pump.join();
                }
            }
            Err(_) => tracing::warn!("{}: no IMFShutdown; leaking its event thread", self.name),
        }
    }
}

/// Holds the process's multithreaded COM apartment open, which puts every
/// thread that hasn't initialised COM itself (the encoder thread, the event
/// pump) in it implicitly — what hardware MFTs expect.
struct MtaGuard(CO_MTA_USAGE_COOKIE);

impl MtaGuard {
    fn new() -> Result<Self> {
        let cookie = unsafe { CoIncrementMTAUsage() }.context("CoIncrementMTAUsage failed")?;
        Ok(Self(cookie))
    }
}

impl Drop for MtaGuard {
    fn drop(&mut self) {
        let _ = unsafe { CoDecrementMTAUsage(self.0) };
    }
}

/// `MFStartup`, once per process. Never paired with `MFShutdown`: an
/// encoder can be created again at any time, and the process exiting
/// cleans up.
fn ensure_mf_started() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow!("MFStartup failed: {e}"))
}

fn adapter_luid(device: &ID3D11Device) -> Result<LUID> {
    let dxgi_device: IDXGIDevice = device.cast().context("Device has no IDXGIDevice")?;
    let adapter = unsafe { dxgi_device.GetAdapter() }.context("IDXGIDevice::GetAdapter failed")?;
    let desc = unsafe { adapter.GetDesc() }.context("IDXGIAdapter::GetDesc failed")?;
    Ok(desc.AdapterLuid)
}

/// Hardware H.264 encoders taking NV12, best first, restricted to the
/// adapter with `luid` — the capture device's, since the MFT must read
/// textures that live there. `MFTEnum2` (Windows 10 1703+) filters by
/// adapter; where it's missing, `MFTEnumEx` lists every adapter's and we
/// hope the first is ours (the setup fails cleanly if it isn't).
fn enumerate_encoders(luid: LUID) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);

    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;

    let mut luid_bytes = [0u8; 8];
    luid_bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    luid_bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 1) }.context("MFCreateAttributes failed")?;
    let attributes = attributes.context("MFCreateAttributes returned nothing")?;
    unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes) }
        .context("Failed to set MFT_ENUM_ADAPTER_LUID")?;

    let enumerated = unsafe {
        MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &attributes, &mut array, &mut count)
    };
    if let Err(e) = enumerated {
        tracing::debug!("MFTEnum2 failed ({e}); falling back to MFTEnumEx");
        unsafe { MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut array, &mut count) }
            .context("MFTEnumEx failed")?;
    }

    let mut activates = Vec::with_capacity(count as usize);
    if !array.is_null() {
        for i in 0..count as usize {
            if let Some(activate) = unsafe { (*array.add(i)).take() } {
                activates.push(activate);
            }
        }
        unsafe { CoTaskMemFree(Some(array as *const _)) };
    }
    Ok(activates)
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut ptr = windows::core::PWSTR::null();
    let mut len = 0;
    unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len) }.ok()?;
    let name = unsafe { ptr.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(ptr.0 as *const _)) };
    name
}

/// Input and output stream IDs. Most MFTs use fixed IDs and say so with
/// `E_NOTIMPL`, which means 0 for both.
fn stream_ids(transform: &IMFTransform) -> Result<(u32, u32)> {
    let mut input = [0u32];
    let mut output = [0u32];
    match unsafe { transform.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => Ok((input[0], output[0])),
        Err(e) if e.code() == E_NOTIMPL => Ok((0, 0)),
        Err(e) => Err(e).context("GetStreamIDs failed"),
    }
}

/// Two 32-bit values packed the way `MF_MT_FRAME_SIZE` and friends expect.
fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// Attributes both media types share: size, rate, progressive, square
/// pixels, and the colour space the GPU video processor writes (BT.709,
/// studio range) — which becomes the stream's VUI, so the decoder converts
/// back with the same matrix.
fn video_media_type(subtype: &GUID, width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType failed")?;
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps.max(1), 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(media_type)
}

fn output_media_type(width: u32, height: u32, fps: u32, bitrate: u32, profile: u32) -> Result<IMFMediaType> {
    let media_type = video_media_type(&MFVideoFormat_H264, width, height, fps)?;
    unsafe {
        media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        // Constrained Baseline: the receiver decodes with OpenH264, which
        // is built around it (no CABAC, no B-frames).
        media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
        media_type.SetUINT32(&MF_MT_MPEG2_LEVEL, h264_level(width, height, fps))?;
    }
    Ok(media_type)
}

fn input_media_type(width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    video_media_type(&MFVideoFormat_NV12, width, height, fps)
}

/// The lowest H.264 level (as `level_idc`, e.g. 51 for 5.1, which is also
/// what `eAVEncH264VLevel` uses) whose macroblock rate and frame size limits
/// fit `width`x`height` at `fps`. Tops out at 5.2, OpenH264's maximum.
fn h264_level(width: u32, height: u32, fps: u32) -> u32 {
    // (level_idc, MaxMBPS, MaxFS) from table A-1 of the H.264 spec.
    const LEVELS: [(u32, u64, u64); 8] = [
        (31, 108_000, 3_600),
        (32, 216_000, 5_120),
        (40, 245_760, 8_192),
        (42, 522_240, 8_704),
        (50, 589_824, 22_080),
        (51, 983_040, 36_864),
        (52, 2_073_600, 36_864),
        (52, u64::MAX, u64::MAX),
    ];
    let frame_mbs = width.div_ceil(16) as u64 * height.div_ceil(16) as u64;
    let mbps = frame_mbs * fps.max(1) as u64;
    LEVELS
        .iter()
        .find(|&&(_, max_mbps, max_fs)| mbps <= max_mbps && frame_mbs <= max_fs)
        .map_or(52, |&(level, _, _)| level)
}

/// Low-latency, constant-bitrate, no B-frames, keyframes on request. Each
/// setting is best effort: vendors support different subsets, and an
/// encoder that ignores one still produces a stream we can use.
fn configure_codec_api(api: &ICodecAPI, name: &str, bitrate: u32, fps: u32) {
    let settings: [(&GUID, VARIANT, &str); 5] = [
        (&CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low-latency mode"),
        (
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            "CBR rate control",
        ),
        (&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate), "mean bitrate"),
        (&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "no B-frames"),
        (&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps.max(1) * GOP_SECONDS), "GOP size"),
    ];
    for (key, value, what) in settings {
        if let Err(e) = unsafe { api.SetValue(key, &value) } {
            tracing::info!("{name}: couldn't set {what}: {e}");
        }
    }
}

/// Starts the thread that blocks on the MFT's events and forwards their
/// type and status. It ends when the MFT is shut down (`GetEvent` then
/// fails) or the encoder, and with it the receiver, is gone.
fn start_event_pump(transform: &IMFTransform) -> Result<(Receiver<EventResult>, JoinHandle<()>)> {
    let generator: IMFMediaEventGenerator =
        transform.cast().context("MFT has no IMFMediaEventGenerator")?;
    let generator = SendCom(generator);
    let (tx, rx): (Sender<EventResult>, _) = std::sync::mpsc::channel();
    let pump = std::thread::Builder::new()
        .name("mf-events".into())
        .spawn(move || {
            let generator = generator;
            loop {
                let event = unsafe { generator.0.GetEvent(MF_EVENT_FLAG_NONE) }.and_then(|event| unsafe {
                    Ok((event.GetType()?, event.GetStatus()?))
                });
                let failed = event.is_err();
                if tx.send(event.context("IMFMediaEventGenerator::GetEvent failed")).is_err() || failed {
                    return;
                }
            }
        })
        .context("Failed to spawn MFT event thread")?;
    Ok((rx, pump))
}

/// Lets the MFT's event generator move to the pump thread. Hardware MFTs
/// live in the MTA and are free-threaded, so calling them from another
/// thread is fine; the `windows` crate just doesn't mark these interfaces
/// `Send`.
struct SendCom<T>(T);
unsafe impl<T> Send for SendCom<T> {}

/// Copies out a sample's data, whatever buffers it's split across.
fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }.context("ConvertToContiguousBuffer failed")?;
    let mut ptr = std::ptr::null_mut();
    let mut len = 0;
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) }.context("IMFMediaBuffer::Lock failed")?;
    let data = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
    let _ = unsafe { buffer.Unlock() };
    Ok(data)
}

/// Splits an Annex-B stream into NAL units, each with its start code (3 or
/// 4 bytes) still attached.
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| &data[start..starts.get(k + 1).copied().unwrap_or(data.len())])
        .collect()
}

/// The type of a NAL unit that still has its start code attached.
fn nal_type(nal: &[u8]) -> Option<u8> {
    let payload = nal.iter().position(|&b| b == 1)? + 1;
    nal.get(payload).map(|b| b & 0x1f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nals_with_3_and_4_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC, 0xDD];
        let nals = split_nals(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0, 0, 0, 1, 0x67, 0xAA]);
        assert_eq!(nals[1], &[0, 0, 1, 0x68, 0xBB]);
        assert_eq!(nals[2], &[0, 0, 0, 1, 0x65, 0xCC, 0xDD]);
        let types: Vec<_> = nals.iter().filter_map(|n| nal_type(n)).collect();
        assert_eq!(types, [NAL_SPS, NAL_PPS, NAL_IDR]);
    }

    #[test]
    fn picks_lowest_level_that_fits() {
        assert_eq!(h264_level(1280, 720, 30), 31);
        assert_eq!(h264_level(1920, 1088, 30), 40);
        assert_eq!(h264_level(1920, 1088, 60), 42);
        assert_eq!(h264_level(2560, 1392, 60), 51);
        assert_eq!(h264_level(3840, 2160, 60), 52);
        assert_eq!(h264_level(7680, 4320, 60), 52);
    }

    /// Captures the primary monitor, encodes it on the hardware encoder and
    /// decodes the result with OpenH264, the way the receiver will. Needs an
    /// interactive desktop and a GPU with an H.264 encoder MFT:
    /// `cargo test --bin screenshare codec::mf_encoder::tests -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn hardware_encodes_real_frames_openh264_can_decode() {
        use crate::capture::{CaptureTarget, session::start_capture};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let monitor = windows_capture::monitor::Monitor::primary().expect("primary monitor");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let paused = Arc::new(AtomicBool::new(false));
        let session = start_capture(CaptureTarget::Monitor(monitor), tx, paused, 30, 1920, 1080)
            .expect("start_capture should succeed on a real desktop session");

        let first = rx.recv_timeout(Duration::from_secs(5)).expect("expected a frame within 5s");
        let mut encoder = MfEncoder::new(first.surface.texture(), first.width, first.height, 30)
            .expect("a hardware H.264 encoder should be available");
        println!("encoder: {}", encoder.name());
        let mut decoder = crate::codec::H264Decoder::new().expect("decoder");

        let mut frames = vec![first];
        let mut decoded = 0;
        let mut idrs = 0;
        for i in 0..60 {
            let frame = match frames.pop() {
                Some(frame) => frame,
                None => rx.recv_timeout(Duration::from_secs(5)).expect("expected a frame within 5s"),
            };
            if i == 30 {
                encoder.force_keyframe();
            }
            let start = Instant::now();
            let h264 = encoder.encode(&frame).expect("hardware encode should succeed");
            println!("frame {i}: {} bytes in {:?}", h264.len(), start.elapsed());
            drop(frame);
            if h264.is_empty() {
                continue;
            }
            if crate::diag::has_idr(&h264) {
                idrs += 1;
                let types: Vec<_> = split_nals(&h264).iter().filter_map(|n| nal_type(n)).collect();
                assert!(
                    types.contains(&NAL_SPS) && types.contains(&NAL_PPS),
                    "IDR without SPS/PPS: {types:?}"
                );
                let sps = split_nals(&h264).into_iter().find(|n| nal_type(n) == Some(NAL_SPS)).unwrap();
                let header = sps.iter().position(|&b| b == 1).unwrap() + 1;
                // profile_idc 66 (Baseline) with constraint_set1 (0x40):
                // Constrained Baseline, what OpenH264 is built around.
                let (profile_idc, constraints) = (sps[header + 1], sps[header + 2]);
                println!("SPS profile_idc {profile_idc}, constraint flags {constraints:#04x}");
                assert_eq!(profile_idc, 66, "encoder ignored the Baseline profile request");
            }
            if let Some((w, h, _)) = decoder.decode(&h264).expect("OpenH264 should decode the hardware stream") {
                assert_eq!((w, h), (encoder.width, encoder.height));
                decoded += 1;
            }
        }
        session.stop();

        assert!(idrs >= 2, "expected the first frame and the forced one to be IDRs, got {idrs}");
        assert!(decoded >= 50, "only {decoded} of 60 frames decoded");
    }
}
