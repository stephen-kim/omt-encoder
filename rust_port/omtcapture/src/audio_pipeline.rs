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
            // Simulate work
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

    pub fn run_audio_loop(running: Arc<std::sync::atomic::AtomicBool>, settings: AudioSettings, send: SendCoordinator) {
        println!("Starting Linux ALSA pipeline...");
        
        let pcm_hdmi = if settings.mode == "hdmi" || settings.mode == "both" {
            open_pcm(
                &settings.hdmi_device,
                settings.sample_rate,
                settings.channels,
                settings.arecord_buffer_usec,
                settings.arecord_period_usec,
            ).ok()
        } else {
            None
        };

        let pcm_trs = if settings.mode == "trs" || settings.mode == "both" {
            open_pcm(
                &settings.trs_device,
                settings.sample_rate,
                settings.channels,
                settings.arecord_buffer_usec,
                settings.arecord_period_usec,
            ).ok()
        } else {
            None
        };
        
        if pcm_hdmi.is_none() && pcm_trs.is_none() {
            println!("No audio inputs available.");
            return;
        }

        let frame_size = settings.samples_per_channel; // Number of frames to read
        let channels = settings.channels as usize;
        let buffer_size = frame_size * channels;
        
        let mut hdmi_buf = vec![0.0f32; buffer_size];
        let mut trs_buf = vec![0.0f32; buffer_size];
        let mut mix_buf = vec![0.0f32; buffer_size];
        let mut monitor_buf = vec![0.0f32; buffer_size];

        let pcm_monitor = if settings.monitor.enabled {
            open_pcm_playback(
                &settings.monitor.device,
                settings.sample_rate,
                settings.channels,
                settings.arecord_buffer_usec,
                settings.arecord_period_usec,
            ).ok()
        } else {
            None
        };

        while running.load(std::sync::atomic::Ordering::SeqCst) {
             let mut hdmi_ok = false;
             let mut trs_ok = false;

             // Read HDMI
             if let Some(pcm) = &pcm_hdmi {
                 if read_pcm(pcm, &mut hdmi_buf, frame_size).is_ok() {
                     hdmi_ok = true;
                 }
             }

             // Read TRS
             if let Some(pcm) = &pcm_trs {
                 if read_pcm(pcm, &mut trs_buf, frame_size).is_ok() {
                     trs_ok = true;
                 }
             }

             if !hdmi_ok && !trs_ok {
                 // Prevent busy loop if both fail
                 std::thread::sleep(Duration::from_millis(10));
                 continue;
             }

             // Mix
             // Simple additive mix with gain
             for i in 0..buffer_size {
                 let mut sample = 0.0;
                 if hdmi_ok { sample += hdmi_buf[i]; }
                 if trs_ok { sample += trs_buf[i]; }
                 sample *= settings.mix_gain;
                 
                 // Clamp
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
                 let _ = write_pcm(pcm, &monitor_buf, frame_size);
             }
             
             // Create Frame
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
                 active_channels: 0x3, // Stereo (Left | Right)
                 reserved1: 0,
             });
             
             // Convert Interleaved mix_buf to Planar format
             let mut planar_buf = vec![0.0f32; buffer_size];
             for ch in 0..channels {
                 let planar_offset = ch * frame_size;
                 for s in 0..frame_size {
                     planar_buf[planar_offset + s] = mix_buf[s * channels + ch];
                 }
             }

             // Convert f32 to bytes (assuming little endian)
             let mut byte_data = bytes::BytesMut::with_capacity(buffer_size * 4);
             for sample in &planar_buf {
                 byte_data.put_f32_le(*sample);
             }
             frame.data = byte_data.freeze();
             frame.update_data_length();

             send.enqueue_audio(frame);
        }
    }

    fn open_pcm(device: &str, rate: u32, channels: u32, buffer_usec: u32, period_usec: u32) -> Result<PCM, alsa::Error> {
        let pcm = PCM::new(device, Direction::Capture, false)?;
        
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(channels)?;
        hwp.set_rate(rate, ValueOr::Nearest)?;
        hwp.set_format(Format::float())?; // Try float first
        hwp.set_access(Access::RWInterleaved)?;
        if buffer_usec > 0 {
            let _ = hwp.set_buffer_time_near(buffer_usec, ValueOr::Nearest);
        }
        if period_usec > 0 {
            let _ = hwp.set_period_time_near(period_usec, ValueOr::Nearest);
        }
        
        pcm.hw_params(&hwp)?;
        Ok(pcm)
    }

    fn open_pcm_playback(device: &str, rate: u32, channels: u32, buffer_usec: u32, period_usec: u32) -> Result<PCM, alsa::Error> {
        let pcm = PCM::new(device, Direction::Playback, false)?;
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(channels)?;
        hwp.set_rate(rate, ValueOr::Nearest)?;
        hwp.set_format(Format::float())?;
        hwp.set_access(Access::RWInterleaved)?;
        if buffer_usec > 0 {
            let _ = hwp.set_buffer_time_near(buffer_usec, ValueOr::Nearest);
        }
        if period_usec > 0 {
            let _ = hwp.set_period_time_near(period_usec, ValueOr::Nearest);
        }
        pcm.hw_params(&hwp)?;
        Ok(pcm)
    }

    fn read_pcm(pcm: &PCM, buf: &mut [f32], frames: usize) -> Result<(), alsa::Error> {
        let io = pcm.io_f32()?;
        match io.readi(buf) {
            Ok(count) => {
                if count != frames {
                    // Short read?
                    // Could be valid end of stream or xrun?
                }
                Ok(())
            },
            Err(e) => {
                // Try recover
                println!("ALSA read error: {}", e);
                pcm.try_recover(e, false)?;
                Ok(())
            }
        }
    }

    fn write_pcm(pcm: &PCM, buf: &[f32], frames: usize) -> Result<(), alsa::Error> {
        let io = pcm.io_f32()?;
        match io.writei(buf) {
            Ok(count) => {
                if count != frames {
                    // Short write; ignore.
                }
                Ok(())
            }
            Err(e) => {
                pcm.try_recover(e, false)?;
                Ok(())
            }
        }
    }
}
