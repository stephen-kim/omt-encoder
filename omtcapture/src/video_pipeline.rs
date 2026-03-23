use std::sync::atomic::AtomicU8;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::send_coordinator::SendCoordinator;
use crate::settings::{PreviewSettings, VideoSettings};

pub struct VideoPipeline {
    settings: Arc<tokio::sync::RwLock<VideoSettings>>,
    preview: Arc<tokio::sync::RwLock<PreviewSettings>>,
    suggested_quality_hint: Arc<AtomicU8>,
    running: Arc<std::sync::atomic::AtomicBool>,
    restart_requested: Arc<std::sync::atomic::AtomicBool>,
    preview_restart_requested: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
    send: SendCoordinator,
}

impl VideoPipeline {
    pub fn new(
        settings: VideoSettings,
        preview: PreviewSettings,
        send: SendCoordinator,
        suggested_quality_hint: Arc<AtomicU8>,
    ) -> Self {
        VideoPipeline {
            settings: Arc::new(tokio::sync::RwLock::new(settings)),
            preview: Arc::new(tokio::sync::RwLock::new(preview)),
            suggested_quality_hint,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            restart_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            preview_restart_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread_handle: None,
            send,
        }
    }

    pub fn start(&mut self) {
        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let running = self.running.clone();
        let send = self.send.clone();
        let suggested_quality_hint = self.suggested_quality_hint.clone();

        #[cfg(target_os = "linux")]
        let settings = self.settings.clone();
        #[cfg(target_os = "linux")]
        let preview = self.preview.clone();
        #[cfg(target_os = "linux")]
        let restart_requested = self.restart_requested.clone();
        #[cfg(target_os = "linux")]
        let preview_restart_requested = self.preview_restart_requested.clone();

        self.thread_handle = Some(thread::spawn(move || {
            #[cfg(target_os = "linux")]
            {
                while running.load(std::sync::atomic::Ordering::SeqCst) {
                    let current_settings = settings.blocking_read().clone();
                    linux::run_video_loop(
                        running.clone(),
                        restart_requested.clone(),
                        preview_restart_requested.clone(),
                        current_settings,
                        preview.clone(),
                        send.clone(),
                        suggested_quality_hint.clone(),
                    );
                    if running.load(std::sync::atomic::Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            }

            #[cfg(not(target_os = "linux"))]
            stub::run_video_loop(running, send, suggested_quality_hint);
        }));
    }

    pub fn update_video(&self, settings: VideoSettings) {
        *self.settings.blocking_write() = settings;
        // Restart capture/transform/vmx with the new config, but keep the server running and
        // avoid forcing receivers (OBS) to restart.
        self.restart_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn update_preview(&self, preview: PreviewSettings) {
        *self.preview.blocking_write() = preview;
        // Match C# sender: preview changes should NOT tear down capture/encode/network.
        // Only restart the preview workers.
        self.preview_restart_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn stop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.restart_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.preview_restart_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod stub {
    use super::*;

    pub fn run_video_loop(
        running: Arc<std::sync::atomic::AtomicBool>,
        _send: SendCoordinator,
        _suggested_quality_hint: Arc<AtomicU8>,
    ) {
        println!("Video capture available on Linux only. Stubbing for macOS.");
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use bytes::Bytes;
    use libomtnet::{OMTCodec, OMTFrame, OMTFrameType, OMTVideoFlags, OMTVideoHeader};
    use libvmx_sys::root;
    use std::collections::HashSet;
    use std::io::{Read, Write};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    use v4l::buffer::Type;
    use v4l::format::FourCC;
    use v4l::fraction::Fraction;
    use v4l::io::traits::CaptureStream;
    use v4l::prelude::*;
    use v4l::video::capture::Parameters as CaptureParameters;
    use v4l::video::Capture;

    struct TransformContext {
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        input_buf: Vec<u8>,
        output_buf: Vec<u8>,
    }

    struct PreviewSink {
        output: String,
        pix_fmt: String,
        input_rate: String,
        input_width: u32,
        input_height: u32,
        preview_width: u32,
        preview_height: u32,
        preview_format: String,
        rotate: u32,
        last_sent: Instant,
        interval_ms: u64,
        tx: Option<mpsc::SyncSender<Bytes>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    pub fn run_video_loop(
        running: Arc<std::sync::atomic::AtomicBool>,
        restart_requested: Arc<std::sync::atomic::AtomicBool>,
        preview_restart_requested: Arc<std::sync::atomic::AtomicBool>,
        settings: VideoSettings,
        preview: Arc<tokio::sync::RwLock<PreviewSettings>>,
        send: SendCoordinator,
        suggested_quality_hint: Arc<AtomicU8>,
    ) {
        println!(
            "Starting Linux V4L2 pipeline on {}...",
            settings.device_path
        );
        // Clear any pending restart now that we're starting.
        restart_requested.store(false, std::sync::atomic::Ordering::SeqCst);
        preview_restart_requested.store(false, std::sync::atomic::Ordering::SeqCst);

        let dev = match Device::with_path(&settings.device_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open video device: {}", e);
                return;
            }
        };

        // Set the desired capture format explicitly. Without this, V4L2 may default to the
        // smallest resolution the device supports (e.g. 720x576 instead of 1920x1080).
        {
            let desired_fourcc = match settings.codec.to_ascii_uppercase().as_str() {
                "UYVY" => FourCC::new(b"UYVY"),
                "NV12" => FourCC::new(b"NV12"),
                "YV12" | "YU12" => FourCC::new(b"YU12"),
                "BGRA" => FourCC::new(b"BGRA"),
                _ => FourCC::new(b"YUYV"),
            };
            if let Ok(mut current_fmt) = dev.format() {
                current_fmt.width = settings.width.max(1);
                current_fmt.height = settings.height.max(1);
                current_fmt.fourcc = desired_fourcc;
                match dev.set_format(&current_fmt) {
                    Ok(actual) => {
                        println!(
                            "V4L2 format set: requested {}x{} {:?}, got {}x{} {:?}",
                            settings.width, settings.height, desired_fourcc,
                            actual.width, actual.height, actual.fourcc
                        );
                    }
                    Err(e) => {
                        eprintln!("Warning: failed to set V4L2 format: {}", e);
                    }
                }
            }
        }

        let fmt = match dev.format() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to read device format: {}", e);
                return;
            }
        };

        let mut input_rate_n = settings.frame_rate_n.max(1);
        let mut input_rate_d = settings.frame_rate_d.max(1);

        // Try to set capture frame interval (fps). Some devices ignore this, but when supported
        // it can reduce internal buffering and stabilize capture timing.
        if settings.frame_rate_n > 0 {
            let interval =
                Fraction::new(settings.frame_rate_d.max(1), settings.frame_rate_n.max(1));
            let params = CaptureParameters::new(interval);
            if let Err(e) = dev.set_params(&params) {
                eprintln!("Warning: failed to set V4L2 capture params (fps): {}", e);
            }
        }
        if let Ok(params) = dev.params() {
            if params.interval.numerator > 0 && params.interval.denominator > 0 {
                // v4l interval is time-per-frame (num/den sec), fps = den/num
                input_rate_n = params.interval.denominator;
                input_rate_d = params.interval.numerator;
            }
        }

        let input_width = fmt.width;
        let input_height = fmt.height;
        let input_fourcc = fmt.fourcc;
        let input_codec = fourcc_to_codec(input_fourcc);
        let input_pix_fmt = codec_to_pix_fmt(input_codec);
        let input_frame_bytes = frame_size_bytes(input_codec, input_width, input_height);

        let desired_codec = parse_codec(&settings.codec).unwrap_or(OMTCodec::YUY2);
        let desired_pix_fmt = codec_to_pix_fmt(desired_codec);

        let use_native = settings.use_native_format;
        let output_codec = if use_native {
            input_codec
        } else {
            desired_codec
        };
        let output_width = if use_native {
            input_width
        } else {
            settings.width.max(1)
        };
        let output_height = if use_native {
            input_height
        } else {
            settings.height.max(1)
        };
        let output_rate_n = if use_native {
            input_rate_n
        } else {
            settings.frame_rate_n.max(1)
        };
        let output_rate_d = if use_native {
            input_rate_d
        } else {
            settings.frame_rate_d.max(1)
        };
        let mut effective_output_codec = output_codec;
        let mut effective_output_width = output_width;
        let mut effective_output_height = output_height;
        let mut effective_output_rate_n = output_rate_n;
        let mut effective_output_rate_d = output_rate_d;

        let needs_transform = !use_native
            && (input_width != output_width
                || input_height != output_height
                || input_codec as i32 != output_codec as i32);

        println!(
            "Format: {} {}x{} {}fps -> {} {}x{} {}fps (native={})",
            codec_to_name(input_codec),
            input_width,
            input_height,
            format!("{}/{}", input_rate_n, input_rate_d),
            codec_to_name(output_codec),
            output_width,
            output_height,
            format!("{}/{}", output_rate_n, output_rate_d),
            use_native
        );

        let mut transform = if needs_transform {
            match start_transform(
                input_pix_fmt,
                input_width,
                input_height,
                input_rate_n,
                input_rate_d,
                desired_pix_fmt,
                output_width,
                output_height,
            ) {
                Ok(ctx) => Some(ctx),
                Err(e) => {
                    eprintln!("Video transform start failed: {}", e);
                    None
                }
            }
        } else {
            None
        };

        if needs_transform && transform.is_none() {
            eprintln!("Transform unavailable. Falling back to native output.");
            // Match C# behavior on transform startup failure.
            effective_output_codec = input_codec;
            effective_output_width = input_width;
            effective_output_height = input_height;
            effective_output_rate_n = input_rate_n;
            effective_output_rate_d = input_rate_d;
        }

        // Determine what we will actually encode/send.
        let encode_codec = effective_output_codec;
        let encode_width = effective_output_width;
        let encode_height = effective_output_height;
        let _encode_stride = if transform.is_some() {
            codec_stride(encode_codec, encode_width)
        } else {
            fmt.stride
        };

        // Fewer kernel-side buffers reduces capture->send latency.
        // 2 is usually safe at 1080p30 on Pi-class hardware; increase if you see V4L2 overruns.
        let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 2) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to create video stream: {}", e);
                return;
            }
        };

        let mut preview_sinks = {
            let current_preview = preview.blocking_read().clone();
            build_preview_sinks(
                &settings,
                &current_preview,
                input_width,
                input_height,
                input_fourcc,
            )
        };
        let mut preview_enabled = !preview_sinks.is_empty();

        let mut frame_count: usize = 0;
        let mut sent_bytes: usize = 0;
        let mut startup_debug_frames: usize = 0;
        let mut fps_window_start = Instant::now();
        let mut fps_window_frames: usize = 0;
        let mut consecutive_capture_errors: u32 = 0;
        let mut throttle_fps = !use_native
            && (input_rate_n as u64 * effective_output_rate_d as u64
                > effective_output_rate_n as u64 * input_rate_d as u64);
        if needs_transform && transform.is_none() {
            // Match C# behavior: when transform failed and we fall back to native format,
            // disable software FPS throttling.
            throttle_fps = false;
        }
        let output_frame_interval = Duration::from_secs_f64(
            effective_output_rate_d as f64 / effective_output_rate_n as f64,
        );
        let mut last_output_frame_at = Instant::now() - output_frame_interval;
        let mut last_vmx_error_log = Instant::now() - Duration::from_secs(5);
        let mut vmx_instance: Option<*mut root::VMX_INSTANCE> = None;
        let mut vmx_buffer = vec![
            0u8;
            (frame_size_bytes(encode_codec, encode_width, encode_height) * 2)
                .max(8 * 1024 * 1024)
        ];

        let mut current_quality_level = suggested_quality_hint.load(Ordering::Relaxed);
        if codec_to_vmx_image_format(encode_codec).is_some() {
            let size = root::VMX_SIZE {
                width: encode_width as i32,
                height: encode_height as i32,
            };
            unsafe {
                let inst = root::VMX_Create(
                    size,
                    vmx_profile_from_quality_level(current_quality_level),
                    root::VMX_COLORSPACE_VMX_COLORSPACE_BT709,
                );
                if !inst.is_null() {
                    vmx_instance = Some(inst);
                    let _ = root::VMX_SetThreads(inst, 2);
                } else {
                    eprintln!("VMX create failed, sending raw video codec.");
                }
            }
        } else {
            eprintln!(
                "Codec {} is not VMX encodable, sending raw video.",
                codec_to_name(encode_codec)
            );
        }

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            if restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                // Caller requested a config reload.
                break;
            }
            if preview_restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                preview_restart_requested.store(false, std::sync::atomic::Ordering::SeqCst);
                let current_preview = preview.blocking_read().clone();
                stop_preview_sinks(&mut preview_sinks);
                preview_sinks = build_preview_sinks(
                    &settings,
                    &current_preview,
                    input_width,
                    input_height,
                    input_fourcc,
                );
                preview_enabled = !preview_sinks.is_empty();
            }
            let new_quality_level = suggested_quality_hint.load(Ordering::Relaxed);
            if new_quality_level != current_quality_level {
                if let Some(inst) = vmx_instance {
                    unsafe {
                        let previous_quality = root::VMX_GetQuality(inst);
                        root::VMX_Destroy(inst);
                        let size = root::VMX_SIZE {
                            width: encode_width as i32,
                            height: encode_height as i32,
                        };
                        let new_inst = root::VMX_Create(
                            size,
                            vmx_profile_from_quality_level(new_quality_level),
                            root::VMX_COLORSPACE_VMX_COLORSPACE_BT709,
                        );
                        if !new_inst.is_null() {
                            let _ = root::VMX_SetThreads(new_inst, 2);
                            root::VMX_SetQuality(new_inst, previous_quality);
                            vmx_instance = Some(new_inst);
                            current_quality_level = new_quality_level;
                            println!(
                                "VMX profile switched due to receiver quality hint: level={}",
                                new_quality_level
                            );
                        } else {
                            vmx_instance = None;
                            eprintln!(
                                "VMX recreate failed for quality level {}",
                                new_quality_level
                            );
                        }
                    }
                }
            }

