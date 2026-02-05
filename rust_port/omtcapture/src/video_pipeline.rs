use std::sync::Arc;
use tokio::sync::broadcast;
use libomtnet::OMTFrame;
use std::thread;
use std::time::Duration;

use crate::settings::VideoSettings;

pub struct VideoPipeline {
    settings: VideoSettings,
    tx: broadcast::Sender<OMTFrame>,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl VideoPipeline {
    pub fn new(settings: VideoSettings, tx: broadcast::Sender<OMTFrame>) -> Self {
        VideoPipeline {
            settings,
            tx,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread_handle: None,
        }
    }

    pub fn start(&mut self) {
        self.running.store(true, std::sync::atomic::Ordering::SeqCst);
        let running = self.running.clone();
        let settings = self.settings.clone();
        let tx = self.tx.clone();

        self.thread_handle = Some(thread::spawn(move || {
            #[cfg(target_os = "linux")]
            linux::run_video_loop(running, settings, tx);

            #[cfg(not(target_os = "linux"))]
            stub::run_video_loop(running, settings, tx);
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
    pub fn run_video_loop(running: Arc<std::sync::atomic::AtomicBool>, _settings: VideoSettings, _tx: broadcast::Sender<OMTFrame>) {
        println!("Video capture available on Linux only. Stubbing for macOS.");
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use v4l::prelude::*;
    use v4l::format::FourCC;
    use v4l::buffer::Type;
    use v4l::io::traits::CaptureConfigurable;

    pub fn run_video_loop(running: Arc<std::sync::atomic::AtomicBool>, settings: VideoSettings, tx: broadcast::Sender<OMTFrame>) {
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
        fmt.fourcc = FourCC::new(settings.codec.as_bytes());
        if let Err(e) = dev.set_format(&fmt) {
            eprintln!("Failed to set video format: {}", e);
        }
        
        // Start streaming
        let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 4) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to create video stream: {}", e);
                return;
            }
        };

        // Initialize libvmx if not using native format
        let mut vmx_instance: Option<*mut libvmx_sys::VMX_INSTANCE> = None;
        if !settings.use_native_format {
            unsafe {
                let size = libvmx_sys::VMX_SIZE {
                    width: settings.width as i32,
                    height: settings.height as i32,
                };
                vmx_instance = Some(libvmx_sys::VMX_Create(
                    size,
                    libvmx_sys::VMX_PROFILE_VMX_PROFILE_DEFAULT,
                    libvmx_sys::VMX_COLORSPACE_VMX_COLORSPACE_BT709,
                ));
                println!("libvmx encoder initialized.");
            }
        }

        let mut compress_buffer = vec![0u8; (settings.width * settings.height * 2) as usize];

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let (data, _) = match stream.next() {
                Ok(res) => res,
                Err(e) => {
                    eprintln!("Failed to read video frame: {}", e);
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };

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
                        "UYVY" => libvmx_sys::VMX_EncodeUYVY(inst, data.as_ptr() as *mut _, stride, 0),
                        "YUY2" => libvmx_sys::VMX_EncodeYUY2(inst, data.as_ptr() as *mut _, stride, 0),
                        _ => libvmx_sys::VMX_ERR_VMX_ERR_INVALID_CODEC_FORMAT,
                    };

                    if err == libvmx_sys::VMX_ERR_VMX_ERR_OK {
                        let compressed_len = libvmx_sys::VMX_SaveTo(inst, compress_buffer.as_mut_ptr(), compress_buffer.len() as i32);
                        if compressed_len > 0 {
                            final_data = bytes::Bytes::copy_from_slice(&compress_buffer[..compressed_len as usize]);
                            codec = OMTCodec::VMX1 as i32;
                        } else {
                            final_data = bytes::Bytes::copy_from_slice(data);
                        }
                    } else {
                        final_data = bytes::Bytes::copy_from_slice(data);
                    }
                }
            } else {
                final_data = bytes::Bytes::copy_from_slice(data);
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

            if let Err(_) = tx.send(frame) {
                // No receivers
            }
        }

        // Cleanup
        if let Some(inst) = vmx_instance {
            unsafe {
                libvmx_sys::VMX_Destroy(inst);
            }
        }
    }
}
