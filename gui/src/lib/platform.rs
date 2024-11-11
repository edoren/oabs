// use ev::MouseEvent;
use leptos::*;
// use logging::log;
// use serde::{Deserialize, Serialize};
// use serde_wasm_bindgen::{from_value, to_value};
// use wasm_bindgen::prelude::*;

// pub struct Platform {}

#[derive(Debug)]
pub struct Platform {
    is_tauri: bool,
    is_desktop: bool,
}

impl Platform {
    fn new() -> Self {
        let window = window();
        let is_tauri = window.get("__TAURI__").is_some();
        let is_desktop = window.navigator().max_touch_points() == 0;

        Self {
            is_tauri,
            is_desktop,
        }
    }

    pub fn is_tauri() -> bool {
        Platform::new().is_tauri
    }

    pub fn is_web() -> bool {
        !Platform::new().is_tauri
    }

    pub fn is_desktop() -> bool {
        Platform::new().is_desktop
    }

    pub fn is_mobile() -> bool {
        !Platform::new().is_desktop
    }
}

// function isMobile() {
//     const regex = /Mobi|Android|webOS|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i;
//     return regex.test(navigator.userAgent);
//   }

//   if (isMobile()) {
//     console.log("Mobile device detected");
//   } else {
//     console.log("Desktop device detected");
//   }
