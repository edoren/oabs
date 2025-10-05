use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use argon2::{Argon2, password_hash::SaltString};
use chacha20poly1305::{
    XChaCha20Poly1305,
    aead::{Aead, AeadCore, KeyInit},
};
use cpal::{
    Device, DevicesError, SampleFormat, SampleRate, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures::TryFutureExt;
// use futures::TryFutureExt;
use local_ip_address::local_ip;
use log::{debug, error, trace};
use rand::{Rng, rngs::OsRng};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::{JoinSet, yield_now},
    time::{Instant, sleep, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::{
    audio::vorbis::VorbisEncoder,
    common::serializers::{OABSMessage, SupportedStreamConfigSerialize},
    server::upnp::{AddPortConfig, DeletePortConfig, PortMappingProtocol, UPnP},
};

enum ClientStatus {
    Connected { id: String },
    Disconnected { id: String },
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
fn get_new_id() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

async fn client_handler(
    mut stream: TcpStream,
    salt: String,
    encrypt_key: [u8; 32],
    config: SupportedStreamConfig,
    client_status_tx: tokio::sync::mpsc::Sender<ClientStatus>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let cipher = XChaCha20Poly1305::new((&encrypt_key).into());

    let random_verifier = OsRng.r#gen::<[u8; 8]>().to_vec();

    let auth_msg = serde_json::to_vec(&OABSMessage::AuthenticateRequest {
        salt: salt.to_string(),
        verifier: random_verifier.clone(),
    })?;

    debug!("Sending auth request");
    send_data(&mut stream, &auth_msg).await?;

    let auth_result = timeout_at(
        Instant::now() + Duration::from_secs(5),
        recv_data(&mut stream)
            .map_err(|e| anyhow!("Failed receiving authentication response: {e}")),
    )
    .await
    .map_err(|e| anyhow!("Failed authentication, client timeout: {e}"))
    .flatten()?;

    let auth_msg: OABSMessage =
        serde_json::from_slice(&auth_result).context("Could not parse client message")?;
    debug!("Message received {auth_msg:?}");
    if let OABSMessage::Authenticate { verifier } = auth_msg {
        let ciphertext = &verifier[..verifier.len() - 24];
        let nonce = verifier[verifier.len() - 24..]
            .iter()
            .map(|v| v ^ 0x5)
            .collect::<Vec<u8>>();
        let verifier_decrypted = cipher
            .decrypt(nonce.deref().into(), ciphertext)
            .map_err(|e| anyhow!("Failed decrypting verifier: {e}"))?;
        if verifier_decrypted != random_verifier {
            return Err(anyhow!("FAILED VERIFIER"));
        }
    }

    let client_id = get_new_id().to_string();

    let client_id_msg = serde_json::to_vec(&OABSMessage::ClientId {
        id: client_id.clone(),
    })?;
    send_data(&mut stream, &client_id_msg)
        .await
        .context("Failed sending client id")?;

    let value = serde_json::to_vec(&SupportedStreamConfigSerialize(&config))?;
    send_data(&mut stream, &value).await?;

    let _ = client_status_tx
        .send(ClientStatus::Connected {
            id: client_id.clone(),
        })
        .await;

    let client_task = async {
        let instanto = Instant::now();
        let mut ping_timeout = instanto + Duration::from_secs(5);
        let mut pong_deadline = instanto + Duration::from_secs(10);

        loop {
            tokio::select! {
                _ = sleep_until(ping_timeout) => {
                    match send_data(&mut stream, "PING".as_bytes()).await {
                        Ok(_) => {
                            trace!("Sending PING to client with id {client_id:?}");
                            ping_timeout = Instant::now() + Duration::from_secs(5);
                        }
                        Err(e) => match e.kind() {
                            io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset => {
                                debug!("Connection aborted from client {client_id}");
                                break;
                            }
                            _ => {
                                error!("Error sending PING from client with id {client_id:?}: {e:?}");
                            }
                        },
                    }
                },
                result = timeout_at(pong_deadline, recv_data(&mut stream)) => {
                    let read_result = match result {
                        Ok(result) => result,
                        Err(_) => {
                            debug!("Client with id {client_id:?} forgot to PONG");
                            break;
                        }
                    };

                    match read_result {
                        Err(e) => {
                            if e.kind() == io::ErrorKind::UnexpectedEof {
                                debug!("Connection aborted from client with id {client_id:?}");
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
                                    trace!("Received PONG from client with id {client_id:?}");
                                    pong_deadline = Instant::now() + Duration::from_secs(10);
                                }
                                if value == "CLOSE" {
                                    debug!("Close requested from client with id {client_id:?}");
                                    break;
                                }
                            }
                        }
                    }
                }
            };
        }
    };

    tokio::select! {
        _ = client_task => {},
        _ = cancellation_token.cancelled() => {}
    };

    let _ = send_data(&mut stream, "CLOSE".as_bytes()).await;

    let _ = client_status_tx
        .send(ClientStatus::Disconnected {
            id: client_id.clone(),
        })
        .await;

    debug!("Client closed with id {client_id:?}");

    Ok(())
}

async fn payload_server_task(
    server_addr: SocketAddr,
    salt: String,
    encrypt_key: [u8; 32],
    config: SupportedStreamConfig,
    client_status_tx: mpsc::Sender<ClientStatus>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    debug!("Starting payload server");

    let listener = TcpListener::bind(server_addr).await?;
    debug!("Payload server listening on: {}", listener.local_addr()?);
    let mut streams = JoinSet::new();
    loop {
        let stream = tokio::select! {
            result = listener.accept() => {
                result?.0
            },
            _ = cancellation_token.cancelled() => {
                break;
            }
        };
        streams.spawn(client_handler(
            stream,
            salt.clone(),
            encrypt_key.clone(),
            config.clone(),
            client_status_tx.clone(),
            cancellation_token.child_token(),
        ));
    }

    while let Some(_res) = streams.join_next().await {}

    debug!("Stopping payload server");
    Ok(())
}

async fn playback_capturer_task(
    device: Device,
    config: SupportedStreamConfig,
    data_send_tx: mpsc::UnboundedSender<Vec<f32>>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    debug!("Playback capturer");

    let stream = device.build_input_stream(
        &config.into(),
        move |data: &[f32], _| {
            let mut samples = Vec::new();
            samples.extend_from_slice(&data);
            let res = data_send_tx.send(data.to_vec());
            if let Err(e) = res {
                error!("{e:?}");
            }
        },
        move |err| {
            error!("En error occurred on stream: {}", err);
        },
        None,
    )?;

    debug!("Begin recording");
    stream.play()?;
    cancellation_token.cancelled().await;
    stream.pause()?;
    debug!("Stopped recording");

    drop(stream);

    Ok(())
}

async fn stream_server_task(
    server_addr: SocketAddr,
    encrypt_key: [u8; 32],
    quality: f32,
    config: SupportedStreamConfig,
    mut data_send_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    mut client_status_rx: mpsc::Receiver<ClientStatus>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    debug!("Starting stream server");

    let sock = UdpSocket::bind(server_addr).await?;
    debug!("Stream server listening on: {}", sock.local_addr()?);

    let cipher = XChaCha20Poly1305::new((&encrypt_key).into());

    let audio_channels = config.channels().into();

    let mut encoder = VorbisEncoder::new(
        config.channels().into(),
        config.sample_rate().0.try_into()?,
        quality,
    );

    let serial_num = encoder.get_serial_number();
    let header_data = encoder.get_header_data();

    debug!("Serial: {serial_num}");
    debug!("Header size: {}", header_data.len());

    let mut buffer = [0; 256];
    let mut registered_clients: HashMap<String, Option<SocketAddr>> = HashMap::new();

    let mut client_detected = false;

    'stream_loop: loop {
        if cancellation_token.is_cancelled() {
            break;
        }

        if let Ok(client_status) = client_status_rx.try_recv() {
            match client_status {
                ClientStatus::Connected { id } => {
                    registered_clients.insert(id, None);
                }
                ClientStatus::Disconnected { id } => {
                    registered_clients.remove(&id);
                    debug!("Removing client with id {id:?}");
                    client_detected = !registered_clients.is_empty();
                }
            }
        }

        match sock.try_recv_from(&mut buffer) {
            Ok((len, source)) => {
                let msg = unsafe { std::str::from_utf8_unchecked(&buffer[..len]) };
                if msg == "START" {
                    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
                    let mut header_data_ecrypted = cipher
                        .encrypt(&nonce, header_data.deref())
                        .map_err(|e| anyhow!("{e}"))?;
                    header_data_ecrypted.extend_from_slice(
                        nonce.iter().map(|v| v ^ 0x7).collect::<Vec<u8>>().deref(),
                    );

                    let _ = sock.send_to(&header_data_ecrypted, source).await?;
                }
                if msg.starts_with("HEADER_OK") {
                    if let Some(id) = msg.split(" ").nth(1) {
                        client_detected = true;
                        registered_clients.insert(id.into(), Some(source));
                        debug!("Added client {source} with id {id:?}");
                    }
                }
            }
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::ConnectionReset => {}
                _ => error!("{:?}: {e}", e.kind()),
            },
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

        if raw_samples.len() % audio_channels != 0 {
            error!("Invalid audio data length");
            continue;
        }
        let sample_count = raw_samples.len() / audio_channels;

        // Deinterlace the audio obtained from cpal
        // E.g for 2 channels: [L0, R0, L1, R1, L2, R2] -> [[L1, L2, L3], [R1, R2, R3]]
        let audio_block: Vec<Vec<f32>> = (0..audio_channels)
            .map(|channel| {
                (0..sample_count)
                    .map(|sample| raw_samples[sample * audio_channels + channel])
                    .collect()
            })
            .collect();

        let encoded_data = encoder.encode(audio_block)?;

        if registered_clients.is_empty() || !client_detected {
            continue;
        }

        if encoded_data.is_empty() {
            continue;
        }

        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut encoded_data_ecrypted = cipher
            .encrypt(&nonce, encoded_data.deref())
            .map_err(|e| anyhow!("{e}"))?;
        encoded_data_ecrypted
            .extend_from_slice(nonce.iter().map(|v| v ^ 0x7).collect::<Vec<u8>>().deref());

        for source in registered_clients.values() {
            if let Some(source) = source {
                let _len = sock.send_to(&encoded_data_ecrypted, source).await?;
            }
        }

        yield_now().await;
    }

    debug!("Stopping stream server");
    Ok(())
}

async fn upnp_connector_task(port: u16, cancellation_token: CancellationToken) -> Result<()> {
    debug!("Starting UPnP task");

    let local_ipv4 = match local_ip() {
        Ok(IpAddr::V4(ipv4)) => ipv4,
        Ok(IpAddr::V6(ipv6)) => {
            debug!("IPv6 {ipv6} not supported for UPnP");
            return Ok(());
        }
        Err(_) => return Ok(()),
    };

    let add_mapping = AddPortConfig {
        internal_client: local_ipv4,
        internal_port: port,
        external_port: port,
        protocol: PortMappingProtocol::BOTH,
        enabled: true,
        description: "OABS Server".into(),
        lease_duration: 600,
    };

    let delete_mapping = DeletePortConfig {
        external_port: port,
        protocol: PortMappingProtocol::BOTH,
    };

    let upnp = match UPnP::new().await {
        Ok(devices) => devices,
        Err(_e) => {
            debug!("UPnP not supported");
            return Ok(());
        }
    };

    let _ = upnp.delete_port(&delete_mapping).await;
    let result = upnp.add_port(&add_mapping).await;

    if result.is_ok() {
        if let Ok(ip) = upnp.get_external_ip_address().await {
            debug!("Running on external ip {ip}")
        } else {
            error!("Could not get external ip");
        }
    }

    let mut retry_time: u64 = (add_mapping.lease_duration / 2).into();
    loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                break;
            },
            _ = sleep(Duration::from_secs(retry_time)) => { },
        }

        let result = upnp.add_port(&add_mapping).await;
        if result.is_ok() {
            retry_time = (add_mapping.lease_duration / 2).into();
        } else {
            retry_time = 5;
        }
    }

    let _ = upnp.delete_port(&delete_mapping).await;

    debug!("Stopping UPnP task");
    Ok(())
}

