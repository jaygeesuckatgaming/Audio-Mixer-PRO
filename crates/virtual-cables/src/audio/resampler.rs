use std::sync::Mutex;

const MIN_CAPACITY_FRAMES: usize = 64;

/// Bridges playback and capture streams that may use different sample rates
/// and/or sample widths. Source frames are stored as f64; linear interpolation
/// is used for upsampling and a windowed box average for downsampling.
pub struct Resampler {
    inner: Mutex<Inner>,
}

struct Inner {
    channels: usize,
    buf: Vec<f64>,
    read: usize,
    size: usize,
    pos: f64,
    source_rate: u32,
    target_rate: u32,
    source_bytes: usize,
    target_bytes: usize,
    dropped: u64,
    underruns: u64,
}

impl Resampler {
    pub fn new(capacity_frames: usize, channels: usize) -> Self {
        let channels = channels.max(1);
        let capacity_frames = capacity_frames.max(MIN_CAPACITY_FRAMES);
        Self {
            inner: Mutex::new(Inner {
                channels,
                buf: vec![0.0; capacity_frames * channels],
                read: 0,
                size: 0,
                pos: 0.0,
                source_rate: 0,
                target_rate: 0,
                source_bytes: 0,
                target_bytes: 0,
                dropped: 0,
                underruns: 0,
            }),
        }
    }

    /// Select source/target rate and sample width. Returns true when the
    /// configuration changed (and the buffered audio was reset).
    pub fn configure(
        &self,
        source_rate: u32,
        target_rate: u32,
        source_bytes: usize,
        target_bytes: usize,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.source_rate == source_rate
            && inner.target_rate == target_rate
            && inner.source_bytes == source_bytes
            && inner.target_bytes == target_bytes
        {
            return false;
        }
        inner.source_rate = source_rate;
        inner.target_rate = target_rate;
        inner.source_bytes = source_bytes;
        inner.target_bytes = target_bytes;
        inner.reset();
        true
    }

    pub fn reset(&self) {
        self.inner.lock().unwrap().reset();
    }

    pub fn stats(&self) -> (usize, u64, u64) {
        let inner = self.inner.lock().unwrap();
        (inner.size, inner.dropped, inner.underruns)
    }

    /// Accept PCM bytes in the source format and append their frames.
    pub fn write(&self, p: &[u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let bytes_per_sample = inner.source_bytes;
        if !(2..=4).contains(&bytes_per_sample) {
            return 0;
        }
        let channels = inner.channels;
        let frame_bytes = bytes_per_sample * channels;
        let frames = p.len() / frame_bytes;
        let mut frame = vec![0.0f64; channels];
        for f in 0..frames {
            let base = f * frame_bytes;
            for ch in 0..channels {
                let off = base + ch * bytes_per_sample;
                frame[ch] = decode_sample(&p[off..off + bytes_per_sample], bytes_per_sample);
            }
            inner.push_frame(&frame);
        }
        frames * frame_bytes
    }

    /// Produce PCM bytes in the target format; missing source audio is silence.
    pub fn read(&self, p: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let bytes_per_sample = inner.target_bytes;
        if !(2..=4).contains(&bytes_per_sample) || inner.source_rate == 0 || inner.target_rate == 0
        {
            p.fill(0);
            return 0;
        }
        let channels = inner.channels;
        let frame_bytes = bytes_per_sample * channels;
        let frames = p.len() / frame_bytes;
        if frames == 0 {
            return 0;
        }

        let step = inner.source_rate as f64 / inner.target_rate as f64;
        let mut values = vec![0.0f64; channels];
        let mut produced = 0usize;

        for f in 0..frames {
            values.fill(0.0);
            if step < 1.0 {
                let i0 = inner.pos.floor() as usize;
                let frac = inner.pos - i0 as f64;
                if inner.size >= i0 + 2 {
                    for (ch, value) in values.iter_mut().enumerate() {
                        let a = inner.frame_at(i0, ch);
                        let b = inner.frame_at(i0 + 1, ch);
                        *value = a * (1.0 - frac) + b * frac;
                    }
                } else {
                    inner.underruns += 1;
                }
            } else {
                let start = inner.pos;
                let end = start + step;
                let i0 = start.floor() as usize;
                let i1 = end.ceil() as usize;
                if i1 <= inner.size {
                    for (ch, value) in values.iter_mut().enumerate() {
                        let mut sum = 0.0;
                        let mut weight = 0.0;
                        for i in i0..i1 {
                            let lo = start.max(i as f64);
                            let hi = end.min(i as f64 + 1.0);
                            if hi > lo {
                                let w = hi - lo;
                                sum += inner.frame_at(i, ch) * w;
                                weight += w;
                            }
                        }
                        if weight > 0.0 {
                            *value = sum / weight;
                        }
                    }
                } else {
                    inner.underruns += 1;
                }
            }

            let base = f * frame_bytes;
            for ch in 0..channels {
                let off = base + ch * bytes_per_sample;
                encode_sample(
                    &mut p[off..off + bytes_per_sample],
                    bytes_per_sample,
                    values[ch],
                );
            }
            produced += frame_bytes;
            inner.pos += step;
        }

        let mut consume = inner.pos.floor() as usize;
        if consume > 0 {
            if consume > inner.size {
                consume = inner.size;
                inner.pos = 0.0;
            } else {
                inner.pos -= consume as f64;
            }
            let capacity = inner.capacity_frames();
            inner.read = (inner.read + consume) % capacity;
            inner.size -= consume;
        }
        produced
    }
}

impl Inner {
    fn capacity_frames(&self) -> usize {
        self.buf.len() / self.channels
    }

    fn reset(&mut self) {
        self.read = 0;
        self.size = 0;
        self.pos = 0.0;
    }

