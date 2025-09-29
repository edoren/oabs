use std::{mem::MaybeUninit, net::SocketAddr, time::Duration};

use anyhow::{Result, anyhow};
use aotuv_lancer_vorbis_sys::*;
use cpal::{
    Device, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use log::{debug, error, info, trace};
use ogg_next_sys::*;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Producer, Split},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{
        self,
        mpsc::{self},
    },
};
use tokio_util::sync::CancellationToken;

use crate::common::{
    constants::{DEFAULT_LATENCY, DEFAULT_VOLUME},
    serializers::{OABSMessage, SupportedStreamConfigDeserialize},
};

#[derive(Clone)]
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
    unsafe {
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
    unsafe {
        let num_channels = vi.assume_init_ref().channels as usize;
        let convsize = 4096 / num_channels;

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
                            let samples =
                                vorbis_synthesis_pcmout(vd.as_mut_ptr(), &mut pcm) as usize;
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

                            producer.push_slice(&interlaced_data);

                            // Tell libvorbis how many samples we actually consumed
                            vorbis_synthesis_read(vd.as_mut_ptr(), bout as i32);
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
}

pub struct ClientController {
    selected_device: Option<Device>,
    latency: (sync::watch::Sender<u32>, sync::watch::Receiver<u32>),
    volume: (sync::watch::Sender<u32>, sync::watch::Receiver<u32>),

    cancellation_token: Option<CancellationToken>,

    decode_joinh: Option<std::thread::JoinHandle<Result<()>>>,
    player_joinh: Option<std::thread::JoinHandle<Result<()>>>,
    stream_receiver_joinh: Option<tokio::task::JoinHandle<Result<()>>>,
    server_receiver_joinh: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl ClientController {
    pub fn new() -> Self {
        Self {
            selected_device: cpal::default_host().default_output_device(),
            latency: sync::watch::channel(DEFAULT_LATENCY as u32),
            volume: sync::watch::channel(DEFAULT_VOLUME as u32),
            cancellation_token: None,
            decode_joinh: None,
            player_joinh: None,
            stream_receiver_joinh: None,
            server_receiver_joinh: None,
        }
    }

    pub fn get_latency(&self) -> u32 {
        *self.volume.1.borrow()
    }

    pub fn get_volume(&self) -> u32 {
        *self.latency.1.borrow()
    }

    pub fn get_devices() -> Vec<Device> {
        let host = cpal::default_host();
        if let Ok(device) = host.output_devices() {
            device.filter(|x| x.name().is_ok()).collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_device_names() -> Vec<String> {
        let output_devices = Self::get_devices();
        output_devices
            .iter()
            .map(|x| x.name().unwrap_or_default())
            .collect()
    }

    pub fn set_device_name(&mut self, device_name: String) {
        self.selected_device = Self::get_devices()
            .into_iter()
            .find(|d| d.name().is_ok_and(|d_name| d_name == device_name));
    }

    pub fn set_volume(&self, volume: u32) {
        self.volume.0.send_replace(volume);
    }

    pub fn set_latency(&self, latency: u32) {
        self.latency.0.send_replace(latency);
    }

    fn create_decode_task(
        &self,
        mut encoded_rx: mpsc::Receiver<(StreamStatus, Vec<u8>)>,
        mut producer: <HeapRb<f32> as ringbuf::traits::Split>::Prod,
    ) -> std::thread::JoinHandle<Result<()>> {
        std::thread::spawn(move || {
            debug!("Starting decode task");

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

            'decode_loop: loop {
                let (stream_status, data) = match encoded_rx.blocking_recv() {
                    Some(val) => val,
                    None => continue,
                };

                match stream_status {
                    StreamStatus::Starting => unsafe {
                        let _ = decode_first_package(
                            &data, &mut oy, &mut os, &mut og, &mut op, &mut vi, &mut vc, &mut vd,
                            &mut vb,
                        );
                    },
                    StreamStatus::Streaming => unsafe {
                        let _ = decode_next_package(
                            &data,
                            &mut oy,
                            &mut os,
                            &mut og,
                            &mut op,
                            &mut vi,
                            &mut vd,
                            &mut vb,
                            &mut producer,
                        );
                    },
                    StreamStatus::Stopped => {
                        break 'decode_loop;
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

            debug!("Stopping decode task");
            Ok(())
        })
    }

    async fn stream_receiver_task(
        client_id: String,
        server_address: SocketAddr,
        encoded_tx: mpsc::Sender<(StreamStatus, Vec<u8>)>,
        child_token: CancellationToken,
    ) -> Result<()> {
        debug!("Starting stream receiver task");

        let local_addr: SocketAddr = "0.0.0.0:12312".parse()?;
        let cli = UdpSocket::bind(local_addr).await?;
        cli.connect(server_address).await?;

        debug!("Using local address: {}", cli.local_addr()?);

        let mut buf = [0; 10240];

        let mut stream_status = StreamStatus::Starting;
        loop {
            match stream_status {
                StreamStatus::Starting => {
                    cli.send("START".as_bytes()).await?;
                }
                _ => {}
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
                _ = child_token.cancelled() => {
                    stream_status = StreamStatus::Stopped;
                    0
                }
            };

            let data = if recv_len > 0 {
                buf[..recv_len].to_owned()
            } else {
                Vec::new()
            };

            let _ = encoded_tx.send((stream_status.clone(), data)).await;

            match stream_status {
                StreamStatus::Starting => {
                    let _ = cli.send(format!("HEADER_OK {client_id}").as_bytes()).await;
                    stream_status = StreamStatus::Streaming;
                }
                StreamStatus::Streaming => {}
                StreamStatus::Stopped => {
                    break;
                }
            };
        }

        debug!("Stopping stream receiver task");
        Ok::<(), anyhow::Error>(())
    }

    fn create_stream_receiver_task(
        client_id: String,
        server_address: SocketAddr,
        encoded_tx: mpsc::Sender<(StreamStatus, Vec<u8>)>,
        child_token: CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            Self::stream_receiver_task(client_id, server_address, encoded_tx, child_token).await
        })
    }

    async fn server_receiver_task(
        mut tcp_stream: TcpStream,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        debug!("Starting server connection task");

        let mut close_requested = false;

        let child_token = cancellation_token.child_token();
        loop {
            let data = tokio::select! {
                result = recv_data(&mut tcp_stream) => {
                    match result {
                        Ok(data) => data,
                        Err(e) => {
                            error!("{e:?}");
                            continue;
                        },
                    }
                },
                _ = child_token.cancelled() => {
                    close_requested = true;
                    vec![]
                }
            };

            if close_requested {
                if let Err(e) = send_data(&mut tcp_stream, "CLOSE".as_bytes()).await {
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
                    if let Err(e) = send_data(&mut tcp_stream, "PONG".as_bytes()).await {
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

        cancellation_token.cancel();

        debug!("Stopping server connection task");

        Ok(())
    }

    fn create_server_receiver_task(
        tcp_stream: TcpStream,
        cancellation_token: CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(
            async move { Self::server_receiver_task(tcp_stream, cancellation_token).await },
        )
    }

    fn create_playback_task(
        &self,
        config: SupportedStreamConfig,
        device: Device,
        mut consumer: <HeapRb<f32> as ringbuf::traits::Split>::Cons,
        child_token: CancellationToken,
    ) -> std::thread::JoinHandle<Result<()>> {
        let volume = self.volume.1.clone();

        std::thread::spawn(move || {
            debug!("Starting playback task");

            let err_fn = move |err| {
                error!("An error occurred on stream: {}", err);
            };

            let playback_stream = device.build_output_stream(
                &config.into(),
                move |data: &mut [f32], _: &_| {
                    let requested_count = data.len();
                    let read_count = consumer.pop_slice(data);
                    data.iter_mut()
                        .for_each(|s| *s = *s * *volume.borrow() as f32 / 100.0);
                    if read_count < requested_count {
                        trace!("input stream fell behind: try increasing latency");
                    }
                },
                err_fn,
                None,
            )?;

            playback_stream.play()?;
            loop {
                if child_token.is_cancelled() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            playback_stream.pause()?;

            drop(playback_stream);

            debug!("Stopping playback task");

            Ok(())
        })
    }

    pub async fn start(&mut self, server_address: SocketAddr) -> Result<()> {
        let cancellation_token = CancellationToken::new();

        let device = self
            .selected_device
            .as_ref()
            .ok_or(anyhow!("Could not find default output device"))?;

        let mut tcp_stream = TcpStream::connect(server_address).await?;

        info!("Connected to server {}", server_address);

        // Wait for the socket to be readable
        tcp_stream.readable().await?;

        // Creating the buffer **after** the `await` prevents it from
        // being stored in the async task.
        let client_id: String = loop {
            match recv_data(&mut tcp_stream).await {
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
            match recv_data(&mut tcp_stream).await {
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
        let latency_frames =
            (*self.latency.1.borrow() as f32 / 1_000.0) * config.sample_rate().0 as f32;
        let latency_samples = latency_frames as usize * config.channels() as usize;

        let (encoded_tx, encoded_rx) = mpsc::channel::<(StreamStatus, Vec<u8>)>(20);

        // The buffer to share samples
        let (mut producer, consumer) = HeapRb::<f32>::new(latency_samples).split();

        debug!("Latency Samples: {latency_samples}");

        // Fill the samples with 0.0 equal to the length of the delay.
        for _ in 0..latency_samples {
            // The ring buffer has twice as much space as necessary to add latency here,
            // so this should never fail
            producer
                .try_push(0.0)
                .map_err(|e| anyhow!("Could not fill buffer: {e}"))?;
        }

        self.decode_joinh
            .replace(self.create_decode_task(encoded_rx, producer));
        self.player_joinh.replace(self.create_playback_task(
            config,
            device.clone(),
            consumer,
            cancellation_token.child_token(),
        ));
        self.stream_receiver_joinh
            .replace(Self::create_stream_receiver_task(
                client_id.clone(),
                server_address,
                encoded_tx,
                cancellation_token.child_token(),
            ));
        self.server_receiver_joinh
            .replace(Self::create_server_receiver_task(
                tcp_stream,
                cancellation_token.clone(),
            ));

        self.cancellation_token.replace(cancellation_token);

        Ok(())
    }

    pub async fn wait(&mut self) -> Result<()> {
        if let Some(joinh) = self.stream_receiver_joinh.take() {
            if let Err(e) = joinh.await {
                error!("{e:?}");
            };
        }
        if let Some(joinh) = self.server_receiver_joinh.take() {
            if let Err(e) = joinh.await {
                error!("{e:?}");
            };
        }
        if let Some(joinh) = self.decode_joinh.take() {
            if let Err(e) = joinh.join() {
                error!("{e:?}");
            };
        }
        if let Some(joinh) = self.player_joinh.take() {
            if let Err(e) = joinh.join() {
                error!("{e:?}");
            };
        }
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(cancellation_token) = self.cancellation_token.take() {
            cancellation_token.cancel();
            self.wait().await?;
        }
        info!("Client closed successfully");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        return self.cancellation_token.is_some();
    }
}
