use std::{mem::MaybeUninit, slice};

use anyhow::{Result, anyhow};
use aotuv_lancer_vorbis_sys::*;
use ogg_next_sys::*;
use rand::{Rng, thread_rng};

pub struct VorbisEncoder {
    os: Box<MaybeUninit<ogg_stream_state>>,
    og: Box<MaybeUninit<ogg_page>>,
    op: Box<MaybeUninit<ogg_packet>>,

    vi: Box<MaybeUninit<vorbis_info>>,
    vc: Box<MaybeUninit<vorbis_comment>>,
    vd: Box<MaybeUninit<vorbis_dsp_state>>,
    vb: Box<MaybeUninit<vorbis_block>>,

    header_data: Vec<u8>,
    serial_num: i32,
}

/// VorbisEncoder owns a set of raw C structures wrapped in
/// `Box<MaybeUninit<_>>`.  All of those structures are plain C structs
/// containing only primitives and raw pointers, which are `Send`.  The
/// `MaybeUninit<T>` wrapper is `Send` as long as `T` is `Send`.  Because
/// every field in `VorbisEncoder` satisfies this invariant, the type can
/// safely be sent across thread boundaries.
///
/// Without this marker the compiler would infer that the type is not
/// `Send` because it contains `unsafe` code that touches raw pointers.
/// By providing the manual implementation we signal that the author has
/// verified the safety.
unsafe impl Send for VorbisEncoder {}

impl VorbisEncoder {
    pub fn new(channels: i32, sample_rate: i32, quality: f32) -> Self {
        let mut os: Box<MaybeUninit<ogg_stream_state>> = Box::new(MaybeUninit::uninit());
        let mut og: Box<MaybeUninit<ogg_page>> = Box::new(MaybeUninit::uninit());
        let op: Box<MaybeUninit<ogg_packet>> = Box::new(MaybeUninit::uninit());

        let mut vi: Box<MaybeUninit<vorbis_info>> = Box::new(MaybeUninit::uninit());
        let mut vc: Box<MaybeUninit<vorbis_comment>> = Box::new(MaybeUninit::uninit());
        let mut vd: Box<MaybeUninit<vorbis_dsp_state>> = Box::new(MaybeUninit::uninit());
        let mut vb: Box<MaybeUninit<vorbis_block>> = Box::new(MaybeUninit::uninit());

        let mut header_data: Vec<u8> = Vec::new();
        let serial_num = thread_rng().r#gen();

        unsafe {
            vorbis_info_init(vi.as_mut_ptr());

            let ret = vorbis_encode_init_vbr(
                vi.as_mut_ptr(),
                channels.into(),
                sample_rate.into(),
                quality.into(),
            );
            if ret != 0 {
                panic!("vorbis_encode_init_vbr returned {ret}");
            }

            vorbis_comment_init(vc.as_mut_ptr());
            // vorbis_comment_add_tag(vc.as_mut_ptr(), "ENCODER","encoder_example.c");

            vorbis_analysis_init(vd.as_mut_ptr(), vi.as_mut_ptr());
            vorbis_block_init(vd.as_mut_ptr(), vb.as_mut_ptr());

            ogg_stream_init(os.as_mut_ptr(), serial_num);

            {
                let mut header = Box::new(MaybeUninit::uninit());
                let mut header_comm = Box::new(MaybeUninit::uninit());
                let mut header_code = Box::new(MaybeUninit::uninit());

                vorbis_analysis_headerout(
                    vd.as_mut_ptr(),
                    vc.as_mut_ptr(),
                    header.as_mut_ptr(),
                    header_comm.as_mut_ptr(),
                    header_code.as_mut_ptr(),
                );
                ogg_stream_packetin(os.as_mut_ptr(), header.as_mut_ptr());
                ogg_stream_packetin(os.as_mut_ptr(), header_comm.as_mut_ptr());
                ogg_stream_packetin(os.as_mut_ptr(), header_code.as_mut_ptr());

                /* This ensures the actual
                 * audio data will start on a new page, as per spec
                 */
                while ogg_stream_flush(os.as_mut_ptr(), og.as_mut_ptr()) != 0 {
                    let ogg_page = og.assume_init_ref();
                    header_data.extend_from_slice(slice::from_raw_parts(
                        ogg_page.header,
                        ogg_page.header_len as usize,
                    ));
                    header_data.extend_from_slice(slice::from_raw_parts(
                        ogg_page.body,
                        ogg_page.body_len as usize,
                    ));
                }
            }
        }

        Self {
            os,
            og,
            op,
            vi,
            vc,
            vd,
            vb,
            header_data,
            serial_num,
        }
    }

