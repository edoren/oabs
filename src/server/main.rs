use std::{
    cmp::Eq,
    collections::HashMap,
    io::{self, ErrorKind},
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::ExitCode,
    slice,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{anyhow, Result};
use aotuv_lancer_vorbis_sys::*;
use clap::{ArgAction, Parser, ValueEnum};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, SampleRate, SupportedStreamConfig,
};
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use local_ip_address::local_ip;
use log::{debug, error, info, trace};
use oabs_lib::{
    history::HistoryFile,
    serializers::{OABSMessage, SupportedStreamConfigSerialize},
};
use ogg_next_sys::*;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    signal,
    sync::{mpsc, watch},
    task::{yield_now, JoinSet},
    time::{sleep, sleep_until, timeout_at, Instant},
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};

mod upnp;
use upnp::{AddPortConfig, DeletePortConfig, PortMappingProtocol, UPnP};

#[derive(
    PartialEq,
    Eq,
    Hash,
    ValueEnum,
    Debug,
    Clone,
    EnumIter,
    EnumString,
    IntoStaticStr,
    Serialize,
    Deserialize,
)]
#[strum(serialize_all = "title_case")]
// #[serde(rename_all = "snake_case")]
enum StreamQuality {
    LOWEST,
    LOW,
    MEDIUM,
    HIGH,
    HIGHEST,
}

enum ClientStatus {
    Connected { id: String },
    Disconnected { id: String },
}

const DEFAULT_PORT: u16 = 48182;

#[derive(Parser, Debug)]
#[command(version, about = "Server", long_about = None)]
struct Opt {
    /// The port name to use
    #[arg(short, long, value_name = "PORT", default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "QUALITY", value_enum)]
    quality: Option<StreamQuality>,

    /// The maximum amount of supported clients
    #[arg(short, long, value_name = "MAX_CLIENTS", default_value_t = 5)]
    max_connections: u16,

    /// The device to capture audio from
    #[arg(short, long, value_name = "DEVICE")]
    device: Option<String>,

