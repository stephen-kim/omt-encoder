use crate::settings::Settings;
use anyhow::Result;
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    pub server: Option<Arc<libomtnet::server::OMTServer>>,
}

pub async fn start_web_server(port: u16, state: WebState) -> Result<()> {
    let app = Router::new()
        .route("/", get(handle_index))
        .route("/api/config", get(get_config).post(update_config))
        .route("/api/devices", get(get_devices))
        .route("/api/fbname", get(get_fb_name))
        .route("/api/fbinfo", get(get_fb_info))
        .route("/api/status", get(get_status))
        .route("/api/stats", get(get_stats))
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
    Json(payload): Json<Value>,
) -> Json<UpdateResult> {
    let old_settings = {
        let settings = state.settings.read().await;
        settings.clone()
    };

    let has_force_zero_timestamps = payload.pointer("/send/force_zero_timestamps").is_some();

    let mut new_settings: Settings = match serde_json::from_value(payload) {
        Ok(v) => v,
        Err(e) => {
            return Json(UpdateResult {
                ok: false,
                message: format!("Invalid config payload: {}", e),
            })
        }
    };

    normalize_audio_mode(&mut new_settings);
    // Defensive merge: the browser UI can submit empty strings for selects/inputs
    // (e.g. when toggling checkboxes disables sections). Avoid nuking critical fields
    // and accidentally stopping unrelated pipelines.
    merge_empty_fields(&old_settings, &mut new_settings);
    if !has_force_zero_timestamps {
        new_settings.send.force_zero_timestamps = old_settings.send.force_zero_timestamps;
    }
    clamp_send_settings(&mut new_settings);
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
    // The UI already maps checkbox selection -> {mode, hdmi_device, trs_device}.
    // Do NOT clear devices here; clearing can accidentally nuke inputs when toggling UI controls
    // (and can cause capture to "go dead" for receivers like OBS).
    let mode = settings.audio.mode.trim().to_ascii_lowercase();
    settings.audio.mode = match mode.as_str() {
        "none" | "hdmi" | "trs" | "both" => mode,
        _ => "both".to_string(),
    };
}

fn clamp_send_settings(settings: &mut Settings) {
    // Keep values in a sane range even if UI submits empty/0.
    settings.send.audio_queue_capacity = settings.send.audio_queue_capacity.clamp(1, 16);
    settings.send.video_queue_capacity = settings.send.video_queue_capacity.clamp(1, 8);
}

