use ev::MouseEvent;
use leptos::*;
use logging::log;
use oabs_client_ui_lib::{
    platform::Platform,
    tauri_api::dialog::{self, ConfirmDialogOptions, MessageDialogOptions, confirm, message},
};
use serde::{Deserialize, Serialize};
use serde_wasm_bindgen::{from_value, to_value};
use wasm_bindgen::prelude::*;
use web_sys::ConnectionType;

#[wasm_bindgen(js_namespace = ["window", "__TAURI__", "window"])]
extern "C" {
    #[wasm_bindgen]
    fn getCurrentWindow() -> Window;

    #[derive(Debug)]
    type LogicalSize;

    #[wasm_bindgen(constructor)]
    fn new(width: u32, height: u32) -> LogicalSize;

    #[derive(Debug)]
    type Window;

    #[wasm_bindgen(catch, method)]
    async fn setSize(this: &Window, size: LogicalSize) -> Result<JsValue, JsValue>;
}

#[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
extern "C" {
    #[wasm_bindgen(catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct StartServerArgs {
    server_name: String,
    password: String,
    latency: u32,
    volume: u32,
    device_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SetVolumeArgs {
    volume: u32,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ShowDialogArgs {
    message: String,
    kind: String,
}

#[derive(Clone, PartialEq)]
enum PlayingStatus {
    STARTING,
    PLAYING,
    STOPPED,
}

#[component]
pub fn App() -> impl IntoView {
    const DEFAULT_VOLUME: i32 = 100;

    const DEFAULT_LATENCY: i32 = 150;
    const MIN_LATENCY: i32 = 50;
    const MAX_LATENCY: i32 = 500;
    const STEP_LATENCY: i32 = 10;

    let (server_name, set_server_name) = create_signal(String::new());
    let (server_password, set_server_password) = create_signal(String::new());
    let (volume, set_volume) = create_signal(DEFAULT_VOLUME);
    let (latency, set_latency) = create_signal(DEFAULT_LATENCY);
    let (selected_device_name, set_selected_device_name) = create_signal(String::new());

    let (device_names, set_device_names) = create_signal(Vec::<String>::new());
    let (playing_status, set_playing_status) = create_signal(PlayingStatus::STOPPED);

    #[cfg(not(debug_assertions))]
    if Platform::is_desktop() {
        window_event_listener(ev::keydown, |ev| {
            // Prevent F5 or Ctrl+R (Windows/Linux) and Command+R (Mac) from refreshing the page
            if ev.key() == "F5"
                || (ev.ctrl_key() && ev.key() == "r")
                || (ev.meta_key() && ev.key() == "r")
            {
                ev.prevent_default();
            }
        });

        window_event_listener(ev::contextmenu, |ev| {
            ev.prevent_default();
        });
    }

    spawn_local(async move {
        if let Ok(js_value) = invoke("get_devices", JsValue::null()).await {
            if let Ok(mut devices) = from_value::<Vec<String>>(js_value) {
                devices.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
                set_device_names.set(devices);
            }
        }
        if let Ok(js_value) = invoke("get_history", JsValue::null()).await {
            if let Ok(devices) = from_value::<StartServerArgs>(js_value) {
                set_volume.set(devices.volume as i32);
                set_latency.set(devices.latency as i32);
                if let Some(device_name) = devices.device_name {
                    if device_name.is_empty() && !device_names.get_untracked().is_empty() {
                        set_selected_device_name.set(device_names.get_untracked()[0].clone());
                    } else {
                        set_selected_device_name.set(device_name);
                    }
                }
                set_server_name.set(devices.server_name);
            }
        }
        if let Ok(js_value) = invoke("is_running", JsValue::null()).await {
            if let Ok(is_running) = from_value::<bool>(js_value) {
                set_playing_status.set(if is_running {
                    PlayingStatus::PLAYING
                } else {
                    PlayingStatus::STOPPED
                });
            }
        }

        if Platform::is_desktop() {
            if let Some(main_element) = document().get_element_by_id("main_oabs") {
                if let Ok(Some(style)) = window().get_computed_style(&main_element) {
                    if let Ok(margin) = style.get_property_value("margin") {
                        let margin_value: u32 = margin
                            .strip_suffix("px")
                            .and_then(|val| val.parse().ok())
                            .unwrap_or(20);

                        let _ = getCurrentWindow()
                            .setSize(LogicalSize::new(
                                main_element.client_width() as u32 + margin_value * 2,
                                main_element.client_height() as u32 + margin_value * 2,
                            ))
                            .await;
                    }
                }
            }
        }
    });

    // on_cleanup(move || { });

    let set_volume_fun = move |ev| {
        let v = event_target_value(&ev);
        set_volume.set(v.parse().unwrap_or(DEFAULT_VOLUME));
        spawn_local(async move {
            let volume = volume.get_untracked();
            let args = to_value(&SetVolumeArgs {
                volume: volume as u32,
            })
            .unwrap_or_default();
            let _ = invoke("set_volume", args).await;
            // log!("set_volume_fun: {new_msg:?}");
        });
    };

    let start_pressed = move |ev: MouseEvent| {
        ev.prevent_default();
        set_playing_status.set(PlayingStatus::STARTING);

        spawn_local(async move {
            if let Ok(connection) = window().navigator().connection() {
                log!("EDOREN {:?}", connection.type_());
                if connection.type_() == ConnectionType::Cellular {
                    let result = confirm(
                        "You are using a cellullar network, this might incur \
                                 in additional costs, do you want to continue?",
                        Some(ConfirmDialogOptions {
                            title: None,
                            kind: Some(dialog::DialogKind::WARNING),
                            ok_label: None,
                            cancel_label: None,
                        }),
                    )
                    .await;

                    if let Ok(value) = result {
                        if !value {
                            set_playing_status.set(PlayingStatus::STOPPED);
                            return;
                        }
                    }
                }
            }

            let name = server_name.get_untracked();
            let password = server_password.get_untracked();
            let selected_device_name = if Platform::is_mobile() {
                None
            } else {
                Some(selected_device_name.get_untracked())
            };
            let volume = volume.get_untracked();
            let latency = latency.get_untracked();

            let args = to_value(&StartServerArgs {
                server_name: name,
                password: password,
                latency: latency as u32,
                volume: volume as u32,
                device_name: selected_device_name,
            })
            .unwrap_or_default();

            // // Learn more about Tauri commands at https://tauri.app/v1/guides/features/command
            let result = invoke("start_server", args).await;

            log!("start_server: {result:?}");
            if let Err(js_err) = result {
                set_playing_status.set(PlayingStatus::STOPPED);
                if let Some(error_msg) = js_err.as_string() {
                    let result = message(
                        &error_msg,
                        Some(MessageDialogOptions {
                            title: None,
                            kind: Some(dialog::DialogKind::ERROR),
                            ok_label: None,
                        }),
                    )
                    .await;
                    log!("message: {result:?}");
                }
            } else {
                set_playing_status.set(PlayingStatus::PLAYING);
            }
        });
    };

    let stop_pressed = move |ev: MouseEvent| {
        ev.prevent_default();
        spawn_local(async move {
            // // Learn more about Tauri commands at https://tauri.app/v1/guides/features/command
            let result = invoke("stop_server", JsValue::null()).await;
            if result.is_ok() {
                set_playing_status.set(PlayingStatus::STOPPED);
            }
            log!("stop_server: {result:?}");
        });
    };

    view! {
        <main id="main_oabs" class="container m-5">
            <Show when=move || Platform::is_mobile()>
                <label class="flex justify-center text-3xl text-gray-400 dark:text-gray-500">
                    OABS
                </label>
                <label class="flex justify-center text-sm mb-4 text-gray-400 dark:text-gray-500">
                    Open Audio Broadcast Software
                </label>
            </Show>
            <div>
                <label
                    for="server-hostname"
                    class="block mb-2 text-sm font-medium text-gray-900 dark:text-white"
                >
                    Server name
                </label>
                <div class="mt-2">
                    <input
                        type="text"
                        name="server-hostname"
                        id="server-hostname"
                        autocomplete="off"
                        autocapitalize="off"
                        placeholder="Enter the server host name..."
                        class="bg-gray-50 border border-gray-300 text-gray-900 disabled:text-gray-500 text-sm rounded-lg focus:ring-blue-500 focus:border-blue-500 block w-full p-2.5 dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-white disabled:dark:text-gray-400 dark:focus:ring-blue-500 dark:focus:border-blue-500"
                        value=move || server_name.get()
                        on:input=move |ev| {
                            set_server_name.set(event_target_value(&ev));
                        }
                        disabled=move || playing_status.get() != PlayingStatus::STOPPED
                    />
                </div>
            </div>
            <div class="mt-4">
                <label
                    for="server-password"
                    class="block mb-2 text-sm font-medium text-gray-900 dark:text-white"
                >
                    Server password
                </label>
                <div class="mt-2">
                    <input
                        type="password"
                        name="server-password"
                        id="server-password"
                        autocomplete="off"
                        autocapitalize="off"
                        placeholder="Enter the server password..."
                        class="bg-gray-50 border border-gray-300 text-gray-900 disabled:text-gray-500 text-sm rounded-lg focus:ring-blue-500 focus:border-blue-500 block w-full p-2.5 dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-white disabled:dark:text-gray-400 dark:focus:ring-blue-500 dark:focus:border-blue-500"
                        value=move || server_password.get()
                        on:input=move |ev| {
                            set_server_password.set(event_target_value(&ev));
                        }
                        disabled=move || playing_status.get() != PlayingStatus::STOPPED
                    />
                </div>
            </div>
            <Show when=move || Platform::is_desktop()>
                <div class="mt-4">
                    <label
                        for="device-name"
                        class="block mb-2 text-sm font-medium text-gray-900 dark:text-white"
                    >
                        Select a device
                    </label>
                    <select
                        id="device-name"
                        class="bg-gray-50 border border-gray-300 text-gray-900 disabled:text-gray-500 text-sm rounded-lg focus:ring-blue-500 focus:border-blue-500 block w-full p-2.5 dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-white disabled:dark:text-gray-400 dark:focus:ring-blue-500 dark:focus:border-blue-500 opacity-100 appearance-none"
                        on:change=move |ev| {
                            let device_name = event_target_value(&ev);
                            set_selected_device_name.set(device_name.clone());
                        }
                        prop:value=move || selected_device_name.get().to_string()
                        disabled=move || playing_status.get() != PlayingStatus::STOPPED
                    >
                        <For
                            each=move || device_names.get()
                            key=|v| v.clone()
                            children=move |v| {
                                view! { <option value=v.clone()>{v}</option> }
                            }
                        />
                    </select>
                </div>
            </Show>
            <div class="mt-4 grid grid-cols-10 gap-4">
                <div class="col-span-5">
                    <div class="grid grid-cols-1">
                        <div>
                            <label for="volume" class="sr-only">
                                Labels range
                            </label>
                            <input
                                id="volume"
                                type="range"
                                value=move || volume.get()
                                min="0"
                                max="100"
                                autocomplete="off"
                                class="w-full h-2 slider-thumb:bg-blue-700 slider-thumb:hover:bg-blue-800 slider-thumb:dark:bg-blue-600 slider-thumb:dark:hover:bg-blue-700 slider-thumb:appearance-none slider-thumb:w-4 slider-thumb:h-4 slider-thumb:rounded-full bg-gray-100 rounded-lg appearance-none cursor-pointer dark:bg-gray-700"
                                // on:change=update_volume
                                on:input=set_volume_fun
                            />
                        </div>
                        <div class="flex justify-center text-sm text-gray-900 disabled:text-gray-500 dark:text-white disabled:dark:text-gray-400">
                            <span>{move || format!("Volume {}%", volume.get())}</span>
                        </div>
                    </div>
                </div>
                <div class="col-span-5">
                    <div class="relative flex items-center">
                        <button
                            type="button"
                            id="decrement-button"
                            class="bg-gray-100 dark:bg-gray-700 enabled:dark:hover:bg-gray-600 text-gray-900 disabled:text-gray-500 dark:text-white disabled:dark:text-gray-400 dark:border-gray-600 enabled:hover:bg-gray-200 border border-gray-300 rounded-s-lg p-3 h-11 focus:ring-gray-100 dark:focus:ring-gray-700 focus:ring-2 focus:outline-none"
                            on:click=move |_ev| {
                                let val = (latency.get() - STEP_LATENCY).max(MIN_LATENCY);
                                set_latency.set(val);
                            }
                            disabled=move || playing_status.get() != PlayingStatus::STOPPED
                        >
                            <svg
                                class="w-3 h-3"
                                aria-hidden="true"
                                xmlns="http://www.w3.org/2000/svg"
                                fill="none"
                                viewBox="0 0 18 2"
                            >
                                <path
                                    stroke="currentColor"
                                    stroke-linecap="round"
                                    stroke-linejoin="round"
                                    stroke-width="2"
                                    d="M1 1h16"
                                />
                            </svg>
                        </button>
                        <div class="grid grid-cols-1">
                            <input
                                type="number"
                                id="latency"
                                value=move || latency.get()
                                autocomplete="off"
                                class="bg-gray-50 border-x-0 border-gray-300 h-6 font-medium text-center text-gray-900 disabled:text-gray-500 text-sm focus:ring-blue-500 focus:border-blue-500 block w-full dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-white disabled:dark:text-gray-400 dark:focus:ring-blue-500 dark:focus:border-blue-500 spin-button-none"
                                placeholder=""
                                prop:value=move || latency.get()
                                on:change=move |ev| {
                                    let val = event_target_value(&ev)
                                        .parse::<i32>()
                                        .map_or(
                                            latency.get(),
                                            |v| v.max(MIN_LATENCY).min(MAX_LATENCY),
                                        );
                                    set_latency.set(val);
                                }
                                disabled=move || playing_status.get() != PlayingStatus::STOPPED
                            />
                            <div class="bg-gray-50 border-x-0 border-gray-300 h-5 text-center text-gray-800 disabled:text-gray-400 text-xs focus:ring-blue-500 focus:border-blue-500 block w-full dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-gray-400 disabled:dark:text-gray-200 dark:focus:ring-blue-500 dark:focus:border-blue-500">
                                <span>{"Latency (ms)"}</span>
                            </div>
                        </div>
                        <button
                            type="button"
                            id="increment-button"
                            data-input-counter-increment="bedrooms-input"
                            class="bg-gray-100 dark:bg-gray-700 enabled:dark:hover:bg-gray-600 text-gray-900 disabled:text-gray-500 dark:text-white disabled:dark:text-gray-400 dark:border-gray-600 enabled:hover:bg-gray-200 border border-gray-300 rounded-e-lg p-3 h-11 focus:ring-gray-100 dark:focus:ring-gray-700 focus:ring-2 focus:outline-none"
                            on:click=move |_ev| {
                                let val = (latency.get() + STEP_LATENCY).min(MAX_LATENCY);
                                set_latency.set(val);
                            }
                            disabled=move || playing_status.get() != PlayingStatus::STOPPED
                        >
                            <svg
                                class="w-3 h-3"
                                aria-hidden="true"
                                xmlns="http://www.w3.org/2000/svg"
                                fill="none"
                                viewBox="0 0 18 18"
                            >
                                <path
                                    stroke="currentColor"
                                    stroke-linecap="round"
                                    stroke-linejoin="round"
                                    stroke-width="2"
                                    d="M9 1v16M1 9h16"
                                />
                            </svg>
                        </button>
                    </div>
                </div>
            </div>
            <div class="mt-6 grid grid-cols-8">
                <Show when=move || { playing_status.get() == PlayingStatus::PLAYING }>
                    <div class="col-start-1 flex items-center justify-center">
                        <div class="playing_icon">
                            <span />
                            <span />
                            <span />
                        </div>
                    </div>
                </Show>
                <Show when=move || { playing_status.get() == PlayingStatus::STARTING }>
                    <div class="col-start-1 flex items-center justify-center">
                        <span class="loader"></span>
                    </div>
                </Show>
                <Show when=move || { playing_status.get() == PlayingStatus::STOPPED }>
                    <div class="col-start-2 col-end-8 flex items-center justify-center">
                        <button
                            class="px-5 py-2.5 text-sm font-medium border rounded-lg focus:ring-4 focus:z-10 focus:outline-none enabled:text-white enabled:bg-blue-700 enabled:hover:bg-blue-800 enabled:focus:ring-blue-300 enabled:dark:bg-blue-600 enabled:dark:hover:bg-blue-700 enabled:dark:focus:ring-blue-800 disabled:text-gray-500 disabled:bg-white disabled:border-gray-200 disabled:focus:ring-gray-100 disabled:dark:focus:ring-gray-700 disabled:dark:bg-gray-800 disabled:dark:text-gray-400 disabled:dark:border-gray-600"
                            on:click=start_pressed
                        >
                            // disabled=move || playing_status.get() != PlayingStatus::STOPPED
                            "Start Listening"
                        </button>
                    </div>
                </Show>
                <Show when=move || { playing_status.get() != PlayingStatus::STOPPED }>
                    <div class="col-start-2 col-end-8 flex items-center justify-center">
                        <button
                            class="px-5 py-2.5 text-sm font-medium border rounded-lg focus:ring-4 focus:z-10 focus:outline-none enabled:text-white enabled:bg-blue-700 enabled:hover:bg-blue-800 enabled:focus:ring-blue-300 enabled:dark:bg-blue-600 enabled:dark:hover:bg-blue-700 enabled:dark:focus:ring-blue-800 disabled:text-gray-500 disabled:bg-white disabled:border-gray-200 disabled:focus:ring-gray-100 disabled:dark:focus:ring-gray-700 disabled:dark:bg-gray-800 disabled:dark:text-gray-400 disabled:dark:border-gray-600"
                            on:click=stop_pressed
                        >
                            // disabled=move || playing_status.get() == PlayingStatus::STOPPED
                            "Stop Listening"
                        </button>
                    </div>
                </Show>
            </div>
        </main>
    }
}
