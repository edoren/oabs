use std::{net::SocketAddr, sync::mpsc};

use anyhow::{anyhow, Result};
use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat,
};
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use log::{debug, error};
use oabs_lib::SupportedStreamConfigDeserialize;
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use tokio::{
    io::AsyncReadExt,
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
    #[arg(short, long, value_name = "SERVER", default_value_t = String::from("127.0.0.1:48182"))]
    server: String,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "DELAY_MS", default_value_t = 150.0)]
    latency: f32,
}

#[tokio::main]
async fn main() -> Result<()> {
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

    let host = cpal::default_host();

    let output_devices: Vec<Device> = host
        .output_devices()?
        .filter(|x| x.name().is_ok())
        .collect();

    let devices_names = output_devices
        .iter()
        .map(|x| x.name().unwrap_or(String::new()))
        .collect::<Vec<String>>();

    let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select the output device")
        .default(0)
        .items(&devices_names)
        .interact()?;

    let device = &output_devices[selection];

    let remote_addr: SocketAddr = opt.server.parse()?;

    debug!("Connecting to server {}", opt.server);
    let mut stream = TcpStream::connect(remote_addr).await?;

    // Wait for the socket to be readable
    stream.readable().await?;

    // Creating the buffer **after** the `await` prevents it from
    // being stored in the async task.
    let mut buf = [0; 256];

    // Try to read data, this may still fail with `WouldBlock`
    // if the readiness event is a false positive.
    let config = match stream.try_read(&mut buf) {
        Ok(n) => {
            let payload = std::str::from_utf8(&buf[0..n])?;
            serde_json::from_str(payload).map(|SupportedStreamConfigDeserialize(dur)| dur)?
        }
        // Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
        //     continue;
        // }
        Err(e) => {
            return Err(e.into());
        }
    };

    // Create a delay in case the input and output devices aren't synced.
    let latency_frames = (opt.latency / 1_000.0) * config.sample_rate().0 as f32;
    let latency_samples = latency_frames as usize * config.channels() as usize;

    let buffer_size = latency_samples * 2 * 4;
    debug!("Buffer Size: {buffer_size}");

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

    // let selected_name = "Voicemeeter Out B3 (VB-Audio Voicemeeter VAIO)";

    // let device = input_devices
    //     .into_iter()
    //     .find(|p| p.name().is_ok_and(|n| n == selected_name))
    //     .ok_or(anyhow!(
    //         "Selected device {selected_name:?} could not be found"
    //     ))?;

    // let config = device
    //     .default_input_config()
    //     .map_err(|e| anyhow!("Failed to get default input config: {e}"))?;

    // // A flag to indicate that recording is in progress.
    // debug!("Begin recording...");

    // // Run the input stream on a separate thread.

    let (close_tx, close_rx) = watch::channel(false);
    let (player_close_tx, player_close_rx) = mpsc::channel();
    // let (data_recv_tx, data_recv_rx) = mpsc::channel::<SampleData>();

    // debug!("Sample Format {:?}", config.sample_format());

    let err_fn = move |err| {
        error!("an error occurred on stream: {}", err);
    };

    let mut close_rx_stream_receiver_task = close_rx.clone();
    let stream_receiver_task = async move {
        let local_addr: SocketAddr = if remote_addr.is_ipv4() {
            "0.0.0.0:12312".parse()?
        } else {
            "[::]:0".parse()?
        };
        let cli = UdpSocket::bind(local_addr).await?;
        cli.connect(remote_addr).await?;
        let mut buf = [0; 5120];

        cli.send("START".as_bytes()).await?;

        loop {
            let recv_result;
            tokio::select! {
                result = cli.recv(&mut buf) => {
                    recv_result = result;
                },
                result = close_rx_stream_receiver_task.changed() => {
                    if result.is_ok() && *close_rx_stream_receiver_task.borrow_and_update() {
                        break;
                    }
                    continue;
                }
            };

            match recv_result {
                Ok(len) => {
                    let mut decoder: VorbisDecoder<&[u8]> = VorbisDecoder::new(&buf[..len])?;
                    while let Some(decoded_block) = decoder.decode_audio_block()? {
                        let channel1 = decoded_block.samples()[0];
                        let data_count = channel1.len();
                        let written_count = producer.push_slice(channel1);
                        if written_count < data_count {
                            error!("output stream fell behind: try increasing latency");
                        }
                    }
                }
                Err(e) => return Err::<(), anyhow::Error>(anyhow!("{e}")),
            }
        }
        Ok(())
    };

    let config_clone = config.clone();
    let device_clone = device.clone();
    let player_task = move || -> Result<()> {
        let stream = match config_clone.sample_format() {
            SampleFormat::F32 => device_clone.build_output_stream(
                &config_clone.into(),
                move |data: &mut [f32], _: &_| {
                    let received_count = data.len();
                    let read_count = consumer.pop_slice(data);
                    if read_count < received_count {
                        error!("input stream fell behind: try increasing latency");
                    }
                },
                err_fn,
                None,
            )?,
            sample_format => return Err(anyhow!("Unsupported sample format '{sample_format}'")),
        };

        stream.play()?;
        loop {
            if player_close_rx.recv()? {
                debug!("Closing stream thread");
                break;
            }
        }
        stream.pause()?;

        drop(stream);

        Ok(())
    };

    debug!("{config:?}");

    let stream_receiver_joinh = tokio::spawn(stream_receiver_task);
    let player_joinh = std::thread::spawn(player_task);

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
            result = stream.read_i32() => {
                if let Ok(result) = result {
                    if result == -1 {
                        debug!("Server closed");
                    }
                }
             }
        };
    }

    close_tx.send(true)?;
    if let Err(e) = stream_receiver_joinh.await? {
        error!("{e:?}");
    };

    player_close_tx.send(true)?;
    player_joinh
        .join()
        .map_err(|e| anyhow!("Could not join stream thread: {e:?}"))??;

    debug!("Client closed successfully");

    Ok(())
}
