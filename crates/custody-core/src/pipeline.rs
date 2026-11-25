//! Fetch finalized heights concurrently; deliver validated results in sequence.
use futures_util::{future::try_join, stream, StreamExt};
use std::{
    future::Future,
    num::NonZeroUsize,
    ops::RangeInclusive,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// A per-reader cap on heights in flight, separate from retry delays and queue size.
pub struct FetchLimit {
    configured: NonZeroUsize,
    current: NonZeroUsize,
    last_change: Instant,
}

impl FetchLimit {
    pub fn new(configured: NonZeroUsize) -> Self {
        Self {
            configured,
            current: configured,
            last_change: Instant::now(),
        }
    }

    pub fn current(&self) -> NonZeroUsize {
        self.current
    }

    pub fn throttled(&mut self) {
        self.current = NonZeroUsize::new(self.current.get().div_ceil(2)).unwrap();
        self.last_change = Instant::now();
    }

    pub fn drained(&mut self) {
        self.recover_at(Instant::now());
    }

    fn recover_at(&mut self, now: Instant) {
        if self.current < self.configured
            && now.duration_since(self.last_change) >= Duration::from_secs(60)
        {
            self.current = NonZeroUsize::new(self.current.get() + 1).unwrap();
            self.last_change = now;
        }
    }
}

/// The source bounds each response and emits compact observations, never raw
/// blocks. The sink returns success only after its durable transaction commits.
/// A failed height ends the run; resume from the *persisted* cursor, not the
/// highest scheduled or downloaded height.
pub async fn ordered_range<T, E, F, W>(
    heights: RangeInclusive<u64>,
    concurrency: NonZeroUsize,
    ready_capacity: NonZeroUsize,
    fetch: impl Fn(u64) -> F,
    write: impl FnMut(u64, T) -> W,
) -> Result<(), E>
where
    F: Future<Output = Result<T, E>>,
    W: Future<Output = Result<(), E>>,
{
    ordered_range_observed(heights, concurrency, ready_capacity, fetch, write, |_| ()).await
}

/// One sample after a successful commit. The target is the finalized head
/// sampled at the start of this range, not an estimate of the live chain tip.
pub struct Progress {
    pub height: u64,
    pub target: u64,
    pub count: u64,
    pub elapsed: Duration,
    pub fetch_time: Duration,
    pub write_time: Duration,
    pub queue_high_water: usize,
    pub capacity: usize,
}

#[derive(Default)]
pub struct HeadPace {
    prior: Option<(Instant, i64, i64)>,
}

impl HeadPace {
    /// Compare durable cursor and finalized head over the same wall-clock interval,
    /// including idle polls, RPC delays and time between passes.
    pub fn sample(&mut self, network: &str, head: i64, committed: i64) -> bool {
        let now = Instant::now();
        if let Some((last, old_head, old_commit)) = self.prior {
            let elapsed = now.duration_since(last);
            if elapsed < Duration::from_secs(15) {
                return false;
            }
            let dt = elapsed.as_secs_f64();
            let received = (i128::from(head) - i128::from(old_head)) as f64 / dt;
            let written = (i128::from(committed) - i128::from(old_commit)) as f64 / dt;
            eprintln!("{network} finalized={head} committed={committed} lag={} head/s={received:.2} committed/s={written:.2} net_catchup/s={:.2}",
                i128::from(head)-i128::from(committed), written-received);
        }
        self.prior = Some((now, head, committed));
        true
    }
}

pub fn log_progress(network: &'static str) -> impl FnMut(Progress) {
    let mut last = Instant::now();
    move |p| {
        if p.height == p.target || last.elapsed() >= Duration::from_secs(10) {
            let count = p.count as f64;
            eprintln!("{network} committed={} target={} remaining={} heights/s={:.2} mean_fetch_decode_ms={:.1} mean_commit_ms={:.1} peak_ready={}/{}",
                p.height, p.target, p.target - p.height, count / p.elapsed.as_secs_f64().max(1e-9),
                p.fetch_time.as_secs_f64()*1000.0/count, p.write_time.as_secs_f64()*1000.0/count,
                p.queue_high_water, p.capacity);
            last = Instant::now();
        }
    }
}

pub async fn ordered_range_observed<T, E, F, W>(
    heights: RangeInclusive<u64>,
    concurrency: NonZeroUsize,
    ready_capacity: NonZeroUsize,
    fetch: impl Fn(u64) -> F,
    mut write: impl FnMut(u64, T) -> W,
    mut observe: impl FnMut(Progress),
) -> Result<(), E>
where
    F: Future<Output = Result<T, E>>,
    W: Future<Output = Result<(), E>>,
{
    let target = *heights.end();
    let start = Instant::now();
    let (tx, mut rx) = mpsc::channel(ready_capacity.get());
    let producer = async move {
        let mut items = stream::iter(heights)
            .map(|height| {
                let next = fetch(height);
                async move {
                    let started = Instant::now();
                    let item = next.await;
                    (height, item, started.elapsed())
                }
            })
            .buffered(concurrency.get());
        while let Some((height, item, duration)) = items.next().await {
            let failed = item.is_err();
            // Send a fetch failure in order so the verified prefix can commit.
            if tx.send((height, item, duration)).await.is_err() || failed {
                break;
            }
        }
        Ok(())
    };
    let writer = async move {
        let (mut count, mut peak) = (0, 0);
        let (mut fetch_time, mut write_time) = (Duration::ZERO, Duration::ZERO);
        while let Some((height, next, duration)) = rx.recv().await {
            peak = peak.max(rx.len() + 1);
            let began = Instant::now();
            write(height, next?).await?;
            write_time += began.elapsed();
            fetch_time += duration;
            count += 1;
            observe(Progress {
                height,
                target,
                count,
                elapsed: start.elapsed(),
                fetch_time,
                write_time,
                queue_high_water: peak,
                capacity: ready_capacity.get(),
            });
        }
        Ok(())
    };
    try_join(producer, writer).await.map(|_| ())
}

#[cfg(test)]
mod throttle_tests {
    use super::*;

    #[test]
    fn throttle_stays_nonzero_and_recovers_one_slot_per_healthy_minute() {
        let mut limit = FetchLimit::new(NonZeroUsize::new(5).unwrap());
        for expected in [3, 2, 1, 1] {
            limit.throttled();
            assert_eq!(limit.current().get(), expected);
        }
        let last = limit.last_change;
        for (secs, slots) in [
            (59, 1),
            (60, 2),
            (61, 2),
            (120, 3),
            (180, 4),
            (240, 5),
            (300, 5),
        ] {
            limit.recover_at(last + Duration::from_secs(secs));
            assert_eq!(limit.current().get(), slots);
        }
    }
}
