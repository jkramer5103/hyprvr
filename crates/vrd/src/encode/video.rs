//! Low-latency H.264/HEVC encoding of a screen region with FFmpeg (VAAPI or libx264).
// Bindgen constant types differ between FFmpeg versions (7.x here, 9.x on the laptop);
// keep explicit casts.
#![allow(clippy::unnecessary_cast)]

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{Result, bail};
use ffmpeg_sys_next as ff;
use hyprvr_proto::msg::{Rect, VideoCodec};
use hyprvr_proto::tiles::{Frame, PixelFormat};

pub const AVERROR_EOF: i32 = -((b'E' as i32) | ((b'O' as i32) << 8) | ((b'F' as i32) << 16) | ((b' ' as i32) << 24));
const EAGAIN: i32 = -(libc::EAGAIN);

pub fn av_err(code: i32) -> String {
    let mut buf = [0 as libc::c_char; 256];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn check(code: i32, what: &str) -> Result<i32> {
    if code < 0 {
        bail!("{what}: {}", av_err(code));
    }
    Ok(code)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Vaapi(PathBuf),
    X264,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Vaapi(_) => "vaapi",
            Backend::X264 => "x264",
        }
    }
}

pub struct EncoderParams {
    pub backend: Backend,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub struct VideoEncoder {
    ctx: *mut ff::AVCodecContext,
    device: *mut ff::AVBufferRef,
    sws: *mut ff::SwsContext,
    sws_src: Option<PixelFormat>,
    sw_frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    pub width: u32,
    pub height: u32,
    pub backend: Backend,
    pts: i64,
    force_idr: bool,
}

unsafe impl Send for VideoEncoder {}

fn set_opt(obj: *mut libc::c_void, k: &str, v: &str) -> i32 {
    let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
    unsafe { ff::av_opt_set(obj, k.as_ptr(), v.as_ptr(), 0) }
}

impl VideoEncoder {
    /// Open an encoder. For VAAPI, tries low-power (VDEnc) first and falls back.
    pub fn new(p: &EncoderParams) -> Result<Self> {
        match &p.backend {
            Backend::Vaapi(_) => match Self::open(p, true) {
                Ok(e) => Ok(e),
                Err(e) => {
                    tracing::debug!("VAAPI low-power encode unavailable ({e:#}); using full-power");
                    Self::open(p, false)
                }
            },
            Backend::X264 => Self::open(p, false),
        }
    }

