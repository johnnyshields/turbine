# Turbine

Epoch-based buffer rotation for io_uring.

Turbine pre-allocates buffer arenas tied to scheduler epochs. Every N microseconds
the scheduler rotates to a new arena. In-flight I/O from the previous epoch completes
into the old arena (now read-only), and the current epoch's arena uses append-only
bump allocation — no contention, no locking.

## Crates

- **turbine-core** — arenas, epochs, io_uring registration, cross-thread transfer
- **turbine** — facade re-exporting the public API

## Quick Start

```rust
use turbine::prelude::*;

let config = PoolConfig::default();
let mut pool = IouringBufferPool::new(config, NoopHooks)?;

let mut buf = pool.lease(4096).expect("arena has space");
buf.as_mut_slice()[..5].copy_from_slice(b"hello");

pool.rotate();
pool.try_collect(0);
```
