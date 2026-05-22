use std::sync::atomic::AtomicU8;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::send_coordinator::SendCoordinator;
use crate::settings::{PreviewSettings, VideoSettings};

/// Shared raw frame for web preview snapshot generation.
#[derive(Clone)]
pub struct SharedPreviewFrame {
    pub data: Arc<std::sync::Mutex<Option<PreviewFrameData>>>,
}

pub struct PreviewFrameData {
    pub raw: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pix_fmt: String,
}

impl SharedPreviewFrame {
    pub fn new() -> Self {
        Self {
            data: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

pub struct VideoPipeline {
    settings: Arc<tokio::sync::RwLock<VideoSettings>>,
    preview: Arc<tokio::sync::RwLock<PreviewSettings>>,
    suggested_quality_hint: Arc<AtomicU8>,
    active_quality_mask: Arc<AtomicU8>,
    active_codec_mask: Arc<AtomicU8>,
    running: Arc<std::sync::atomic::AtomicBool>,
    restart_requested: Arc<std::sync::atomic::AtomicBool>,
    preview_restart_requested: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
    send: SendCoordinator,
    pub shared_frame: SharedPreviewFrame,
}

impl VideoPipeline {
    pub fn new(
        settings: VideoSettings,
        preview: PreviewSettings,
        send: SendCoordinator,
        suggested_quality_hint: Arc<AtomicU8>,
        active_quality_mask: Arc<AtomicU8>,
        active_codec_mask: Arc<AtomicU8>,
    ) -> Self {
        VideoPipeline {
            settings: Arc::new(tokio::sync::RwLock::new(settings)),
            preview: Arc::new(tokio::sync::RwLock::new(preview)),
            suggested_quality_hint,
            active_quality_mask,
            active_codec_mask,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            restart_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            preview_restart_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread_handle: None,
            send,
            shared_frame: SharedPreviewFrame::new(),
        }
    }

    pub fn start(&mut self) {
        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let running = self.running.clone();
        let send = self.send.clone();
        let suggested_quality_hint = self.suggested_quality_hint.clone();
        let active_quality_mask = self.active_quality_mask.clone();
        let active_codec_mask = self.active_codec_mask.clone();
        let shared_frame = self.shared_frame.clone();

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
                        active_quality_mask.clone(),
                        active_codec_mask.clone(),
                        shared_frame.clone(),
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
    use memmap2::MmapOptions;
    use std::collections::HashSet;
    use std::ffi::CStr;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt;
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

    struct HwEncoderContext {
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        codec: OMTCodec,
        read_buf: Vec<u8>,
        header_skipped: bool,
    }

    enum CaptureSource<'a> {
        V4l { stream: MmapStream<'a> },
        Mplane(MplaneCapture),
        Ffmpeg(FfmpegCapture),
    }

    struct FfmpegCapture {
        child: Child,
        stdout: ChildStdout,
        frame_buf: Vec<u8>,
    }

    const VIDEO_MAX_PLANES: usize = 8;
    const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
    const V4L2_MEMORY_MMAP: u32 = 1;
    const V4L2_FIELD_NONE: u32 = 1;
    const V4L2_PIX_FMT_BGR24: u32 = u32::from_le_bytes(*b"BGR3");
    const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
    const DRM_MODE_CONNECTED: u32 = 1;
    const DRM_FORMAT_BGR888: u32 = u32::from_le_bytes(*b"BG24");
    const DRM_PLANE_TYPE_PRIMARY: i32 = 1;
    const DRM_MODE_OBJECT_PLANE: u32 = 0xeeeeeeee;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct V4l2PlanePixFormat {
        sizeimage: u32,
        bytesperline: u32,
        reserved: [u16; 6],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct V4l2PixFormatMplane {
        width: u32,
        height: u32,
        pixelformat: u32,
        field: u32,
        colorspace: u32,
        plane_fmt: [V4l2PlanePixFormat; VIDEO_MAX_PLANES],
        num_planes: u8,
        flags: u8,
        ycbcr_enc: u8,
        quantization: u8,
        xfer_func: u8,
        reserved: [u8; 7],
    }

    impl Default for V4l2PixFormatMplane {
        fn default() -> Self {
            Self {
                width: 0,
                height: 0,
                pixelformat: 0,
                field: 0,
                colorspace: 0,
                plane_fmt: [V4l2PlanePixFormat::default(); VIDEO_MAX_PLANES],
                num_planes: 0,
                flags: 0,
                ycbcr_enc: 0,
                quantization: 0,
                xfer_func: 0,
                reserved: [0; 7],
            }
        }
    }

    #[repr(C)]
    struct V4l2Format {
        type_: u32,
        // v4l2_format has a 200-byte union at offset 8 on aarch64. We only use
        // pix_mp, but the ioctl request size must match the C struct exactly.
        _pad: u32,
        fmt: V4l2PixFormatMplane,
        _fmt_padding: [u8; 8],
    }

    impl Default for V4l2Format {
        fn default() -> Self {
            Self {
                type_: 0,
                _pad: 0,
                fmt: V4l2PixFormatMplane::default(),
                _fmt_padding: [0; 8],
            }
        }
    }

    #[repr(C)]
    #[derive(Default)]
    struct V4l2RequestBuffers {
        count: u32,
        type_: u32,
        memory: u32,
        capabilities: u32,
        flags: u8,
        reserved: [u8; 3],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct V4l2Plane {
        bytesused: u32,
        length: u32,
        m: u64,
        data_offset: u32,
        reserved: [u32; 11],
    }

    impl Default for V4l2Plane {
        fn default() -> Self {
            Self {
                bytesused: 0,
                length: 0,
                m: 0,
                data_offset: 0,
                reserved: [0; 11],
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct V4l2TimeCode {
        type_: u32,
        flags: u32,
        frames: u8,
        seconds: u8,
        minutes: u8,
        hours: u8,
        userbits: [u8; 4],
    }

    #[repr(C)]
    struct V4l2Buffer {
        index: u32,
        type_: u32,
        bytesused: u32,
        flags: u32,
        field: u32,
        timestamp: libc::timeval,
        timecode: V4l2TimeCode,
        sequence: u32,
        memory: u32,
        m: u64,
        length: u32,
        reserved2: u32,
        request_fd: i32,
    }

    impl Default for V4l2Buffer {
        fn default() -> Self {
            Self {
                index: 0,
                type_: 0,
                bytesused: 0,
                flags: 0,
                field: 0,
                timestamp: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                timecode: V4l2TimeCode::default(),
                sequence: 0,
                memory: 0,
                m: 0,
                length: 0,
                reserved2: 0,
                request_fd: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Default)]
    struct V4l2ExportBuffer {
        type_: u32,
        index: u32,
        plane: u32,
        flags: u32,
        fd: i32,
        reserved: [u32; 11],
    }

    nix::ioctl_readwrite!(vidioc_g_fmt, b'V', 4, V4l2Format);
    nix::ioctl_readwrite!(vidioc_s_fmt, b'V', 5, V4l2Format);
    nix::ioctl_readwrite!(vidioc_reqbufs, b'V', 8, V4l2RequestBuffers);
    nix::ioctl_readwrite!(vidioc_querybuf, b'V', 9, V4l2Buffer);
    nix::ioctl_readwrite!(vidioc_qbuf, b'V', 15, V4l2Buffer);
    nix::ioctl_readwrite!(vidioc_expbuf, b'V', 16, V4l2ExportBuffer);
    nix::ioctl_readwrite!(vidioc_dqbuf, b'V', 17, V4l2Buffer);
    nix::ioctl_write_ptr!(vidioc_streamon, b'V', 18, u32);
    nix::ioctl_write_ptr!(vidioc_streamoff, b'V', 19, u32);

    #[repr(C)]
    struct DrmModeRes {
        count_fbs: libc::c_int,
        fbs: *mut u32,
        count_crtcs: libc::c_int,
        crtcs: *mut u32,
        count_connectors: libc::c_int,
        connectors: *mut u32,
        count_encoders: libc::c_int,
        encoders: *mut u32,
        min_width: u32,
        max_width: u32,
        min_height: u32,
        max_height: u32,
    }

    #[repr(C)]
    struct DrmModeConnector {
        connector_id: u32,
        encoder_id: u32,
        connector_type: u32,
        connector_type_id: u32,
        connection: u32,
        mm_width: u32,
        mm_height: u32,
        subpixel: u32,
        count_modes: libc::c_int,
        modes: *mut libc::c_void,
        count_props: libc::c_int,
        props: *mut u32,
        prop_values: *mut u64,
        count_encoders: libc::c_int,
        encoders: *mut u32,
    }

    #[repr(C)]
    struct DrmModeEncoder {
        encoder_id: u32,
        encoder_type: u32,
        crtc_id: u32,
        possible_crtcs: u32,
        possible_clones: u32,
    }

    #[repr(C)]
    struct DrmModePlaneRes {
        count_planes: u32,
        planes: *mut u32,
    }

    #[repr(C)]
    struct DrmModePlane {
        count_formats: u32,
        formats: *mut u32,
        plane_id: u32,
        crtc_id: u32,
        fb_id: u32,
        crtc_x: u32,
        crtc_y: u32,
        x: u32,
        y: u32,
        possible_crtcs: u32,
        gamma_size: u32,
    }

    #[repr(C)]
    struct DrmModeObjectProperties {
        count_props: u32,
        props: *mut u32,
        prop_values: *mut u64,
    }

    #[repr(C)]
    struct DrmModePropertyRes {
        prop_id: u32,
        flags: u32,
        name: [libc::c_char; 32],
        count_values: libc::c_int,
        values: *mut u64,
        count_enums: libc::c_int,
        enums: *mut libc::c_void,
        count_blobs: libc::c_int,
        blob_ids: *mut u32,
    }

    #[link(name = "drm")]
    extern "C" {
        fn drmSetClientCap(fd: libc::c_int, capability: u64, value: u64) -> libc::c_int;
        fn drmModeGetResources(fd: libc::c_int) -> *mut DrmModeRes;
        fn drmModeFreeResources(ptr: *mut DrmModeRes);
        fn drmModeGetConnector(fd: libc::c_int, connector_id: u32) -> *mut DrmModeConnector;
        fn drmModeFreeConnector(ptr: *mut DrmModeConnector);
        fn drmModeGetEncoder(fd: libc::c_int, encoder_id: u32) -> *mut DrmModeEncoder;
        fn drmModeFreeEncoder(ptr: *mut DrmModeEncoder);
        fn drmModeGetPlaneResources(fd: libc::c_int) -> *mut DrmModePlaneRes;
        fn drmModeFreePlaneResources(ptr: *mut DrmModePlaneRes);
        fn drmModeGetPlane(fd: libc::c_int, plane_id: u32) -> *mut DrmModePlane;
        fn drmModeFreePlane(ptr: *mut DrmModePlane);
        fn drmModeObjectGetProperties(
            fd: libc::c_int,
            object_id: u32,
            object_type: u32,
        ) -> *mut DrmModeObjectProperties;
        fn drmModeFreeObjectProperties(ptr: *mut DrmModeObjectProperties);
        fn drmModeGetProperty(fd: libc::c_int, property_id: u32) -> *mut DrmModePropertyRes;
        fn drmModeFreeProperty(ptr: *mut DrmModePropertyRes);
        fn drmPrimeFDToHandle(
            fd: libc::c_int,
            prime_fd: libc::c_int,
            handle: *mut u32,
        ) -> libc::c_int;
        fn drmModeAddFB2(
            fd: libc::c_int,
            width: u32,
            height: u32,
            pixel_format: u32,
            bo_handles: *const u32,
            pitches: *const u32,
            offsets: *const u32,
            buf_id: *mut u32,
            flags: u32,
        ) -> libc::c_int;
        fn drmModeRmFB(fd: libc::c_int, buffer_id: u32) -> libc::c_int;
        fn drmModeSetPlane(
            fd: libc::c_int,
            plane_id: u32,
            crtc_id: u32,
            fb_id: u32,
            flags: u32,
            crtc_x: i32,
            crtc_y: i32,
            crtc_w: u32,
            crtc_h: u32,
            src_x: u32,
            src_y: u32,
            src_w: u32,
            src_h: u32,
        ) -> libc::c_int;
    }

    struct MplaneBuffer {
        map: memmap2::MmapMut,
        length: usize,
        dma_fd: Option<File>,
        drm_handle: u32,
        fb_id: u32,
    }

    struct DrmMirror {
        file: File,
        crtc_id: u32,
        plane_id: u32,
        width: u32,
        height: u32,
    }

    struct MplaneCapture {
        file: File,
        buffers: Vec<MplaneBuffer>,
        frame_size: usize,
        mirror: Option<DrmMirror>,
        mirror_errors: u32,
        frames: usize,
        window_start: Instant,
    }

    impl Drop for FfmpegCapture {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl MplaneCapture {
        fn start(
            settings: &VideoSettings,
            width: u32,
            height: u32,
            enable_drm_mirror: bool,
        ) -> Result<(Self, u32, u32, u32), String> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&settings.device_path)
                .map_err(|e| format!("open mplane capture {}: {}", settings.device_path, e))?;
            let fd = file.as_raw_fd();

            let mut fmt = V4l2Format::default();
            fmt.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            fmt.fmt.width = width.max(1);
            fmt.fmt.height = height.max(1);
            fmt.fmt.pixelformat = V4L2_PIX_FMT_BGR24;
            fmt.fmt.field = V4L2_FIELD_NONE;
            fmt.fmt.num_planes = 1;
            if unsafe { vidioc_s_fmt(fd, &mut fmt) }.is_err() {
                // Some drivers reject S_FMT when already fixed; G_FMT below decides support.
                fmt = V4l2Format::default();
                fmt.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            }
            unsafe { vidioc_g_fmt(fd, &mut fmt) }
                .map_err(|e| format!("VIDIOC_G_FMT mplane: {}", e))?;

            if fmt.fmt.pixelformat != V4L2_PIX_FMT_BGR24 || fmt.fmt.num_planes == 0 {
                return Err(format!(
                    "unsupported mplane format {} planes={}",
                    fourcc_u32_to_string(fmt.fmt.pixelformat),
                    fmt.fmt.num_planes
                ));
            }

            let width = fmt.fmt.width;
            let height = fmt.fmt.height;
            let pitch = fmt.fmt.plane_fmt[0].bytesperline;
            let frame_size = fmt.fmt.plane_fmt[0].sizeimage as usize;
            if pitch == 0 || frame_size == 0 {
                return Err("invalid mplane pitch/frame size".to_string());
            }

            let mut req = V4l2RequestBuffers {
                count: 4,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                memory: V4L2_MEMORY_MMAP,
                ..Default::default()
            };
            unsafe { vidioc_reqbufs(fd, &mut req) }
                .map_err(|e| format!("VIDIOC_REQBUFS mplane: {}", e))?;
            if req.count < 2 {
                return Err(format!("not enough mplane buffers: {}", req.count));
            }

            let mut mirror = if enable_drm_mirror {
                match DrmMirror::open(width, height) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        eprintln!("Direct DRM HDMI monitor unavailable: {}", e);
                        None
                    }
                }
            } else {
                None
            };

            let mut buffers = Vec::new();
            for index in 0..req.count {
                let mut planes = [V4l2Plane::default(); VIDEO_MAX_PLANES];
                let mut buf = V4l2Buffer {
                    type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                    memory: V4L2_MEMORY_MMAP,
                    index,
                    length: VIDEO_MAX_PLANES as u32,
                    m: planes.as_mut_ptr() as u64,
                    ..Default::default()
                };
                unsafe { vidioc_querybuf(fd, &mut buf) }
                    .map_err(|e| format!("VIDIOC_QUERYBUF mplane: {}", e))?;
                let length = planes[0].length as usize;
                let offset = planes[0].m as u64;
                let map = unsafe {
                    MmapOptions::new()
                        .len(length)
                        .offset(offset)
                        .map_mut(&file)
                        .map_err(|e| format!("mmap mplane buffer: {}", e))?
                };

                let mut dma_fd = None;
                let mut drm_handle = 0u32;
                let mut fb_id = 0u32;
                if let Some(ref mut drm) = mirror {
                    let mut exp = V4l2ExportBuffer {
                        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                        index,
                        plane: 0,
                        flags: libc::O_CLOEXEC as u32,
                        ..Default::default()
                    };
                    unsafe { vidioc_expbuf(fd, &mut exp) }
                        .map_err(|e| format!("VIDIOC_EXPBUF mplane: {}", e))?;
                    let exported = unsafe { File::from_raw_fd(exp.fd) };
                    drm.add_framebuffer(exported.as_raw_fd(), pitch, &mut drm_handle, &mut fb_id)
                        .map_err(|e| format!("DRM add framebuffer: {}", e))?;
                    dma_fd = Some(exported);
                }

                unsafe { vidioc_qbuf(fd, &mut buf) }
                    .map_err(|e| format!("VIDIOC_QBUF mplane: {}", e))?;

                buffers.push(MplaneBuffer {
                    map,
                    length,
                    dma_fd,
                    drm_handle,
                    fb_id,
                });
            }

            let type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            unsafe { vidioc_streamon(fd, &type_ as *const u32) }
                .map_err(|e| format!("VIDIOC_STREAMON mplane: {}", e))?;

            println!(
                "V4L2 multiplanar capture started: {}x{} BGR3 pitch={} buffers={} drm_mirror={}",
                width,
                height,
                pitch,
                buffers.len(),
                mirror.is_some()
            );

            Ok((
                Self {
                    file,
                    buffers,
                    frame_size,
                    mirror,
                    mirror_errors: 0,
                    frames: 0,
                    window_start: Instant::now(),
                },
                width,
                height,
                pitch,
            ))
        }

        fn next_frame(&mut self, copy_frame: bool) -> Result<Option<Bytes>, String> {
            let fd = self.file.as_raw_fd();
            loop {
                let mut planes = [V4l2Plane::default(); VIDEO_MAX_PLANES];
                let mut buf = V4l2Buffer {
                    type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                    memory: V4L2_MEMORY_MMAP,
                    length: VIDEO_MAX_PLANES as u32,
                    m: planes.as_mut_ptr() as u64,
                    ..Default::default()
                };
                match unsafe { vidioc_dqbuf(fd, &mut buf) } {
                    Ok(_) => {
                        let index = buf.index as usize;
                        if index >= self.buffers.len() {
                            return Err(format!("mplane buffer index out of range: {}", index));
                        }
                        let mut disable_mirror = false;
                        if let Some(ref mirror) = self.mirror {
                            match mirror.set_plane(self.buffers[index].fb_id) {
                                Ok(()) => {
                                    self.mirror_errors = 0;
                                }
                                Err(e) => {
                                    self.mirror_errors = self.mirror_errors.saturating_add(1);
                                    if self.mirror_errors == 1 || self.mirror_errors % 60 == 0 {
                                        eprintln!("DRM mirror set plane failed: {}", e);
                                    }
                                    if self.mirror_errors >= 120 {
                                        disable_mirror = true;
                                    }
                                }
                            }
                        }
                        if disable_mirror {
                            eprintln!("Disabling direct DRM HDMI monitor after repeated failures.");
                            self.disable_mirror();
                        }
                        let frame = if copy_frame {
                            let used = planes[0]
                                .bytesused
                                .max(self.frame_size as u32)
                                .min(self.buffers[index].length as u32)
                                as usize;
                            Some(Bytes::copy_from_slice(&self.buffers[index].map[..used]))
                        } else {
                            None
                        };
                        unsafe { vidioc_qbuf(fd, &mut buf) }
                            .map_err(|e| format!("VIDIOC_QBUF mplane: {}", e))?;

                        if self.mirror.is_some() {
                            self.frames += 1;
                            if self.window_start.elapsed() >= Duration::from_secs(10) {
                                let fps =
                                    self.frames as f64 / self.window_start.elapsed().as_secs_f64();
                                println!("Direct DRM HDMI monitor FPS: {:.1}", fps);
                                self.frames = 0;
                                self.window_start = Instant::now();
                            }
                        }
                        return Ok(frame);
                    }
                    Err(nix::errno::Errno::EAGAIN) => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => return Err(format!("VIDIOC_DQBUF mplane: {}", e)),
                }
            }
        }

        fn has_drm_mirror(&self) -> bool {
            self.mirror.is_some()
        }

        fn disable_mirror(&mut self) {
            if let Some(drm) = self.mirror.take() {
                for buffer in self.buffers.iter_mut() {
                    drm.remove_framebuffer(buffer.fb_id);
                    buffer.fb_id = 0;
                    buffer.drm_handle = 0;
                }
            }
            self.mirror_errors = 0;
        }
    }

    impl Drop for MplaneCapture {
        fn drop(&mut self) {
            let type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            let _ = unsafe { vidioc_streamoff(self.file.as_raw_fd(), &type_ as *const u32) };
            self.disable_mirror();
        }
    }

    impl DrmMirror {
        fn open(width: u32, height: u32) -> Result<Self, String> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/dri/card0")
                .map_err(|e| format!("open /dev/dri/card0: {}", e))?;
            let fd = file.as_raw_fd();
            unsafe {
                drmSetClientCap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1);
            }
            let res = unsafe { drmModeGetResources(fd) };
            if res.is_null() {
                return Err("drmModeGetResources failed".to_string());
            }
            let result = unsafe {
                let crtc_id = find_connected_crtc(fd, res)?;
                let plane_id = find_drm_plane(fd, res, crtc_id, DRM_FORMAT_BGR888)?;
                Ok(Self {
                    file,
                    crtc_id,
                    plane_id,
                    width,
                    height,
                })
            };
            unsafe {
                drmModeFreeResources(res);
            }
            if let Ok(ref mirror) = result {
                println!(
                    "Direct DRM HDMI monitor opened: crtc={} plane={} format=BG24",
                    mirror.crtc_id, mirror.plane_id
                );
            }
            result
        }

        fn add_framebuffer(
            &mut self,
            dma_fd: RawFd,
            pitch: u32,
            drm_handle: &mut u32,
            fb_id: &mut u32,
        ) -> Result<(), String> {
            let fd = self.file.as_raw_fd();
            let prime = unsafe { drmPrimeFDToHandle(fd, dma_fd, drm_handle as *mut u32) };
            if prime != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let handles = [*drm_handle, 0, 0, 0];
            let pitches = [pitch, 0, 0, 0];
            let offsets = [0u32, 0, 0, 0];
            let add = unsafe {
                drmModeAddFB2(
                    fd,
                    self.width,
                    self.height,
                    DRM_FORMAT_BGR888,
                    handles.as_ptr(),
                    pitches.as_ptr(),
                    offsets.as_ptr(),
                    fb_id as *mut u32,
                    0,
                )
            };
            if add != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(())
        }

        fn set_plane(&self, fb_id: u32) -> Result<(), String> {
            if fb_id == 0 {
                return Ok(());
            }
            let r = unsafe {
                drmModeSetPlane(
                    self.file.as_raw_fd(),
                    self.plane_id,
                    self.crtc_id,
                    fb_id,
                    0,
                    0,
                    0,
                    self.width,
                    self.height,
                    0,
                    0,
                    self.width << 16,
                    self.height << 16,
                )
            };
            if r != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(())
        }

        fn remove_framebuffer(&self, fb_id: u32) {
            if fb_id != 0 {
                unsafe {
                    drmModeRmFB(self.file.as_raw_fd(), fb_id);
                }
            }
        }
    }

    unsafe fn find_connected_crtc(fd: RawFd, res: *mut DrmModeRes) -> Result<u32, String> {
        let connectors =
            std::slice::from_raw_parts((*res).connectors, (*res).count_connectors.max(0) as usize);
        for &connector_id in connectors {
            let conn = drmModeGetConnector(fd, connector_id);
            if conn.is_null() {
                continue;
            }
            let connected = (*conn).connection == DRM_MODE_CONNECTED && (*conn).count_modes > 0;
            if !connected {
                drmModeFreeConnector(conn);
                continue;
            }
            if (*conn).encoder_id != 0 {
                let enc = drmModeGetEncoder(fd, (*conn).encoder_id);
                if !enc.is_null() {
                    let crtc_id = (*enc).crtc_id;
                    drmModeFreeEncoder(enc);
                    if crtc_id != 0 {
                        drmModeFreeConnector(conn);
                        return Ok(crtc_id);
                    }
                }
            }
            drmModeFreeConnector(conn);
        }
        Err("no connected DRM CRTC found".to_string())
    }

    unsafe fn find_drm_plane(
        fd: RawFd,
        res: *mut DrmModeRes,
        crtc_id: u32,
        format: u32,
    ) -> Result<u32, String> {
        let crtcs = std::slice::from_raw_parts((*res).crtcs, (*res).count_crtcs.max(0) as usize);
        let Some(crtc_index) = crtcs.iter().position(|id| *id == crtc_id) else {
            return Err(format!("CRTC {} not in DRM resources", crtc_id));
        };
        let planes = drmModeGetPlaneResources(fd);
        if planes.is_null() {
            return Err("drmModeGetPlaneResources failed".to_string());
        }
        let plane_ids =
            std::slice::from_raw_parts((*planes).planes, (*planes).count_planes as usize);
        let mut fallback = 0u32;
        for &plane_id in plane_ids {
            let plane = drmModeGetPlane(fd, plane_id);
            if plane.is_null() {
                continue;
            }
            let formats =
                std::slice::from_raw_parts((*plane).formats, (*plane).count_formats as usize);
            let supports_crtc = ((*plane).possible_crtcs & (1 << crtc_index)) != 0;
            let supports_format = formats.contains(&format);
            let type_ = if supports_crtc && supports_format {
                drm_plane_type(fd, plane_id)
            } else {
                -1
            };
            drmModeFreePlane(plane);
            if !supports_crtc || !supports_format {
                continue;
            }
            if type_ == DRM_PLANE_TYPE_PRIMARY {
                drmModeFreePlaneResources(planes);
                return Ok(plane_id);
            }
            if fallback == 0 {
                fallback = plane_id;
            }
        }
        drmModeFreePlaneResources(planes);
        if fallback != 0 {
            Ok(fallback)
        } else {
            Err("no DRM plane supports BG24 on connected CRTC".to_string())
        }
    }

    unsafe fn drm_plane_type(fd: RawFd, plane_id: u32) -> i32 {
        let props = drmModeObjectGetProperties(fd, plane_id, DRM_MODE_OBJECT_PLANE);
        if props.is_null() {
            return -1;
        }
        let prop_ids = std::slice::from_raw_parts((*props).props, (*props).count_props as usize);
        let values =
            std::slice::from_raw_parts((*props).prop_values, (*props).count_props as usize);
        let mut result = -1;
        for (idx, &prop_id) in prop_ids.iter().enumerate() {
            let prop = drmModeGetProperty(fd, prop_id);
            if prop.is_null() {
                continue;
            }
            let name = CStr::from_ptr((*prop).name.as_ptr()).to_string_lossy();
            if name == "type" {
                result = values[idx] as i32;
                drmModeFreeProperty(prop);
                break;
            }
            drmModeFreeProperty(prop);
        }
        drmModeFreeObjectProperties(props);
        result
    }

    impl<'a> CaptureSource<'a> {
        fn next_frame(&mut self, copy_frame: bool) -> Result<Option<Bytes>, String> {
            match self {
                CaptureSource::V4l { stream } => stream
                    .next()
                    .map(|(data, _)| {
                        if copy_frame {
                            Some(Bytes::copy_from_slice(data))
                        } else {
                            None
                        }
                    })
                    .map_err(|e| e.to_string()),
                CaptureSource::Mplane(capture) => capture.next_frame(copy_frame),
                CaptureSource::Ffmpeg(capture) => {
                    if !read_exact(&mut capture.stdout, &mut capture.frame_buf) {
                        return Err("ffmpeg capture stdout ended".to_string());
                    }
                    Ok(if copy_frame {
                        Some(Bytes::copy_from_slice(&capture.frame_buf))
                    } else {
                        None
                    })
                }
            }
        }
    }

    impl HwEncoderContext {
        fn start(
            input_pix_fmt: &str,
            width: u32,
            height: u32,
            fps_n: u32,
            fps_d: u32,
            encoder_name: &str,
            codec_type: &str,
        ) -> Result<Self, String> {
            let rate = format!("{}/{}", fps_n, fps_d);
            let size = format!("{}x{}", width, height);

            // Auto-detect encoder if not specified
            let enc = if encoder_name.is_empty() {
                match codec_type {
                    "h265" => detect_hw_encoder(&[
                        "hevc_rkmpp",
                        "hevc_v4l2m2m",
                        "hevc_vaapi",
                        "hevc_nvenc",
                        "hevc_qsv",
                    ]),
                    "h264" => detect_hw_encoder(&[
                        "h264_rkmpp",
                        "h264_v4l2m2m",
                        "h264_vaapi",
                        "h264_nvenc",
                        "h264_qsv",
                    ]),
                    _ => return Err("Unknown codec type".to_string()),
                }
            } else {
                encoder_name.to_string()
            };

            println!("Starting HW encoder: {} ({})", enc, codec_type);

            let mut cmd = Command::new("ffmpeg");
            cmd.args([
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                input_pix_fmt,
                "-s",
                &size,
                "-r",
                &rate,
                "-i",
                "pipe:0",
                "-c:v",
                &enc,
                "-g",
                "1", // GOP=1 (all-intra)
                "-bf",
                "0", // no B-frames
                "-f",
                "avi", // AVI container for frame boundaries
                "pipe:1",
            ]);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());

            let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn: {}", e))?;
            let stdin = child.stdin.take().ok_or("no stdin")?;
            let stdout = child.stdout.take().ok_or("no stdout")?;

            let omt_codec = if codec_type == "h264" {
                OMTCodec::H264
            } else {
                OMTCodec::H265
            };

            Ok(HwEncoderContext {
                child,
                stdin,
                stdout,
                codec: omt_codec,
                read_buf: vec![0u8; 8 * 1024 * 1024],
                header_skipped: false,
            })
        }

        /// Read one compressed frame from AVI output.
        /// AVI chunks: 4-byte FourCC + 4-byte size (LE) + data.
        fn read_frame(&mut self) -> Option<Vec<u8>> {
            use std::io::Read;

            // Skip AVI header on first call by scanning for first video chunk "00dc"
            if !self.header_skipped {
                // Read in small chunks looking for "00dc" or "01dc"
                let mut hdr_buf = [0u8; 1];
                let mut window = [0u8; 4];
                loop {
                    if self.stdout.read_exact(&mut hdr_buf).is_err() {
                        return None;
                    }
                    window[0] = window[1];
                    window[1] = window[2];
                    window[2] = window[3];
                    window[3] = hdr_buf[0];
                    // "00dc" = compressed video chunk
                    if &window == b"00dc" || &window == b"01dc" {
                        self.header_skipped = true;
                        break;
                    }
                }
                // Read chunk size (4 bytes LE)
                let mut size_buf = [0u8; 4];
                if self.stdout.read_exact(&mut size_buf).is_err() {
                    return None;
                }
                let size = u32::from_le_bytes(size_buf) as usize;
                if size > self.read_buf.len() {
                    self.read_buf.resize(size, 0);
                }
                if self.stdout.read_exact(&mut self.read_buf[..size]).is_err() {
                    return None;
                }
                // Skip padding byte if odd size
                if size % 2 != 0 {
                    let _ = self.stdout.read_exact(&mut [0u8; 1]);
                }
                return Some(self.read_buf[..size].to_vec());
            }

            // Read next AVI chunk
            let mut chunk_hdr = [0u8; 8];
            if self.stdout.read_exact(&mut chunk_hdr).is_err() {
                return None;
            }
            let fourcc = &chunk_hdr[..4];
            let size = u32::from_le_bytes([chunk_hdr[4], chunk_hdr[5], chunk_hdr[6], chunk_hdr[7]])
                as usize;

            // Skip non-video chunks (e.g., "idx1" index)
            if fourcc != b"00dc" && fourcc != b"01dc" {
                // Skip this chunk's data
                if size > 0 && size < 100_000_000 {
                    let mut skip = vec![0u8; size];
                    let _ = self.stdout.read_exact(&mut skip);
                }
                // Try next chunk recursively
                return self.read_frame();
            }

            if size > self.read_buf.len() {
                self.read_buf.resize(size, 0);
            }
            if self.stdout.read_exact(&mut self.read_buf[..size]).is_err() {
                return None;
            }
            if size % 2 != 0 {
                let _ = self.stdout.read_exact(&mut [0u8; 1]);
            }
            Some(self.read_buf[..size].to_vec())
        }
    }

    impl Drop for HwEncoderContext {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn detect_hw_encoder(candidates: &[&str]) -> String {
        for enc in candidates {
            let result = Command::new("ffmpeg")
                .args(["-hide_banner", "-encoders"])
                .output();
            if let Ok(output) = result {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.contains(enc) {
                    return enc.to_string();
                }
            }
        }
        candidates.last().unwrap_or(&"libx265").to_string()
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
        active_quality_mask: Arc<AtomicU8>,
        active_codec_mask: Arc<AtomicU8>,
        shared_frame: SharedPreviewFrame,
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

        let desired_codec = parse_codec(&settings.codec).unwrap_or(OMTCodec::YUY2);
        let desired_pix_fmt = codec_to_pix_fmt(desired_codec);
        let desired_fourcc = codec_to_fourcc(desired_codec);
        let detected_source_fps = if settings.use_native_format {
            detect_v4l2_dv_fps(&settings.device_path)
        } else {
            None
        };

        // Set the desired capture format explicitly. Without this, V4L2 may default to the
        // smallest resolution the device supports (e.g. 720x576 instead of 1920x1080).
        let mut v4l_fmt = dev.format().ok();
        if let Some(mut current_fmt) = v4l_fmt {
            current_fmt.width = settings.width.max(1);
            current_fmt.height = settings.height.max(1);
            current_fmt.fourcc = desired_fourcc;
            match dev.set_format(&current_fmt) {
                Ok(actual) => {
                    println!(
                        "V4L2 format set: requested {}x{} {:?}, got {}x{} {:?}",
                        settings.width,
                        settings.height,
                        desired_fourcc,
                        actual.width,
                        actual.height,
                        actual.fourcc
                    );
                    v4l_fmt = Some(actual);
                }
                Err(e) => {
                    eprintln!("Warning: failed to set V4L2 format: {}", e);
                    v4l_fmt = dev.format().ok();
                }
            }
        }

        let configured_fps = (settings.frame_rate_n.max(1), settings.frame_rate_d.max(1));
        let source_fps = detected_source_fps.unwrap_or(configured_fps);
        let target_fps = if settings.use_native_format
            && settings.frame_rate_n > 0
            && fps_less_than(configured_fps, source_fps)
        {
            println!(
                "Capping native output FPS to configured rate: {}/{} (source {}/{})",
                configured_fps.0, configured_fps.1, source_fps.0, source_fps.1
            );
            configured_fps
        } else {
            source_fps
        };
        let (mut input_rate_n, mut input_rate_d) = target_fps;

        // Try to set capture frame interval (fps). Some devices ignore this, but when supported
        // it can reduce internal buffering and stabilize capture timing.
        if v4l_fmt.is_some() && settings.frame_rate_n > 0 {
            let interval = Fraction::new(input_rate_d.max(1), input_rate_n.max(1));
            let params = CaptureParameters::new(interval);
            if let Err(e) = dev.set_params(&params) {
                eprintln!("Warning: failed to set V4L2 capture params (fps): {}", e);
            }
        }
        if v4l_fmt.is_some() {
            if let Ok(params) = dev.params() {
                if params.interval.numerator > 0 && params.interval.denominator > 0 {
                    // v4l interval is time-per-frame (num/den sec), fps = den/num
                    input_rate_n = params.interval.denominator;
                    input_rate_d = params.interval.numerator;
                }
            }
        }

        let mut source: CaptureSource<'_>;
        let mut suppress_auto_hdmi_preview = false;
        let (input_width, input_height, input_fourcc, input_codec, input_stride) = if let Some(
            fmt,
        ) = v4l_fmt
        {
            let input_codec = fourcc_to_codec(fmt.fourcc);
            let stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 2) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to create video stream: {}", e);
                    return;
                }
            };
            source = CaptureSource::V4l { stream };
            (fmt.width, fmt.height, fmt.fourcc, input_codec, fmt.stride)
        } else {
            eprintln!(
                "V4L2 single-plane format query failed; trying multiplanar native capture for {}.",
                settings.device_path
            );
            let width = settings.width.max(1);
            let height = settings.height.max(1);
            let enable_drm_mirror = preview.blocking_read().auto_hdmi_monitor;
            suppress_auto_hdmi_preview = enable_drm_mirror;
            match MplaneCapture::start(&settings, width, height, enable_drm_mirror) {
                Ok((capture, actual_width, actual_height, pitch)) => {
                    if capture.has_drm_mirror() {
                        println!("Using direct DRM HDMI monitor path.");
                    }
                    source = CaptureSource::Mplane(capture);
                    (
                        actual_width,
                        actual_height,
                        FourCC::new(b"BGR3"),
                        OMTCodec::BGRA,
                        pitch,
                    )
                }
                Err(native_err) => {
                    eprintln!(
                            "Multiplanar native capture unavailable: {}; using ffmpeg capture fallback.",
                            native_err
                        );
                    let capture_format = select_ffmpeg_capture_format(&settings, desired_pix_fmt);
                    let capture = match start_ffmpeg_capture(
                        &settings,
                        &capture_format.pix_fmt,
                        capture_format.force_input_format,
                        width,
                        height,
                        input_rate_n,
                        input_rate_d,
                    ) {
                        Ok(capture) => capture,
                        Err(e) => {
                            eprintln!("Failed to start ffmpeg capture fallback: {}", e);
                            return;
                        }
                    };
                    source = CaptureSource::Ffmpeg(capture);
                    (
                        width,
                        height,
                        capture_format.fourcc,
                        capture_format.codec,
                        codec_stride(capture_format.codec, width),
                    )
                }
            }
        };

        let input_pix_fmt = codec_to_pix_fmt(input_codec);

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
            input_stride
        };

        let mut preview_sinks = {
            let mut current_preview = preview.blocking_read().clone();
            if suppress_auto_hdmi_preview {
                current_preview.auto_hdmi_monitor = false;
            }
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
        let mut throttle_fps = if use_native {
            settings.frame_rate_n > 0 && effective_output_rate_n > 0
        } else {
            input_rate_n as u64 * effective_output_rate_d as u64
                > effective_output_rate_n as u64 * input_rate_d as u64
        };
        if needs_transform && transform.is_none() {
            // Match C# behavior: when transform failed and we fall back to native format,
            // disable software FPS throttling.
            throttle_fps = false;
        }
        let output_frame_interval = Duration::from_secs_f64(
            effective_output_rate_d as f64 / effective_output_rate_n as f64,
        );
        let mut last_output_frame_at = Instant::now() - output_frame_interval;
        let mut last_snapshot = Instant::now() - Duration::from_secs(2);
        let mut last_preview_hotplug_check = Instant::now();
        let mut last_detected_hdmi_outputs = connected_hdmi_framebuffers();

        let mut current_quality_level = suggested_quality_hint.load(Ordering::Relaxed);
        // Multi-quality VMX instances: encode at LQ, SQ, HQ simultaneously.
        struct QualityInstance {
            inst: *mut root::VMX_INSTANCE,
            level: u8,
            buffer: Vec<u8>,
        }
        let mut quality_instances: Vec<QualityInstance> = Vec::new();

        if codec_to_vmx_image_format(encode_codec).is_some() {
            let size = root::VMX_SIZE {
                width: encode_width as i32,
                height: encode_height as i32,
            };
            let vmx_threads = std::thread::available_parallelism()
                .map(|n| n.get().clamp(2, 4))
                .unwrap_or(2);
            let buf_size = (frame_size_bytes(encode_codec, encode_width, encode_height) * 2)
                .max(8 * 1024 * 1024);
            for &(level, profile) in &[
                (1u8, root::VMX_PROFILE_VMX_PROFILE_OMT_LQ),
                (2u8, root::VMX_PROFILE_VMX_PROFILE_OMT_SQ),
                (3u8, root::VMX_PROFILE_VMX_PROFILE_OMT_HQ),
            ] {
                unsafe {
                    let inst =
                        root::VMX_Create(size, profile, root::VMX_COLORSPACE_VMX_COLORSPACE_BT709);
                    if !inst.is_null() {
                        let _ = root::VMX_SetThreads(inst, vmx_threads as i32);
                        quality_instances.push(QualityInstance {
                            inst,
                            level,
                            buffer: vec![0u8; buf_size],
                        });
                    }
                }
            }
            if !quality_instances.is_empty() {
                println!(
                    "Multi-quality encoding enabled: {} quality levels, {} VMX threads each",
                    quality_instances.len(),
                    vmx_threads
                );
            }
        } else {
            eprintln!(
                "Codec {} is not VMX encodable, sending raw video.",
                codec_to_name(encode_codec)
            );
        }

        // HW encoders (H.264/H.265 all-intra) — created on demand when clients request them.
        let mut hw_h264: Option<HwEncoderContext> = None;
        let mut hw_h265: Option<HwEncoderContext> = None;
        let input_pix_fmt_str = codec_to_pix_fmt(encode_codec).to_string();

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            if restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                // Caller requested a config reload.
                break;
            }
            if preview_restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                preview_restart_requested.store(false, std::sync::atomic::Ordering::SeqCst);
                let mut current_preview = preview.blocking_read().clone();
                if suppress_auto_hdmi_preview {
                    current_preview.auto_hdmi_monitor = false;
                }
                stop_preview_sinks(&mut preview_sinks);
                preview_sinks = build_preview_sinks(
                    &settings,
                    &current_preview,
                    input_width,
                    input_height,
                    input_fourcc,
                );
                preview_enabled = !preview_sinks.is_empty();
                last_detected_hdmi_outputs = connected_hdmi_framebuffers();
            } else if last_preview_hotplug_check.elapsed() >= Duration::from_secs(2) {
                last_preview_hotplug_check = Instant::now();
                let mut current_preview = preview.blocking_read().clone();
                if suppress_auto_hdmi_preview {
                    current_preview.auto_hdmi_monitor = false;
                }
                if current_preview.auto_hdmi_monitor
                    || preview_has_framebuffer_output(&current_preview)
                    || !last_detected_hdmi_outputs.is_empty()
                {
                    let connected_hdmi_outputs = connected_hdmi_framebuffers();
                    if connected_hdmi_outputs != last_detected_hdmi_outputs {
                        println!(
                            "HDMI preview outputs changed: {:?} -> {:?}",
                            last_detected_hdmi_outputs, connected_hdmi_outputs
                        );
                        stop_preview_sinks(&mut preview_sinks);
                        preview_sinks = build_preview_sinks(
                            &settings,
                            &current_preview,
                            input_width,
                            input_height,
                            input_fourcc,
                        );
                        preview_enabled = !preview_sinks.is_empty();
                        last_detected_hdmi_outputs = connected_hdmi_outputs;
                    }
                }
            }
            // Quality-level switching is handled by per-quality VMX instances.
            // No need to recreate the default instance.

            let copy_frame = if throttle_fps {
                let now = Instant::now();
                if now.duration_since(last_output_frame_at) < output_frame_interval {
                    false
                } else {
                    last_output_frame_at = now;
                    true
                }
            } else {
                true
            };

            let raw_data = match source.next_frame(copy_frame) {
                Ok(Some(frame)) => {
                    consecutive_capture_errors = 0;
                    frame
                }
                Ok(None) => {
                    consecutive_capture_errors = 0;
                    continue;
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
            let capture_timestamp = crate::timebase::presentation_100ns();
            if restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            if preview_restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
                // Apply preview changes as soon as possible, but after we finish a V4L2 read to
                // avoid leaving the stream in a weird state.
                continue;
            }
            let raw_data = if input_codec == OMTCodec::BGRA
                && raw_data.len() == input_width as usize * input_height as usize * 3
            {
                Bytes::from(bgr24_to_bgra(&raw_data))
            } else {
                raw_data
            };

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
                        raw_data.clone(),
                        input_codec,
                        input_width,
                        input_height,
                        input_stride,
                    )
                };

