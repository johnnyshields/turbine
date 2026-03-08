use crate::ArenaIdx;

/// Hook called when buffers are pinned or released.
///
/// Implement this to track buffer lifecycle events for garbage collection,
/// metrics, or debugging.
pub trait BufferPinHook {
    fn on_pin(&self, epoch: u64, buf_id: u32);
}

/// Observer notified on epoch transitions.
///
/// Implement this to react to epoch rotation and arena collection events.
pub trait EpochObserver {
    /// Called when the clock rotates. `retired` is the epoch that just became
    /// read-only; `active` is the new writable epoch.
    fn on_rotate(&self, retired: u64, active: u64);

    /// Called when a retired epoch's arena is reclaimed.
    fn on_collect(&self, epoch: u64);

    /// Called when a new arena is allocated.
    fn on_arena_alloc(&self, _arena_idx: ArenaIdx) {}

    /// Called when an arena is freed (munmapped).
    fn on_arena_free(&self, _arena_idx: ArenaIdx) {}

    /// Called after a collect sweep with the number of arenas collected.
    fn on_collect_sweep(&self, _collected: usize) {}
}

/// No-op implementations of all hooks, for standalone use.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHooks;

impl BufferPinHook for NoopHooks {
    #[inline(always)]
    fn on_pin(&self, _epoch: u64, _buf_id: u32) {}
}

impl EpochObserver for NoopHooks {
    #[inline(always)]
    fn on_rotate(&self, _retired: u64, _active: u64) {}
    #[inline(always)]
    fn on_collect(&self, _epoch: u64) {}
    #[inline(always)]
    fn on_collect_sweep(&self, _collected: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_hooks_default() {
        let hooks = NoopHooks::default();
        // Just verify it can be created via Default
        let _ = format!("{hooks:?}");
    }

    #[test]
    fn noop_hooks_clone_copy() {
        let hooks = NoopHooks;
        let cloned = hooks.clone();
        let copied = hooks;
        let _ = (cloned, copied);
    }

    #[test]
    fn noop_buffer_pin_hook() {
        let hooks = NoopHooks;
        // Should not panic — it's a no-op
        hooks.on_pin(0, 0);
        hooks.on_pin(u64::MAX, u32::MAX);
    }

    #[test]
    fn noop_epoch_observer_on_rotate() {
        let hooks = NoopHooks;
        hooks.on_rotate(0, 1);
        hooks.on_rotate(u64::MAX, u64::MAX);
    }

    #[test]
    fn noop_epoch_observer_on_collect() {
        let hooks = NoopHooks;
        hooks.on_collect(0);
        hooks.on_collect(u64::MAX);
    }

    #[test]
    fn noop_epoch_observer_on_arena_alloc() {
        let hooks = NoopHooks;
        hooks.on_arena_alloc(ArenaIdx::new(0));
        hooks.on_arena_alloc(ArenaIdx::new(99));
    }

    #[test]
    fn noop_epoch_observer_on_arena_free() {
        let hooks = NoopHooks;
        hooks.on_arena_free(ArenaIdx::new(0));
        hooks.on_arena_free(ArenaIdx::new(99));
    }

    #[test]
    fn noop_epoch_observer_on_collect_sweep() {
        let hooks = NoopHooks;
        hooks.on_collect_sweep(0);
        hooks.on_collect_sweep(100);
    }

    /// Test that the default trait impls for EpochObserver work.
    struct MinimalObserver;
    impl EpochObserver for MinimalObserver {
        fn on_rotate(&self, _retired: u64, _active: u64) {}
        fn on_collect(&self, _epoch: u64) {}
        // on_arena_alloc, on_arena_free, on_collect_sweep use defaults
    }

    #[test]
    fn default_trait_impls_are_callable() {
        let obs = MinimalObserver;
        obs.on_arena_alloc(ArenaIdx::new(0));
        obs.on_arena_free(ArenaIdx::new(0));
        obs.on_collect_sweep(42);
    }
}
