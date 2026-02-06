use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::settings::{PreviewSettings, VideoSettings};
use crate::send_coordinator::SendCoordinator;

pub struct VideoPipeline {
    settings: VideoSettings,
    send: SendCoordinator,
    preview: PreviewSettings,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl VideoPipeline {
    pub fn new(settings: VideoSettings, preview: PreviewSettings, send: SendCoordinator) -> Self {
        VideoPipeline {
            settings,
            send,
            preview,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread_handle: None,
        }
    }

    pub fn start(&mut self) {
        self.running.store(true, std::sync::atomic::Ordering::SeqCst);
        let running = self.running.clone();
        let settings = self.settings.clone();
        let preview = self.preview.clone();
        let send = self.send.clone();

        self.thread_handle = Some(thread::spawn(move || {
            #[cfg(target_os = "linux")]
            linux::run_video_loop(running, settings, preview, send);

            #[cfg(not(target_os = "linux"))]
            stub::run_video_loop(running, settings, preview, send);
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
    pub fn run_video_loop(running: Arc<std::sync::atomic::AtomicBool>, _settings: VideoSettings, _preview: PreviewSettings, _send: SendCoordinator) {
        println!("Video capture available on Linux only. Stubbing for macOS.");
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use libomtnet::{OMTCodec, OMTFrame, OMTFrameType, OMTVideoHeader};
    use libvmx_sys::root::{
        VMX_INSTANCE,
        VMX_SIZE,
        VMX_Create,
        VMX_Destroy,
        VMX_EncodeUYVY,
        VMX_EncodeYUY2,
        VMX_SaveTo,
        VMX_PROFILE_VMX_PROFILE_DEFAULT,
        VMX_COLORSPACE_VMX_COLORSPACE_BT709,
        VMX_ERR_VMX_ERR_INVALID_CODEC_FORMAT,
        VMX_ERR_VMX_ERR_OK,
    };
    use v4l::prelude::*;
    use v4l::format::FourCC;
    use v4l::buffer::Type;
    use v4l::io::traits::CaptureStream;
    use v4l::video::Capture;
    use std::io::Write;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::time::Instant;

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

    pub fn run_video_loop(running: Arc<std::sync::atomic::AtomicBool>, settings: VideoSettings, preview: PreviewSettings, send: SendCoordinator) {
        println!("Starting Linux V4L2 pipeline on {}...", settings.device_path);
        
        // Open device
        let dev = match Device::with_path(&settings.device_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open video device: {}", e);
                return;
            }
        };

        // Set format
        let mut fmt = dev.format().unwrap();
        fmt.width = settings.width;
        fmt.height = settings.height;
        let mut codec_bytes = *b"YUYV";
        let codec_src = settings.codec.as_bytes();
        if codec_src.len() >= 4 {
            codec_bytes.copy_from_slice(&codec_src[0..4]);
        }
        fmt.fourcc = FourCC::new(&codec_bytes);
        if let Err(e) = dev.set_format(&fmt) {
            eprintln!("Failed to set video format: {}", e);
        }
        let fmt = dev.format().unwrap_or(fmt);
        let input_width = fmt.width;
        let input_height = fmt.height;
        let input_fourcc = fmt.fourcc;
        
        // Start streaming
        let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 4) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to create video stream: {}", e);
                return;
            }
        };

        // Initialize libvmx if not using native format
        let mut vmx_instance: Option<*mut VMX_INSTANCE> = None;
        if !settings.use_native_format {
            unsafe {
                let size = VMX_SIZE {
                    width: settings.width as i32,
                    height: settings.height as i32,
                };
                vmx_instance = Some(VMX_Create(
                    size,
                    VMX_PROFILE_VMX_PROFILE_DEFAULT,
                    VMX_COLORSPACE_VMX_COLORSPACE_BT709,
                ));
                println!("libvmx encoder initialized.");
            }
        }

        let mut compress_buffer = vec![0u8; (settings.width * settings.height * 2) as usize];

        let mut preview_sinks = build_preview_sinks(&settings, &preview, input_width, input_height, input_fourcc);

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let (data, _) = match stream.next() {
                Ok(res) => res,
                Err(e) => {
                    eprintln!("Failed to read video frame: {}", e);
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };

            let raw_data = data;

            let mut frame = OMTFrame::new(OMTFrameType::Video);
            frame.header.timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64 / 100;
            
            let mut codec = match &settings.codec as &str {
                "UYVY" => OMTCodec::UYVY as i32,
                "YUY2" => OMTCodec::YUY2 as i32,
                "NV12" => OMTCodec::NV12 as i32,
                _ => OMTCodec::UYVY as i32,
            };

            let final_data: bytes::Bytes;

            if let Some(inst) = vmx_instance {
                unsafe {
                    let stride = (settings.width * 2) as i32;
                    let err = match &settings.codec as &str {
                        "UYVY" => VMX_EncodeUYVY(inst, raw_data.as_ptr() as *mut _, stride, 0),
                        "YUY2" => VMX_EncodeYUY2(inst, raw_data.as_ptr() as *mut _, stride, 0),
                        _ => VMX_ERR_VMX_ERR_INVALID_CODEC_FORMAT,
                    };

                    if err == VMX_ERR_VMX_ERR_OK {
                        let compressed_len = VMX_SaveTo(inst, compress_buffer.as_mut_ptr(), compress_buffer.len() as i32);
                        if compressed_len > 0 {
                            final_data = bytes::Bytes::copy_from_slice(&compress_buffer[..compressed_len as usize]);
                            codec = OMTCodec::VMX1 as i32;
                        } else {
                            final_data = bytes::Bytes::copy_from_slice(raw_data);
                        }
                    } else {
                        final_data = bytes::Bytes::copy_from_slice(raw_data);
                    }
                }
            } else {
                final_data = bytes::Bytes::copy_from_slice(raw_data);
            }

            frame.video_header = Some(OMTVideoHeader {
                codec,
                width: fmt.width as i32,
                height: fmt.height as i32,
                frame_rate_n: settings.frame_rate_n as i32,
                frame_rate_d: settings.frame_rate_d as i32,
                aspect_ratio: fmt.width as f32 / fmt.height as f32,
                flags: 0,
                color_space: 709,
            });

            frame.data = final_data;
            frame.update_data_length();

            send.enqueue_video(frame);

            if preview.enabled {
                let now = Instant::now();
                for sink in preview_sinks.iter_mut() {
                    if sink.child.try_wait().ok().flatten().is_some() {
                        let _ = restart_preview_sink(sink);
                    }
                    if sink.interval_ms == 0 || now.duration_since(sink.last_sent).as_millis() as u64 >= sink.interval_ms {
                        if sink.stdin.write_all(raw_data).is_err() {
                            let _ = restart_preview_sink(sink);
                        } else {
                            sink.last_sent = now;
                        }
                    }
                }
            }
        }

        for sink in preview_sinks.iter_mut() {
            let _ = sink.child.kill();
            let _ = sink.child.wait();
        }

        // Cleanup
        if let Some(inst) = vmx_instance {
            unsafe {
                VMX_Destroy(inst);
            }
        }
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
            "YUY2" => "yuyv422",
            "NV12" => "nv12",
            _ => match settings.codec.as_str() {
                "UYVY" => "uyvy422",
                "YUY2" => "yuyv422",
                "NV12" => "nv12",
                _ => "uyvy422",
            },
        };

        let interval_ms = if preview.fps == 0 { 0 } else { 1000 / preview.fps as u64 };
        let mut sinks = Vec::new();

        for output in outputs {
            let mut cmd = Command::new("ffmpeg");
            let input_rate = if settings.frame_rate_d == 0 {
            "30".to_string()
        } else {
            format!("{}/{}", settings.frame_rate_n, settings.frame_rate_d)
        };
            cmd.args([
            "-loglevel", "error",
                "-f", "rawvideo",
                "-pix_fmt", pix_fmt,
                "-s", &format!("{}x{}", input_width, input_height),
                "-r", &input_rate,
                "-i", "pipe:0",
                "-vf", &format!("scale={}:{}:flags=fast_bilinear,format={}", preview.width, preview.height, preview.pixel_format),
                "-f", "fbdev",
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
            "-loglevel", "error",
            "-f", "rawvideo",
            "-pix_fmt", &sink.pix_fmt,
            "-s", &format!("{}x{}", sink.input_width, sink.input_height),
            "-r", &sink.input_rate,
            "-i", "pipe:0",
            "-vf", &format!("scale={}:{}:flags=fast_bilinear,format={}", sink.preview_width, sink.preview_height, sink.preview_format),
            "-f", "fbdev",
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
}
