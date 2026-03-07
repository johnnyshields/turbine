use std::sync::Arc;

use sharded_slab::Slab;

pub struct ShardedSlabPool {
    inner: Arc<Slab<Vec<u8>>>,
}

impl ShardedSlabPool {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Slab::new()),
        }
    }

    pub fn lease(&self, len: usize) -> usize {
        self.inner.insert(vec![0u8; len]).expect("slab full")
    }

    pub fn release(&self, key: usize) {
        self.inner.remove(key);
    }

    pub fn handle(&self) -> ShardedSlabHandle {
        ShardedSlabHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for ShardedSlabPool {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ShardedSlabHandle {
    inner: Arc<Slab<Vec<u8>>>,
}

impl ShardedSlabHandle {
    pub fn release(&self, key: usize) {
        self.inner.remove(key);
    }
}
