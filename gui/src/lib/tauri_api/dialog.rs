use serde::{Deserialize, Serialize};
use serde_wasm_bindgen::to_value;
use wasm_bindgen::prelude::*;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DialogKind {
    INFO,
    WARNING,
    ERROR,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DialogFilter {
    /** Filter name. */
    pub name: String,
    /**
     * Extensions to filter, without a `.` prefix.
     * @example
     * ```rs
     * extensions: ['svg', 'png']
     * ```
     */
    pub extensions: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OpenDialogOptions {
    /** The title of the dialog window. */
    pub title: Option<String>,
    /** The filters of the dialog. */
    pub filters: Option<Vec<DialogFilter>>,
    /** Initial directory or file path. */
    pub default_path: Option<String>,
    /** Whether the dialog allows multiple selection or not. */
    pub multiple: Option<bool>,
    /** Whether the dialog is a directory selection or not. */
    pub directory: Option<bool>,
    /**
     * If `directory` is true, indicates that it will be read recursively later.
     * Defines whether subdirectories will be allowed on the scope or not.
     */
    pub recursive: Option<bool>,
    /** Whether to allow creating directories in the dialog. Enabled by default. macOS Only */
    pub can_create_directories: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SaveDialogOptions {
    /** The title of the dialog window. */
    pub title: Option<String>,
    /** The filters of the dialog. */
    pub filters: Option<Vec<DialogFilter>>,
    /**
     * Initial directory or file path.
     * If it's a directory path, the dialog pub struct will change to that folder.
     * If it's not an existing directory, the file name will be set to the dialog's file name input and the dialog will be set to the parent folder.
     */
    pub default_path: Option<String>,
    /** Whether to allow creating directories in the dialog. Enabled by default. macOS Only */
    pub can_create_directories: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MessageDialogOptions {
    /** The title of the dialog. Defaults to the app name. */
    pub title: Option<String>,
    /** The type of the dialog. Defaults to `DialogType::INFO`. */
    pub kind: Option<DialogKind>,
    /** The label of the confirm button. */
    pub ok_label: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmDialogOptions {
    /** The title of the dialog. Defaults to the app name. */
    pub title: Option<String>,
    /** The type of the dialog. Defaults to `info`. */
    pub kind: Option<DialogKind>,
    /** The label of the confirm button. */
    pub ok_label: Option<String>,
    /** The label of the cancel button. */
    pub cancel_label: Option<String>,
}

#[wasm_bindgen(js_namespace = ["window", "__TAURI__", "dialog"])]
extern "C" {
    #[wasm_bindgen(catch, js_name = "ask")]
    async fn askInternal(message: &str, options: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_name = "confirm")]
    async fn confirmInternal(message: &str, options: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_name = "message")]
    async fn messageInternal(message: &str, options: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_name = "open")]
    async fn openInternal(options: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_name = "save")]
    async fn saveInternal(options: JsValue) -> Result<JsValue, JsValue>;
}

pub async fn ask(message: &str, options: Option<ConfirmDialogOptions>) -> Result<bool, String> {
    askInternal(message, to_value(&options).unwrap())
        .await
        .map(|val| val.as_bool().unwrap_or(false))
        .map_err(|err| err.as_string().unwrap_or_default())
}

pub async fn confirm(message: &str, options: Option<ConfirmDialogOptions>) -> Result<bool, String> {
    confirmInternal(message, to_value(&options).unwrap())
        .await
        .map(|val| val.as_bool().unwrap_or(false))
        .map_err(|err| err.as_string().unwrap_or_default())
}

pub async fn message(message: &str, options: Option<MessageDialogOptions>) -> Result<bool, String> {
    messageInternal(message, to_value(&options).unwrap())
        .await
        .map(|val| val.as_bool().unwrap_or(false))
        .map_err(|err| err.as_string().unwrap_or_default())
}

pub async fn open(options: OpenDialogOptions) -> Result<JsValue, String> {
    openInternal(to_value(&options).unwrap())
        .await
        .map(|val| val)
        .map_err(|err| err.as_string().unwrap_or_default())
}

pub async fn save(options: SaveDialogOptions) -> Result<Option<String>, String> {
    saveInternal(to_value(&options).unwrap())
        .await
        .map(|val| val.as_string())
        .map_err(|err| err.as_string().unwrap_or_default())
}
