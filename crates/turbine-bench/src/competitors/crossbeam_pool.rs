use crossbeam_epoch::{self as epoch, Atomic, Owned};
use std::sync::atomic::Ordering;

struct Node {
    buf: Vec<u8>,
    next: Atomic<Node>,
}

pub struct CrossbeamPool {
    head: Atomic<Node>,
}

impl CrossbeamPool {
    pub fn new(_buf_size: usize) -> Self {
        Self {
            head: Atomic::null(),
        }
    }

    pub fn lease(&self, len: usize) -> Vec<u8> {
        let guard = epoch::pin();
        let mut head = self.head.load(Ordering::Acquire, &guard);
        while !head.is_null() {
            let next = unsafe { head.deref() }.next.load(Ordering::Relaxed, &guard);
            match self.head.compare_exchange(
                head,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
                &guard,
            ) {
                Ok(_) => {
                    let node = unsafe { head.into_owned() };
                    let mut buf = node.into_box().buf;
                    if buf.len() < len {
                        buf.resize(len, 0);
                    }
                    return buf;
                }
                Err(e) => head = e.current,
            }
        }
        vec![0u8; len]
    }

    pub fn release(&self, buf: Vec<u8>) {
        let guard = epoch::pin();
        let node = Owned::new(Node {
            buf,
            next: Atomic::null(),
        });
        let mut node = node;
        loop {
            let head = self.head.load(Ordering::Relaxed, &guard);
            node.next.store(head, Ordering::Relaxed);
            match self.head.compare_exchange(
                head,
                node,
                Ordering::Release,
                Ordering::Relaxed,
                &guard,
            ) {
                Ok(_) => break,
                Err(e) => node = e.new,
            }
        }
    }
}

impl Drop for CrossbeamPool {
    fn drop(&mut self) {
        unsafe {
            let guard = epoch::unprotected();
            let mut current = self.head.load(Ordering::Relaxed, guard);
            while !current.is_null() {
                let next = current.deref().next.load(Ordering::Relaxed, guard);
                drop(current.into_owned());
                current = next;
            }
        }
    }
}
