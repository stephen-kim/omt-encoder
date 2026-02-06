use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::settings::AudioSettings;
use crate::send_coordinator::SendCoordinator;

pub struct AudioPipeline {
    settings: AudioSettings,
    send: SendCoordinator,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl AudioPipeline {
    pub fn new(settings: AudioSettings, send: SendCoordinator) -> Self {
        AudioPipeline {
            settings,
            send,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread_handle: None,
        }
    }

    pub fn start(&mut self) {
        if self.settings.mode == "none" {
            println!("Audio pipeline disabled.");
            return;
        }

        self.running.store(true, std::sync::atomic::Ordering::SeqCst);
        let running = self.running.clone();
        let settings = self.settings.clone();
        let send = self.send.clone();

        self.thread_handle = Some(thread::spawn(move || {
            #[cfg(target_os = "linux")]
            linux::run_audio_loop(running, settings, send);

            #[cfg(not(target_os = "linux"))]
            stub::run_audio_loop(running, settings, send);
        }));
    }

    pub fn stop(&mut self) {
        self.running.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod stub {
    use super::*;
    pub fn run_audio_loop(running: Arc<std::sync::atomic::AtomicBool>, _settings: AudioSettings, _send: SendCoordinator) {
        println!("Available on Linux only. Stubbing audio loop for macOS.");
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use alsa::{Direction, ValueOr};
    use alsa::pcm::{PCM, HwParams, Format, Access};
    use bytes::BufMut;
    use libomtnet::{OMTFrame, OMTFrameType};
    use std::fmt;

    #[derive(Clone, Copy, Debug)]
    enum SampleFormat {
        Float32,
        S16,
    }

    impl fmt::Display for SampleFormat {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                SampleFormat::Float32 => write!(f, "f32"),
                SampleFormat::S16 => write!(f, "s16"),
            }
        }
    }

    struct AlsaInput {
        pcm: PCM,
        format: SampleFormat,
    }

    struct AlsaOutput {
        pcm: PCM,
        format: SampleFormat,
    }

    pub fn run_audio_loop(running: Arc<std::sync::atomic::AtomicBool>, settings: AudioSettings, send: SendCoordinator) {
        println!("Starting Linux ALSA pipeline...");

        let pcm_hdmi = if settings.mode == "hdmi" || settings.mode == "both" {
            match open_pcm_capture(
                &settings.hdmi_device,
                settings.sample_rate,
                settings.channels,
                settings.arecord_buffer_usec,
                settings.arecord_period_usec,
            ) {
                Ok(pcm) => Some(pcm),
                Err(e) => {
                    eprintln!("Failed to open HDMI audio device {}: {}", settings.hdmi_device, e);
                    None
                }
            }
        } else {
            None
        };

        let pcm_trs = if settings.mode == "trs" || settings.mode == "both" {
            match open_pcm_capture(
                &settings.trs_device,
                settings.sample_rate,
                settings.channels,
                settings.arecord_buffer_usec,
                settings.arecord_period_usec,
            ) {
                Ok(pcm) => Some(pcm),
                Err(e) => {
                    eprintln!("Failed to open TRS audio device {}: {}", settings.trs_device, e);
                    None
                }
            }
        } else {
            None
        };

        if pcm_hdmi.is_none() && pcm_trs.is_none() {
            println!("No audio inputs available.");
            return;
        }

        let frame_size = settings.samples_per_channel;
        let channels = settings.channels as usize;
        let buffer_size = frame_size * channels;

        let mut hdmi_buf = vec![0.0f32; buffer_size];
        let mut trs_buf = vec![0.0f32; buffer_size];
        let mut mix_buf = vec![0.0f32; buffer_size];
        let mut monitor_buf = vec![0.0f32; buffer_size];
        let mut hdmi_s16: Vec<i16> = Vec::new();
        let mut trs_s16: Vec<i16> = Vec::new();
        let mut monitor_s16: Vec<i16> = Vec::new();

        let pcm_monitor = if settings.monitor.enabled {
            match open_pcm_playback(
                &settings.monitor.device,
                settings.sample_rate,
                settings.channels,
                settings.arecord_buffer_usec,
                settings.arecord_period_usec,
            ) {
                Ok(pcm) => Some(pcm),
                Err(e) => {
                    eprintln!("Failed to open monitor output {}: {}", settings.monitor.device, e);
                    None
                }
            }
        } else {
            None
        };

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let mut hdmi_ok = false;
            let mut trs_ok = false;

            if let Some(pcm) = &pcm_hdmi {
                if read_pcm(pcm, &mut hdmi_buf, frame_size, channels, &mut hdmi_s16).is_ok() {
                    hdmi_ok = true;
                }
            }

            if let Some(pcm) = &pcm_trs {
                if read_pcm(pcm, &mut trs_buf, frame_size, channels, &mut trs_s16).is_ok() {
                    trs_ok = true;
                }
            }

            if !hdmi_ok && !trs_ok {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            for i in 0..buffer_size {
                let mut sample = 0.0;
                if hdmi_ok { sample += hdmi_buf[i]; }
                if trs_ok { sample += trs_buf[i]; }
                sample *= settings.mix_gain;
                if sample > 1.0 { sample = 1.0; }
                if sample < -1.0 { sample = -1.0; }
                mix_buf[i] = sample;
            }

            if let Some(pcm) = &pcm_monitor {
                let gain = settings.monitor.gain;
                for i in 0..buffer_size {
                    let mut sample = mix_buf[i] * gain;
                    if sample > 1.0 { sample = 1.0; }
                    if sample < -1.0 { sample = -1.0; }
                    monitor_buf[i] = sample;
                }
                let _ = write_pcm(pcm, &monitor_buf, frame_size, channels, &mut monitor_s16);
            }

            let mut frame = OMTFrame::new(OMTFrameType::Audio);
            frame.header.timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64 / 100;
            frame.audio_header = Some(libomtnet::OMTAudioHeader {
                codec: libomtnet::OMTCodec::FPA1 as i32,
                sample_rate: settings.sample_rate as i32,
                samples_per_channel: frame_size as i32,
                channels: channels as i32,
                active_channels: active_channel_mask(channels),
                reserved1: 0,
            });

            let mut planar_buf = vec![0.0f32; buffer_size];
            for ch in 0..channels {
                let planar_offset = ch * frame_size;
                for s in 0..frame_size {
                    planar_buf[planar_offset + s] = mix_buf[s * channels + ch];
                }
            }

            let mut byte_data = bytes::BytesMut::with_capacity(buffer_size * 4);
            for sample in &planar_buf {
                byte_data.put_f32_le(*sample);
            }
            frame.data = byte_data.freeze();
            frame.update_data_length();

            send.enqueue_audio(frame);
        }
    }

    fn open_pcm_capture(device: &str, rate: u32, channels: u32, buffer_usec: u32, period_usec: u32) -> Result<AlsaInput, alsa::Error> {
        if let Ok(pcm) = PCM::new(device, Direction::Capture, false) {
            if apply_hw_params(&pcm, rate, channels, buffer_usec, period_usec, SampleFormat::Float32).is_ok() {
                return Ok(AlsaInput { pcm, format: SampleFormat::Float32 });
            }
        }
        let pcm = PCM::new(device, Direction::Capture, false)?;
        apply_hw_params(&pcm, rate, channels, buffer_usec, period_usec, SampleFormat::S16)?;
        Ok(AlsaInput { pcm, format: SampleFormat::S16 })
    }

    fn open_pcm_playback(device: &str, rate: u32, channels: u32, buffer_usec: u32, period_usec: u32) -> Result<AlsaOutput, alsa::Error> {
        if let Ok(pcm) = PCM::new(device, Direction::Playback, false) {
            if apply_hw_params(&pcm, rate, channels, buffer_usec, period_usec, SampleFormat::Float32).is_ok() {
                return Ok(AlsaOutput { pcm, format: SampleFormat::Float32 });
            }
        }
        let pcm = PCM::new(device, Direction::Playback, false)?;
        apply_hw_params(&pcm, rate, channels, buffer_usec, period_usec, SampleFormat::S16)?;
        Ok(AlsaOutput { pcm, format: SampleFormat::S16 })
    }

    fn apply_hw_params(
        pcm: &PCM,
        rate: u32,
        channels: u32,
        buffer_usec: u32,
        period_usec: u32,
        format: SampleFormat,
    ) -> Result<(), alsa::Error> {
        let hwp = HwParams::any(pcm)?;
        hwp.set_channels(channels)?;
        hwp.set_rate(rate, ValueOr::Nearest)?;
        match format {
            SampleFormat::Float32 => hwp.set_format(Format::float())?,
            SampleFormat::S16 => hwp.set_format(Format::s16())?,
        }
        hwp.set_access(Access::RWInterleaved)?;
        if buffer_usec > 0 {
            let _ = hwp.set_buffer_time_near(buffer_usec, ValueOr::Nearest);
        }
        if period_usec > 0 {
            let _ = hwp.set_period_time_near(period_usec, ValueOr::Nearest);
        }
        pcm.hw_params(&hwp)?;
        drop(hwp);
        Ok(())
    }

    fn read_pcm(
        input: &AlsaInput,
        buffer: &mut [f32],
        frames: usize,
        channels: usize,
        scratch_i16: &mut Vec<i16>,
    ) -> Result<usize, alsa::Error> {
        match input.format {
            SampleFormat::Float32 => {
                let io = input.pcm.io_f32()?;
                let count = io.readi(buffer)?;
                Ok(count)
            }
            SampleFormat::S16 => {
                let samples = frames * channels;
                if scratch_i16.len() < samples {
                    scratch_i16.resize(samples, 0);
                }
                let io = input.pcm.io_i16()?;
                let count = io.readi(&mut scratch_i16[..samples])?;
                let read_samples = count * channels;
                for i in 0..read_samples {
                    buffer[i] = scratch_i16[i] as f32 / i16::MAX as f32;
                }
                Ok(count)
            }
        }
    }

    fn write_pcm(
        output: &AlsaOutput,
        buffer: &[f32],
        frames: usize,
        channels: usize,
        scratch_i16: &mut Vec<i16>,
    ) -> Result<usize, alsa::Error> {
        match output.format {
            SampleFormat::Float32 => {
                let io = output.pcm.io_f32()?;
                let count = io.writei(buffer)?;
                Ok(count)
            }
            SampleFormat::S16 => {
                let samples = frames * channels;
                if scratch_i16.len() < samples {
                    scratch_i16.resize(samples, 0);
                }
                for i in 0..samples {
                    let mut sample = buffer[i];
                    if sample > 1.0 { sample = 1.0; }
                    if sample < -1.0 { sample = -1.0; }
                    scratch_i16[i] = (sample * i16::MAX as f32) as i16;
                }
                let io = output.pcm.io_i16()?;
                let count = io.writei(&scratch_i16[..samples])?;
                Ok(count)
            }
        }
    }

    fn active_channel_mask(channels: usize) -> u32 {
        if channels == 0 {
            return 0;
        }
        if channels >= 32 {
            return u32::MAX;
        }
        (1u32 << channels) - 1
    }
}
