mod audio_pipeline;
mod discovery;
mod send_coordinator;
mod settings;
mod timebase;
mod video_pipeline;
mod web_server;

use anyhow::Result;
use audio_pipeline::AudioPipeline;
use libomtnet::server::OMTServer;
use send_coordinator::SendCoordinator;
use settings::Settings;
use std::path::Path;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::{watch, Mutex, RwLock};
use video_pipeline::VideoPipeline;
use web_server::{start_web_server, WebState};

struct RuntimePipelines {
    audio: AudioPipeline,
    video: VideoPipeline,
    send: SendCoordinator,
    audio_tx: tokio::sync::broadcast::Sender<libomtnet::OMTFrame>,
    settings: Settings,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    println!("Starting OMT Capture (Rust Port)...");

    // Load or initialize settings
    let config_path = "config.json";
    let settings = load_settings_with_xml_fallback(config_path);

    let shared_settings = Arc::new(RwLock::new(settings));
    let (settings_tx, mut settings_rx) = watch::channel(shared_settings.read().await.clone());

    // Server on port 6400 (protocol port)
    let server = OMTServer::new(6400).await?;
    let suggested_quality_hint = server.suggested_quality_hint();
    let active_quality_mask = server.active_quality_mask();
    let active_codec_mask = server.active_codec_mask();

    // Detect supported codecs (VMX1 always, H264/H265 if HW encoder available)
    let supported = detect_supported_codecs();
    server.set_supported_codecs(supported);
    println!("Supported codecs: VMX1{}{}",
        if supported & 2 != 0 { " H264" } else { "" },
        if supported & 4 != 0 { " H265" } else { "" });

    server
        .set_sender_info_xml(Some(build_sender_info_xml(
            &shared_settings.read().await.video.name,
        )))
        .await;
    server.set_tally(false, false).await;
    let tx = server.get_senders();

    let initial_settings = shared_settings.read().await.clone();
    let send = SendCoordinator::new(tx.clone(), initial_settings.send.clone());
    let mut audio = AudioPipeline::new(initial_settings.audio.clone(), tx.audio.clone());
    let audio_levels = audio.levels.clone();
    audio.start();
    let mut video = VideoPipeline::new(
        initial_settings.video.clone(),
        initial_settings.preview.clone(),
        send.clone(),
        suggested_quality_hint.clone(),
        active_quality_mask.clone(),
        active_codec_mask.clone(),
    );
    video.start();
    let video_shared_frame = video.shared_frame.clone();

    let mdns_publisher = Arc::new(Mutex::new(discovery::MdnsPublisher::start(
        &initial_settings.video.name,
        6400,
    )));
    let audio_tx_clone = tx.audio.clone();
    let pipelines = Arc::new(Mutex::new(RuntimePipelines {
        audio,
        video,
        send,
        audio_tx: audio_tx_clone,
        settings: initial_settings,
    }));

    let server = Arc::new(server);

    // Start Web Server
    let web_settings = shared_settings.read().await.web.clone();
    if web_settings.enabled {
        let preview_frame: Arc<tokio::sync::Mutex<Option<bytes::Bytes>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let web_state = WebState {
            settings: shared_settings.clone(),
            settings_tx: settings_tx.clone(),
            config_path: config_path.to_string(),
            server: Some(Arc::clone(&server)),
            audio_levels: Some(audio_levels.clone()),
            preview_frame: preview_frame.clone(),
        };

        // Background: generate preview JPEG snapshots from video pipeline frames
        let pf = preview_frame.clone();
        let sf = video_shared_frame.clone();
        tokio::spawn(async move {
            generate_preview_loop(pf, sf).await;
        });

        tokio::spawn(async move {
            if let Err(e) = start_web_server(web_settings.port, web_state).await {
                eprintln!("Web server error: {}", e);
            }
        });
    }

