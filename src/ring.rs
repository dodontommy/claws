use std::collections::VecDeque;

/// Bounded byte ring with a monotonic write counter.
///
/// `total_written` is the count of bytes ever appended; subtracting `inner.len()`
/// gives the seq number of the oldest still-buffered byte. A reader holds a
/// `seq` from a prior `read_from` call and uses it to ask for "everything
/// since then." If their seq is older than what's still buffered, they jump
/// forward to the oldest available — they lose data, but never re-read.
pub struct RingBuffer {
    inner: VecDeque<u8>,
    capacity: usize,
    total_written: u64,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            total_written: 0,
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        let n = data.len();
        let cap = self.capacity;
        if n >= cap {
            self.inner.clear();
            self.inner.extend(&data[n - cap..]);
        } else {
            let overflow = (self.inner.len() + n).saturating_sub(cap);
            for _ in 0..overflow {
                self.inner.pop_front();
            }
            self.inner.extend(data);
        }
        self.total_written += n as u64;
    }

    pub fn read_from(&self, seq: u64) -> (Vec<u8>, u64) {
        let oldest = self.total_written.saturating_sub(self.inner.len() as u64);
        let start = seq.max(oldest);
        if start >= self.total_written {
            return (Vec::new(), self.total_written);
        }
        let skip = (start - oldest) as usize;
        let bytes: Vec<u8> = self.inner.iter().copied().skip(skip).collect();
        (bytes, self.total_written)
    }

    pub fn total_written(&self) -> u64 {
        self.total_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_under_capacity() {
        let mut r = RingBuffer::new(8);
        r.append(b"hello");
        let (bytes, seq) = r.read_from(0);
        assert_eq!(bytes, b"hello");
        assert_eq!(seq, 5);
    }

    #[test]
    fn append_wraps() {
        let mut r = RingBuffer::new(4);
        r.append(b"abcdefg");
        let (bytes, seq) = r.read_from(0);
        assert_eq!(bytes, b"defg");
        assert_eq!(seq, 7);
    }

    #[test]
    fn read_from_advances() {
        let mut r = RingBuffer::new(8);
        r.append(b"hello");
        let (b1, s1) = r.read_from(0);
        assert_eq!(b1, b"hello");
        r.append(b" world");
        let (b2, s2) = r.read_from(s1);
        assert_eq!(b2, b" world");
        assert_eq!(s2, 11);
    }

    #[test]
    fn ahead_of_writes_returns_empty() {
        let mut r = RingBuffer::new(8);
        r.append(b"hi");
        let (bytes, seq) = r.read_from(99);
        assert_eq!(bytes, b"");
        assert_eq!(seq, 2);
    }
}
