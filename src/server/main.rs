use std::{
    cmp::Eq,
    collections::HashMap,
    io,
    mem::MaybeUninit,
    net::{Ipv4Addr, SocketAddr},
    slice,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{anyhow, Result};
use aotuv_lancer_vorbis_sys::*;
use clap::{Parser, ValueEnum};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, SampleRate, SupportedStreamConfig,
};
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use log::{debug, error, info, trace};
use oabs_lib::{OABSMessage, SupportedStreamConfigSerialize};
use ogg_next_sys::*;
use rand::{thread_rng, Rng};
use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    signal,
    sync::{
        mpsc::{self},
        watch,
    },
    task::{yield_now, JoinSet},
    time::{sleep, timeout_at, Instant},
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};

#[derive(PartialEq, Eq, Hash, ValueEnum, Debug, Clone, EnumIter, EnumString, IntoStaticStr)]
#[strum(serialize_all = "title_case")]
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

#[derive(Parser, Debug)]
#[command(version, about = "Server", long_about = None)]
struct Opt {
    /// The port name to use
    #[arg(short, long, value_name = "PORT", default_value_t = 48182)]
    port: u16,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "QUALITY", value_enum)]
    quality: Option<StreamQuality>,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "MAX_CLIENTS", default_value_t = 5)]
    max_connections: u16,
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
        let mut pong_instant = Instant::now() + Duration::from_secs(10);
        loop {
            match send_data(&mut stream, "PING".as_bytes()).await {
                Ok(_) => {
                    trace!("Sending PING to client with id {client_id:?}")
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

            let read_result = match timeout_at(pong_instant, recv_data(&mut stream)).await {
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
                            sleep(Duration::from_secs(5)).await;
                            pong_instant = Instant::now() + Duration::from_secs(10);
                        }
                    }
                }
            }
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

    let mut header_data: Vec<u8> = Vec::new();
    unsafe {
        vorbis_info_init(vi.as_mut_ptr());

        let ret = vorbis_encode_init_vbr(
            vi.as_mut_ptr(),
            config.channels().into(),
            (config.sample_rate().0 as i32).into(),
            quality,
        );
        if ret != 0 {
            panic!("YAY");
        }

        vorbis_comment_init(vc.as_mut_ptr());
        // vorbis_comment_add_tag(vc.as_mut_ptr(), "ENCODER","encoder_example.c");

        vorbis_analysis_init(vd.as_mut_ptr(), vi.as_mut_ptr());
        vorbis_block_init(vd.as_mut_ptr(), vb.as_mut_ptr());

        let serial_num = thread_rng().gen();
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

    let mut buffer = [0; 8];
    let mut encoded_data: Vec<u8> = Vec::new();
    let mut registered_clients: HashMap<String, Option<SocketAddr>> = HashMap::new();
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
                }
            }
        }

        if let Ok((len, source)) = sock.try_recv_from(&mut buffer) {
            let msg = unsafe { std::str::from_utf8_unchecked(&buffer[..len]) };
            if msg.starts_with("START") {
                let id = msg.split(" ").nth(1).unwrap();
                registered_clients.insert(id.into(), Some(source));
                info!("Added client {source} with id {id:?}");
                let _len = sock.send_to(&header_data, source).await?;
            }
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

        if registered_clients.is_empty() {
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

            let res = unsafe { vorbis_analysis_wrote(vd.as_mut_ptr(), sample_count as i32) };
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

#[tokio::main]
async fn main() -> Result<()> {
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

    println!(" ██████╗  █████╗ ██████╗ ███████╗");
    println!("██╔═══██╗██╔══██╗██╔══██╗██╔════╝");
    println!("██║   ██║███████║██████╔╝███████╗");
    println!("██║   ██║██╔══██║██╔══██╗╚════██║");
    println!("╚██████╔╝██║  ██║██████╔╝███████║");
    println!(" ╚═════╝ ╚═╝  ╚═╝╚═════╝ ╚══════╝");
    println!("[ Open Audio Broadcast Software ]");
    println!();

    let server_addr = SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), opt.port);
    let host = cpal::default_host();

    let device = {
        #[cfg(not(target_os = "android"))]
        {
            let input_devices: Vec<Device> =
                host.input_devices()?.filter(|x| x.name().is_ok()).collect();

            let devices_names = input_devices
                .iter()
                .map(|x| x.name().unwrap_or(String::new()))
                .collect::<Vec<String>>();

            let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
                .with_prompt("Select the device to capture")
                .default(0)
                .items(&devices_names)
                .interact()?;

            input_devices[selection].clone()
        }

        #[cfg(target_os = "android")]
        host.default_input_device()
            .ok_or(anyhow!("Could not find default input device"))?
    };

    let quality_value = {
        let quialities: Vec<StreamQuality> = StreamQuality::iter().collect();

        let quality = if let Some(quality) = opt.quality {
            quality
        } else {
            #[cfg(not(target_os = "android"))]
            {
                let quialities_names: Vec<&str> = quialities.iter().map(|e| e.into()).collect();
                let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
                    .with_prompt("Select the stream quality")
                    .default(quialities.len() / 2)
                    .items(&quialities_names)
                    .interact()?;
                quialities[selection].clone()
            }
            #[cfg(target_os = "android")]
            {
                quialities[quialities.len() / 2].clone()
            }
        };

        let quality_val_min = -0.1;
        let quality_val_max = 1.0;

        quality_val_min
            + (quality_val_max - quality_val_min) / (quialities.len() - 1) as f32
                * ((quality as u32) as f32)
    };

    #[cfg(not(target_os = "android"))]
    println!();

    let configs = device
        .supported_input_configs()?
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

    let err_fn = move |err| {
        error!("an error occurred on stream: {}", err);
    };

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
                    println!("{e:?}");
                }
            },
            err_fn,
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

    let payload_joinh = tokio::spawn(payload_server(
        server_addr,
        config.clone(),
        close_rx.clone(),
        client_status_tx,
    ));
    let close_joinh = tokio::spawn(close_task);

    let local_sdasd = tokio::task::LocalSet::new();
    let stream_joinh = local_sdasd.spawn_local(stream_server(
        server_addr,
        quality_value,
        config.clone(),
        data_send_rx,
        close_rx,
        client_status_rx,
    ));
    let playback_capturer_joinh = local_sdasd.spawn_local(playback_capturer_task);

    local_sdasd.await;

    let (payload_res, close_res) = tokio::join!(payload_joinh, close_joinh);
    if let Err(e) = stream_joinh.await {
        error!("{e:?}");
    }
    if let Err(e) = playback_capturer_joinh.await {
        error!("{e:?}");
    }
    if let Err(e) = payload_res {
        error!("{e:?}");
    }
    if let Err(e) = close_res {
        error!("{e:?}");
    }

    debug!("Server closed successfully");

    Ok(())
}
