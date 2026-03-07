use thiserror::Error;

/// Errors produced by turbine buffer operations.
#[derive(Debug, Error)]
pub enum TurbineError {
    #[error("arena full: requested {requested} bytes, {available} available")]
    ArenaFull { requested: usize, available: usize },

    #[error("epoch {0} not found in clock ring")]
    EpochNotFound(u64),

    #[error("epoch {0} still has {1} outstanding leases")]
    EpochNotCollectable(u64, usize),

    #[error("arena still has outstanding leases on drop ({0} remaining)")]
    LeakedLeases(usize),

    #[error("mmap failed: {0}")]
    Mmap(std::io::Error),

    #[error("munmap failed: {0}")]
    Munmap(std::io::Error),

    #[error("io_uring buffer registration failed: {0}")]
    Registration(std::io::Error),

    #[error("pool configuration invalid: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, TurbineError>;
