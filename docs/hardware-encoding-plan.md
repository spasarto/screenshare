# Hardware video encoding plan

Date: 2026-09-18
Status: **Draft.** Not started.

## Why

A 1440p60 stress test between two local instances averaged 2–9 fps with the
machine nearly idle. The per-stage counters in `src/diag.rs` show the network
is not involved (no send-side drops, send buffer never above 1 KB, ~1–3 Mbit/s).
The limit is CPU work done one frame at a time on single threads:

Measured on a 4K display (`captured 3840x2088`, encoded at `2560x1392`),
release build, OpenH264 after tuning (`skip_frames(false)`, low complexity,
screen-content mode):

| Stage | Where | Cost per frame |
|---|---|---|
| GPU → CPU readback + two copies of a 32 MB BGRA frame | capture thread | 15–21 ms |
| Nearest-neighbour downscale, per pixel (`capture::scale_bgra`) | encoder thread | 10–13 ms |
| BGRA → I420 conversion + OpenH264 encode | encoder thread | 100–150 ms, keyframes 400–900 ms |
| OpenH264 decode + I420 → RGBA | receiver's decoder thread | 15–26 ms |
| RGBA texture upload (14 MB) | GUI thread | 5–10 ms |

60 fps allows 16.7 ms per frame per stage. Encoding is 6–10× over that, and
OpenH264 can't close the gap: the `openh264` crate only exposes single-slice
or size-limited slicing, so its thread pool never runs frames in parallel.
In testing, size-limited slices made encoding slower, not faster.

A GPU hardware encoder (NVENC, Intel Quick Sync or AMD AMF) does the
colour conversion, the scaling and the encode on the GPU, with no readback.
The frame never leaves video memory until it is a few tens of KB of H.264.

Decoding at ~20 ms is *also* over budget for 60 fps, so the receive side is in
scope too (phase 5), though it matters less than encoding.

## Approach

Use **Media Foundation** (`windows` crate, `Win32_Media_MediaFoundation`),
which is how Windows exposes every vendor's hardware H.264 encoder behind one
API. We don't need vendor SDKs (NVENC/AMF/oneVPL) or new native dependencies.

Options considered and rejected:

- **`windows-capture`'s `VideoEncoder`.** Built on `MediaTranscoder`, which
  writes to a file or container stream. There's no frame-by-frame output, no
  way to force a keyframe, and latency is unpredictable. It suits recording,
  not live streaming.
- **Vendor SDKs directly.** Three code paths, three native dependencies, and
  no gain over Media Foundation, which drives the same hardware.
- **FFmpeg (`ffmpeg-next`, h264_nvenc / h264_qsv / h264_amf).** Works, but
  ships a large native dependency and still needs D3D11 interop to avoid
  readback.

OpenH264 stays as the **fallback**: machines with no hardware encoder,
remote-desktop sessions, broken drivers, and any failure while the hardware
encoder is starting up.

## Target pipeline

```
WGC frame (BGRA, D3D11 texture, native size)
  └─ ID3D11VideoProcessor: scale + BGRA→NV12 in one GPU pass      [capture thread]
      └─ NV12 texture from a small ring
          ├─ hardware path: IMFSample(DXGI buffer) → H.264 MFT    [encoder thread]
          └─ fallback:      readback NV12 (≈ 5 MB, not 32 MB)
                            → OpenH264 from YUV planes            [encoder thread]
                              (no CPU scaling, no CPU colour conversion)
  → Annex-B H.264 → existing sender / framing / data channel (unchanged)
```

The fallback benefits from phases 1–2 as well: readback shrinks ~6× and the
CPU scale and colour-conversion steps disappear.

## Phases

Each phase ships on its own and leaves the app working.

### Phase 0: prerequisites and measurements (S)

- Add the `windows` features we need: `Win32_Media_MediaFoundation`,
  `Win32_Graphics_Direct3D`, `Win32_Graphics_Direct3D11`, `Win32_Graphics_Dxgi`,
  `Win32_Graphics_Dxgi_Common`, `Win32_System_WinRT_Direct3D11`,
  `Win32_System_WinRT_Graphics_Capture`, `Graphics_Capture`,
  `Graphics_DirectX_Direct3D11`, `Win32_System_Variant` (ICodecAPI values).
- **Data channel throughput check.** Nothing has pushed the webrtc-rs SCTP
  channel past ~3 Mbit/s yet. A hardware encoder at 1440p60 will want
  8–15 Mbit/s. Add a debug-only mode that feeds the existing sender
  synthetic frames of a set size and rate, then read `send avg`,
  `drop_buffered` and `max buffered` from the diag log. If it can't keep
  up, that's a separate problem to fix first (see
  `docs/webrtc-upgrade-investigation.md`), because a faster encoder
  would just move the bottleneck.

