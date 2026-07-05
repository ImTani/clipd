//! `capture::convert` — BGRA→NV12 colour conversion on the GPU video processor.
//!
//! Net-new for Milestone 1 (no spike covered it). Converts each captured BGRA8
//! frame to NV12 using `ID3D11VideoProcessor` — the **dedicated video-processor
//! engine**, not a 3D-queue compute shader (`01-PROJECT-PLAN.md` data-flow rule
//! 1 / pitfall 16a), so the conversion does not queue behind a game's 3D work.
//! Pixels stay on the GPU end to end.
//!
//! ## Colour (the guaranteed first-week bug — pitfall, plan §4)
//! Input is tagged **RGB full-range BT.709** (`DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709`)
//! and output **YCbCr studio/limited-range BT.709**
//! (`DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709`). This is half of "correct
//! colours"; the other half is tagging the encoder's H.264 VUI to match
//! (Task E), so a player reconstructs the same primaries/matrix/range.
//!
//! ## Output textures
//! Conversion targets a small round-robin pool of NV12 textures so the async
//! encoder can still hold the previous frame's texture while the next is being
//! produced. See `DECISIONS.md` for the pool-vs-fence tradeoff.

use std::mem::ManuallyDrop;

use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::capture::canvas::letterbox_rect;
use crate::gpu::GpuContext;

/// Output NV12 textures cycled so the encoder can hold frame N while later frames
/// are produced. Sized above the engine's input-channel depth (4) plus the
/// in-encoder and in-conversion frames, so a queued frame's texture is never
/// recycled under it. Not a hard fence — the depth gives the slack; a
/// fence-based recycle is the proper fix (deferred). See `DECISIONS.md`.
const NV12_POOL_LEN: usize = 8;

/// Errors from setting up or running the video processor.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// A Direct3D video call failed.
    #[error("Direct3D video-processor call failed: {0}")]
    Windows(#[from] windows::core::Error),
    /// A resource creation call returned success but no object.
    #[error("video-processor resource creation returned no object")]
    NoResource,
}

/// A GPU BGRA→NV12 converter bound to the shared device. Lives on the capture
/// thread; `convert` is `&mut self` (single-threaded use).
pub struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    pool: Vec<ID3D11Texture2D>,
    next: usize,
    width: u32,
    height: u32,
}

impl Converter {
    /// Build a converter that scales an `input` (w, h) capture source into a fixed
    /// `canvas` (w, h) NV12 output at `fps` (the rate only describes the processor;
    /// the grid enforces true CFR upstream).
    ///
    /// **Fixed canvas + letterbox (M4-2, DECISIONS 2026-07-05 / pitfall 11):** the
    /// output is a fixed `canvas` (evened by [`canvas_size`](crate::capture::canvas::
    /// canvas_size)); the video processor scales the (possibly odd, any-aspect,
    /// changing-on-resize) `input` **to fit, centered** within it ([`letterbox_rect`])
    /// and fills the rest with black. Because the canvas never changes, a window
    /// resize rebuilds only the input side (pool + this converter) — the encoder and
    /// epoch are untouched, so a clip spans resizes at one resolution.
    /// [`Self::dimensions`] returns the canvas, which the caller feeds to the encoder.
    pub fn new(
        gpu: &GpuContext,
        input: (u32, u32),
        canvas: (u32, u32),
        fps: u32,
    ) -> Result<Self, ConvertError> {
        let (in_w, in_h) = (input.0.max(1), input.1.max(1));
        let (out_w, out_h) = (canvas.0.max(2) & !1, canvas.1.max(2) & !1);
        let (lb_l, lb_t, lb_r, lb_b) = letterbox_rect((in_w, in_h), (out_w, out_h));
        // SAFETY: standard video-device/context casts + enumerator/processor
        // creation. The content desc and rects are caller-owned, passed by pointer.
        unsafe {
            let video_device: ID3D11VideoDevice = gpu.device().cast()?;
            let video_context: ID3D11VideoContext1 = gpu.context().cast()?;

            let rate = DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            };
            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content_desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;

            // Colour spaces: full-range BT.709 RGB in, studio/limited BT.709 YCbCr
            // out. These are void COM methods (no HRESULT).
            video_context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            video_context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );

