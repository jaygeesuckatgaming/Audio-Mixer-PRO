//! The ASIO driver object.
//!
//! The mixer is the clock master: it publishes a block and signals an event, the
//! pump thread here copies that block into the host's input pointers, calls the
//! host's `bufferSwitch`, then copies the host's outputs back. One block each
//! way, no resampling, and the DAW follows our timer.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::abi::*;
use crate::section::{Shared, BLOCK_FRAMES, PAIRS};

/// ASIO buffers are planar: one buffer per channel, so two stereo pairs are
/// four input and four output channels.
const CHANNELS: usize = PAIRS * 2;

/// Frames per host callback. The section block is 512; JUCE (Waveform) runs
/// its graph in 256-frame blocks, so the host is advertised and called at this
/// size, twice per section block.
const HOST_BLOCK_FRAMES: usize = BLOCK_FRAMES;

struct DriverState {
    shared: Option<Shared>,
    callbacks: ASIOCallbacks,
    in_ptrs: [*mut f32; CHANNELS],
    out_ptrs: [*mut f32; CHANNELS],
    /// Second buffer slot of each channel. A host that runs half-size blocks
    /// writes the second half here in the same callback (JUCE does this), so a
    /// 512-frame section block is two planes of 256.
    in_ptrs2: [*mut f32; CHANNELS],
    out_ptrs2: [*mut f32; CHANNELS],
    split_buffers: AtomicBool,
    /// Buffers this driver allocated because the host left `ASIOBufferInfo.buffers`
    /// empty. JUCE (Waveform's audio layer) does exactly that: it passes the
    /// array with null pointers and expects `createBuffers` to supply them.
    owned: Vec<Vec<f32>>,
    running: AtomicBool,
    stop: AtomicBool,
    sample_pos: AtomicU64,
    /// Block the host is currently working on. `outputReady` arrives after
    /// `bufferSwitch` on hosts whose graph runs asynchronously (JUCE does), and
    /// only then are the output buffers valid.
    current_seq: AtomicU64,
    /// Block the host last reported `outputReady` for. The block is only
    /// published to the mixer once this matches, so the mixer never reads a
    /// block the host is still writing.
    out_ready_seq: AtomicU64,
    /// Block the pump is waiting on. A signal for any other block is stale and
    /// must not be copied, or it would land in the wrong output slot.
    waiting: AtomicU64,
    /// 0 = not known yet, 1 = host uses outputReady, 2 = host fills the buffers
    /// inside bufferSwitch.
    ready_host: AtomicU8,
    /// Consecutive blocks with no outputReady, used to decide `ready_host`.
    no_signal: AtomicU8,
    /// Blocks a known outputReady host failed to finish in time (published as
    /// silence rather than as half-written audio).
    late_blocks: AtomicU64,
    /// Block the host signalled as written but which has not been copied yet.
    /// The copy happens one callback later, so a host whose engine is still
    /// filling the buffers cannot be read half-written.
    pending_copy: AtomicU64,
    /// outputReady signals seen for the block being dispatched (one per host
    /// call: two when the host runs half-size blocks).
    signals: AtomicU8,
    /// Peaks since the last one-second trace, to show which side is silent.
    in_peak: AtomicU32,
    out_peak: AtomicU32,
    ready_calls: AtomicU64,
    rate: f64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DriverState {
    fn new() -> Self {
        Self {
            shared: None,
            callbacks: ASIOCallbacks {
                buffer_switch: None,
                sample_rate_did_change: None,
                asio_message: None,
                buffer_switch_time_info: None,
            },
            in_ptrs: [std::ptr::null_mut(); CHANNELS],
            in_ptrs2: [std::ptr::null_mut(); CHANNELS],
            out_ptrs2: [std::ptr::null_mut(); CHANNELS],
            split_buffers: AtomicBool::new(false),
            out_ptrs: [std::ptr::null_mut(); CHANNELS],
            owned: Vec::new(),
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            sample_pos: AtomicU64::new(0),
            current_seq: AtomicU64::new(0),
            out_ready_seq: AtomicU64::new(u64::MAX),
            waiting: AtomicU64::new(u64::MAX),
            ready_host: AtomicU8::new(0),
            no_signal: AtomicU8::new(0),
            late_blocks: AtomicU64::new(0),
            pending_copy: AtomicU64::new(u64::MAX),
            signals: AtomicU8::new(0),
            in_peak: AtomicU32::new(0),
            out_peak: AtomicU32::new(0),
            ready_calls: AtomicU64::new(0),
            rate: 48_000.0,
            thread: None,
        }
    }
}

#[repr(C)]
pub struct AsioObject {
    /// Must stay first: the host talks to this pointer as `IASIO*`.
    vtbl: *const IASIOVtbl,
    state: *mut DriverState,
    refs: AtomicU32,
}

#[repr(C)]
struct ClassFactory {
    vtbl: *const IClassFactoryVtbl,
    refs: AtomicU32,
}

// The vtable is a static; the only mutable state is the atomic refcount.
unsafe impl Sync for ClassFactory {}
unsafe impl Send for ClassFactory {}

static FACTORY_REFS: AtomicU32 = AtomicU32::new(0);

unsafe fn object<'a>(this: *mut c_void) -> &'a mut AsioObject {
    &mut *(this as *mut AsioObject)
}

unsafe fn state<'a>(this: *mut c_void) -> &'a mut DriverState {
    &mut *object(this).state
}

// ---------------------------------------------------------------------------
// Object lifetime
// ---------------------------------------------------------------------------

unsafe extern "system" fn asio_query_interface(
    this: *mut c_void,
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    if ppv.is_null() {
        return E_NOINTERFACE;
    }
    let obj = object(this);
    obj.refs.fetch_add(1, Ordering::SeqCst);
    *ppv = this;
    S_OK
}

unsafe extern "system" fn asio_add_ref(this: *mut c_void) -> u32 {
    object(this).refs.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn asio_release(this: *mut c_void) -> u32 {
    let obj = object(this);
    let refs = obj.refs.fetch_sub(1, Ordering::SeqCst);
    if refs == 1 {
        let mut state = Box::from_raw(obj.state);
        state.stop.store(true, Ordering::SeqCst);
        if let Some(shared) = &state.shared {
            shared.publish_block();
        }
        if let Some(thread) = state.thread.take() {
            let _ = thread.join();
        }
        drop(state);
        drop(Box::from_raw(obj));
        0
    } else {
        refs - 1
    }
}

// ---------------------------------------------------------------------------
// IASIO
// ---------------------------------------------------------------------------

/// Control-plane trace, appended to
/// `%LOCALAPPDATA%\Audio Mixer PRO\asio\driver.log`. A host only calls these
/// when it opens the device, so this stays tiny; it exists because "device
/// didn't start correctly" in a DAW says nothing about which call it disliked.
fn trace(line: &str) {
    use std::io::Write;
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let dir = std::path::PathBuf::from(base)
        .join("Audio Mixer PRO")
        .join("asio");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("driver.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 256 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{now}] {line}");
    }
}

unsafe extern "system" fn asio_init(this: *mut c_void, _sys_handle: *mut c_void) -> ASIOBool {
    let state = state(this);
    match Shared::open_or_create(48_000) {
        Ok(shared) => {
            state.rate = shared.rate() as f64;
            shared.set_host_attached(true);
            state.shared = Some(shared);
            trace(&format!("init -> TRUE ({} Hz)", state.rate));
            ASIO_TRUE
        }
        Err(err) => {
            trace(&format!("init -> FALSE: {err}"));
            ASIO_FALSE
        }
    }
}

unsafe extern "system" fn asio_get_driver_name(this: *mut c_void, name: *mut i8) {
    if name.is_null() {
        return;
    }
    let _ = this;
    let bytes = DRIVER_NAME.as_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        *name.add(i) = *byte as i8;
    }
    *name.add(bytes.len()) = 0;
}

