use crate::epoch::arena::Arena;
use crate::error::{Result, TurbineError};

/// Tracks whether buffers are registered with an io_uring instance.
pub struct RingRegistration {
    registered: bool,
}

impl RingRegistration {
    pub fn new() -> Self {
        Self { registered: false }
    }

    /// Register all arenas as fixed buffers with the io_uring submitter.
    ///
    /// Each arena maps to a contiguous buf_index range. Arena 0 → buf_index 0, etc.
    pub fn register(
        &mut self,
        submitter: &io_uring::Submitter<'_>,
        arenas: &[Arena],
    ) -> Result<()> {
        let iovecs: Vec<libc::iovec> = arenas.iter().map(|a| a.as_iovec()).collect();
        // SAFETY: iovecs point to valid mmap regions owned by arenas.
        unsafe {
            submitter
                .register_buffers(&iovecs)
                .map_err(TurbineError::Registration)?;
        }
        self.registered = true;
        tracing::info!(count = arenas.len(), "registered io_uring fixed buffers");
        Ok(())
    }

    /// Unregister previously registered buffers.
    pub fn unregister(&mut self, submitter: &io_uring::Submitter<'_>) -> Result<()> {
        if self.registered {
            submitter
                .unregister_buffers()
                .map_err(TurbineError::Registration)?;
            self.registered = false;
            tracing::info!("unregistered io_uring fixed buffers");
        }
        Ok(())
    }

    pub fn is_registered(&self) -> bool {
        self.registered
    }

    /// Map an arena index to the io_uring buf_index.
    /// Since each arena is registered as one iovec entry, the mapping is 1:1.
    pub fn arena_to_buf_index(arena_idx: usize) -> u16 {
        assert!(arena_idx <= u16::MAX as usize, "arena index {arena_idx} exceeds u16::MAX");
        arena_idx as u16
    }
}

impl Default for RingRegistration {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_not_registered() {
        let reg = RingRegistration::new();
        assert!(!reg.is_registered());
    }

    #[test]
    fn default_trait_matches_new() {
        let reg = RingRegistration::default();
        assert!(!reg.is_registered());
    }

    #[test]
    fn arena_to_buf_index_mapping() {
        assert_eq!(RingRegistration::arena_to_buf_index(0), 0);
        assert_eq!(RingRegistration::arena_to_buf_index(1), 1);
        assert_eq!(RingRegistration::arena_to_buf_index(42), 42);
        assert_eq!(RingRegistration::arena_to_buf_index(255), 255);
    }

    #[test]
    fn is_registered_tracks_state() {
        // Without a real io_uring ring we can only test the initial state
        // and the arena_to_buf_index helper. The register/unregister methods
        // require a live Submitter, so they are covered by integration tests.
        let reg = RingRegistration::new();
        assert!(!reg.is_registered());
    }
}
