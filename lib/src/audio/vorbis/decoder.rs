use std::mem::MaybeUninit;

use anyhow::{Result, anyhow};
use aotuv_lancer_vorbis_sys::*;
use log::{debug, error};
use ogg_next_sys::*;

pub struct VorbisDecoder {
    oy: Box<MaybeUninit<ogg_sync_state>>,
    os: Box<MaybeUninit<ogg_stream_state>>,

    og: Box<MaybeUninit<ogg_page>>,
    op: Box<MaybeUninit<ogg_packet>>,

    vi: Box<MaybeUninit<vorbis_info>>,

    vc: Box<MaybeUninit<vorbis_comment>>,
    vd: Box<MaybeUninit<vorbis_dsp_state>>,
    vb: Box<MaybeUninit<vorbis_block>>,
}

/// VorbisDecoder owns a set of raw C structures wrapped in
/// `Box<MaybeUninit<_>>`.  All of those structures are plain C structs
/// containing only primitives and raw pointers, which are `Send`.  The
/// `MaybeUninit<T>` wrapper is `Send` as long as `T` is `Send`.  Because
/// every field in `VorbisDecoder` satisfies this invariant, the type can
/// safely be sent across thread boundaries.
///
/// Without this marker the compiler would infer that the type is not
/// `Send` because it contains `unsafe` code that touches raw pointers.
/// By providing the manual implementation we signal that the author has
/// verified the safety.
unsafe impl Send for VorbisDecoder {}

impl VorbisDecoder {
    pub fn new() -> Self {
        let mut oy = Box::new(MaybeUninit::uninit());
        let os = Box::new(MaybeUninit::uninit());

        let og = Box::new(MaybeUninit::uninit());
        let op = Box::new(MaybeUninit::uninit());

        let vi = Box::new(MaybeUninit::uninit());

        let vc = Box::new(MaybeUninit::uninit());
        let vd = Box::new(MaybeUninit::uninit());
        let vb = Box::new(MaybeUninit::uninit());

        unsafe {
            ogg_sync_init(oy.as_mut_ptr());
        }

        Self {
            oy,
            os,
            og,
            op,
            vi,
            vc,
            vd,
            vb,
        }
    }

    pub fn decode_first_package(&mut self, packet: &[u8]) -> Result<()> {
        unsafe {
            debug!("Header size: {}", packet.len());

            let buffer_ptr =
                ogg_sync_buffer(self.oy.as_mut_ptr(), packet.len().try_into()?).cast::<u8>();
            let decode_buffer = std::slice::from_raw_parts_mut(buffer_ptr, packet.len());
            decode_buffer[..packet.len()].copy_from_slice(packet);
            ogg_sync_wrote(self.oy.as_mut_ptr(), packet.len().try_into()?);

            /* Get the first page. */
            if ogg_sync_pageout(self.oy.as_mut_ptr(), self.og.as_mut_ptr()) != 1 {
                return Err(anyhow!("Input does not appear to be an Ogg bitstream."));
            }

            let serial_no = ogg_page_serialno(self.og.as_mut_ptr());
            debug!("Serial number found: {serial_no}");
            ogg_stream_init(self.os.as_mut_ptr(), serial_no);

            if ogg_stream_pagein(self.os.as_mut_ptr(), self.og.as_mut_ptr()) != 0 {
                return Err(anyhow!("Error reading first page of Ogg bitstream data."));
            }

            if ogg_stream_packetout(self.os.as_mut_ptr(), self.op.as_mut_ptr()) != 1 {
                return Err(anyhow!("Error reading initial header packet."));
            }

            if vorbis_synthesis_idheader(self.op.as_mut_ptr()) != 1 {
                return Err(anyhow!("This is not a valid Vorbis first packet."));
            }

            vorbis_info_init(self.vi.as_mut_ptr());
            vorbis_comment_init(self.vc.as_mut_ptr());

            if vorbis_synthesis_headerin(
                self.vi.as_mut_ptr(),
                self.vc.as_mut_ptr(),
                self.op.as_mut_ptr(),
            ) < 0
            {
                return Err(anyhow!(
                    "This Ogg bitstream does not contain Vorbis audio data."
                ));
            }

            let mut i = 0;

            while i < 2 {
                while i < 2 {
                    let mut result = ogg_sync_pageout(self.oy.as_mut_ptr(), self.og.as_mut_ptr());
                    if result == 0 {
                        break;
                    }

                    // Don't complain about missing or corrupt data yet. We'll
                    // catch it at the packet output phase
                    if result == 1 {
                        ogg_stream_pagein(self.os.as_mut_ptr(), self.og.as_mut_ptr());
                        // We can ignore any errors here as they'll also become apparent at packetout
                        while i < 2 {
                            result =
                                ogg_stream_packetout(self.os.as_mut_ptr(), self.op.as_mut_ptr());
                            if result == 0 {
                                break;
                            }
                            if result < 0 {
                                // Uh oh; data at some point was corrupted or missing!
                                // We can't tolerate that in a header. Die.
                                return Err(anyhow!("Corrupt secondary header. Exiting."));
                            }
                            result = vorbis_synthesis_headerin(
                                self.vi.as_mut_ptr(),
                                self.vc.as_mut_ptr(),
                                self.op.as_mut_ptr(),
                            );
                            if result < 0 {
                                return Err(anyhow!("Corrupt secondary header. Exiting."));
                            }
                            i += 1;
                        }
                    }
                }
            }

            {
                // let ptr = vc.assume_init_ref().user_comments;
                // ptr.user_comments
                // while(*ptr){
                //   error!("%s\n",*ptr);
                //   ++ptr;
                // }
                let vi_ref = self.vi.assume_init_ref();
                let vc_ref = self.vc.assume_init_ref();
                debug!(
                    "Bitstream is {} channel, {}Hz",
                    vi_ref.channels, vi_ref.rate
                );
                let vendor = std::ffi::CStr::from_ptr(vc_ref.vendor);
                debug!("Encoded by: {}", vendor.to_str()?);
            }

            vorbis_synthesis_init(self.vd.as_mut_ptr(), self.vi.as_mut_ptr());
            vorbis_block_init(self.vd.as_mut_ptr(), self.vb.as_mut_ptr());

            Ok(())
        }
    }