            let raw_payload = payload.clone();

            // Share raw frame for web preview (~1 per second)
            if last_snapshot.elapsed() >= Duration::from_millis(1000) {
                last_snapshot = Instant::now();
                if let Ok(mut lock) = shared_frame.data.try_lock() {
                    *lock = Some(PreviewFrameData {
                        raw: payload.to_vec(),
                        width: frame_width,
                        height: frame_height,
                        pix_fmt: codec_to_pix_fmt(frame_codec).to_string(),
                    });
                }
            }

            let codec_mask = active_codec_mask.load(Ordering::Relaxed);
            let network_flags = video_flags_from_source_codec(frame_codec);
            let payload_len = raw_payload.len();
            if startup_debug_frames < 10 {
                println!(
                    "Video raw frame[{}]: codec={}, {}x{}, payload={}, fps={}/{}",
                    startup_debug_frames + 1,
                    codec_to_name(frame_codec),
                    frame_width,
                    frame_height,
                    payload_len,
                    effective_output_rate_n,
                    effective_output_rate_d
                );
                startup_debug_frames += 1;
            }
            // Encode and send at each quality level for quality-specific channels.
            // Must happen BEFORE frame.data consumes payload via Bytes::copy_from_slice.
            if !quality_instances.is_empty() {
                let mask = active_quality_mask.load(Ordering::Relaxed);
                for qi in quality_instances.iter_mut() {
                    // Skip quality levels with no active receivers
                    let bit = match qi.level {
                        1 => 1u8, // LQ
                        2 => 2u8, // SQ
                        _ => 4u8, // HQ
                    };
                    if mask & bit == 0 {
                        continue;
                    }
                    let err = unsafe {
                        vmx_encode_frame(
                            qi.inst,
                            frame_codec,
                            raw_payload.as_ptr(),
                            frame_height,
                            frame_stride as i32,
                        )
                    };
                    if err == root::VMX_ERR_VMX_ERR_OK {
                        let compressed_len = unsafe {
                            root::VMX_SaveTo(
                                qi.inst,
                                qi.buffer.as_mut_ptr(),
                                qi.buffer.len() as i32,
                            )
                        };
                        if compressed_len > 0 {
                            let preview_payload_len =
                                unsafe { root::VMX_GetEncodedPreviewLength(qi.inst) };
                            let preview_total_len = if preview_payload_len > 0 {
                                Some(OMTVideoHeader::SIZE as i32 + preview_payload_len)
                            } else {
                                None
                            };
                            let mut qframe = OMTFrame::new(OMTFrameType::Video);
                            qframe.header.timestamp = capture_timestamp;
                            qframe.video_header = Some(OMTVideoHeader {
                                codec: OMTCodec::VMX1 as i32,
                                width: frame_width as i32,
                                height: frame_height as i32,
                                frame_rate_n: effective_output_rate_n as i32,
                                frame_rate_d: effective_output_rate_d as i32,
                                aspect_ratio: frame_width as f32 / frame_height as f32,
                                flags: network_flags,
                                color_space: 709,
                            });
                            qframe.data =
                                Bytes::copy_from_slice(&qi.buffer[..compressed_len as usize]);
                            qframe.update_data_length();
                            qframe.preview_data_length = preview_total_len;
                            send.send_video_quality(qframe, qi.level);
                        }
                    }
                }
            }

