use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use libomtnet::ServerSenders;

use crate::settings::SendSettings;
use libomtnet::OMTFrame;

struct Queues {
    audio: VecDeque<OMTFrame>,
    // Latest-wins: video should never build latency. Keep only the newest frame.
    video_latest: Option<OMTFrame>,
}

// If both audio and video are queued, don't let audio fully starve video.
// `AudioPipeline` can enqueue far more frequently than video; if we always send audio first,
// video can accumulate seconds of delay.
const AUDIO_BURST_BEFORE_VIDEO: usize = 4;

#[derive(Default)]
struct SendStats {
    audio_dropped: usize,
    video_dropped: usize,
    audio_send_zero: usize,
    video_send_zero: usize,
    last_log: Option<Instant>,
}

struct Inner {
    queues: Mutex<Queues>,
    stats: Mutex<SendStats>,
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
        let inner = Arc::new(Inner {
            queues: Mutex::new(Queues {
                audio: VecDeque::with_capacity(audio_capacity),
                video_latest: None,
            }),
            stats: Mutex::new(SendStats::default()),
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
            // Replace the previous unsent frame to avoid building delay.
            if guard.video_latest.is_some() {
                let mut stats = self.inner.stats.lock().unwrap();
                stats.video_dropped += 1;
            }
            guard.video_latest = Some(frame);
            self.inner.cv.notify_one();
            return true;
        }

        let queue = &mut guard.audio;
        let capacity = self.inner.settings.audio_queue_capacity;

        while queue.len() >= capacity {
            queue.pop_front();
            let mut stats = self.inner.stats.lock().unwrap();
            stats.audio_dropped += 1;
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
    while inner.running.load(Ordering::SeqCst) {
        let mut next_frame = None;
        let mut is_audio = false;
        {
            let mut guard = inner.queues.lock().unwrap();
            if guard.audio.is_empty() && guard.video_latest.is_none() {
                guard = inner
                    .cv
                    .wait_timeout(guard, Duration::from_millis(10))
                    .unwrap()
                    .0;
            }

            let has_audio = !guard.audio.is_empty();
            let has_video = guard.video_latest.is_some();

            // If video is waiting and we've sent a burst of audio frames, force a video frame out.
            if has_video && (!has_audio || audio_budget == 0) {
                if let Some(frame) = guard.video_latest.take() {
                    next_frame = Some(frame);
                    audio_budget = AUDIO_BURST_BEFORE_VIDEO;
                }
            } else if has_audio {
                if let Some(frame) = guard.audio.pop_front() {
                    next_frame = Some(frame);
                    is_audio = true;
                    audio_budget = audio_budget.saturating_sub(1);
                }
            } else if let Some(frame) = guard.video_latest.take() {
                next_frame = Some(frame);
                audio_budget = AUDIO_BURST_BEFORE_VIDEO;
            }
        }

        if let Some(mut frame) = next_frame {
            // Keep capture timestamps as produced by the audio/video pipelines.
            // Optionally force zero timestamps for debugging.
            if inner.settings.force_zero_timestamps {
                frame.header.timestamp = 0;
            }
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
                let mut stats = inner.stats.lock().unwrap();
                if is_audio {
                    stats.audio_send_zero += 1;
                } else {
                    stats.video_send_zero += 1;
                }
            }
        }

        log_stats(&inner);
    }
}

fn log_stats(inner: &Arc<Inner>) {
    let mut stats = inner.stats.lock().unwrap();
    let now = Instant::now();
    if let Some(last) = stats.last_log {
        if now.duration_since(last).as_secs_f64() < 5.0 {
            return;
        }
    }

    if stats.audio_dropped > 0
        || stats.video_dropped > 0
        || stats.audio_send_zero > 0
        || stats.video_send_zero > 0
    {
        println!(
            "Send stats (last 5s): audioDropped={}, videoDropped={}, audioSendZero={}, videoSendZero={}",
            stats.audio_dropped, stats.video_dropped, stats.audio_send_zero, stats.video_send_zero
        );
    }

    stats.audio_dropped = 0;
    stats.video_dropped = 0;
    stats.audio_send_zero = 0;
    stats.video_send_zero = 0;
    stats.last_log = Some(now);
}

fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.clamp(min, max)
}