unsafe extern "system" fn asio_get_driver_version(_this: *mut c_void) -> i32 {
    trace("get_driver_version -> 1");
    1
}

unsafe extern "system" fn asio_get_error_message(_this: *mut c_void, message: *mut i8) {
    if message.is_null() {
        return;
    }
    for (i, byte) in b"OK\0".iter().enumerate() {
        *message.add(i) = *byte as i8;
    }
}

unsafe extern "system" fn asio_start(this: *mut c_void) -> ASIOError {
    let state = state(this);
    if state.shared.is_none() {
        trace("start -> NotPresent (init has not run)");
        return ASE_NotPresent;
    }
    if state.callbacks.buffer_switch.is_none() && state.callbacks.buffer_switch_time_info.is_none()
    {
        trace("start -> NotPresent (no callbacks)");
        return ASE_NotPresent;
    }
    if state.running.swap(true, Ordering::SeqCst) {
        trace("start -> OK (already running)");
        return ASE_OK;
    }
    state.stop.store(false, Ordering::SeqCst);
    let ptr = StatePtr(state as *mut DriverState);
    let handle = std::thread::Builder::new()
        .name("asio-insert-pump".to_string())
        .spawn(move || pump(ptr));
    match handle {
        Ok(thread) => {
            state.thread = Some(thread);
            trace("start -> OK (pump running)");
            ASE_OK
        }
        Err(err) => {
            state.running.store(false, Ordering::SeqCst);
            trace(&format!("start -> HWMalfunction (no thread: {err})"));
            ASE_HWMalfunction
        }
    }
}

unsafe extern "system" fn asio_stop(this: *mut c_void) -> ASIOError {
    let state = state(this);
    if !state.running.swap(false, Ordering::SeqCst) {
        return ASE_OK;
    }
    state.stop.store(true, Ordering::SeqCst);
    if let Some(shared) = state.shared.as_ref() {
        shared.publish_block();
    }
    if let Some(thread) = state.thread.take() {
        let _ = thread.join();
    }
    ASE_OK
}

unsafe extern "system" fn asio_get_channels(
    _this: *mut c_void,
    inputs: *mut i32,
    outputs: *mut i32,
) -> ASIOError {
    if !inputs.is_null() {
        *inputs = CHANNELS as i32;
    }
    if !outputs.is_null() {
        *outputs = CHANNELS as i32;
    }
    trace(&format!("get_channels -> {CHANNELS}/{CHANNELS}"));
    ASE_OK
}