            let (raw_data, _) = match stream.next() {
                Ok(res) => {
                    consecutive_capture_errors = 0;
                    res
                }
                Err(e) => {
                    eprintln!("Failed to read video frame: {}", e);
                    consecutive_capture_errors = consecutive_capture_errors.saturating_add(1);
                    // Match C# behavior: recover by restarting capture if V4L2 keeps failing.
                    if consecutive_capture_errors >= 20 {
                        eprintln!("Too many consecutive video read errors; restarting capture.");
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            if restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            if preview_restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                // Apply preview changes as soon as possible, but after we finish a V4L2 read to
                // avoid leaving the stream in a weird state.
                continue;
            }
            if throttle_fps {
                let now = Instant::now();
                if now.duration_since(last_output_frame_at) < output_frame_interval {
                    continue;
                }
                last_output_frame_at = now;
            }

            let (payload, frame_codec, frame_width, frame_height, frame_stride) =
                if let Some(ref mut ctx) = transform {
                    if raw_data.len() < ctx.input_buf.len() {
                        eprintln!(
                            "Short input frame: {} < {}",
                            raw_data.len(),
                            ctx.input_buf.len()
                        );
                        consecutive_capture_errors = consecutive_capture_errors.saturating_add(1);
                        if consecutive_capture_errors >= 20 {
                            eprintln!("Too many short video frames; restarting capture.");
                            break;
                        }
                        continue;
                    }
                    let input_len = ctx.input_buf.len();
                    ctx.input_buf.copy_from_slice(&raw_data[..input_len]);
                    if ctx.stdin.write_all(&ctx.input_buf).is_err() {
                        eprintln!("Transform stdin write failed.");
                        break;
                    }
                    if !read_exact(&mut ctx.stdout, &mut ctx.output_buf) {
                        eprintln!("Transform stdout read failed.");
                        break;
                    }
                    (
                        Bytes::copy_from_slice(&ctx.output_buf),
                        effective_output_codec,
                        effective_output_width,
                        effective_output_height,
                        codec_stride(effective_output_codec, effective_output_width),
                    )
                } else {
                    (
                        Bytes::copy_from_slice(raw_data),
                        input_codec,
                        input_width,
                        input_height,
                        fmt.stride,
                    )
                };

            let network_frame = if let (Some(inst), Some(_fmt)) =
                (vmx_instance, codec_to_vmx_image_format(frame_codec))
            {
                let err = unsafe {
                    vmx_encode_frame(
                        inst,
                        frame_codec,
                        payload.as_ptr(),
                        frame_height,
                        frame_stride as i32,
                    )
                };
                if err == root::VMX_ERR_VMX_ERR_OK {
                    let compressed_len = unsafe {
                        root::VMX_SaveTo(inst, vmx_buffer.as_mut_ptr(), vmx_buffer.len() as i32)
                    };
                    if compressed_len > 0 {
                        let preview_payload_len =
                            unsafe { root::VMX_GetEncodedPreviewLength(inst) };
                        let preview_total_len = if preview_payload_len > 0 {
                            Some(OMTVideoHeader::SIZE as i32 + preview_payload_len)
                        } else {
                            None
                        };
                        Some((
                            Bytes::copy_from_slice(&vmx_buffer[..compressed_len as usize]),
                            OMTCodec::VMX1,
                            preview_total_len,
                            video_flags_from_source_codec(frame_codec),
                        ))
                    } else {
                        if last_vmx_error_log.elapsed() >= Duration::from_secs(1) {
                            eprintln!("VMX_SaveTo returned {}", compressed_len);
                            last_vmx_error_log = Instant::now();
                        }
                        None
                    }
                } else {
                    if last_vmx_error_log.elapsed() >= Duration::from_secs(1) {
                        eprintln!("VMX encode failed with err={}", err);
                        last_vmx_error_log = Instant::now();
                    }
                    None
                }
            } else {
                if frame_codec == OMTCodec::VMX1 {
                    Some((payload, frame_codec, None, 0))
                } else {
                    if last_vmx_error_log.elapsed() >= Duration::from_secs(1) {
                        eprintln!(
                            "No VMX encoder available for codec {}. Dropping frame.",
                            codec_to_name(frame_codec)
                        );
                        last_vmx_error_log = Instant::now();
                    }
                    None
                }
            };

            let Some((network_payload, network_codec, preview_data_length, network_flags)) =
                network_frame
            else {
                // No sleep needed: the next iteration blocks on stream.next() (V4L2 read).
                continue;
            };

            let mut frame = OMTFrame::new(OMTFrameType::Video);
            frame.video_header = Some(OMTVideoHeader {
                codec: network_codec as i32,
                width: frame_width as i32,
                height: frame_height as i32,
                frame_rate_n: effective_output_rate_n as i32,
                frame_rate_d: effective_output_rate_d as i32,
                aspect_ratio: frame_width as f32 / frame_height as f32,
                flags: network_flags,
                color_space: 709,
            });
            frame.data = network_payload;
            frame.update_data_length();
            frame.preview_data_length = preview_data_length;

            let payload_len = frame.data.len();
            if startup_debug_frames < 10 {
                println!(
                    "Video out frame[{}]: codec={}, {}x{}, payload={}, previewLen={:?}, fps={}/{}",
                    startup_debug_frames + 1,
                    codec_to_name(network_codec),
                    frame_width,
                    frame_height,
                    payload_len,
                    preview_data_length,
                    effective_output_rate_n,
                    effective_output_rate_d
                );
                startup_debug_frames += 1;
            }
            // enqueue_video always succeeds (latest-wins semantics).
            send.enqueue_video(frame);
            frame_count += 1;
            sent_bytes += payload_len;
            fps_window_frames += 1;

            if frame_count >= 60 {
                println!("Sent {} frames, {} bytes.", frame_count, sent_bytes);
                frame_count = 0;
                sent_bytes = 0;
            }

            let elapsed = fps_window_start.elapsed().as_secs_f64();
            if elapsed >= 2.0 {
                let fps = fps_window_frames as f64 / elapsed;
                println!(
                    "Video FPS: {:.1} (sent {} frames in {:.2}s)",
                    fps, fps_window_frames, elapsed
                );
                fps_window_start = Instant::now();
                fps_window_frames = 0;
            }

            if preview_enabled {
                let now = Instant::now();
                // Only copy a new preview frame when at least one sink is ready to accept one.
                let mut should_make_preview = false;
                for sink in preview_sinks.iter_mut() {
                    if sink.interval_ms == 0
                        || now.duration_since(sink.last_sent).as_millis() as u64 >= sink.interval_ms
                    {
                        should_make_preview = true;
                        break;
                    }
                }
                if should_make_preview {
                    let preview_bytes = Bytes::copy_from_slice(raw_data);
                    for sink in preview_sinks.iter_mut() {
                        if sink.interval_ms != 0
                            && (now.duration_since(sink.last_sent).as_millis() as u64)
                                < sink.interval_ms
                        {
                            continue;
                        }
                        if let Some(tx) = sink.tx.as_ref() {
                            if tx.try_send(preview_bytes.clone()).is_ok() {
                                sink.last_sent = now;
                            }
                        }
                    }
                }
            }
        }

        if let Some(mut ctx) = transform {
            let _ = ctx.child.kill();
            let _ = ctx.child.wait();
        }
        if let Some(inst) = vmx_instance {
            unsafe {
                root::VMX_Destroy(inst);
            }
        }

        // Stop preview workers.
        stop_preview_sinks(&mut preview_sinks);

        let _ = input_frame_bytes;
    }

