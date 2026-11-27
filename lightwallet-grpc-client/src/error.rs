//! Error types for the [`Indexer`](super::Indexer) and
//! `TransparentIndexer` traits.
//!
/// Callers can depend on:
/// - `InvalidScheme` and `InvalidAuthority` are deterministic — retrying
///   with the same URI will always fail.
/// - `Transport` wraps a [`tonic::transport::Error`] and may be transient
///   (e.g. DNS resolution, TCP connect timeout). Retrying may succeed.
///
/// ```
/// use lightwallet_grpc_client::GetClientError;
///
/// let e = GetClientError::InvalidScheme;
/// assert_eq!(e.to_string(), "bad uri: invalid scheme");
///
/// let e = GetClientError::InvalidAuthority;
/// assert_eq!(e.to_string(), "bad uri: invalid authority");
///
/// // Transport variant accepts From<tonic::transport::Error>
/// let _: fn(tonic::transport::Error) -> GetClientError = GetClientError::from;
/// ```
#[derive(Debug, thiserror::Error)]
pub enum GetClientError {
    /// The URI scheme is not `http` or `https`.
    #[error("bad uri: invalid scheme")]
    InvalidScheme,

    /// The URI has no authority.
    #[error("bad uri: invalid authority")]
    InvalidAuthority,

    /// The gRPC transport could not be set up.
    #[error(transparent)]
    Transport(#[from] tonic::transport::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_from_conversion() {
        // Verify the From impl exists at compile time.
        let _: fn(tonic::transport::Error) -> GetClientError = GetClientError::from;
    }
}
