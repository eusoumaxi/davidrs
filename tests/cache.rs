//! The re-exported Moka types behave as Moka documents them.
#![cfg(feature = "cache")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use davidrs::cache::{Cache, CacheBuilder, EvictionPolicy};

#[tokio::test]
async fn concurrent_misses_on_one_key_run_one_load() {
    let cache: Cache<&'static str, u32> = Cache::new(16);
    let loads = Arc::new(AtomicUsize::new(0));
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let cache = cache.clone();
            let loads = Arc::clone(&loads);
            tokio::spawn(async move {
                cache
                    .get_with("rate", async move {
                        loads.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        7
                    })
                    .await
            })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.expect("task"), 7);
    }
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_entry_expires_after_its_time_to_live() {
    let cache: Cache<&'static str, u32> = CacheBuilder::new(16)
        .eviction_policy(EvictionPolicy::lru())
        .time_to_live(Duration::from_millis(50))
        .build();
    cache.insert("rate", 7).await;
    assert_eq!(cache.get("rate").await, Some(7));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(cache.get("rate").await, None);
}
