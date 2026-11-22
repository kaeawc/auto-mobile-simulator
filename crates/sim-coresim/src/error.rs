//! Error type for simulator service calls.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong talking to the simulator service.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A private framework, class, method or symbol is missing on this host.
    #[error("simulator service API unavailable: {0}. Check `xcode-select -p` / DEVELOPER_DIR and that Xcode is installed")]
    Unavailable(String),
    /// The service reported an error for an operation.
    #[error("{op} failed: {message}")]
    Service { op: &'static str, message: String },
    /// No device matched.
    #[error("no device matches `{0}`")]
    NotFound(String),
    /// Several devices matched.
    #[error("`{0}` matches several devices; use a UDID")]
    Ambiguous(String),
    /// The device has no usable display (usually: not booted).
    #[error("device has no display: {0}")]
    NoDisplay(String),
    /// An argument was rejected before reaching the service.
    #[error("invalid argument: {0}")]
    Invalid(String),
}
