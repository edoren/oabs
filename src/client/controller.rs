use std::{mem::MaybeUninit, net::SocketAddr, time::Duration};

use anyhow::{anyhow, Result};
use aotuv_lancer_vorbis_sys::*;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device,
};
use log::{debug, error, info, trace};
use ogg_next_sys::*;
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    signal,
    sync::watch,
};

use crate::common::{
    constants::{DEFAULT_LATENCY, DEFAULT_VOLUME},
    serializers::{OABSMessage, SupportedStreamConfigDeserialize},
};

enum StreamStatus {
    Starting,
    Streaming,
    Stopped,
}

async fn send_data(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    Ok(())
}

async fn recv_data(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let len = stream.read_u16().await?;
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

unsafe fn decode_first_package(
    packet: &[u8],
    oy: &mut Box<MaybeUninit<ogg_sync_state>>,
    os: &mut Box<MaybeUninit<ogg_stream_state>>,

    og: &mut Box<MaybeUninit<ogg_page>>,
    op: &mut Box<MaybeUninit<ogg_packet>>,

    vi: &mut Box<MaybeUninit<vorbis_info>>,

    vc: &mut Box<MaybeUninit<vorbis_comment>>,
    vd: &mut Box<MaybeUninit<vorbis_dsp_state>>,
    vb: &mut Box<MaybeUninit<vorbis_block>>,
) -> Result<()> {
    let buffer_ptr = ogg_sync_buffer(oy.as_mut_ptr(), packet.len().try_into()?).cast::<u8>();
    let decode_buffer = std::slice::from_raw_parts_mut(buffer_ptr, packet.len());
    decode_buffer[..packet.len()].copy_from_slice(packet);
    ogg_sync_wrote(oy.as_mut_ptr(), packet.len().try_into()?);

    debug!("Header size: {}", packet.len());

    /* Get the first page. */
    if ogg_sync_pageout(oy.as_mut_ptr(), og.as_mut_ptr()) != 1 {
        return Err(anyhow!("Input does not appear to be an Ogg bitstream."));
    }

    let serial_no = ogg_page_serialno(og.as_mut_ptr());
    debug!("Serial number found: {serial_no}");
    ogg_stream_init(os.as_mut_ptr(), serial_no);

    if ogg_stream_pagein(os.as_mut_ptr(), og.as_mut_ptr()) != 0 {
        return Err(anyhow!("Error reading first page of Ogg bitstream data."));
    }

    if ogg_stream_packetout(os.as_mut_ptr(), op.as_mut_ptr()) != 1 {
        return Err(anyhow!("Error reading initial header packet."));
    }

    if vorbis_synthesis_idheader(op.as_mut_ptr()) != 1 {
        return Err(anyhow!("This is not a valid Vorbis first packet."));
    }

    vorbis_info_init(vi.as_mut_ptr());
    vorbis_comment_init(vc.as_mut_ptr());

    if vorbis_synthesis_headerin(vi.as_mut_ptr(), vc.as_mut_ptr(), op.as_mut_ptr()) < 0 {
        return Err(anyhow!(
            "This Ogg bitstream does not contain Vorbis audio data."
        ));
    }

    let mut i = 0;

    while i < 2 {
        while i < 2 {
            let mut result = ogg_sync_pageout(oy.as_mut_ptr(), og.as_mut_ptr());
            if result == 0 {
                break;
            }

            // Don't complain about missing or corrupt data yet. We'll
            // catch it at the packet output phase
            if result == 1 {
                ogg_stream_pagein(os.as_mut_ptr(), og.as_mut_ptr());
                // We can ignore any errors here as they'll also become apparent at packetout
                while i < 2 {
                    result = ogg_stream_packetout(os.as_mut_ptr(), op.as_mut_ptr());
                    if result == 0 {
                        break;
                    }
                    if result < 0 {
                        // Uh oh; data at some point was corrupted or missing!
                        // We can't tolerate that in a header. Die.
                        return Err(anyhow!("Corrupt secondary header. Exiting."));
                    }
                    result = vorbis_synthesis_headerin(
                        vi.as_mut_ptr(),
                        vc.as_mut_ptr(),
                        op.as_mut_ptr(),
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
        let vi_ref = vi.assume_init_ref();
        let vc_ref = vc.assume_init_ref();
        debug!(
            "Bitstream is {} channel, {}Hz",
            vi_ref.channels, vi_ref.rate
        );
        let vendor = std::ffi::CStr::from_ptr(vc_ref.vendor);
        debug!("Encoded by: {}", vendor.to_str()?);
    }

    vorbis_synthesis_init(vd.as_mut_ptr(), vi.as_mut_ptr());
    vorbis_block_init(vd.as_mut_ptr(), vb.as_mut_ptr());

    Ok(())
}

unsafe fn decode_next_package<P: Producer<Item = f32>>(
    packet: &[u8],
    oy: &mut Box<MaybeUninit<ogg_sync_state>>,
    os: &mut Box<MaybeUninit<ogg_stream_state>>,

    og: &mut Box<MaybeUninit<ogg_page>>,
    op: &mut Box<MaybeUninit<ogg_packet>>,

    vi: &mut Box<MaybeUninit<vorbis_info>>,

    // vc: &mut Box<MaybeUninit<vorbis_comment>>,
    vd: &mut Box<MaybeUninit<vorbis_dsp_state>>,
    vb: &mut Box<MaybeUninit<vorbis_block>>,

    producer: &mut P,
) -> Result<()> {
    let convsize = 4096 / vi.assume_init_ref().channels;

    let buffer_ptr = ogg_sync_buffer(oy.as_mut_ptr(), packet.len().try_into()?).cast::<u8>();
    let decode_buffer = std::slice::from_raw_parts_mut(buffer_ptr, packet.len());
    decode_buffer[..packet.len()].copy_from_slice(packet);
    ogg_sync_wrote(oy.as_mut_ptr(), packet.len().try_into()?);

    loop {
        let mut result = ogg_sync_pageout(oy.as_mut_ptr(), og.as_mut_ptr());
        if result == 0 {
            break;
        }

        if result < 0 {
            // Missing or corrupt data at this page position
            error!("Corrupt or missing data in bitstream; continuing...");
            break;
        } else {
            // Can safely ignore errors at this point
            ogg_stream_pagein(os.as_mut_ptr(), og.as_mut_ptr());

            loop {
                result = ogg_stream_packetout(os.as_mut_ptr(), op.as_mut_ptr());

                if result == 0 {
                    break;
                }

                if result < 0 {
                    // Missing or corrupt data at this page position
                    // no reason to complain; already complained above
                } else {
                    // We have a packet. Decode it
                    if vorbis_synthesis(vb.as_mut_ptr(), op.as_mut_ptr()) == 0 {
                        vorbis_synthesis_blockin(vd.as_mut_ptr(), vb.as_mut_ptr());
                    }

                    // pcm is a multichannel float vector. In stereo, for example, pcm[0] is left,
                    // and pcm[1] is right. samples is the size of each channel. Convert the float
                    // values (-1.0 <= range <= 1.0) to whatever PCM format and write it out
                    let mut pcm: *mut *mut f32 = std::ptr::null_mut();
                    loop {
                        let samples = vorbis_synthesis_pcmout(vd.as_mut_ptr(), &mut pcm);
                        if samples == 0 {
                            break;
                        }

                        let bout = std::cmp::min(samples, convsize);

                        // Get the channel 1
                        let mono = *pcm.offset(0);
                        let data = std::slice::from_raw_parts(mono, bout as usize);

                        producer.push_slice(data);

                        // Tell libvorbis how many samples we actually consumed
                        vorbis_synthesis_read(vd.as_mut_ptr(), bout);
                    }
                }
            }
        }

        if ogg_page_eos(og.as_mut_ptr()) > 0 {
            break;
        };
    }

    Ok(())
}

pub struct ClientController {
    selected_device: Option<Device>,
    latency: u32,
    volume: u32,
}

impl ClientController {
    pub fn new() -> Self {
        Self {
            selected_device: cpal::default_host().default_output_device(),
            latency: DEFAULT_LATENCY as u32,
            volume: DEFAULT_VOLUME as u32,
        }
    }

    pub fn get_latency(&self) -> u32 {
        self.volume
    }

    pub fn get_volume(&self) -> u32 {
        self.latency
    }

    pub fn get_devices(&self) -> Vec<Device> {
        let host = cpal::default_host();
        if let Ok(device) = host.output_devices() {
            device.filter(|x| x.name().is_ok()).collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_device_names(&self) -> Vec<String> {
        let output_devices = self.get_devices();
        output_devices
            .iter()
            .map(|x| x.name().unwrap_or_default())
            .collect()
    }

    pub fn set_device(&mut self, device_name: String) {
        self.selected_device = self
            .get_devices()
            .into_iter()
            .find(|d| d.name().is_ok_and(|d_name| d_name == device_name));
    }

    pub fn set_volume(&mut self, volume: u32) {
        self.volume = volume;
    }

    pub fn set_latency(&mut self, latency: u32) {
        self.latency = latency;
    }

    pub async fn start(&self, server_address: SocketAddr) -> Result<()> {
        let device = self
            .selected_device
            .as_ref()
            .ok_or(anyhow!("Could not find default output device"))?;

        debug!("Connecting to server {}", server_address);
        let mut stream = TcpStream::connect(server_address).await?;

        info!("Connected to server {}", server_address);

        // Wait for the socket to be readable
        stream.readable().await?;

        // Creating the buffer **after** the `await` prevents it from
        // being stored in the async task.
        let client_id: String = loop {
            match recv_data(&mut stream).await {
                Ok(data) => {
                    let value: OABSMessage = serde_json::from_slice(&data)?;
                    if let OABSMessage::ClientId { id } = value {
                        break id;
                    }
                    return Err(anyhow!("Client id not found"));
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        };
        debug!("Receiving client_id {:?}", client_id);

        // Try to read data, this may still fail with `WouldBlock`
        // if the readiness event is a false positive.
        let config = loop {
            match recv_data(&mut stream).await {
                Ok(data) => {
                    break serde_json::from_slice(&data)
                        .map(|SupportedStreamConfigDeserialize(dur)| dur)?;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        };

        debug!("Receiving config {:?}", config);

        // Create a delay in case the input and output devices aren't synced.
        let latency_frames = (self.latency as f32 / 1_000.0) * config.sample_rate().0 as f32;
        let latency_samples = latency_frames as usize * config.channels() as usize;

        // The buffer to share samples
        let ring = HeapRb::<f32>::new(latency_samples);
        let (mut producer, mut consumer) = ring.split();

        debug!("Latency Samples: {latency_samples}");

        // Fill the samples with 0.0 equal to the length of the delay.
        for _ in 0..latency_samples {
            // The ring buffer has twice as much space as necessary to add latency here,
            // so this should never fail
            producer
                .try_push(0.0)
                .map_err(|e| anyhow!("Could not fill buffer: {e}"))?;
        }

        let (close_tx, close_rx) = watch::channel(false);

        let err_fn = move |err| {
            error!("an error occurred on stream: {}", err);
        };

        let mut close_rx_stream_receiver_task = close_rx.clone();
        let stream_receiver_task = async move {
            debug!("Starting stream receiver task");

            let local_addr: SocketAddr = "0.0.0.0:12312".parse()?;
            let cli = UdpSocket::bind(local_addr).await?;
            cli.connect(server_address).await?;

            debug!("Using local address: {}", cli.local_addr()?);

            let mut buf = [0; 10240];

            let mut oy: Box<MaybeUninit<ogg_sync_state>> = Box::new(MaybeUninit::uninit());
            let mut os: Box<MaybeUninit<ogg_stream_state>> = Box::new(MaybeUninit::uninit());

            let mut og: Box<MaybeUninit<ogg_page>> = Box::new(MaybeUninit::uninit());
            let mut op: Box<MaybeUninit<ogg_packet>> = Box::new(MaybeUninit::uninit());

            let mut vi: Box<MaybeUninit<vorbis_info>> = Box::new(MaybeUninit::uninit());

            let mut vc: Box<MaybeUninit<vorbis_comment>> = Box::new(MaybeUninit::uninit());
            let mut vd: Box<MaybeUninit<vorbis_dsp_state>> = Box::new(MaybeUninit::uninit());
            let mut vb: Box<MaybeUninit<vorbis_block>> = Box::new(MaybeUninit::uninit());

            unsafe {
                ogg_sync_init(oy.as_mut_ptr());
            };

            let mut stream_status = StreamStatus::Starting;
            loop {
                match stream_status {
                    StreamStatus::Starting => {
                        cli.send("START".as_bytes()).await?;
                    }
                    StreamStatus::Streaming => {}
                    StreamStatus::Stopped => {
                        break;
                    }
                };

                let recv_len = tokio::select! {
                    result = cli.recv(&mut buf) => {
                        match result {
                            Ok(len) => len,
                            Err(e) => {
                                error!("{e:?}");
                                continue;
                            },
                        }
                    },
                    result = close_rx_stream_receiver_task.changed() => {
                        if result.is_ok() && *close_rx_stream_receiver_task.borrow_and_update() {
                            stream_status = StreamStatus::Stopped;
                        }
                        continue;
                    }
                };

                match stream_status {
                    StreamStatus::Starting => {
                        unsafe {
                            decode_first_package(
                                &buf[..recv_len],
                                &mut oy,
                                &mut os,
                                &mut og,
                                &mut op,
                                &mut vi,
                                &mut vc,
                                &mut vd,
                                &mut vb,
                            )?;
                        }
                        let _ = cli.send(format!("HEADER_OK {client_id}").as_bytes()).await;
                        stream_status = StreamStatus::Streaming;
                    }
                    StreamStatus::Streaming => unsafe {
                        decode_next_package(
                            &buf[..recv_len],
                            &mut oy,
                            &mut os,
                            &mut og,
                            &mut op,
                            &mut vi,
                            &mut vd,
                            &mut vb,
                            &mut producer,
                        )?;
                    },
                    StreamStatus::Stopped => {
                        break;
                    }
                };
            }

            unsafe {
                vorbis_block_clear(vb.as_mut_ptr());
                vorbis_dsp_clear(vd.as_mut_ptr());

                ogg_stream_clear(os.as_mut_ptr());
                vorbis_comment_clear(vc.as_mut_ptr());
                vorbis_info_clear(vi.as_mut_ptr());

                ogg_sync_clear(oy.as_mut_ptr());
            }

            debug!("Closing stream receiver task");
            Ok::<(), anyhow::Error>(())
        };

        let config_clone = config.clone();
        let device_clone = device.clone();
        let mut player_close_rx = close_rx.clone();
        let volume = self.volume;
        let player_task = move || {
            debug!("Starting player task");

            let playback_stream = device_clone.build_output_stream(
                &config_clone.into(),
                move |data: &mut [f32], _: &_| {
                    let requested_count = data.len();
                    let read_count = consumer.pop_slice(data);
                    data.iter_mut()
                        .for_each(|s| *s = *s * volume as f32 / 100.0);
                    if read_count < requested_count {
                        trace!("input stream fell behind: try increasing latency");
                    }
                },
                err_fn,
                None,
            )?;

            debug!("Starting playback");
            playback_stream.play()?;
            loop {
                if let Ok(changed) = player_close_rx.has_changed() {
                    if changed && *player_close_rx.borrow_and_update() {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            debug!("Stopping playback");
            playback_stream.pause()?;

            drop(playback_stream);

            debug!("Stopping player task");
            Ok::<(), anyhow::Error>(())
        };

        let server_receiver_close_tx = close_tx.clone();
        let mut server_receiver_close_rx = close_rx.clone();
        let server_receiver = async move {
            debug!("Starting server connection task");

            let mut close_requested = false;

            loop {
                let data = tokio::select! {
                    result = recv_data(&mut stream) => {
                        match result {
                            Ok(data) => data,
                            Err(e) => {
                                error!("{e:?}");
                                continue;
                            },
                        }
                    },
                    result = server_receiver_close_rx.changed() => {
                        if result.is_ok() && *server_receiver_close_rx.borrow_and_update() {
                            close_requested = true;
                            vec![]
                        } else {
                            error!("Error getting close signal");
                            continue;
                        }
                    }
                };

                if close_requested {
                    if let Err(e) = send_data(&mut stream, "CLOSE".as_bytes()).await {
                        error!("Failed to send CLOSE: {e:?}");
                    }
                    break;
                }

                let value = match std::str::from_utf8(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if value == "PING" {
                    trace!("PING");
                    loop {
                        if let Err(e) = send_data(&mut stream, "PONG".as_bytes()).await {
                            debug!("Error Sending Pong: {e:?}");
                        } else {
                            trace!("PONG");
                            break;
                        }
                    }
                }
                if value == "CLOSE" {
                    debug!("Server requested close");
                    break;
                }
            }

            drop(server_receiver_close_rx);
            server_receiver_close_tx.send(true)?;

            debug!("Stopping server connection task");

            Ok::<(), anyhow::Error>(())
        };

        let close_task_close_tx = close_tx.clone();
        let mut close_task_close_rx = close_rx;

        #[cfg(not(target_os = "windows"))]
        let close_task = async move {
            tokio::select! {
                _ = signal::ctrl_c() => { },
                _ = close_task_close_rx.changed() => { },

            };
            close_task_close_tx.send(true)?;
            return Ok::<(), anyhow::Error>(());
        };

        #[cfg(target_os = "windows")]
        let close_task = async move {
            let mut ctrl_c_signal = signal::windows::ctrl_c()?;
            let mut ctrl_close_signal = signal::windows::ctrl_close()?;
            let mut ctrl_break_signal = signal::windows::ctrl_break()?;
            let mut ctrl_logoff_signal = signal::windows::ctrl_logoff()?;
            let mut ctrl_shutdown_signal = signal::windows::ctrl_shutdown()?;
            tokio::select! {
                _ = ctrl_c_signal.recv() => { },
                _ = ctrl_close_signal.recv() => { },
                _ = ctrl_break_signal.recv() => { },
                _ = ctrl_logoff_signal.recv() => { },
                _ = ctrl_shutdown_signal.recv() => { },
                _ = close_task_close_rx.changed() => { },
            }
            drop(close_task_close_rx);
            close_task_close_tx.send(true)?;
            return Ok::<(), anyhow::Error>(());
        };

        let close_joinh = tokio::spawn(close_task);

        let player_joinh = std::thread::spawn(player_task);

        let (close_res, stream_receiver_res, server_receiver_res) =
            tokio::join!(close_joinh, stream_receiver_task, server_receiver);

        let player_res = player_joinh.join();

        if let Err(e) = close_res {
            error!("{e:?}");
        };
        if let Err(e) = player_res {
            error!("{e:?}");
        };
        if let Err(e) = stream_receiver_res {
            error!("{e:?}");
        };
        if let Err(e) = server_receiver_res {
            error!("{e:?}");
        };

        close_tx.closed().await;

        info!("Client closed successfully");

        Ok(())
    }
}