    fn open(p: &EncoderParams, low_power: bool) -> Result<Self> {
        if p.width < 16 || p.height < 16 || !p.width.is_multiple_of(2) || !p.height.is_multiple_of(2) {
            bail!("bad encoder size {}x{}", p.width, p.height);
        }
        let name = match (&p.backend, p.codec) {
            (Backend::Vaapi(_), VideoCodec::H264) => "h264_vaapi",
            (Backend::Vaapi(_), VideoCodec::Hevc) => "hevc_vaapi",
            (Backend::X264, VideoCodec::H264) => "libx264",
            (Backend::X264, VideoCodec::Hevc) => "libx265",
        };
        let cname = CString::new(name).unwrap();
        let mut me = VideoEncoder {
            ctx: ptr::null_mut(),
            device: ptr::null_mut(),
            sws: ptr::null_mut(),
            sws_src: None,
            sw_frame: ptr::null_mut(),
            pkt: ptr::null_mut(),
            width: p.width,
            height: p.height,
            backend: p.backend.clone(),
            pts: 0,
            force_idr: true,
        };
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                bail!("FFmpeg has no {name} encoder");
            }
            me.ctx = ff::avcodec_alloc_context3(codec);
            let c = &mut *me.ctx;
            c.width = p.width as i32;
            c.height = p.height as i32;
            c.time_base = ff::AVRational { num: 1, den: 1_000_000 };
            c.framerate = ff::AVRational { num: p.fps as i32, den: 1 };
            c.gop_size = 1_000_000; // IDR only on demand
            c.max_b_frames = 0;
            c.bit_rate = p.kbps as i64 * 1000;
            c.rc_max_rate = c.bit_rate;
            // ~2 frames of VBV keeps frame sizes (and so latency) bounded.
            c.rc_buffer_size = ((c.bit_rate / p.fps.max(1) as i64) * 2) as i32;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            let sw_fmt;
            match &p.backend {
                Backend::Vaapi(node) => {
                    me.device = create_vaapi_device(node)?;
                    let frames = ff::av_hwframe_ctx_alloc(me.device);
                    if frames.is_null() {
                        bail!("av_hwframe_ctx_alloc failed");
                    }
                    let fc = &mut *((*frames).data as *mut ff::AVHWFramesContext);
                    fc.format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                    fc.sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                    fc.width = p.width as i32;
                    fc.height = p.height as i32;
                    fc.initial_pool_size = 4;
                    let mut frames = frames;
                    if let Err(e) = check(ff::av_hwframe_ctx_init(frames), "av_hwframe_ctx_init") {
                        ff::av_buffer_unref(&mut frames);
                        return Err(e);
                    }
                    c.hw_frames_ctx = frames; // owned by the codec context now
                    c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                    sw_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                    let pd = c.priv_data;
                    set_opt(pd, "rc_mode", "VBR");
                    set_opt(pd, "async_depth", "1");
                    if low_power {
                        set_opt(pd, "low_power", "1");
                    }
                    if p.codec == VideoCodec::H264 {
                        set_opt(pd, "profile", "high");
                        set_opt(pd, "coder", "cabac");
                    }
                }
                Backend::X264 => {
                    c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_YUV420P;
                    sw_fmt = ff::AVPixelFormat::AV_PIX_FMT_YUV420P;
                    c.thread_count = 4;
                    let pd = c.priv_data;
                    set_opt(pd, "preset", "ultrafast");
                    set_opt(pd, "tune", "zerolatency");
                    set_opt(pd, "forced-idr", "1");
                    if p.codec == VideoCodec::Hevc {
                        set_opt(pd, "x265-params", "repeat-headers=1:bframes=0");
                    }
                }
            }
            check(ff::avcodec_open2(me.ctx, codec, ptr::null_mut()), &format!("open {name}"))?;
            me.sw_frame = ff::av_frame_alloc();
            let f = &mut *me.sw_frame;
            f.format = sw_fmt as i32;
            f.width = p.width as i32;
            f.height = p.height as i32;
            check(ff::av_frame_get_buffer(me.sw_frame, 0), "av_frame_get_buffer")?;
            me.pkt = ff::av_packet_alloc();
        }
        Ok(me)
    }

    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }

    fn ensure_sws(&mut self, src: PixelFormat) -> Result<()> {
        if self.sws_src == Some(src) {
            return Ok(());
        }
        unsafe {
            if !self.sws.is_null() {
                ff::sws_freeContext(self.sws);
            }
            let sf = match src {
                PixelFormat::Bgrx => ff::AVPixelFormat::AV_PIX_FMT_BGR0,
                PixelFormat::Rgbx => ff::AVPixelFormat::AV_PIX_FMT_RGB0,
            };
            let df: ff::AVPixelFormat = std::mem::transmute((*self.sw_frame).format);
            let (w, h) = (self.width as i32, self.height as i32);
            self.sws = ff::sws_getContext(w, h, sf, w, h, df, SWS_POINT_FLAG, ptr::null_mut(), ptr::null_mut(), ptr::null());
            if self.sws.is_null() {
                bail!("sws_getContext failed");
            }
            // RGB (full range) -> BT.709 limited range, matching the VUI we signal.
            let coeffs = ff::sws_getCoefficients(ff::SWS_CS_ITU709 as i32);
            ff::sws_setColorspaceDetails(self.sws, coeffs, 1, coeffs, 0, 0, 1 << 16, 1 << 16);
        }
        self.sws_src = Some(src);
        Ok(())
    }

    /// Encode `rect` of `frame` (rect size must equal the encoder size).
    pub fn encode(&mut self, frame: &Frame, rect: Rect, pts_us: i64) -> Result<Vec<Packet>> {
        if rect.w != self.width || rect.h != self.height || rect.x + rect.w > frame.width || rect.y + rect.h > frame.height {
            bail!("encode rect {rect:?} does not match encoder {}x{} / frame", self.width, self.height);
        }
        self.ensure_sws(frame.format)?;
        unsafe {
            check(ff::av_frame_make_writable(self.sw_frame), "av_frame_make_writable")?;
            let off = rect.y as usize * frame.stride + rect.x as usize * 4;
            let src = [frame.data[off..].as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride = [frame.stride as i32, 0, 0, 0];
            let sf = &*self.sw_frame;
            ff::sws_scale(self.sws, src.as_ptr(), src_stride.as_ptr(), 0, self.height as i32, sf.data.as_ptr(), sf.linesize.as_ptr());

            let input = match self.backend {
                Backend::Vaapi(_) => {
                    let hw = ff::av_frame_alloc();
                    let r = ff::av_hwframe_get_buffer((*self.ctx).hw_frames_ctx, hw, 0);
                    if r < 0 {
                        let mut hw = hw;
                        ff::av_frame_free(&mut hw);
                        check(r, "av_hwframe_get_buffer")?;
                    }
                    let r = ff::av_hwframe_transfer_data(hw, self.sw_frame, 0);
                    if r < 0 {
                        let mut hw = hw;
                        ff::av_frame_free(&mut hw);
                        check(r, "av_hwframe_transfer_data")?;
                    }
                    hw
                }
                Backend::X264 => self.sw_frame,
            };
            // Monotonic pts even if the caller's clock repeats.
            self.pts = pts_us.max(self.pts + 1);
            (*input).pts = self.pts;
            (*input).pict_type = if self.force_idr {
                ff::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            if self.force_idr {
                (*input).flags |= ff::AV_FRAME_FLAG_KEY as i32;
            } else {
                (*input).flags &= !(ff::AV_FRAME_FLAG_KEY as i32);
            }
            let r = ff::avcodec_send_frame(self.ctx, input);
            if !matches!(self.backend, Backend::X264) {
                let mut hw = input;
                ff::av_frame_free(&mut hw);
            }
            check(r, "avcodec_send_frame")?;
            self.force_idr = false;
            let mut out = Vec::new();
            loop {
                let r = ff::avcodec_receive_packet(self.ctx, self.pkt);
                if r == EAGAIN || r == AVERROR_EOF {
                    break;
                }
                check(r, "avcodec_receive_packet")?;
                let p = &*self.pkt;
                out.push(Packet {
                    data: std::slice::from_raw_parts(p.data, p.size as usize).to_vec(),
                    keyframe: p.flags & ff::AV_PKT_FLAG_KEY as i32 != 0,
                });
                ff::av_packet_unref(self.pkt);
            }
            Ok(out)
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.sws.is_null() {
                ff::sws_freeContext(self.sws);
            }
            if !self.sw_frame.is_null() {
                ff::av_frame_free(&mut self.sw_frame);
            }
            if !self.pkt.is_null() {
                ff::av_packet_free(&mut self.pkt);
            }
            if !self.ctx.is_null() {
                ff::avcodec_free_context(&mut self.ctx);
            }
            if !self.device.is_null() {
                ff::av_buffer_unref(&mut self.device);
            }
        }
    }
}

fn create_vaapi_device(node: &Path) -> Result<*mut ff::AVBufferRef> {
    let path = CString::new(node.to_string_lossy().as_bytes())?;
    let mut dev: *mut ff::AVBufferRef = ptr::null_mut();
    unsafe {
        check(
            ff::av_hwdevice_ctx_create(&mut dev, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, path.as_ptr(), ptr::null_mut(), 0),
            &format!("open VAAPI device {}", node.display()),
        )?;
    }
    Ok(dev)
}

/// Pick a backend: VAAPI if it can open a tiny encoder, otherwise x264.
pub fn probe_backend(choice: crate::config::EncoderChoice, node: &Path, codec: VideoCodec) -> Result<Backend> {
    use crate::config::EncoderChoice;
    let try_open = |b: Backend| {
        VideoEncoder::new(&EncoderParams { backend: b.clone(), codec, width: 256, height: 128, fps: 60, kbps: 2000 }).map(|_| b)
    };
    match choice {
        EncoderChoice::Vaapi => try_open(Backend::Vaapi(node.to_path_buf())),
        EncoderChoice::X264 => try_open(Backend::X264),
        EncoderChoice::Auto => match try_open(Backend::Vaapi(node.to_path_buf())) {
            Ok(b) => Ok(b),
            Err(e) => {
                tracing::warn!("VAAPI unavailable ({e:#}); falling back to software x264");
                try_open(Backend::X264)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x264_idr_has_parameter_sets() {
        let (w, h) = (320u32, 240u32);
        let mut e = VideoEncoder::new(&EncoderParams { backend: Backend::X264, codec: VideoCodec::H264, width: w, height: h, fps: 60, kbps: 4000 })
            .unwrap();
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut first_key = None;
        let mut count = 0;
        for i in 0..10u32 {
            for (j, b) in buf.iter_mut().enumerate() {
                *b = ((j as u32 + i * 7) % 251) as u8;
            }
            if i == 6 {
                e.request_idr();
            }
            let f = Frame { data: &buf, stride: (w * 4) as usize, width: w, height: h, format: PixelFormat::Bgrx };
            for p in e.encode(&f, Rect { x: 0, y: 0, w, h }, i as i64 * 16_666).unwrap() {
                // zerolatency: one packet per frame, immediately.
                count += 1;
                if p.keyframe {
                    // Annex B with SPS (nal type 7) in-band.
                    assert!(p.data.windows(5).any(|s| s[..4] == [0, 0, 0, 1] && s[4] & 0x1f == 7) || p.data.windows(4).any(|s| s[..3] == [0, 0, 1] && s[3] & 0x1f == 7));
                    if first_key.is_none() {
                        first_key = Some(i);
                    }
                }
            }
        }
        assert_eq!(count, 10);
        assert_eq!(first_key, Some(0));
    }
}

/// `SWS_POINT` (1 << 4). Literal because ffmpeg 8+ moved it into the `SwsFlags` enum.
const SWS_POINT_FLAG: std::ffi::c_int = 0x10;
