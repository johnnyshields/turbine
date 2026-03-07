use std::sync::{Arc, Mutex};

use slab::Slab;

pub struct SlabPool {
    inner: Arc<Mutex<Slab<Vec<u8>>>>,
}

impl SlabPool {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Slab::new())),
        }
    }

    pub fn lease(&self, len: usize) -> usize {
        let mut slab = self.inner.lock().unwrap();
        slab.insert(vec![0u8; len])
    }

    pub fn release(&self, key: usize) {
        let mut slab = self.inner.lock().unwrap();
        slab.remove(key);
    }

    pub fn handle(&self) -> SlabHandle {
        SlabHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for SlabPool {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SlabHandle {
    inner: Arc<Mutex<Slab<Vec<u8>>>>,
}

impl SlabHandle {
    pub fn release(&self, key: usize) {
        let mut slab = self.inner.lock().unwrap();
        slab.remove(key);
    }
}
