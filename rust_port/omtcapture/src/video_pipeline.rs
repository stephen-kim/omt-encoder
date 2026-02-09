use std::sync::atomic::AtomicU8;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::send_coordinator::SendCoordinator;
use crate::settings::{PreviewSettings, VideoSettings};

pub struct VideoPipeline {
    settings: VideoSettings,
    send: SendCoordinator,
    preview: PreviewSettings,
    suggested_quality_hint: Arc<AtomicU8>,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl VideoPipeline {
    pub fn new(
        settings: VideoSettings,
        preview: PreviewSettings,
        send: SendCoordinator,
        suggested_quality_hint: Arc<AtomicU8>,
    ) -> Self {
        VideoPipeline {
            settings,
            send,
            preview,
            suggested_quality_hint,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread_handle: None,
        }
    }

    pub fn start(&mut self) {
        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let running = self.running.clone();
        let settings = self.settings.clone();
        let preview = self.preview.clone();
        let send = self.send.clone();
        let suggested_quality_hint = self.suggested_quality_hint.clone();

        self.thread_handle = Some(thread::spawn(move || {
            #[cfg(target_os = "linux")]
            linux::run_video_loop(running, settings, preview, send, suggested_quality_hint);

            #[cfg(not(target_os = "linux"))]
            stub::run_video_loop(running, settings, preview, send, suggested_quality_hint);
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

    pub fn run_video_loop(
        running: Arc<std::sync::atomic::AtomicBool>,
        _settings: VideoSettings,
        _preview: PreviewSettings,
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
    use libomtnet::{OMTCodec, OMTFrame, OMTFrameType, OMTVideoHeader};
    use libvmx_sys::root;
    use std::io::{Read, Write};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    use v4l::buffer::Type;
    use v4l::format::FourCC;
    use v4l::io::traits::CaptureStream;
    use v4l::prelude::*;
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
        stdin: ChildStdin,
        child: Child,
        last_sent: Instant,
        interval_ms: u64,
    }

    pub fn run_video_loop(
        running: Arc<std::sync::atomic::AtomicBool>,
        settings: VideoSettings,
        preview: PreviewSettings,
        send: SendCoordinator,
        suggested_quality_hint: Arc<AtomicU8>,
    ) {
        println!(
            "Starting Linux V4L2 pipeline on {}...",
            settings.device_path
        );

        let dev = match Device::with_path(&settings.device_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open video device: {}", e);
                return;
            }
        };

        let fmt = match dev.format() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to read device format: {}", e);
                return;
            }
        };

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
            settings.width
        };
        let output_height = if use_native {
            input_height
        } else {
            settings.height
        };
        let output_rate_n = if use_native {
            settings.frame_rate_n
        } else {
            settings.frame_rate_n
        };
        let output_rate_d = if use_native {
            settings.frame_rate_d
        } else {
            settings.frame_rate_d
        };

        let needs_transform = !use_native
            && (input_width != output_width
                || input_height != output_height
                || input_codec as i32 != output_codec as i32);

        println!(
            "Format: {} {}x{} -> {} {}x{} (native={})",
            codec_to_name(input_codec),
            input_width,
            input_height,
            codec_to_name(output_codec),
            output_width,
            output_height,
            use_native
        );

        let mut transform = if needs_transform {
            match start_transform(
                input_pix_fmt,
                input_width,
                input_height,
                output_rate_n,
                output_rate_d,
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
        }

        let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 4) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to create video stream: {}", e);
                return;
            }
        };

        let mut preview_sinks =
            build_preview_sinks(&settings, &preview, input_width, input_height, input_fourcc);

        let mut frame_count: usize = 0;
        let mut sent_bytes: usize = 0;
        let mut startup_debug_frames: usize = 0;
        let mut fps_window_start = Instant::now();
        let mut fps_window_frames: usize = 0;
        let mut last_vmx_error_log = Instant::now() - Duration::from_secs(5);
        let mut vmx_instance: Option<*mut root::VMX_INSTANCE> = None;
        let mut vmx_buffer = vec![
            0u8;
            (frame_size_bytes(output_codec, output_width, output_height) * 2)
                .max(8 * 1024 * 1024)
        ];

