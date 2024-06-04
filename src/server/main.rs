use std::{
    env,
    net::SocketAddr,
    sync::mpsc::{self, Receiver},
};

use anyhow::{anyhow, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, SupportedStreamConfig,
};
use oabs_lib::SupportedStreamConfigSerialize;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, UdpSocket},
    signal,
    sync::watch,
    task::yield_now,
};

struct SampleData {
    data: Vec<u8>,
}

async fn payload_server(
    server_addr: SocketAddr,
    config: SupportedStreamConfig,
    mut close_rx: watch::Receiver<bool>,
) -> Result<()> {
    println!("Starting Payload Server");
    let listener = TcpListener::bind(server_addr).await?;
    println!("Payload server listening on: {}", listener.local_addr()?);
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
        println!("Sending to {:?} payload {:?}", socket.local_addr(), config);
        let value = serde_json::to_string(&SupportedStreamConfigSerialize(&config))?;
        socket.write(value.as_bytes()).await?;
    }
    println!("Stopping Payload Server");
    Ok(())
}

async fn stream_server(
    server_addr: SocketAddr,
    data_send_rx: Receiver<SampleData>,
    mut close_rx: watch::Receiver<bool>,
) -> Result<()> {
    println!("Starting Stream Server");
    let sock = UdpSocket::bind(server_addr).await?;
    println!("Stream server listening on: {}", sock.local_addr()?);
    let mut buffer = [0; 123];
    loop {
        if let Ok(changed) = close_rx.has_changed() {
            if changed && *close_rx.borrow_and_update() {
                break;
            }
        }

        let sample = loop {
            let result = data_send_rx.recv();
            if let Ok(data) = result {
                break data;
            } else {
                // sleep(Duration::from_millis(10)).await;
            }
            yield_now().await;
        };

        if let Ok((_, source)) = sock.try_recv_from(&mut buffer) {
            let _ = sock.send_to(&sample.data, source).await?;
            // println!("{:?} bytes sent", len);
        } else {
            yield_now().await;
        }
    }
    println!("Stopping Stream Server");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server_addr: SocketAddr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:48182".into())
        .parse()?;

    let host = cpal::default_host();

    let input_devices: Vec<Device> = host.input_devices()?.collect();

    let devices_names = input_devices
        .iter()
        .filter(|x| x.name().is_ok())
        .map(|x| x.name().unwrap_or(String::new()))
        .collect::<Vec<String>>();

    println!("{devices_names:?}");

    let selected_name = "Voicemeeter Out B3 (VB-Audio Voicemeeter VAIO)";

    let device = input_devices
        .into_iter()
        .find(|p| p.name().is_ok_and(|n| n == selected_name))
        .ok_or(anyhow!(
            "Selected device {selected_name:?} could not be found"
        ))?;

    let config = device
        .default_input_config()
        .map_err(|e| anyhow!("Failed to get default input config: {e}"))?;

    // A flag to indicate that recording is in progress.
    println!("Begin recording...");

    // Run the input stream on a separate thread.

    let (close_tx, close_rx) = watch::channel(false);
    let (stream_close_tx, stream_close_rx) = mpsc::channel();
    let (data_send_tx, data_send_rx) = mpsc::channel::<SampleData>();

    println!("Sample Format {:?}", config.sample_format());

    let err_fn = move |err| {
        eprintln!("an error occurred on stream: {}", err);
    };

    let config_clone = config.clone();
    let stream_thread = std::thread::spawn(move || -> Result<()> {
        let stream = match config_clone.sample_format() {
            SampleFormat::I8 => device.build_input_stream(
                &config_clone.into(),
                move |data: &[i8], _: &_| {
                    let _ = data_send_tx.send(SampleData {
                        data: data
                            .into_iter()
                            .flat_map(|v| v.to_be_bytes())
                            .collect::<Vec<u8>>(),
                    });
                },
                err_fn,
                None,
            )?,
            SampleFormat::I16 => device.build_input_stream(
                &config_clone.into(),
                move |data: &[i16], _: &_| {
                    let _ = data_send_tx.send(SampleData {
                        data: data
                            .into_iter()
                            .flat_map(|v| v.to_be_bytes())
                            .collect::<Vec<u8>>(),
                    });
                },
                err_fn,
                None,
            )?,
            SampleFormat::I32 => device.build_input_stream(
                &config_clone.into(),
                move |data: &[i32], _: &_| {
                    let _ = data_send_tx.send(SampleData {
                        data: data
                            .into_iter()
                            .flat_map(|v| v.to_be_bytes())
                            .collect::<Vec<u8>>(),
                    });
                },
                err_fn,
                None,
            )?,
            SampleFormat::F32 => device.build_input_stream(
                &config_clone.into(),
                move |data: &[f32], _: &_| {
                    let _ = data_send_tx.send(SampleData {
                        data: data
                            .into_iter()
                            .flat_map(|v| v.to_be_bytes())
                            .collect::<Vec<u8>>(),
                    });
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
            if stream_close_rx.recv()? {
                println!("Closing stream thread");
                break;
            }
        }
        stream.pause()?;

        drop(stream);

        Ok(())
    });

    let mut ctrl_c_signal = signal::windows::ctrl_c()?;
    let mut ctrl_close_signal = signal::windows::ctrl_close()?;
    let mut ctrl_break_signal = signal::windows::ctrl_break()?;
    let mut ctrl_logoff_signal = signal::windows::ctrl_logoff()?;
    let mut ctrl_shutdown_signal = signal::windows::ctrl_shutdown()?;

    let stream_joinh = tokio::spawn(stream_server(server_addr, data_send_rx, close_rx.clone()));
    let payload_joinh = tokio::spawn(payload_server(server_addr, config, close_rx.clone()));

    tokio::select! {
        _ = ctrl_c_signal.recv() => { },
        _ = ctrl_close_signal.recv() => { },
        _ = ctrl_break_signal.recv() => { },
        _ = ctrl_logoff_signal.recv() => { },
        _ = ctrl_shutdown_signal.recv() => { },
    };

    close_tx.send(true)?;
    if let Err(e) = stream_joinh.await? {
        eprintln!("{e:?}");
    }
    if let Err(e) = payload_joinh.await? {
        eprintln!("{e:?}");
    }

    stream_close_tx.send(true)?;
    stream_thread
        .join()
        .map_err(|e| anyhow!("Could not join stream thread: {e:?}"))??;

    println!("Server closed successfully");

    Ok(())
}
