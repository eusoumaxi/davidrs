//! In-process caches: [`moka`]'s own types, re-exported.
//!
//! Moka already has time-to-live, size-aware eviction and coalesced loads, so
//! this module adds no wrapper that would only hide its API. It pins one
//! version of that API for every function built on this crate.
//!
//! Whether a value is worth storing is the caller's rule and usually one
//! comparison: write it where the value is inserted.
//!
//! # Examples
//!
//! ```
//! use std::time::Duration;
//!
//! use davidrs::cache::Cache;
//!
//! let rates: Cache<String, u32> = Cache::builder()
//!     .max_capacity(1_000)
//!     .time_to_live(Duration::from_secs(300))
//!     .build();
//! # let _ = rates;
//! ```

pub use moka::future::{Cache, CacheBuilder};
pub use moka::policy::EvictionPolicy;