fn merge_empty_fields(old: &Settings, new: &mut Settings) {
    // Video: never allow empty device path/codec/name via UI glitches.
    if new.video.device_path.trim().is_empty() {
        new.video.device_path = old.video.device_path.clone();
    }
    if new.video.codec.trim().is_empty() {
        new.video.codec = old.video.codec.clone();
    }
    if new.video.name.trim().is_empty() {
        new.video.name = old.video.name.clone();
    }
    if new.video.width == 0 {
        new.video.width = old.video.width;
    }
    if new.video.height == 0 {
        new.video.height = old.video.height;
    }
    if new.video.frame_rate_n == 0 {
        new.video.frame_rate_n = old.video.frame_rate_n;
    }
    if new.video.frame_rate_d == 0 {
        new.video.frame_rate_d = old.video.frame_rate_d;
    }

    // Preview: keep lists stable if UI omits them.
    if new.preview.output_devices.is_empty() && !old.preview.output_devices.is_empty() {
        new.preview.output_devices = old.preview.output_devices.clone();
    }
    if new.preview.outputs.is_empty() && !old.preview.outputs.is_empty() {
        new.preview.outputs = old.preview.outputs.clone();
    }
    if new.preview.fps == 0 {
        new.preview.fps = old.preview.fps;
    }
    if new.preview.pixel_format.trim().is_empty() {
        new.preview.pixel_format = old.preview.pixel_format.clone();
    }

    // Audio: allow intentional disable via mode=none, but otherwise keep device strings.
    let mode = new.audio.mode.trim().to_ascii_lowercase();
    if mode != "none" {
        if (mode == "hdmi" || mode == "both") && new.audio.hdmi_device.trim().is_empty() {
            new.audio.hdmi_device = old.audio.hdmi_device.clone();
        }
        if (mode == "trs" || mode == "both") && new.audio.trs_device.trim().is_empty() {
            new.audio.trs_device = old.audio.trs_device.clone();
        }
    }
    if new.audio.sample_rate == 0 {
        new.audio.sample_rate = old.audio.sample_rate;
    }
    if new.audio.channels == 0 {
        new.audio.channels = old.audio.channels;
    }
    if new.audio.samples_per_channel == 0 {
        new.audio.samples_per_channel = old.audio.samples_per_channel;
    }
    // mix_gain is f32; if UI sends NaN, reset to old.
    if !new.audio.mix_gain.is_finite() {
        new.audio.mix_gain = old.audio.mix_gain;
    }
    if new.audio.arecord_buffer_usec == 0 {
        new.audio.arecord_buffer_usec = old.audio.arecord_buffer_usec;
    }
    if new.audio.arecord_period_usec == 0 {
        new.audio.arecord_period_usec = old.audio.arecord_period_usec;
    }

    // Send: if UI omits these or sends 0 while toggling unrelated controls, keep previous values.
    if new.send.audio_queue_capacity == 0 {
        new.send.audio_queue_capacity = old.send.audio_queue_capacity;
    }
    if new.send.video_queue_capacity == 0 {
        new.send.video_queue_capacity = old.send.video_queue_capacity;
    }

    // Monitor settings
    if new.audio.monitor.device.trim().is_empty() {
        new.audio.monitor.device = old.audio.monitor.device.clone();
    }
    if !new.audio.monitor.gain.is_finite() {
        new.audio.monitor.gain = old.audio.monitor.gain;
    }

    // Web: never allow port 0 from empty inputs.
    if new.web.port == 0 {
        new.web.port = old.web.port;
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
            name = friendly_fb_name(n.trim());
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
            name = friendly_fb_name(n.trim());
        }
    }

    Json(FramebufferNameResponse { name })
}

async fn get_status() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

async fn get_stats(State(state): State<WebState>) -> impl IntoResponse {
    let cpu = get_cpu_usage();
    let mem = get_mem_usage();
    let video_format = get_video_format();
    let connections = if let Some(ref server) = state.server {
        server.get_conn_info().await
    } else {
        vec![]
    };
    Json(serde_json::json!({
        "cpu": cpu,
        "mem": mem,
        "connections": connections,
        "videoFormat": video_format,
    }))
}

fn get_cpu_usage() -> String {
    // Read /proc/stat for overall CPU usage
    if let Ok(stat) = fs::read_to_string("/proc/stat") {
        if let Some(line) = stat.lines().next() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                let user: u64 = parts[1].parse().unwrap_or(0);
                let nice: u64 = parts[2].parse().unwrap_or(0);
                let system: u64 = parts[3].parse().unwrap_or(0);
                let idle: u64 = parts[4].parse().unwrap_or(0);
                let total = user + nice + system + idle;
                if total > 0 {
                    let used = user + nice + system;
                    return format!("{}%", used * 100 / total);
                }
            }
        }
    }
    "N/A".to_string()
}

fn get_mem_usage() -> String {
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        let mut total: u64 = 0;
        let mut available: u64 = 0;
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                total = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            } else if line.starts_with("MemAvailable:") {
                available = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            }
        }
        if total > 0 {
            let used = total - available;
            return format!("{} / {} MB", used / 1024, total / 1024);
        }
    }
    "N/A".to_string()
}