    let pipelines_for_updates = pipelines.clone();
    let tx_for_updates = tx.clone();
    let server_for_updates = Arc::clone(&server);
    let server_for_updates_task = Arc::clone(&server_for_updates);
    let mdns_for_updates = Arc::clone(&mdns_publisher);
    tokio::spawn(async move {
        loop {
            if settings_rx.changed().await.is_err() {
                break;
            }
            let new_settings = settings_rx.borrow().clone();
            let mut guard = pipelines_for_updates.lock().await;
            let old_name = guard.settings.video.name.clone();

            let audio_changed = audio_settings_changed(&guard.settings, &new_settings);
            let video_changed = video_settings_changed(&guard.settings, &new_settings);
            let send_changed = send_settings_changed(&guard.settings, &new_settings);
            let preview_changed = preview_settings_changed(&guard.settings, &new_settings);

            if send_changed {
                guard.audio.stop();
                guard.video.stop();
                guard.send =
                    SendCoordinator::new(tx_for_updates.clone(), new_settings.send.clone());
                guard.audio = AudioPipeline::new(new_settings.audio.clone(), guard.audio_tx.clone());
                guard.audio.start();
                // Recreate video pipeline only when send settings force us to recreate the
                // coordinator. For regular video/preview updates we use in-place restart flags
                // to avoid making receivers like OBS require a restart.
                guard.video = VideoPipeline::new(
                    new_settings.video.clone(),
                    new_settings.preview.clone(),
                    guard.send.clone(),
                    suggested_quality_hint.clone(),
                    active_quality_mask.clone(),
                    active_codec_mask.clone(),
                );
                guard.video.start();
            } else {
                if audio_changed {
                    guard.audio.stop();
                    guard.audio =
                        AudioPipeline::new(new_settings.audio.clone(), guard.audio_tx.clone());
                    guard.audio.start();
                }
                if video_changed {
                    guard.video.update_video(new_settings.video.clone());
                }
                if preview_changed {
                    guard.video.update_preview(new_settings.preview.clone());
                }
            }

            let name_changed = old_name != new_settings.video.name;
            let new_name = new_settings.video.name.clone();
            guard.settings = new_settings;
            drop(guard);

            if name_changed {
                server_for_updates_task
                    .set_sender_info_xml(Some(build_sender_info_xml(&new_name)))
                    .await;
                let mut mdns = mdns_for_updates.lock().await;
                *mdns = discovery::MdnsPublisher::start(&new_name, 6400);
            }
        }
    });

    println!("OMT Capture is running.");

    // Run server and wait for shutdown
    tokio::select! {
        res = server_for_updates.run() => {
            if let Err(e) = res {
                eprintln!("OMT Server error: {}", e);
            }
        },
        _ = signal::ctrl_c() => {
            println!("Shutting down...");
        }
    }

    {
        let mut guard = pipelines.lock().await;
        guard.audio.stop();
        guard.video.stop();
    }
    // Keep publisher alive for process lifetime, then drop on shutdown.
    drop(mdns_publisher);

    // Save settings on exit
    let final_settings = shared_settings.read().await;
    if let Err(e) = final_settings.save(config_path) {
        eprintln!("Failed to save settings: {}", e);
    }

    Ok(())
}

fn load_settings_with_xml_fallback(config_path: &str) -> Settings {
    if let Ok(settings) = Settings::load(config_path) {
        return settings;
    }

    let xml_path = Path::new(config_path).with_file_name("config.xml");
    if xml_path.exists() {
        match Settings::load_from_xml(&xml_path) {
            Ok(settings) => {
                println!(
                    "Loaded legacy XML config from {} and migrated to {}",
                    xml_path.display(),
                    config_path
                );
                if let Err(e) = settings.save(config_path) {
                    eprintln!("Failed to persist migrated JSON config: {}", e);
                }
                return settings;
            }
            Err(e) => {
                eprintln!(
                    "Failed to parse legacy XML config {}: {}",
                    xml_path.display(),
                    e
                );
            }
        }
    }

    println!("Config file not found or invalid, using defaults.");
    let defaults = Settings::default();
    if let Err(e) = defaults.save(config_path) {
        eprintln!("Failed to write default config: {}", e);
    }
    defaults
}

fn build_sender_info_xml(source_name: &str) -> String {
    let product = if source_name.trim().is_empty() {
        "OMT Capture".to_string()
    } else {
        source_name.trim().to_string()
    };
    format!(
        "<OMTInfo ProductName=\"{}\" Manufacturer=\"OpenMediaTransport\" Version=\"{}\" />",
        xml_escape_attr(&product),
        env!("CARGO_PKG_VERSION")
    )
}

fn xml_escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
}

fn audio_settings_changed(old: &Settings, new: &Settings) -> bool {
    old.audio.mode != new.audio.mode
        || old.audio.hdmi_device != new.audio.hdmi_device
        || old.audio.trs_device != new.audio.trs_device
        || old.audio.sample_rate != new.audio.sample_rate
        || old.audio.channels != new.audio.channels
        || old.audio.samples_per_channel != new.audio.samples_per_channel
        || (old.audio.mix_gain - new.audio.mix_gain).abs() > f32::EPSILON
        || old.audio.arecord_buffer_usec != new.audio.arecord_buffer_usec
        || old.audio.arecord_period_usec != new.audio.arecord_period_usec
        || old.audio.restart_after_failed_reads != new.audio.restart_after_failed_reads
        || old.audio.restart_cooldown_ms != new.audio.restart_cooldown_ms
        || old.audio.monitor.enabled != new.audio.monitor.enabled
        || old.audio.monitor.device != new.audio.monitor.device
        || (old.audio.monitor.gain - new.audio.monitor.gain).abs() > f32::EPSILON
}