unsafe extern "system" fn asio_get_latencies(
    _this: *mut c_void,
    input: *mut i32,
    output: *mut i32,
) -> ASIOError {
    if !input.is_null() {
        *input = HOST_BLOCK_FRAMES as i32;
    }
    if !output.is_null() {
        *output = HOST_BLOCK_FRAMES as i32;
    }
    ASE_OK
}

unsafe extern "system" fn asio_get_buffer_size(
    _this: *mut c_void,
    min: *mut i32,
    max: *mut i32,
    preferred: *mut i32,
    granularity: *mut i32,
) -> ASIOError {
    for (target, value) in [
        (min, HOST_BLOCK_FRAMES as i32),
        (max, HOST_BLOCK_FRAMES as i32),
        (preferred, HOST_BLOCK_FRAMES as i32),
        (granularity, 0),
    ] {
        if !target.is_null() {
            *target = value;
        }
    }
    trace(&format!(
        "get_buffer_size -> min {HOST_BLOCK_FRAMES} max {HOST_BLOCK_FRAMES} preferred {HOST_BLOCK_FRAMES} granularity 0"
    ));
    ASE_OK
}

unsafe extern "system" fn asio_can_sample_rate(
    this: *mut c_void,
    rate: ASIOSampleRate,
) -> ASIOError {
    let state = state(this);
    let wanted = state
        .shared
        .as_ref()
        .map(|shared| shared.rate() as f64)
        .unwrap_or(state.rate);
    let ok = (rate - wanted).abs() < 0.5;
    trace(&format!("can_sample_rate({rate}) -> {ok}"));
    if ok {
        ASE_OK
    } else {
        ASE_NotPresent
    }
}

unsafe extern "system" fn asio_get_sample_rate(
    this: *mut c_void,
    rate: *mut ASIOSampleRate,
) -> ASIOError {
    let state = state(this);
    let wanted = state
        .shared
        .as_ref()
        .map(|shared| shared.rate() as f64)
        .unwrap_or(state.rate);
    if !rate.is_null() {
        *rate = wanted;
    }
    ASE_OK
}

unsafe extern "system" fn asio_set_sample_rate(
    this: *mut c_void,
    rate: ASIOSampleRate,
) -> ASIOError {
    let result = asio_can_sample_rate(this, rate);
    trace(&format!(
        "set_sample_rate({rate}) -> {}",
        if result == ASE_OK { "OK" } else { "NotPresent" }
    ));
    result
}

unsafe extern "system" fn asio_get_clock_sources(
    _this: *mut c_void,
    clocks: *mut ASIOClockSource,
    count: *mut i32,
) -> ASIOError {
    if !clocks.is_null() {
        let mut source = ASIOClockSource {
            index: 0,
            associated_channel: -1,
            associated_group: -1,
            is_current_source: ASIO_TRUE,
            name: [0; 32],
        };
        for (i, byte) in b"Internal\0".iter().enumerate() {
            source.name[i] = *byte;
        }
        *clocks = source;
    }
    if !count.is_null() {
        *count = 1;
    }
    ASE_OK
}

unsafe extern "system" fn asio_set_clock_source(_this: *mut c_void, reference: i32) -> ASIOError {
    if reference == 0 {
        ASE_OK
    } else {
        ASE_NotPresent
    }
}

unsafe extern "system" fn asio_get_sample_position(
    this: *mut c_void,
    samples: *mut ASIOSamples,
    timestamp: *mut ASIOTimeStamp,
) -> ASIOError {
    let state = state(this);
    let position = state.sample_pos.load(Ordering::Relaxed);
    if !samples.is_null() {
        *samples = ASIOSamples {
            hi: (position >> 32) as u32,
            lo: position as u32,
        };
    }
    if !timestamp.is_null() {
        *timestamp = ASIOTimeStamp { hi: 0, lo: 0 };
    }
    ASE_OK
}

unsafe extern "system" fn asio_get_block_size(_this: *mut c_void, size: *mut i32) -> ASIOError {
    if !size.is_null() {
        *size = HOST_BLOCK_FRAMES as i32;
    }
    ASE_OK
}

