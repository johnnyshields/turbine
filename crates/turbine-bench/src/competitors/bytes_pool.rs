use bytes::BytesMut;

pub struct BytesPool {
    free_list: Vec<BytesMut>,
    buf_size: usize,
}

impl BytesPool {
    pub fn new(count: usize, buf_size: usize) -> Self {
        let mut free_list = Vec::with_capacity(count);
        for _ in 0..count {
            free_list.push(BytesMut::zeroed(buf_size));
        }
        Self { free_list, buf_size }
    }

    pub fn lease(&mut self) -> BytesMut {
        self.free_list.pop().unwrap_or_else(|| BytesMut::zeroed(self.buf_size))
    }

    pub fn release(&mut self, buf: BytesMut) {
        self.free_list.push(buf);
    }
}