    /// The device to capture audio from
    #[arg(short, long, action = ArgAction::SetTrue)]
    list_devices: bool,
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
fn get_id() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

async fn client_handler(
    mut stream: TcpStream,
    config: SupportedStreamConfig,
    mut close_rx: watch::Receiver<bool>,
    client_status_tx: tokio::sync::mpsc::Sender<ClientStatus>,
) -> Result<()> {
    let client_id = get_id().to_string();

    let client_id_msg = serde_json::to_vec(&OABSMessage::ClientId {
        id: client_id.clone(),
    })?;
    send_data(&mut stream, &client_id_msg).await?;

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
                                info!("Connection aborted from client {client_id}");
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
                            info!("Client with id {client_id:?} forgot to PONG");
                            break;
                        }
                    };

                    match read_result {
                        Err(e) => {
                            if e.kind() == io::ErrorKind::UnexpectedEof {
                                info!("Connection aborted from client with id {client_id:?}");
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
        _ = close_rx.changed() => {}
    };

    let _ = send_data(&mut stream, "CLOSE".as_bytes()).await;

    let _ = client_status_tx
        .send(ClientStatus::Disconnected {
            id: client_id.clone(),
        })
        .await;

    info!("Client closed with id {client_id:?}");

    Ok(())
}

async fn payload_server(
    server_addr: SocketAddr,
    config: SupportedStreamConfig,
    mut close_rx: watch::Receiver<bool>,
    client_status_tx: mpsc::Sender<ClientStatus>,
) -> Result<()> {
    debug!("Starting payload server");

    let listener = TcpListener::bind(server_addr).await?;
    info!("Payload server listening on: {}", listener.local_addr()?);
    let mut streams = JoinSet::new();
    loop {
        let stream = tokio::select! {
            result = listener.accept() => {
                result?.0
            },
            result = close_rx.changed() => {
                if result.is_ok() && *close_rx.borrow_and_update() {
                    break;
                }
                continue;
            }
        };
        streams.spawn(client_handler(
            stream,
            config.clone(),
            close_rx.clone(),
            client_status_tx.clone(),
        ));
    }

    while let Some(_res) = streams.join_next().await {}

    debug!("Stopping payload server");
    Ok(())
}

async fn stream_server(
    server_addr: SocketAddr,
    quality: f32,
    config: SupportedStreamConfig,
    mut data_send_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    mut close_rx: watch::Receiver<bool>,
    mut client_status_rx: mpsc::Receiver<ClientStatus>,
) -> Result<()> {
    debug!("Starting stream server");

    let sock = UdpSocket::bind(server_addr).await?;
    info!("Stream server listening on: {}", sock.local_addr()?);

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
            panic!("YAY");
        }

        vorbis_comment_init(vc.as_mut_ptr());
        // vorbis_comment_add_tag(vc.as_mut_ptr(), "ENCODER","encoder_example.c");

        vorbis_analysis_init(vd.as_mut_ptr(), vi.as_mut_ptr());
        vorbis_block_init(vd.as_mut_ptr(), vb.as_mut_ptr());

        serial_num = thread_rng().gen();
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

    debug!("Header Data: {}", header_data.len());

    let mut buffer = [0; 256];
    let mut encoded_data: Vec<u8> = Vec::new();
    let mut registered_clients: HashMap<String, Option<SocketAddr>> = HashMap::new();

    let mut client_detected = false;

    'stream_loop: loop {
        if let Ok(changed) = close_rx.has_changed() {
            if changed && *close_rx.borrow_and_update() {
                break;
            }
        }

        if let Ok(client_status) = client_status_rx.try_recv() {
            match client_status {
                ClientStatus::Connected { id } => {
                    registered_clients.insert(id, None);
                }
                ClientStatus::Disconnected { id } => {
                    registered_clients.remove(&id);
                    info!("Removing client with id {id:?}");
                    client_detected = !registered_clients.is_empty();
                }
            }
        }

        match sock.try_recv_from(&mut buffer) {
            Ok((len, source)) => {
                let msg = unsafe { std::str::from_utf8_unchecked(&buffer[..len]) };
                if msg == "START" {
                    let _ = sock.send_to(&header_data, source).await?;
                }
                if msg.starts_with("HEADER_OK") {
                    if let Some(id) = msg.split(" ").nth(1) {
                        client_detected = true;
                        registered_clients.insert(id.into(), Some(source));
                        info!("Added client {source} with id {id:?}");
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

        let mut audio_block: Vec<Vec<f32>> = Vec::new();
        for _ in 0..audio_channels - 1 {
            audio_block.push(raw_samples.clone());
        }
        audio_block.push(raw_samples);

        {
            let sample_count = audio_block[0].len();
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
                    panic!("PUTA MADRE");
                }

                unsafe {
                    channel_samples
                        .as_ptr()
                        .copy_to_nonoverlapping(*channel_encode_buffer, sample_count);
                }
            }

            let res = unsafe { vorbis_analysis_wrote(vd.as_mut_ptr(), sample_count.try_into()?) };
            if res != 0 {
                panic!("FUCK ERROR WRITE");
            }

            // SAFETY: we assume the functions inside this unsafe block follow their
            // documented contract
            unsafe {
                while vorbis_analysis_blockout(vd.as_mut_ptr(), vb.as_mut_ptr()) == 1 {
                    let res = vorbis_analysis(vb.as_mut_ptr(), std::ptr::null_mut());
                    if res != 0 {
                        panic!("FUCK ERROR WRITE");
                    }
                    let res = vorbis_bitrate_addblock(vb.as_mut_ptr());
                    if res != 0 {
                        panic!("FUCK ERROR WRITE");
                    }

                    while vorbis_bitrate_flushpacket(vd.as_mut_ptr(), op.as_mut_ptr()) == 1 {
                        let res = ogg_stream_packetin(os.as_mut_ptr(), op.as_mut_ptr());
                        if res != 0 {
                            panic!("FUCK ERROR WRITE");
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

        for source in registered_clients.values() {
            if let Some(source) = source {
                let _len = sock.send_to(&encoded_data, source).await?;
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

async fn upnp_connector(port: u16, mut close_rx: watch::Receiver<bool>) -> Result<()> {
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
            info!("Running on external ip {ip}")
        } else {
            error!("Could not get external ip");
        }
    }

    let mut retry_time: u64 = (add_mapping.lease_duration / 2).into();
    loop {
        tokio::select! {
            _ = close_rx.changed() => {
                if *close_rx.borrow_and_update() {
                    break;
                }
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

async fn main_wrapper() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return Err(anyhow!("This program is not supported on Android"));
    }

    let opt = Opt::parse();

    let app_config_dir = dirs::config_dir()
        .ok_or(anyhow!("Could not get config dir"))?
        .join("oabs_server");

    // Logging

    let logs_dir = app_config_dir.join("logs");
    let default_filter = |filter: LevelFilter| {
        EnvFilter::builder()
            .with_default_directive(filter.into())
            .from_env_lossy()
    };

    let file_appender = RollingFileAppender::builder()
        .max_log_files(7)
        .rotation(Rotation::DAILY)
        .filename_prefix("oabs_server")
        .filename_suffix("log")
        .build(logs_dir.clone())?;
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(default_filter(LevelFilter::DEBUG))
        .boxed();

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_filter(default_filter(LevelFilter::DEBUG))
        .boxed();

    let mut layers = Vec::new();
    layers.push(file_layer);
    layers.push(stdout_layer);
    tracing_subscriber::registry().with(layers).init();

    // App

    let server_addr = SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), opt.port);

    let host = cpal::default_host();
    let mut devices: Vec<Device> = host.devices()?.filter(|x| x.name().is_ok()).collect();
    devices.sort_by(|a, b| {
        a.name()
            .unwrap_or_default()
            .to_lowercase()
            .cmp(&b.name().unwrap_or_default().to_lowercase())
    });
    let mut devices_names = devices
        .iter()
        .map(|x| x.name().unwrap_or_default())
        .collect::<Vec<String>>();

    if opt.list_devices {
        devices_names.iter().for_each(|dn| println!("{dn}"));
        return Ok(());
    }

    eprintln!(" ██████╗  █████╗ ██████╗ ███████╗");
    eprintln!("██╔═══██╗██╔══██╗██╔══██╗██╔════╝");
    eprintln!("██║   ██║███████║██████╔╝███████╗");
    eprintln!("██║   ██║██╔══██║██╔══██╗╚════██║");
    eprintln!("╚██████╔╝██║  ██║██████╔╝███████║");
    eprintln!(" ╚═════╝ ╚═╝  ╚═╝╚═════╝ ╚══════╝");
    eprintln!("[ Open Audio Broadcast Software ]");
    eprintln!();

    // CLI UI

    let mut history_file = HistoryFile::new(&app_config_dir.join("history.json"), Some(10), true)?;

    let mut additional_space = false;

    let device_history = history_file.get("device");
    let device_name = if let Some(device_name) = opt.device {
        let device_found = devices_names.iter().find(|&d| d == &device_name);
        if let Some(device_name) = device_found {
            device_name.clone()
        } else {
            return Err(anyhow!("Could not find a device with name {device_name}"));
        }
    } else {
        #[cfg(not(target_os = "android"))]
        {
            if let Some(last_device) = device_history.get_last() {
                if let Some(index) = devices_names.iter().position(|d| d == &last_device) {
                    devices.swap(0, index);
                    devices_names.swap(0, index);
                }
            }

            additional_space = true;
            let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
                .with_prompt("Select the device to capture")
                .default(0)
                .items(&devices_names)
                .interact()?;

            devices_names[selection].clone()
        }

        #[cfg(target_os = "android")]
        host.default_input_device()
            .ok_or(anyhow!("Could not find default input device"))?
            .name()?
    };
    device_history.add(device_name.clone());

    let device = devices
        .iter()
        .find(|d| d.name().is_ok_and(|n| n == device_name))
        .take()
        .ok_or(anyhow!("Could not find requested device"))?;

    let quality_value = {
        let qualities: Vec<StreamQuality> = StreamQuality::iter().collect();

        let quality_history = history_file.get("quality");
        let quality = if let Some(quality) = opt.quality {
            quality
        } else {
            #[cfg(not(target_os = "android"))]
            {
                let default: usize = if let Some(Ok(last_quality)) = quality_history
                    .get_last()
                    .map(|q| q.to_owned().parse::<StreamQuality>())
                {
                    qualities
                        .iter()
                        .position(|d| d == &last_quality)
                        .unwrap_or(0)
                } else {
                    qualities.len() / 2
                };

                let qualities_names: Vec<&str> = qualities.iter().map(|e| e.into()).collect();
                let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
                    .with_prompt("Select the stream quality")
                    .default(default)
                    .items(&qualities_names)
                    .interact()?;
                additional_space = true;
                qualities[selection].clone()
            }
            #[cfg(target_os = "android")]
            {
                qualities[qualities.len() / 2].clone()
            }
        };
        quality_history.add(Into::<&str>::into(&quality).to_string());

        let quality_val_min = -0.1;
        let quality_val_max = 1.0;

        quality_val_min
            + (quality_val_max - quality_val_min) / (qualities.len() - 1) as f32
                * ((quality as u32) as f32)
    };

    drop(history_file);
    if additional_space {
        eprintln!();
    }

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

    let (close_tx, close_rx) = watch::channel(false);
    let (data_send_tx, data_send_rx) = mpsc::unbounded_channel::<Vec<_>>();
    let (client_status_tx, client_status_rx) = mpsc::channel::<ClientStatus>(100);

    debug!("Sample Format {:?}", config.sample_format());

    let config_clone = config.clone();
    let mut stream_close_rx = close_rx.clone();
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

        info!("Begin recording");
        stream.play()?;
        loop {
            if let Ok(_) = stream_close_rx.changed().await {
                if *stream_close_rx.borrow_and_update() {
                    break;
                }
            }
        }
        stream.pause()?;
        info!("Stopped recording");

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
        close_tx.send(true)?;
        close_tx.closed().await;
        return Ok::<(), anyhow::Error>(());
    };

    let stream_server_task = stream_server(
        server_addr,
        quality_value,
        config.clone(),
        data_send_rx,
        close_rx.clone(),
        client_status_rx,
    );

    let payload_joinh = tokio::spawn(payload_server(
        server_addr,
        config.clone(),
        close_rx.clone(),
        client_status_tx,
    ));
    let upnp_joinh = tokio::spawn(upnp_connector(opt.port, close_rx.clone()));
    let close_joinh = tokio::spawn(close_task);

    drop(close_rx);

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

#[tokio::main]
async fn main() -> ExitCode {
    let result = main_wrapper().await;
    if let Err(err) = &result {
        error!("{err}");
        return ExitCode::FAILURE;
    }
    return ExitCode::SUCCESS;
}
