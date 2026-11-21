//! The shared memory the mixer and the ASIO driver meet in.
//!
//! One fixed-size section per install (not per slot): two stereo pairs, with a
//! ring of four 512-frame blocks in each direction, plus a header with sequence
//! counters. The ring is what makes it real-time safe: the mixer never waits for
//! a return, it writes the next block into the next slot while the driver is
//! still handing the previous one to the host. Whoever starts first creates the
//! section; the other opens the same object, so the driver presents a silent
//! device when the mixer is not running and the mixer can write into a section
//! nobody is reading.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

pub const MAGIC: [u8; 4] = *b"AMPI";
pub const VERSION: u32 = 2;
pub const BLOCK_FRAMES: usize = 512;
pub const PAIRS: usize = 2;
/// Blocks in each direction. Four gives the driver room to be a block late
/// without the mixer overwriting audio it has not copied yet.
pub const BLOCKS: usize = 4;
const HEADER_BYTES: usize = 64;
const BLOCK_BYTES: usize = BLOCK_FRAMES * 2 * 4;
const DIRECTION_BYTES: usize = PAIRS * BLOCKS * BLOCK_BYTES;
pub const SECTION_BYTES: usize = HEADER_BYTES + DIRECTION_BYTES * 2;

const SECTION_BASE: &str = "Local\\AudioMixerProInsert_v2";

