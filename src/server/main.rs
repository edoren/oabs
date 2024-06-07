use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    num::{NonZeroU32, NonZeroU8},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, SampleRate, SupportedStreamConfig,
};
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use log::{debug, error, trace};
use oabs_lib::{OABSMessage, SupportedStreamConfigSerialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    signal,
    sync::watch,
    task::{yield_now, JoinSet},
    time::{timeout_at, Instant},
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};
use vorbis_rs::VorbisEncoderBuilder;

enum ClientStatus {
    Connected { id: String },
    Disconnected { id: String },
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

#[derive(Parser, Debug)]
#[command(version, about = "Server", long_about = None)]
struct Opt {
    /// The port name to use
    #[arg(short, long, value_name = "PORT", default_value_t = 48182)]
    port: u16,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "MAX_CLIENTS", default_value_t = 5)]
    max_connections: u16,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "VERBOSE", default_value_t = false)]
    verbose: bool,
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
    let address = stream.peer_addr()?;

    debug!("Sending to {:?} payload {:?}", address, client_id);
    let client_id_msg = serde_json::to_vec(&OABSMessage::ClientId {
        id: client_id.clone(),
    })?;
    send_data(&mut stream, &client_id_msg).await?;

    debug!("Sending to {:?} payload {:?}", address, config);
    let value = serde_json::to_vec(&SupportedStreamConfigSerialize(&config))?;
    send_data(&mut stream, &value).await?;

    let _ = client_status_tx
        .send(ClientStatus::Connected {
            id: client_id.clone(),
        })
        .await;

    let client_task = async {
        loop {
            let pong_instant = Instant::now() + Duration::from_secs(10);
            match send_data(&mut stream, "PING".as_bytes()).await {
                Ok(_) => {}
                Err(_) => todo!(),
            }

            let read_result = match timeout_at(pong_instant, recv_data(&mut stream)).await {
                Ok(result) => result,
                Err(_) => {
                    break;
                }
            };

            match read_result {
                Err(_) => {
                    break;
                }
                Ok(data) => {
                    if data.len() == 0 {
                        debug!("Empty data frame for client {client_id}. Closing.");
                        break;
                    }
                    let value = unsafe { std::str::from_utf8_unchecked(&data) };
                    if value == "PONG" {
                        trace!("YAAAAAS PONG");
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

    debug!("Client closed with id: {client_id}");

    Ok(())
}

async fn payload_server(
    server_addr: SocketAddr,
    config: SupportedStreamConfig,
    mut close_rx: watch::Receiver<bool>,
    client_status_tx: tokio::sync::mpsc::Sender<ClientStatus>,
) -> Result<()> {
    debug!("Starting Payload Server");

    let listener = TcpListener::bind(server_addr).await?;
    debug!("Payload server listening on: {}", listener.local_addr()?);
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

    debug!("Stopping Payload Server");
    Ok(())
}

async fn stream_server(
    server_addr: SocketAddr,
    data_send_rx: Receiver<Vec<u8>>,
    mut close_rx: watch::Receiver<bool>,
    mut client_status_rx: tokio::sync::mpsc::Receiver<ClientStatus>,
) -> Result<()> {
    debug!("Starting Stream Server");

    let sock = UdpSocket::bind(server_addr).await?;
    debug!("Stream server listening on: {}", sock.local_addr()?);

    let mut buffer = [0; 8];
    let mut registered_clients: HashMap<String, Option<SocketAddr>> = HashMap::new();
    loop {
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
                    debug!("Removing client with id {id}");
                }
            }
        }

        if let Ok((len, source)) = sock.try_recv_from(&mut buffer) {
            let msg = unsafe { std::str::from_utf8_unchecked(&buffer[..len]) };
            if msg.starts_with("START") {
                let id = msg.split(" ").nth(1).unwrap();
                registered_clients.insert(id.into(), Some(source));
                debug!("Added interested {source} with id {id}");
            }
        }

        let samples = loop {
            let result = data_send_rx.recv();
            if let Ok(data) = result {
                break data;
            }
            yield_now().await;
        };

        for source in registered_clients.values() {
            if let Some(source) = source {
                let _len = sock.send_to(&samples, source).await?;
            }
        }

        yield_now().await;
    }

    debug!("Stopping Stream Server");
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

    let server_addr = SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), opt.port);
    let host = cpal::default_host();

    #[cfg(not(target_os = "android"))]
    let device = {
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
    };

    #[cfg(target_os = "android")]
    let device = host
        .default_input_device()
        .ok_or(anyhow!("Could not find default input device"))?;

    let configs = device
        .supported_input_configs()?
        .filter(|c| c.sample_format() == SampleFormat::F32)
        .filter_map(|c| {
            c.try_with_sample_rate(SampleRate(44100))
                .or(c.try_with_sample_rate(c.min_sample_rate()))
        })
        .collect::<Vec<SupportedStreamConfig>>();

    let config = configs
        .first()
        .ok_or(anyhow!("Could not find a valid configuration"))?;

    // Run the input stream on a separate thread.

    let (close_tx, close_rx) = watch::channel(false);
    let (stream_close_tx, stream_close_rx) = mpsc::channel();
    let (data_send_tx, data_send_rx) = mpsc::channel();
    let (client_status_tx, client_status_rx) = tokio::sync::mpsc::channel::<ClientStatus>(100);

    debug!("Sample Format {:?}", config.sample_format());

    let err_fn = move |err| {
        error!("an error occurred on stream: {}", err);
    };

    let config_clone = config.clone();
    let stream_thread = std::thread::spawn(move || -> Result<()> {
        let stream = device.build_input_stream(
            &config_clone.into(),
            move |data: &[f32], _| {
                let mut encoded_data: Vec<u8> = Vec::new();
                let sampling_frequency = NonZeroU32::new(48000).ok_or(anyhow!("WUT")).unwrap();
                let channels = NonZeroU8::new(1).ok_or(anyhow!("WUT")).unwrap();
                {
                    let mut encoder =
                        VorbisEncoderBuilder::new(sampling_frequency, channels, &mut encoded_data)
                            .unwrap()
                            .build()
                            .unwrap();
                    let encode_result = encoder.encode_audio_block([data]);
                    if let Err(err) = encode_result {
                        error!("Encoding Error: {err:?}")
                    }
                }
                let mut samples = Vec::new();
                samples.extend_from_slice(&data);
                let _ = data_send_tx.send(encoded_data);
            },
            err_fn,
            None,
        )?;

        debug!("Begin recording");
        stream.play()?;
        loop {
            if stream_close_rx.recv()? {
                debug!("Closing stream thread");
                break;
            }
        }
        stream.pause()?;
        debug!("Stopped recording");

        drop(stream);

        Ok(())
    });

    let stream_joinh = tokio::spawn(stream_server(
        server_addr,
        data_send_rx,
        close_rx.clone(),
        client_status_rx,
    ));
    let payload_joinh = tokio::spawn(payload_server(
        server_addr,
        config.clone(),
        close_rx.clone(),
        client_status_tx,
    ));

    #[cfg(not(target_os = "windows"))]
    {
        signal::ctrl_c().await?;
    }
    #[cfg(target_os = "windows")]
    {
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
        };
    }

    close_tx.send(true)?;
    if let Err(e) = stream_joinh.await? {
        error!("{e:?}");
    }
    if let Err(e) = payload_joinh.await? {
        error!("{e:?}");
    }

    stream_close_tx.send(true)?;
    stream_thread
        .join()
        .map_err(|e| anyhow!("Could not join stream thread: {e:?}"))??;

    debug!("Server closed successfully");

    Ok(())
}