unsafe extern "system" fn asio_create_buffers(
    this: *mut c_void,
    infos: *mut ASIOBufferInfo,
    num_channels: i32,
    buffer_size: i32,
    callbacks: *mut ASIOCallbacks,
) -> ASIOError {
    trace(&format!(
        "create_buffers(num_channels {num_channels}, buffer_size {buffer_size}, infos null {}, callbacks null {})",
        infos.is_null(),
        callbacks.is_null()
    ));
    if infos.is_null()
        || callbacks.is_null()
        || num_channels != (CHANNELS * 2) as i32
        || (buffer_size != BLOCK_FRAMES as i32 && buffer_size != HOST_BLOCK_FRAMES as i32)
    {
        trace("create_buffers -> NotPresent (unsupported shape)");
        return ASE_NotPresent;
    }
    let state = state(this);
    state.callbacks = *callbacks;
    state.split_buffers.store(false, Ordering::Relaxed);
    // Keep buffers from an earlier createBuffers call alive: a host may still
    // hold the pointers it was given, and a few 2 KB vectors are cheap.
    let mut missing = 0usize;
    let mut supplied = 0usize;
    for i in 0..num_channels as usize {
        let info = &mut *infos.add(i);
        let channel = info.channel_num as usize;
        trace(&format!(
            "  channel[{i}] is_input {} num {} buffer0 {:p} buffer1 {:p}",
            info.is_input, info.channel_num, info.buffers[0], info.buffers[1]
        ));
        if channel >= CHANNELS {
            trace("create_buffers -> NotPresent (channel number out of range)");
            return ASE_NotPresent;
        }
        if info.buffers[0].is_null() && info.buffers[1].is_null() {
            // JUCE hands us the array with empty pointers and expects the driver
            // to provide the storage; write the pointers back so the host sees
            // the same memory the pump uses.
            let mut first = vec![0.0f32; BLOCK_FRAMES];
            let mut second = vec![0.0f32; BLOCK_FRAMES];
            info.buffers[0] = first.as_mut_ptr() as *mut c_void;
            info.buffers[1] = second.as_mut_ptr() as *mut c_void;
            state.owned.push(first);
            state.owned.push(second);
            supplied += 1;
        }
        // The host usually fills both slots (double buffering); use whichever is
        // present, preferring the first.
        let pointer = if info.buffers[0].is_null() {
            info.buffers[1]
        } else {
            info.buffers[0]
        } as *mut f32;
        if pointer.is_null() {
            missing += 1;
            continue;
        }
        // JUCE runs the graph in half-size blocks and puts the second half in
        // buffers[1] of the same callback. Keep both, and note whether they are
        // actually distinct (a host may pass one pointer twice).
        let second = info.buffers[1] as *mut f32;
        let split = !second.is_null() && second != pointer;
        if split {
            state.split_buffers.store(true, Ordering::Relaxed);
        }
        if info.is_input != 0 {
            state.in_ptrs[channel] = pointer;
            state.in_ptrs2[channel] = if split { second } else { pointer };
        } else {
            state.out_ptrs[channel] = pointer;
            state.out_ptrs2[channel] = if split { second } else { pointer };
        }
    }
    if supplied > 0 {
        trace(&format!("  supplied buffers for {supplied} channels"));
    }
    if missing > 0 {
        trace(&format!(
            "create_buffers -> NotPresent ({missing} of {num_channels} entries had no buffer)"
        ));
        return ASE_NotPresent;
    }
    state.running.store(false, Ordering::SeqCst);
    trace("create_buffers -> OK");
    ASE_OK
}

unsafe extern "system" fn asio_dispose_buffers(this: *mut c_void) -> ASIOError {
    let _ = asio_stop(this);
    let state = state(this);
    state.in_ptrs = [std::ptr::null_mut(); CHANNELS];
    state.out_ptrs = [std::ptr::null_mut(); CHANNELS];
    state.in_ptrs2 = [std::ptr::null_mut(); CHANNELS];
    state.out_ptrs2 = [std::ptr::null_mut(); CHANNELS];
    state.split_buffers.store(false, Ordering::Relaxed);
    state.callbacks = ASIOCallbacks {
        buffer_switch: None,
        sample_rate_did_change: None,
        asio_message: None,
        buffer_switch_time_info: None,
    };
    ASE_OK
}

unsafe extern "system" fn asio_control_panel(this: *mut c_void) -> ASIOError {
    let state = state(this);
    trace("control_panel");
    match state.shared.as_ref() {
        Some(shared) if shared.host_attached() => {}
        _ => return ASE_NotPresent,
    }
    ASE_OK
}

unsafe extern "system" fn asio_future(
    _this: *mut c_void,
    selector: i32,
    opt: *mut c_void,
) -> ASIOError {
    trace(&format!("future(selector {selector})"));
    match selector {
        kAsioSelectorSupported => {
            if opt.is_null() {
                return 0;
            }
            let wanted = *(opt as *const i32);
            matches!(
                wanted,
                kAsioEngineVersion
                    | kAsioResetRequest
                    | kAsioBufferSizeChange
                    | kAsioResyncRequest
                    | kAsioLatenciesChanged
                    | kAsioSupportsTimeInfo
                    | kAsioSupportsTimeCode
            ) as ASIOError
        }
        kAsioEngineVersion => {
            if !opt.is_null() {
                *(opt as *mut i32) = 2;
            }
            ASE_OK
        }
        kAsioSupportsTimeInfo => {
            if !opt.is_null() {
                *(opt as *mut i32) = 1;
            }
            ASE_OK
        }
        kAsioSupportsTimeCode => {
            if !opt.is_null() {
                *(opt as *mut i32) = 0;
            }
            ASE_OK
        }
        kAsioResetRequest | kAsioResyncRequest | kAsioLatenciesChanged => ASE_OK,
        _ => ASE_NotPresent,
    }
}

unsafe extern "system" fn asio_output_ready(this: *mut c_void) -> ASIOError {
    // The host says it has written the outputs for the block we handed it. Do not
    // read them here: Waveform's engine can still be filling the buffers at this
    // point, and reading then produced blocks whose tails were zeros. Record the
    // block and let the pump copy it on its next callback, a full block later.
    let state = state(this);
    let seq = state.current_seq.load(Ordering::Relaxed);
    if state.waiting.load(Ordering::Relaxed) == seq {
        state.pending_copy.store(seq, Ordering::Relaxed);
        state.out_ready_seq.store(seq, Ordering::Relaxed);
        state.signals.fetch_add(1, Ordering::Relaxed);
        state.ready_calls.fetch_add(1, Ordering::Relaxed);
    }
    ASE_OK
}

