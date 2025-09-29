use std::process::ExitCode;

use anyhow::{Ok, Result, anyhow};
use clap::{ArgAction, Parser, ValueEnum};
use dialoguer::{FuzzySelect, theme::ColorfulTheme};
use log::error;
use oabs_cli::history::HistoryFile;
use oabs_lib::{common::constants::DEFAULT_PORT, server::ServerController};
use serde::{Deserialize, Serialize};
use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    EnvFilter, Layer, filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt,
};

#[derive(
    PartialEq,
    Eq,
    Hash,
    ValueEnum,
    Debug,
    Clone,
    EnumIter,
    EnumString,
    IntoStaticStr,
    Serialize,
    Deserialize,
)]
#[strum(serialize_all = "title_case")]
pub enum StreamQuality {
    LOWEST,
    LOW,
    MEDIUM,
    HIGH,
    HIGHEST,
}

#[derive(Parser, Debug)]
#[command(version, about = "Server", long_about = None)]
struct Opt {
    /// The port name to use
    #[arg(short, long, value_name = "PORT", default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "QUALITY", value_enum)]
    quality: Option<StreamQuality>,

    /// The maximum amount of supported clients
    #[arg(short, long, value_name = "MAX_CLIENTS", default_value_t = 5)]
    max_connections: u16,

    /// The device to capture audio from
    #[arg(short, long, value_name = "DEVICE_NAME")]
    device_name: Option<String>,

    /// Include output devices. Some output devices may not be supported and fail.
    #[arg(long, action = ArgAction::SetTrue)]
    include_output_devices: bool,

    /// The device to capture audio from
    #[arg(short, long, action = ArgAction::SetTrue)]
    list_devices: bool,
}

async fn main_wrapper() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return Err(anyhow!("This program is not supported on Android"));
    }

    let opt = Opt::parse();

    let app_config_dir = dirs::config_dir()
        .ok_or(anyhow!("Could not get config dir"))?
        .join("me.edoren.oabs-server");

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

    let timer = time::format_description::parse(
        "[year]-[month padding:zero]-[day padding:zero] [hour]:[minute]:[second]",
    )?;
    let time_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let timer = tracing_subscriber::fmt::time::OffsetTime::new(time_offset, timer);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_timer(timer)
        .with_filter(default_filter(LevelFilter::DEBUG))
        .boxed();

    let mut layers = Vec::new();
    layers.push(file_layer);
    layers.push(stdout_layer);
    tracing_subscriber::registry().with(layers).init();

    // App

    let mut controller = ServerController::new();
    controller.set_should_include_output_devices(opt.include_output_devices);
    let mut device_names = controller.get_device_names();
    device_names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));

    if opt.list_devices {
        device_names.into_iter().for_each(|dn| println!("{dn}"));
        return Ok(());
    }

    eprintln!(" ██████╗  █████╗ ██████╗ ███████╗");
    eprintln!("██╔═══██╗██╔══██╗██╔══██╗██╔════╝");
    eprintln!("██║   ██║███████║██████╔╝███████╗");
    eprintln!("██║   ██║██╔══██║██╔══██╗╚════██║");
    eprintln!("╚██████╔╝██║  ██║██████╔╝███████║");
    eprintln!(" ╚═════╝ ╚═╝  ╚═╝╚═════╝ ╚══════╝");
    eprintln!("[ Open Audio Broadcast Software ]");
    eprintln!();

    // CLI UI

    let mut history_file = HistoryFile::new(&app_config_dir.join("history.json"), Some(10), true)?;

    let mut additional_space = false;

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
        {
            if let Some(last_device) = device_history.get_last() {
                if let Some(index) = device_names.iter().position(|d| d == &last_device) {
                    device_names.swap(0, index);
                }
            }

            additional_space = true;
            let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
                .with_prompt("Select the device to capture")
                .default(0)
                .items(&device_names)
                .interact()?;

            Some(device_names[selection].clone())
        }

        #[cfg(target_os = "android")]
        None
    };

    if let Some(device_name) = device_name_opt {
        device_history.add(device_name.clone());
        controller.set_device(device_name);
    }

    let quality_value = {
        let qualities: Vec<StreamQuality> = StreamQuality::iter().collect();

        let quality_history = history_file.get("quality");
        let quality = if let Some(quality) = opt.quality {
            quality
        } else {
            #[cfg(not(target_os = "android"))]
            {
                let default: usize = if let Some(last_quality) = quality_history
                    .get_last()
                    .and_then(|q| q.parse::<StreamQuality>().ok())
                {
                    qualities
                        .iter()
                        .position(|d| d == &last_quality)
                        .unwrap_or(0)
                } else {
                    qualities.len() / 2
                };

                let qualities_names: Vec<&str> = qualities.iter().map(|e| e.into()).collect();
                let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
                    .with_prompt("Select the stream quality")
                    .default(default)
                    .items(&qualities_names)
                    .interact()?;
                additional_space = true;
                qualities[selection].clone()
            }
            #[cfg(target_os = "android")]
            {
                qualities[qualities.len() / 2].clone()
            }
        };
        quality_history.add(Into::<&str>::into(&quality).to_string());

        let quality_val_min = -0.1;
        let quality_val_max = 1.0;

        quality_val_min
            + (quality_val_max - quality_val_min) / (qualities.len() - 1) as f32
                * ((quality as u32) as f32)
    };

    drop(history_file);
    if additional_space {
        eprintln!();
    }

    controller.start(DEFAULT_PORT, quality_value).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let result = main_wrapper().await;
    if let Err(err) = &result {
        error!("{err}");
        return ExitCode::FAILURE;
    }
    return ExitCode::SUCCESS;
}
