use ev::MouseEvent;
use leptos::*;
use logging::log;
use serde::{Deserialize, Serialize};
use serde_wasm_bindgen::{from_value, to_value};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct StartServerArgs {
    server_name: String,
    latency: u32,
    volume: u32,
    device_name: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SetVolumeArgs {
    volume: u32,
}

#[component]
pub fn App() -> impl IntoView {
    const DEFAULT_VOLUME: i32 = 100;

    const DEFAULT_LATENCY: i32 = 100;
    const MIN_LATENCY: i32 = 50;
    const MAX_LATENCY: i32 = 500;
    const STEP_LATENCY: i32 = 10;

    let (server_name, set_server_name) = create_signal(String::new());
    // let (greet_msg, set_greet_msg) = create_signal(String::new());
    let (volume, set_volume) = create_signal(DEFAULT_VOLUME);
    let (latency, set_latency) = create_signal(DEFAULT_LATENCY);
    let (device_names, set_device_names) = create_signal(Vec::<String>::new());
    let (selected_device_name, set_selected_device_name) = create_signal(String::new());

    spawn_local(async move {
        if let Ok(js_value) = invoke("get_devices", JsValue::null()).await {
            if let Ok(devices) = from_value::<Vec<String>>(js_value) {
                set_device_names.set(devices);
            }
        }
        if let Ok(js_value) = invoke("get_history", JsValue::null()).await {
            if let Ok(devices) = from_value::<StartServerArgs>(js_value) {
                log!("{:?}", devices);
                set_volume.set(devices.volume as i32);
                set_latency.set(devices.latency as i32);
                set_selected_device_name.set(devices.device_name);
                set_server_name.set(devices.server_name);
            }
        }
    });

    on_cleanup(move || {
        spawn_local(async move {
            let new_msg = invoke("stop_server", JsValue::null()).await;
            log!("on_cleanup: {new_msg:?}");
        });
    });

    let set_volume_fun = move |ev| {
        let v = event_target_value(&ev);
        set_volume.set(v.parse().unwrap_or(DEFAULT_VOLUME));
        spawn_local(async move {
            let volume = volume.get_untracked();
            let args = to_value(&SetVolumeArgs {
                volume: volume as u32,
            })
            .unwrap();
            let new_msg = invoke("set_volume", args).await;
            log!("set_volume_fun: {new_msg:?}");
        });
    };

    let start_pressed = move |ev: MouseEvent| {
        ev.prevent_default();
        spawn_local(async move {
            let server_name = server_name.get_untracked();
            let selected_device_name = selected_device_name.get_untracked();
            let volume = volume.get_untracked();
            let latency = latency.get_untracked();

            // let name = server_name.get_untracked();
            if server_name.is_empty() || selected_device_name.is_empty() {
                return;
            }

            let args = to_value(&StartServerArgs {
                server_name: server_name,
                latency: latency as u32,
                volume: volume as u32,
                device_name: selected_device_name,
            })
            .unwrap();
            // let args = to_value(&GreetArgs { name: &name }).unwrap();
            // // Learn more about Tauri commands at https://tauri.app/v1/guides/features/command
            let new_msg = invoke("start_server", args).await;
            log!("start_server: {new_msg:?}");
            // set_greet_msg.set(new_msg);
        });
    };

    let stop_pressed = move |ev: MouseEvent| {
        ev.prevent_default();
        spawn_local(async move {
            // // Learn more about Tauri commands at https://tauri.app/v1/guides/features/command
            let new_msg = invoke("stop_server", JsValue::null()).await;
            log!("stop_server: {new_msg:?}");
            // set_greet_msg.set(new_msg);
        });
    };

    view! {
        <main class="container">
            <div>
                <label
                    for="server-name"
                    class="block mb-2 text-sm font-medium text-gray-900 dark:text-white"
                >
                    Server name
                </label>
                <div class="mt-2">
                    <input
                        type="text"
                        name="server-name"
                        id="server-name"
                        autocomplete="off"
                        placeholder="Enter a name..."
                        class="bg-gray-50 border border-gray-300 text-gray-900 text-sm rounded-lg focus:ring-blue-500 focus:border-blue-500 block w-full p-2.5 dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-white dark:focus:ring-blue-500 dark:focus:border-blue-500"
                        value=move || server_name.get()
                        on:input=move |ev| {
                            set_server_name.set(event_target_value(&ev));
                        }
                    />
                </div>
            </div>
            <div class="mt-4">
                <label
                    for="device-name"
                    class="block mb-2 text-sm font-medium text-gray-900 dark:text-white"
                >
                    Select a device
                </label>
                <select
                    id="device-name"
                    class="bg-gray-50 border border-gray-300 text-gray-900 text-sm rounded-lg focus:ring-blue-500 focus:border-blue-500 block w-full p-2.5 dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-white dark:focus:ring-blue-500 dark:focus:border-blue-500"
                    on:change=move |ev| {
                        let device_name = event_target_value(&ev);
                        set_selected_device_name.set(device_name.clone());
                    }
                    prop:value=move || selected_device_name.get().to_string()
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
            <div class="mt-4 grid grid-cols-10 gap-4">
                <div class="col-span-6">
                    <div class="relative mb-6">
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
                            class="w-full h-2 bg-gray-200 rounded-lg appearance-none cursor-pointer dark:bg-gray-700"
                            // on:change=update_volume
                            on:input=set_volume_fun
                        />
                        // on:input=move |ev| {
                        // let v = event_target_value(&ev);
                        // set_volume.set(v);
                        // }
                        // <span class="text-sm text-gray-500 dark:text-gray-400 absolute start-0 -bottom-6">
                        // {move || volume.get()}%
                        // </span>
                        <span class="text-sm text-gray-500 dark:text-gray-400 absolute start-1/2 -translate-x-1/2 rtl:translate-x-1/2 -bottom-6">
                            {move || format!("Volume {}", volume.get())}%
                        </span>
                    // <span class="text-sm text-gray-500 dark:text-gray-400 absolute end-0 -bottom-6">
                    // 100%
                    // </span>
                    </div>
                </div>
                <div class="col-span-4">
                    <div class="relative flex items-center">
                        <button
                            type="button"
                            id="decrement-button"
                            class="bg-gray-100 dark:bg-gray-700 dark:hover:bg-gray-600 dark:border-gray-600 hover:bg-gray-200 border border-gray-300 rounded-s-lg p-3 h-11 focus:ring-gray-100 dark:focus:ring-gray-700 focus:ring-2 focus:outline-none"
                            on:click=move |_ev| {
                                let val = (latency.get() - STEP_LATENCY).max(MIN_LATENCY);
                                set_latency.set(val);
                            }
                        >
                            <svg
                                class="w-3 h-3 text-gray-900 dark:text-white"
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
                        <input
                            type="text"
                            id="latency"
                            value=move || latency.get()
                            autocomplete="off"
                            class="bg-gray-50 border-x-0 border-gray-300 h-11 font-medium text-center text-gray-900 text-sm focus:ring-blue-500 focus:border-blue-500 block w-full pb-6 dark:bg-gray-700 dark:border-gray-600 dark:placeholder-gray-400 dark:text-white dark:focus:ring-blue-500 dark:focus:border-blue-500"
                            placeholder=""
                            prop:value=move || latency.get()
                            on:change=move |ev| {
                                let val = event_target_value(&ev)
                                    .parse::<i32>()
                                    .map_or(latency.get(), |v| v.max(MIN_LATENCY).min(MAX_LATENCY));
                                set_latency.set(val);
                            }
                            required
                        />
                        <div class="absolute bottom-1 start-1/2 -translate-x-1/2 rtl:translate-x-1/2 flex items-center text-xs text-gray-400 space-x-1 rtl:space-x-reverse">
                            // <svg
                            // class="w-2.5 h-2.5 text-gray-400"
                            // aria-hidden="true"
                            // xmlns="http://www.w3.org/2000/svg"
                            // fill="none"
                            // viewBox="0 0 20 20"
                            // >
                            // <path
                            // stroke="currentColor"
                            // stroke-linecap="round"
                            // stroke-linejoin="round"
                            // stroke-width="2"
                            // d="M3 8v10a1 1 0 0 0 1 1h4v-5a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v5h4a1 1 0 0 0 1-1V8M1 10l9-9 9 9"
                            // />
                            // </svg>
                            <span>{"Latency (ms)"}</span>
                        </div>
                        <button
                            type="button"
                            id="increment-button"
                            data-input-counter-increment="bedrooms-input"
                            class="bg-gray-100 dark:bg-gray-700 dark:hover:bg-gray-600 dark:border-gray-600 hover:bg-gray-200 border border-gray-300 rounded-e-lg p-3 h-11 focus:ring-gray-100 dark:focus:ring-gray-700 focus:ring-2 focus:outline-none"
                            on:click=move |_ev| {
                                let val = (latency.get() + STEP_LATENCY).min(MAX_LATENCY);
                                set_latency.set(val);
                            }
                        >
                            <svg
                                class="w-3 h-3 text-gray-900 dark:text-white"
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
            <div class="mt-6 flex justify-center">
                <div>
                    <button
                        class="text-white bg-blue-700 hover:bg-blue-800 focus:ring-4 focus:ring-blue-300 font-medium rounded-lg text-sm px-5 py-2.5 me-2 mb-2 dark:bg-blue-600 dark:hover:bg-blue-700 focus:outline-none dark:focus:ring-blue-800"
                        on:click=start_pressed
                    >
                        "Start Listening"
                    </button>
                </div>
                <div>
                    <button
                        class="py-2.5 px-5 me-2 mb-2 text-sm font-medium text-gray-900 focus:outline-none bg-white rounded-lg border border-gray-200 hover:bg-gray-100 hover:text-blue-700 focus:z-10 focus:ring-4 focus:ring-gray-100 dark:focus:ring-gray-700 dark:bg-gray-800 dark:text-gray-400 dark:border-gray-600 dark:hover:text-white dark:hover:bg-gray-700"
                        on:click=stop_pressed
                    >
                        "Stop Listening"
                    </button>
                </div>
            </div>
        </main>
    }
}