static ASIO_VTBL: IASIOVtbl = IASIOVtbl {
    query_interface: asio_query_interface,
    add_ref: asio_add_ref,
    release: asio_release,
    init: asio_init,
    get_driver_name: asio_get_driver_name,
    get_driver_version: asio_get_driver_version,
    get_error_message: asio_get_error_message,
    start: asio_start,
    stop: asio_stop,
    get_channels: asio_get_channels,
    get_latencies: asio_get_latencies,
    get_buffer_size: asio_get_buffer_size,
    can_sample_rate: asio_can_sample_rate,
    get_sample_rate: asio_get_sample_rate,
    set_sample_rate: asio_set_sample_rate,
    get_clock_sources: asio_get_clock_sources,
    set_clock_source: asio_set_clock_source,
    get_sample_position: asio_get_sample_position,
    get_block_size: asio_get_block_size,
    create_buffers: asio_create_buffers,
    dispose_buffers: asio_dispose_buffers,
    control_panel: asio_control_panel,
    future: asio_future,
    output_ready: asio_output_ready,
};

// ---------------------------------------------------------------------------
// Pump
// ---------------------------------------------------------------------------

struct StatePtr(*mut DriverState);
unsafe impl Send for StatePtr {}

fn pump(ptr: StatePtr) {
    let state = unsafe { &*ptr.0 };
    let Some(shared) = state.shared.as_ref() else {
        return;
    };
    let rate = shared.rate().max(1);
    // One block of the host's time. When the mixer is not publishing (it is not
    // wired yet, or it was closed), the driver still runs its own clock and
    // feeds silence: a host that starts a device and then sees the sample
    // position frozen reports "device didn't start correctly".
    let period_ms = ((BLOCK_FRAMES as f64 / rate as f64) * 1000.0).round() as u32;
    let period = std::time::Duration::from_millis(period_ms.max(1) as u64);
    let mut last_seq = shared.seq_in();
    let mut reported_silent = false;
    let mut reported_audio = false;
    let mut last_block = std::time::Instant::now();
    let mut trace_at = std::time::Instant::now();

    while !state.stop.load(Ordering::SeqCst) {
        if trace_at.elapsed() >= std::time::Duration::from_secs(1) {
            trace_at = std::time::Instant::now();
            trace(&format!(
                "stats: blocks {} in_peak {:.3} out_peak {:.3} outputReady {} seq_in {} seq_out {}",
                state.sample_pos.load(Ordering::Relaxed) / BLOCK_FRAMES as u64,
                f32::from_bits(state.in_peak.swap(0, Ordering::Relaxed)),
                f32::from_bits(state.out_peak.swap(0, Ordering::Relaxed)),
                state.ready_calls.swap(0, Ordering::Relaxed),
                shared.seq_in(),
                shared.seq_out()
            ));
            // Per-channel detail for the first pair: tells a genuine gain apart
            // from a pointer/channel mix-up or garbage.
            let detail = |ptrs: &[*mut f32; CHANNELS]| {
                let mut text = String::new();
                for channel in 0..PAIRS * 2 {
                    let buffer = ptrs[channel];
                    if buffer.is_null() {
                        text.push_str(" null");
                        continue;
                    }
                    let mut peak = 0.0f32;
                    for frame in 0..BLOCK_FRAMES {
                        unsafe {
                            peak = peak.max((*buffer.add(frame)).abs());
                        }
                    }
                    text.push_str(&format!(" {peak:.3}"));
                }
                text
            };
            let head = |buffer: *mut f32| {
                if buffer.is_null() {
                    "null".to_string()
                } else {
                    let mut text = String::new();
                    for frame in 0..4 {
                        unsafe {
                            text.push_str(&format!(" {:.3}", *buffer.add(frame)));
                        }
                    }
                    text
                }
            };
            let head6 = |buffer: *mut f32| {
                if buffer.is_null() {
                    "null".to_string()
                } else {
                    let mut text = String::new();
                    for frame in 0..6 {
                        unsafe {
                            text.push_str(&format!(" {:.3}", *buffer.add(frame)));
                        }
                    }
                    text
                }
            };
            // Is the host's output even related to what it was given? A scaled
            // copy correlates near 1; an oscillator or unrelated data near 0.
            let corr = |a: *mut f32, b: *mut f32| {
                if a.is_null() || b.is_null() {
                    return 0.0;
                }
                let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
                for frame in 0..BLOCK_FRAMES {
                    unsafe {
                        let x = *a.add(frame);
                        let y = *b.add(frame);
                        dot += x * y;
                        na += x * x;
                        nb += y * y;
                    }
                }
                dot / (na.sqrt() * nb.sqrt()).max(1e-9)
            };
            // Where is the loudest sample? A spike in the host's output that is
            // not in its input is generated by the host; one at the same place
            // came from the mixer.
            let peak_at = |buffer: *mut f32| {
                if buffer.is_null() {
                    return (0usize, 0.0f32);
                }
                let (mut index, mut value) = (0usize, 0.0f32);
                for frame in 0..BLOCK_FRAMES {
                    unsafe {
                        let sample = *buffer.add(frame);
                        if sample.abs() > value.abs() {
                            value = sample;
                            index = frame;
                        }
                    }
                }
                (index, value)
            };
            let (in_at, in_value) = peak_at(state.in_ptrs[0]);
            let (out_at, out_value) = peak_at(state.out_ptrs[0]);
            // First two and last two samples: a boundary discontinuity shows up
            // there, a mid-block spike elsewhere.
            let ends = |buffer: *mut f32| {
                if buffer.is_null() {
                    "null".to_string()
                } else {
                    unsafe {
                        format!(
                            " {} {} ... {} {}",
                            *buffer,
                            *buffer.add(1),
                            *buffer.add(BLOCK_FRAMES - 2),
                            *buffer.add(BLOCK_FRAMES - 1)
                        )
                    }
                }
            };
            trace(&format!(
                "  in per channel{}  out per channel{}  corr {:.2}/{:.2}  in0[0..4]{}  out0[0..6]{}",
                detail(&state.in_ptrs),
                detail(&state.out_ptrs),
                corr(state.in_ptrs[0], state.out_ptrs[0]),
                corr(state.in_ptrs[1], state.out_ptrs[1]),
                head(state.in_ptrs[0]),
                head6(state.out_ptrs[0])
            ));
            trace(&format!(
                "  peak at in[{in_at}]={in_value:.3} out[{out_at}]={out_value:.3}  in ends{}  out ends{}",
                ends(state.in_ptrs[0]),
                ends(state.out_ptrs[0])
            ));
            // How many samples of the returned block differ from what was sent,
            // and where the first one is. One differing sample at index 0 is a
            // boundary discontinuity; many means real processing.
            let (mut differences, mut first) = (0usize, usize::MAX);
            if !state.in_ptrs[0].is_null() && !state.out_ptrs[0].is_null() {
                for frame in 0..BLOCK_FRAMES {
                    unsafe {
                        let delta =
                            (*state.in_ptrs[0].add(frame) - *state.out_ptrs[0].add(frame)).abs();
                        if delta > 1e-4 {
                            differences += 1;
                            if first == usize::MAX {
                                first = frame;
                            }
                        }
                    }
                }
            }
            trace(&format!(
                "  differing samples: {differences} first at {}",
                if first == usize::MAX {
                    "-".to_string()
                } else {
                    first.to_string()
                }
            ));
            // Where does the host stop writing? A block that is only partly
            // filled (zeros to the end) means it thinks the buffer is smaller
            // than the 512 frames we advertised.
            let filled = |buffer: *mut f32| {
                if buffer.is_null() {
                    return (usize::MAX, 0usize);
                }
                let (mut last, mut zeros) = (usize::MAX, 0usize);
                for frame in 0..BLOCK_FRAMES {
                    unsafe {
                        if *buffer.add(frame) == 0.0 {
                            zeros += 1;
                        } else {
                            last = frame;
                        }
                    }
                }
                (last, zeros)
            };
            let (out_last, out_zeros) = filled(state.out_ptrs[0]);
            let (in_last, in_zeros) = filled(state.in_ptrs[0]);
            trace(&format!(
                "  last nonzero: out {} (zeros {out_zeros})  in {} (zeros {in_zeros})",
                if out_last == usize::MAX {
                    "-".to_string()
                } else {
                    out_last.to_string()
                },
                if in_last == usize::MAX {
                    "-".to_string()
                } else {
                    in_last.to_string()
                }
            ));
        }
        // Wait up to one block for the mixer. A wake-up that does not carry a
        // new block is ignored; only if nothing has arrived for a whole block
        // does the driver generate a silent one, so a running mixer alone sets
        // the pace and no extra blocks are inserted between its publishes.
        let woken = shared.wait_block(period_ms.max(1));
        // `asio_stop` publishes a block to wake this thread; leave before that
        // wake-up is mistaken for audio, so the sample position stops exactly
        // where the host stopped counting.
        if state.stop.load(Ordering::SeqCst) {
            break;
        }
        let target_seq = shared.seq_in();
        let fresh = woken && target_seq != last_seq;
        if !fresh {
            // Give the mixer three blocks of grace: publish jitter must not make
            // the driver slip a silent block between real ones, which would put
            // the return one block out of step every time.
            if last_block.elapsed() < period * 3 {
                continue;
            }
            if !reported_silent {
                reported_silent = true;
                trace("pump: running without the mixer (silent clock)");
            }
            last_seq = target_seq;
            // Present silence for this period and return it as block `target_seq`.
            for channel in 0..CHANNELS {
                let input = state.in_ptrs[channel];
                if !input.is_null() {
                    unsafe { std::ptr::write_bytes(input, 0u8, BLOCK_FRAMES) };
                }
            }
            call_host(&state, &shared, rate, target_seq);
            last_block = std::time::Instant::now();
            continue;
        }

        // One host callback per published block, so no audio is ever skipped.
        let mut seq = last_seq;
        while seq < target_seq {
            seq += 1;
            for pair in 0..PAIRS {
                let source = shared.input_block(pair, seq);
                let split = state.split_buffers.load(Ordering::Relaxed);
                for channel in 0..2 {
                    let first = state.in_ptrs[pair * 2 + channel];
                    let second = state.in_ptrs2[pair * 2 + channel];
                    if first.is_null() {
                        continue;
                    }
                    for frame in 0..BLOCK_FRAMES {
                        let value = unsafe { *source.add(frame * 2 + channel) };
                        unsafe {
                            if split {
                                let half = BLOCK_FRAMES / 2;
                                if frame < half {
                                    *first.add(frame) = value;
                                } else if !second.is_null() {
                                    *second.add(frame - half) = value;
                                }
                            } else {
                                *first.add(frame) = value;
                            }
                        }
                    }
                }
            }
            call_host(&state, &shared, rate, seq);
            if !reported_audio {
                reported_audio = true;
                trace("pump: first block from the mixer");
            }
        }
        last_seq = target_seq;
        last_block = std::time::Instant::now();
    }
    shared.set_host_attached(false);
}

