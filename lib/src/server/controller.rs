use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Deref,
    slice,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use aotuv_lancer_vorbis_sys::*;
use argon2::{Argon2, password_hash::SaltString};
use chacha20poly1305::{
    XChaCha20Poly1305,
    aead::{Aead, AeadCore, KeyInit},
};
use cpal::{
    Device, DevicesError, SampleFormat, SampleRate, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures::TryFutureExt;
use local_ip_address::local_ip;
use log::{debug, error, trace};
use ogg_next_sys::*;
use rand::{Rng, rngs::OsRng, thread_rng};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    signal,
    sync::mpsc,
    task::{JoinSet, yield_now},
    time::{Instant, sleep, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::{
    common::serializers::{OABSMessage, SupportedStreamConfigSerialize},
    server::upnp::{AddPortConfig, DeletePortConfig, PortMappingProtocol, UPnP},
};

enum ClientStatus {
    Connected { id: String },
    Disconnected { id: String },
}

async fn send_data(stream: &mut TcpStream, data: &[u8]) -> Result<(), io::Error> {
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    Ok(())
}

async fn recv_data(stream: &mut TcpStream) -> Result<Vec<u8>, io::Error> {
    let len = stream.read_u16().await?;
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

static COUNTER: AtomicUsize = AtomicUsize::new(1);
fn get_new_id() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

async fn client_handler(
    mut stream: TcpStream,
    salt: String,
    encrypt_key: [u8; 32],
    config: SupportedStreamConfig,
    client_status_tx: tokio::sync::mpsc::Sender<ClientStatus>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let cipher = XChaCha20Poly1305::new((&encrypt_key).into());

    let random_verifier = OsRng.r#gen::<[u8; 8]>().to_vec();

    let auth_msg = serde_json::to_vec(&OABSMessage::AuthenticateRequest {
        salt: salt.to_string(),
        verifier: random_verifier.clone(),
    })?;

    debug!("Sending auth request");
    send_data(&mut stream, &auth_msg).await?;

    let auth_result = timeout_at(
        Instant::now() + Duration::from_secs(5),
        recv_data(&mut stream).map_err(|e| anyhow!("Failed receiving auth payload {e}")),
    )
    .await
    .map_err(|e| anyhow!("Failed authentication, client timeout: {e}"))
    .flatten()?;

    let auth_msg: OABSMessage =
        serde_json::from_slice(&auth_result).context("Could not parse client message")?;
    debug!("Message received {auth_msg:?}");
    if let OABSMessage::Authenticate { verifier } = auth_msg {
        let ciphertext = &verifier[..verifier.len() - 24];
        let nonce = verifier[verifier.len() - 24..]
            .iter()
            .map(|v| v ^ 0x5)
            .collect::<Vec<u8>>();
        let verifier_decrypted = cipher
            .decrypt(nonce.deref().into(), ciphertext)
            .map_err(|e| anyhow!("Failed decrypting verifier: {e}"))?;
        if verifier_decrypted != random_verifier {
            return Err(anyhow!("FAILED VERIFIER"));
        }
    }

    let client_id = get_new_id().to_string();

    let client_id_msg = serde_json::to_vec(&OABSMessage::ClientId {
        id: client_id.clone(),
    })?;
    send_data(&mut stream, &client_id_msg)
        .await
        .context("Failed sending client id")?;

    let value = serde_json::to_vec(&SupportedStreamConfigSerialize(&config))?;
    send_data(&mut stream, &value).await?;

    let _ = client_status_tx
        .send(ClientStatus::Connected {
            id: client_id.clone(),
        })
        .await;

    let client_task = async {
        let instanto = Instant::now();
        let mut ping_timeout = instanto + Duration::from_secs(5);
        let mut pong_deadline = instanto + Duration::from_secs(10);

        loop {
            tokio::select! {
                _ = sleep_until(ping_timeout) => {
                    match send_data(&mut stream, "PING".as_bytes()).await {
                        Ok(_) => {
                            trace!("Sending PING to client with id {client_id:?}");
                            ping_timeout = Instant::now() + Duration::from_secs(5);
                        }
                        Err(e) => match e.kind() {
                            io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset => {
                                debug!("Connection aborted from client {client_id}");
                                break;
                            }
                            _ => {
                                error!("Error sending PING from client with id {client_id:?}: {e:?}");
                            }
                        },
                    }
                },
                result = timeout_at(pong_deadline, recv_data(&mut stream)) => {
                    let read_result = match result {
                        Ok(result) => result,
                        Err(_) => {
                            debug!("Client with id {client_id:?} forgot to PONG");
                            break;
                        }
                    };

                    match read_result {
                        Err(e) => {
                            if e.kind() == io::ErrorKind::UnexpectedEof {
                                debug!("Connection aborted from client with id {client_id:?}");
                                break;
                            }
                        }
                        Ok(data) => {
                            if data.len() == 0 {
                                debug!("Empty data frame for client with id {client_id:?}. Closing.");
                                break;
                            }
                            if let Ok(value) = std::str::from_utf8(&data) {
                                if value == "PONG" {
                                    trace!("Received PONG from client with id {client_id:?}");
                                    pong_deadline = Instant::now() + Duration::from_secs(10);
                                }
                                if value == "CLOSE" {
                                    debug!("Close requested from client with id {client_id:?}");
                                    break;
                                }
                            }
                        }
                    }
                }
            };
        }
    };

    tokio::select! {
        _ = client_task => {},
        _ = cancellation_token.cancelled() => {}
    };

    let _ = send_data(&mut stream, "CLOSE".as_bytes()).await;

    let _ = client_status_tx
        .send(ClientStatus::Disconnected {
            id: client_id.clone(),
        })
        .await;

    debug!("Client closed with id {client_id:?}");

    Ok(())
}

async fn payload_server(
    server_addr: SocketAddr,
    salt: String,
    encrypt_key: [u8; 32],
    config: SupportedStreamConfig,
    client_status_tx: mpsc::Sender<ClientStatus>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    debug!("Starting payload server");

    let listener = TcpListener::bind(server_addr).await?;
    debug!("Payload server listening on: {}", listener.local_addr()?);
    let mut streams = JoinSet::new();
    loop {
        let stream = tokio::select! {
            result = listener.accept() => {
                result?.0
            },
            _ = cancellation_token.cancelled() => {
                break;
            }
        };
        streams.spawn(
            client_handler(
                stream,
                salt.clone(),
                encrypt_key.clone(),
                config.clone(),
                client_status_tx.clone(),
                cancellation_token.child_token(),
            )
            .inspect_err(|e| error!("Client error: {e}")),
        );
    }

    while let Some(_res) = streams.join_next().await {}

    debug!("Stopping payload server");
    Ok(())
}

async fn stream_server(
    server_addr: SocketAddr,
    encrypt_key: [u8; 32],
    quality: f32,
    config: SupportedStreamConfig,
    mut data_send_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    mut client_status_rx: mpsc::Receiver<ClientStatus>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    debug!("Starting stream server");

    let sock = UdpSocket::bind(server_addr).await?;
    debug!("Stream server listening on: {}", sock.local_addr()?);

    let cipher = XChaCha20Poly1305::new((&encrypt_key).into());

    let mut os: Box<MaybeUninit<ogg_stream_state>> = Box::new(MaybeUninit::uninit());
    let mut og: Box<MaybeUninit<ogg_page>> = Box::new(MaybeUninit::uninit());
    let mut op: Box<MaybeUninit<ogg_packet>> = Box::new(MaybeUninit::uninit());

    let mut vi: Box<MaybeUninit<vorbis_info>> = Box::new(MaybeUninit::uninit());
    let mut vc: Box<MaybeUninit<vorbis_comment>> = Box::new(MaybeUninit::uninit());
    let mut vd: Box<MaybeUninit<vorbis_dsp_state>> = Box::new(MaybeUninit::uninit());
    let mut vb: Box<MaybeUninit<vorbis_block>> = Box::new(MaybeUninit::uninit());

    let serial_num;
    let mut header_data: Vec<u8> = Vec::new();

    unsafe {
        vorbis_info_init(vi.as_mut_ptr());

        let ret = vorbis_encode_init_vbr(
            vi.as_mut_ptr(),
            config.channels().into(),
            config.sample_rate().0.try_into()?,
            quality,
        );
        if ret != 0 {
            panic!("vorbis_encode_init_vbr returned {ret}");
        }

        vorbis_comment_init(vc.as_mut_ptr());
        // vorbis_comment_add_tag(vc.as_mut_ptr(), "ENCODER","encoder_example.c");

        vorbis_analysis_init(vd.as_mut_ptr(), vi.as_mut_ptr());
        vorbis_block_init(vd.as_mut_ptr(), vb.as_mut_ptr());

        serial_num = thread_rng().r#gen();
        debug!("Serial: {}", serial_num);
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

    debug!("Header size: {}", header_data.len());

    let mut buffer = [0; 256];
    let mut encoded_data: Vec<u8> = Vec::new();
    let mut registered_clients: HashMap<String, Option<SocketAddr>> = HashMap::new();

    let mut client_detected = false;

    'stream_loop: loop {
        if cancellation_token.is_cancelled() {
            break;
        }

        if let Ok(client_status) = client_status_rx.try_recv() {
            match client_status {
                ClientStatus::Connected { id } => {
                    registered_clients.insert(id, None);
                }
                ClientStatus::Disconnected { id } => {
                    registered_clients.remove(&id);
                    debug!("Removing client with id {id:?}");
                    client_detected = !registered_clients.is_empty();
                }
            }
        }

        match sock.try_recv_from(&mut buffer) {
            Ok((len, source)) => {
                let msg = unsafe { std::str::from_utf8_unchecked(&buffer[..len]) };
                if msg == "START" {
                    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
                    let mut header_data_ecrypted = cipher
                        .encrypt(&nonce, header_data.deref())
                        .map_err(|e| anyhow!("{e}"))?;
                    header_data_ecrypted.extend_from_slice(
                        nonce.iter().map(|v| v ^ 0x7).collect::<Vec<u8>>().deref(),
                    );

                    let _ = sock.send_to(&header_data_ecrypted, source).await?;
                }
                if msg.starts_with("HEADER_OK") {
                    if let Some(id) = msg.split(" ").nth(1) {
                        client_detected = true;
                        registered_clients.insert(id.into(), Some(source));
                        debug!("Added client {source} with id {id:?}");
                    }
                }
            }
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::ConnectionReset => {}
                _ => error!("{:?}: {e}", e.kind()),
            },
        }

        let raw_samples: Vec<f32> = loop {
            let result = data_send_rx.recv().await;
            if let Some(data) = result {
                break data;
            }
            if data_send_rx.is_closed() {
                break 'stream_loop;
            }
            yield_now().await;
        };

        if registered_clients.is_empty() || !client_detected {
            continue;
        }

        let audio_channels = unsafe { vi.assume_init_ref().channels as usize };
        let sample_count = raw_samples.len() / audio_channels;

        // Deinterlace the audio obtained from cpal
        // E.g for 2 channels: [L0, R0, L1, R1, L2, R2] -> [[L1, L2, L3], [R1, R2, R3]]
        let audio_block: Vec<Vec<f32>> = (0..audio_channels)
            .map(|channel| {
                (0..sample_count)
                    .map(|sample| raw_samples[sample * audio_channels + channel])
                    .collect()
            })
            .collect();

        {
            let encoder_buffer = unsafe {
                slice::from_raw_parts_mut(
                    vorbis_analysis_buffer(vd.as_mut_ptr(), sample_count.try_into()?),
                    audio_channels,
                )
            };

            for (channel_samples, channel_encode_buffer) in
                audio_block.iter().zip(encoder_buffer.iter_mut())
            {
                let channel_samples: &Vec<f32> = channel_samples.as_ref();
                if channel_samples.len() != sample_count {
                    panic!("Invalid channel sample count");
                }

                unsafe {
                    channel_samples
                        .as_ptr()
                        .copy_to_nonoverlapping(*channel_encode_buffer, sample_count);
                }
            }

            let res = unsafe { vorbis_analysis_wrote(vd.as_mut_ptr(), sample_count.try_into()?) };
            if res != 0 {
                panic!("vorbis_analysis_wrote returned {res}");
            }

            // SAFETY: we assume the functions inside this unsafe block follow their
            // documented contract
            unsafe {
                while vorbis_analysis_blockout(vd.as_mut_ptr(), vb.as_mut_ptr()) == 1 {
                    let res = vorbis_analysis(vb.as_mut_ptr(), std::ptr::null_mut());
                    if res != 0 {
                        panic!("vorbis_analysis returned {res}");
                    }
                    let res = vorbis_bitrate_addblock(vb.as_mut_ptr());
                    if res != 0 {
                        panic!("vorbis_bitrate_addblock returned {res}");
                    }

                    while vorbis_bitrate_flushpacket(vd.as_mut_ptr(), op.as_mut_ptr()) == 1 {
                        let res = ogg_stream_packetin(os.as_mut_ptr(), op.as_mut_ptr());
                        if res != 0 {
                            panic!("ogg_stream_packetin returned {res}");
                        }

                        while ogg_stream_flush(os.as_mut_ptr(), og.as_mut_ptr()) != 0 {
                            let ogg_page = og.assume_init_ref();
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
        }

        if encoded_data.is_empty() {
            continue;
        }

        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut encoded_data_ecrypted = cipher
            .encrypt(&nonce, encoded_data.deref())
            .map_err(|e| anyhow!("{e}"))?;
        encoded_data_ecrypted
            .extend_from_slice(nonce.iter().map(|v| v ^ 0x7).collect::<Vec<u8>>().deref());

        for source in registered_clients.values() {
            if let Some(source) = source {
                let _len = sock.send_to(&encoded_data_ecrypted, source).await?;
            }
        }

        encoded_data.clear();

        yield_now().await;
    }

    unsafe {
        ogg_stream_clear(os.as_mut_ptr());
        vorbis_block_clear(vb.as_mut_ptr());
        vorbis_dsp_clear(vd.as_mut_ptr());
        vorbis_comment_clear(vc.as_mut_ptr());
        vorbis_info_clear(vi.as_mut_ptr());
    }

    debug!("Stopping stream server");
    Ok(())
}

async fn upnp_connector(port: u16, cancellation_token: CancellationToken) -> Result<()> {
    debug!("Starting UPnP task");

    let local_ipv4 = match local_ip() {
        Ok(IpAddr::V4(ipv4)) => ipv4,
        Ok(IpAddr::V6(ipv6)) => {
            debug!("IPv6 {ipv6} not supported for UPnP");
            return Ok(());
        }
        Err(_) => return Ok(()),
    };

    let add_mapping = AddPortConfig {
        internal_client: local_ipv4,
        internal_port: port,
        external_port: port,
        protocol: PortMappingProtocol::BOTH,
        enabled: true,
        description: "OABS Server".into(),
        lease_duration: 600,
    };

    let delete_mapping = DeletePortConfig {
        external_port: port,
        protocol: PortMappingProtocol::BOTH,
    };

    let upnp = match UPnP::new().await {
        Ok(devices) => devices,
        Err(_e) => {
            debug!("Error setting UPnP port");
            return Ok(());
        }
    };

    let _ = upnp.delete_port(&delete_mapping).await;
    let result = upnp.add_port(&add_mapping).await;

    if result.is_ok() {
        if let Ok(ip) = upnp.get_external_ip_address().await {
            debug!("Running on external ip {ip}")
        } else {
            error!("Could not get external ip");
        }
    }

    let mut retry_time: u64 = (add_mapping.lease_duration / 2).into();
    loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                break;
            },
            _ = sleep(Duration::from_secs(retry_time)) => { },
        }

        let result = upnp.add_port(&add_mapping).await;
        if result.is_ok() {
            retry_time = (add_mapping.lease_duration / 2).into();
        } else {
            retry_time = 5;
        }
    }

    let _ = upnp.delete_port(&delete_mapping).await;

    debug!("Stopping UPnP task");
    Ok::<(), anyhow::Error>(())
}

pub struct ServerController {
    should_include_output_devices: bool,
    selected_device: Option<Device>,
}

impl ServerController {
    pub fn new() -> Self {
        Self {
            should_include_output_devices: false,
            selected_device: cpal::default_host().default_input_device(),
        }
    }

    pub fn set_should_include_output_devices(&mut self, should_include_output_devices: bool) {
        self.should_include_output_devices = should_include_output_devices;
    }

    pub fn get_devices(&self) -> Vec<Device> {
        let host = cpal::default_host();

        let devices_result: Result<Vec<_>, DevicesError> =
            host.input_devices().and_then(|output_devices| {
                if self.should_include_output_devices {
                    host.output_devices().map(|input_devices| {
                        output_devices
                            .into_iter()
                            .chain(input_devices.into_iter())
                            .collect()
                    })
                } else {
                    Ok(output_devices.into_iter().collect())
                }
            });
        if let Ok(devices) = devices_result {
            devices.into_iter().filter(|x| x.name().is_ok()).collect()
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

    pub async fn start(&self, port: u16, quality: f32, password: String) -> Result<()> {
        let device = self
            .selected_device
            .as_ref()
            .ok_or(anyhow!("Could not find default input device"))?;

        let server_addr = SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), port);

        let configs = device
            .supported_input_configs()?
            .chain(device.supported_output_configs()?)
            .filter(|c| c.sample_format() == SampleFormat::F32)
            .filter_map(|c| {
                c.try_with_sample_rate(SampleRate(48000))
                    .or(c.try_with_sample_rate(c.min_sample_rate()))
            })
            .collect::<Vec<SupportedStreamConfig>>();

        let config = configs
            .first()
            .ok_or(anyhow!("Could not find a valid configuration"))?;

        // Run the input stream on a separate thread.

        let cancellation_token = CancellationToken::new();
        let (data_send_tx, data_send_rx) = mpsc::unbounded_channel::<Vec<_>>();
        let (client_status_tx, client_status_rx) = mpsc::channel::<ClientStatus>(100);

        debug!("Sample Format {:?}", config.sample_format());

        let config_clone = config.clone();
        let cancellation_token_child = cancellation_token.child_token();
        let playback_capturer_task = async move {
            let stream = device.build_input_stream(
                &config_clone.into(),
                move |data: &[f32], _| {
                    let mut samples = Vec::new();
                    samples.extend_from_slice(&data);
                    let res = data_send_tx.send(data.to_vec());
                    if let Err(e) = res {
                        error!("{e:?}");
                    }
                },
                move |err| {
                    error!("En error occurred on stream: {}", err);
                },
                None,
            )?;

            debug!("Begin recording");
            stream.play()?;
            cancellation_token_child.cancelled().await;
            stream.pause()?;
            debug!("Stopped recording");

            drop(stream);

            Ok::<(), anyhow::Error>(())
        };

        #[cfg(not(target_os = "windows"))]
        let close_task = async move {
            signal::ctrl_c().await?;
            close_tx.send(true)?;
            close_tx.closed().await;
            return Ok::<(), anyhow::Error>(());
        };

        #[cfg(target_os = "windows")]
        let cancellation_token_clone = cancellation_token.clone();
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
            }
            cancellation_token_clone.cancel();
            cancellation_token_clone.cancelled().await;
            return Ok::<(), anyhow::Error>(());
        };

        let (encrypt_key, salt) = {
            let salt = SaltString::generate(&mut OsRng).to_string();

            let mut encrypt_key = [0u8; 32]; // Can be any desired size
            Argon2::default()
                .hash_password_into(&password.into_bytes(), salt.as_bytes(), &mut encrypt_key)
                .map_err(|e| anyhow!("Could not hash password: {e}"))?;

            (encrypt_key, salt)
        };

        let stream_server_task = stream_server(
            server_addr,
            encrypt_key,
            quality,
            config.clone(),
            data_send_rx,
            client_status_rx,
            cancellation_token.child_token(),
        );

        let payload_joinh = tokio::spawn(payload_server(
            server_addr,
            salt,
            encrypt_key,
            config.clone(),
            client_status_tx,
            cancellation_token.child_token(),
        ));
        let upnp_joinh = tokio::spawn(upnp_connector(
            server_addr.port(),
            cancellation_token.child_token(),
        ));
        let close_joinh = tokio::spawn(close_task);

        drop(cancellation_token);

        let (payload_res, upnp_res, close_res, playback_capturer_res, stream_res) = tokio::join!(
            payload_joinh,
            upnp_joinh,
            close_joinh,
            playback_capturer_task,
            stream_server_task
        );
        if let Err(e) = stream_res {
            error!("{e:?}");
        }
        if let Err(e) = playback_capturer_res {
            error!("{e:?}");
        }
        if let Err(e) = payload_res {
            error!("{e:?}");
        }
        if let Err(e) = upnp_res {
            error!("{e:?}");
        }
        if let Err(e) = close_res {
            error!("{e:?}");
        }

        debug!("Server closed successfully");

        Ok(())
    }
}
