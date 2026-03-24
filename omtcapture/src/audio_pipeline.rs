use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::settings::AudioSettings;
use libomtnet::OMTFrame;
use tokio::sync::broadcast;

pub struct AudioPipeline {
    settings: AudioSettings,
    audio_tx: broadcast::Sender<OMTFrame>,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl AudioPipeline {
    pub fn new(settings: AudioSettings, audio_tx: broadcast::Sender<OMTFrame>) -> Self {
        AudioPipeline {
            settings,
            audio_tx,
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
        let audio_tx = self.audio_tx.clone();

        self.thread_handle = Some(thread::spawn(move || {
            // Set real-time scheduling for the audio thread to prevent glitches
            // from CPU contention with video encode/preview.
            #[cfg(target_os = "linux")]
            unsafe {
                let param = libc::sched_param { sched_priority: 50 };
                if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
                    // Fallback: at least raise nice priority.
                    libc::setpriority(libc::PRIO_PROCESS, 0, -15);
                }
            }

            #[cfg(target_os = "linux")]
            linux::run_audio_loop(running, settings, audio_tx);

            #[cfg(not(target_os = "linux"))]
            stub::run_audio_loop(running, settings, audio_tx);
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
        _audio_tx: broadcast::Sender<OMTFrame>,
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
    use std::io::Write;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::time::Instant;

    fn is_eagain(e: &alsa::Error) -> bool {
        e.errno() == nix::errno::Errno::EAGAIN
    }

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
        nonblock: bool,
    }

    struct AlsaOutput {
        pcm: PCM,
        format: SampleFormat,
    }

    struct AplayOutput {
        child: Child,
        stdin: ChildStdin,
    }

    impl Drop for AplayOutput {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    enum MonitorOutput {
        Alsa(AlsaOutput),
        Aplay(AplayOutput),
    }

    pub fn run_audio_loop(
        running: Arc<std::sync::atomic::AtomicBool>,
        settings: AudioSettings,
        audio_tx: broadcast::Sender<OMTFrame>,
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

        // IMPORTANT: Never exit the audio thread just because inputs are missing/invalid.
        // If audio stops entirely, receivers like OBS may "go dead" and require manual restarts.
        // Instead, keep sending silence and periodically retry opening the inputs.
        let mut restart_last_attempt =
            Instant::now() - Duration::from_millis(settings.restart_cooldown_ms.max(1));
        let mut effective_rate = settings.sample_rate.max(1);
        let mut hdmi_channels: usize = 0;
        let mut trs_channels: usize = 0;
        let mut active_hdmi = false;
        let mut active_trs = false;

        let mut pcm_hdmi: Option<AlsaInput> = None;
        let mut pcm_trs: Option<AlsaInput> = None;
        if let Some((h, t)) = try_start_inputs(
            expect_hdmi,
            expect_trs,
            &settings,
            &mut effective_rate,
            &mut hdmi_channels,
            &mut trs_channels,
        ) {
            pcm_hdmi = h;
            pcm_trs = t;
            active_hdmi = pcm_hdmi.is_some();
            active_trs = pcm_trs.is_some();
        } else {
            println!("Audio pipeline: no inputs started yet; sending silence until inputs become available.");
        }

        let frame_size = settings.samples_per_channel.max(1);
        let output_channels = 2usize;
        let buffer_size = frame_size * output_channels;
        println!(
            "Audio pipeline running. Rate: {}, Output Channels: {}, HDMI In: {}ch, TRS In: {}ch",
            effective_rate, output_channels, hdmi_channels, trs_channels
        );

        let mut hdmi_buf = vec![0.0f32; frame_size * hdmi_channels.max(1)];
        let mut trs_buf = vec![0.0f32; frame_size * trs_channels.max(1)];
        let mut mix_buf = vec![0.0f32; buffer_size];
        let mut hdmi_s16: Vec<i16> = Vec::new();
        let mut trs_s16: Vec<i16> = Vec::new();
        // Reuse send buffers to avoid per-frame heap allocations (important on Pi/OrangePi).
        let mut planar_scratch = vec![0.0f32; buffer_size];
        let mut packed_scratch: Vec<f32> = Vec::with_capacity(buffer_size);
        let mut wire_scratch: Vec<u8> = Vec::with_capacity(buffer_size * 4);
        let mut consecutive_read_failures: u32 = 0;
        let mut read_failures_total: u32 = 0;
        let mut read_failures_hdmi: u32 = 0;
        let mut read_failures_trs: u32 = 0;
        let mut last_read_log = Instant::now();
        let mut last_level_log = Instant::now();

        // Monitor output runs on a separate thread to prevent blocking the capture loop.
        // If the monitor can't keep up, frames are silently dropped (best-effort).
        let monitor_tx: Option<std::sync::mpsc::SyncSender<Vec<f32>>> = if settings.monitor.enabled
        {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
            let mon_settings = settings.clone();
            let mon_running = running.clone();
            thread::spawn(move || {
                run_monitor_thread(mon_running, mon_settings, effective_rate, output_channels, rx);
            });
            Some(tx)
        } else {
            None
        };

        let mut loop_start = Instant::now();
        let mut frame_counter: u64 = 0;
        let mut xrun_count: u64 = 0;
        let mut slow_read_count: u64 = 0;
        let mut slow_send_count: u64 = 0;
        let mut last_diag_log = Instant::now();
        let mut consecutive_silence: u32 = 0;
        let mut silence_events: u64 = 0;
        let mut audio_timestamp: i64 = 0;

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            // Diagnostics: log timing anomalies (stderr to avoid stdout lock contention).
            let iter_elapsed = loop_start.elapsed();
            if iter_elapsed.as_millis() > 20 && frame_counter > 10 {
                eprintln!(
                    "AUDIO DIAG: loop iteration took {}ms (frame {})",
                    iter_elapsed.as_millis(),
                    frame_counter
                );
            }
            loop_start = Instant::now();
            frame_counter += 1;

            if last_diag_log.elapsed().as_secs() >= 10 {
                if xrun_count > 0 || slow_read_count > 0 || slow_send_count > 0 || silence_events > 0 {
                    eprintln!(
                        "AUDIO DIAG 10s: xruns={}, slow_reads={}, slow_sends={}, silence_events={}",
                        xrun_count, slow_read_count, slow_send_count, silence_events
                    );
                }
                xrun_count = 0;
                slow_read_count = 0;
                slow_send_count = 0;
                silence_events = 0;
                last_diag_log = Instant::now();
            }

            // If no inputs are currently active, send silence and retry device start periodically.
            if pcm_hdmi.is_none() && pcm_trs.is_none() {
                mix_buf.fill(0.0);
                if let Some(ref tx) = monitor_tx {
                    let gain = settings.monitor.gain;
                    let mon: Vec<f32> = mix_buf
                        .iter()
                        .map(|s| (s * gain).clamp(-1.0, 1.0))
                        .collect();
                    let _ = tx.try_send(mon);
                }
                send_audio_frame(
                    &settings,
                    &audio_tx,
                    &mix_buf,
                    frame_size,
                    output_channels,
                    effective_rate,
                    &mut planar_scratch,
                    &mut packed_scratch,
                    &mut wire_scratch,
                    &mut audio_timestamp,
                );

                if restart_last_attempt.elapsed()
                    >= Duration::from_millis(settings.restart_cooldown_ms.max(1))
                {
                    restart_last_attempt = Instant::now();
                    let mut new_rate = effective_rate;
                    let mut new_hdmi_ch = hdmi_channels;
                    let mut new_trs_ch = trs_channels;
                    if let Some((h, t)) = try_start_inputs(
                        expect_hdmi,
                        expect_trs,
                        &settings,
                        &mut new_rate,
                        &mut new_hdmi_ch,
                        &mut new_trs_ch,
                    ) {
                        pcm_hdmi = h;
                        pcm_trs = t;
                        active_hdmi = pcm_hdmi.is_some();
                        active_trs = pcm_trs.is_some();
                        effective_rate = new_rate;
                        hdmi_channels = new_hdmi_ch;
                        trs_channels = new_trs_ch;
                        println!(
                            "Audio inputs started. Rate: {}, HDMI In: {}ch, TRS In: {}ch",
                            effective_rate, hdmi_channels, trs_channels
                        );
                    }
                }

                thread::sleep(Duration::from_millis(
                    (frame_size as u64 * 1000 / effective_rate as u64).max(1),
                ));
                continue;
            }
            // Read from ALSA sources. Returns: GotData / NoDataYet (EAGAIN) / Error
            let read_start = Instant::now();
            let hdmi_result = if let Some(pcm) = &pcm_hdmi {
                read_pcm(pcm, &mut hdmi_buf, frame_size, hdmi_channels.max(1), &mut hdmi_s16)
            } else {
                Ok(0)
            };
            let trs_result = if let Some(pcm) = &pcm_trs {
                read_pcm(pcm, &mut trs_buf, frame_size, trs_channels.max(1), &mut trs_s16)
            } else {
                Ok(0)
            };
            let read_elapsed = read_start.elapsed();
            if read_elapsed.as_millis() > 20 {
                slow_read_count += 1;
                eprintln!(
                    "AUDIO DIAG: read_pcm took {}ms (frame {})",
                    read_elapsed.as_millis(),
                    frame_counter
                );
            }

            let hdmi_ok = hdmi_result.as_ref().map(|n| *n > 0).unwrap_or(false);
            let trs_ok = trs_result.as_ref().map(|n| *n > 0).unwrap_or(false);
            let hdmi_eagain = hdmi_result.as_ref().map(|n| *n == 0).unwrap_or(false);
            let trs_eagain = trs_result.as_ref().map(|n| *n == 0).unwrap_or(false);
            let hdmi_error = hdmi_result.is_err();
            let trs_error = trs_result.is_err();

            // If all active sources returned EAGAIN (no data ready yet), just wait briefly
            // and retry. Do NOT send repeated/stale frames — that causes extreme stuttering.
            let all_eagain = (active_hdmi || active_trs)
                && (!active_hdmi || hdmi_eagain || hdmi_ok)
                && (!active_trs || trs_eagain || trs_ok)
                && !hdmi_ok && !trs_ok
                && !hdmi_error && !trs_error;

            if all_eagain {
                // No data from any source (wait already happened inside read_pcm).
                continue;
            }

            // Track actual read errors (not EAGAIN).
            if active_hdmi && hdmi_error {
                read_failures_hdmi += 1;
            }
            if active_trs && trs_error {
                read_failures_trs += 1;
            }
            if (active_hdmi && hdmi_error) || (active_trs && trs_error) {
                read_failures_total += 1;
            }

            let all_expected_failed = (active_hdmi || active_trs)
                && (!active_hdmi || (!hdmi_ok && !hdmi_eagain))
                && (!active_trs || (!trs_ok && !trs_eagain));

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
                        active_hdmi = pcm_hdmi.is_some();
                        active_trs = pcm_trs.is_some();
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

                // Real error: send silence to keep stream alive.
                mix_buf.fill(0.0);
                send_audio_frame(
                    &settings,
                    &audio_tx,
                    &mix_buf,
                    frame_size,
                    output_channels,
                    effective_rate,
                    &mut planar_scratch,
                    &mut packed_scratch,
                    &mut wire_scratch,
                    &mut audio_timestamp,
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

            if let Some(ref tx) = monitor_tx {
                let gain = settings.monitor.gain;
                let mon: Vec<f32> = mix_buf
                    .iter()
                    .map(|s| (s * gain).clamp(-1.0, 1.0))
                    .collect();
                let _ = tx.try_send(mon);
            }

            // Detect silence in outgoing audio to distinguish data vs transport issues.
            let peak = mix_buf.iter().fold(0.0f32, |a, s| a.max(s.abs()));
            if peak < 1e-6 {
                consecutive_silence += 1;
            } else {
                if consecutive_silence >= 3 {
                    silence_events += 1;
                    eprintln!(
                        "AUDIO DIAG: {} consecutive silence frames ended at frame {} ({}ms gap)",
                        consecutive_silence,
                        frame_counter,
                        consecutive_silence as u64 * frame_size as u64 * 1000 / effective_rate as u64
                    );
                }
                consecutive_silence = 0;
            }

            let send_start = Instant::now();
            send_audio_frame(
                &settings,
                &audio_tx,
                &mix_buf,
                frame_size,
                output_channels,
                effective_rate,
                &mut planar_scratch,
                &mut packed_scratch,
                &mut wire_scratch,
                &mut audio_timestamp,
            );
            let send_elapsed = send_start.elapsed();
            if send_elapsed.as_millis() > 5 {
                slow_send_count += 1;
                eprintln!(
                    "AUDIO DIAG: send_audio_frame took {}ms (frame {})",
                    send_elapsed.as_millis(),
                    frame_counter
                );
            }
        }
    }

    fn run_monitor_thread(
        running: Arc<std::sync::atomic::AtomicBool>,
        settings: AudioSettings,
        effective_rate: u32,
        output_channels: usize,
        rx: std::sync::mpsc::Receiver<Vec<f32>>,
    ) {
        let frame_size = settings.samples_per_channel.max(1);
        let mut pcm_monitor: Option<MonitorOutput> = None;
        let mut last_monitor_attempt =
            Instant::now() - Duration::from_millis(settings.restart_cooldown_ms.max(1));
        let mut monitor_s16: Vec<i16> = Vec::new();
        let mut monitor_write_buf: Vec<u8> = Vec::new();

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let buffer = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(buf) => buf,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            if pcm_monitor.is_none()
                && last_monitor_attempt.elapsed()
                    >= Duration::from_millis(settings.restart_cooldown_ms.max(1))
            {
                last_monitor_attempt = Instant::now();
                match open_monitor_output(&settings, effective_rate, output_channels as u32) {
                    Ok(pcm) => pcm_monitor = Some(pcm),
                    Err(e) => {
                        eprintln!(
                            "Failed to open monitor output {}: {}",
                            settings.monitor.device, e
                        );
                    }
                }
            }

            if let Some(monitor) = pcm_monitor.as_mut() {
                if !write_monitor_output(
                    monitor,
                    &buffer,
                    frame_size,
                    output_channels,
                    &mut monitor_s16,
                    &mut monitor_write_buf,
                ) {
                    pcm_monitor = None;
                }
            }
        }
    }

    /// System monotonic clock in 100ns units (matching OBS os_gettime_ns() / 100).
    /// NOT relative to process start — uses raw CLOCK_MONOTONIC so receiver
    /// timestamps align with the system timebase.
    fn system_monotonic_100ns() -> i64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        }
        (ts.tv_sec as i64 * 10_000_000) + (ts.tv_nsec as i64 / 100)
    }

    fn send_audio_frame(
        _settings: &AudioSettings,
        audio_tx: &broadcast::Sender<OMTFrame>,
        interleaved: &[f32],
        samples_per_channel: usize,
        channels: usize,
        sample_rate: u32,
        planar_scratch: &mut Vec<f32>,
        packed_scratch: &mut Vec<f32>,
        wire_scratch: &mut Vec<u8>,
        audio_timestamp: &mut i64,
    ) {
        if planar_scratch.len() != interleaved.len() {
            planar_scratch.resize(interleaved.len(), 0.0);
        }
        for ch in 0..channels {
            let planar_offset = ch * samples_per_channel;
            for s in 0..samples_per_channel {
                planar_scratch[planar_offset + s] = interleaved[s * channels + ch];
            }
        }

        // Match C# libomtnet OMTFPA1Codec.Encode behavior:
        // - keep channel count in header
        // - compact payload to only active (non-silent) channel planes
        // - advertise active channels via bitmask
        let active_mask = pack_active_planar_channels(
            planar_scratch,
            samples_per_channel,
            channels,
            packed_scratch,
        );

        let mut frame = OMTFrame::new(OMTFrameType::Audio);
        // Use system monotonic clock (like OBS's os_gettime_ns / 100).
        // The receiver multiplies by 100 to get nanoseconds for OBS.
        frame.header.timestamp = system_monotonic_100ns();
        frame.audio_header = Some(libomtnet::OMTAudioHeader {
            codec: libomtnet::OMTCodec::FPA1 as i32,
            sample_rate: sample_rate as i32,
            samples_per_channel: samples_per_channel as i32,
            channels: channels as i32,
            active_channels: active_mask,
            reserved1: 0,
        });

        wire_scratch.clear();
        wire_scratch.reserve(packed_scratch.len() * 4);
        for &sample in packed_scratch.iter() {
            wire_scratch.put_f32_le(sample);
        }
        // Use BytesMut to avoid a separate heap allocation per frame.
        let mut bm = bytes::BytesMut::with_capacity(wire_scratch.len());
        bm.extend_from_slice(wire_scratch);
        frame.data = bm.freeze();
        frame.update_data_length();
        // Send directly to broadcast — no intermediate thread hops.
        let _ = audio_tx.send(frame);
    }

    fn pack_active_planar_channels(
        planar: &[f32],
        samples_per_channel: usize,
        channels: usize,
        out: &mut Vec<f32>,
    ) -> u32 {
        out.clear();
        out.reserve(planar.len());
        let mut mask = 0u32;

        for ch in 0..channels {
            let start = ch * samples_per_channel;
            let end = start + samples_per_channel;
            let plane = &planar[start..end];
            let active = plane.iter().any(|v| *v != 0.0);
            if !active {
                continue;
            }

            if ch >= 32 {
                mask = u32::MAX;
            } else {
                mask |= 1u32 << ch;
            }
            out.extend_from_slice(plane);
        }

        mask
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

    fn build_device_candidates(device: &str, include_default: bool) -> Vec<String> {
        let mut candidates = vec![device.to_string()];
        if let Some(suffix) = device.strip_prefix("hw:") {
            candidates.push(format!("plughw:{suffix}"));
            candidates.push(format!("plug:hw:{suffix}"));
        }
        // For monitor playback we can safely try generic fallbacks.
        // For capture inputs (HDMI/TRS), keep routing deterministic and do not silently
        // fall back to an unrelated "default" source.
        if include_default && !candidates.iter().any(|v| v == "default") {
            candidates.push("default".to_string());
        }
        candidates.dedup();
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

        // Use non-blocking only when both sources are active (prevents one stalling the other).
        let nonblock = use_hdmi && use_trs;

        for rate in rate_candidates {
            let mut hdmi_input: Option<AlsaInput> = None;
            let mut trs_input: Option<AlsaInput> = None;
            let mut opened_hdmi_channels = 0usize;
            let mut opened_trs_channels = 0usize;

            if use_hdmi {
                for device in build_device_candidates(&settings.hdmi_device, false) {
                    for ch in &channel_candidates {
                        if let Ok(input) = open_pcm_capture(
                            &device,
                            rate,
                            *ch,
                            settings.arecord_buffer_usec,
                            settings.arecord_period_usec,
                            nonblock,
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
                for device in build_device_candidates(&settings.trs_device, false) {
                    for ch in &channel_candidates {
                        if let Ok(input) = open_pcm_capture(
                            &device,
                            rate,
                            *ch,
                            settings.arecord_buffer_usec,
                            settings.arecord_period_usec,
                            nonblock,
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
        nonblock: bool,
    ) -> Result<AlsaInput, alsa::Error> {
        // Blocking mode gives the most reliable timing for single-source capture.
        // Non-blocking is only needed when multiple capture devices are used (HDMI+TRS)
        // to prevent one idle/unplugged device from stalling the entire audio loop.
        if let Ok(pcm) = PCM::new(device, Direction::Capture, nonblock) {
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
                    nonblock,
                });
            }
        }
        let pcm = PCM::new(device, Direction::Capture, nonblock)?;
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
            nonblock,
        })
    }

    fn open_pcm_playback(
        device: &str,
        rate: u32,
        channels: u32,
        buffer_usec: u32,
        period_usec: u32,
    ) -> Result<AlsaOutput, alsa::Error> {
        // Playback is also opened non-blocking so a dead monitor device can't stall audio capture.
        if let Ok(pcm) = PCM::new(device, Direction::Playback, true) {
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
        let pcm = PCM::new(device, Direction::Playback, true)?;
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

    fn open_monitor_output(
        settings: &AudioSettings,
        preferred_rate: u32,
        preferred_channels: u32,
    ) -> Result<MonitorOutput, String> {
        // Unlike capture, playback devices frequently don't support f32; also many "hw:" devices
        // only accept specific formats/rates. Try a conservative set of candidates.
        let mut rate_candidates = vec![preferred_rate, settings.sample_rate, 48_000, 44_100];
        rate_candidates.retain(|v| *v > 0);
        rate_candidates.dedup();

        let mut channel_candidates = vec![preferred_channels, 2, 1];
        channel_candidates.retain(|v| *v > 0);
        channel_candidates.dedup();

        let mut last_err: Option<String> = None;
        for dev in build_device_candidates(&settings.monitor.device, true) {
            for rate in &rate_candidates {
                for ch in &channel_candidates {
                    match open_pcm_playback(
                        &dev,
                        *rate,
                        *ch,
                        settings.arecord_buffer_usec,
                        settings.arecord_period_usec,
                    ) {
                        Ok(out) => {
                            if dev != settings.monitor.device
                                || *rate != preferred_rate
                                || *ch != preferred_channels
                            {
                                println!(
                                    "Monitor output opened on {} (rate={}, channels={}, format={})",
                                    dev, rate, ch, out.format
                                );
                            }
                            return Ok(MonitorOutput::Alsa(out));
                        }
                        Err(e) => last_err = Some(e.to_string()),
                    }
                }
            }
        }

        // Final fallback: start `aplay` process in S16 mode.
        for dev in build_device_candidates(&settings.monitor.device, true) {
            for rate in &rate_candidates {
                for ch in &channel_candidates {
                    match open_aplay_output(&dev, *rate, *ch as usize) {
                        Ok(out) => {
                            println!(
                                "Monitor output opened via aplay on {} (rate={}, channels={})",
                                dev, rate, ch
                            );
                            return Ok(MonitorOutput::Aplay(out));
                        }
                        Err(e) => last_err = Some(e),
                    }
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| "monitor output unavailable (alsa/aplay open failed)".to_string()))
    }

    fn open_aplay_output(device: &str, rate: u32, channels: usize) -> Result<AplayOutput, String> {
        let mut cmd = Command::new("aplay");
        cmd.arg("-q")
            .arg("-D")
            .arg(device)
            .arg("-t")
            .arg("raw")
            .arg("-f")
            .arg("S16_LE")
            .arg("-c")
            .arg(channels.to_string())
            .arg("-r")
            .arg(rate.to_string())
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("aplay spawn failed for {}: {}", device, e))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "aplay stdin unavailable".to_string())?;
        Ok(AplayOutput { child, stdin })
    }

    fn write_monitor_output(
        output: &mut MonitorOutput,
        buffer: &[f32],
        frames: usize,
        channels: usize,
        scratch_i16: &mut Vec<i16>,
        scratch_bytes: &mut Vec<u8>,
    ) -> bool {
        match output {
            MonitorOutput::Alsa(out) => {
                write_pcm(out, buffer, frames, channels, scratch_i16).is_ok()
            }
            MonitorOutput::Aplay(out) => {
                write_aplay(out, buffer, frames, channels, scratch_i16, scratch_bytes).is_ok()
            }
        }
    }

    fn write_aplay(
        output: &mut AplayOutput,
        buffer: &[f32],
        frames: usize,
        channels: usize,
        scratch_i16: &mut Vec<i16>,
        scratch_bytes: &mut Vec<u8>,
    ) -> Result<(), std::io::Error> {
        if output.child.try_wait()?.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "aplay exited",
            ));
        }

        let samples = frames * channels;
        if scratch_i16.len() < samples {
            scratch_i16.resize(samples, 0);
        }
        if scratch_bytes.len() < samples * 2 {
            scratch_bytes.resize(samples * 2, 0);
        }

        for i in 0..samples {
            let mut sample = buffer[i];
            if sample > 1.0 {
                sample = 1.0;
            }
            if sample < -1.0 {
                sample = -1.0;
            }
            let v = (sample * i16::MAX as f32) as i16;
            scratch_i16[i] = v;
            let b = v.to_le_bytes();
            scratch_bytes[i * 2] = b[0];
            scratch_bytes[i * 2 + 1] = b[1];
        }

        output.stdin.write_all(&scratch_bytes[..samples * 2])?;
        Ok(())
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

    /// Wait for data to be available, then read. Returns the number of frames read.
    /// Returns Ok(0) only if wait times out (no data within timeout).
    fn read_pcm(
        input: &AlsaInput,
        buffer: &mut [f32],
        frames: usize,
        channels: usize,
        scratch_i16: &mut Vec<i16>,
    ) -> Result<usize, alsa::Error> {
        // Clear to silence so partial reads don't leak old samples.
        buffer.fill(0.0);

        // Ensure the PCM is running before waiting for data.
        match input.pcm.state() {
            State::XRun | State::Suspended => {
                eprintln!("AUDIO DIAG: ALSA XRUN detected, recovering");
                let _ = input.pcm.prepare();
                let _ = input.pcm.start();
            }
            State::Prepared | State::Setup => {
                let _ = input.pcm.start();
            }
            _ => {}
        }

        // In non-blocking mode, wait for data before reading to avoid EAGAIN busy-loop.
        // In blocking mode, readi() blocks internally so wait() is unnecessary.
        if input.nonblock {
            if !input.pcm.wait(Some(50))? {
                return Ok(0);
            }
        }

        match input.format {
            SampleFormat::Float32 => {
                let io = input.pcm.io_f32()?;
                match io.readi(buffer) {
                    Ok(count) => Ok(count),
                    Err(e) => {
                        if is_eagain(&e) {
                            return Ok(0);
                        }
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
                scratch_i16[..samples].fill(0);
                let count = match io.readi(&mut scratch_i16[..samples]) {
                    Ok(v) => v,
                    Err(e) => {
                        if is_eagain(&e) {
                            return Ok(0);
                        }
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
}
