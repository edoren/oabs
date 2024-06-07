use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::mpsc,
};

use anyhow::{anyhow, Result};
use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device,
};
use dialoguer::{theme::ColorfulTheme, FuzzySelect, Input};
use log::{debug, error, info, trace};
use oabs_lib::{OABSMessage, SupportedStreamConfigDeserialize};
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
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};
use vorbis_rs::VorbisDecoder;

#[derive(Parser, Debug)]
#[command(version, about = "Client", long_about = None)]
struct Opt {
    /// The server address to connect to
    #[arg(short, long, value_name = "SERVER")]
    server: Option<String>,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "DELAY_MS")]
    latency: Option<f32>,
}

enum StreamStatus {
    Starting,
    Streaming,
    // Stopped,
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

async fn main_ex() -> Result<()> {
    let opt = Opt::parse();

    let app_config_dir = dirs::config_dir()
        .ok_or(anyhow!("Could not get config dir"))?
        .join("oabs");

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
        .filename_prefix("oabs")
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

    let host = cpal::default_host();

    let input_theme = ColorfulTheme::default();

    let server_address = if let Some(server) = opt.server {
        server
    } else {
        Input::with_theme(&input_theme)
            .with_prompt("Enter the server")
            .default(String::from("localhost:48182"))
            .validate_with(|input: &String| -> Result<(), &str> {
                let value = input.to_socket_addrs();
                if let Ok(mut iter) = value {
                    while let Some(val) = iter.next() {
                        if val.is_ipv4() {
                            return Ok(());
                        }
                    }
                }
                Err("This is not a valid address")
            })
            .interact()?
    };

    let latency = if let Some(latency) = opt.latency {
        latency
    } else {
        Input::with_theme(&input_theme)
            .with_prompt("Enter the latency")
            .default(150.0)
            .interact()?
    };

    #[cfg(not(target_os = "android"))]
    let device = {
        let output_devices: Vec<Device> = host
            .output_devices()?
            .filter(|x| x.name().is_ok())
            .collect();

        let devices_names = output_devices
            .iter()
            .map(|x| x.name().unwrap_or(String::new()))
            .collect::<Vec<String>>();

        let selection = FuzzySelect::with_theme(&input_theme)
            .with_prompt("Select the output device")
            .default(0)
            .items(&devices_names)
            .interact()?;

        output_devices[selection].clone()
    };

    #[cfg(target_os = "android")]
    let device = host
        .default_output_device()
        .ok_or(anyhow!("Could not find default output device"))?;

    println!();

    let remote_addr = server_address
        .to_socket_addrs()?
        .filter(|s| s.is_ipv4())
        .next()
        .ok_or(anyhow!("Could not resolve address"))?;

    debug!("Connecting to server {}", server_address);
    let mut stream = TcpStream::connect(remote_addr).await?;

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
    let latency_frames = (latency / 1_000.0) * config.sample_rate().0 as f32;
    let latency_samples = latency_frames as usize * config.channels() as usize;

    // The buffer to share samples
    let ring = HeapRb::<f32>::new(latency_samples * 2);
    let (mut producer, mut consumer) = ring.split();

    // Fill the samples with 0.0 equal to the length of the delay.
    for _ in 0..latency_samples {
        // The ring buffer has twice as much space as necessary to add latency here,
        // so this should never fail
        producer
            .try_push(0.0)
            .map_err(|e| anyhow!("Could not fill buffer: {e}"))?;
    }

    let (close_tx, close_rx) = watch::channel(false);
    let (player_close_tx, player_close_rx) = mpsc::channel();

    let err_fn = move |err| {
        error!("an error occurred on stream: {}", err);
    };

    let mut close_rx_stream_receiver_task = close_rx.clone();
    let stream_receiver_task = async move {
        let local_addr: SocketAddr = "0.0.0.0:12312".parse()?;
        let cli = UdpSocket::bind(local_addr).await?;
        debug!("Using local address: {}", cli.local_addr()?);
        cli.connect(remote_addr).await?;
        let mut buf = [0; 5120];

        let mut stream_status = StreamStatus::Starting;
        loop {
            match stream_status {
                StreamStatus::Starting => {
                    cli.send(format!("START {client_id}").as_bytes()).await?;
                    stream_status = StreamStatus::Streaming;
                }
                StreamStatus::Streaming => {} // StreamStatus::Stopped => {}
            };

            let recv_len = tokio::select! {
                result = cli.recv(&mut buf) => {
                    match result {
                        Ok(len) => len,
                        Err(_e) => continue,
                    }
                },
                result = close_rx_stream_receiver_task.changed() => {
                    if result.is_ok() && *close_rx_stream_receiver_task.borrow_and_update() {
                        break;
                    }
                    continue;
                }
            };

            match stream_status {
                StreamStatus::Starting => {
                    // let response = unsafe { std::str::from_utf8_unchecked(&buf[..recv_len]) };
                }
                StreamStatus::Streaming => {
                    let mut decoder: VorbisDecoder<&[u8]> = VorbisDecoder::new(&buf[..recv_len])?;
                    while let Some(decoded_block) = decoder.decode_audio_block()? {
                        let channel1 = decoded_block.samples()[0];
                        let data_count = channel1.len();
                        let written_count = producer.push_slice(channel1);
                        if written_count < data_count {
                            trace!("output stream fell behind: try increasing latency");
                        }
                    }
                } // StreamStatus::Stopped => {}
            };
        }

        Ok::<(), anyhow::Error>(())
    };

    let config_clone = config.clone();
    let device_clone = device.clone();
    let player_task = move || -> Result<()> {
        debug!("Creating playback thread");

        let stream = device_clone.build_output_stream(
            &config_clone.into(),
            move |data: &mut [f32], _: &_| {
                let received_count = data.len();
                let read_count = consumer.pop_slice(data);
                if read_count < received_count {
                    trace!("input stream fell behind: try increasing latency");
                }
            },
            err_fn,
            None,
        )?;

        info!("Starting playback");
        stream.play()?;
        loop {
            if player_close_rx.recv()? {
                debug!("Playback thread");
                break;
            }
        }
        info!("Stopping playback");
        stream.pause()?;

        drop(stream);

        debug!("Stopping playback thread");
        Ok(())
    };

    let server_receiver = async move {
        loop {
            if let Ok(data) = recv_data(&mut stream).await {
                let value = unsafe { std::str::from_utf8_unchecked(&data) };
                if value == "PING" {
                    trace!("YAAAAAS PING");
                    loop {
                        if let Ok(_) = send_data(&mut stream, "PONG".as_bytes()).await {
                            break;
                        }
                    }
                }
                if value == "CLOSE" {
                    trace!("Server requested close");
                    break;
                }
            }
        }
    };

    let stream_receiver_joinh = tokio::spawn(stream_receiver_task);
    let player_joinh = std::thread::spawn(player_task);

    #[cfg(not(target_os = "windows"))]
    {
        tokio::select! {
            _ = signal::ctrl_c() => { },
            _ = server_receiver => { }
        };
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
            _ = server_receiver => { }
        };
    }

    close_tx.send(true)?;
    if let Err(e) = stream_receiver_joinh.await? {
        error!("{e:?}");
    };

    player_close_tx.send(true)?;
    player_joinh
        .join()
        .map_err(|e| anyhow!("Could not join the playback thread: {e:?}"))??;

    debug!("Client closed successfully");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let result = main_ex().await;
    if let Err(err) = result {
        error!("{err}");
        return Err(err);
    }
    Ok(())
}