fn get_video_format() -> String {
    // Read current config to get active video format
    if let Ok(config) = fs::read_to_string("/opt/omtcapture-rs/config.json") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&config) {
            let w = v["video"]["width"].as_u64().unwrap_or(0);
            let h = v["video"]["height"].as_u64().unwrap_or(0);
            let codec = v["video"]["codec"].as_str().unwrap_or("?");
            let fps_n = v["video"]["frame_rate_n"].as_u64().unwrap_or(0);
            let fps_d = v["video"]["frame_rate_d"].as_u64().unwrap_or(1);
            let native = v["video"]["use_native_format"].as_bool().unwrap_or(false);
            let fps = if fps_d > 0 { fps_n as f64 / fps_d as f64 } else { 0.0 };
            if native {
                return format!("Native ({:.1} fps)", fps);
            }
            return format!("{}x{} {} {:.1} fps", w, h, codec, fps);
        }
    }
    "Unknown".to_string()
}

fn run_command(cmd: &str, args: &[&str]) -> String {
    let output = Command::new(cmd).args(args).output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => format!("{} failed: {}", cmd, e),
    }
}

fn friendly_fb_name(driver: &str) -> String {
    let lower = driver.to_lowercase();
    // HDMI outputs
    if lower.contains("drm") || lower.contains("hdmi") {
        return format!("HDMI ({})", driver);
    }
    // SPI LCD panels
    if lower.contains("ili9341") { return "SPI LCD (ILI9341 320x240)".to_string(); }
    if lower.contains("ili9486") { return "SPI LCD (ILI9486 480x320)".to_string(); }
    if lower.contains("ili9488") { return "SPI LCD (ILI9488 480x320)".to_string(); }
    if lower.contains("st7789") { return "SPI LCD (ST7789 240x240)".to_string(); }
    if lower.contains("st7735") { return "SPI LCD (ST7735 160x128)".to_string(); }
    if lower.contains("ssd1306") { return "OLED (SSD1306)".to_string(); }
    if lower.contains("hx8357") { return "SPI LCD (HX8357 480x320)".to_string(); }
    // DSI displays
    if lower.contains("dsi") { return format!("DSI Display ({})", driver); }
    // Fallback
    driver.to_string()
}

fn list_devices(dir: &str, prefix: &str) -> Vec<String> {
    if prefix == "video" {
        // Filter to real V4L2 capture devices with friendly names
        return list_video_capture_devices();
    }
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

fn list_video_capture_devices() -> Vec<String> {
    // Parse v4l2-ctl --list-devices to get device names, then check each
    // /dev/videoN for actual Video Capture capability.
    let output = match Command::new("v4l2-ctl").args(["--list-devices"]).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return list_all_dev_entries("/dev", "video"),
    };

    // Parse: device name lines are unindented, /dev paths are tab-indented
    let mut current_name = String::new();
    let mut candidates: Vec<(String, String)> = Vec::new(); // (path, name)
    for line in output.lines() {
        if !line.starts_with('\t') && !line.is_empty() {
            current_name = line.trim_end_matches(':').to_string();
        } else {
            let trimmed = line.trim();
            if trimmed.starts_with("/dev/video") {
                candidates.push((trimmed.to_string(), current_name.clone()));
            }
        }
    }

    // Check each candidate's Device Caps (not overall Capabilities) for Video Capture.
    // video1 on Cam Link is Metadata Capture only — must be filtered out.
    let mut devices = Vec::new();
    for (path, name) in candidates {
        if let Ok(caps) = Command::new("v4l2-ctl")
            .args(["--device", &path, "--info"])
            .output()
        {
            let info = String::from_utf8_lossy(&caps.stdout);
            // Extract only the Device Caps section (after "Device Caps")
            if let Some(devcaps_start) = info.find("Device Caps") {
                let devcaps = &info[devcaps_start..];
                if devcaps.contains("Video Capture")
                    && !devcaps.contains("Multiplanar")
                    && !devcaps.contains("Metadata Capture")
                {
                    devices.push(format!("{} ({})", path, name));
                }
            }
        }
    }

    if devices.is_empty() {
        return list_all_dev_entries("/dev", "video");
    }
    devices
}

fn list_all_dev_entries(dir: &str, prefix: &str) -> Vec<String> {
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
