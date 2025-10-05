use std::{net::SocketAddr, ops::Deref};

use anyhow::{Result, anyhow};
use argon2::Argon2;
use chacha20poly1305::{AeadCore, KeyInit, XChaCha20Poly1305, aead::Aead};
use cpal::{
    Device, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use log::{debug, error, trace};
use rand::rngs::OsRng;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Producer, Split},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{self},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::{
    audio::vorbis::VorbisDecoder,
    common::{
        constants::{DEFAULT_LATENCY, DEFAULT_VOLUME},
        serializers::{OABSMessage, SupportedStreamConfigDeserialize},
    },
};

#[derive(Clone)]
enum StreamStatus {
    Starting,
    Streaming,
    Stopped,
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

pub struct ClientController {
    selected_device: Option<Device>,
    latency: (sync::watch::Sender<u32>, sync::watch::Receiver<u32>),
    volume: (sync::watch::Sender<u32>, sync::watch::Receiver<u32>),

    cancellation_token: Option<CancellationToken>,

    tasks: JoinSet<Result<()>>,
}

impl ClientController {
    pub fn new() -> Self {
        Self {
            selected_device: cpal::default_host().default_output_device(),
            latency: sync::watch::channel(DEFAULT_LATENCY as u32),
            volume: sync::watch::channel(DEFAULT_VOLUME as u32),
            cancellation_token: None,
            tasks: JoinSet::new(),
        }
    }

    pub fn get_latency(&self) -> u32 {
        *self.volume.1.borrow()
    }

    pub fn get_volume(&self) -> u32 {
        *self.latency.1.borrow()
    }

