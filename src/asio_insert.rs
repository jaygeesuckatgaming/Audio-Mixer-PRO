//! Mixer side of the external ASIO insert.
//!
//! The mixer is the clock master: once per block it writes each assigned strip's
//! audio (pre- or post-FX) into the shared ring, and substitutes whatever the
//! ASIO host returns. A return is inherently one or two blocks late — that is
//! the price of an external insert — and nothing blocks: a missing return leaves
//! the dry (or locally processed) audio in place.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use asio_insert::section::{Shared, BLOCKS, BLOCK_FRAMES, PAIRS};

use crate::audio::MAX_STRIPS;

/// Per-strip insert assignment: -1 off, otherwise the stereo pair it uses.
pub struct InsertMap {
    pub slot: [AtomicI32; MAX_STRIPS],
    /// false = pre-FX (before our own plug-in chain), true = post-FX.
    pub post: [AtomicBool; MAX_STRIPS],
}

impl InsertMap {
    pub fn new() -> Self {
        Self {
            slot: std::array::from_fn(|_| AtomicI32::new(-1)),
            post: std::array::from_fn(|_| AtomicBool::new(false)),
        }
    }

    pub fn set(&self, strip: usize, slot: Option<usize>, post: bool) {
        if strip >= MAX_STRIPS {
            return;
        }
        let value = match slot {
            Some(pair) if pair < PAIRS => pair as i32,
            _ => -1,
        };
        self.slot[strip].store(value, Ordering::Relaxed);
        self.post[strip].store(post, Ordering::Relaxed);
    }

    pub fn get(&self, strip: usize) -> Option<(usize, bool)> {
        if strip >= MAX_STRIPS {
            return None;
        }
        let slot = self.slot[strip].load(Ordering::Relaxed);
        if slot < 0 {
            None
        } else {
            Some((slot as usize, self.post[strip].load(Ordering::Relaxed)))
        }
    }

    pub fn active(&self) -> bool {
        self.slot
            .iter()
            .any(|slot| slot.load(Ordering::Relaxed) >= 0)
    }
}

impl Default for InsertMap {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedInsertMap = std::sync::Arc<InsertMap>;

pub fn insert_map() -> SharedInsertMap {
    std::sync::Arc::new(InsertMap::new())
}

/// The transport. Owned by the DSP, so all of it runs on the mixer thread.
pub struct Inserts {
    shared: Option<Shared>,
    seq: u64,
    next_return: [u64; PAIRS],
    /// Last returned block per pair, de-interleaved on demand.
    returns: [Vec<f32>; PAIRS],
    ready: [bool; PAIRS],
    pub misses: u64,
    pub blocks: u64,
}

impl Inserts {
    pub fn new(rate: u32) -> Self {
        let shared = Shared::open_or_create(rate).ok().map(|mut shared| {
            // The section may have been created earlier by the driver at a
            // different rate; the mixer is the master, so publish ours.
            shared.set_rate(rate);
            shared
        });
        // The ring's sequence numbers live in the section, not in this struct:
        // the driver may have been running (and returning blocks) before this
        // instance existed. Start from the next block this instance will send;
        // returns from an earlier mixer run are not our audio, and the driver
        // may not be running at all.
        let seq = shared.as_ref().map(|shared| shared.seq_in()).unwrap_or(0);
        let next_return = seq.saturating_add(1);
        Self {
            shared,
            seq,
            next_return: [next_return; PAIRS],
            returns: std::array::from_fn(|_| vec![0.0; BLOCK_FRAMES * 2]),
            ready: [false; PAIRS],
            misses: 0,
            blocks: 0,
        }
    }

    pub fn available(&self) -> bool {
        self.shared.is_some()
    }

    pub fn host_attached(&self) -> bool {
        self.shared
            .as_ref()
            .map(|shared| shared.host_attached())
            .unwrap_or(false)
    }

    pub fn set_rate(&mut self, rate: u32) {
        if let Some(shared) = &mut self.shared {
            shared.set_rate(rate);
        }
    }

    /// Collect the returns that have arrived since the last block. Call once per
    /// block, before the strips write their sends.
    pub fn pull(&mut self) {
        for pair in 0..PAIRS {
            self.ready[pair] = false;
            let Some(shared) = &self.shared else {
                continue;
            };
            let seq_out = shared.seq_out();
            let next = self.next_return[pair];
            // The driver keeps BLOCKS returned blocks. If it got further ahead
            // than that while this thread was stalled, the block we still owed
            // has been overwritten (and the next driver callback would overwrite
            // the one after it). Take the newest block instead of reading a slot
            // that is being written.
            if seq_out >= next.saturating_add((BLOCKS - 1) as u64) {
                self.misses += seq_out.saturating_sub(next);
                let source = shared.output_block(pair, seq_out);
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        source,
                        self.returns[pair].as_mut_ptr(),
                        BLOCK_FRAMES * 2,
                    );
                }
                self.next_return[pair] = seq_out.saturating_add(1);
                self.ready[pair] = true;
            } else if seq_out >= next {
                let source = shared.output_block(pair, next);
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        source,
                        self.returns[pair].as_mut_ptr(),
                        BLOCK_FRAMES * 2,
                    );
                }
                self.next_return[pair] = next + 1;
                self.ready[pair] = true;
            } else if self.seq > 0 {
                // The mixer has published, but this block has not come back yet.
                self.misses += 1;
            }
        }
    }

    pub fn ready(&self, pair: usize) -> bool {
        self.ready[pair.min(PAIRS - 1)]
    }

    /// Copy the returned block into the strip's planar buffers.
    pub fn take_return_planar(
        &self,
        pair: usize,
        left: &mut [f32],
        right: &mut [f32],
        frames: usize,
    ) {
        let pair = pair.min(PAIRS - 1);
        let frames = frames.min(BLOCK_FRAMES);
        let returned = &self.returns[pair];
        for frame in 0..frames {
            left[frame] = returned[frame * 2];
            right[frame] = returned[frame * 2 + 1];
        }
    }

    /// Write one block of planar stereo audio as this block's send.
    pub fn send_planar(&mut self, pair: usize, left: &[f32], right: &[f32], frames: usize) {
        let Some(shared) = &self.shared else {
            return;
        };
        if pair >= PAIRS {
            return;
        }
        let frames = frames.min(BLOCK_FRAMES);
        // The true next block number lives in the section. Using a local counter
        // here would write the wrong slot after a restart while the driver kept
        // running.
        let target = shared.input_block(pair, shared.seq_in() + 1);
        for frame in 0..frames {
            unsafe {
                *target.add(frame * 2) = left[frame];
                *target.add(frame * 2 + 1) = right[frame];
            }
        }
        for frame in frames..BLOCK_FRAMES {
            unsafe {
                *target.add(frame * 2) = 0.0;
                *target.add(frame * 2 + 1) = 0.0;
            }
        }
    }

    /// A pair with no strip on this block: keep the host's input silent.
    pub fn silence(&mut self, pair: usize) {
        let Some(shared) = &self.shared else {
            return;
        };
        if pair >= PAIRS {
            return;
        }
        let target = shared.input_block(pair, shared.seq_in() + 1);
        unsafe {
            std::ptr::write_bytes(target, 0u8, BLOCK_FRAMES * 2);
        }
    }

    /// Close the block: publish the sends so the driver can act on them.
    pub fn publish(&mut self) {
        let Some(shared) = &self.shared else {
            return;
        };
        self.seq = shared.seq_in() + 1;
        self.blocks += 1;
        shared.publish_block();
    }
}