    fn stop_preview_sinks(preview_sinks: &mut [PreviewSink]) {
        for sink in preview_sinks.iter_mut() {
            sink.tx.take();
            if let Some(handle) = sink.handle.take() {
                let _ = handle.join();
            }
        }
    }

    unsafe fn vmx_encode_frame(
        inst: *mut root::VMX_INSTANCE,
        codec: OMTCodec,
        data_ptr: *const u8,
        height: u32,
        stride: i32,
    ) -> root::VMX_ERR {
        let ptr = data_ptr as *mut u8;
        match codec {
            OMTCodec::UYVY => root::VMX_EncodeUYVY(inst, ptr, stride, 0),
            OMTCodec::UYVA => root::VMX_EncodeUYVA(inst, ptr, stride, 0),
            OMTCodec::YUY2 => root::VMX_EncodeYUY2(inst, ptr, stride, 0),
            OMTCodec::NV12 => {
                let y_bytes = (stride as usize).saturating_mul(height as usize);
                let uv_ptr = ptr.add(y_bytes);
                root::VMX_EncodeNV12(inst, ptr, stride, uv_ptr, stride, 0)
            }
            OMTCodec::YV12 => {
                let y_stride = stride.max(1) as usize;
                let y_bytes = y_stride.saturating_mul(height as usize);
                let uv_stride = (y_stride / 2).max(1);
                let uv_bytes = uv_stride.saturating_mul((height as usize) / 2);
                let u_ptr = ptr.add(y_bytes);
                let v_ptr = u_ptr.add(uv_bytes);
                root::VMX_EncodeYV12(
                    inst,
                    ptr,
                    y_stride as i32,
                    u_ptr,
                    uv_stride as i32,
                    v_ptr,
                    uv_stride as i32,
                    0,
                )
            }
            OMTCodec::BGRA => root::VMX_EncodeBGRA(inst, ptr, stride, 0),
            OMTCodec::P216 => root::VMX_EncodeP216(inst, ptr, stride, 0),
            OMTCodec::PA16 => root::VMX_EncodePA16(inst, ptr, stride, 0),
            _ => root::VMX_ERR_VMX_ERR_INVALID_CODEC_FORMAT,
        }
    }

