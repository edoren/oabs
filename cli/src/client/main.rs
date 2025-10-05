use std::{
    net::{SocketAddr, ToSocketAddrs},
    process::ExitCode,
};

use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Parser};
use dialoguer::{FuzzySelect, Input, Password, theme::ColorfulTheme};
use log::error;
use oabs_common::{history::HistoryFile, signal};
use oabs_lib::{
    client::ClientController,
    common::constants::{DEFAULT_PORT, DEFAULT_SERVER_NAME, MAX_LATENCY, MIN_LATENCY},
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    EnvFilter, Layer, filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt,
};

#[derive(Parser, Debug)]
#[command(version, about = "Client", long_about = None)]
struct Opt {
    /// The server address to connect to
    #[arg(short, long, value_name = "SERVER")]
    server: Option<String>,

    #[arg(short, long,
          help = format!("Specify the delay between input and output in milliseconds [{MIN_LATENCY}, {MAX_LATENCY}]"),
          value_name = "DELAY_MS", value_parser = clap::value_parser!(u32).range((MIN_LATENCY as i64)..=(MAX_LATENCY as i64)))]
    latency: Option<u32>,

    /// Specify the password
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,

    /// Specify the volume [0, 100]
    #[arg(short, long, value_name = "VOLUME", value_parser = clap::value_parser!(u32).range(0..=100))]
    volume: Option<u32>,

    /// The device to capture audio from
    #[arg(short, long, value_name = "DEVICE_NAME")]
    device_name: Option<String>,

    /// Use the default device to play music
    #[cfg(not(target_os = "android"))]
    #[arg(long, action = ArgAction::SetTrue)]
    default_device: bool,
}

fn parse_server_name(input: &str) -> Option<SocketAddr> {
    let mut splitted = input.split(":");
    let host = if let Some(val) = splitted.next() {
        val.to_string()
    } else {
        return None;
    };
    let port = splitted.next();
    let address = if port.is_none() {
        format!("{host}:{DEFAULT_PORT}")
    } else {
        host
    };
    let value = address.to_socket_addrs();
    if let Ok(mut iter) = value {
        while let Some(val) = iter.next() {
            if val.is_ipv4() {
                return Some(val);
            }
        }
    }
    None
}

async fn cli() -> Result<()> {
    let opt = Opt::parse();

    let app_config_dir = dirs::config_dir()
        .ok_or(anyhow!("Could not get config dir"))?
        .join("me.edoren.oabs-client");

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

    let timer = time::format_description::parse(
        "[year]-[month padding:zero]-[day padding:zero] [hour]:[minute]:[second]",
    )?;
    let time_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let timer = tracing_subscriber::fmt::time::OffsetTime::new(time_offset, timer);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_timer(timer)
        .with_filter(default_filter(default_stdout_level_filter))
        .boxed();

    let mut layers = Vec::new();
    layers.push(file_layer);
    layers.push(stdout_layer);
    tracing_subscriber::registry().with(layers).init();

    // App

    eprintln!(" ██████╗  █████╗ ██████╗ ███████╗");
    eprintln!("██╔═══██╗██╔══██╗██╔══██╗██╔════╝");
    eprintln!("██║   ██║███████║██████╔╝███████╗");
    eprintln!("██║   ██║██╔══██║██╔══██╗╚════██║");
    eprintln!("╚██████╔╝██║  ██║██████╔╝███████║");
    eprintln!(" ╚═════╝ ╚═╝  ╚═╝╚═════╝ ╚══════╝");
    eprintln!("[ Open Audio Broadcast Software ]");
    eprintln!();

    let mut controller = ClientController::new();

    let input_theme = ColorfulTheme::default();

    let mut history_file = HistoryFile::new(&app_config_dir.join("history.json"), Some(10), true)?;

    let password = opt
        .password
        .or_else(|| {
            Password::with_theme(&input_theme)
                .with_prompt("Enter the password")
                .interact()
                .ok()
        })
        .context("Error setting password")?;

    let server_address = if let Some(server) = opt.server {
        parse_server_name(&server).ok_or(anyhow!("This is not a valid address"))?
    } else {
        let history = history_file.get("server_name");
        let result = Input::with_theme(&input_theme)
            .with_prompt("Enter the server")
            .default(
                history
                    .get_last()
                    .unwrap_or(String::from(DEFAULT_SERVER_NAME)),
            )
            .validate_with(|input: &String| -> Result<(), &str> {
                match parse_server_name(input) {
                    Some(_) => Ok(()),
                    None => Err("This is not a valid address"),
                }
            })
            .history_with(history)
            .interact_text()?;
        parse_server_name(&result).ok_or(anyhow!("This is not a valid address"))?
    };

    let latency = if let Some(latency) = opt.latency {
        latency
    } else {
        let history = history_file.get("latency");
        Input::with_theme(&input_theme)
            .with_prompt("Enter the latency")
            .default(
                history
                    .get_last()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(controller.get_latency()),
            )
            .validate_with(|val: &u32| -> Result<(), &str> {
                if *val >= MIN_LATENCY && *val <= MAX_LATENCY {
                    Ok(())
                } else {
                    Err("Volume should be between 0 and 100")
                }
            })
            .history_with(history)
            .interact_text()? as u32
    };

    let volume = if let Some(volume) = opt.volume {
        volume
    } else {
        let history = history_file.get("volume");
        Input::with_theme(&input_theme)
            .with_prompt("Enter the volume")
            .default(
                history
                    .get_last()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(controller.get_volume()),
            )
            .validate_with(|val: &u32| -> Result<(), &str> {
                if *val <= 100 {
                    Ok(())
                } else {
                    Err("Volume should be between 0 and 100")
                }
            })
            .history_with(history)
            .interact_text()? as u32
    };

    let mut device_names = ClientController::get_device_names();
    device_names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    let device_history = history_file.get("device_name");
    let device_name_opt = if let Some(device_name) = opt.device_name {
        let device_found = device_names.iter().find(|&d| d == &device_name);
        if let Some(device_name) = device_found {
            Some(device_name.clone())
        } else {
            return Err(anyhow!("Could not find a device with name {device_name}"));
        }
    } else {
        #[cfg(not(target_os = "android"))]
        if !opt.default_device {
            if let Some(last_device) = device_history.get_last() {
                if let Some(index) = device_names.iter().position(|d| d == &last_device) {
                    device_names.swap(0, index);
                }
            }

            let selection = FuzzySelect::with_theme(&input_theme)
                .with_prompt("Select the output device")
                .default(0)
                .items(&device_names)
                .interact()?;

            Some(device_names[selection].clone())
        } else {
            None
        }

        #[cfg(target_os = "android")]
        None
    };
    drop(device_names);

    if let Some(device_name) = device_name_opt {
        device_history.add(device_name.clone());
        controller.set_device(device_name);
    }

    drop(history_file);
    eprintln!();

    controller.set_latency(latency);
    controller.set_volume(volume);

    controller.start(server_address, password).await?;

    tokio::select! {
        _ = controller.wait() => { },
        _ = signal::close() => { },
    }

    controller.stop().await?;

    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let result = cli().await;
    if let Err(err) = &result {
        error!("{err}");
        return ExitCode::FAILURE;
    }
    return ExitCode::SUCCESS;
}