            // Fill the canvas outside the content with opaque black (the letterbox
            // bars); place the full input into the centered fit-rect. RGBA(0,0,0,1)
            // with ycbcr=FALSE → the VP converts to studio-range video black.
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            let src = RECT {
                left: 0,
                top: 0,
                right: in_w as i32,
                bottom: in_h as i32,
            };
            let dst = RECT {
                left: lb_l,
                top: lb_t,
                right: lb_r,
                bottom: lb_b,
            };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));

            let mut pool = Vec::with_capacity(NV12_POOL_LEN);
            for _ in 0..NV12_POOL_LEN {
                pool.push(create_nv12_texture(gpu, out_w, out_h)?);
            }

            Ok(Self {
                video_device,
                video_context,
                enumerator,
                processor,
                pool,
                next: 0,
                width: out_w,
                height: out_h,
            })
        }
    }

    /// Convert one BGRA input texture to NV12. Returns a handle to the pool
    /// texture the result was written into (the caller wraps it as an MF sample).
    pub fn convert(&mut self, input: &ID3D11Texture2D) -> Result<ID3D11Texture2D, ConvertError> {
        let output = self.pool[self.next].clone();
        self.next = (self.next + 1) % self.pool.len();

        // SAFETY: create the input/output views over caller/pool textures, submit
        // one Blt, then release the input view. All resources belong to the shared
        // (multithread-protected) device.
        unsafe {
            let input_view = self.create_input_view(input)?;
            let output_view = self.create_output_view(&output)?;

            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };

            let blt = self.video_context.VideoProcessorBlt(
                &self.processor,
                &output_view,
                0,
                std::slice::from_ref(&stream),
            );

            // Release the manually-managed input-view refs regardless of Blt result.
            ManuallyDrop::drop(&mut stream.pInputSurface);
            ManuallyDrop::drop(&mut stream.pInputSurfaceRight);
            blt?;
        }

        Ok(output)
    }

    /// Output frame dimensions.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Create a video-processor input view over a BGRA texture.
    ///
    /// # Safety
    /// `input` must be a 2D texture on the shared device.
    unsafe fn create_input_view(
        &self,
        input: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView, ConvertError> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view = None;
        self.video_device.CreateVideoProcessorInputView(
            input,
            &self.enumerator,
            &desc,
            Some(&mut view),
        )?;
        view.ok_or(ConvertError::NoResource)
    }

    /// Create a video-processor output view over an NV12 pool texture.
    ///
    /// # Safety
    /// `output` must be an NV12 2D texture on the shared device.
    unsafe fn create_output_view(
        &self,
        output: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorOutputView, ConvertError> {
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        self.video_device.CreateVideoProcessorOutputView(
            output,
            &self.enumerator,
            &desc,
            Some(&mut view),
        )?;
        view.ok_or(ConvertError::NoResource)
    }
}

/// Create one NV12 render-target texture on the shared device.
fn create_nv12_texture(
    gpu: &GpuContext,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, ConvertError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: standard texture creation; no initial data. The out-param is
    // written on S_OK.
    unsafe {
        gpu.device()
            .CreateTexture2D(&desc, None, Some(&mut texture))?;
    }
    texture.ok_or(ConvertError::NoResource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{AdapterSelection, GpuContext};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    /// A blank BGRA render-target/shader-resource texture usable as a
    /// video-processor input (contents irrelevant — the test only exercises setup).
    fn make_bgra(gpu: &GpuContext, width: u32, height: u32) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: standard texture creation; no initial data.
        unsafe {
            gpu.device()
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .expect("BGRA texture");
        }
        texture.expect("BGRA texture out-param")
    }

    /// The M4 even-dimension rule: a window can have ODD dimensions, but NV12/H.264
    /// need even. `Converter::new` must round the output down to even and the video
    /// processor must Blt an odd BGRA input into that even NV12 output without error.
    /// Needs a real GPU (video processor) → `#[ignore]`d; run on the test machine
    /// with `cargo test --ignored` (04-TEST-MACHINE.md).
    #[test]
    #[ignore = "needs a GPU video processor; run with --ignored on the test machine"]
    fn odd_input_scales_into_fixed_canvas() {
        let gpu = GpuContext::new(AdapterSelection::Auto).expect("shared D3D11 device");
        // Odd, differently-aspect window input into a fixed 1920x1080 canvas — the VP
        // must letterbox-scale it without error (M4-2). Also covers the odd-dim case.
        let mut converter =
            Converter::new(&gpu, (1115, 627), (1920, 1080), 60).expect("converter (odd → canvas)");
        assert_eq!(converter.dimensions(), (1920, 1080), "output is the canvas");
        let input = make_bgra(&gpu, 1115, 627);
        converter
            .convert(&input)
            .expect("odd BGRA -> letterboxed NV12 canvas Blt");
    }
}
