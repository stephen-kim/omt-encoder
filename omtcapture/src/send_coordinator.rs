use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use libomtnet::ServerSenders;

use crate::settings::SendSettings;
use libomtnet::OMTFrame;

enum TaggedFrame {
    Audio(OMTFrame),
    Video(OMTFrame),
}

const AUDIO_BURST_BEFORE_VIDEO: usize = 4;

struct Inner {
    audio_dropped: AtomicUsize,
    video_dropped: AtomicUsize,
    audio_send_zero: AtomicUsize,
    video_send_zero: AtomicUsize,
    running: AtomicBool,
    tx: ServerSenders,
    settings: SendSettings,
}

#[derive(Clone)]
pub struct SendCoordinator {
    inner: Arc<Inner>,
    sender: mpsc::SyncSender<TaggedFrame>,
}

impl SendCoordinator {
    pub fn new(tx: ServerSenders, settings: SendSettings) -> Self {
        let audio_capacity = clamp(settings.audio_queue_capacity, 1, 16);
        let video_capacity = clamp(settings.video_queue_capacity, 1, 8);
        // Channel capacity = total frames that can be in-flight.
        let channel_capacity = audio_capacity + video_capacity + 4;

        let (sender, receiver) = mpsc::sync_channel::<TaggedFrame>(channel_capacity);

        let inner = Arc::new(Inner {
            audio_dropped: AtomicUsize::new(0),
            video_dropped: AtomicUsize::new(0),
            audio_send_zero: AtomicUsize::new(0),
            video_send_zero: AtomicUsize::new(0),
            running: AtomicBool::new(true),
            tx,
            settings: SendSettings {
                audio_queue_capacity: audio_capacity,
                video_queue_capacity: video_capacity,
                force_zero_timestamps: settings.force_zero_timestamps,
            },
        });

        let thread_inner = inner.clone();
        thread::spawn(move || run_send_loop(thread_inner, receiver));

        Self { inner, sender }
    }

    #[allow(dead_code)]
    pub fn enqueue_audio(&self, frame: OMTFrame) {
        // try_send is non-blocking. If channel is full, drop the frame.
        if self
            .sender
            .try_send(TaggedFrame::Audio(frame))
            .is_err()
        {
            self.inner.audio_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[allow(dead_code)]
    pub fn enqueue_video(&self, frame: OMTFrame) -> bool {
        if self
            .sender
            .try_send(TaggedFrame::Video(frame))
            .is_err()
        {
            self.inner.video_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

impl Drop for SendCoordinator {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::SeqCst);
    }
}

fn run_send_loop(inner: Arc<Inner>, receiver: mpsc::Receiver<TaggedFrame>) {
    let mut audio_budget = AUDIO_BURST_BEFORE_VIDEO;
    let mut last_stats_log = Instant::now();
    // Local staging queues for priority scheduling.
    let mut audio_pending: Vec<OMTFrame> = Vec::new();
    let mut video_pending: Vec<OMTFrame> = Vec::new();

    while inner.running.load(Ordering::SeqCst) {
        // Drain all available frames from the channel (non-blocking after first).
        if audio_pending.is_empty() && video_pending.is_empty() {
            // Block on first frame to avoid busy-loop.
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(frame) => match frame {
                    TaggedFrame::Audio(f) => audio_pending.push(f),
                    TaggedFrame::Video(f) => video_pending.push(f),
                },
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Drain remaining without blocking.
        while let Ok(frame) = receiver.try_recv() {
            match frame {
                TaggedFrame::Audio(f) => audio_pending.push(f),
                TaggedFrame::Video(f) => video_pending.push(f),
            }
        }

        // Trim video to capacity (keep newest).
        let vcap = inner.settings.video_queue_capacity;
        if video_pending.len() > vcap {
            let drop_count = video_pending.len() - vcap;
            video_pending.drain(..drop_count);
            inner.video_dropped.fetch_add(drop_count, Ordering::Relaxed);
        }

        // Send with audio burst scheduling.
        let mut sent_any = true;
        while sent_any {
            sent_any = false;
            let has_audio = !audio_pending.is_empty();
            let has_video = !video_pending.is_empty();

            if has_video && (!has_audio || audio_budget == 0) {
                if let Some(frame) = pop_front(&mut video_pending) {
                    send_frame(&inner, frame, false);
                    audio_budget = AUDIO_BURST_BEFORE_VIDEO;
                    sent_any = true;
                }
            } else if has_audio {
                if let Some(frame) = pop_front(&mut audio_pending) {
                    send_frame(&inner, frame, true);
                    audio_budget = audio_budget.saturating_sub(1);
                    sent_any = true;
                }
            } else if let Some(frame) = pop_front(&mut video_pending) {
                send_frame(&inner, frame, false);
                audio_budget = AUDIO_BURST_BEFORE_VIDEO;
                sent_any = true;
            }
        }

        let now = Instant::now();
        if now.duration_since(last_stats_log).as_secs_f64() >= 30.0 {
            last_stats_log = now;
            let ad = inner.audio_dropped.swap(0, Ordering::Relaxed);
            let vd = inner.video_dropped.swap(0, Ordering::Relaxed);
            let az = inner.audio_send_zero.swap(0, Ordering::Relaxed);
            let vz = inner.video_send_zero.swap(0, Ordering::Relaxed);
            if ad > 0 || vd > 0 || az > 0 || vz > 0 {
                println!(
                    "Send stats: audioDropped={}, videoDropped={}, audioSendZero={}, videoSendZero={}",
                    ad, vd, az, vz
                );
            }
        }
    }
}

fn send_frame(inner: &Inner, mut frame: OMTFrame, is_audio: bool) {
    frame.header.timestamp = if inner.settings.force_zero_timestamps {
        0
    } else {
        crate::timebase::monotonic_100ns()
    };
    let send_result = if is_audio {
        inner.tx.audio.send(frame)
    } else {
        inner.tx.video.send(frame)
    };
    let receivers = match send_result {
        Ok(n) => n,
        Err(_) => 0,
    };
    if receivers == 0 {
        if is_audio {
            inner.audio_send_zero.fetch_add(1, Ordering::Relaxed);
        } else {
            inner.video_send_zero.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn pop_front<T>(v: &mut Vec<T>) -> Option<T> {
    if v.is_empty() {
        None
    } else {
        Some(v.remove(0))
    }
}

fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.clamp(min, max)
}
