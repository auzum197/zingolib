//! General library utilities such as parsing and conversions.

pub mod conversion;
pub mod error;

/// Returns number of seconds since unix epoch.
#[cfg(any(test, feature = "testutils"))]
pub(crate) fn now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("should never fail when comparing with an instant so far in the past")
        .as_secs() as u32
}
