use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::settings::SendSettings;
use libomtnet::OMTFrame;

struct Queues {
    audio: VecDeque<OMTFrame>,
    video: VecDeque<OMTFrame>,
}

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
    tx: broadcast::Sender<OMTFrame>,
    settings: SendSettings,
}

#[derive(Clone)]
pub struct SendCoordinator {
    inner: Arc<Inner>,
}

impl SendCoordinator {
    pub fn new(tx: broadcast::Sender<OMTFrame>, settings: SendSettings) -> Self {
        let audio_capacity = clamp(settings.audio_queue_capacity, 1, 16);
        let video_capacity = clamp(settings.video_queue_capacity, 1, 8);
        let inner = Arc::new(Inner {
            queues: Mutex::new(Queues {
                audio: VecDeque::with_capacity(audio_capacity),
                video: VecDeque::with_capacity(video_capacity),
            }),
            stats: Mutex::new(SendStats::default()),
            cv: Condvar::new(),
            running: AtomicBool::new(true),
            tx,
            settings: SendSettings {
                audio_queue_capacity: audio_capacity,
                video_queue_capacity: video_capacity,
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
        let queue = if is_audio {
            &mut guard.audio
        } else {
            &mut guard.video
        };
        let capacity = if is_audio {
            self.inner.settings.audio_queue_capacity
        } else {
            self.inner.settings.video_queue_capacity
        };

        while queue.len() >= capacity {
            queue.pop_front();
            let mut stats = self.inner.stats.lock().unwrap();
            if is_audio {
                stats.audio_dropped += 1;
            } else {
                stats.video_dropped += 1;
            }
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

            if let Some(frame) = guard.audio.pop_front() {
                next_frame = Some(frame);
                is_audio = true;
            } else if let Some(frame) = guard.video.pop_front() {
                next_frame = Some(frame);
            }
        }

        if let Some(mut frame) = next_frame {
            if inner.settings.force_zero_timestamps {
                frame.header.timestamp = 0;
            } else {
                frame.header.timestamp = monotonic_timestamp_100ns();
            }
            if inner.tx.send(frame).is_err() {
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

fn monotonic_timestamp_100ns() -> i64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    (start.elapsed().as_nanos() / 100) as i64
}
