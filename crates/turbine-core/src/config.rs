use crate::error::{Result, TurbineError};

/// Configuration for an [`IouringBufferPool`](crate::buffer::IouringBufferPool).
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Size of each arena in bytes. Must be a multiple of the page size (4096).
    pub arena_size: usize,

    /// Number of arenas in the epoch ring. Minimum 2.
    pub arena_count: usize,

    /// Page size for mmap alignment. Defaults to 4096.
    pub page_size: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            arena_size: 2 * 1024 * 1024, // 2 MiB
            arena_count: 4,
            page_size: 4096,
        }
    }
}

impl PoolConfig {
    pub fn validate(&self) -> Result<()> {
        if self.arena_count < 2 {
            return Err(TurbineError::InvalidConfig(
                "arena_count must be >= 2".into(),
            ));
        }
        if self.page_size == 0 || (self.page_size & (self.page_size - 1)) != 0 {
            return Err(TurbineError::InvalidConfig(format!(
                "page_size ({}) must be a power of two",
                self.page_size
            )));
        }
        if self.arena_size == 0 {
            return Err(TurbineError::InvalidConfig(
                "arena_size must be > 0".into(),
            ));
        }
        if !self.arena_size.is_multiple_of(self.page_size) {
            return Err(TurbineError::InvalidConfig(format!(
                "arena_size ({}) must be a multiple of page_size ({})",
                self.arena_size, self.page_size
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        PoolConfig::default().validate().unwrap();
    }

    #[test]
    fn arena_count_below_two_rejected() {
        let cfg = PoolConfig {
            arena_count: 1,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unaligned_arena_size_rejected() {
        let cfg = PoolConfig {
            arena_size: 4097,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_power_of_two_page_size_rejected() {
        let cfg = PoolConfig {
            page_size: 3000,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn arena_size_zero_rejected() {
        let cfg = PoolConfig {
            arena_size: 0,
            arena_count: 2,
            page_size: 4096,
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("arena_size"), "error should mention arena_size: {msg}");
    }

    #[test]
    fn page_size_zero_rejected() {
        let cfg = PoolConfig {
            arena_size: 4096,
            arena_count: 2,
            page_size: 0,
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("page_size"), "error should mention page_size: {msg}");
    }

    #[test]
    fn page_size_not_power_of_two_rejected() {
        let cfg = PoolConfig {
            arena_size: 6000,
            arena_count: 2,
            page_size: 6000,
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("power of two"), "error should mention power of two: {msg}");
    }

    #[test]
    fn arena_count_one_rejected() {
        let cfg = PoolConfig {
            arena_size: 4096,
            arena_count: 1,
            page_size: 4096,
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("arena_count"), "error should mention arena_count: {msg}");
    }
}
