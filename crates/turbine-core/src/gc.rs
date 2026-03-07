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
    #[inline]
    fn on_pin(&self, _epoch: u64, _buf_id: u32) {}
}

impl EpochObserver for NoopHooks {
    #[inline]
    fn on_rotate(&self, _retired: u64, _active: u64) {}
    #[inline]
    fn on_collect(&self, _epoch: u64) {}
}
