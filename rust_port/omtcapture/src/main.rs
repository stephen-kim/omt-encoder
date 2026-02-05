mod audio_pipeline;
mod video_pipeline;
mod settings;
mod web_server;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::signal;
use audio_pipeline::AudioPipeline;
use video_pipeline::VideoPipeline;
use settings::Settings;
use web_server::{WebState, start_web_server};
use libomtnet::server::OMTServer;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    println!("Starting OMT Capture (Rust Port)...");

    // Load or initialize settings
    let config_path = "config.json";
    let settings = Settings::load(config_path).unwrap_or_else(|_| {
        println!("Config file not found or invalid, using defaults.");
        Settings::default()
    });
    
    let shared_settings = Arc::new(RwLock::new(settings));

    // Server on port 6400 (protocol port)
    let server = OMTServer::new(6400).await?;
    let tx = server.get_sender();

    // Start Audio Pipeline
    let audio_settings = shared_settings.read().await.audio.clone();
    let mut audio = AudioPipeline::new(audio_settings, tx.clone());
    audio.start();

    // Start Video Pipeline
    let video_settings = shared_settings.read().await.video.clone();
    let mut video = VideoPipeline::new(video_settings, tx);
    video.start();

    // Start Web Server
    let web_port = shared_settings.read().await.web.port;
    let web_state = WebState {
        settings: shared_settings.clone(),
    };
    
    let _web_server_handle = tokio::spawn(async move {
        if let Err(e) = start_web_server(web_port, web_state).await {
            eprintln!("Web server error: {}", e);
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

    // Stop pipelines
    audio.stop();
    video.stop();
    
    // Save settings on exit
    let final_settings = shared_settings.read().await;
    if let Err(e) = final_settings.save(config_path) {
        eprintln!("Failed to save settings: {}", e);
    }

    Ok(())
}
