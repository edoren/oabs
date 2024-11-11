use std::{
    net::{SocketAddr, ToSocketAddrs},
    process::ExitCode,
};

use anyhow::{anyhow, Result};
use clap::{ArgAction, Parser};
use dialoguer::{theme::ColorfulTheme, FuzzySelect, Input};
use log::error;
use oabs_cli::history::HistoryFile;
use oabs_lib::{
    client::ClientController,
    common::constants::{DEFAULT_PORT, DEFAULT_SERVER_NAME, MAX_LATENCY, MIN_LATENCY},
};
use tokio::signal;
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

    #[arg(short, long,
          help = format!("Specify the delay between input and output in milliseconds [{MIN_LATENCY}, {MAX_LATENCY}]"),
          value_name = "DELAY_MS", value_parser = clap::value_parser!(u32).range((MIN_LATENCY as i64)..=(MAX_LATENCY as i64)))]
    latency: Option<u32>,

    /// Specify the volume [0, 100]
    #[arg(short, long, value_name = "VOLUME", value_parser = clap::value_parser!(u32).range(0..=100))]
    volume: Option<u32>,

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

    let device_name = {
        #[cfg(not(target_os = "android"))]
        if !opt.default_device {
            let devices_names = ClientController::get_device_names();

            let selection = FuzzySelect::with_theme(&input_theme)
                .with_prompt("Select the output device")
                .default(0)
                .items(&devices_names)
                .interact()?;

            Some(devices_names[selection].clone())
        } else {
            None
        }

        #[cfg(target_os = "android")]
        None
    };

    drop(history_file);
    eprintln!();

    controller.set_latency(latency);
    controller.set_volume(volume);
    if let Some(device_name) = device_name {
        controller.set_device_name(device_name);
    }

    controller.start(server_address).await?;

    #[cfg(not(target_os = "windows"))]
    tokio::select! {
        _ = signal::ctrl_c() => { },
        _ = controller.wait() => { },
    };

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
            _ = controller.wait() => { },
        }
    };

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
