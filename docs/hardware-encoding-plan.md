# Hardware video encoding plan

Date: 2026-09-18
Status: **Draft.** Phase 0 done (2026-09-19). Phase 1 done (2026-09-19). Phase 2 done (2026-09-19). Phase 3 landed (2026-09-23), verified on AMD only. Phase 4 landed (2026-09-23). Phase 5 landed (2026-09-23), verified on AMD only. Phase 6 not started.

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

### Phase 0: prerequisites and measurements (S) — done

- Added the `windows` features we need to `Cargo.toml`:
  `Win32_Media_MediaFoundation`, `Win32_Graphics_Direct3D`,
  `Win32_Graphics_Direct3D11`, `Win32_Graphics_Dxgi`,
  `Win32_Graphics_Dxgi_Common`, `Win32_System_WinRT_Direct3D11`,
  `Win32_System_WinRT_Graphics_Capture`, `Graphics_Capture`,
  `Graphics_DirectX_Direct3D11`, `Win32_System_Variant` (ICodecAPI values).
  Confirmed the crate resolves and the app still builds and tests clean.
- **Data channel throughput check.** Added `src/synthetic_load.rs`, a
  debug-only headless mode (`--synthetic-kb`, `--synthetic-fps`,
  `--synthetic-secs`, alongside the existing `--server`/`--client`/`--pin`)
  that runs the real signaling handshake and peer connection, then feeds
  the existing `peer::spawn_sender` fixed-size frames at a fixed rate
  instead of capturing and encoding.
  - Two local instances, real signaling server, loopback ICE
    (`local=host remote=host relay=false`).
  - At **17 KB @ 60 fps (~8.2 Mbit/s)**: `drop_buffered 0`, max buffered
    ~34 KB. Clean.
  - At **31 KB @ 60 fps (~15 Mbit/s)**, the top of the range a hardware
    encoder would want: `drop_buffered` ~8 of every 60 frames (~13%),
    max buffered pinned near the 256 KB cap
    (`MAX_BUFFERED_AMOUNT` in `transport/peer.rs`).
  - At 40 KB @ 60 fps (~19 Mbit/s) drops rise further (~30%).
  - **Conclusion: the data channel can't sustain the top of the target
    range yet.** It's fine low in the range but degrades before 15 Mbit/s
    even on loopback with no encode cost in the loop, confirming the
    concern above — a faster encoder alone would just move the
    bottleneck to the transport. This is a separate problem to fix first
    (see `docs/webrtc-upgrade-investigation.md`); phases 1–3 should not
    raise `MAX_BITRATE` until it's addressed.

### Phase 1: our own WGC capture on a video-capable device (M) — done

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

Landed as `src/capture/session.rs`, replacing `src/capture/wgc.rs`. Cursor-on
and border-off are applied best-effort (logged at debug if unsupported); the
slack-based rate cap in the frame handler stays regardless of whether
`SetMinUpdateInterval` succeeds, rather than being an either/or fallback.
`Recreate` on the WinRT `IDirect3DDevice` needed the same `SendDirectX`-style
`Send` opt-in `windows-capture` uses internally, plus forcing capture of the
whole wrapper (Rust 2021 disjoint closure capture otherwise reaches past it
and grabs the non-`Send` field directly). Verified with a live capture test
against the primary monitor (`cargo test --bin screenshare
capture::session::tests -- --ignored`) and a manual run of the app.

### Phase 2: GPU scale and colour conversion (M) — done

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

