pub use turbine_core::buffer::leased::LeasedBuffer;
pub use turbine_core::buffer::pinned::PinnedWrite;
pub use turbine_core::buffer::pool::IouringBufferPool;
pub use turbine_core::config::PoolConfig;
pub use turbine_core::epoch::arena::{Arena, ArenaState};
pub use turbine_core::epoch::clock::EpochClock;
pub use turbine_core::error::{Result, TurbineError};
pub use turbine_core::gc::{BufferPinHook, EpochObserver, NoopHooks};
pub use turbine_core::ring::registration::RingRegistration;
pub use turbine_core::transfer::handle::{ReturnedBuffer, SendableBuffer, TransferHandle};

/// Convenience re-exports.
pub mod prelude {
    pub use crate::*;
}
