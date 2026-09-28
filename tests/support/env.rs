//! Changes to the process environment, for tests that read configuration.
//!
//! Writing the environment is `unsafe` since Rust 2024: another thread may
//! read it at the same moment. These helpers are the only unsafe code in the
//! repository, and every test that calls them first takes its file's
//! environment lock, or runs before any other thread of its process starts.

/// Sets `name` to `value` while the caller holds its environment lock.
#[expect(unsafe_code)]
pub fn set(name: &str, value: impl AsRef<std::ffi::OsStr>) {
    unsafe { std::env::set_var(name, value) }
}

/// Removes `name` while the caller holds its environment lock.
#[expect(unsafe_code)]
pub fn remove(name: &str) {
    unsafe { std::env::remove_var(name) }
}