Landed as `src/capture/video_processor.rs`. The capture thread creates a
`VideoProcessor` per (input size, output size) pair (recreated on window
resize) and, per frame, `VideoProcessorBlt`s the captured BGRA texture into
the next of 3 ring-buffered NV12 textures, then reads that back to two CPU
planes (`Y`, interleaved `UV`) — the only readback left, and far smaller than
the old full-size BGRA one. `capture::scale_bgra` is gone; the aspect-fit
sizing logic moved to `capture::aspect_fit_16`, rounding up to multiples of
16 instead of 2 (the crop via `MF_MT_MINIMUM_DISPLAY_APERTURE` is deferred to
phase 3 — for now the fallback stretches into the padding rather than
cropping it, since nothing consumes the aperture metadata yet).
`codec::H264Encoder::encode` now takes the Y/UV planes directly; a small
`Nv12Yuv` adapter de-interleaves chroma into OpenH264's `YUVSource` shape (one
cheap pass, no BGRA → I420 conversion). The outgoing "you are sharing"
preview thumbnail is generated straight from NV12 (`capture::nv12_preview_rgba`)
since the encoder-input frame is no longer BGRA. `ENC_SCALE_US` stays in
`diag` (now always 0, since the scale happens upstream on the GPU); a new
`CAP_GPU_US`/`CAP_GPU_MAX_US` pair tracks the Blt itself, logged alongside
readback in the capture pipeline line. Verified with a live capture test that captures the primary monitor, runs it
through the real video processor, and feeds the resulting NV12 planes to a
real `H264Encoder` (`cargo test --bin screenshare capture::session::tests --
--ignored`). Not yet re-run: the two-local-instances stress test from phase 0
to confirm the expected latency drop end to end.

### Phase 3: Media Foundation hardware encoder (L) — landed, AMD verified

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

Landed as `src/codec/mf_encoder.rs` (`MfEncoder`), with the trait in
`src/codec/mod.rs`. Differences from the plan above:

- **`EncoderInput` is `capture::Frame`, always a GPU texture.** There's no
  CPU-planes variant: nothing produces CPU frames today (the capture always
  runs through the video processor). The frame carries an `Nv12Surface`, a
  lease on one of the video processor's 3 ring textures; the ring skips a
  texture while any clone of its lease is alive and drops the frame
  (`drop_queue_full`) if all 3 are held. `MfEncoder` keeps a clone of each
  frame until its output comes out, so the capture thread can't overwrite a
  texture the encoder is still reading. OpenH264 (`H264Encoder`, now also a
  `VideoEncoder`) calls `Frame::read_back` itself, so the NV12 readback moved
  from the capture thread to the encoder thread and is skipped entirely on
  the hardware path. Its diag counters moved with it (`readbacks` in the
  `encode` line); the preview still reads back once a second.
- **The trait isn't `Send`** and has a `size()` method. Encoders are
  created and used on the encoder thread only.
- **The 2 s keyframe timer moved to `app.rs`**, which calls
  `force_keyframe()` on either encoder once 2 s pass without an IDR in the
  output. `CODECAPI_AVEncMPVGOPSize` is `fps * 4`, a backstop for an encoder
  that ignores forced keyframes, rather than "large".
