use std::{
    ffi::c_int,
    mem::MaybeUninit,
    net::{SocketAddr, ToSocketAddrs},
    os::raw::c_void,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use aotuv_lancer_vorbis_sys::*;
use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device,
};
use dialoguer::{theme::ColorfulTheme, FuzzySelect, Input};
use log::{debug, error, info, trace};
use oabs_lib::{OABSMessage, SupportedStreamConfigDeserialize};
use ringbuf::{
    storage::Heap,
    traits::{Consumer, Producer, Split},
    wrap::caching::Caching,
    HeapRb, SharedRb,
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

unsafe fn create_vorbis_file(
    source: *mut ::std::os::raw::c_void,
) -> Result<Box<MaybeUninit<OggVorbis_File>>> {
    let mut vf: Box<MaybeUninit<OggVorbis_File>> = Box::new(MaybeUninit::uninit());
    let callbacks = ov_callbacks {
        read_func: {
            // This read callback should match the stdio fread behavior.
            // See: https://man7.org/linux/man-pages/man3/fread.3p.html
            unsafe extern "C" fn read_func(
                ptr: *mut c_void,
                size: usize,
                count: usize,
                datasource: *mut c_void,
            ) -> usize {
                let source =
                    &mut *(datasource.cast::<Caching<Arc<SharedRb<Heap<u8>>>, false, true>>());
                let buf = std::slice::from_raw_parts_mut(ptr.cast(), size * count);
                let ret = source.pop_slice(buf);
                return ret;
            }
            Some(read_func)
        },
        seek_func: None,
        close_func: {
            unsafe extern "C" fn close_func(datasource: *mut c_void) -> c_int {
                // Drop the Read when it's no longer needed by vorbisfile.
                // This is called by ov_clear
                drop(Box::from_raw(datasource.cast::<Caching<
                    Arc<SharedRb<Heap<u8>>>,
                    false,
                    true,
                >>()));
                0
            }
            Some(close_func)
        },
        tell_func: None,
    };

    let res = ov_open_callbacks(source, vf.as_mut_ptr(), std::ptr::null(), 0, callbacks);
    if res != 0 {
        Err(anyhow!(
            "Could not create OggVorbis instance. Error code: {res}"
        ))
    } else {
        Ok(vf)
    }
}

unsafe fn decode_part(
    vf: &mut OggVorbis_File,
    out: &mut [&mut Caching<Arc<SharedRb<Heap<f32>>>, true, false>],
) -> Result<()> {
    let mut current_bitstream = Box::new(MaybeUninit::uninit());
    let mut sample_buf = Box::new(MaybeUninit::uninit());

    loop {
        let samples_read = ov_read_float(
            vf,
            sample_buf.as_mut_ptr(),
            2048,
            current_bitstream.as_mut_ptr(),
        );

        if samples_read <= 0 {
            return Ok(());
        }

        if samples_read > 0 {
            if current_bitstream.assume_init_read() != 0 {
                return Err(anyhow!("VorbisError::UnsupportedStreamChaining"));
            }

            let buf: *mut *mut f32 = sample_buf.assume_init_read();
            let channels = (*(*vf).vi).channels as usize;
            let samples_read = samples_read as usize;

            let mut values = std::slice::from_raw_parts(buf, channels)
                .iter()
                .map(|channel_samples| std::slice::from_raw_parts(*channel_samples, samples_read));

            for i in 0..out.len() {
                out[0].push_slice(values.nth(i).unwrap_or(&[]));
            }
        }
    }
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

    #[cfg(debug_assertions)]
    let default_stdout_level_filter = LevelFilter::DEBUG;
    #[cfg(not(debug_assertions))]
    let default_stdout_level_filter = LevelFilter::INFO;

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_filter(default_filter(default_stdout_level_filter))
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
    let latency_frames = (latency / 1_000.0) * config.sample_rate().0 as f32;
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
        debug!("Using local address: {}", cli.local_addr()?);
        cli.connect(remote_addr).await?;
        let mut buf = [0; 10240];

        let decode_ring = HeapRb::<u8>::new(5400);
        let (mut decode_producer, decode_consumer) = decode_ring.split();
        let source = Box::into_raw(decode_consumer.into());

        let mut vorbis_file: Option<Box<MaybeUninit<OggVorbis_File>>> = None;

        let mut stream_status = StreamStatus::Starting;
        loop {
            match stream_status {
                StreamStatus::Starting => {
                    cli.send(format!("START {client_id}").as_bytes()).await?;
                }
                StreamStatus::Streaming => {} // StreamStatus::Stopped => {}
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
                        break;
                    }
                    continue;
                }
            };

            match stream_status {
                StreamStatus::Starting => {
                    decode_producer.push_slice(&buf[..recv_len]);

                    match unsafe { create_vorbis_file(source.cast()) } {
                        Ok(vf) => {
                            vorbis_file = Some(vf);
                        }
                        Err(e) => {
                            error!("Error creating Vorbis File: {e:?} - Header Buffer: {recv_len}");
                        }
                    }

                    stream_status = StreamStatus::Streaming;
                }
                StreamStatus::Streaming => {
                    if let Some(vf) = &mut vorbis_file {
                        decode_producer.push_slice(&buf[..recv_len]);
                        if let Err(e) =
                            unsafe { decode_part(vf.assume_init_mut(), &mut [&mut producer]) }
                        {
                            error!("Error Decoding Part: {e:?}");
                        }
                    } else {
                        error!("This should not happen");
                    }
                } // StreamStatus::Stopped => {}
            };
        }

        if let Some(vf) = &mut vorbis_file {
            unsafe { ov_clear(vf.as_mut_ptr()) };
        }

        debug!("Closing stream receiver task");
        Ok::<(), anyhow::Error>(())
    };

    let config_clone = config.clone();
    let device_clone = device.clone();
    let mut player_close_rx = close_rx;
    let player_task = move || {
        debug!("Starting player task");

        let playback_stream = device_clone.build_output_stream(
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

        debug!("Starting playback");
        playback_stream.play()?;
        loop {
            if let Ok(changed) = player_close_rx.has_changed() {
                if changed && *player_close_rx.borrow_and_update() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        debug!("Stopping playback");
        playback_stream.pause()?;

        drop(playback_stream);

        debug!("Stopping player task");
        Ok::<(), anyhow::Error>(())
    };

    let server_receiver = async move {
        debug!("Starting server connection task");

        loop {
            if let Ok(data) = recv_data(&mut stream).await {
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
        }

        debug!("Stopping server connection task");
    };

    #[cfg(not(target_os = "windows"))]
    let close_task = async move {
        tokio::select! {
            _ = signal::ctrl_c() => { },
            _ = server_receiver => { }
        };
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
            _ = server_receiver => { }
        }
        close_tx.send(true)?;
        close_tx.closed().await;
        return Ok::<(), anyhow::Error>(());
    };

    let close_joinh = tokio::spawn(close_task);

    let player_joinh = std::thread::spawn(player_task);

    let (close_res, stream_receiver_res) = tokio::join!(close_joinh, stream_receiver_task);

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

    info!("Client closed successfully");

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
