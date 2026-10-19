//! Error type shared by every Partition Manager crate.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at offset {offset} (len {len}): {source}")]
    Io {
        offset: u64,
        len: usize,
        #[source]
        source: std::io::Error,
    },
    #[error("parse error: {0}")]
    Parse(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("cancelled")]
    Cancelled,
    #[error("platform error {code}: {msg}")]
    Platform { code: u32, msg: String },
    #[error("device busy: {0}")]
    Busy(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
}

impl Error {
    /// Build an [`Error::Io`] carrying the offset and length of the failed access.
    pub fn io(offset: u64, len: usize, e: std::io::Error) -> Self {
        Error::Io {
            offset,
            len,
            source: e,
        }
    }
}
