//! The shared memory the mixer and the ASIO driver meet in.
//!
//! One fixed-size section per install (not per slot): two stereo pairs of
//! 512 frames in each direction, plus a small header with sequence counters.
//! Whoever starts first creates it; the other opens the same object, so the
//! driver presents a silent device when the mixer is not running and the mixer
//! can write into a section nobody is reading.

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
pub const VERSION: u32 = 1;
pub const BLOCK_FRAMES: usize = 512;
pub const PAIRS: usize = 2;
const HEADER_BYTES: usize = 64;
const PLANE_BYTES: usize = PAIRS * BLOCK_FRAMES * 2 * 4;
pub const SECTION_BYTES: usize = HEADER_BYTES + PLANE_BYTES * 2;

const SECTION_NAME: &str = "Local\\AudioMixerProInsert_v1";
const BLOCK_EVENT: &str = "Local\\AudioMixerProInsert_v1_block";
const READY_EVENT: &str = "Local\\AudioMixerProInsert_v1_ready";

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
            let section = wide(SECTION_NAME);
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
            let block_event = CreateEventW(std::ptr::null(), 0, 0, wide(BLOCK_EVENT).as_ptr());
            let ready_event = CreateEventW(std::ptr::null(), 0, 0, wide(READY_EVENT).as_ptr());
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

    pub fn publish_return(&self) {
        let header = self.header();
        header.blocks.fetch_add(1, Ordering::Relaxed);
        header.seq_out.fetch_add(1, Ordering::Release);
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
    pub fn input(&self, pair: usize) -> *mut f32 {
        let pair = pair.min(PAIRS - 1);
        // Byte offset: `base` is *mut u8, and one stereo plane is
        // BLOCK_FRAMES * 2 samples of f32.
        let plane = BLOCK_FRAMES * 2 * std::mem::size_of::<f32>();
        unsafe { self.base.add(HEADER_BYTES + pair * plane) as *mut f32 }
    }

    /// Interleaved stereo output plane for a pair (the host writes here).
    pub fn output(&self, pair: usize) -> *mut f32 {
        let pair = pair.min(PAIRS - 1);
        let plane = BLOCK_FRAMES * 2 * std::mem::size_of::<f32>();
        unsafe { self.base.add(HEADER_BYTES + PLANE_BYTES + pair * plane) as *mut f32 }
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
