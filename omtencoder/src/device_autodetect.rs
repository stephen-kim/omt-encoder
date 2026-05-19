use crate::settings::Settings;
use std::path::Path;
use std::process::Command;

pub fn apply_startup_autodetect(settings: &mut Settings) -> bool {
    let mut changed = false;

    if !is_video_capture_device(&settings.video.device_path) {
        if let Some(device) = detect_video_capture_device() {
            println!(
                "Autodetected video input: {} (previous setting: {})",
                device, settings.video.device_path
            );
            settings.video.device_path = device;
            changed = true;
        } else {
            eprintln!(
                "No usable V4L2 video capture device found. Check HDMI RX overlay, USB capture, or camera connection."
            );
        }
    }

    if settings.audio.mode == "hdmi"
        || settings.audio.mode == "both"
        || settings.audio.hdmi_device.trim().is_empty()
    {
        if !is_alsa_capture_device(&settings.audio.hdmi_device) {
            if let Some(device) = detect_hdmi_audio_capture_device() {
                println!(
                    "Autodetected HDMI audio input: {} (previous setting: {})",
                    device, settings.audio.hdmi_device
                );
                settings.audio.hdmi_device = device;
                changed = true;
            } else if settings.audio.mode == "hdmi" {
                eprintln!(
                    "No HDMI ALSA capture device found. Video can still run, but HDMI audio capture may fail."
                );
            }
        }
    }

    changed
}

fn detect_video_capture_device() -> Option<String> {
    let mut devices = list_video_capture_devices();
    devices.sort_by_key(|path| video_device_rank(path));
    devices.into_iter().next()
}

fn list_video_capture_devices() -> Vec<String> {
    let output = Command::new("v4l2-ctl")
        .args(["--list-devices"])
        .output()
        .ok();

    let candidates = if let Some(output) = output {
        parse_v4l2_list_devices(&String::from_utf8_lossy(&output.stdout))
    } else {
        list_dev_video_entries()
    };

    candidates
        .into_iter()
        .filter(|path| is_video_capture_device(path))
        .collect()
}

fn parse_v4l2_list_devices(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("/dev/video"))
        .map(ToOwned::to_owned)
        .collect()
}

fn list_dev_video_entries() -> Vec<String> {
    let mut devices = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("video") {
                devices.push(format!("/dev/{}", name));
            }
        }
    }
    devices.sort();
    devices
}

fn is_video_capture_device(path: &str) -> bool {
    if path.trim().is_empty() || !Path::new(path).exists() {
        return false;
    }
    let Ok(output) = Command::new("v4l2-ctl")
        .args(["--device", path, "--info"])
        .output()
    else {
        return false;
    };
    let info = String::from_utf8_lossy(&output.stdout);
    let Some(devcaps_start) = info.find("Device Caps") else {
        return false;
    };
    let devcaps = &info[devcaps_start..];
    devcaps.contains("Video Capture")
        && !devcaps.contains("Memory-to-Memory")
        && !devcaps.contains("Metadata Capture")
}

fn video_device_rank(path: &str) -> u8 {
    let Ok(output) = Command::new("v4l2-ctl")
        .args(["--device", path, "--info"])
        .output()
    else {
        return 50;
    };
    let info = String::from_utf8_lossy(&output.stdout).to_lowercase();
    if info.contains("rk_hdmirx") || info.contains("hdmirx") || info.contains("hdmi rx") {
        0
    } else if info.contains("uvcvideo") || info.contains("usb") {
        10
    } else {
        20
    }
}

fn detect_hdmi_audio_capture_device() -> Option<String> {
    let output = Command::new("arecord").arg("-l").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fallback = None;

    for line in text.lines() {
        let Some((card, device)) = parse_arecord_card_device(line) else {
            continue;
        };
        let alsa = format!("plughw:{},{}", card, device);
        let lower = line.to_lowercase();
        if lower.contains("hdmiin")
            || lower.contains("hdmi-in")
            || lower.contains("hdmirx")
            || lower.contains("hdmi rx")
        {
            return Some(alsa);
        }
        if fallback.is_none() && lower.contains("hdmi") {
            fallback = Some(alsa);
        }
    }

    fallback
}

fn is_alsa_capture_device(device: &str) -> bool {
    let Some((wanted_card, wanted_device)) = parse_alsa_hw_device(device) else {
        return false;
    };
    let Ok(output) = Command::new("arecord").arg("-l").output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().any(|line| {
        parse_arecord_card_device(line)
            .map(|(card, dev)| card == wanted_card && dev == wanted_device)
            .unwrap_or(false)
    })
}

fn parse_alsa_hw_device(device: &str) -> Option<(u32, u32)> {
    let trimmed = device.trim();
    let rest = trimmed
        .strip_prefix("plughw:")
        .or_else(|| trimmed.strip_prefix("hw:"))?;
    let mut parts = rest.split(',');
    let card = parts.next()?.parse().ok()?;
    let dev = parts.next()?.parse().ok()?;
    Some((card, dev))
}

fn parse_arecord_card_device(line: &str) -> Option<(u32, u32)> {
    let trimmed = line.trim_start();
    let card_start = trimmed.strip_prefix("card ")?;
    let (card_text, after_card) = card_start.split_once(':')?;
    let (_, device_start) = after_card.split_once("device ")?;
    let (device_text, _) = device_start.split_once(':')?;
    Some((
        card_text.trim().parse().ok()?,
        device_text.trim().parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v4l2_paths() {
        let paths = parse_v4l2_list_devices(
            "rk_hdmirx:\n\t/dev/video0\nrga:\n\t/dev/video1\n\t/dev/media0\n",
        );
        assert_eq!(paths, vec!["/dev/video0", "/dev/video1"]);
    }

    #[test]
    fn parses_alsa_devices() {
        let line = "card 0: rockchiphdmiin [rockchip-hdmiin], device 0: rockchip-hdmiin i2s-hifi-0 [rockchip-hdmiin i2s-hifi-0]";
        assert_eq!(parse_arecord_card_device(line), Some((0, 0)));
        assert_eq!(parse_alsa_hw_device("plughw:0,0"), Some((0, 0)));
        assert_eq!(parse_alsa_hw_device("hw:3,1"), Some((3, 1)));
    }
}
