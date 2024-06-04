use std::{net::SocketAddr, sync::mpsc};

use anyhow::{anyhow, Result};
use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat,
};
use oabs_lib::SupportedStreamConfigDeserialize;
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use tokio::{
    net::{TcpStream, UdpSocket},
    signal,
    sync::watch,
};

#[derive(Parser, Debug)]
#[command(version, about = "Client", long_about = None)]
struct Opt {
    /// The input audio device to use
    #[arg(short, long, value_name = "SERVER", default_value_t = String::from("127.0.0.1:48182"))]
    server: String,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "DELAY_MS", default_value_t = 150.0)]
    latency: f32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();

    let host = cpal::default_host();

    let device = host
        .default_output_device()
        .ok_or(anyhow!("Could not find default output device"))?;

    println!("{:?}", device.name());

    let remote_addr: SocketAddr = opt.server.parse()?;

    println!("Connecting to server {}", opt.server);
    let stream = TcpStream::connect(remote_addr).await?;

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
    println!("Buffer Size: {buffer_size}");

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
    // println!("Begin recording...");

    // // Run the input stream on a separate thread.

    let (close_tx, close_rx) = watch::channel(false);
    let (player_close_tx, player_close_rx) = mpsc::channel();
    // let (data_recv_tx, data_recv_rx) = mpsc::channel::<SampleData>();

    // println!("Sample Format {:?}", config.sample_format());

    let err_fn = move |err| {
        eprintln!("an error occurred on stream: {}", err);
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
                    let mut output_fell_behind = false;
                    for sample in buf[..len]
                        .chunks_exact(4)
                        .map(TryInto::try_into)
                        .map(Result::unwrap)
                        .map(f32::from_be_bytes)
                    {
                        if producer.try_push(sample).is_err() {
                            output_fell_behind = true;
                        }
                    }
                    if output_fell_behind {
                        eprintln!("output stream fell behind: try increasing latency");
                    }
                }
                Err(e) => return Err::<(), anyhow::Error>(anyhow!("{e}")),
            }
        }
        Ok(())
    };

    let config_clone = config.clone();
    let player_task = move || -> Result<()> {
        let stream = match config_clone.sample_format() {
            // SampleFormat::I8 => device.build_output_stream(
            //     &config_clone.into(),
            //     move |data: &mut [i8], _: &_| {
            //         let result = data_recv_rx.recv();
            //         for sample in data {
            //             *sample = match consumer.try_pop() {
            //                 Some(s) => s,
            //                 None => {
            //                     input_fell_behind = true;
            //                     0.0
            //                 }
            //             };
            //         }

            //         if let Ok(sample) = result {
            //             let result: Vec<i8> = sample
            //                 .data
            //                 .chunks_exact(1)
            //                 .map(TryInto::try_into)
            //                 .map(Result::unwrap)
            //                 .map(i8::from_be_bytes)
            //                 .collect();
            //         }
            //         let _ = data_send_tx.send(SampleData {
            //             format: SampleFormat::I8,
            //             data: data
            //                 .into_iter()
            //                 .flat_map(|v| v.to_be_bytes())
            //                 .collect::<Vec<u8>>(),
            //         });
            //     },
            //     err_fn,
            //     None,
            // )?,
            // SampleFormat::I16 => device.build_output_stream(
            //     &config_clone.into(),
            //     move |data: &mut [i16], _: &_| {
            //         let _ = data_send_tx.send(SampleData {
            //             format: SampleFormat::I16,
            //             data: data
            //                 .into_iter()
            //                 .flat_map(|v| v.to_be_bytes())
            //                 .collect::<Vec<u8>>(),
            //         });
            //     },
            //     err_fn,
            //     None,
            // )?,
            // SampleFormat::I32 => device.build_output_stream(
            //     &config_clone.into(),
            //     move |data: &mut [i32], _: &_| {
            //         let _ = data_send_tx.send(SampleData {
            //             format: SampleFormat::I32,
            //             data: data
            //                 .into_iter()
            //                 .flat_map(|v| v.to_be_bytes())
            //                 .collect::<Vec<u8>>(),
            //         });
            //     },
            //     err_fn,
            //     None,
            // )?,
            SampleFormat::F32 => device.build_output_stream(
                &config_clone.into(),
                move |data: &mut [f32], _: &_| {
                    let mut input_fell_behind = false;
                    for sample in data {
                        *sample = match consumer.try_pop() {
                            Some(s) => s,
                            None => {
                                input_fell_behind = true;
                                0.0
                            }
                        };
                    }
                    if input_fell_behind {
                        eprintln!("input stream fell behind: try increasing latency");
                    }
                },
                err_fn,
                None,
            )?,
            sample_format => {
                return Err(anyhow::Error::msg(format!(
                    "Unsupported sample format '{sample_format}'"
                )))
            }
        };

        stream.play()?;
        loop {
            if player_close_rx.recv()? {
                println!("Closing stream thread");
                break;
            }
        }
        stream.pause()?;

        drop(stream);

        Ok(())
    };

    println!("{config:?}");

    let mut ctrl_c_signal = signal::windows::ctrl_c()?;
    let mut ctrl_close_signal = signal::windows::ctrl_close()?;
    let mut ctrl_break_signal = signal::windows::ctrl_break()?;
    let mut ctrl_logoff_signal = signal::windows::ctrl_logoff()?;
    let mut ctrl_shutdown_signal = signal::windows::ctrl_shutdown()?;

    let stream_receiver_joinh = tokio::spawn(stream_receiver_task);
    let player_joinh = std::thread::spawn(player_task);

    tokio::select! {
        _ = ctrl_c_signal.recv() => { },
        _ = ctrl_close_signal.recv() => { },
        _ = ctrl_break_signal.recv() => { },
        _ = ctrl_logoff_signal.recv() => { },
        _ = ctrl_shutdown_signal.recv() => { },
    };

    close_tx.send(true)?;
    if let Err(e) = stream_receiver_joinh.await? {
        eprintln!("{e:?}");
    };

    player_close_tx.send(true)?;
    player_joinh
        .join()
        .map_err(|e| anyhow!("Could not join stream thread: {e:?}"))??;

    println!("Client closed successfully");

    Ok(())
}
