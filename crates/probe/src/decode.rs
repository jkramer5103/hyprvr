//! Software H.264/HEVC decoding to RGBA with FFmpeg (BT.709 limited range, like the encoder).
// Bindgen constant types differ between FFmpeg versions (7.x here, 9.x on the laptop);
// keep explicit casts.
#![allow(clippy::unnecessary_cast)]

use std::ptr;

use anyhow::{Result, bail};
use ffmpeg_sys_next as ff;

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    sws: *mut ff::SwsContext,
    sws_key: (i32, i32, i32),
}

// The FFmpeg objects are only used from one task at a time.
unsafe impl Send for Decoder {}

fn check(code: i32, what: &str) -> Result<i32> {
    if code < 0 {
        let mut buf = [0 as libc::c_char; 256];
        let msg = unsafe {
            ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        };
        bail!("{what}: {msg}");
    }
    Ok(code)
}

impl Decoder {
    pub fn new(hevc: bool) -> Result<Self> {
        unsafe {
            let id = if hevc { ff::AVCodecID::AV_CODEC_ID_HEVC } else { ff::AVCodecID::AV_CODEC_ID_H264 };
            let codec = ff::avcodec_find_decoder(id);
            if codec.is_null() {
                bail!("no decoder for {id:?}");
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            (*ctx).thread_count = 1;
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            check(ff::avcodec_open2(ctx, codec, ptr::null_mut()), "avcodec_open2")?;
            Ok(Decoder { ctx, pkt: ff::av_packet_alloc(), frame: ff::av_frame_alloc(), sws: ptr::null_mut(), sws_key: (0, 0, -1) })
        }
    }

    /// Decode one access unit; returns (width, height, rgba) for each output picture.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<(u32, u32, Vec<u8>)>> {
        let mut out = vec![];
        unsafe {
            check(ff::av_new_packet(self.pkt, data.len() as i32), "av_new_packet")?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let r = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            check(r, "avcodec_send_packet")?;
            loop {
                let r = ff::avcodec_receive_frame(self.ctx, self.frame);
                if r == -(libc::EAGAIN) || r == ff::AVERROR_EOF {
                    break;
                }
                check(r, "avcodec_receive_frame")?;
                let f = &*self.frame;
                let (w, h, fmt) = (f.width, f.height, f.format);
                if self.sws_key != (w, h, fmt) {
                    if !self.sws.is_null() {
                        ff::sws_freeContext(self.sws);
                    }
                    self.sws = ff::sws_getContext(
                        w,
                        h,
                        std::mem::transmute::<i32, ff::AVPixelFormat>(fmt),
                        w,
                        h,
                        ff::AVPixelFormat::AV_PIX_FMT_RGBA,
                        SWS_POINT_FLAG,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null(),
                    );
                    if self.sws.is_null() {
                        bail!("sws_getContext");
                    }
                    let c = ff::sws_getCoefficients(ff::SWS_CS_ITU709 as i32);
                    ff::sws_setColorspaceDetails(self.sws, c, 0, c, 1, 0, 1 << 16, 1 << 16);
                    self.sws_key = (w, h, fmt);
                }
                let mut rgba = vec![0u8; (w * h * 4) as usize];
                let dst = [rgba.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
                let dst_stride = [w * 4, 0, 0, 0];
                ff::sws_scale(self.sws, f.data.as_ptr() as *const *const u8, f.linesize.as_ptr(), 0, h, dst.as_ptr(), dst_stride.as_ptr());
                ff::av_frame_unref(self.frame);
                out.push((w as u32, h as u32, rgba));
            }
        }
        Ok(out)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.frame);
            if !self.sws.is_null() {
                ff::sws_freeContext(self.sws);
            }
        }
    }
}

/// `SWS_POINT` (1 << 4). Literal because ffmpeg 8+ moved it into the `SwsFlags` enum.
const SWS_POINT_FLAG: std::ffi::c_int = 0x10;