            // H.264 encoding (demand-driven, gated on codec_mask bit 1)
            if codec_mask & 2 != 0 {
                use std::io::Write;
                if hw_h264.is_none() {
                    match HwEncoderContext::start(
                        &input_pix_fmt_str,
                        encode_width,
                        encode_height,
                        effective_output_rate_n,
                        effective_output_rate_d,
                        &settings.hw_encoder,
                        "h264",
                    ) {
                        Ok(ctx) => {
                            println!("H.264 HW encoder started");
                            hw_h264 = Some(ctx);
                        }
                        Err(e) => eprintln!("H.264 encoder failed: {}", e),
                    }
                }
                if let Some(ref mut enc) = hw_h264 {
                    if enc.stdin.write_all(&raw_payload).is_ok() {
                        if let Some(compressed) = enc.read_frame() {
                            let mut f = OMTFrame::new(OMTFrameType::Video);
                            f.header.timestamp = capture_timestamp;
                            f.video_header = Some(OMTVideoHeader {
                                codec: OMTCodec::H264 as i32,
                                width: frame_width as i32,
                                height: frame_height as i32,
                                frame_rate_n: effective_output_rate_n as i32,
                                frame_rate_d: effective_output_rate_d as i32,
                                aspect_ratio: frame_width as f32 / frame_height as f32,
                                flags: 0,
                                color_space: 709,
                            });
                            f.data = Bytes::from(compressed);
                            f.update_data_length();
                            send.send_video_codec(f, OMTCodec::H264);
                        }
                    }
                }
            } else {
                hw_h264 = None;
            }

