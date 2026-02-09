use crate::settings::Settings;
use anyhow::Result;
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::sync::RwLock;

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

#[derive(Debug, Serialize, Deserialize)]
pub struct FramebufferNameResponse {
    pub name: String,
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
        .route("/api/fbname", get(get_fb_name))
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
    Json(mut new_settings): Json<Settings>,
) -> Json<UpdateResult> {
    let old_settings = {
        let settings = state.settings.read().await;
        settings.clone()
    };

    normalize_audio_mode(&mut new_settings);
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
    let _ = state.settings_tx.send(new_settings.clone());
    let video_changed = !video_equals(&old_settings, &new_settings);
    let web_changed = !web_equals(&old_settings, &new_settings);
    let name_changed = old_settings.video.name != new_settings.video.name;

    Json(UpdateResult {
        ok: true,
        message: build_update_message(video_changed, web_changed, name_changed),
    })
}

fn normalize_audio_mode(settings: &mut Settings) {
    match settings.audio.mode.trim().to_ascii_lowercase().as_str() {
        "hdmi" => settings.audio.trs_device.clear(),
        "trs" => settings.audio.hdmi_device.clear(),
        "none" => {
            settings.audio.hdmi_device.clear();
            settings.audio.trs_device.clear();
        }
        _ => {}
    }
}

fn build_update_message(video_changed: bool, web_changed: bool, name_changed: bool) -> String {
    if web_changed {
        return "Saved. Web port changes require restart.".to_string();
    }
    if video_changed && name_changed {
        return "Saved. Video updated. Source name change requires restart.".to_string();
    }
    "Saved. Changes applied.".to_string()
}

fn video_equals(left: &Settings, right: &Settings) -> bool {
    left.video.name == right.video.name
        && left.video.device_path == right.video.device_path
        && left.video.width == right.video.width
        && left.video.height == right.video.height
        && left.video.frame_rate_n == right.video.frame_rate_n
        && left.video.frame_rate_d == right.video.frame_rate_d
        && left.video.codec == right.video.codec
        && left.video.use_native_format == right.video.use_native_format
}

fn web_equals(left: &Settings, right: &Settings) -> bool {
    left.web.enabled == right.web.enabled && left.web.port == right.web.port
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

    Json(FramebufferInfoResponse {
        name,
        width,
        height,
    })
}

async fn get_fb_name(Query(query): Query<FbQuery>) -> impl IntoResponse {
    let fb = query.path.split('/').last().unwrap_or_default();
    let mut name = String::new();

    if fb.starts_with("fb") {
        let name_path = format!("/sys/class/graphics/{}/name", fb);
        if let Ok(n) = fs::read_to_string(name_path) {
            name = n.trim().to_string();
        }
    }

    Json(FramebufferNameResponse { name })
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