/// One host callback for block `seq`. The block is published to the mixer only
/// once its outputs are final: a synchronous host has them when `bufferSwitch`
/// returns, an asynchronous one (JUCE) after it calls `outputReady`. Publishing
/// a block the host is still writing is what turns the return into noise, so a
/// host that misses the deadline gets a silent block instead.
fn call_host(state: &DriverState, shared: &Shared, rate: u32, seq: u64) {
    let callbacks = state.callbacks;
    let position = state.sample_pos.load(Ordering::Relaxed);
    // Publish what the host finished during the previous callback. A full block
    // has passed since it signalled, so its engine cannot still be writing.
    let pending = state.pending_copy.swap(u64::MAX, Ordering::Relaxed);
    if pending != u64::MAX {
        copy_outputs(state, shared, pending);
        shared.set_seq_out(pending);
    }
    state.current_seq.store(seq, Ordering::Relaxed);
    state.out_ready_seq.store(u64::MAX, Ordering::Relaxed);
    state.waiting.store(seq, Ordering::Relaxed);
    state
        .in_peak
        .fetch_max(input_peak(state).to_bits(), Ordering::Relaxed);
    // The output buffers belong to the driver and some hosts mix into them
    // rather than overwrite. Left uncleared, every block would sum with the
    // previous one - a comb filter that saturates.
    let split = state.split_buffers.load(Ordering::Relaxed);
    let clear = if split {
        BLOCK_FRAMES / 2
    } else {
        BLOCK_FRAMES
    };
    for channel in 0..CHANNELS {
        let first = state.out_ptrs[channel];
        if !first.is_null() {
            unsafe { std::ptr::write_bytes(first, 0u8, clear) };
        }
        if split {
            let second = state.out_ptrs2[channel];
            if !second.is_null() {
                unsafe { std::ptr::write_bytes(second, 0u8, clear) };
            }
        }
    }

    // A host whose graph runs in half-size blocks (JUCE) must be called once per
    // half, with the buffer slot for that half: its buffers[0] carries the first
    // 256 frames and buffers[1] the second, and it only fills the one it is
    // called with. Calling it once would drop half of every block.
    let split = state.split_buffers.load(Ordering::Relaxed);
    state.signals.store(0, Ordering::Relaxed);
    let calls: i32 = if split { 2 } else { 1 };
    for chunk in 0..calls {
        let clear = if split {
            BLOCK_FRAMES / 2
        } else {
            BLOCK_FRAMES
        };
        let pointers = if split && chunk == 1 {
            state.out_ptrs2
        } else {
            state.out_ptrs
        };
        for channel in 0..CHANNELS {
            let buffer = pointers[channel];
            if !buffer.is_null() {
                unsafe { std::ptr::write_bytes(buffer, 0u8, clear) };
            }
        }
        if let Some(time_info) = callbacks.buffer_switch_time_info {
            let mut time: ASIOTime = unsafe { std::mem::zeroed() };
            time.time_info.sample_rate = rate as f64;
            time.time_info.sample_position = ASIOSamples {
                hi: (position >> 32) as u32,
                lo: position as u32,
            };
            // kSystemTimeValid | kSamplePositionValid | kSampleRateValid.
            time.time_info.flags = 1 | 2 | 4;
            let nanos = ((position as f64 / rate.max(1) as f64) * 1_000_000_000.0) as u64;
            time.time_info.system_time = ASIOTimeStamp {
                hi: (nanos >> 32) as u32,
                lo: nanos as u32,
            };
            time.time_info.speed = 1.0;
            unsafe { time_info(&mut time, chunk, ASIO_FALSE) };
        } else if let Some(buffer_switch) = callbacks.buffer_switch {
            unsafe { buffer_switch(chunk, ASIO_FALSE) };
        }
    }

    let period =
        std::time::Duration::from_micros((BLOCK_FRAMES as u64 * 1_000_000) / rate.max(1) as u64);
    let mode = state.ready_host.load(Ordering::Relaxed);
    let budget = if mode == 2 {
        std::time::Duration::from_millis(1)
    } else {
        period
    };
    let deadline = std::time::Instant::now() + budget;
    while (state.signals.load(Ordering::Relaxed) as i32) < calls
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_micros(50));
    }
    let signalled = state.signals.load(Ordering::Relaxed) as i32 >= calls;

    if signalled {
        // `output_ready` recorded this block for the next callback's copy; it is
        // published then, not now.
        state.ready_host.store(1, Ordering::Relaxed);
        state.no_signal.store(0, Ordering::Relaxed);
    } else if mode == 1 {
        // A host that uses outputReady did not finish in time: publish silence
        // rather than half-written audio.
        for pair in 0..PAIRS {
            let target = shared.output_block(pair, seq);
            unsafe { std::ptr::write_bytes(target, 0u8, BLOCK_FRAMES * 2) };
        }
        state.late_blocks.fetch_add(1, Ordering::Relaxed);
        shared.set_seq_out(seq);
    } else {
        // Synchronous so far: the host wrote the buffers inside bufferSwitch.
        copy_outputs(state, shared, seq);
        if mode == 0 {
            let misses = state.no_signal.fetch_add(1, Ordering::Relaxed) + 1;
            if misses >= 3 {
                state.ready_host.store(2, Ordering::Relaxed);
            }
        }
        shared.set_seq_out(seq);
    }
    state.waiting.store(u64::MAX, Ordering::Relaxed);
    state
        .sample_pos
        .fetch_add(BLOCK_FRAMES as u64, Ordering::Relaxed);
}