- **Enumeration:** `MFTEnum2` filtered by `MFT_ENUM_ADAPTER_LUID` (the capture
  texture's adapter), falling back to `MFTEnumEx`. Every candidate is tried
  in order, not just the first, so one hitting a session limit falls through
  to the next.
- **Event handling:** a small `mf-events` thread blocks on `GetEvent` and
  forwards events over a channel, so `encode` waits with timeouts (2 s
  stall → error). `encode` submits one frame and waits for its output. If
  the MFT asks for more input while still owing output for 250 ms, the
  encoder is treated as buffering frames and `encode` stops waiting for
  that many frames of lag from then on. Teardown sends end-of-stream, then
  `IMFShutdown::Shutdown` to wake and end the pump thread.
- **Rate control:** CBR only. Low-delay VBR hasn't been tried.
- **Profile:** asks for Constrained Baseline, falling back to Baseline if the
  output type is refused. Level is the lowest that fits the size and rate
  (`h264_level`). VUI is BT.709 primaries, transfer and matrix, studio range,
  matching the video processor's output.
- **SPS/PPS:** cached whenever the encoder emits them, and prepended to any
  IDR that comes without them (falling back to `MF_MT_MPEG_SEQUENCE_HEADER`).
- **Not done: the `MF_MT_MINIMUM_DISPLAY_APERTURE` crop.** It's still deferred
  from phase 2. The video processor still stretches into the ≤15 px of
  16-alignment padding, because OpenH264 (the fallback) has no cropping to
  match.
- **Selection was opt-in** through a `SCREENSHARE_HW_ENCODER` environment
  variable until phase 4 replaced it with a setting.

Verified with `cargo test --bin screenshare codec::mf_encoder::tests --
--ignored --nocapture` on an AMD GPU (`AMDh264Encoder`). The test captures
the primary monitor at a 1920×1080 target and encodes 60 frames at
**~4–6 ms each (first keyframe 8 ms)**, compared with 100–150 ms for
OpenH264. The SPS reports profile_idc 66 with constraint_set1 (Constrained
Baseline), and both the first IDR and a forced one carry SPS/PPS. OpenH264
decodes every frame. **Still to do before calling phase 3 done:** the same
test on NVIDIA and Intel, and the two-instance 1440p60 stress test.

### Phase 4: selection, fallback, settings (S) — landed

- `app.rs` encoder thread: try the hardware encoder, and on any error during
  set-up or the first frames, log why and fall back to OpenH264 for the rest
  of the session.
- Settings → "Video encoder: Automatic / Software only" (default Automatic),
  persisted alongside resolution and frame rate.
- Stats panel and diag log show which encoder is running.
- Raise `MAX_BITRATE` only once phase 0 shows the data channel can carry it.

Landed as `EncoderSelector` in `src/app.rs`, which owns the encoder thread's
encoder. The `SCREENSHARE_HW_ENCODER` variable is gone. Details:

- **Fallback triggers:** `MfEncoder::new` failing (no hardware encoder,
  session limit, remote desktop), any `encode` error at any point in the
  session (not only the first frames; the 2 s stall timeout is the usual
  one), or no output at all after `HW_FIRST_OUTPUT_WITHIN` (30) frames. Any
  of these switches to OpenH264 for the rest of the session, including
  after later resolution changes. On an encode failure the same frame is
  re-encoded with OpenH264, so no frame is lost. The first OpenH264 frame
  is an IDR, so the viewer recovers straight away.
- **Setting:** `EncoderChoice` in `viewer/settings.rs`, carried to the
  session as `Quality::hardware_encoder`. Settings saved before this change
  load as Automatic (`#[serde(default)]`). Like resolution and frame rate,
  it applies from the next session.
- **Stats panel:** an "Encoder" row under Sending: Hardware, Software, or
  "Software (fallback)" when Automatic couldn't use the hardware encoder
  (`SessionStats::encoder`). The encoder's own name goes to the log
  (`Encoding WxH with <name> (hardware|software)`) and to the diag
  `encode [<name>]` line, not the panel.
- **`MAX_BITRATE` stays at 10 Mbit/s.** The phase 0 data channel limit
  still applies.

Not yet verified by hand: forcing each fallback trigger in a live session,
and the Settings toggle end to end. Unit tests cover the setting's default
and migration, and the stats label.

### Phase 5: receive side (M–L) — landed, AMD verified

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

Landed as `src/codec/mf_decoder.rs` (`MfDecoder`) and
`src/viewer/remote_screen.rs` (`RemoteScreen`), with a `VideoDecoder` trait
and an `Nv12Frame` type (tightly packed Y and interleaved UV planes) in
`src/codec/mod.rs`. Differences from the plan above, and details:

- **NV12 all the way to the GPU.** Both decoders produce `Nv12Frame`.
  OpenH264's strided I420 is packed into NV12 in one pass (Y rows copied,
  U/V interleaved), replacing `write_rgba8`. The slot the GUI takes frames
  from holds `Nv12Frame`; `DecodedFrame` (RGBA) is left only for the small
  outgoing preview.