pub struct ServerController {
    should_include_output_devices: bool,
    selected_device: Option<Device>,

    cancellation_token: Option<CancellationToken>,
    tasks: JoinSet<Result<()>>,
}

impl ServerController {
    pub fn new() -> Self {
        Self {
            should_include_output_devices: false,
            selected_device: cpal::default_host().default_input_device(),

            cancellation_token: None,
            tasks: JoinSet::new(),
        }
    }

    pub fn set_should_include_output_devices(&mut self, should_include_output_devices: bool) {
        self.should_include_output_devices = should_include_output_devices;
    }

    pub fn get_devices(&self) -> Vec<Device> {
        let host = cpal::default_host();

        let devices_result: Result<Vec<_>, DevicesError> =
            host.input_devices().and_then(|output_devices| {
                if self.should_include_output_devices {
                    host.output_devices().map(|input_devices| {
                        output_devices
                            .into_iter()
                            .chain(input_devices.into_iter())
                            .collect()
                    })
                } else {
                    Ok(output_devices.into_iter().collect())
                }
            });
        if let Ok(devices) = devices_result {
            devices.into_iter().filter(|x| x.name().is_ok()).collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_device_names(&self) -> Vec<String> {
        let output_devices = self.get_devices();
        output_devices
            .iter()
            .map(|x| x.name().unwrap_or_default())
            .collect()
    }

    pub fn set_device(&mut self, device_name: String) {
        self.selected_device = self
            .get_devices()
            .into_iter()
            .find(|d| d.name().is_ok_and(|d_name| d_name == device_name));
    }

    pub async fn start(&mut self, port: u16, quality: f32, password: String) -> Result<()> {
        if self.cancellation_token.is_some() {
            return Err(anyhow!("Server already running"));
        };

        let cancellation_token = {
            let token = CancellationToken::new();
            self.cancellation_token = Some(token.clone());
            token
        };

        let device = self
            .selected_device
            .take()
            .ok_or(anyhow!("Could not find default input device"))?;

        let server_addr = SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), port);

        let configs = device
            .supported_input_configs()?
            .chain(device.supported_output_configs()?)
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

        let (data_send_tx, data_send_rx) = mpsc::unbounded_channel::<Vec<_>>();
        let (client_status_tx, client_status_rx) = mpsc::channel::<ClientStatus>(100);

        debug!("Sample Format {:?}", config.sample_format());

        let (encrypt_key, salt) = {
            let salt = SaltString::generate(&mut OsRng).to_string();

            let mut encrypt_key = [0u8; 32]; // Can be any desired size
            Argon2::default()
                .hash_password_into(&password.into_bytes(), salt.as_bytes(), &mut encrypt_key)
                .map_err(|e| anyhow!("Could not hash password: {e}"))?;

            (encrypt_key, salt)
        };

        self.tasks.spawn(playback_capturer_task(
            device,
            config.clone(),
            data_send_tx,
            cancellation_token.child_token(),
        ));
        self.tasks.spawn(stream_server_task(
            server_addr,
            encrypt_key,
            quality,
            config.clone(),
            data_send_rx,
            client_status_rx,
            cancellation_token.child_token(),
        ));
        self.tasks.spawn(payload_server_task(
            server_addr,
            salt,
            encrypt_key,
            config.clone(),
            client_status_tx,
            cancellation_token.child_token(),
        ));
        self.tasks.spawn(upnp_connector_task(
            server_addr.port(),
            cancellation_token.child_token(),
        ));

        debug!("Server closed successfully");

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
            debug!("Server closed successfully");
        }
        Ok(())
    }
}

impl Drop for ServerController {
    fn drop(&mut self) {
        if let Some(cancellation_token) = self.cancellation_token.take() {
            cancellation_token.cancel();
        }
    }
}