    fn frame_at(&self, i: usize, channel: usize) -> f64 {
        let capacity = self.capacity_frames();
        let mut index = self.read + i;
        if index >= capacity {
            index -= capacity;
        }
        self.buf[index * self.channels + channel]
    }

    fn push_frame(&mut self, frame: &[f64]) {
        let capacity = self.capacity_frames();
        if self.size == capacity {
            self.read = (self.read + 1) % capacity;
            self.size -= 1;
            self.pos -= 1.0;
            if self.pos < 0.0 {
                self.pos = 0.0;
            }
            self.dropped += 1;
        }
        let write = (self.read + self.size) % capacity;
        self.buf[write * self.channels..(write + 1) * self.channels].copy_from_slice(frame);
        self.size += 1;
    }
}

fn decode_sample(b: &[u8], bytes_per_sample: usize) -> f64 {
    match bytes_per_sample {
        2 => i16::from_le_bytes([b[0], b[1]]) as f64 / 32768.0,
        3 => {
            let mut v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
            if v & 0x80_0000 != 0 {
                v |= !0xFF_FFFF;
            }
            v as f64 / 8_388_608.0
        }
        4 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64 / 2_147_483_648.0,
        _ => 0.0,
    }
}

fn encode_sample(b: &mut [u8], bytes_per_sample: usize, v: f64) {
    let v = v.clamp(-1.0, 1.0);
    match bytes_per_sample {
        2 => b.copy_from_slice(&((v * 32767.0).round() as i16).to_le_bytes()),
        3 => {
            let s = (v * 8_388_607.0).round() as i32;
            b[0] = s as u8;
            b[1] = (s >> 8) as u8;
            b[2] = (s >> 16) as u8;
        }
        4 => b.copy_from_slice(&((v * 2_147_483_647.0).round() as i32).to_le_bytes()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    fn int32_frame(v: i32) -> Vec<u8> {
        let mut b = Vec::with_capacity(8);
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
        b
    }

    fn int16_frame(v: i16) -> Vec<u8> {
        let mut b = Vec::with_capacity(4);
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
        b
    }

    #[test]
    fn upsample() {
        let r = Resampler::new(1024, 2);
        r.configure(48_000, 192_000, 4, 4);
        let mut src = int32_frame(0x4000_0000);
        src.extend_from_slice(&int32_frame(0x4000_0000));
        r.write(&src);
        let mut out = vec![0u8; 32];
        assert_eq!(r.read(&mut out), out.len());
        for chunk in out.chunks_exact(4) {
            let v = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            assert!((v as f64 / 2_147_483_648.0 - 0.5).abs() < 1e-5);
        }
    }

    #[test]
    fn downsample() {
        let r = Resampler::new(1024, 2);
        r.configure(192_000, 48_000, 4, 4);
        let values = [0x0CCC_CCCDi32, 0x1999_9999, 0x2666_6666, 0x3333_3333];
        let mut src = Vec::new();
        for v in values {
            src.extend_from_slice(&int32_frame(v));
        }
        r.write(&src);
        let mut out = vec![0u8; 8];
        assert_eq!(r.read(&mut out), out.len());
        let v = i32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        assert!((v as f64 / 2_147_483_648.0 - 0.25).abs() < 1e-4);
    }

    #[test]
    fn format_conversion() {
        let r = Resampler::new(1024, 2);
        r.configure(48_000, 48_000, 2, 4);
        let mut src = int16_frame(16384);
        src.extend_from_slice(&int16_frame(16384));
        r.write(&src);
        let mut out = vec![0u8; 16];
        assert_eq!(r.read(&mut out), out.len());
        for chunk in out.chunks_exact(4) {
            let v = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            assert!((v as f64 / 2_147_483_648.0 - 0.5).abs() < 1e-4);
        }
    }

    #[test]
    fn underrun_is_silence() {
        let r = Resampler::new(1024, 2);
        r.configure(48_000, 192_000, 4, 4);
        let mut out = [1u8; 8];
        r.read(&mut out);
        assert_eq!(out, [0u8; 8]);
        let (_, _, underruns) = r.stats();
        assert!(underruns > 0);
    }

    #[test]
    fn preserves_pitch() {
        const SRC: u32 = 48_000;
        const DST: u32 = 192_000;
        const FREQ: f64 = 1000.0;
        let r = Resampler::new(48_000, 2);
        r.configure(SRC, DST, 4, 4);
        let mut out = Vec::new();
        let mut written = 0usize;
        for _ in 0..12 {
            let mut buf = Vec::with_capacity(480 * 8);
            for i in 0..480 {
                let v = (0.5
                    * (2.0 * std::f64::consts::PI * FREQ * (written + i) as f64 / SRC as f64).sin()
                    * 2_147_483_647.0)
                    .round() as i32;
                buf.extend_from_slice(&int32_frame(v));
            }
            r.write(&buf);
            written += 480;
            let mut chunk = vec![0u8; 1728 * 8];
            r.read(&mut chunk);
            out.extend_from_slice(&chunk);
        }
        let mut crossings = 0i32;
        let mut prev = i32::from_le_bytes([out[0], out[1], out[2], out[3]]) as f64;
        for f in 1..out.len() / 8 {
            let cur =
                i32::from_le_bytes([out[f * 8], out[f * 8 + 1], out[f * 8 + 2], out[f * 8 + 3]])
                    as f64;
            if (prev < 0.0 && cur >= 0.0) || (prev >= 0.0 && cur < 0.0) {
                crossings += 1;
            }
            prev = cur;
        }
        let seconds = (out.len() / 8) as f64 / DST as f64;
        let expected = 2.0 * FREQ * seconds;
        assert!((crossings as f64 - expected).abs() <= 6.0);
    }
}
