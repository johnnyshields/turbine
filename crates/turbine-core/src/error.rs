use thiserror::Error;

use crate::ArenaIdx;

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

    #[error("arena limit exceeded: {current} arenas, max {max}")]
    ArenaLimitExceeded { current: usize, max: usize },

    #[error("no registration slot available for arena {0}")]
    NoRegistrationSlot(ArenaIdx),

    #[error("madvise failed: {0}")]
    Madvise(std::io::Error),

    #[error("pool configuration invalid: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, TurbineError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_full_display() {
        let e = TurbineError::ArenaFull { requested: 1024, available: 512 };
        let msg = format!("{e}");
        assert!(msg.contains("1024"), "should contain requested bytes");
        assert!(msg.contains("512"), "should contain available bytes");
    }

    #[test]
    fn epoch_not_found_display() {
        let e = TurbineError::EpochNotFound(42);
        let msg = format!("{e}");
        assert!(msg.contains("42"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn epoch_not_collectable_display() {
        let e = TurbineError::EpochNotCollectable(7, 3);
        let msg = format!("{e}");
        assert!(msg.contains("7"));
        assert!(msg.contains("3"));
        assert!(msg.contains("leases"));
    }

    #[test]
    fn leaked_leases_display() {
        let e = TurbineError::LeakedLeases(5);
        let msg = format!("{e}");
        assert!(msg.contains("5"));
        assert!(msg.contains("leases"));
    }

    #[test]
    fn mmap_display() {
        let e = TurbineError::Mmap(std::io::Error::new(std::io::ErrorKind::Other, "oom"));
        let msg = format!("{e}");
        assert!(msg.contains("mmap"));
        assert!(msg.contains("oom"));
    }

    #[test]
    fn munmap_display() {
        let e = TurbineError::Munmap(std::io::Error::new(std::io::ErrorKind::Other, "bad"));
        let msg = format!("{e}");
        assert!(msg.contains("munmap"));
        assert!(msg.contains("bad"));
    }

    #[test]
    fn registration_display() {
        let e = TurbineError::Registration(std::io::Error::new(std::io::ErrorKind::Other, "fail"));
        let msg = format!("{e}");
        assert!(msg.contains("registration"));
        assert!(msg.contains("fail"));
    }

    #[test]
    fn arena_limit_exceeded_display() {
        let e = TurbineError::ArenaLimitExceeded { current: 10, max: 8 };
        let msg = format!("{e}");
        assert!(msg.contains("10"));
        assert!(msg.contains("8"));
    }

    #[test]
    fn no_registration_slot_display() {
        let e = TurbineError::NoRegistrationSlot(ArenaIdx::new(5));
        let msg = format!("{e}");
        assert!(msg.contains("5"));
        assert!(msg.contains("registration slot"));
    }

    #[test]
    fn madvise_display() {
        let e = TurbineError::Madvise(std::io::Error::new(std::io::ErrorKind::Other, "fail"));
        let msg = format!("{e}");
        assert!(msg.contains("madvise"));
        assert!(msg.contains("fail"));
    }

    #[test]
    fn invalid_config_display() {
        let e = TurbineError::InvalidConfig("too small".into());
        let msg = format!("{e}");
        assert!(msg.contains("invalid"));
        assert!(msg.contains("too small"));
    }
}
