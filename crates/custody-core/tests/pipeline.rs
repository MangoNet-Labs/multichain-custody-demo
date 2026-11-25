use custody_core::pipeline::{ordered_range, ordered_range_observed};
use std::{
    cell::{Cell, RefCell},
    future::ready,
    num::NonZeroUsize,
};
use tokio::sync::{oneshot, Notify};

fn n(size: usize) -> NonZeroUsize {
    NonZeroUsize::new(size).unwrap()
}

#[tokio::test]
async fn progress_only_counts_successful_commits_not_prefetched_heights() {
    let mut seen = Vec::new();
    let result = ordered_range_observed(
        14..=17,
        n(3),
        n(2),
        |_| ready(Ok(())),
        |height, _| {
            ready(if height == 16 {
                Err("rollback")
            } else {
                Ok(())
            })
        },
        |p| {
            assert!((1..=p.capacity).contains(&p.queue_high_water));
            seen.push((p.height, p.target, p.count));
        },
    )
    .await;
    assert_eq!(result, Err("rollback"));
    assert_eq!(seen, [(14, 17, 1), (15, 17, 2)]);
}

#[tokio::test]
async fn a_delayed_first_height_does_not_serialize_downloads_or_reorder_writes() {
    let ready_third = Notify::new();
    let ready_third = &ready_third;
    let writes = RefCell::new(Vec::new());
    ordered_range(
        1..=6,
        n(3),
        n(1),
        |h| async move {
            if h == 1 {
                ready_third.notified().await;
            }
            if h == 3 {
                ready_third.notify_one();
            }
            Ok::<_, ()>(h)
        },
        |h, observed| {
            writes.borrow_mut().push((h, observed));
            ready(Ok(()))
        },
    )
    .await
    .unwrap();
    assert_eq!(
        writes.into_inner(),
        (1..=6).map(|h| (h, h)).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_missing_height_commits_the_prefix_only_and_resume_starts_at_the_gap() {
    let mut committed = Vec::new();
    let mut store = |h, ()| {
        committed.push(h);
        ready(Ok(()))
    };
    let run = ordered_range(
        20..=26,
        n(4),
        n(2),
        |h| ready(if h == 23 { Err("no receipts") } else { Ok(()) }),
        &mut store,
    );
    assert_eq!(run.await, Err("no receipts"));
    assert_eq!(committed, [20, 21, 22]);
    let next = committed.last().unwrap() + 1;
    ordered_range(
        next..=26,
        n(4),
        n(2),
        |_| ready(Ok::<_, ()>(())),
        |h, ()| {
            committed.push(h);
            ready(Ok(()))
        },
    )
    .await
    .unwrap();
    assert_eq!(committed, [20, 21, 22, 23, 24, 25, 26]);
}

#[tokio::test]
async fn a_blocked_writer_bounds_prefetch_and_an_aborted_write_stops_delivery() {
    let (signal, busy) = oneshot::channel();
    let (release, wait) = oneshot::channel::<()>();
    let mut first_write = Some((signal, wait));
    let fetched = Cell::new(0);
    let process = ordered_range(
        1..=10_000,
        n(4),
        n(2),
        |h| {
            fetched.set(h);
            ready(Ok(h))
        },
        |h, _| {
            assert_eq!(h, 1);
            let (signal, wait) = first_write.take().unwrap();
            async move {
                signal.send(()).unwrap();
                wait.await.unwrap();
                Err("database unavailable")
            }
        },
    );
    let observe = async {
        busy.await.unwrap();
        tokio::task::yield_now().await;
        // Four in the ordering window, two queued, one being written,
        // and at most one result waiting to be enqueued.
        assert!((2..=8).contains(&fetched.get()));
        release.send(()).unwrap();
    };
    let (outcome, ()) = tokio::join!(process, observe);
    assert_eq!(outcome, Err("database unavailable"));
}
