use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::send_coordinator::SendCoordinator;
use crate::settings::AudioSettings;

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

        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod stub {
    use super::*;
    pub fn run_audio_loop(
        running: Arc<std::sync::atomic::AtomicBool>,
        _settings: AudioSettings,
        _send: SendCoordinator,
    ) {
        println!("Available on Linux only. Stubbing audio loop for macOS.");
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use alsa::pcm::{Access, Format, HwParams, State, PCM};
    use alsa::{Direction, ValueOr};
    use bytes::BufMut;
    use libomtnet::{OMTFrame, OMTFrameType};
    use std::fmt;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

    pub fn run_audio_loop(
        running: Arc<std::sync::atomic::AtomicBool>,
        settings: AudioSettings,
        send: SendCoordinator,
    ) {
        println!("Starting Linux ALSA pipeline...");
        let mode = settings.mode.trim().to_lowercase();
        let mut expect_hdmi = mode == "hdmi" || mode == "both";
        let mut expect_trs = mode == "trs" || mode == "both";
        if expect_hdmi && settings.hdmi_device.trim().is_empty() {
            println!("Audio pipeline: HDMI device not set; disabling HDMI input.");
            expect_hdmi = false;
        }
        if expect_trs && settings.trs_device.trim().is_empty() {
            println!("Audio pipeline: TRS device not set; disabling TRS input.");
            expect_trs = false;
        }

        let mut restart_last_attempt =
            Instant::now() - Duration::from_millis(settings.restart_cooldown_ms.max(1));
        let mut effective_rate = settings.sample_rate;
        let mut hdmi_channels: usize = 0;
        let mut trs_channels: usize = 0;
        let (mut pcm_hdmi, mut pcm_trs) = match try_start_inputs(
            expect_hdmi,
            expect_trs,
            &settings,
            &mut effective_rate,
            &mut hdmi_channels,
            &mut trs_channels,
        ) {
            Some(v) => v,
            None => {
                println!("Audio pipeline error: No input devices could be started.");
                return;
            }
        };

        if pcm_hdmi.is_none() && pcm_trs.is_none() {
            println!("No audio inputs available.");
            return;
        }

        let frame_size = settings.samples_per_channel;
        let output_channels = 2usize;
        let buffer_size = frame_size * output_channels;
        println!(
            "Audio pipeline started. Rate: {}, Output Channels: {}, HDMI In: {}ch, TRS In: {}ch",
            effective_rate, output_channels, hdmi_channels, trs_channels
        );

        let mut hdmi_buf = vec![0.0f32; frame_size * hdmi_channels.max(1)];
        let mut trs_buf = vec![0.0f32; frame_size * trs_channels.max(1)];
        let mut mix_buf = vec![0.0f32; buffer_size];
        let mut monitor_buf = vec![0.0f32; buffer_size];
        let mut last_mix_buf = vec![0.0f32; buffer_size];
        let mut has_last_mix = false;
        let mut hdmi_s16: Vec<i16> = Vec::new();
        let mut trs_s16: Vec<i16> = Vec::new();
        let mut monitor_s16: Vec<i16> = Vec::new();
        let mut consecutive_read_failures: u32 = 0;
        let mut read_failures_total: u32 = 0;
        let mut read_failures_hdmi: u32 = 0;
        let mut read_failures_trs: u32 = 0;
        let mut last_read_log = Instant::now();
        let mut last_level_log = Instant::now();

        let pcm_monitor = if settings.monitor.enabled {
            match open_pcm_playback(
                &settings.monitor.device,
                effective_rate,
                output_channels as u32,
                settings.arecord_buffer_usec,
                settings.arecord_period_usec,
            ) {
                Ok(pcm) => Some(pcm),
                Err(e) => {
                    eprintln!(
                        "Failed to open monitor output {}: {}",
                        settings.monitor.device, e
                    );
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
                if read_pcm(
                    pcm,
                    &mut hdmi_buf,
                    frame_size,
                    hdmi_channels.max(1),
                    &mut hdmi_s16,
                )
                .is_ok()
                {
                    hdmi_ok = true;
                }
            }

            if let Some(pcm) = &pcm_trs {
                if read_pcm(
                    pcm,
                    &mut trs_buf,
                    frame_size,
                    trs_channels.max(1),
                    &mut trs_s16,
                )
                .is_ok()
                {
                    trs_ok = true;
                }
            }

            if expect_hdmi && !hdmi_ok {
                read_failures_hdmi += 1;
            }
            if expect_trs && !trs_ok {
                read_failures_trs += 1;
            }
            if (expect_hdmi && !hdmi_ok) || (expect_trs && !trs_ok) {
                read_failures_total += 1;
            }

            let all_expected_failed = (expect_hdmi || expect_trs)
                && (!expect_hdmi || !hdmi_ok)
                && (!expect_trs || !trs_ok);

            if all_expected_failed {
                consecutive_read_failures += 1;
                if settings.restart_after_failed_reads > 0
                    && consecutive_read_failures >= settings.restart_after_failed_reads
                    && restart_last_attempt.elapsed().as_millis() as u64
                        >= settings.restart_cooldown_ms
                {
                    println!("Audio pipeline: no input data; attempting restart.");
                    restart_last_attempt = Instant::now();
                    if let Some((new_hdmi, new_trs)) = try_start_inputs(
                        expect_hdmi,
                        expect_trs,
                        &settings,
                        &mut effective_rate,
                        &mut hdmi_channels,
                        &mut trs_channels,
                    ) {
                        pcm_hdmi = new_hdmi;
                        pcm_trs = new_trs;
                        hdmi_buf.resize(frame_size * hdmi_channels.max(1), 0.0);
                        trs_buf.resize(frame_size * trs_channels.max(1), 0.0);
                        println!(
                            "Audio pipeline: restart success. Rate: {}, HDMI In: {}ch, TRS In: {}ch",
                            effective_rate, hdmi_channels, trs_channels
                        );
                        consecutive_read_failures = 0;
                        continue;
                    }
                }

                if has_last_mix {
                    mix_buf.copy_from_slice(&last_mix_buf);
                } else {
                    mix_buf.fill(0.0);
                }
                send_audio_frame(
                    &settings,
                    &send,
                    &mix_buf,
                    frame_size,
                    output_channels,
                    effective_rate,
                );
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            consecutive_read_failures = 0;

            for i in 0..frame_size {
                let mut left = 0.0f32;
                let mut right = 0.0f32;

                if hdmi_ok {
                    if hdmi_channels <= 1 {
                        let s = hdmi_buf[i];
                        left += s;
                        right += s;
                    } else {
                        left += hdmi_buf[i * hdmi_channels];
                        right += hdmi_buf[i * hdmi_channels + 1];
                    }
                }
                if trs_ok {
                    if trs_channels <= 1 {
                        let s = trs_buf[i];
                        left += s;
                        right += s;
                    } else {
                        left += trs_buf[i * trs_channels];
                        right += trs_buf[i * trs_channels + 1];
                    }
                }

                let mut left = left * settings.mix_gain;
                let mut right = right * settings.mix_gain;
                if left > 1.0 {
                    left = 1.0;
                }
                if left < -1.0 {
                    left = -1.0;
                }
                if right > 1.0 {
                    right = 1.0;
                }
                if right < -1.0 {
                    right = -1.0;
                }

                mix_buf[i * 2] = left;
                mix_buf[i * 2 + 1] = right;
            }
            last_mix_buf.copy_from_slice(&mix_buf);
            has_last_mix = true;

            if let Some(pcm) = &pcm_monitor {
                let gain = settings.monitor.gain;
                for i in 0..buffer_size {
                    let mut sample = mix_buf[i] * gain;
                    if sample > 1.0 {
                        sample = 1.0;
                    }
                    if sample < -1.0 {
                        sample = -1.0;
                    }
                    monitor_buf[i] = sample;
                }
                let _ = write_pcm(
                    pcm,
                    &monitor_buf,
                    frame_size,
                    output_channels,
                    &mut monitor_s16,
                );
            }

            if last_level_log.elapsed() >= Duration::from_secs(5) {
                let hdmi_db = if hdmi_ok {
                    calculate_rms_db(&hdmi_buf, frame_size * hdmi_channels.max(1))
                } else {
                    -100.0
                };
                let trs_db = if trs_ok {
                    calculate_rms_db(&trs_buf, frame_size * trs_channels.max(1))
                } else {
                    -100.0
                };
                let mix_db = calculate_rms_db(&mix_buf, buffer_size);
                println!(
                    "Audio Levels (dB) -> HDMI: {:.1} | TRS: {:.1} | Mix: {:.1}",
                    hdmi_db, trs_db, mix_db
                );
                last_level_log = Instant::now();
            }
            if last_read_log.elapsed() >= Duration::from_secs(5) {
                println!(
                    "Audio read failures (last 5s): total={}, hdmi={}, trs={}",
                    read_failures_total, read_failures_hdmi, read_failures_trs
                );
                read_failures_total = 0;
                read_failures_hdmi = 0;
                read_failures_trs = 0;
                last_read_log = Instant::now();
            }

            send_audio_frame(
                &settings,
                &send,
                &mix_buf,
                frame_size,
                output_channels,
                effective_rate,
            );
        }
    }

    fn send_audio_frame(
        settings: &AudioSettings,
        send: &SendCoordinator,
        interleaved: &[f32],
        samples_per_channel: usize,
        channels: usize,
        sample_rate: u32,
    ) {
        let mut frame = OMTFrame::new(OMTFrameType::Audio);
        frame.header.timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64
            / 100;
        frame.audio_header = Some(libomtnet::OMTAudioHeader {
            codec: libomtnet::OMTCodec::FPA1 as i32,
            sample_rate: sample_rate as i32,
            samples_per_channel: samples_per_channel as i32,
            channels: channels as i32,
            active_channels: active_channel_mask(channels),
            reserved1: 0,
        });

        let mut planar_buf = vec![0.0f32; interleaved.len()];
        for ch in 0..channels {
            let planar_offset = ch * samples_per_channel;
            for s in 0..samples_per_channel {
                planar_buf[planar_offset + s] = interleaved[s * channels + ch];
            }
        }

        let mut byte_data = bytes::BytesMut::with_capacity(planar_buf.len() * 4);
        for sample in &planar_buf {
            byte_data.put_f32_le(*sample);
        }
        frame.data = byte_data.freeze();
        frame.update_data_length();
        let _ = settings;
        send.enqueue_audio(frame);
    }

    fn calculate_rms_db(buffer: &[f32], count: usize) -> f64 {
        if count == 0 {
            return -100.0;
        }
        let mut sum = 0.0f64;
        for s in &buffer[..count.min(buffer.len())] {
            let v = *s as f64;
            sum += v * v;
        }
        let rms = (sum / count as f64).sqrt();
        20.0 * (rms + 1e-9).log10()
    }

    fn build_device_candidates(device: &str) -> Vec<String> {
        let mut candidates = vec![device.to_string()];
        if let Some(suffix) = device.strip_prefix("hw:") {
            candidates.push(format!("plughw:{suffix}"));
            candidates.push(format!("plug:hw:{suffix}"));
        }
        candidates
    }

    fn try_start_inputs(
        use_hdmi: bool,
        use_trs: bool,
        settings: &AudioSettings,
        effective_rate: &mut u32,
        hdmi_channels: &mut usize,
        trs_channels: &mut usize,
    ) -> Option<(Option<AlsaInput>, Option<AlsaInput>)> {
        let mut rate_candidates = vec![settings.sample_rate, 48_000, 44_100];
        rate_candidates.dedup();
        let mut channel_candidates = vec![settings.channels.max(1), 2, 1];
        channel_candidates.dedup();

        for rate in rate_candidates {
            let mut hdmi_input: Option<AlsaInput> = None;
            let mut trs_input: Option<AlsaInput> = None;
            let mut opened_hdmi_channels = 0usize;
            let mut opened_trs_channels = 0usize;

            if use_hdmi {
                for device in build_device_candidates(&settings.hdmi_device) {
                    for ch in &channel_candidates {
                        if let Ok(input) = open_pcm_capture(
                            &device,
                            rate,
                            *ch,
                            settings.arecord_buffer_usec,
                            settings.arecord_period_usec,
                        ) {
                            println!(
                                "Started audio input on {}. Rate: {}, Channels: {}, Format: {}",
                                device, rate, ch, input.format
                            );
                            opened_hdmi_channels = *ch as usize;
                            hdmi_input = Some(input);
                            break;
                        }
                    }
                    if hdmi_input.is_some() {
                        break;
                    }
                }
            }

            if use_trs {
                for device in build_device_candidates(&settings.trs_device) {
                    for ch in &channel_candidates {
                        if let Ok(input) = open_pcm_capture(
                            &device,
                            rate,
                            *ch,
                            settings.arecord_buffer_usec,
                            settings.arecord_period_usec,
                        ) {
                            println!(
                                "Started audio input on {}. Rate: {}, Channels: {}, Format: {}",
                                device, rate, ch, input.format
                            );
                            opened_trs_channels = *ch as usize;
                            trs_input = Some(input);
                            break;
                        }
                    }
                    if trs_input.is_some() {
                        break;
                    }
                }
            }

            // Both requested and both opened -> success
            if (!use_hdmi || hdmi_input.is_some()) && (!use_trs || trs_input.is_some()) {
                *effective_rate = rate;
                *hdmi_channels = if use_hdmi { opened_hdmi_channels } else { 0 };
                *trs_channels = if use_trs { opened_trs_channels } else { 0 };
                return Some((hdmi_input, trs_input));
            }

            // Fallback to single input if mode==both
            if use_hdmi && use_trs {
                if hdmi_input.is_some() {
                    println!("Audio pipeline: TRS input unavailable; using HDMI only.");
                    *effective_rate = rate;
                    *hdmi_channels = opened_hdmi_channels;
                    *trs_channels = 0;
                    return Some((hdmi_input, None));
                }
                if trs_input.is_some() {
                    println!("Audio pipeline: HDMI input unavailable; using TRS only.");
                    *effective_rate = rate;
                    *hdmi_channels = 0;
                    *trs_channels = opened_trs_channels;
                    return Some((None, trs_input));
                }
            }
        }
        None
    }

    fn open_pcm_capture(
        device: &str,
        rate: u32,
        channels: u32,
        buffer_usec: u32,
        period_usec: u32,
    ) -> Result<AlsaInput, alsa::Error> {
        if let Ok(pcm) = PCM::new(device, Direction::Capture, false) {
            if apply_hw_params(
                &pcm,
                rate,
                channels,
                buffer_usec,
                period_usec,
                SampleFormat::Float32,
            )
            .is_ok()
            {
                return Ok(AlsaInput {
                    pcm,
                    format: SampleFormat::Float32,
                });
            }
        }
        let pcm = PCM::new(device, Direction::Capture, false)?;
        apply_hw_params(
            &pcm,
            rate,
            channels,
            buffer_usec,
            period_usec,
            SampleFormat::S16,
        )?;
        Ok(AlsaInput {
            pcm,
            format: SampleFormat::S16,
        })
    }

    fn open_pcm_playback(
        device: &str,
        rate: u32,
        channels: u32,
        buffer_usec: u32,
        period_usec: u32,
    ) -> Result<AlsaOutput, alsa::Error> {
        if let Ok(pcm) = PCM::new(device, Direction::Playback, false) {
            if apply_hw_params(
                &pcm,
                rate,
                channels,
                buffer_usec,
                period_usec,
                SampleFormat::Float32,
            )
            .is_ok()
            {
                return Ok(AlsaOutput {
                    pcm,
                    format: SampleFormat::Float32,
                });
            }
        }
        let pcm = PCM::new(device, Direction::Playback, false)?;
        apply_hw_params(
            &pcm,
            rate,
            channels,
            buffer_usec,
            period_usec,
            SampleFormat::S16,
        )?;
        Ok(AlsaOutput {
            pcm,
            format: SampleFormat::S16,
        })
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
                match io.readi(buffer) {
                    Ok(count) => Ok(count),
                    Err(e) => {
                        if matches!(input.pcm.state(), State::XRun | State::Suspended) {
                            let _ = input.pcm.prepare();
                        }
                        Err(e)
                    }
                }
            }
            SampleFormat::S16 => {
                let samples = frames * channels;
                if scratch_i16.len() < samples {
                    scratch_i16.resize(samples, 0);
                }
                let io = input.pcm.io_i16()?;
                let count = match io.readi(&mut scratch_i16[..samples]) {
                    Ok(v) => v,
                    Err(e) => {
                        if matches!(input.pcm.state(), State::XRun | State::Suspended) {
                            let _ = input.pcm.prepare();
                        }
                        return Err(e);
                    }
                };
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
                    if sample > 1.0 {
                        sample = 1.0;
                    }
                    if sample < -1.0 {
                        sample = -1.0;
                    }
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