/// Interleave the host's planar output buffers into the section block `seq`.
fn copy_outputs(state: &DriverState, shared: &Shared, seq: u64) {
    let mut peak = 0.0f32;
    let split = state.split_buffers.load(Ordering::Relaxed);
    let half = BLOCK_FRAMES / 2;
    for pair in 0..PAIRS {
        let target = shared.output_block(pair, seq);
        for channel in 0..2 {
            let first = state.out_ptrs[pair * 2 + channel];
            let second = state.out_ptrs2[pair * 2 + channel];
            for frame in 0..BLOCK_FRAMES {
                let value = unsafe {
                    if split {
                        if frame < half {
                            if first.is_null() {
                                0.0
                            } else {
                                *first.add(frame)
                            }
                        } else if second.is_null() {
                            0.0
                        } else {
                            *second.add(frame - half)
                        }
                    } else if first.is_null() {
                        0.0
                    } else {
                        *first.add(frame)
                    }
                };
                peak = peak.max(value.abs());
                unsafe { *target.add(frame * 2 + channel) = value };
            }
        }
    }
    state.out_peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
}

/// Highest absolute sample in the buffers the driver is about to hand the host.
fn input_peak(state: &DriverState) -> f32 {
    let mut peak = 0.0f32;
    let half = BLOCK_FRAMES / 2;
    let split = state.split_buffers.load(Ordering::Relaxed);
    for channel in 0..CHANNELS {
        let first = state.in_ptrs[channel];
        if first.is_null() {
            continue;
        }
        let frames = if split { half } else { BLOCK_FRAMES };
        for frame in 0..frames {
            unsafe {
                peak = peak.max((*first.add(frame)).abs());
            }
        }
        if split {
            let second = state.in_ptrs2[channel];
            if !second.is_null() {
                for frame in 0..half {
                    unsafe {
                        peak = peak.max((*second.add(frame)).abs());
                    }
                }
            }
        }
    }
    peak
}

