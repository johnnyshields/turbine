pub struct VecBaseline;

impl VecBaseline {
    pub fn lease(len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    pub fn release(_buf: Vec<u8>) {
        // drop
    }
}
