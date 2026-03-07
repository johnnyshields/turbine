pub mod competitors;

pub const SIZES: &[usize] = &[64, 512, 4096, 65536];

/// Compute arena size for a given buffer size.
/// Ensures at least 64 buffers fit, rounded up to page alignment.
pub fn arena_size_for(buf_size: usize) -> usize {
    let min = buf_size * 64;
    let aligned = (min + 4095) & !4095; // next multiple of 4096
    aligned.max(4096)
}