    pub fn decode_next_package<F: FnMut(&Vec<f32>)>(
        &mut self,
        packet: &[u8],
        mut producer: F,
    ) -> Result<()> {
        unsafe {
            let num_channels = self.vi.assume_init_ref().channels as usize;
            let convsize = 4096 / num_channels;

            let buffer_ptr =
                ogg_sync_buffer(self.oy.as_mut_ptr(), packet.len().try_into()?).cast::<u8>();
            let decode_buffer = std::slice::from_raw_parts_mut(buffer_ptr, packet.len());
            decode_buffer[..packet.len()].copy_from_slice(packet);
            ogg_sync_wrote(self.oy.as_mut_ptr(), packet.len().try_into()?);

            loop {
                let mut result = ogg_sync_pageout(self.oy.as_mut_ptr(), self.og.as_mut_ptr());
                if result == 0 {
                    break;
                }

                if result < 0 {
                    // Missing or corrupt data at this page position
                    error!("Corrupt or missing data in bitstream; continuing...");
                    break;
                } else {
                    // Can safely ignore errors at this point
                    ogg_stream_pagein(self.os.as_mut_ptr(), self.og.as_mut_ptr());

                    loop {
                        result = ogg_stream_packetout(self.os.as_mut_ptr(), self.op.as_mut_ptr());

                        if result == 0 {
                            break;
                        }

                        if result < 0 {
                            // Missing or corrupt data at this page position
                            // no reason to complain; already complained above
                        } else {
                            // We have a packet. Decode it
                            if vorbis_synthesis(self.vb.as_mut_ptr(), self.op.as_mut_ptr()) == 0 {
                                vorbis_synthesis_blockin(
                                    self.vd.as_mut_ptr(),
                                    self.vb.as_mut_ptr(),
                                );
                            }

                            // pcm is a multichannel float vector. In stereo, for example, pcm[0] is left,
                            // and pcm[1] is right. samples is the size of each channel. Convert the float
                            // values (-1.0 <= range <= 1.0) to whatever PCM format and write it out
                            let mut pcm: *mut *mut f32 = std::ptr::null_mut();
                            loop {
                                let samples =
                                    vorbis_synthesis_pcmout(self.vd.as_mut_ptr(), &mut pcm)
                                        as usize;
                                if samples == 0 {
                                    break;
                                }

                                let bout = std::cmp::min(samples, convsize);

                                // Gether the channel data
                                let data: Vec<&[f32]> = (0..num_channels)
                                    .map(|channel_num| {
                                        let channel_data = *pcm.offset(channel_num as isize);
                                        std::slice::from_raw_parts(channel_data, bout)
                                    })
                                    .collect();

                                // Interlace the audio so cpal can play it
                                // E.g for 2 channels:  [[L1, L2, L3], [R1, R2, R3]] -> [L0, R0, L1, R1, L2, R2]
                                let mut interlaced_data = Vec::with_capacity(samples * bout);
                                for sample_num in 0..bout {
                                    for channel_num in 0..num_channels {
                                        interlaced_data.push(data[channel_num][sample_num]);
                                    }
                                }

                                producer(&interlaced_data);

                                // Tell libvorbis how many samples we actually consumed
                                vorbis_synthesis_read(self.vd.as_mut_ptr(), bout as i32);
                            }
                        }
                    }
                }

                if ogg_page_eos(self.og.as_mut_ptr()) > 0 {
                    break;
                };
            }

            Ok(())
        }
    }
}

impl Drop for VorbisDecoder {
    fn drop(&mut self) {
        unsafe {
            vorbis_block_clear(self.vb.as_mut_ptr());
            vorbis_dsp_clear(self.vd.as_mut_ptr());

            ogg_stream_clear(self.os.as_mut_ptr());
            vorbis_comment_clear(self.vc.as_mut_ptr());
            vorbis_info_clear(self.vi.as_mut_ptr());

            ogg_sync_clear(self.oy.as_mut_ptr());
        }
    }
}