- **A render pass, not a paint callback.** The GUI thread uploads Y
  (`R8Unorm`) and UV (`Rg8Unorm`) with `queue.write_texture`, then one render
  pass converts them (BT.709 studio range, matching the sender's video
  processor and VUI) into an `Rgba8Unorm` texture registered with egui via
  `register_native_texture`. egui draws that as an ordinary image, so Fit and
  Actual zoom and the scroll area are unchanged. A paint callback would have
  needed a viewport reaching past the window at Actual zoom, which wgpu
  rejects. The texture ID is registered once and re-pointed at the new
  texture when the size changes. Without a wgpu render state (which can't
  happen in this build) it falls back to converting to RGBA on the CPU.
- **Decoder:** Microsoft's H.264 decoder MFT (`CLSID_MSH264DecoderMFT`), which
  is synchronous, with `MF_LOW_LATENCY` and `CODECAPI_AVLowLatencyMode` so it
  doesn't hold frames back for reordering. It gets its own D3D11 device
  (`capture::session::create_device`) through an `IMFDXGIDeviceManager`, so it
  decodes with DXVA. Each output texture (one slice of the decoder's texture
  array) is copied to a cached staging texture with `CopySubresourceRegion`
  and read back as NV12, cropped to `MF_MT_MINIMUM_DISPLAY_APERTURE`. If the
  MFT refuses the device, it decodes in software into system memory
  (`IMF2DBuffer::Lock2D`), which is still multithreaded.
  `MF_E_TRANSFORM_STREAM_CHANGE` re-picks NV12 output and re-reads the
  layout. When one input yields several pictures, only the newest is kept
  (counted in `overwritten before display`).
- **Selection and fallback:** `DecoderSelector` in `app.rs`, modelled on
  `EncoderSelector`. If the MFT fails to start, returns any decode error, or
  produces no picture within `HW_FIRST_OUTPUT_WITHIN` (30) frames of the
  first IDR, the session switches to OpenH264 for good. The frame that failed
  is re-fed to OpenH264; otherwise the picture comes back at the next IDR
  (≤ 2 s).
- **Setting:** the phase 4 setting now covers both directions and is
  relabelled "Video codec" (Automatic / Software only). Software only means
  OpenH264 both ways. `Quality::hardware_encoder` is now `hardware_video`.
  The saved field is still `encoder`, so existing settings carry over.
- **Stats:** a "Decoder" row under Receiving (Hardware / Software /
  Software (fallback)), from `SessionStats::decoder`. `EncoderInfo` is now
  `CodecInfo`, shared by both rows. The diag `decode [<name>]` line names the
  decoder.

Verified on an AMD GPU (RX 7900 XTX):

- `cargo test --bin screenshare codec::mf_decoder::tests -- --ignored
  --nocapture` captures the primary monitor at 1920×1080, encodes with
  OpenH264 and decodes with the MFT on DXVA. All 30 frames decoded, one out
  per one in, at **~3–4 ms each in a debug build** (first IDR 17 ms), with
  luma within a few levels of the source.
- A GPU unit test (`viewer::remote_screen::tests`) checks the shader's BT.709
  conversion against known white and red.
- In a two-instance session on one machine (debug build, sharing a Notepad
  window), the receiver picked `Microsoft H.264 decoder (DXVA)`, decoded at
  3.1–4.9 ms average, and the GUI upload fell to ~0.9 ms.

**Not yet done:** NVIDIA and Intel, the 1440p60 stress test, forcing each
decoder fallback in a live session, and checking colours by eye in the
running app (only the unit test checks them).

### Phase 6: follow-ups (S each)

- A 4K / "native" resolution option once 1440p60 holds.
- Use `GraphicsCaptureFrame` dirty regions to skip encoding unchanged frames.
- Remove `diag` counters that are no longer useful, or keep them as a
  debug-only feature.

## Risks and open questions

- **Profile compatibility with the OpenH264 decoder.** Since phase 5 the
  receiver decodes with Media Foundation where it can, but OpenH264 is still
  the fallback, which is built around Constrained
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
