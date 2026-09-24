//! GPU scale + BGRA→NV12 colour conversion via `ID3D11VideoProcessor`
//! (phase 2 of `docs/hardware-encoding-plan.md`). Runs on the capture
//! thread right after a WGC frame arrives, so only a small NV12 surface —
//! not the full-size BGRA capture — ever needs a CPU readback.

use anyhow::{Context as _, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::core::Interface;

/// Output textures kept in flight. Lets the Blt for one frame start while an
/// earlier frame's texture is still being read back or (once phase 3 lands)
/// held by the hardware encoder.
const RING_SIZE: usize = 3;

/// Scales and colour-converts captured BGRA into NV12, entirely on the GPU.
/// One instance per (input size, output size) pair; recreate when either
/// changes (window resize, or a different target resolution).
pub struct VideoProcessor {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_size: (u32, u32),
    output_size: (u32, u32),
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl VideoProcessor {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        fps: u32,
        input_size: (u32, u32),
        output_size: (u32, u32),
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice =
            device.cast().context("Device has no ID3D11VideoDevice")?;
        let video_context: ID3D11VideoContext =
            context.cast().context("Context has no ID3D11VideoContext")?;

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: fps.max(1), Denominator: 1 },
            InputWidth: input_size.0,
            InputHeight: input_size.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: fps.max(1), Denominator: 1 },
            OutputWidth: output_size.0,
            OutputHeight: output_size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
            .context("CreateVideoProcessorEnumerator failed")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor failed")?;

        let src_rect = full_rect(input_size);
        let dst_rect = full_rect(output_size);
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_color_space());
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output_color_space());
            video_context.VideoProcessorSetStreamOutputRate(
                &processor,
                0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL,
                false,
                None,
            );
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src_rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst_rect));
        }

        let ring = (0..RING_SIZE)
            .map(|_| create_output_texture(device, &video_device, &enumerator, output_size))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            input_size,
            output_size,
            ring,
            next: 0,
        })
    }

    pub fn input_size(&self) -> (u32, u32) {
        self.input_size
    }

    pub fn output_size(&self) -> (u32, u32) {
        self.output_size
    }

    /// Scales and colour-converts `input` (BGRA) into the next ring texture
    /// and returns it. The reference is only valid until the next call to
    /// `convert` cycles the ring back onto the same slot.
    pub fn convert(&mut self, input: &ID3D11Texture2D) -> Result<&ID3D11Texture2D> {
        let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut input_view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                input,
                &self.enumerator,
                &view_desc,
                Some(&mut input_view),
            )
        }
        .context("CreateVideoProcessorInputView failed")?;
        let input_view = input_view.context("CreateVideoProcessorInputView produced no view")?;

        let idx = self.next;
        self.next = (self.next + 1) % self.ring.len();
        let (texture, output_view) = &self.ring[idx];

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: core::mem::ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        };
        let blt_result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // `pInputSurface` is a `ManuallyDrop` so the struct can be shared
        // with fields like `ppPastSurfaces` without a double free; reclaim
        // it ourselves so the input view's COM reference isn't leaked.
        let _ = unsafe { core::mem::ManuallyDrop::take(&mut stream.pInputSurface) };
        blt_result.context("VideoProcessorBlt failed")?;

        Ok(texture)
    }
}

fn full_rect(size: (u32, u32)) -> RECT {
    RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 }
}

/// `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` is a bitfield with no field accessors
/// in the `windows` crate's binding, so it's built by hand:
/// bit 0 Usage, bit 1 RGB_Range, bit 2 YCbCr_Matrix, bit 3 YCbCr_xvYCC,
/// bits 4-5 Nominal_Range (0 undefined, 1 = 16-235, 2 = 0-255).
fn color_space(rgb_studio_range: bool, ycbcr_bt709: bool, nominal_range: u32) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    let bits = ((rgb_studio_range as u32) << 1) | ((ycbcr_bt709 as u32) << 2) | ((nominal_range & 0x3) << 4);
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: bits }
}

/// Captured BGRA is full-range RGB (0-255), straight off the screen.
fn input_color_space() -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    color_space(false, false, 2)
}

/// BT.709, studio (16-235) range: the plan's target output space, so the
/// encoder's VUI (phase 3) must claim the same or colours will shift.
fn output_color_space() -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    color_space(false, true, 1)
}

fn create_output_texture(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    size: (u32, u32),
) -> Result<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D (NV12 output) failed")?;
    let texture = texture.context("CreateTexture2D produced no output texture")?;

    let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut output_view = None;
    unsafe {
        video_device.CreateVideoProcessorOutputView(&texture, enumerator, &view_desc, Some(&mut output_view))
    }
    .context("CreateVideoProcessorOutputView failed")?;
    let output_view = output_view.context("CreateVideoProcessorOutputView produced no view")?;

    Ok((texture, output_view))
}

/// Copies an NV12 texture back to the CPU as two tightly-packed planes: `Y`
/// (`width * height` bytes) and interleaved `UV` (`width * height / 2`
/// bytes). Named same as the BGRA readback it replaces, but ~6x smaller at
/// the same resolution since there's no colour data to duplicate 4x over.
pub fn read_back_nv12(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> windows::core::Result<(Vec<u8>, Vec<u8>)> {
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
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

    let w = width as usize;
    let h = height as usize;
    let mut y_plane = vec![0u8; w * h];
    let mut uv_plane = vec![0u8; w * (h / 2)];
    unsafe {
        let base = mapped.pData as *const u8;
        for row in 0..h {
            let src = base.add(row * mapped.RowPitch as usize);
            let dst = y_plane.as_mut_ptr().add(row * w);
            std::ptr::copy_nonoverlapping(src, dst, w);
        }
        // The chroma plane immediately follows the luma plane, using the
        // same row pitch — the layout every D3D11 NV12 implementation uses.
        let uv_base = base.add(h * mapped.RowPitch as usize);
        for row in 0..h / 2 {
            let src = uv_base.add(row * mapped.RowPitch as usize);
            let dst = uv_plane.as_mut_ptr().add(row * w);
            std::ptr::copy_nonoverlapping(src, dst, w);
        }
        context.Unmap(&staging, 0);
    }

    Ok((y_plane, uv_plane))
}
