use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub name: String,
    pub device_path: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate_n: u32,
    pub frame_rate_d: u32,
    pub codec: String,
    pub use_native_format: bool,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            name: "Video".to_string(),
            device_path: "/dev/video0".to_string(),
            width: 1920,
            height: 1080,
            frame_rate_n: 60,
            frame_rate_d: 1,
            codec: "UYVY".to_string(),
            use_native_format: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorSettings {
    pub enabled: bool,
    pub device: String,
    pub gain: f32,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            device: "default".to_string(),
            gain: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub mode: String, // none | hdmi | trs | both
    pub hdmi_device: String,
    pub trs_device: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub samples_per_channel: usize,
    pub mix_gain: f32,
    pub arecord_buffer_usec: u32,
    pub arecord_period_usec: u32,
    pub restart_after_failed_reads: u32,
    pub restart_cooldown_ms: u64,
    pub monitor: MonitorSettings,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            mode: "both".to_string(),
            hdmi_device: "hw:0,0".to_string(),
            trs_device: "hw:1,0".to_string(),
            sample_rate: 48000,
            channels: 2,
            samples_per_channel: 1024,
            mix_gain: 1.0,
            arecord_buffer_usec: 100000,
            arecord_period_usec: 20000,
            restart_after_failed_reads: 5,
            restart_cooldown_ms: 1000,
            monitor: MonitorSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SendSettings {
    pub audio_queue_capacity: usize,
    pub video_queue_capacity: usize,
    pub force_zero_timestamps: bool,
}

impl Default for SendSettings {
    fn default() -> Self {
        Self {
            audio_queue_capacity: 8,
            video_queue_capacity: 1,
            force_zero_timestamps: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSettings {
    pub enabled: bool,
    pub port: u16,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8080,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewSettings {
    pub enabled: bool,
    pub output_device: String,
    pub output_devices: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_format: String,
}

impl Default for PreviewSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            output_device: "".to_string(),
            output_devices: Vec::new(),
            width: 1920,
            height: 1080,
            fps: 30,
            pixel_format: "rgb565le".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub send: SendSettings,
    pub preview: PreviewSettings,
    pub web: WebSettings,
}

impl Settings {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let settings: Settings = serde_json::from_str(&content)?;
        Ok(settings)
    }

    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings_serialization() {
        let settings = Settings::default();
        let json = serde_json::to_string_pretty(&settings).unwrap();
        
        let deserialized: Settings = serde_json::from_str(&json).unwrap();
        
        assert_eq!(settings.video.width, deserialized.video.width);
        assert_eq!(settings.audio.sample_rate, deserialized.audio.sample_rate);
        assert_eq!(settings.web.port, deserialized.web.port);
    }

    #[test]
    fn test_custom_settings_serialization() {
        let mut settings = Settings::default();
        settings.video.width = 1280;
        settings.video.height = 720;
        settings.audio.mode = "hdmi".to_string();

        let json = serde_json::to_string(&settings).unwrap();
        let deserialized: Settings = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.video.width, 1280);
        assert_eq!(deserialized.video.height, 720);
        assert_eq!(deserialized.audio.mode, "hdmi");
    }
}
