// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use anyhow::{anyhow, Result};
use log::error;
use oabs_lib::{
    client::ClientController,
    common::constants::{DEFAULT_LATENCY, DEFAULT_PORT, DEFAULT_VOLUME},
};
use serde::{Deserialize, Serialize};
use tauri::{path::PathResolver, AppHandle, Manager, Runtime, State};
use tokio::sync::Mutex;
mod history;
use history::HistoryFile;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};

#[derive(Default)]
struct ClientState {
    instance: Option<ClientController>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryData {
    server_name: String,
    device_name: String,
    latency: u32,
    volume: u32,
}

fn get_history_file<R: Runtime>(path_resolver: &PathResolver<R>) -> Result<HistoryFile> {
    let app_config_dir = path_resolver
        .app_config_dir()
        .map_err(|e| anyhow!("Could not get config dir: {e}"))?;
    HistoryFile::new(&app_config_dir.join("history.json"), Some(10), true)
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

type ClientData = Arc<Mutex<ClientState>>;

#[tauri::command]
async fn start_server(
    app_handle: AppHandle,
    data: State<'_, ClientData>,
    server_name: String,
    latency: u32,
    volume: u32,
    device_name: Option<String>,
) -> Result<(), String> {
    let server_address =
        parse_server_name(&server_name).ok_or("Invalid server name".to_string())?;

    let mut unlocked_data = data.lock().await;

    if unlocked_data.instance.is_some() {
        return Err("The server is already running".into());
    }

    if let Ok(mut history_file) = get_history_file(app_handle.path()).map_err(|e| format!("{e}")) {
        history_file.get("server_name").add(server_name);
        if let Some(device_name) = device_name.clone() {
            history_file.get("device_name").add(device_name);
        }
        history_file.get("latency").add(latency.to_string());
        history_file.get("volume").add(volume.to_string());
    }

    let mut controller = ClientController::new();

    controller.set_latency(latency);
    controller.set_volume(volume);
    if let Some(device_name) = device_name {
        controller.set_device_name(device_name);
    }

    controller
        .start(server_address)
        .await
        .map_err(|e| format!("{e}"))?;

    unlocked_data.instance.replace(controller);

    Ok(())
}

#[tauri::command]
async fn stop_server(data: State<'_, ClientData>) -> Result<(), String> {
    if let Some(mut controller) = data.lock().await.instance.take() {
        return controller.stop().await.map_err(|e| format!("{e}"));
    }
    Ok(())
}

#[tauri::command]
async fn set_volume(data: State<'_, ClientData>, volume: u32) -> Result<(), String> {
    if let Some(controller) = &data.lock().await.instance {
        controller.set_volume(volume)
    }
    Ok(())
}

#[tauri::command]
async fn is_running(data: State<'_, ClientData>) -> Result<bool, String> {
    if let Some(controller) = &data.lock().await.instance {
        return Ok(controller.is_running());
    }
    Ok(false)
}

#[tauri::command]
fn get_devices() -> Vec<String> {
    ClientController::get_device_names()
}

#[tauri::command]
async fn get_history(app_handle: AppHandle) -> Result<HistoryData, String> {
    let mut history_file = get_history_file(app_handle.path()).map_err(|e| format!("{e}"))?;
    Ok(HistoryData {
        server_name: history_file
            .get("server_name")
            .get_last()
            .unwrap_or(String::new()),
        device_name: history_file
            .get("device_name")
            .get_last()
            .unwrap_or(String::new()),
        latency: history_file
            .get("latency")
            .get_last()
            .and_then(|val| val.parse().ok())
            .unwrap_or(DEFAULT_LATENCY as u32),
        volume: history_file
            .get("volume")
            .get_last()
            .and_then(|val| val.parse().ok())
            .unwrap_or(DEFAULT_VOLUME as u32),
    })
}

#[cfg(not(target_os = "android"))]
#[derive(Clone, serde::Serialize)]
struct Payload {
    args: Vec<String>,
    cwd: String,
}

fn setup_logging<R: Runtime>(path_resolver: &PathResolver<R>) -> Result<()> {
    let mut layers = Vec::new();

    let default_filter = |filter: LevelFilter| {
        EnvFilter::builder()
            .with_default_directive(filter.into())
            .from_env_lossy()
    };

    let log_dir = path_resolver
        .app_log_dir()
        .map_err(|e| anyhow!("Could not get log dir: {e}"))?;

    let file_appender = RollingFileAppender::builder()
        .max_log_files(7)
        .rotation(Rotation::DAILY)
        .filename_prefix("oabs")
        .filename_suffix("log")
        .build(log_dir.clone())?;

    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(default_filter(LevelFilter::DEBUG))
        .boxed();
    layers.push(file_layer);

    #[cfg(debug_assertions)]
    let default_stdout_level_filter = LevelFilter::DEBUG;
    #[cfg(not(debug_assertions))]
    let default_stdout_level_filter = LevelFilter::INFO;
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_filter(default_filter(default_stdout_level_filter))
        .boxed();
    layers.push(stdout_layer);

    tracing_subscriber::registry().with(layers).init();

    Ok(())
}

pub fn main() -> Result<()> {
    // App

    let client_data = ClientData::default();

    let mut builder = tauri::Builder::default();

    builder = builder
        .setup(|app| {
            // Only include this code on debug builds and desktops
            #[cfg(debug_assertions)]
            if let Some(window) = app.get_webview_window("main") {
                window.open_devtools();
            }

            // Logging
            setup_logging(app.handle().path())?;

            Ok(())
        })
        .manage(client_data.clone())
        .invoke_handler(tauri::generate_handler![
            start_server,
            stop_server,
            get_devices,
            get_history,
            set_volume,
            is_running
        ]);

    #[cfg(not(target_os = "android"))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            println!("{}, {argv:?}, {cwd}", app.package_info().name);
            let _ = tauri::Emitter::emit(app, "single-instance", Payload { args: argv, cwd });
        }));
    }

    builder
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(move |_window, event| match event {
            tauri::WindowEvent::CloseRequested { .. } => {
                if let Some(mut controller) = client_data.blocking_lock().instance.take() {
                    if let Err(err) = tauri::async_runtime::block_on(controller.stop()) {
                        error!("{err}");
                    }
                }
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(err) = main() {
        error!("Application failed with error: {err}");
    }
}