/// The probe sets `AMI_SECTION_SUFFIX` so it can run while the mixer has a live
/// insert without the two publishing into the same ring.
fn names() -> (String, String, String) {
    let suffix = std::env::var("AMI_SECTION_SUFFIX").unwrap_or_default();
    (
        format!("{SECTION_BASE}{suffix}"),
        format!("{SECTION_BASE}_block{suffix}"),
        format!("{SECTION_BASE}_ready{suffix}"),
    )
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct Shared {
    mapping: HANDLE,
    base: *mut u8,
    block_event: HANDLE,
    ready_event: HANDLE,
}

// The pointers belong to a file mapping the process owns; both sides lock via
// the events, and the atomics are only used through plain loads/stores.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// Create (or open) the section and its two events. The mixer and the
    /// driver both call this; the first one initialises the header.
    pub fn open_or_create(rate: u32) -> Result<Self, String> {
        unsafe {
            let (section_name, block_name, ready_name) = names();
            let section = wide(&section_name);
            let mapping = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                SECTION_BYTES as u32,
                section.as_ptr(),
            );
            if mapping.is_null() {
                return Err("could not create the insert section".to_string());
            }
            let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, SECTION_BYTES);
            let base = view.Value as *mut u8;
            if base.is_null() {
                let _ = CloseHandle(mapping);
                return Err("could not map the insert section".to_string());
            }
            let block_event = CreateEventW(std::ptr::null(), 0, 0, wide(&block_name).as_ptr());
            let ready_event = CreateEventW(std::ptr::null(), 0, 0, wide(&ready_name).as_ptr());
            if block_event.is_null() || ready_event.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: base as *mut c_void,
                });
                let _ = CloseHandle(mapping);
                return Err("could not create the insert events".to_string());
            }
            let mut shared = Self {
                mapping,
                base,
                block_event,
                ready_event,
            };
            // A brand-new mapping is zero-filled, so the magic tells us whether
            // anybody initialised it; no reliance on GetLastError.
            let magic = shared.header().magic;
            if magic == MAGIC {
                if shared.header().version != VERSION {
                    return Err(format!(
                        "insert section version {} does not match {VERSION}",
                        shared.header().version
                    ));
                }
            } else if magic == [0, 0, 0, 0] {
                let header = shared.header_mut();
                header.magic.copy_from_slice(&MAGIC);
                header.version = VERSION;
                header.rate = rate.max(1);
                header.block = BLOCK_FRAMES as u32;
                header.pairs = PAIRS as u32;
            } else {
                return Err("the insert section is not ours".to_string());
            }
            Ok(shared)
        }
    }

    fn header(&self) -> &Header {
        unsafe { &*(self.base as *const Header) }
    }

    fn header_mut(&mut self) -> &mut Header {
        unsafe { &mut *(self.base as *mut Header) }
    }

    /// Sample rate published by whoever created the section (the mixer).
    pub fn rate(&self) -> u32 {
        self.header().rate.max(1)
    }

    /// The mixer may update the rate after a settings change.
    pub fn set_rate(&mut self, rate: u32) {
        self.header_mut().rate = rate.max(1);
    }

    pub fn seq_in(&self) -> u64 {
        self.header().seq_in.load(Ordering::Acquire)
    }

    pub fn seq_out(&self) -> u64 {
        self.header().seq_out.load(Ordering::Acquire)
    }

    pub fn publish_block(&self) {
        let header = self.header();
        header.seq_in.fetch_add(1, Ordering::Release);
        unsafe {
            SetEvent(self.block_event);
        }
    }

    /// Record how many blocks the driver has returned. Stored rather than
    /// incremented so a driver that starts late (or one that processed several
    /// published blocks in one wake-up) publishes an exact count.
    pub fn set_seq_out(&self, seq: u64) {
        let header = self.header();
        header.seq_out.store(seq, Ordering::Release);
        header.blocks.store(seq, Ordering::Relaxed);
        unsafe {
            SetEvent(self.ready_event);
        }
    }

    pub fn note_missed(&self) {
        self.header().missed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn blocks(&self) -> u64 {
        self.header().blocks.load(Ordering::Relaxed)
    }

    pub fn missed(&self) -> u64 {
        self.header().missed.load(Ordering::Relaxed)
    }

    pub fn set_host_attached(&self, attached: bool) {
        self.header()
            .host
            .store(u32::from(attached), Ordering::Relaxed);
    }

    pub fn host_attached(&self) -> bool {
        self.header().host.load(Ordering::Relaxed) != 0
    }

    /// Interleaved stereo input plane for a pair (the mixer writes here).
    /// Interleaved stereo input block for (`pair`, `seq`); the mixer writes here.
    /// The slot is `seq % BLOCKS`, so a writer never touches the block the
    /// driver is still handing to the host (up to BLOCKS blocks of slack).
    pub fn input_block(&self, pair: usize, seq: u64) -> *mut f32 {
        let pair = pair.min(PAIRS - 1);
        let slot = (seq as usize) % BLOCKS;
        let offset = HEADER_BYTES + (pair * BLOCKS + slot) * BLOCK_BYTES;
        unsafe { self.base.add(offset) as *mut f32 }
    }

    /// Interleaved stereo output block for (`pair`, `seq`); the host writes here.
    pub fn output_block(&self, pair: usize, seq: u64) -> *mut f32 {
        let pair = pair.min(PAIRS - 1);
        let slot = (seq as usize) % BLOCKS;
        let offset = HEADER_BYTES + DIRECTION_BYTES + (pair * BLOCKS + slot) * BLOCK_BYTES;
        unsafe { self.base.add(offset) as *mut f32 }
    }

    pub fn wait_block(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.block_event, timeout_ms) == WAIT_OBJECT_0 }
    }

    pub fn wait_return(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.ready_event, timeout_ms) == WAIT_OBJECT_0 }
    }

    pub fn is_timeout(code: u32) -> bool {
        code == WAIT_TIMEOUT
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base as *mut c_void,
            });
            let _ = CloseHandle(self.block_event);
            let _ = CloseHandle(self.ready_event);
            let _ = CloseHandle(self.mapping);
        }
    }
}

/// Fixed header at offset zero. Written once by the creator; the sequence
/// counters are the only fields that move afterwards.
#[repr(C)]
struct Header {
    magic: [u8; 4],
    version: u32,
    rate: u32,
    block: u32,
    pairs: u32,
    _pad: u32,
    seq_in: AtomicU64,
    seq_out: AtomicU64,
    blocks: AtomicU64,
    missed: AtomicU64,
    host: AtomicU32,
    mixer: AtomicU32,
}

const _: () = assert!(std::mem::size_of::<Header>() == HEADER_BYTES);
