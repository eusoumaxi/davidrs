//! [`StreamBody`]: a response body that owns the task producing it.
//!
//! What matters is what happens when things go wrong: the reader leaves, the
//! budget runs out, the producer panics or ignores cancellation. The Lambda
//! loop, `streaming::run`, is exercised in `tests/lambda_loop.rs`.
#![cfg(feature = "streaming")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use davidrs::streaming::StreamBody;
use davidrs::{Deadline, RuntimeError};
use futures_util::StreamExt as _;
use tokio::sync::oneshot;

fn a_minute() -> Deadline {
    Deadline::after(Duration::from_secs(60))
}

/// Sets its flag when dropped, which is how a test sees a future go away.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn wait_for(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the flag is set in time");
}

#[tokio::test]
async fn a_stream_delivers_every_chunk_in_order_then_ends() {
    let body = StreamBody::spawn(4, a_minute(), |producer| async move {
        for index in 0..3_u8 {
            if !producer.send(vec![index]).await {
                return;
            }
        }
    });
    let chunks: Vec<Bytes> = body.map(|chunk| chunk.expect("chunk")).collect().await;
    assert_eq!(chunks, [[0].as_slice(), &[1], &[2]]);
}

/// `mpsc::channel(0)` panics; the body reads a zero capacity as one.
#[tokio::test]
async fn a_zero_capacity_still_streams() {
    let body = StreamBody::spawn(0, a_minute(), |producer| async move {
        producer.send("a").await;
        producer.send("b").await;
    });
    assert_eq!(body.collect::<Vec<_>>().await.len(), 2);
}

#[tokio::test]
async fn the_producer_carries_the_deadline_it_was_given() {
    let deadline = a_minute();
    let (report, outcome) = oneshot::channel();
    let _body = StreamBody::spawn(1, deadline, move |producer| async move {
        let _ = report.send(producer.deadline());
    });
    assert_eq!(outcome.await.expect("reported"), deadline);
}

/// Dropping the body cancels the token and closes the channel at once, so a
/// cooperative producer sees both.
#[tokio::test]
async fn dropping_the_body_tells_the_producer_to_stop() {
    let (report, outcome) = oneshot::channel();
    let body = StreamBody::spawn(1, a_minute(), move |producer| async move {
        producer.cancelled().await;
        let sent = producer.send("late").await;
        let _ = report.send((producer.should_stop(), sent));
    });
    drop(body);
    let (should_stop, sent) = tokio::time::timeout(Duration::from_secs(2), outcome)
        .await
        .expect("the producer reports in time")
        .expect("the producer reports");
    assert!(should_stop);
    assert!(!sent, "nobody reads a dropped body");
}

#[tokio::test]
async fn a_producer_that_ignores_cancellation_is_dropped_after_the_grace_period() {
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    let mut body = StreamBody::spawn(1, a_minute(), |producer| async move {
        let _guard = DropFlag(flag);
        producer.send("started").await;
        std::future::pending::<()>().await;
    });
    body.next().await.expect("started").expect("chunk");
    drop(body);
    wait_for(&dropped).await;
}

#[tokio::test]
async fn an_expired_budget_ends_the_stream_even_while_someone_reads() {
    let body = StreamBody::spawn(
        4,
        Deadline::after(Duration::from_millis(200)),
        |producer| async move {
            producer.send("x").await;
            while !producer.should_stop() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
    );
    let chunks = tokio::time::timeout(Duration::from_secs(5), body.collect::<Vec<_>>())
        .await
        .expect("the stream ends on its own");
    assert_eq!(chunks.len(), 2);
    assert!(matches!(
        chunks.last(),
        Some(Err(RuntimeError::DeadlineExceeded { .. }))
    ));
}

#[tokio::test]
async fn a_producer_finishing_after_its_deadline_cannot_report_a_clean_end() {
    let deadline = Deadline::after(Duration::from_millis(100));
    let body = StreamBody::spawn(1, deadline, |producer| async move {
        std::thread::sleep(producer.deadline().remaining() + Duration::from_millis(5));
    });
    let chunks = body.collect::<Vec<_>>().await;
    assert!(matches!(
        chunks.last(),
        Some(Err(RuntimeError::DeadlineExceeded { .. }))
    ));
}

#[tokio::test]
async fn a_producer_failure_ends_the_stream_with_that_error() {
    let mut body = StreamBody::spawn(4, a_minute(), |producer| async move {
        producer.send("partial").await;
        producer
            .fail(RuntimeError::message("upstream refused"))
            .await;
    });
    assert_eq!(body.next().await.expect("chunk").expect("ok"), "partial");
    let error = body.next().await.expect("item").expect_err("failure");
    assert_eq!(error.to_string(), "upstream refused");
    assert!(body.next().await.is_none());
}

/// The panic message is not repeated: it may hold whatever the producer was
/// working on.
#[tokio::test]
async fn a_producer_panic_is_a_final_error_item_not_a_clean_end() {
    let mut body = StreamBody::spawn(4, a_minute(), |producer| async move {
        producer.send("first").await;
        panic!("secret detail");
    });
    assert_eq!(body.next().await.expect("chunk").expect("ok"), "first");
    let error = body.next().await.expect("item").expect_err("panic");
    assert_eq!(
        error.to_string(),
        "the stream producer stopped unexpectedly"
    );
    assert!(
        body.next().await.is_none(),
        "the stream ends after reporting"
    );
}

/// The channel closes before the task ends, so the body must wait for the
/// task rather than end the stream on the closed channel.
#[tokio::test]
async fn a_panic_after_the_producer_lets_go_of_the_channel_is_still_reported() {
    let mut body = StreamBody::spawn(1, a_minute(), |producer| async move {
        drop(producer);
        tokio::time::sleep(Duration::from_millis(10)).await;
        panic!("late");
    });
    let item = body.next().await.expect("an error item");
    assert!(item.is_err());
    assert!(body.next().await.is_none());
}

#[tokio::test]
async fn a_ready_chunk_needs_no_producer() {
    let chunks: Vec<_> = StreamBody::once(Some(Bytes::from_static(b"image")))
        .collect()
        .await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].as_ref().expect("chunk"), "image");
    assert!(StreamBody::once(None).collect::<Vec<_>>().await.is_empty());
}

#[tokio::test]
async fn shutdown_cancels_the_producer_and_waits_for_it() {
    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    let body = StreamBody::spawn(1, a_minute(), |producer| async move {
        producer.cancelled().await;
        flag.store(true, Ordering::SeqCst);
    });
    tokio::time::timeout(Duration::from_secs(2), body.shutdown())
        .await
        .expect("shutdown completes");
    assert!(finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn shutting_down_a_ready_body_returns_at_once() {
    tokio::time::timeout(Duration::from_secs(1), StreamBody::once(None).shutdown())
        .await
        .expect("nothing to wait for");
}