fn video_settings_changed(old: &Settings, new: &Settings) -> bool {
    old.video.name != new.video.name
        || old.video.device_path != new.video.device_path
        || old.video.width != new.video.width
        || old.video.height != new.video.height
        || old.video.frame_rate_n != new.video.frame_rate_n
        || old.video.frame_rate_d != new.video.frame_rate_d
        || old.video.codec != new.video.codec
        || old.video.use_native_format != new.video.use_native_format
        || old.video.encoder != new.video.encoder
        || old.video.hw_encoder != new.video.hw_encoder
}

fn send_settings_changed(old: &Settings, new: &Settings) -> bool {
    old.send.audio_queue_capacity != new.send.audio_queue_capacity
        || old.send.video_queue_capacity != new.send.video_queue_capacity
        || old.send.force_zero_timestamps != new.send.force_zero_timestamps
}

fn preview_settings_changed(old: &Settings, new: &Settings) -> bool {
    old.preview.enabled != new.preview.enabled
        || old.preview.output_device != new.preview.output_device
        || old.preview.output_devices != new.preview.output_devices
        || old.preview.outputs != new.preview.outputs
        || old.preview.width != new.preview.width
        || old.preview.height != new.preview.height
        || old.preview.fps != new.preview.fps
        || old.preview.pixel_format != new.preview.pixel_format
}

async fn generate_preview_loop(
    jpeg_out: Arc<tokio::sync::Mutex<Option<bytes::Bytes>>>,
    shared_frame: video_pipeline::SharedPreviewFrame,
) {
    use std::io::Write;

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        // Read latest raw frame from video pipeline
        let (raw, width, height, pix_fmt) = {
            let lock = shared_frame.data.lock().unwrap();
            match lock.as_ref() {
                Some(f) => (f.raw.clone(), f.width, f.height, f.pix_fmt.clone()),
                None => continue,
            }
        };

        // Convert to JPEG via ffmpeg (runs in blocking threadpool)
        let result = tokio::task::spawn_blocking(move || {
            let mut child = std::process::Command::new("ffmpeg")
                .args([
                    "-loglevel", "error",
                    "-f", "rawvideo",
                    "-pix_fmt", &pix_fmt,
                    "-s", &format!("{}x{}", width, height),
                    "-i", "pipe:0",
                    "-frames:v", "1",
                    "-f", "image2",
                    "-vcodec", "mjpeg",
                    "-q:v", "8",
                    "pipe:1",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            child.stdin.as_mut()?.write_all(&raw).ok()?;
            drop(child.stdin.take());
            let output = child.wait_with_output().ok()?;
            if output.status.success() && !output.stdout.is_empty() {
                Some(bytes::Bytes::from(output.stdout))
            } else {
                None
            }
        })
        .await;

        if let Ok(Some(jpeg)) = result {
            *jpeg_out.lock().await = Some(jpeg);
        }
    }
}

fn detect_supported_codecs() -> u8 {
    let mut mask: u8 = 1; // VMX1 always

    // Test each HW encoder by actually trying to encode a tiny frame
    let h264_encoders = ["h264_rkmpp", "h264_nvenc", "h264_qsv", "h264_vaapi", "h264_v4l2m2m"];
    let h265_encoders = ["hevc_rkmpp", "hevc_nvenc", "hevc_qsv", "hevc_vaapi", "hevc_v4l2m2m"];

    for enc in h264_encoders {
        if test_hw_encoder(enc) {
            println!("H.264 HW encoder available: {}", enc);
            mask |= 2;
            break;
        }
    }

    for enc in h265_encoders {
        if test_hw_encoder(enc) {
            println!("H.265 HW encoder available: {}", enc);
            mask |= 4;
            break;
        }
    }

    mask
}

fn test_hw_encoder(encoder: &str) -> bool {
    // Try encoding a single 16x16 black frame to verify the encoder actually works
    use std::io::Write;
    let mut child = match std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error",
            "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", "16x16", "-r", "1",
            "-i", "pipe:0",
            "-c:v", encoder, "-frames:v", "1",
            "-f", "null", "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Feed one 16x16 YUV420 frame (16*16*3/2 = 384 bytes)
    if let Some(ref mut stdin) = child.stdin {
        let _ = stdin.write_all(&[0u8; 384]);
    }
    drop(child.stdin.take());
    matches!(child.wait(), Ok(status) if status.success())
}