            // H.265 encoding (demand-driven, gated on codec_mask bit 2)
            if codec_mask & 4 != 0 {
                use std::io::Write;
                if hw_h265.is_none() {
                    match HwEncoderContext::start(
                        &input_pix_fmt_str,
                        encode_width,
                        encode_height,
                        effective_output_rate_n,
                        effective_output_rate_d,
                        &settings.hw_encoder,
                        "h265",
                    ) {
                        Ok(ctx) => {
                            println!("H.265 HW encoder started");
                            hw_h265 = Some(ctx);
                        }
                        Err(e) => eprintln!("H.265 encoder failed: {}", e),
                    }
                }
                if let Some(ref mut enc) = hw_h265 {
                    if enc.stdin.write_all(&raw_payload).is_ok() {
                        if let Some(compressed) = enc.read_frame() {
                            let mut f = OMTFrame::new(OMTFrameType::Video);
                            f.header.timestamp = capture_timestamp;
                            f.video_header = Some(OMTVideoHeader {
                                codec: OMTCodec::H265 as i32,
                                width: frame_width as i32,
                                height: frame_height as i32,
                                frame_rate_n: effective_output_rate_n as i32,
                                frame_rate_d: effective_output_rate_d as i32,
                                aspect_ratio: frame_width as f32 / frame_height as f32,
                                flags: 0,
                                color_space: 709,
                            });
                            f.data = Bytes::from(compressed);
                            f.update_data_length();
                            send.send_video_codec(f, OMTCodec::H265);
                        }
                    }
                }
            } else {
                hw_h265 = None;
            }