    pub fn get_devices() -> Vec<Device> {
        let host = cpal::default_host();
        if let Ok(device) = host.output_devices() {
            device.filter(|x| x.name().is_ok()).collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_device_names() -> Vec<String> {
        let output_devices = Self::get_devices();
        output_devices
            .iter()
            .map(|x| x.name().unwrap_or_default())
            .collect()
    }

    pub fn set_device(&mut self, device_name: String) {
        self.selected_device = Self::get_devices()
            .into_iter()
            .find(|d| d.name().is_ok_and(|d_name| d_name == device_name));
    }

    pub fn set_volume(&self, volume: u32) {
        self.volume.0.send_replace(volume);
    }

    pub fn set_latency(&self, latency: u32) {
        self.latency.0.send_replace(latency);
    }

    async fn create_decode_task(
        encrypt_key: [u8; 32],
        mut encoded_rx: sync::mpsc::Receiver<(StreamStatus, Vec<u8>)>,
        mut producer: <HeapRb<f32> as ringbuf::traits::Split>::Prod,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        debug!("Starting decode task");

        let mut decoder = VorbisDecoder::new();

        let cancellation_token = cancellation_token;

        'decode_loop: loop {
            let (stream_status, data) = match encoded_rx.recv().await {
                Some(val) => val,
                None => continue,
            };

            if cancellation_token.is_cancelled() {
                break;
            }

            let cipher = XChaCha20Poly1305::new(encrypt_key.as_slice().into());
            let nonce = data[data.len() - 24..]
                .iter()
                .map(|v| v ^ 0x7)
                .collect::<Vec<u8>>();
            let ciphertext = &data[..data.len() - 24];

            let data = cipher
                .decrypt(nonce.deref().into(), ciphertext)
                .map_err(|e| anyhow!("{e}"))?;

            match stream_status {
                StreamStatus::Starting => {
                    let _ = decoder
                        .decode_first_package(&data)
                        .inspect_err(|e| error!("Error while decoding first  {e}"));
                }
                StreamStatus::Streaming => {
                    let _ = decoder
                        .decode_next_package(&data, |stream| {
                            producer.push_slice(&stream);
                        })
                        .inspect_err(|e| error!("Error while decoding {e}"));
                }
                StreamStatus::Stopped => {
                    break 'decode_loop;
                }
            };
        }

        debug!("Stopping decode task");
        Ok(())
    }

    async fn stream_receiver_task(
        client_id: String,
        server_address: SocketAddr,
        encoded_tx: sync::mpsc::Sender<(StreamStatus, Vec<u8>)>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        debug!("Starting stream receiver task");

        let local_addr: SocketAddr = "0.0.0.0:12312".parse()?;
        let cli = UdpSocket::bind(local_addr).await?;
        cli.connect(server_address).await?;

        debug!("Using local address: {}", cli.local_addr()?);

        let mut buf = [0; 10240];

        let mut stream_status = StreamStatus::Starting;
        loop {
            match stream_status {
                StreamStatus::Starting => {
                    cli.send("START".as_bytes()).await?;
                }
                _ => {}
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
                _ = cancellation_token.cancelled() => {
                    stream_status = StreamStatus::Stopped;
                    0
                }
            };

            let data = if recv_len > 0 {
                buf[..recv_len].to_owned()
            } else {
                Vec::new()
            };

            let _ = encoded_tx.send((stream_status.clone(), data)).await;

            match stream_status {
                StreamStatus::Starting => {
                    let _ = cli.send(format!("HEADER_OK {client_id}").as_bytes()).await;
                    stream_status = StreamStatus::Streaming;
                }
                StreamStatus::Streaming => {}
                StreamStatus::Stopped => {
                    break;
                }
            };
        }

        debug!("Stopping stream receiver task");
        Ok::<(), anyhow::Error>(())
    }

    async fn server_receiver_task(
        mut tcp_stream: TcpStream,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        debug!("Starting server connection task");

        let mut close_requested = false;

        loop {
            let data = tokio::select! {
                result = recv_data(&mut tcp_stream) => {
                    match result {
                        Ok(data) => data,
                        Err(e) => {
                            error!("{e:?}");
                            continue;
                        },
                    }
                },
                _ = cancellation_token.cancelled() => {
                    close_requested = true;
                    vec![]
                }
            };

            if close_requested {
                if let Err(e) = send_data(&mut tcp_stream, "CLOSE".as_bytes()).await {
                    error!("Failed to send CLOSE: {e:?}");
                }
                break;
            }

            let value = match std::str::from_utf8(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if value == "PING" {
                trace!("PING");
                loop {
                    if let Err(e) = send_data(&mut tcp_stream, "PONG".as_bytes()).await {
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

        cancellation_token.cancel();

        debug!("Stopping server connection task");

        Ok(())
    }

    async fn create_playback_task(
        volume: sync::watch::Receiver<u32>,
        config: SupportedStreamConfig,
        device: Device,
        mut consumer: <HeapRb<f32> as ringbuf::traits::Split>::Cons,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        debug!("Starting playback task");

        let err_fn = move |err| {
            error!("An error occurred on stream: {}", err);
        };

        let playback_stream = device.build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &_| {
                let requested_count = data.len();
                let read_count = consumer.pop_slice(data);
                data.iter_mut()
                    .for_each(|s| *s = *s * *volume.borrow() as f32 / 100.0);
                if read_count < requested_count {
                    trace!("input stream fell behind: try increasing latency");
                }
            },
            err_fn,
            None,
        )?;

        playback_stream.play()?;
        cancellation_token.cancelled().await;
        playback_stream.pause()?;

        drop(playback_stream);

        debug!("Stopping playback task");

        Ok(())
    }

    pub async fn start(&mut self, server_address: SocketAddr, password: String) -> Result<()> {
        let cancellation_token = CancellationToken::new();

        let device = self
            .selected_device
            .as_ref()
            .ok_or(anyhow!("Could not find default output device"))?;

        let mut tcp_stream = TcpStream::connect(server_address).await?;

        debug!("Connected to server {}", server_address);

        // Wait for the socket to be readable
        tcp_stream.readable().await?;

        // Creating the buffer **after** the `await` prevents it from
        // being stored in the async task.

        let (salt, verifier) = loop {
            match recv_data(&mut tcp_stream).await {
                Ok(data) => {
                    let value: OABSMessage = serde_json::from_slice(&data)?;
                    if let OABSMessage::AuthenticateRequest { salt, verifier } = value {
                        break (salt, verifier);
                    }
                    return Err(anyhow!("Didn't get authenticate request"));
                }
                Err(e) => {
                    return Err(anyhow!("Failed to receive data: {e}"));
                }
            }
        };

        let mut encrypt_key = [0u8; 32];
        Argon2::default()
            .hash_password_into(&password.into_bytes(), salt.as_bytes(), &mut encrypt_key)
            .map_err(|e| anyhow!("Could not hash password: {e}"))?;

        let cipher = XChaCha20Poly1305::new((&encrypt_key).into());
        let nonce: chacha20poly1305::aead::generic_array::GenericArray<u8, _> =
            XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut verifier_encrypted = cipher
            .encrypt(&nonce, verifier.deref())
            .map_err(|e| anyhow!("{e}"))?;
        verifier_encrypted
            .extend_from_slice(nonce.iter().map(|v| v ^ 0x5).collect::<Vec<u8>>().deref());

        let auth_msg = serde_json::to_vec(&OABSMessage::Authenticate {
            verifier: verifier_encrypted,
        })?;

        send_data(&mut tcp_stream, auth_msg.as_slice()).await?;

        let client_id: String = loop {
            match recv_data(&mut tcp_stream).await {
                Ok(data) => {
                    let value: OABSMessage = serde_json::from_slice(&data)?;
                    if let OABSMessage::ClientId { id } = value {
                        break id;
                    }
                    return Err(anyhow!("Client id not found"));
                }
                Err(_e) => {
                    // TODO: Add delay
                    return Err(anyhow!(
                        "Could not authenticate, please check your password"
                    ));
                }
            }
        };
        debug!("Receiving client_id {:?}", client_id);

        // Try to read data, this may still fail with `WouldBlock`
        // if the readiness event is a false positive.
        let config = loop {
            match recv_data(&mut tcp_stream).await {
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
        let latency_frames =
            (*self.latency.1.borrow() as f32 / 1_000.0) * config.sample_rate().0 as f32;
        let latency_samples = latency_frames as usize * config.channels() as usize;

        let (encoded_tx, encoded_rx) = sync::mpsc::channel::<(StreamStatus, Vec<u8>)>(20);

        // The buffer to share samples
        let (mut producer, consumer) = HeapRb::<f32>::new(latency_samples).split();

        debug!("Latency Samples: {latency_samples}");

        // Fill the samples with 0.0 equal to the length of the delay.
        for _ in 0..latency_samples {
            // The ring buffer has twice as much space as necessary to add latency here,
            // so this should never fail
            producer
                .try_push(0.0)
                .map_err(|e| anyhow!("Could not fill buffer: {e}"))?;
        }

        self.tasks.spawn(Self::create_decode_task(
            encrypt_key,
            encoded_rx,
            producer,
            cancellation_token.child_token(),
        ));
        self.tasks.spawn(Self::create_playback_task(
            self.volume.1.clone(),
            config,
            device.clone(),
            consumer,
            cancellation_token.child_token(),
        ));
        self.tasks.spawn(Self::stream_receiver_task(
            client_id.clone(),
            server_address,
            encoded_tx,
            cancellation_token.child_token(),
        ));
        self.tasks.spawn(Self::server_receiver_task(
            tcp_stream,
            cancellation_token.clone(),
        ));

        self.cancellation_token.replace(cancellation_token);

        Ok(())
    }

    pub async fn wait(&mut self) -> Result<()> {
        while let Some(result) = self.tasks.join_next().await {
            let internal_result = result.map_err(|e| anyhow!("Error joining {e}")).flatten();
            if let Err(e) = internal_result {
                error!("{e}");
            }
        }
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(cancellation_token) = self.cancellation_token.take() {
            cancellation_token.cancel();
            self.wait().await?;
        }
        debug!("Client closed successfully");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        return self.cancellation_token.is_some();
    }
}

impl Drop for ClientController {
    fn drop(&mut self) {
        if let Some(cancellation_token) = self.cancellation_token.take() {
            cancellation_token.cancel();
        }
    }
}
