use crate::error::{Result, TurbineError};

/// Configuration for an [`IouringBufferPool`](crate::buffer::IouringBufferPool).
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Size of each arena in bytes. Must be a multiple of the page size (4096).
    pub arena_size: usize,

    /// Number of arenas to allocate at startup. Minimum 1.
    pub initial_arenas: usize,

    /// Maximum number of arenas to keep in the free pool before munmapping.
    pub max_free_arenas: usize,

    /// Hard cap on total arenas. 0 = unlimited.
    pub max_total_arenas: usize,

    /// Number of pre-allocated io_uring registration slots.
    pub registration_slots: usize,

    /// Page size for mmap alignment. Defaults to 4096.
    pub page_size: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            arena_size: 2 * 1024 * 1024, // 2 MiB
            initial_arenas: 4,
            max_free_arenas: 4,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        }
    }
}

impl PoolConfig {
    pub fn validate(&self) -> Result<()> {
        if self.initial_arenas < 1 {
            return Err(TurbineError::InvalidConfig(
                "initial_arenas must be >= 1".into(),
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
        if self.registration_slots < self.initial_arenas {
            return Err(TurbineError::InvalidConfig(format!(
                "registration_slots ({}) must be >= initial_arenas ({})",
                self.registration_slots, self.initial_arenas
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
    fn initial_arenas_zero_rejected() {
        let cfg = PoolConfig {
            initial_arenas: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("initial_arenas"), "error should mention initial_arenas: {msg}");
    }

    #[test]
    fn initial_arenas_one_accepted() {
        let cfg = PoolConfig {
            initial_arenas: 1,
            registration_slots: 1,
            ..Default::default()
        };
        cfg.validate().unwrap();
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
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("arena_size"), "error should mention arena_size: {msg}");
    }

    #[test]
    fn page_size_zero_rejected() {
        let cfg = PoolConfig {
            page_size: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("page_size"), "error should mention page_size: {msg}");
    }

    #[test]
    fn page_size_not_power_of_two_rejected() {
        let cfg = PoolConfig {
            arena_size: 6000,
            page_size: 6000,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("power of two"), "error should mention power of two: {msg}");
    }

    #[test]
    fn registration_slots_less_than_initial_arenas_rejected() {
        let cfg = PoolConfig {
            initial_arenas: 4,
            registration_slots: 2,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("registration_slots"), "error should mention registration_slots: {msg}");
    }
}
