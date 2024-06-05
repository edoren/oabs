use std::{
    collections::HashSet,
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU8},
    sync::mpsc::{self, Receiver},
};

use anyhow::{anyhow, Result};
use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, SampleRate, SupportedStreamConfig,
};
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use log::{debug, error};
use oabs_lib::SupportedStreamConfigSerialize;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, UdpSocket},
    signal,
    sync::watch,
    task::yield_now,
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};
use vorbis_rs::VorbisEncoderBuilder;

#[derive(Parser, Debug)]
#[command(version, about = "Server", long_about = None)]
struct Opt {
    /// The port name to use
    #[arg(short, long, value_name = "PORT", default_value_t = 48182)]
    port: u16,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "DELAY_MS", default_value_t = 150.0)]
    latency: f32,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "VERBOSE", default_value_t = false)]
    verbose: bool,
}

async fn payload_server(
    server_addr: SocketAddr,
    config: SupportedStreamConfig,
    mut close_rx: watch::Receiver<bool>,
) -> Result<()> {
    debug!("Starting Payload Server");
    let listener = TcpListener::bind(server_addr).await?;
    debug!("Payload server listening on: {}", listener.local_addr()?);
    let mut sockets = Vec::new();
    loop {
        let mut socket;
        tokio::select! {
            result = listener.accept() => {
                socket = result?.0;
            },
            result = close_rx.changed() => {
                if result.is_ok() && *close_rx.borrow_and_update() {
                    break;
                }
                continue;
            }
        };
        debug!("Sending to {:?} payload {:?}", socket.peer_addr(), config);
        let value = serde_json::to_string(&SupportedStreamConfigSerialize(&config))?;
        socket.write(value.as_bytes()).await?;
        sockets.push(socket);
    }
    for mut socket in sockets {
        let _ = socket.write_i32(-1).await;
    }
    debug!("Stopping Payload Server");
    Ok(())
}

async fn stream_server(
    server_addr: SocketAddr,
    data_send_rx: Receiver<Vec<u8>>,
    mut close_rx: watch::Receiver<bool>,
) -> Result<()> {
    debug!("Starting Stream Server");
    let sock = UdpSocket::bind(server_addr).await?;
    debug!("Stream server listening on: {}", sock.local_addr()?);
    let mut buffer = [0; 8];
    let mut interested = HashSet::new();
    loop {
        if let Ok(changed) = close_rx.has_changed() {
            if changed && *close_rx.borrow_and_update() {
                break;
            }
        }

        let samples = loop {
            let result = data_send_rx.recv();
            if let Ok(data) = result {
                break data;
            } else {
                // sleep(Duration::from_millis(10)).await;
            }
            yield_now().await;
        };

        if let Ok((len, source)) = sock.try_recv_from(&mut buffer) {
            let msg = unsafe { std::str::from_utf8_unchecked(&buffer[..len]) };
            if msg == "START" {
                interested.insert(source);
            }
        }

        for source in &interested {
            let _len = sock.send_to(&samples, source).await?;
            // debug!("{:?} bytes sent to {}", len, source);
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

    let server_addr = SocketAddr::new("0.0.0.0".parse()?, opt.port);

    let host = cpal::default_host();

    let input_devices: Vec<Device> = host.input_devices()?.filter(|x| x.name().is_ok()).collect();

    let devices_names = input_devices
        .iter()
        .map(|x| x.name().unwrap_or(String::new()))
        .collect::<Vec<String>>();

    let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select the device to capture")
        .items(&devices_names)
        .interact()?;

    let selected_name = &devices_names[selection];

    let device = input_devices
        .into_iter()
        .find(|p| p.name().is_ok_and(|n| &n == selected_name))
        .ok_or(anyhow!(
            "Selected device {selected_name:?} could not be found"
        ))?;

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

    let stream_joinh = tokio::spawn(stream_server(server_addr, data_send_rx, close_rx.clone()));
    let payload_joinh = tokio::spawn(payload_server(
        server_addr,
        config.clone(),
        close_rx.clone(),
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
