use std::sync::Mutex;

/// Bounded, thread-safe byte ring. Audio written by the playback endpoint is
/// read back by the capture endpoint of the same virtual cable.
pub struct Ring {
    inner: Mutex<Inner>,
}

struct Inner {
    buf: Vec<u8>,
    read: usize,
    write: usize,
    size: usize,
    dropped: u64,
    underruns: u64,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Mutex::new(Inner {
                buf: vec![0u8; capacity],
                read: 0,
                write: 0,
                size: 0,
                dropped: 0,
                underruns: 0,
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.inner.lock().unwrap().buf.len()
    }

    pub fn available(&self) -> usize {
        self.inner.lock().unwrap().size
    }

    pub fn stats(&self) -> (usize, u64, u64) {
        let inner = self.inner.lock().unwrap();
        (inner.size, inner.dropped, inner.underruns)
    }

    /// Keep the newest audio. If the ring is full the oldest bytes are
    /// discarded so the USB worker never blocks.
    pub fn write(&self, p: &[u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let capacity = inner.buf.len();
        for byte in p {
            if inner.size == capacity {
                inner.read = (inner.read + 1) % capacity;
                inner.size -= 1;
                inner.dropped += 1;
            }
            let write = inner.write;
            inner.buf[write] = *byte;
            inner.write = (write + 1) % capacity;
            inner.size += 1;
        }
        p.len()
    }

    /// Read `p.len()` bytes; missing audio becomes digital silence.
    pub fn read_silence(&self, p: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let capacity = inner.buf.len();
        let mut read = 0usize;
        for out in p.iter_mut() {
            if inner.size == 0 {
                *out = 0;
                continue;
            }
            *out = inner.buf[inner.read];
            inner.read = (inner.read + 1) % capacity;
            inner.size -= 1;
            read += 1;
        }
        if read < p.len() {
            inner.underruns += (p.len() - read) as u64;
        }
        read
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.read = 0;
        inner.write = 0;
        inner.size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn loopback() {
        let ring = Ring::new(16);
        assert_eq!(ring.write(&[1, 2, 3]), 3);
        let mut out = [0u8; 3];
        assert_eq!(ring.read_silence(&mut out), 3);
        assert_eq!(out, [1, 2, 3]);
    }

    #[test]
    fn drops_oldest_when_full() {
        let ring = Ring::new(4);
        ring.write(&[1, 2, 3, 4, 5, 6]);
        let mut out = [0u8; 4];
        ring.read_silence(&mut out);
        assert_eq!(out, [3, 4, 5, 6]);
        let (_, dropped, _) = ring.stats();
        assert_eq!(dropped, 2);
    }

    #[test]
    fn missing_audio_is_silence() {
        let ring = Ring::new(8);
        ring.write(&[9]);
        let mut out = [0u8; 3];
        assert_eq!(ring.read_silence(&mut out), 1);
        assert_eq!(out, [9, 0, 0]);
        let (_, _, underruns) = ring.stats();
        assert_eq!(underruns, 2);
    }
}
