//! The ASIO driver object.
//!
//! The mixer is the clock master: it publishes a block and signals an event, the
//! pump thread here copies that block into the host's input pointers, calls the
//! host's `bufferSwitch`, then copies the host's outputs back. One block each
//! way, no resampling, and the DAW follows our timer.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::abi::*;
use crate::section::{Shared, BLOCK_FRAMES, PAIRS};

/// ASIO buffers are planar: one buffer per channel, so two stereo pairs are
/// four input and four output channels.
const CHANNELS: usize = PAIRS * 2;

struct DriverState {
    shared: Option<Shared>,
    callbacks: ASIOCallbacks,
    in_ptrs: [*mut f32; CHANNELS],
    out_ptrs: [*mut f32; CHANNELS],
    running: AtomicBool,
    stop: AtomicBool,
    sample_pos: AtomicU64,
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
            out_ptrs: [std::ptr::null_mut(); CHANNELS],
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            sample_pos: AtomicU64::new(0),
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

unsafe extern "system" fn asio_init(this: *mut c_void, _sys_handle: *mut c_void) -> ASIOBool {
    let state = state(this);
    match Shared::open_or_create(48_000) {
        Ok(shared) => {
            state.rate = shared.rate() as f64;
            shared.set_host_attached(true);
            state.shared = Some(shared);
            ASIO_TRUE
        }
        Err(_) => ASIO_FALSE,
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
        return ASE_NotPresent;
    }
    if state.callbacks.buffer_switch.is_none() && state.callbacks.buffer_switch_time_info.is_none()
    {
        return ASE_NotPresent;
    }
    if state.running.swap(true, Ordering::SeqCst) {
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
            ASE_OK
        }
        Err(_) => {
            state.running.store(false, Ordering::SeqCst);
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
    ASE_OK
}

unsafe extern "system" fn asio_get_latencies(
    _this: *mut c_void,
    input: *mut i32,
    output: *mut i32,
) -> ASIOError {
    if !input.is_null() {
        *input = BLOCK_FRAMES as i32;
    }
    if !output.is_null() {
        *output = BLOCK_FRAMES as i32;
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
        (min, BLOCK_FRAMES as i32),
        (max, BLOCK_FRAMES as i32),
        (preferred, BLOCK_FRAMES as i32),
        (granularity, 0),
    ] {
        if !target.is_null() {
            *target = value;
        }
    }
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
    if (rate - wanted).abs() < 0.5 {
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
    asio_can_sample_rate(this, rate)
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
        *size = BLOCK_FRAMES as i32;
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
    if infos.is_null()
        || callbacks.is_null()
        || num_channels != (CHANNELS * 2) as i32
        || buffer_size != BLOCK_FRAMES as i32
    {
        return ASE_NotPresent;
    }
    let state = state(this);
    state.callbacks = *callbacks;
    for i in 0..num_channels as usize {
        let info = &*infos.add(i);
        let channel = info.channel_num as usize;
        if channel >= CHANNELS {
            return ASE_NotPresent;
        }
        let pointer = info.buffers[0] as *mut f32;
        if info.is_input != 0 {
            state.in_ptrs[channel] = pointer;
        } else {
            state.out_ptrs[channel] = pointer;
        }
    }
    state.running.store(false, Ordering::SeqCst);
    ASE_OK
}

unsafe extern "system" fn asio_dispose_buffers(this: *mut c_void) -> ASIOError {
    let _ = asio_stop(this);
    let state = state(this);
    state.in_ptrs = [std::ptr::null_mut(); CHANNELS];
    state.out_ptrs = [std::ptr::null_mut(); CHANNELS];
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

unsafe extern "system" fn asio_output_ready(_this: *mut c_void) -> ASIOError {
    // The pump copies the host's outputs back after bufferSwitch returns, so
    // there is nothing left to do here; some hosts still call it.
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
    let callbacks = state.callbacks;
    let mut last_seq = shared.seq_in();
    while !state.stop.load(Ordering::SeqCst) {
        if !shared.wait_block(200) {
            continue;
        }
        // `asio_stop` publishes a block to wake this thread; leave before that
        // wake-up is mistaken for audio, so the sample position stops exactly
        // where the host stopped counting.
        if state.stop.load(Ordering::SeqCst) {
            break;
        }
        let seq = shared.seq_in();
        if seq == last_seq {
            continue;
        }
        last_seq = seq;
        // The section stores stereo pairs interleaved; ASIO wants one planar
        // buffer per channel, so split on the way in and interleave on the way
        // back.
        for pair in 0..PAIRS {
            let source = shared.input(pair);
            let left = state.in_ptrs[pair * 2];
            let right = state.in_ptrs[pair * 2 + 1];
            for frame in 0..BLOCK_FRAMES {
                unsafe {
                    let l = *source.add(frame * 2);
                    let r = *source.add(frame * 2 + 1);
                    if !left.is_null() {
                        *left.add(frame) = l;
                    }
                    if !right.is_null() {
                        *right.add(frame) = r;
                    }
                }
            }
        }
        if let Some(buffer_switch) = callbacks.buffer_switch {
            unsafe { buffer_switch(0, ASIO_FALSE) };
        }
        for pair in 0..PAIRS {
            let target = shared.output(pair);
            let left = state.out_ptrs[pair * 2];
            let right = state.out_ptrs[pair * 2 + 1];
            for frame in 0..BLOCK_FRAMES {
                unsafe {
                    let l = if left.is_null() {
                        0.0
                    } else {
                        *left.add(frame)
                    };
                    let r = if right.is_null() {
                        0.0
                    } else {
                        *right.add(frame)
                    };
                    *target.add(frame * 2) = l;
                    *target.add(frame * 2 + 1) = r;
                }
            }
        }
        state
            .sample_pos
            .fetch_add(BLOCK_FRAMES as u64, Ordering::Relaxed);
        shared.publish_return();
    }
    shared.set_host_attached(false);
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
