use rbitcoin_primitives::ParseNetworkError;
use rbitcoin_store::StoreError;
use std::fmt;
use std::path::PathBuf;

/// Startup refuses a tip more than [`MAX_FUTURE_BLOCK_TIME`] past the node clock.
pub use rbitcoin_consensus::{time_too_new as tip_too_far_in_future, MAX_FUTURE_BLOCK_TIME};

/// Startup abort when the store tip is more than [`MAX_FUTURE_BLOCK_TIME`] ahead.
pub const FUTURE_BLOCK_DB_MSG: &str = "Store tip time is more than two hours ahead of the node clock. Check the clock (or --mock-time). Wipe the datadir and redo IBD only if you are sure the clock is correct.";

#[derive(Debug)]
pub enum NodeError {
    Config(String),
    /// Operator InitError: exit 1, `Error:` prefix (Core functional mapping).
    Init(String),
    /// Tip time is more than two hours ahead of the node clock.
    FutureTip,
    Network(ParseNetworkError),
    Datadir {
        path: PathBuf,
        source: std::io::Error,
    },
    Store(StoreError),
    /// Exclusive datadir flock is held by another process.
    Locked(PathBuf),
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeError::Config(s) => write!(f, "configuration error: {s}"),
            NodeError::Init(s) => write!(f, "Error: {s}"),
            NodeError::FutureTip => write!(f, "{FUTURE_BLOCK_DB_MSG}"),
            NodeError::Network(e) => write!(f, "{e}"),
            NodeError::Datadir { path, source } => {
                write!(f, "datadir error at {}: {source}", path.display())
            }
            NodeError::Store(e) => write!(f, "{e}"),
            NodeError::Locked(p) => write!(f, "{}", crate::lock::lock_busy_msg(p)),
        }
    }
}

impl std::error::Error for NodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NodeError::Network(e) => Some(e),
            NodeError::Datadir { source, .. } => Some(source),
            NodeError::Store(e) => Some(e),
            NodeError::Config(_)
            | NodeError::Init(_)
            | NodeError::FutureTip
            | NodeError::Locked(_) => None,
        }
    }
}

impl From<ParseNetworkError> for NodeError {
    fn from(e: ParseNetworkError) -> Self {
        NodeError::Network(e)
    }
}

impl From<StoreError> for NodeError {
    fn from(e: StoreError) -> Self {
        NodeError::Store(e)
    }
}
