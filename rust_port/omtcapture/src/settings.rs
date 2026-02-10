use anyhow::Result;
use roxmltree::Document;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

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
            // Match C# defaults
            frame_rate_n: 60000,
            frame_rate_d: 1001,
            codec: "YUY2".to_string(),
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
            // Match C# defaults
            hdmi_device: "hw:3,0".to_string(),
            trs_device: "hw:2,0".to_string(),
            sample_rate: 48000,
            channels: 2,
            samples_per_channel: 480,
            mix_gain: 0.5,
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
            // Match C# sender defaults: lower receiver buffering for "live" use.
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

    pub fn load_from_xml<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let doc = Document::parse(&content)?;
        let root = doc
            .descendants()
            .find(|n| n.has_tag_name("settings"))
            .ok_or_else(|| anyhow::anyhow!("config.xml missing <settings> root"))?;

        let mut out = Settings::default();

        out.video.name = child_text(root, "name")
            .unwrap_or(&out.video.name)
            .to_string();
        out.video.device_path = child_text(root, "devicePath")
            .unwrap_or(&out.video.device_path)
            .to_string();
        out.video.width = child_u32(root, "width").unwrap_or(out.video.width);
        out.video.height = child_u32(root, "height").unwrap_or(out.video.height);
        out.video.frame_rate_n = child_u32(root, "frameRateN").unwrap_or(out.video.frame_rate_n);
        out.video.frame_rate_d = child_u32(root, "frameRateD").unwrap_or(out.video.frame_rate_d);
        out.video.codec = child_text(root, "codec")
            .unwrap_or(&out.video.codec)
            .to_string();
        out.video.use_native_format =
            child_bool(root, "useNativeFormat").unwrap_or(out.video.use_native_format);

        if let Some(audio) = root.children().find(|n| n.has_tag_name("audio")) {
            out.audio.mode = child_text(audio, "mode")
                .unwrap_or(&out.audio.mode)
                .to_string();
            out.audio.hdmi_device = child_text(audio, "hdmiDevice")
                .unwrap_or(&out.audio.hdmi_device)
                .to_string();
            out.audio.trs_device = child_text(audio, "trsDevice")
                .unwrap_or(&out.audio.trs_device)
                .to_string();
            out.audio.sample_rate = child_u32(audio, "sampleRate").unwrap_or(out.audio.sample_rate);
            out.audio.channels = child_u32(audio, "channels").unwrap_or(out.audio.channels);
            out.audio.samples_per_channel =
                child_usize(audio, "samplesPerChannel").unwrap_or(out.audio.samples_per_channel);
            out.audio.mix_gain = child_f32(audio, "mixGain").unwrap_or(out.audio.mix_gain);
            out.audio.arecord_buffer_usec =
                child_u32(audio, "arecordBufferUsec").unwrap_or(out.audio.arecord_buffer_usec);
            out.audio.arecord_period_usec =
                child_u32(audio, "arecordPeriodUsec").unwrap_or(out.audio.arecord_period_usec);
            out.audio.restart_after_failed_reads = child_u32(audio, "restartAfterFailedReads")
                .unwrap_or(out.audio.restart_after_failed_reads);
            out.audio.restart_cooldown_ms =
                child_u64(audio, "restartCooldownMs").unwrap_or(out.audio.restart_cooldown_ms);

            if let Some(monitor) = audio.children().find(|n| n.has_tag_name("monitor")) {
                out.audio.monitor.enabled =
                    child_bool(monitor, "enabled").unwrap_or(out.audio.monitor.enabled);
                out.audio.monitor.device = child_text(monitor, "device")
                    .unwrap_or(&out.audio.monitor.device)
                    .to_string();
                out.audio.monitor.gain =
                    child_f32(monitor, "gain").unwrap_or(out.audio.monitor.gain);
            }
        }

        if let Some(send) = root.children().find(|n| n.has_tag_name("send")) {
            out.send.audio_queue_capacity =
                child_usize(send, "audioQueueCapacity").unwrap_or(out.send.audio_queue_capacity);
            out.send.video_queue_capacity =
                child_usize(send, "videoQueueCapacity").unwrap_or(out.send.video_queue_capacity);
            out.send.force_zero_timestamps =
                child_bool(send, "forceZeroTimestamps").unwrap_or(out.send.force_zero_timestamps);
        }

        if let Some(preview) = root.children().find(|n| n.has_tag_name("preview")) {
            out.preview.enabled = child_bool(preview, "enabled").unwrap_or(out.preview.enabled);
            out.preview.output_devices = preview
                .children()
                .filter(|n| n.has_tag_name("output"))
                .filter_map(|n| n.text())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
            out.preview.output_device = child_text(preview, "output")
                .unwrap_or(&out.preview.output_device)
                .to_string();
            if out.preview.output_devices.is_empty() && !out.preview.output_device.trim().is_empty()
            {
                out.preview
                    .output_devices
                    .push(out.preview.output_device.clone());
            }
            out.preview.width = child_u32(preview, "width").unwrap_or(out.preview.width);
            out.preview.height = child_u32(preview, "height").unwrap_or(out.preview.height);
            out.preview.fps = child_u32(preview, "fps").unwrap_or(out.preview.fps);
            out.preview.pixel_format = child_text(preview, "pixelFormat")
                .unwrap_or(&out.preview.pixel_format)
                .to_string();
        } else {
            // C# preview defaults to current video size.
            out.preview.width = out.video.width;
            out.preview.height = out.video.height;
        }

        if let Some(web) = root.children().find(|n| n.has_tag_name("web")) {
            out.web.enabled = child_bool(web, "enabled").unwrap_or(out.web.enabled);
            out.web.port = child_u16(web, "port").unwrap_or(out.web.port);
        }

        normalize_audio_mode(&mut out.audio);
        Ok(out)
    }
}

fn child_text<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|n| n.has_tag_name(name))
        .and_then(|n| n.text())
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn child_bool(node: roxmltree::Node<'_, '_>, name: &str) -> Option<bool> {
    child_text(node, name).and_then(|v| v.parse::<bool>().ok())
}

fn child_u16(node: roxmltree::Node<'_, '_>, name: &str) -> Option<u16> {
    child_text(node, name).and_then(|v| v.parse::<u16>().ok())
}

fn child_u32(node: roxmltree::Node<'_, '_>, name: &str) -> Option<u32> {
    child_text(node, name).and_then(|v| v.parse::<u32>().ok())
}

fn child_u64(node: roxmltree::Node<'_, '_>, name: &str) -> Option<u64> {
    child_text(node, name).and_then(|v| v.parse::<u64>().ok())
}

fn child_usize(node: roxmltree::Node<'_, '_>, name: &str) -> Option<usize> {
    child_text(node, name).and_then(|v| v.parse::<usize>().ok())
}

fn child_f32(node: roxmltree::Node<'_, '_>, name: &str) -> Option<f32> {
    child_text(node, name).and_then(|v| v.parse::<f32>().ok())
}

fn normalize_audio_mode(audio: &mut AudioSettings) {
    match audio.mode.trim().to_ascii_lowercase().as_str() {
        "hdmi" => audio.trs_device.clear(),
        "trs" => audio.hdmi_device.clear(),
        "none" => {
            audio.hdmi_device.clear();
            audio.trs_device.clear();
        }
        _ => {}
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