            frame_count += 1;
            sent_bytes += payload_len;
            fps_window_frames += 1;

            if frame_count >= 900 {
                println!("Sent {} frames, {} bytes.", frame_count, sent_bytes);
                frame_count = 0;
                sent_bytes = 0;
            }

            let elapsed = fps_window_start.elapsed().as_secs_f64();
            if elapsed >= 30.0 {
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
                    let preview_bytes = raw_data.clone();
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
        for qi in &quality_instances {
            unsafe {
                root::VMX_Destroy(qi.inst);
            }
        }

        // Stop preview workers.
        stop_preview_sinks(&mut preview_sinks);
    }

    fn stop_preview_sinks(preview_sinks: &mut [PreviewSink]) {
        for sink in preview_sinks.iter_mut() {
            sink.tx.take();
            if let Some(handle) = sink.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn preview_has_framebuffer_output(preview: &PreviewSettings) -> bool {
        preview.output_device.starts_with("/dev/fb")
            || preview
                .output_devices
                .iter()
                .any(|d| d.starts_with("/dev/fb"))
            || preview
                .outputs
                .iter()
                .any(|o| o.device.starts_with("/dev/fb"))
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

    struct FfmpegCaptureFormat {
        pix_fmt: String,
        codec: OMTCodec,
        fourcc: FourCC,
        force_input_format: bool,
    }

    fn select_ffmpeg_capture_format(
        settings: &VideoSettings,
        preferred_pix_fmt: &str,
    ) -> FfmpegCaptureFormat {
        let preferred_codec = pix_fmt_to_codec(preferred_pix_fmt).unwrap_or(OMTCodec::YUY2);
        if !settings.use_native_format {
            return FfmpegCaptureFormat {
                pix_fmt: preferred_pix_fmt.to_string(),
                codec: preferred_codec,
                fourcc: codec_to_fourcc(preferred_codec),
                force_input_format: false,
            };
        }

        let supported = ffmpeg_v4l2_capture_pix_fmts(&settings.device_path);
        if let Some(active) = v4l2_active_capture_format(&settings.device_path) {
            if supported.contains(&active.pix_fmt) {
                println!(
                    "FFmpeg native capture format: using active V4L2 format {} ({}) on {}",
                    active.pix_fmt, active.fourcc_name, settings.device_path
                );
                return FfmpegCaptureFormat {
                    pix_fmt: active.pix_fmt,
                    codec: active.codec,
                    fourcc: active.fourcc,
                    force_input_format: true,
                };
            }
        }
        if supported.contains(preferred_pix_fmt) {
            return FfmpegCaptureFormat {
                pix_fmt: preferred_pix_fmt.to_string(),
                codec: preferred_codec,
                fourcc: codec_to_fourcc(preferred_codec),
                force_input_format: true,
            };
        }

        for pix_fmt in ["bgr24", "nv12", "uyvy422", "yuyv422", "bgra", "yuv420p"] {
            if supported.contains(pix_fmt) {
                let codec = if pix_fmt == "bgr24" {
                    OMTCodec::BGRA
                } else {
                    pix_fmt_to_codec(pix_fmt).unwrap_or(preferred_codec)
                };
                println!(
                    "FFmpeg native capture format: using {} because {} is not exposed by {}",
                    pix_fmt, preferred_pix_fmt, settings.device_path
                );
                return FfmpegCaptureFormat {
                    pix_fmt: pix_fmt.to_string(),
                    codec,
                    fourcc: ffmpeg_pix_fmt_to_fourcc(pix_fmt)
                        .unwrap_or_else(|| codec_to_fourcc(codec)),
                    force_input_format: true,
                };
            }
        }

        FfmpegCaptureFormat {
            pix_fmt: preferred_pix_fmt.to_string(),
            codec: preferred_codec,
            fourcc: codec_to_fourcc(preferred_codec),
            force_input_format: false,
        }
    }

    fn detect_v4l2_dv_fps(device_path: &str) -> Option<(u32, u32)> {
        let output = Command::new("v4l2-ctl")
            .args(["-d", device_path, "--all"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let Some(idx) = line.find("frames per second") else {
                continue;
            };
            let before = &line[..idx];
            let token = before
                .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .filter(|part| !part.is_empty())
                .last()?;
            let fps = token.parse::<f64>().ok()?;
            if !(1.0..=240.0).contains(&fps) {
                return None;
            }
            let mut numerator = (fps * 1000.0).round() as u32;
            let mut denominator = 1000u32;
            let divisor = gcd_u32(numerator, denominator).max(1);
            numerator /= divisor;
            denominator /= divisor;
            println!(
                "Detected V4L2 DV timing FPS on {}: {}/{}",
                device_path, numerator, denominator
            );
            return Some((numerator.max(1), denominator.max(1)));
        }
        None
    }

    fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
        while b != 0 {
            let r = a % b;
            a = b;
            b = r;
        }
        a
    }

    fn fps_less_than(left: (u32, u32), right: (u32, u32)) -> bool {
        (left.0 as u64) * (right.1.max(1) as u64) < (right.0 as u64) * (left.1.max(1) as u64)
    }

    struct ActiveCaptureFormat {
        fourcc_name: String,
        fourcc: FourCC,
        pix_fmt: String,
        codec: OMTCodec,
    }

    fn v4l2_active_capture_format(device_path: &str) -> Option<ActiveCaptureFormat> {
        let output = Command::new("v4l2-ctl")
            .args(["-d", device_path, "--all"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let Some(idx) = line.find("Pixel Format") else {
                continue;
            };
            let rest = &line[idx..];
            let Some(first) = rest.find('\'') else {
                continue;
            };
            let after_first = &rest[first + 1..];
            let Some(second) = after_first.find('\'') else {
                continue;
            };
            let fourcc = &after_first[..second];
            if let Some((pix_fmt, codec)) = fourcc_to_ffmpeg_capture(fourcc) {
                return Some(ActiveCaptureFormat {
                    fourcc_name: fourcc.to_string(),
                    fourcc: fourcc_from_str(fourcc)?,
                    pix_fmt: pix_fmt.to_string(),
                    codec,
                });
            }
        }
        None
    }

    fn fourcc_to_ffmpeg_capture(fourcc: &str) -> Option<(&'static str, OMTCodec)> {
        match fourcc {
            "BGR3" => Some(("bgr24", OMTCodec::BGRA)),
            "NV12" => Some(("nv12", OMTCodec::NV12)),
            "YUYV" | "YUY2" => Some(("yuyv422", OMTCodec::YUY2)),
            "UYVY" => Some(("uyvy422", OMTCodec::UYVY)),
            "YU12" | "YV12" => Some(("yuv420p", OMTCodec::YV12)),
            "BGRA" => Some(("bgra", OMTCodec::BGRA)),
            _ => None,
        }
    }

    fn ffmpeg_pix_fmt_to_fourcc(pix_fmt: &str) -> Option<FourCC> {
        match pix_fmt {
            "bgr24" => Some(FourCC::new(b"BGR3")),
            "nv12" => Some(FourCC::new(b"NV12")),
            "yuyv422" => Some(FourCC::new(b"YUYV")),
            "uyvy422" => Some(FourCC::new(b"UYVY")),
            "yuv420p" => Some(FourCC::new(b"YU12")),
            "bgra" => Some(FourCC::new(b"BGRA")),
            _ => None,
        }
    }

    fn fourcc_from_str(value: &str) -> Option<FourCC> {
        let bytes = value.as_bytes();
        if bytes.len() != 4 {
            return None;
        }
        Some(FourCC::new(&[bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn fourcc_u32_to_string(value: u32) -> String {
        String::from_utf8_lossy(&value.to_le_bytes()).to_string()
    }

    fn ffmpeg_v4l2_capture_pix_fmts(device_path: &str) -> HashSet<String> {
        let mut supported = HashSet::new();
        let output = match Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-f",
                "v4l2",
                "-list_formats",
                "all",
                "-i",
                device_path,
            ])
            .output()
        {
            Ok(output) => output,
            Err(_) => return supported,
        };
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in text.lines() {
            if !line.contains("Raw") {
                continue;
            }
            let Some(raw_fmt) = line.split(':').nth(1) else {
                continue;
            };
            let Some(pix_fmt) = raw_fmt.split_whitespace().next() else {
                continue;
            };
            if [
                "bgr24", "rgb24", "nv12", "uyvy422", "yuyv422", "bgra", "yuv420p",
            ]
            .contains(&pix_fmt)
            {
                supported.insert(pix_fmt.to_string());
            }
        }
        supported
    }

    fn start_ffmpeg_capture(
        settings: &VideoSettings,
        output_pix_fmt: &str,
        force_input_format: bool,
        output_width: u32,
        output_height: u32,
        capture_rate_n: u32,
        capture_rate_d: u32,
    ) -> Result<FfmpegCapture, String> {
        let rate = if capture_rate_n == 0 {
            "30".to_string()
        } else {
            format!("{}/{}", capture_rate_n, capture_rate_d.max(1))
        };
        let frame_size = raw_frame_size(output_width, output_height, output_pix_fmt)
            .unwrap_or_else(|| {
                frame_size_bytes(
                    pix_fmt_to_codec(output_pix_fmt).unwrap_or(OMTCodec::YUY2),
                    output_width,
                    output_height,
                )
            });
        println!(
            "Starting ffmpeg V4L2 capture fallback: {} -> {}x{} {} @ {}",
            settings.device_path, output_width, output_height, output_pix_fmt, rate
        );

        let video_size = format!("{}x{}", output_width, output_height);
        let filter = format!(
            "fps={},scale={}:{}:flags=fast_bilinear",
            rate, output_width, output_height
        );
        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "warning".to_string(),
            "-fflags".to_string(),
            "nobuffer".to_string(),
            "-f".to_string(),
            "v4l2".to_string(),
        ];
        if force_input_format {
            args.push("-input_format".to_string());
            args.push(output_pix_fmt.to_string());
        }
        args.extend([
            "-framerate".to_string(),
            rate.clone(),
            "-video_size".to_string(),
            video_size,
            "-i".to_string(),
            settings.device_path.clone(),
            "-an".to_string(),
        ]);
        if !settings.use_native_format {
            args.push("-vf".to_string());
            args.push(filter);
        }
        args.extend([
            "-pix_fmt".to_string(),
            output_pix_fmt.to_string(),
            "-f".to_string(),
            "rawvideo".to_string(),
            "pipe:1".to_string(),
        ]);

        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("ffmpeg spawn: {}", e))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "ffmpeg stdout unavailable".to_string())?;

        Ok(FfmpegCapture {
            child,
            stdout,
            frame_buf: vec![0u8; frame_size],
        })
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

    fn bgr24_to_bgra(input: &[u8]) -> Vec<u8> {
        let pixels = input.len() / 3;
        let mut out = vec![255u8; pixels * 4];
        for (src, dst) in input.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
            dst[0] = src[0];
            dst[1] = src[1];
            dst[2] = src[2];
        }
        out
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
        let connected_hdmi_outputs = connected_hdmi_framebuffers();
        let auto_hdmi_outputs = if preview.auto_hdmi_monitor {
            connected_hdmi_outputs.clone()
        } else {
            Vec::new()
        };
        if !preview.enabled && auto_hdmi_outputs.is_empty() {
            return Vec::new();
        }

        // Resolve output list: prefer per-output `outputs` array, fall back to legacy fields.
        let mut resolved: Vec<ResolvedOutput> = if preview.enabled && !preview.outputs.is_empty() {
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
            let mut devs = if preview.enabled {
                preview.output_devices.clone()
            } else {
                Vec::new()
            };
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
        for device in auto_hdmi_outputs {
            resolved.push(ResolvedOutput {
                device,
                fps: 0,
                pixel_format: String::new(),
                rotate: 0,
            });
        }

        let fourcc_str = std::str::from_utf8(&input_fourcc.repr).unwrap_or("YUYV");
        let pix_fmt = match fourcc_str {
            "BGR3" | "BGRA" => "bgra",
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

            let mut interval_ms = if out.fps == 0 {
                0
            } else {
                1000 / out.fps.max(1) as u64
            };
            let mut input_rate = if out.fps > 0 {
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

            let hdmi_framebuffer = is_hdmi_framebuffer(&out.device);
            if hdmi_framebuffer && !connected_hdmi_outputs.contains(&out.device) {
                println!("Skipping disconnected HDMI monitor output {}", out.device);
                continue;
            }
            let fmt = if hdmi_framebuffer {
                framebuffer_pixel_format(&out.device).unwrap_or_else(|| {
                    if out.pixel_format.trim().is_empty() {
                        "rgb565le".to_string()
                    } else {
                        out.pixel_format.clone()
                    }
                })
            } else if out.pixel_format.trim().is_empty() {
                framebuffer_pixel_format(&out.device).unwrap_or_else(|| "rgb565le".to_string())
            } else {
                out.pixel_format.clone()
            };

            if hdmi_framebuffer {
                interval_ms = 0;
                input_rate = if settings.frame_rate_d == 0 {
                    "30".to_string()
                } else {
                    format!(
                        "{}/{}",
                        settings.frame_rate_n.max(1),
                        settings.frame_rate_d.max(1)
                    )
                };
            }

            if hdmi_framebuffer
                && !(out.rotate == 0
                    && input_width == preview_width
                    && input_height == preview_height
                    && pix_fmt == fmt)
            {
                println!(
                    "Skipping HDMI monitor output {}: direct path requires {}x{} {}, got {}x{} {} rotate={}",
                    out.device,
                    input_width,
                    input_height,
                    pix_fmt,
                    preview_width,
                    preview_height,
                    fmt,
                    out.rotate
                );
                continue;
            }

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
            let direct_fbdev = is_fbdev
                && rotate == 0
                && input_width == preview_width
                && input_height == preview_height
                && pix_fmt == preview_format;
            if direct_fbdev {
                let frame_bytes =
                    match raw_frame_size(preview_width, preview_height, &preview_format) {
                        Some(n) => n,
                        None => return,
                    };
                if is_hdmi_framebuffer(&output) {
                    if let Some(gst_format) = gst_raw_video_format(&preview_format) {
                        if let Ok(mut child) = start_kms_preview(
                            &gst_format,
                            preview_width,
                            preview_height,
                            &input_rate,
                            frame_bytes,
                        ) {
                            println!(
                                "Preview output KMS path: {} ({}x{}, {}, {}fps)",
                                output, preview_width, preview_height, preview_format, input_rate
                            );
                            let mut preview_frames = 0usize;
                            let mut preview_window = Instant::now();
                            let mut stdin = match child.stdin.take() {
                                Some(stdin) => stdin,
                                None => return,
                            };
                            while let Ok(frame) = rx.recv() {
                                if frame.len() < frame_bytes {
                                    continue;
                                }
                                if stdin.write_all(&frame[..frame_bytes]).is_err() {
                                    break;
                                }
                                preview_frames += 1;
                                if preview_window.elapsed() >= Duration::from_secs(10) {
                                    let fps = preview_frames as f64
                                        / preview_window.elapsed().as_secs_f64();
                                    println!("Preview output FPS: {:.1} ({})", fps, output);
                                    preview_frames = 0;
                                    preview_window = Instant::now();
                                }
                            }
                            drop(stdin);
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                    }
                    eprintln!(
                        "Preview output KMS path unavailable for {}. Falling back to fbdev.",
                        output
                    );
                }
                let mut fd = match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&output)
                {
                    Ok(f) => f,
                    Err(_) => return,
                };
                let mut fb_map = unsafe {
                    MmapOptions::new()
                        .len(frame_bytes)
                        .map_mut(&fd)
                        .map_err(|e| {
                            eprintln!(
                                "Preview output mmap failed for {}: {}. Falling back to write().",
                                output, e
                            );
                            e
                        })
                        .ok()
                };
                println!(
                    "Preview output direct framebuffer path: {} ({}x{}, {}, {})",
                    output,
                    preview_width,
                    preview_height,
                    preview_format,
                    if fb_map.is_some() { "mmap" } else { "write" }
                );
                let mut preview_frames = 0usize;
                let mut preview_window = Instant::now();
                while let Ok(frame) = rx.recv() {
                    if frame.len() < frame_bytes {
                        continue;
                    }
                    if let Some(map) = fb_map.as_mut() {
                        map[..frame_bytes].copy_from_slice(&frame[..frame_bytes]);
                    } else {
                        use std::io::Seek;
                        let _ = fd.seek(std::io::SeekFrom::Start(0));
                        if fd.write_all(&frame[..frame_bytes]).is_err() {
                            break;
                        }
                    }
                    preview_frames += 1;
                    if preview_window.elapsed() >= Duration::from_secs(10) {
                        let fps = preview_frames as f64 / preview_window.elapsed().as_secs_f64();
                        println!("Preview output FPS: {:.1} ({})", fps, output);
                        preview_frames = 0;
                        preview_window = Instant::now();
                    }
                }
                return;
            }

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
            let frame_bytes = raw_frame_size(preview_width, preview_height, &preview_format)
                .unwrap_or_else(|| preview_width as usize * preview_height as usize * 2);

            // Outer loop: restart ffmpeg on failure.
            loop {
                let mut cmd = Command::new("ffmpeg");
                cmd.args([
                    "-loglevel",
                    "error",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    &pix_fmt,
                    "-s",
                    &format!("{}x{}", input_width, input_height),
                    "-r",
                    &input_rate,
                    "-i",
                    "pipe:0",
                    "-vf",
                    &vf,
                ]);
                if is_fbdev {
                    cmd.args(["-f", "rawvideo", "pipe:1"]);
                    cmd.stdout(Stdio::piped());
                } else {
                    cmd.args(["-f", "fbdev", &output]);
                    cmd.stdout(Stdio::null());
                }
                cmd.stdin(Stdio::piped()).stderr(Stdio::null());

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let mut stdin = match child.stdin.take() {
                    Some(s) => s,
                    None => break,
                };

                if is_fbdev {
                    let stdout = match child.stdout.take() {
                        Some(s) => s,
                        None => break,
                    };
                    let fb_path = output.clone();

                    // Separate thread reads ffmpeg stdout → writes to fb.
                    // Prevents deadlock: stdin write blocks if ffmpeg can't flush stdout.
                    let reader = std::thread::spawn(move || {
                        let mut stdout = stdout;
                        let mut buf = vec![0u8; frame_bytes];
                        let mut fd = match std::fs::OpenOptions::new().write(true).open(&fb_path) {
                            Ok(f) => f,
                            Err(_) => return,
                        };
                        loop {
                            if !read_exact(&mut stdout, &mut buf) {
                                break;
                            }
                            use std::io::Seek;
                            let _ = fd.seek(std::io::SeekFrom::Start(0));
                            let _ = fd.write_all(&buf);
                        }
                    });

                    let mut alive = true;
                    while alive {
                        match rx.recv() {
                            Ok(frame) => {
                                if stdin.write_all(&frame).is_err() {
                                    alive = false;
                                }
                            }
                            Err(_) => {
                                drop(stdin);
                                let _ = child.kill();
                                let _ = child.wait();
                                let _ = reader.join();
                                return;
                            }
                        }
                    }
                    drop(stdin);
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                } else {
                    loop {
                        match rx.recv() {
                            Ok(frame) => {
                                if stdin.write_all(&frame).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                drop(stdin);
                                let _ = child.kill();
                                let _ = child.wait();
                                return;
                            }
                        }
                    }
                    drop(stdin);
                    let _ = child.kill();
                    let _ = child.wait();
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });

        PreviewSink {
            handle: Some(handle),
            ..sink
        }
    }

    fn start_kms_preview(
        format: &str,
        width: u32,
        height: u32,
        rate: &str,
        frame_bytes: usize,
    ) -> Result<Child, String> {
        Command::new("gst-launch-1.0")
            .args([
                "-q",
                "fdsrc",
                "fd=0",
                &format!("blocksize={}", frame_bytes),
                "do-timestamp=true",
                "!",
                "rawvideoparse",
                &format!("format={}", format),
                &format!("width={}", width),
                &format!("height={}", height),
                &format!("framerate={}", rate),
                "!",
                "queue",
                "max-size-buffers=1",
                "max-size-bytes=0",
                "max-size-time=0",
                "leaky=downstream",
                "!",
                "kmssink",
                "sync=false",
                "async=false",
                "qos=false",
                "force-modesetting=true",
                "restore-crtc=false",
                "skip-vsync=true",
                "enable-last-sample=false",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("gst-launch-1.0 spawn: {}", e))
    }

    fn gst_raw_video_format(pixel_format: &str) -> Option<String> {
        match pixel_format {
            "bgra" => Some("bgra".to_string()),
            "rgba" => Some("rgba".to_string()),
            "rgb24" => Some("rgb".to_string()),
            "bgr24" => Some("bgr".to_string()),
            "rgb565le" => Some("rgb16".to_string()),
            "yuyv422" => Some("yuy2".to_string()),
            "uyvy422" => Some("uyvy".to_string()),
            "nv12" => Some("nv12".to_string()),
            _ => None,
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

    fn codec_to_fourcc(codec: OMTCodec) -> FourCC {
        match codec {
            OMTCodec::UYVY => FourCC::new(b"UYVY"),
            OMTCodec::NV12 => FourCC::new(b"NV12"),
            OMTCodec::YV12 => FourCC::new(b"YU12"),
            OMTCodec::BGRA => FourCC::new(b"BGRA"),
            _ => FourCC::new(b"YUYV"),
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

    fn connected_hdmi_framebuffers() -> Vec<String> {
        if !has_connected_hdmi_connector() {
            return Vec::new();
        }
        let mut outputs = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/class/graphics") else {
            return outputs;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(fb) = name.to_str() else {
                continue;
            };
            if !fb.starts_with("fb") {
                continue;
            }
            let dev = format!("/dev/{fb}");
            if is_hdmi_framebuffer(&dev) {
                outputs.push(dev);
            }
        }
        outputs.sort();
        outputs.dedup();
        outputs
    }

    fn has_connected_hdmi_connector() -> bool {
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return false;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if !(lower.contains("hdmi") || lower.contains("displayport") || lower.contains("dp-")) {
                continue;
            }
            let status_path = entry.path().join("status");
            if std::fs::read_to_string(status_path)
                .map(|v| v.trim() == "connected")
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }

    fn is_hdmi_framebuffer(path: &str) -> bool {
        let Some(fb) = std::path::Path::new(path)
            .file_name()
            .and_then(|v| v.to_str())
        else {
            return false;
        };
        if !fb.starts_with("fb") {
            return false;
        }
        let name = std::fs::read_to_string(format!("/sys/class/graphics/{fb}/name"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        name.contains("drm") || name.contains("hdmi") || name.contains("rockchip")
    }

    fn raw_frame_size(width: u32, height: u32, pixel_format: &str) -> Option<usize> {
        let pixels = width as usize * height as usize;
        match pixel_format {
            "rgb565le" | "yuyv422" | "uyvy422" => Some(pixels * 2),
            "bgra" | "rgba" | "argb" | "abgr" => Some(pixels * 4),
            "rgb24" | "bgr24" => Some(pixels * 3),
            "nv12" | "yuv420p" => Some(pixels * 3 / 2),
            _ => None,
        }
    }

    fn framebuffer_pixel_format(path: &str) -> Option<String> {
        let fb = std::path::Path::new(path).file_name()?.to_str()?;
        if !fb.starts_with("fb") {
            return None;
        }
        let bpp_path = format!("/sys/class/graphics/{fb}/bits_per_pixel");
        let bpp = std::fs::read_to_string(bpp_path)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        match bpp {
            32 => Some("bgra".to_string()),
            16 => Some("rgb565le".to_string()),
            _ => None,
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