    fn codec_to_vmx_image_format(codec: OMTCodec) -> Option<root::VMX_IMAGE_FORMAT> {
        match codec {
            OMTCodec::UYVY => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_UYVY),
            OMTCodec::UYVA => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_UYVA),
            OMTCodec::YUY2 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_YUY2),
            OMTCodec::NV12 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_NV12),
            OMTCodec::YV12 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_YV12),
            OMTCodec::BGRA => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_BGRA),
            OMTCodec::P216 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_P216),
            OMTCodec::PA16 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_PA16),
            _ => None,
        }
    }

    fn video_flags_from_source_codec(codec: OMTCodec) -> u32 {
        let mut flags = 0u32;
        if matches!(codec, OMTCodec::UYVA | OMTCodec::PA16) {
            flags |= OMTVideoFlags::Alpha as u32;
        }
        if matches!(codec, OMTCodec::P216 | OMTCodec::PA16) {
            flags |= OMTVideoFlags::HighBitDepth as u32;
        }
        flags
    }

    fn vmx_profile_from_quality_level(level: u8) -> root::VMX_PROFILE {
        match level {
            3.. => root::VMX_PROFILE_VMX_PROFILE_OMT_HQ,
            2 => root::VMX_PROFILE_VMX_PROFILE_OMT_SQ,
            1 => root::VMX_PROFILE_VMX_PROFILE_OMT_LQ,
            // C# OMTVMX1Codec maps VMX_PROFILE_DEFAULT -> OMT_SQ internally.
            _ => root::VMX_PROFILE_VMX_PROFILE_OMT_SQ,
        }
    }

    fn codec_stride(codec: OMTCodec, width: u32) -> u32 {
        match codec {
            OMTCodec::NV12 => width,
            OMTCodec::YV12 => width,
            OMTCodec::BGRA => width * 4,
            OMTCodec::UYVA => width * 2,
            OMTCodec::P216 => width * 4,
            OMTCodec::PA16 => width * 4,
            _ => width * 2,
        }
    }

    fn start_transform(
        input_pix_fmt: &str,
        input_width: u32,
        input_height: u32,
        input_rate_n: u32,
        input_rate_d: u32,
        output_pix_fmt: &str,
        output_width: u32,
        output_height: u32,
    ) -> Result<TransformContext, String> {
        let input_rate = if input_rate_d == 0 {
            "30".to_string()
        } else {
            format!("{}/{}", input_rate_n, input_rate_d)
        };

        let mut child = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                input_pix_fmt,
                "-s",
                &format!("{}x{}", input_width, input_height),
                "-r",
                &input_rate,
                "-i",
                "pipe:0",
                "-vf",
                &format!(
                    "scale={}:{}:flags=fast_bilinear,format={}",
                    output_width, output_height, output_pix_fmt
                ),
                "-f",
                "rawvideo",
                "-pix_fmt",
                output_pix_fmt,
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn ffmpeg failed: {}", e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "ffmpeg stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "ffmpeg stdout unavailable".to_string())?;

        let input_codec = pix_fmt_to_codec(input_pix_fmt)
            .ok_or_else(|| "unsupported input pix fmt".to_string())?;
        let output_codec = pix_fmt_to_codec(output_pix_fmt)
            .ok_or_else(|| "unsupported output pix fmt".to_string())?;

        Ok(TransformContext {
            child,
            stdin,
            stdout,
            input_buf: vec![0u8; frame_size_bytes(input_codec, input_width, input_height)],
            output_buf: vec![0u8; frame_size_bytes(output_codec, output_width, output_height)],
        })
    }

    fn read_exact<R: Read>(reader: &mut R, buffer: &mut [u8]) -> bool {
        let mut offset = 0usize;
        while offset < buffer.len() {
            match reader.read(&mut buffer[offset..]) {
                Ok(0) => return false,
                Ok(n) => offset += n,
                Err(_) => return false,
            }
        }
        true
    }

    fn parse_codec(codec: &str) -> Option<OMTCodec> {
        match codec.trim().to_ascii_uppercase().as_str() {
            "UYVY" => Some(OMTCodec::UYVY),
            "UYVA" => Some(OMTCodec::UYVA),
            "YUY2" => Some(OMTCodec::YUY2),
            "YUYV" => Some(OMTCodec::YUY2),
            "NV12" => Some(OMTCodec::NV12),
            "YV12" => Some(OMTCodec::YV12),
            "YU12" => Some(OMTCodec::YV12),
            "BGRA" => Some(OMTCodec::BGRA),
            "P216" => Some(OMTCodec::P216),
            "PA16" => Some(OMTCodec::PA16),
            _ => None,
        }
    }

    fn codec_to_name(codec: OMTCodec) -> &'static str {
        match codec {
            OMTCodec::UYVY => "UYVY",
            OMTCodec::UYVA => "UYVA",
            OMTCodec::YUY2 => "YUY2",
            OMTCodec::NV12 => "NV12",
            OMTCodec::YV12 => "YV12",
            OMTCodec::BGRA => "BGRA",
            OMTCodec::P216 => "P216",
            OMTCodec::PA16 => "PA16",
            OMTCodec::VMX1 => "VMX1",
            _ => "UNKNOWN",
        }
    }

    fn codec_to_pix_fmt(codec: OMTCodec) -> &'static str {
        match codec {
            OMTCodec::UYVY => "uyvy422",
            OMTCodec::YUY2 => "yuyv422",
            OMTCodec::NV12 => "nv12",
            OMTCodec::YV12 => "yuv420p",
            OMTCodec::BGRA => "bgra",
            OMTCodec::P216 => "p216le",
            _ => "yuyv422",
        }
    }

    fn pix_fmt_to_codec(pix_fmt: &str) -> Option<OMTCodec> {
        match pix_fmt {
            "uyvy422" => Some(OMTCodec::UYVY),
            "yuyv422" => Some(OMTCodec::YUY2),
            "nv12" => Some(OMTCodec::NV12),
            "yuv420p" => Some(OMTCodec::YV12),
            "bgra" => Some(OMTCodec::BGRA),
            "p216le" => Some(OMTCodec::P216),
            _ => None,
        }
    }

    fn frame_size_bytes(codec: OMTCodec, width: u32, height: u32) -> usize {
        let pixels = width as usize * height as usize;
        match codec {
            OMTCodec::NV12 => pixels * 3 / 2,
            OMTCodec::YV12 => pixels * 3 / 2,
            OMTCodec::BGRA => pixels * 4,
            OMTCodec::UYVA => pixels * 3,
            OMTCodec::P216 => pixels * 4,
            OMTCodec::PA16 => pixels * 6,
            _ => pixels * 2,
        }
    }

    struct ResolvedOutput {
        device: String,
        fps: u32,
        pixel_format: String,
        rotate: u32,
    }

    fn build_preview_sinks(
        settings: &VideoSettings,
        preview: &PreviewSettings,
        input_width: u32,
        input_height: u32,
        input_fourcc: FourCC,
    ) -> Vec<PreviewSink> {
        if !preview.enabled {
            return Vec::new();
        }

        // Resolve output list: prefer per-output `outputs` array, fall back to legacy fields.
        let resolved: Vec<ResolvedOutput> = if !preview.outputs.is_empty() {
            preview
                .outputs
                .iter()
                .filter(|o| !o.device.trim().is_empty())
                .map(|o| ResolvedOutput {
                    device: o.device.clone(),
                    fps: o.fps,
                    pixel_format: if o.pixel_format.trim().is_empty() {
                        preview.pixel_format.clone()
                    } else {
                        o.pixel_format.clone()
                    },
                    rotate: o.rotate,
                })
                .collect()
        } else {
            let mut devs = preview.output_devices.clone();
            if devs.is_empty() && !preview.output_device.is_empty() {
                devs.push(preview.output_device.clone());
            }
            devs.into_iter()
                .map(|d| ResolvedOutput {
                    device: d,
                    fps: preview.fps,
                    pixel_format: preview.pixel_format.clone(),
                    rotate: 0,
                })
                .collect()
        };

        let fourcc_str = std::str::from_utf8(&input_fourcc.repr).unwrap_or("YUYV");
        let pix_fmt = match fourcc_str {
            "UYVY" => "uyvy422",
            "YUY2" | "YUYV" => "yuyv422",
            "NV12" => "nv12",
            _ => match settings.codec.as_str() {
                "UYVY" => "uyvy422",
                "YUY2" => "yuyv422",
                "NV12" => "nv12",
                _ => "uyvy422",
            },
        };

        let mut sinks = Vec::new();
        let mut seen = HashSet::new();

        for out in resolved {
            if !seen.insert(out.device.clone()) {
                continue;
            }

            let interval_ms = if out.fps == 0 {
                0
            } else {
                1000 / out.fps.max(1) as u64
            };
            let input_rate = if out.fps > 0 {
                out.fps.to_string()
            } else if settings.frame_rate_d == 0 {
                "30".to_string()
            } else {
                format!(
                    "{}/{}",
                    settings.frame_rate_n.max(1),
                    settings.frame_rate_d.max(1)
                )
            };
            let (tx, rx) = mpsc::sync_channel::<Bytes>(1);
            let (preview_width, preview_height) = try_get_framebuffer_size(&out.device)
                .or_else(|| {
                    if preview.width > 0 && preview.height > 0 {
                        Some((preview.width, preview.height))
                    } else {
                        None
                    }
                })
                .unwrap_or((input_width, input_height));

            let fmt = if out.pixel_format.trim().is_empty() {
                "rgb565le".to_string()
            } else {
                out.pixel_format.clone()
            };

            println!(
                "Preview output: {} ({}x{} @ {}fps, {})",
                out.device, preview_width, preview_height, input_rate, fmt
            );

            let sink = PreviewSink {
                output: out.device,
                pix_fmt: pix_fmt.to_string(),
                input_rate,
                input_width,
                input_height,
                preview_width,
                preview_height,
                preview_format: fmt,
                rotate: out.rotate,
                last_sent: Instant::now(),
                interval_ms,
                tx: Some(tx),
                handle: None,
            };

            sinks.push(spawn_preview_worker(sink, rx));
        }

        sinks
    }

    fn spawn_preview_worker(sink: PreviewSink, rx: mpsc::Receiver<Bytes>) -> PreviewSink {
        let pix_fmt = sink.pix_fmt.clone();
        let input_width = sink.input_width;
        let input_height = sink.input_height;
        let input_rate = sink.input_rate.clone();
        let preview_width = sink.preview_width;
        let preview_height = sink.preview_height;
        let preview_format = sink.preview_format.clone();
        let rotate = sink.rotate;
        let output = sink.output.clone();
        let is_fbdev = output.starts_with("/dev/fb");

        let handle = std::thread::spawn(move || {
            let mut child: Option<Child> = None;
            let mut ffmpeg_stdin: Option<ChildStdin> = None;
            let mut ffmpeg_stdout: Option<ChildStdout> = None;
            let mut fb_file: Option<std::fs::File> = None;
            let frame_bytes = preview_width as usize * preview_height as usize * 2; // 16bpp
            let mut fb_buf = vec![0u8; frame_bytes];

            let start_ffmpeg = |child: &mut Option<Child>,
                                ffmpeg_stdin: &mut Option<ChildStdin>,
                                ffmpeg_stdout: &mut Option<ChildStdout>,
                                fb_file: &mut Option<std::fs::File>|
             -> Result<(), ()> {
                let mut cmd = Command::new("ffmpeg");
                // Build the video filter chain: scale + optional rotation + format.
                // transpose: 1=90°CW, 2=90°CCW, 3=180° (requires two transposes)
                let vf = if rotate == 1 {
                    format!(
                        "scale={}:{}:flags=fast_bilinear,transpose=1,format={}",
                        preview_height, preview_width, preview_format
                    )
                } else if rotate == 2 {
                    format!(
                        "scale={}:{}:flags=fast_bilinear,transpose=2,format={}",
                        preview_height, preview_width, preview_format
                    )
                } else if rotate == 3 {
                    format!(
                        "scale={}:{}:flags=fast_bilinear,transpose=1,transpose=1,format={}",
                        preview_width, preview_height, preview_format
                    )
                } else {
                    format!(
                        "scale={}:{}:flags=fast_bilinear,format={}",
                        preview_width, preview_height, preview_format
                    )
                };
                cmd.args([
                    "-loglevel", "error",
                    "-f", "rawvideo",
                    "-pix_fmt", &pix_fmt,
                    "-s", &format!("{}x{}", input_width, input_height),
                    "-r", &input_rate,
                    "-i", "pipe:0",
                    "-vf", &vf,
                ]);
                if is_fbdev {
                    // For fbtft (nonstd=1) framebuffers, fbdev output doesn't work.
                    // Output raw frames and write directly to the fb device.
                    cmd.args(["-f", "rawvideo", "pipe:1"]);
                    cmd.stdout(Stdio::piped());
                } else {
                    cmd.args(["-f", "fbdev", &output]);
                    cmd.stdout(Stdio::null());
                }
                cmd.stdin(Stdio::piped()).stderr(Stdio::null());

                let mut c = cmd.spawn().map_err(|_| ())?;
                let s = c.stdin.take().ok_or(())?;
                *ffmpeg_stdin = Some(s);
                if is_fbdev {
                    *ffmpeg_stdout = c.stdout.take();
                    let f = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&output)
                        .map_err(|_| ())?;
                    *fb_file = Some(f);
                }
                *child = Some(c);
                Ok(())
            };

            let ensure_running = |child: &mut Option<Child>,
                                  ffmpeg_stdin: &mut Option<ChildStdin>,
                                  ffmpeg_stdout: &mut Option<ChildStdout>,
                                  fb_file: &mut Option<std::fs::File>| {
                let exited = child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten())
                    .is_some();
                if child.is_none() || exited {
                    let _ = start_ffmpeg(child, ffmpeg_stdin, ffmpeg_stdout, fb_file);
                }
            };

            ensure_running(&mut child, &mut ffmpeg_stdin, &mut ffmpeg_stdout, &mut fb_file);

            while let Ok(frame) = rx.recv() {
                ensure_running(&mut child, &mut ffmpeg_stdin, &mut ffmpeg_stdout, &mut fb_file);
                // Write raw capture frame to ffmpeg stdin.
                if let Some(s) = ffmpeg_stdin.as_mut() {
                    if s.write_all(&frame).is_err() {
                        if let Some(mut c) = child.take() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                        ffmpeg_stdin = None;
                        ffmpeg_stdout = None;
                        fb_file = None;
                        ensure_running(&mut child, &mut ffmpeg_stdin, &mut ffmpeg_stdout, &mut fb_file);
                        continue;
                    }
                }
                // For fbdev: read scaled frame from ffmpeg stdout and write to fb.
                if is_fbdev {
                    if let Some(stdout) = ffmpeg_stdout.as_mut() {
                        if read_exact(stdout, &mut fb_buf) {
                            if let Some(fb) = fb_file.as_mut() {
                                use std::io::Seek;
                                let _ = fb.seek(std::io::SeekFrom::Start(0));
                                let _ = fb.write_all(&fb_buf);
                            }
                        }
                    }
                }
            }

            if let Some(mut c) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        });

        PreviewSink {
            handle: Some(handle),
            ..sink
        }
    }

    fn fourcc_to_codec(fourcc: FourCC) -> OMTCodec {
        match std::str::from_utf8(&fourcc.repr).unwrap_or("YUYV") {
            "UYVY" => OMTCodec::UYVY,
            "YUYV" | "YUY2" => OMTCodec::YUY2,
            "NV12" => OMTCodec::NV12,
            "YV12" | "YU12" => OMTCodec::YV12,
            "P216" => OMTCodec::P216,
            "PA16" => OMTCodec::PA16,
            "UYVA" => OMTCodec::UYVA,
            "BGRA" => OMTCodec::BGRA,
            _ => OMTCodec::UYVY,
        }
    }

    fn try_get_framebuffer_size(path: &str) -> Option<(u32, u32)> {
        let fb = std::path::Path::new(path).file_name()?.to_str()?;
        if !fb.starts_with("fb") {
            return None;
        }
        let size_path = format!("/sys/class/graphics/{fb}/virtual_size");
        let size = std::fs::read_to_string(size_path).ok()?;
        let mut parts = size.trim().split(',');
        let width = parts.next()?.trim().parse::<u32>().ok()?;
        let height = parts.next()?.trim().parse::<u32>().ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        Some((width, height))
    }

    #[allow(dead_code)]
    fn _wallclock_100ns() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64
            / 100
    }
}
