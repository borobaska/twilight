use super::{headers::RatelimitHeaders, GlobalLockPair};
use crate::routing::Path;
use futures_channel::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot::{self, Sender},
};
use futures_timer::{Delay, TryFutureExt as _};
use futures_util::{
    future::FutureExt,
    lock::Mutex,
    stream::StreamExt,
    try_future::TryFutureExt as _,
};
use log::debug;
use std::{
    collections::HashMap,
    io::Error as IoError,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub enum TimeRemaining {
    Finished,
    NotStarted,
    Some(Duration),
}

#[derive(Debug)]
pub struct Bucket {
    pub limit: AtomicU64,
    pub path: Path,
    pub queue: BucketQueue,
    pub remaining: AtomicU64,
    pub reset_after: AtomicU64,
    pub started_at: Mutex<Option<Instant>>,
}

impl Bucket {
    pub fn new(path: Path) -> Self {
        Self {
            limit: AtomicU64::new(u64::max_value()),
            path,
            queue: BucketQueue::default(),
            remaining: AtomicU64::new(u64::max_value()),
            reset_after: AtomicU64::new(u64::max_value()),
            started_at: Mutex::new(None),
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn remaining(&self) -> u64 {
        self.remaining.load(Ordering::Relaxed)
    }

    pub fn reset_after(&self) -> u64 {
        self.reset_after.load(Ordering::Relaxed)
    }

    pub async fn time_remaining(&self) -> TimeRemaining {
        let reset_after = self.reset_after();
        let started_at = match *self.started_at.lock().await {
            Some(v) => v,
            None => return TimeRemaining::NotStarted,
        };
        let elapsed = started_at.elapsed();

        if elapsed > Duration::from_millis(reset_after) {
            return TimeRemaining::Finished;
        }

        TimeRemaining::Some(Duration::from_millis(reset_after) - elapsed)
    }

    pub async fn try_reset(&self) -> bool {
        if self.started_at.lock().await.is_none() {
            return false;
        }

        if let TimeRemaining::Finished = self.time_remaining().await {
            self.remaining.store(self.limit(), Ordering::Relaxed);
            *self.started_at.lock().await = None;

            true
        } else {
            false
        }
    }

    pub async fn update(&self, ratelimits: Option<(u64, u64, u64)>) {
        let bucket_limit = self.limit();

        {
            let mut started_at = self.started_at.lock().await;

            if started_at.is_none() {
                started_at.replace(Instant::now());
            }
        }

        if let Some((limit, remaining, reset_after)) = ratelimits {
            if bucket_limit != limit {
                if bucket_limit == u64::max_value() {
                    self.reset_after.store(reset_after, Ordering::SeqCst);
                    self.limit.store(limit, Ordering::SeqCst);
                }
            }

            self.remaining.store(remaining, Ordering::Relaxed);
        } else {
            self.remaining.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug)]
pub struct BucketQueue {
    rx: Mutex<UnboundedReceiver<Sender<Sender<Option<RatelimitHeaders>>>>>,
    tx: UnboundedSender<Sender<Sender<Option<RatelimitHeaders>>>>,
}

impl BucketQueue {
    pub fn push(&self, tx: Sender<Sender<Option<RatelimitHeaders>>>) {
        let _ = self.tx.unbounded_send(tx);
    }

    pub async fn pop(&self, timeout: Duration) -> Option<Sender<Sender<Option<RatelimitHeaders>>>> {
        let mut rx = self.rx.lock().await;

        // A bit of type weirdness here, but that's because `futures-timer`'s
        // timeout requires an `std::io::Error` for no reason.
        //
        // Once this closes this can be fixed:
        // <https://github.com/rustasync/futures-timer/issues/21>
        StreamExt::next(&mut *rx)
            .map(|x| x.ok_or(IoError::last_os_error()))
            .timeout(timeout)
            .await
            .ok()
    }
}

impl Default for BucketQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded();

        Self {
            rx: Mutex::new(rx),
            tx,
        }
    }
}

pub(super) struct BucketQueueTask {
    bucket: Arc<Bucket>,
    buckets: Arc<Mutex<HashMap<Path, Arc<Bucket>>>>,
    global: Arc<GlobalLockPair>,
    path: Path,
}

impl BucketQueueTask {
    const WAIT: Duration = Duration::from_secs(10);

    pub fn new(
        bucket: Arc<Bucket>,
        buckets: Arc<Mutex<HashMap<Path, Arc<Bucket>>>>,
        global: Arc<GlobalLockPair>,
        path: Path,
    ) -> Self {
        Self {
            bucket,
            buckets,
            global,
            path,
        }
    }

    pub async fn run(self) {
        debug!("[Bucket {:?}] Starting background queue task", self.path);

        while let Some(queue_tx) = self.next().await {
            let (tx, rx) = oneshot::channel();

            if self.global.is_locked() {
                self.global.0.lock().await;
            }

            let _ = queue_tx.send(tx);

            debug!(
                "[Bucket {:?}] Starting to wait for headers from response",
                self.path,
            );

            match rx
                .map_err(|_| IoError::last_os_error())
                .timeout(Self::WAIT)
                .await
            {
                Ok(Some(headers)) => self.handle_headers(&headers).await,
                // - None was sent through the channel (request aborted)
                // - channel was closed
                // - timeout reached
                Ok(None) | Err(_) => {
                    debug!("[Bucket {:?}] Receiver timed out", self.path);
                },
            }
        }

        debug!("[Bucket {:?}] Bucket appears finished, removing", self.path);

        self.buckets.lock().await.remove(&self.path);
    }

    async fn handle_headers(&self, headers: &RatelimitHeaders) {
        let ratelimits = match headers {
            RatelimitHeaders::GlobalLimited {
                reset_after,
            } => {
                self.lock_global(*reset_after).await;

                None
            },
            RatelimitHeaders::None => return,
            RatelimitHeaders::Present {
                global,
                limit,
                remaining,
                reset_after,
                ..
            } => {
                if *global {
                    self.lock_global(*reset_after).await;
                }

                Some((*limit, *remaining, *reset_after))
            },
        };

        debug!("[Bucket {:?}] Updating bucket", self.path);
        self.bucket.update(ratelimits).await;
    }

    async fn lock_global(&self, wait: u64) {
        debug!("[Bucket {:?}] Request got global ratelimited", self.path,);
        self.global.lock();
        let lock = self.global.0.lock().await;
        let _ = Delay::new(Duration::from_millis(wait)).await;
        self.global.unlock();

        drop(lock);
    }

    async fn next(&self) -> Option<Sender<Sender<Option<RatelimitHeaders>>>> {
        debug!("[Bucket {:?}] Starting to get next in queue", self.path);

        self.wait_if_needed().await;

        self.bucket.queue.pop(Self::WAIT).await
    }

    async fn wait_if_needed(&self) {
        let wait = {
            if self.bucket.remaining() > 0 {
                return;
            }

            debug!("[Bucket {:?}] 0 remaining, may have to wait", self.path);

            match self.bucket.time_remaining().await {
                TimeRemaining::Finished => {
                    self.bucket.try_reset().await;

                    return;
                },
                TimeRemaining::NotStarted => return,
                TimeRemaining::Some(dur) => dur,
            }
        };

        debug!(
            "[Bucket {:?}] Waiting for {:?} for ratelimit to pass",
            self.path, wait,
        );

        let _ = Delay::new(wait).await;

        debug!(
            "[Bucket {:?}] Done waiting for ratelimit to pass",
            self.path,
        );

        self.bucket.try_reset().await;
    }
}
