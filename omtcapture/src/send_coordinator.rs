use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use libomtnet::ServerSenders;

use crate::settings::SendSettings;
use libomtnet::OMTFrame;

struct Queues {
    audio: VecDeque<OMTFrame>,
    video: VecDeque<OMTFrame>,
}

// If both audio and video are queued, don't let audio fully starve video.
// `AudioPipeline` can enqueue far more frequently than video; if we always send audio first,
// video can accumulate seconds of delay.
const AUDIO_BURST_BEFORE_VIDEO: usize = 4;

struct Inner {
    queues: Mutex<Queues>,
    // Lock-free stats to avoid contention with the queue mutex.
    audio_dropped: AtomicUsize,
    video_dropped: AtomicUsize,
    audio_send_zero: AtomicUsize,
    video_send_zero: AtomicUsize,
    cv: Condvar,
    running: AtomicBool,
    tx: ServerSenders,
    settings: SendSettings,
}

#[derive(Clone)]
pub struct SendCoordinator {
    inner: Arc<Inner>,
}

impl SendCoordinator {
    pub fn new(tx: ServerSenders, settings: SendSettings) -> Self {
        let audio_capacity = clamp(settings.audio_queue_capacity, 1, 16);
        let video_capacity = clamp(settings.video_queue_capacity, 1, 8);
        let inner = Arc::new(Inner {
            queues: Mutex::new(Queues {
                audio: VecDeque::with_capacity(audio_capacity),
                video: VecDeque::with_capacity(video_capacity),
            }),
            audio_dropped: AtomicUsize::new(0),
            video_dropped: AtomicUsize::new(0),
            audio_send_zero: AtomicUsize::new(0),
            video_send_zero: AtomicUsize::new(0),
            cv: Condvar::new(),
            running: AtomicBool::new(true),
            tx,
            settings: SendSettings {
                audio_queue_capacity: audio_capacity,
                // Keep the setting but sender uses latest-wins semantics regardless.
                video_queue_capacity: clamp(settings.video_queue_capacity, 1, 8),
                force_zero_timestamps: settings.force_zero_timestamps,
            },
        });

        let thread_inner = inner.clone();
        thread::spawn(move || run_send_loop(thread_inner));

        Self { inner }
    }

    #[allow(dead_code)]
    pub fn enqueue_audio(&self, frame: OMTFrame) {
        self.enqueue(frame, true);
    }

    #[allow(dead_code)]
    pub fn enqueue_video(&self, frame: OMTFrame) -> bool {
        self.enqueue(frame, false)
    }

    fn enqueue(&self, frame: OMTFrame, is_audio: bool) -> bool {
        let mut guard = self.inner.queues.lock().unwrap();
        if !is_audio {
            let queue = &mut guard.video;
            let capacity = self.inner.settings.video_queue_capacity;
            let mut dropped = 0usize;
            while queue.len() >= capacity {
                queue.pop_front();
                dropped += 1;
            }
            if dropped > 0 {
                self.inner.video_dropped.fetch_add(dropped, Ordering::Relaxed);
            }
            queue.push_back(frame);
            self.inner.cv.notify_one();
            return true;
        }

        let queue = &mut guard.audio;
        let capacity = self.inner.settings.audio_queue_capacity;

        let mut dropped = 0usize;
        while queue.len() >= capacity {
            queue.pop_front();
            dropped += 1;
        }
        if dropped > 0 {
            self.inner
                .audio_dropped
                .fetch_add(dropped, Ordering::Relaxed);
        }
        queue.push_back(frame);
        self.inner.cv.notify_one();
        true
    }
}

impl Drop for SendCoordinator {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::SeqCst);
        self.inner.cv.notify_all();
    }
}

fn run_send_loop(inner: Arc<Inner>) {
    let mut audio_budget = AUDIO_BURST_BEFORE_VIDEO;
    let mut last_stats_log = Instant::now();
    while inner.running.load(Ordering::SeqCst) {
        let mut next_frame = None;
        let mut is_audio = false;
        {
            let mut guard = inner.queues.lock().unwrap();
            if guard.audio.is_empty() && guard.video.is_empty() {
                guard = inner
                    .cv
                    .wait_timeout(guard, Duration::from_millis(10))
                    .unwrap()
                    .0;
            }

            let has_audio = !guard.audio.is_empty();
            let has_video = !guard.video.is_empty();

            if has_video && (!has_audio || audio_budget == 0) {
                if let Some(frame) = guard.video.pop_front() {
                    next_frame = Some(frame);
                    audio_budget = AUDIO_BURST_BEFORE_VIDEO;
                }
            } else if has_audio {
                if let Some(frame) = guard.audio.pop_front() {
                    next_frame = Some(frame);
                    is_audio = true;
                    audio_budget = audio_budget.saturating_sub(1);
                }
            } else if let Some(frame) = guard.video.pop_front() {
                next_frame = Some(frame);
                audio_budget = AUDIO_BURST_BEFORE_VIDEO;
            }
        }

        if let Some(mut frame) = next_frame {
            // Match C# sender behavior: timestamps are assigned at *send-time*.
            // This avoids large offsets when pipelines restart and reduces receiver-side buffering.
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

        // Log stats periodically without acquiring any mutex.
        let now = Instant::now();
        if now.duration_since(last_stats_log).as_secs_f64() >= 5.0 {
            last_stats_log = now;
            let ad = inner.audio_dropped.swap(0, Ordering::Relaxed);
            let vd = inner.video_dropped.swap(0, Ordering::Relaxed);
            let az = inner.audio_send_zero.swap(0, Ordering::Relaxed);
            let vz = inner.video_send_zero.swap(0, Ordering::Relaxed);
            if ad > 0 || vd > 0 || az > 0 || vz > 0 {
                println!(
                    "Send stats (last 5s): audioDropped={}, videoDropped={}, audioSendZero={}, videoSendZero={}",
                    ad, vd, az, vz
                );
            }
        }
    }
}

fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.clamp(min, max)
}
