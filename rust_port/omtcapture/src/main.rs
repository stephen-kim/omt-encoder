mod audio_pipeline;
mod video_pipeline;
mod settings;
mod web_server;
mod send_coordinator;
mod discovery;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{RwLock, Mutex, watch};
use tokio::signal;
use audio_pipeline::AudioPipeline;
use video_pipeline::VideoPipeline;
use settings::Settings;
use web_server::{WebState, start_web_server};
use libomtnet::server::OMTServer;
use send_coordinator::SendCoordinator;

struct RuntimePipelines {
    audio: AudioPipeline,
    video: VideoPipeline,
    send: SendCoordinator,
    settings: Settings,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    println!("Starting OMT Capture (Rust Port)...");

    // Load or initialize settings
    let config_path = "config.json";
    let settings = Settings::load(config_path).unwrap_or_else(|_| {
        println!("Config file not found or invalid, using defaults.");
        let defaults = Settings::default();
        if let Err(e) = defaults.save(config_path) {
            eprintln!("Failed to write default config: {}", e);
        }
        defaults
    });
    
    let shared_settings = Arc::new(RwLock::new(settings));
    let (settings_tx, mut settings_rx) = watch::channel(shared_settings.read().await.clone());

    // Server on port 6400 (protocol port)
    let server = OMTServer::new(6400).await?;
    let tx = server.get_sender();

    let initial_settings = shared_settings.read().await.clone();
    let send = SendCoordinator::new(tx.clone(), initial_settings.send.clone());
    let mut audio = AudioPipeline::new(initial_settings.audio.clone(), send.clone());
    audio.start();
    let mut video = VideoPipeline::new(initial_settings.video.clone(), initial_settings.preview.clone(), send.clone());
    video.start();

    let _mdns_publisher = discovery::MdnsPublisher::start(&initial_settings.video.name, 6400);
    let pipelines = Arc::new(Mutex::new(RuntimePipelines {
        audio,
        video,
        send,
        settings: initial_settings,
    }));

    // Start Web Server
    let web_port = shared_settings.read().await.web.port;
    let web_state = WebState {
        settings: shared_settings.clone(),
        settings_tx: settings_tx.clone(),
        config_path: config_path.to_string(),
    };
    
    let _web_server_handle = tokio::spawn(async move {
        if let Err(e) = start_web_server(web_port, web_state).await {
            eprintln!("Web server error: {}", e);
        }
    });

    let pipelines_for_updates = pipelines.clone();
    let tx_for_updates = tx.clone();
    tokio::spawn(async move {
        loop {
            if settings_rx.changed().await.is_err() {
                break;
            }
            let new_settings = settings_rx.borrow().clone();
            let mut guard = pipelines_for_updates.lock().await;

            let audio_changed = audio_settings_changed(&guard.settings, &new_settings);
            let video_changed = video_settings_changed(&guard.settings, &new_settings);
            let send_changed = send_settings_changed(&guard.settings, &new_settings);
            let preview_changed = preview_settings_changed(&guard.settings, &new_settings);

            if send_changed {
                guard.audio.stop();
                guard.video.stop();
                guard.send = SendCoordinator::new(tx_for_updates.clone(), new_settings.send.clone());
                guard.audio = AudioPipeline::new(new_settings.audio.clone(), guard.send.clone());
                guard.audio.start();
                guard.video = VideoPipeline::new(new_settings.video.clone(), new_settings.preview.clone(), guard.send.clone());
                guard.video.start();
            } else {
                if audio_changed {
                    guard.audio.stop();
                    guard.audio = AudioPipeline::new(new_settings.audio.clone(), guard.send.clone());
                    guard.audio.start();
                }
                if video_changed || preview_changed {
                    guard.video.stop();
                    guard.video = VideoPipeline::new(new_settings.video.clone(), new_settings.preview.clone(), guard.send.clone());
                    guard.video.start();
                }
            }

            guard.settings = new_settings;
        }
    });

    println!("OMT Capture is running.");

    // Run server and wait for shutdown
    tokio::select! {
        res = server.run() => {
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
    
    // Save settings on exit
    let final_settings = shared_settings.read().await;
    if let Err(e) = final_settings.save(config_path) {
        eprintln!("Failed to save settings: {}", e);
    }

    Ok(())
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
        || old.preview.width != new.preview.width
        || old.preview.height != new.preview.height
        || old.preview.fps != new.preview.fps
        || old.preview.pixel_format != new.preview.pixel_format
}
