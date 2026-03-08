# Future Work

## TLA+ Modeling

Add a TLA+ specification for Turbine's core epoch lifecycle protocol --
specifically the interaction between `rotate()`, `collect()`, and the
split-counter lease mechanism (`Cell<usize>` + `AtomicUsize`).

Key properties to verify:

- **Safety:** An arena is never collected while outstanding leases > 0
  (no use-after-free).
- **Safety:** `rotate()` never leaves the pool without a writable arena
  (the "secure next before retiring current" invariant).
- **Liveness:** Every retired arena is eventually collectible, assuming
  all leases are eventually dropped (no permanent starvation).
- **No ABA:** A recycled arena cannot be confused with its previous
  incarnation by a stale `SendableBuffer` (the split counter reset
  happens only after outstanding == 0).

The model should cover the cross-thread case: one pool thread performing
`rotate()` + `collect()`, and N remote threads holding `SendableBuffer`s
that drop at arbitrary times via `fetch_add(1, Release)`.
