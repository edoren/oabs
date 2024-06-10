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
    traits::{Consumer, Observer, Producer, Split},
    wrap::caching::Caching,
    HeapRb, SharedRb,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    signal,
    sync::watch,
    time::sleep,
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

async fn decoder_task(
    source: Caching<Arc<SharedRb<Heap<u8>>>, false, true>,
    mut producer: Caching<Arc<SharedRb<Heap<f32>>>, true, false>,
    mut close_rx: watch::Receiver<bool>,
) -> Result<()> {
    info!("Starting Decoder Task");

    while source.occupied_len() == 0 {
        sleep(Duration::from_millis(100)).await;
    }

    // println!("Ocupado {}", source.occupied_len());

    // let mut oy: Box<MaybeUninit<ogg_sync_state>> = Box::new(MaybeUninit::uninit());
    // let mut os: Box<MaybeUninit<ogg_stream_state>> = Box::new(MaybeUninit::uninit());

    // let mut og: Box<MaybeUninit<ogg_page>> = Box::new(MaybeUninit::uninit());
    // let mut op: Box<MaybeUninit<ogg_packet>> = Box::new(MaybeUninit::uninit());

    // let mut vi: Box<MaybeUninit<vorbis_info>> = Box::new(MaybeUninit::uninit());

    // let mut vc: Box<MaybeUninit<vorbis_comment>> = Box::new(MaybeUninit::uninit());
    // let mut vd: Box<MaybeUninit<vorbis_dsp_state>> = Box::new(MaybeUninit::uninit());
    // let mut vb: Box<MaybeUninit<vorbis_block>> = Box::new(MaybeUninit::uninit());

    // unsafe {
    //     ogg_sync_init(oy.as_mut_ptr());

    //     // loop {
    //     let mut buffer =
    //         std::slice::from_raw_parts_mut(ogg_sync_buffer(oy.as_mut_ptr(), 8192).cast(), 8192);
    //     let bytes = source.pop_slice(buffer);

    //     println!("buffer {}", bytes);

    //     for data in &buffer[..10] {
    //         println!("{:X?}", data);
    //     }

    //     println!(
    //         "ogg_sync_wrote {}",
    //         ogg_sync_wrote(oy.as_mut_ptr(), bytes as i32)
    //     );

    //     let ret = ogg_sync_pageout(oy.as_mut_ptr(), og.as_mut_ptr());
    //     if ret != 1 {
    //         /* have we simply run out of data?  If so, we're done. */
    //         if bytes < 4096 {
    //             return Ok::<(), anyhow::Error>(());
    //         };

    //         /* error case.  Must not be Vorbis data */
    //         eprintln!("Input does not appear to be an Ogg bitstream. {ret}");
    //     }

    //     // ogg_stream_init(os.as_mut_ptr(), ogg_page_serialno(og.as_mut_ptr()));

    //     // }
    // }

    let mut vf: Box<MaybeUninit<OggVorbis_File>> = Box::new(MaybeUninit::uninit());

    let source = Box::into_raw(source.into());

    unsafe {
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
                    let ret: usize = source.pop_slice(buf);
                    return ret;
                    // match source.read(buf) {
                    //     Ok(n) => n / size,
                    //     Err(err) => {
                    //         // vorbisfile checks errno to tell EOF apart from read errors:
                    //         // https://xiph.org/vorbis/doc/vorbisfile/callbacks.html
                    //         // Rust Read trait implementations are not required to set
                    //         // errno, so make sure we set errno with the most informative
                    //         // value possible, falling back to a non-zero errno, which is
                    //         // implied by the C standard to signal some condition
                    //         // set_errno(Errno(err.raw_os_error().unwrap_or(i32::MAX)));

                    //         0
                    //     }
                    // }
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

        let res = ov_open_callbacks(
            source.cast(),
            vf.as_mut_ptr(),
            std::ptr::null(),
            0,
            callbacks,
        );
        if res != 0 {
            todo!("Error");
        }

        let channels = (*vf.assume_init_ref().vi).channels as usize;

        println!("Channels!! {}", channels);
    }

    loop {
        if let Ok(changed) = close_rx.has_changed() {
            if changed && *close_rx.borrow_and_update() {
                break;
            }
        }

        let mut current_bitstream = Box::new(MaybeUninit::uninit());
        let mut sample_buf = Box::new(MaybeUninit::uninit());

        // SAFETY: we assume ov_read_float follows its documented contract. See the
        // VorbisAudioSamples implementation for more safety information
        unsafe {
            let samples_read = ov_read_float(
                vf.as_mut_ptr(),
                sample_buf.as_mut_ptr(),
                1024, // Most stereo Ogg Vorbis files in the wild use a maximum block size of 2048 samples
                current_bitstream.as_mut_ptr(),
            );

            if samples_read < 0 {
                continue;
            }
            if samples_read == 0 {
                continue;
            }

            if samples_read > 0 {
                // This is not documented, but we can only assume the current bitstream number was
                // initialized if we read some sample; else, ov_read_float may not write to
                // current_bitstream. Read the ov_read_float source code
                if current_bitstream.assume_init_read() != 0 {
                    return Err(anyhow!("VorbisError::UnsupportedStreamChaining"));
                }

                let buf: *mut *mut f32 = sample_buf.assume_init_read();
                let samples_read = samples_read as usize;
                let channels = (*vf.assume_init_ref().vi).channels as usize;

                let mut values =
                    std::slice::from_raw_parts(buf, channels)
                        .iter()
                        .map(|channel_samples| {
                            std::slice::from_raw_parts(*channel_samples, samples_read)
                        });

                let channel1 = values.next().unwrap();
                producer.push_slice(channel1);

                // Ok(self.last_audio_block.as_ref())
            } else {
                // Ok(None)
            }
        }
    }

    // println!("Data 0");

    // let mut decoder = VorbisDecoder::new(source);

    // let mut decoder = if let Err(e) = decoder {
    //     info!("TAMARE {e:?}");
    //     return Err(anyhow!("TARAME"));
    // } else {
    //     decoder.unwrap()
    // };

    // loop {
    //     if let Ok(changed) = close_rx.has_changed() {
    //         if changed && *close_rx.borrow_and_update() {
    //             break;
    //         }
    //     }

    //     let result = decoder.decode_audio_block();
    //     if let

    //     println!("Data 1 {:?}", result.is_none());
    //     if let Some(decoded_block) = result {
    //         let channel1 = decoded_block.samples()[0];
    //         let data_count = channel1.len();
    //         println!("DATA 2 {data_count}");
    //         // let written_count = producer.push_slice(channel1);
    //         // if written_count < data_count {
    //         //     trace!("output stream fell behind: try increasing latency");
    //         // }
    //     }
    // }

    unsafe {
        ov_clear(vf.as_mut_ptr());
    }

    info!("Finishing Decoder Task");
    Ok::<(), anyhow::Error>(())
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

    let decode_ring = HeapRb::<u8>::new(latency_samples * 2 * 10);
    let (mut decode_producer, decode_consumer) = decode_ring.split();

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
        let local_addr: SocketAddr = "0.0.0.0:12312".parse()?;
        let cli = UdpSocket::bind(local_addr).await?;
        debug!("Using local address: {}", cli.local_addr()?);
        cli.connect(remote_addr).await?;
        let mut buf = [0; 10240];

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
                    // let response = unsafe { std::str::from_utf8_unchecked(&buf[..recv_len]) };
                    // println!("Header {recv_len} bytes");
                    decode_producer.push_slice(&buf[..recv_len]);
                    stream_status = StreamStatus::Streaming;
                }
                StreamStatus::Streaming => {
                    // println!("Received {recv_len} bytes");
                    decode_producer.push_slice(&buf[..recv_len]);
                    // let mut decoder: VorbisDecoder<&[u8]> = VorbisDecoder::new(&buf[..recv_len])?;
                    // while let Some(decoded_block) = decoder.decode_audio_block()? {
                    //     let channel1 = decoded_block.samples()[0];
                    //     let data_count = channel1.len();
                    //     let written_count = producer.push_slice(channel1);
                    //     if written_count < data_count {
                    //         trace!("output stream fell behind: try increasing latency");
                    //     }
                    // }
                } // StreamStatus::Stopped => {}
            };
        }

        Ok::<(), anyhow::Error>(())
    };

    let config_clone = config.clone();
    let device_clone = device.clone();
    let mut player_close_rx = close_rx.clone();
    let player_task = async move {
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
            if let Ok(_) = player_close_rx.changed().await {
                if *player_close_rx.borrow_and_update() {
                    break;
                }
            }
        }
        info!("Stopping playback");
        stream.pause()?;

        drop(stream);

        debug!("Stopping playback thread");
        Ok::<(), anyhow::Error>(())
    };

    let server_receiver = async move {
        info!("Creating PONG handler");

        loop {
            if let Ok(data) = recv_data(&mut stream).await {
                let value = unsafe { std::str::from_utf8_unchecked(&data) };
                debug!("YAAAAAS VALUE {value}");
                if value == "PING" {
                    debug!("YAAAAAS PING");
                    loop {
                        if let Err(e) = send_data(&mut stream, "PONG".as_bytes()).await {
                            debug!("ERROR SENDING PONG: {e:?}");
                        } else {
                            debug!("YAAAAAS PONG");
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

        info!("Closing PONG handler");
    };

    #[cfg(not(target_os = "windows"))]
    let close_task = async move {
        tokio::select! {
            _ = signal::ctrl_c() => { },
            _ = server_receiver => { }
        };
        close_tx.send(true)?;
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
        return Ok::<(), anyhow::Error>(());
    };

    let close_joinh = tokio::spawn(close_task);
    let decoder_joinh = tokio::spawn(decoder_task(
        decode_consumer,
        producer,
        close_rx.clone(),
    ));

    let local = tokio::task::LocalSet::new();
    let stream_joinh = local.spawn_local(player_task);
    let stream_receiver_joinh = local.spawn_local(stream_receiver_task);
    local.await;

    if let Err(e) = decoder_joinh.await? {
        error!("{e:?}");
    };
    if let Err(e) = close_joinh.await? {
        error!("{e:?}");
    };
    // if let Err(e) = stream_joinh.await? {
    //     error!("{e:?}");
    // };
    // if let Err(e) = stream_receiver_joinh.await? {
    //     error!("{e:?}");
    // };

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