// ---------------------------------------------------------------------------
// Class factory and exports
// ---------------------------------------------------------------------------

unsafe extern "system" fn factory_query_interface(
    this: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    if ppv.is_null() || riid.is_null() {
        return E_NOINTERFACE;
    }
    let wanted = *riid;
    if wanted != IID_ICLASSFACTORY && wanted != IID_IUNKNOWN {
        return E_NOINTERFACE;
    }
    FACTORY_REFS.fetch_add(1, Ordering::SeqCst);
    *ppv = this;
    S_OK
}

unsafe extern "system" fn factory_add_ref(_this: *mut c_void) -> u32 {
    FACTORY_REFS.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn factory_release(_this: *mut c_void) -> u32 {
    let refs = FACTORY_REFS.fetch_sub(1, Ordering::SeqCst);
    refs.saturating_sub(1)
}

unsafe extern "system" fn factory_create_instance(
    _this: *mut c_void,
    outer: *mut c_void,
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    if ppv.is_null() {
        return E_NOINTERFACE;
    }
    let state = Box::into_raw(Box::new(DriverState::new()));
    let object = Box::into_raw(Box::new(AsioObject {
        vtbl: &ASIO_VTBL,
        state,
        refs: AtomicU32::new(1),
    }));
    *ppv = object as *mut c_void;
    S_OK
}

unsafe extern "system" fn factory_lock_server(_this: *mut c_void, _lock: i32) -> i32 {
    S_OK
}

static FACTORY_VTBL: IClassFactoryVtbl = IClassFactoryVtbl {
    query_interface: factory_query_interface,
    add_ref: factory_add_ref,
    release: factory_release,
    create_instance: factory_create_instance,
    lock_server: factory_lock_server,
};

static FACTORY: ClassFactory = ClassFactory {
    vtbl: &FACTORY_VTBL,
    refs: AtomicU32::new(0),
};

/// `DllGetClassObject`: what a host calls to obtain the driver.
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    rclsid: *const Guid,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    unsafe {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_NOINTERFACE;
        }
        if *rclsid != CLSID_ASIO_INSERT {
            return 0x8004_0111u32 as i32; // CLASS_E_CLASSNOTAVAILABLE
        }
        if *riid != IID_ICLASSFACTORY && *riid != IID_IUNKNOWN {
            return E_NOINTERFACE;
        }
        FACTORY_REFS.fetch_add(1, Ordering::SeqCst);
        *ppv = &FACTORY as *const ClassFactory as *mut c_void;
        S_OK
    }
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    S_FALSE
}