        let mut current_quality_level = suggested_quality_hint.load(Ordering::Relaxed);
        if codec_to_vmx_image_format(output_codec).is_some() {
            let size = root::VMX_SIZE {
                width: output_width as i32,
                height: output_height as i32,
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
                codec_to_name(output_codec)
            );
        }

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let new_quality_level = suggested_quality_hint.load(Ordering::Relaxed);
            if new_quality_level != current_quality_level {
                if let Some(inst) = vmx_instance {
                    unsafe {
                        let previous_quality = root::VMX_GetQuality(inst);
                        root::VMX_Destroy(inst);
                        let size = root::VMX_SIZE {
                            width: output_width as i32,
                            height: output_height as i32,
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
                Ok(res) => res,
                Err(e) => {
                    eprintln!("Failed to read video frame: {}", e);
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };

            let (payload, frame_codec, frame_width, frame_height, frame_stride) =
                if let Some(ref mut ctx) = transform {
                    if raw_data.len() < ctx.input_buf.len() {
                        eprintln!(
                            "Short input frame: {} < {}",
                            raw_data.len(),
                            ctx.input_buf.len()
                        );
                        continue;
                    }
                    ctx.input_buf
                        .copy_from_slice(&raw_data[..ctx.input_buf.len()]);
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
                        output_codec,
                        output_width,
                        output_height,
                        codec_stride(output_codec, output_width),
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
                    Some((payload, frame_codec, None))
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

            let Some((network_payload, network_codec, preview_data_length)) = network_frame else {
                thread::sleep(Duration::from_millis(1));
                continue;
            };

            let mut frame = OMTFrame::new(OMTFrameType::Video);
            frame.header.timestamp = monotonic_100ns();
            frame.video_header = Some(OMTVideoHeader {
                codec: network_codec as i32,
                width: frame_width as i32,
                height: frame_height as i32,
                frame_rate_n: output_rate_n as i32,
                frame_rate_d: output_rate_d as i32,
                aspect_ratio: frame_width as f32 / frame_height as f32,
                flags: 0,
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
                    output_rate_n,
                    output_rate_d
                );
                startup_debug_frames += 1;
            }
            if send.enqueue_video(frame) {
                frame_count += 1;
                sent_bytes += payload_len;
                fps_window_frames += 1;
            } else {
                thread::sleep(Duration::from_millis(1));
                continue;
            }

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

            if preview.enabled {
                let now = Instant::now();
                for sink in preview_sinks.iter_mut() {
                    if sink.child.try_wait().ok().flatten().is_some() {
                        let _ = restart_preview_sink(sink);
                    }
                    if sink.interval_ms == 0
                        || now.duration_since(sink.last_sent).as_millis() as u64 >= sink.interval_ms
                    {
                        if sink.stdin.write_all(raw_data).is_err() {
                            let _ = restart_preview_sink(sink);
                        } else {
                            sink.last_sent = now;
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

        for sink in preview_sinks.iter_mut() {
            let _ = sink.child.kill();
            let _ = sink.child.wait();
        }

        let _ = input_frame_bytes;
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
            OMTCodec::YUY2 => root::VMX_EncodeYUY2(inst, ptr, stride, 0),
            OMTCodec::NV12 => {
                let y_bytes = (stride as usize).saturating_mul(height as usize);
                let uv_ptr = ptr.add(y_bytes);
                root::VMX_EncodeNV12(inst, ptr, stride, uv_ptr, stride, 0)
            }
            OMTCodec::BGRA => root::VMX_EncodeBGRA(inst, ptr, stride, 0),
            OMTCodec::P216 => root::VMX_EncodeP216(inst, ptr, stride, 0),
            _ => root::VMX_ERR_VMX_ERR_INVALID_CODEC_FORMAT,
        }
    }

    fn codec_to_vmx_image_format(codec: OMTCodec) -> Option<root::VMX_IMAGE_FORMAT> {
        match codec {
            OMTCodec::UYVY => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_UYVY),
            OMTCodec::YUY2 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_YUY2),
            OMTCodec::NV12 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_NV12),
            OMTCodec::BGRA => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_BGRA),
            OMTCodec::P216 => Some(root::VMX_IMAGE_FORMAT_VMX_IMAGE_P216),
            _ => None,
        }
    }

    fn vmx_profile_from_quality_level(level: u8) -> root::VMX_PROFILE {
        match level {
            3.. => root::VMX_PROFILE_VMX_PROFILE_OMT_HQ,
            2 => root::VMX_PROFILE_VMX_PROFILE_OMT_SQ,
            1 => root::VMX_PROFILE_VMX_PROFILE_OMT_LQ,
            _ => root::VMX_PROFILE_VMX_PROFILE_OMT_SQ,
        }
    }

    fn codec_stride(codec: OMTCodec, width: u32) -> u32 {
        match codec {
            OMTCodec::NV12 => width,
            OMTCodec::BGRA => width * 4,
            OMTCodec::P216 => width * 4,
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
        match codec {
            "UYVY" => Some(OMTCodec::UYVY),
            "YUY2" => Some(OMTCodec::YUY2),
            "NV12" => Some(OMTCodec::NV12),
            "BGRA" => Some(OMTCodec::BGRA),
            "P216" => Some(OMTCodec::P216),
            _ => None,
        }
    }

    fn codec_to_name(codec: OMTCodec) -> &'static str {
        match codec {
            OMTCodec::UYVY => "UYVY",
            OMTCodec::YUY2 => "YUY2",
            OMTCodec::NV12 => "NV12",
            OMTCodec::BGRA => "BGRA",
            OMTCodec::P216 => "P216",
            OMTCodec::VMX1 => "VMX1",
            _ => "UNKNOWN",
        }
    }

    fn codec_to_pix_fmt(codec: OMTCodec) -> &'static str {
        match codec {
            OMTCodec::UYVY => "uyvy422",
            OMTCodec::YUY2 => "yuyv422",
            OMTCodec::NV12 => "nv12",
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
            "bgra" => Some(OMTCodec::BGRA),
            "p216le" => Some(OMTCodec::P216),
            _ => None,
        }
    }

    fn frame_size_bytes(codec: OMTCodec, width: u32, height: u32) -> usize {
        let pixels = width as usize * height as usize;
        match codec {
            OMTCodec::NV12 => pixels * 3 / 2,
            OMTCodec::BGRA => pixels * 4,
            OMTCodec::P216 => pixels * 4,
            _ => pixels * 2,
        }
    }

    fn monotonic_100ns() -> i64 {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let start = START.get_or_init(Instant::now);
        (start.elapsed().as_nanos() / 100) as i64
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

        let mut outputs = preview.output_devices.clone();
        if outputs.is_empty() && !preview.output_device.is_empty() {
            outputs.push(preview.output_device.clone());
        }

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

        let interval_ms = if preview.fps == 0 {
            0
        } else {
            1000 / preview.fps as u64
        };
        let mut sinks = Vec::new();

        for output in outputs {
            let mut cmd = Command::new("ffmpeg");
            let input_rate = if settings.frame_rate_d == 0 {
                "30".to_string()
            } else {
                format!("{}/{}", settings.frame_rate_n, settings.frame_rate_d)
            };

            cmd.args([
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                pix_fmt,
                "-s",
                &format!("{}x{}", input_width, input_height),
                "-r",
                &input_rate,
                "-i",
                "pipe:0",
                "-vf",
                &format!(
                    "scale={}:{}:flags=fast_bilinear,format={}",
                    preview.width, preview.height, preview.pixel_format
                ),
                "-f",
                "fbdev",
                &output,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

            if let Ok(mut child) = cmd.spawn() {
                if let Some(stdin) = child.stdin.take() {
                    sinks.push(PreviewSink {
                        output,
                        pix_fmt: pix_fmt.to_string(),
                        input_rate: input_rate.clone(),
                        input_width,
                        input_height,
                        preview_width: preview.width,
                        preview_height: preview.height,
                        preview_format: preview.pixel_format.clone(),
                        stdin,
                        child,
                        last_sent: Instant::now(),
                        interval_ms,
                    });
                }
            }
        }

        sinks
    }

    fn restart_preview_sink(sink: &mut PreviewSink) -> Result<(), ()> {
        let _ = sink.child.kill();
        let _ = sink.child.wait();

        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            &sink.pix_fmt,
            "-s",
            &format!("{}x{}", sink.input_width, sink.input_height),
            "-r",
            &sink.input_rate,
            "-i",
            "pipe:0",
            "-vf",
            &format!(
                "scale={}:{}:flags=fast_bilinear,format={}",
                sink.preview_width, sink.preview_height, sink.preview_format
            ),
            "-f",
            "fbdev",
            &sink.output,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|_| ())?;
        let stdin = child.stdin.take().ok_or(())?;
        sink.stdin = stdin;
        sink.child = child;
        sink.last_sent = Instant::now();
        Ok(())
    }

    fn fourcc_to_codec(fourcc: FourCC) -> OMTCodec {
        match std::str::from_utf8(&fourcc.repr).unwrap_or("YUYV") {
            "UYVY" => OMTCodec::UYVY,
            "YUYV" | "YUY2" => OMTCodec::YUY2,
            "NV12" => OMTCodec::NV12,
            "P216" => OMTCodec::P216,
            "UYVA" => OMTCodec::UYVA,
            "BGRA" => OMTCodec::BGRA,
            _ => OMTCodec::UYVY,
        }
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
