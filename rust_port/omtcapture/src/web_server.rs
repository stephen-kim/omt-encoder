use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::process::Command;
use std::fs;
use anyhow::Result;
use tokio::sync::watch;
use crate::settings::Settings;

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    #[serde(rename = "audioInputs")]
    pub audio_inputs: String,
    #[serde(rename = "audioOutputs")]
    pub audio_outputs: String,
    #[serde(rename = "videoDevices")]
    pub video_devices: Vec<String>,
    #[serde(rename = "framebuffers")]
    pub framebuffers: Vec<String>,
    #[serde(rename = "displayMode")]
    pub display_mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FramebufferInfoResponse {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
pub struct WebState {
    pub settings: Arc<RwLock<Settings>>,
    pub settings_tx: watch::Sender<Settings>,
    pub config_path: String,
}

pub async fn start_web_server(port: u16, state: WebState) -> Result<()> {
    let app = Router::new()
        .route("/", get(handle_index))
        .route("/api/config", get(get_config).post(update_config))
        .route("/api/devices", get(get_devices))
        .route("/api/fbinfo", get(get_fb_info))
        .route("/api/status", get(get_status))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Web server listening on port {}", port);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn get_config(State(state): State<WebState>) -> Json<Settings> {
    let settings = state.settings.read().await;
    Json(settings.clone())
}

async fn update_config(
    State(state): State<WebState>,
    Json(new_settings): Json<Settings>,
) -> Json<UpdateResult> {
    {
        let mut settings = state.settings.write().await;
        *settings = new_settings.clone();
    }
    if let Err(e) = new_settings.save(&state.config_path) {
        return Json(UpdateResult {
            ok: false,
            message: format!("Failed to save config: {}", e),
        });
    }
    let _ = state.settings_tx.send(new_settings);
    Json(UpdateResult {
        ok: true,
        message: "Settings updated successfully".to_string(),
    })
}

async fn get_devices() -> Json<DeviceSnapshot> {
    Json(DeviceSnapshot {
        audio_inputs: run_command("arecord", &["-l"]),
        audio_outputs: run_command("aplay", &["-l"]),
        video_devices: list_devices("/dev", "video"),
        framebuffers: list_devices("/dev", "fb"),
        display_mode: get_display_mode(),
    })
}

#[derive(Deserialize)]
struct FbQuery {
    path: String,
}

async fn get_fb_info(Query(query): Query<FbQuery>) -> impl IntoResponse {
    let fb = query.path.split('/').last().unwrap_or_default();
    let mut name = String::new();
    let mut width = 0;
    let mut height = 0;

    if fb.starts_with("fb") {
        let base_path = format!("/sys/class/graphics/{}", fb);
        if let Ok(n) = fs::read_to_string(format!("{}/name", base_path)) {
            name = n.trim().to_string();
        }
        if let Ok(size) = fs::read_to_string(format!("{}/virtual_size", base_path)) {
            let parts: Vec<&str> = size.trim().split(',').collect();
            if parts.len() == 2 {
                width = parts[0].parse().unwrap_or(0);
                height = parts[1].parse().unwrap_or(0);
            }
        }
    }

    Json(FramebufferInfoResponse { name, width, height })
}

async fn get_status() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

fn run_command(cmd: &str, args: &[&str]) -> String {
    let output = Command::new(cmd).args(args).output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => format!("{} failed: {}", cmd, e),
    }
}

fn list_devices(dir: &str, prefix: &str) -> Vec<String> {
    let mut devices = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(prefix) {
                devices.push(format!("{}/{}", dir, name));
            }
        }
    }
    devices.sort();
    devices
}

fn get_display_mode() -> String {
    // Simplified display mode check
    if is_service_active("lightdm") || is_service_active("gdm") {
        "desktop".to_string()
    } else {
        "console".to_string()
    }
}

fn is_service_active(service: &str) -> bool {
    let output = Command::new("systemctl")
        .args(&["is-active", service])
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim() == "active",
        Err(_) => false,
    }
}
