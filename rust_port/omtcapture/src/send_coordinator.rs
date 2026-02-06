use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use tokio::sync::broadcast;

use libomtnet::OMTFrame;
use crate::settings::SendSettings;

struct Queues {
    audio: VecDeque<OMTFrame>,
    video: VecDeque<OMTFrame>,
}

struct Inner {
    queues: Mutex<Queues>,
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
        let inner = Arc::new(Inner {
            queues: Mutex::new(Queues {
                audio: VecDeque::with_capacity(settings.audio_queue_capacity),
                video: VecDeque::with_capacity(settings.video_queue_capacity),
            }),
            cv: Condvar::new(),
            running: AtomicBool::new(true),
            tx,
            settings,
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
    pub fn enqueue_video(&self, frame: OMTFrame) {
        self.enqueue(frame, false);
    }

    #[allow(dead_code)]
    fn enqueue(&self, frame: OMTFrame, is_audio: bool) {
        let mut guard = self.inner.queues.lock().unwrap();
        let queue = if is_audio { &mut guard.audio } else { &mut guard.video };
        let capacity = if is_audio {
            self.inner.settings.audio_queue_capacity
        } else {
            self.inner.settings.video_queue_capacity
        };

        if capacity == 0 {
            return;
        }

        while queue.len() >= capacity {
            queue.pop_front();
        }
        queue.push_back(frame);
        self.inner.cv.notify_one();
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
        {
            let mut guard = inner.queues.lock().unwrap();
            if guard.audio.is_empty() && guard.video.is_empty() {
                guard = inner.cv.wait_timeout(guard, Duration::from_millis(10)).unwrap().0;
            }

            if let Some(frame) = guard.audio.pop_front() {
                next_frame = Some(frame);
            } else if let Some(frame) = guard.video.pop_front() {
                next_frame = Some(frame);
            }
        }

        if let Some(mut frame) = next_frame {
            if inner.settings.force_zero_timestamps {
                frame.header.timestamp = 0;
            }
            let _ = inner.tx.send(frame);
        }
    }
}