    pub fn get_serial_number(&self) -> i32 {
        self.serial_num
    }

    pub fn get_header_data(&self) -> Vec<u8> {
        self.header_data.clone()
    }

    pub fn encode(&mut self, raw_audio_samples: Vec<Vec<f32>>) -> Result<Vec<u8>> {
        let audio_channels = unsafe { self.vi.assume_init_ref().channels as usize };

        let sample_count = raw_audio_samples.get(0).map(|v| v.len()).unwrap_or(0);
        if sample_count == 0 {
            return Err(anyhow!("Invalid channel sample count"));
        }

        for channel_samples in raw_audio_samples.iter() {
            if channel_samples.len() != sample_count {
                return Err(anyhow!("Invalid channel sample count"));
            }
        }

        let mut encoded_data = Vec::new();

        unsafe {
            let encoder_buffer = slice::from_raw_parts_mut(
                vorbis_analysis_buffer(self.vd.as_mut_ptr(), sample_count.try_into()?),
                audio_channels,
            );

            for (channel_samples, channel_encode_buffer) in
                raw_audio_samples.iter().zip(encoder_buffer.iter_mut())
            {
                channel_samples
                    .as_ptr()
                    .copy_to_nonoverlapping(*channel_encode_buffer, sample_count);
            }

            let res = vorbis_analysis_wrote(self.vd.as_mut_ptr(), sample_count.try_into()?);
            if res != 0 {
                panic!("vorbis_analysis_wrote returned {res}");
            }

            // SAFETY: we assume the functions inside this unsafe block follow their
            // documented contract
            while vorbis_analysis_blockout(self.vd.as_mut_ptr(), self.vb.as_mut_ptr()) == 1 {
                let res = vorbis_analysis(self.vb.as_mut_ptr(), std::ptr::null_mut());
                if res != 0 {
                    panic!("vorbis_analysis returned {res}");
                }
                let res = vorbis_bitrate_addblock(self.vb.as_mut_ptr());
                if res != 0 {
                    panic!("vorbis_bitrate_addblock returned {res}");
                }

                while vorbis_bitrate_flushpacket(self.vd.as_mut_ptr(), self.op.as_mut_ptr()) == 1 {
                    let res = ogg_stream_packetin(self.os.as_mut_ptr(), self.op.as_mut_ptr());
                    if res != 0 {
                        panic!("ogg_stream_packetin returned {res}");
                    }

                    while ogg_stream_flush(self.os.as_mut_ptr(), self.og.as_mut_ptr()) != 0 {
                        let ogg_page = self.og.assume_init_ref();
                        encoded_data.extend_from_slice(slice::from_raw_parts(
                            ogg_page.header,
                            ogg_page.header_len as usize,
                        ));
                        encoded_data.extend_from_slice(slice::from_raw_parts(
                            ogg_page.body,
                            ogg_page.body_len as usize,
                        ));
                    }
                }
            }
        }

        Ok(encoded_data)
    }
}

impl Drop for VorbisEncoder {
    fn drop(&mut self) {
        unsafe {
            ogg_stream_clear(self.os.as_mut_ptr());
            vorbis_block_clear(self.vb.as_mut_ptr());
            vorbis_dsp_clear(self.vd.as_mut_ptr());
            vorbis_comment_clear(self.vc.as_mut_ptr());
            vorbis_info_clear(self.vi.as_mut_ptr());
        }
    }
}