### Phase 1: our own WGC capture on a video-capable device (M)

`windows-capture` creates its D3D11 device with `D3D11_CREATE_DEVICE_BGRA_SUPPORT`
only. Media Foundation's `IMFDXGIDeviceManager` and `ID3D11VideoDevice` expect
`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, and the crate has no way to pass in our
own device. So:

- New `capture/session.rs`: create the device ourselves
  (`BGRA_SUPPORT | VIDEO_SUPPORT`, hardware driver, default adapter), turn on
  `ID3D11Multithread::SetMultithreadProtected(true)` (capture thread and
  encoder MFT share the immediate context), and wrap it with
  `CreateDirect3D11DeviceFromDXGIDevice` for WinRT.
- `Direct3D11CaptureFramePool::CreateFreeThreaded` +
  `GraphicsCaptureSession`, with the behaviour the current code sets through
  `windows-capture`: cursor on (`IsCursorCaptureEnabled`), border off
  (`IsBorderRequired`, where supported), and `MinUpdateInterval` where the OS
  has it (else keep the existing slack-based cap in `wgc.rs`).
- Recreate the frame pool when `ContentSize` changes (window resized).
- Keep `windows-capture` for the picker and thumbnails. Only session capture
  moves.
- For this phase, keep the CPU readback so behaviour is identical. It's pure
  plumbing, to be verified against the diag numbers.

### Phase 2: GPU scale and colour conversion (M)

- `ID3D11VideoDevice` / `ID3D11VideoContext` → one `ID3D11VideoProcessor`
  per (input size, output size) pair. `VideoProcessorBlt` BGRA → NV12 with
  scaling, output colour space BT.709 limited range (set it explicitly,
  and set the matching VUI on the encoder so colours don't shift).
- Output size: same aspect-fit rule as `capture::scale_bgra`, rounded to
  **multiples of 16** in the encoded size, with the padding cropped via
  `MF_MT_MINIMUM_DISPLAY_APERTURE` (hardware encoders commonly require
  16-aligned surfaces; 2088 isn't).
- Ring of 3 NV12 output textures. A texture goes back to the ring only once
  the encoder has released it (see phase 3), and never while the MFT still
  holds its sample.
- Fallback path: copy NV12 to a staging texture, map, and feed OpenH264 the
  Y/UV planes directly (a `YUVSource` over NV12, or one CPU pass NV12 → I420,
  which is far cheaper than BGRA → I420). Delete `scale_bgra` from the
  session path once nothing uses it.
- Expected result, before any hardware encoding: readback ~3–5 ms,
  `scale avg` 0, OpenH264 encode somewhat faster (no RGB conversion).

### Phase 3: Media Foundation hardware encoder (L)

New `codec/mf_encoder.rs`, behind a small trait shared with OpenH264:

```rust
pub trait VideoEncoder: Send {
    fn encode(&mut self, frame: &EncoderInput) -> Result<Vec<u8>>; // Annex-B
    fn force_keyframe(&mut self);
    fn name(&self) -> &str; // for logs: "NVIDIA H.264 Encoder MFT", "OpenH264"
}
```

`EncoderInput` is either a GPU NV12 texture or CPU NV12 planes.

Setup:

- `MFStartup` once per process; COM MTA on the encoder thread.
- `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE |
  MFT_ENUM_FLAG_SORTANDFILTER, NV12 in, H264 out)`. Take the first one,
  log its `MFT_FRIENDLY_NAME_Attribute`, and prefer the one on the capture
  device's adapter (`MFT_ENUM_ADAPTER_LUID`).
- Hardware MFTs are **asynchronous**: set `MF_TRANSFORM_ASYNC_UNLOCK`, then
  drive them with `IMFMediaEventGenerator::GetEvent`
  (`METransformNeedInput` → `ProcessInput`, `METransformHaveOutput` →
  `ProcessOutput`) on the encoder thread.
- `MFT_MESSAGE_SET_D3D_MANAGER` with an `IMFDXGIDeviceManager` wrapping the
  capture device.
- Output type first, then input type: H264, frame size, frame rate,
  `MF_MT_AVG_BITRATE`, `MF_MT_INTERLACE_MODE` progressive, profile, level.
- `ICodecAPI`:
  - `CODECAPI_AVLowLatencyMode` = true (one frame in, one frame out)
  - `CODECAPI_AVEncCommonRateControlMode` = CBR or low-delay VBR (try both)
  - `CODECAPI_AVEncCommonMeanBitRate` from `bitrate_for`
  - `CODECAPI_AVEncMPVDefaultBPictureCount` = 0
  - `CODECAPI_AVEncMPVGOPSize` set large, with keyframes driven by the
    existing 2 s timer through `CODECAPI_AVEncVideoForceKeyFrame` (check what
    each vendor does with 0, which may mean "encoder's choice")
- Input samples: `MFCreateDXGISurfaceBuffer(NV12 texture)` →
  `IMFSample`, with time and duration in 100 ns units from the capture
  timestamp.

Output:

- Concatenate the output sample's buffer: Media Foundation's H.264 encoders
  emit Annex-B with start codes.
- **SPS/PPS before every IDR.** A viewer that joins or recovers mid-stream
  needs them. If the encoder only reports them once
  (`MF_MT_MPEG_SEQUENCE_HEADER` on the output type), cache them and prepend
  to each IDR ourselves. Check with `diag::has_idr` plus a NAL type 7/8 scan.
- Resolution change: tear down and re-create, as the OpenH264 path does now.

### Phase 4: selection, fallback, settings (S)

- `app.rs` encoder thread: try the hardware encoder, and on any error during
  set-up or the first frames, log why and fall back to OpenH264 for the rest
  of the session.
- Settings → "Video encoder: Automatic / Software only" (default Automatic),
  persisted alongside resolution and frame rate.
- Stats panel and diag log show which encoder is running.
- Raise `MAX_BITRATE` only once phase 0 shows the data channel can carry it.

### Phase 5: receive side (M–L)

At 60 fps the receiver's ~20 ms OpenH264 decode and ~7 ms 14 MB RGBA upload
are over budget. In order of payoff:

1. **Skip the CPU RGBA step.** Upload NV12 planes as two textures and convert
   in an egui/wgpu paint callback shader. That's 5 MB instead of 14 MB per
   frame, with no CPU colour conversion.
2. **Hardware decode** through the Media Foundation H.264 decoder MFT, output to
   system-memory NV12, feeding the same upload path. Keeping frames on the GPU
   end to end would mean D3D11 → wgpu texture interop, which isn't worth it
   yet.

Keep OpenH264 as the decoder fallback.

### Phase 6: follow-ups (S each)

- A 4K / "native" resolution option once 1440p60 holds.
- Use `GraphicsCaptureFrame` dirty regions to skip encoding unchanged frames.
- Remove `diag` counters that are no longer useful, or keep them as a
  debug-only feature.

## Risks and open questions

- **Profile compatibility with the OpenH264 decoder.** Until phase 5 lands,
  the receiver decodes with OpenH264, which is built around Constrained
  Baseline. Hardware encoders default to Main/High (CABAC, B-frames). Ask for
  `eAVEncH264VProfile_ConstrainedBase` (B-frames already off). Verify every
  vendor honours it, and check whether OpenH264 decodes Main/High well enough
  to relax this.
- **Level.** 2560×1392 @ 60 needs level 5.1; 3840×2160 @ 60 needs 5.2, which
  is OpenH264's maximum.
- **Multi-GPU laptops.** Capture, video processor and encoder must share one
  adapter. The default adapter usually drives the internal panel. A window on
  an external monitor wired to the discrete GPU may need the capture device
  created on that adapter instead.
- **Encoder session limits.** Consumer NVIDIA drivers limit concurrent NVENC
  sessions. Two local instances plus OBS or a game recorder could hit it.
  Treat "create failed" as a normal fallback case, not an error dialog.
- **Async MFT quirks.** Some vendors emit extra `METransformNeedInput` events,
  need `MFT_MESSAGE_NOTIFY_START_OF_STREAM`, or stall without
  `MFT_MESSAGE_COMMAND_DRAIN` on teardown. Test on NVIDIA, Intel and AMD
  before calling phase 3 done.
- **Remote Desktop / no GPU.** `MFTEnumEx` returns nothing, and the OpenH264
  path must keep working with the phase 2 video processor unavailable, so the
  pure-CPU path needs to stay (possibly via WARP).
- **Data channel throughput** (phase 0). If webrtc-rs can't sustain the
  bitrate, the fix is on the transport side, not here.

## How we'll know it worked

Same test as the baseline (4K source, 1440p60 setting, two local instances,
moving content), read from the `pipeline` log lines:

- `capture`: `drop_queue_full` near 0, readback gone (hardware path).
- `encode`: `avg` under 8 ms, `out` about equal to `in`, `idr` about 0.5/s.
- `send`: `drop_buffered` 0 at the target bitrate.
- `decode` + `gui`: 60 frames a second after phase 5.
