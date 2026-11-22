//! Loads the driver the way a DAW does and checks the round trip.
//!
//! The probe is both sides of the test: it creates the shared section like the
//! mixer does, then loads `asio_insert.dll`, obtains `IASIO` through
//! `DllGetClassObject`, and drives it. Its `bufferSwitch` callback acts as the
//! host, copying each planar channel through, so the returned block must equal
//! the sent one.

#![allow(non_snake_case, dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use asio_insert::abi::*;
use asio_insert::section::{Shared, BLOCK_FRAMES, PAIRS};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32;

/// Two stereo pairs = four planar channels, exactly what ASIO asks for.
const CHANNELS: usize = PAIRS * 2;

/// Addresses of the planar channel buffers the driver fills and the host
/// writes, published to the callback (which runs on the driver's thread). ASIO
/// gives two buffers per channel for double buffering, and `bufferSwitch` tells
/// the host which one this callback belongs to.
static IN_ADDR: [[AtomicUsize; CHANNELS]; 2] =
    [const { [const { AtomicUsize::new(0) }; CHANNELS] }; 2];
static OUT_ADDR: [[AtomicUsize; CHANNELS]; 2] =
    [const { [const { AtomicUsize::new(0) }; CHANNELS] }; 2];

/// The host side: a DAW would process the input and write the output. Here the
/// output is a straight copy, so the round trip must be sample-identical.
/// The ASIO object, so the asynchronous host can call `outputReady` on it.
static ASIO: AtomicUsize = AtomicUsize::new(0);
/// `AMI_PROBE_ASYNC=1`: behave like JUCE - leave the outputs alone in
/// `bufferSwitch`, write them a moment later on another thread and then call
/// `outputReady`. `AMI_PROBE_ADDITIVE=1`: mix into the output buffers instead of
/// overwriting them, which is legal and what Waveform appears to do.
static ASYNC: AtomicBool = AtomicBool::new(false);
static ADDITIVE: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn buffer_switch(index: i32, _direct: ASIOBool) {
    let slot = index.clamp(0, 1) as usize;
    if ASYNC.load(Ordering::Relaxed) {
        // JUCE-style: the graph finishes after the callback returns.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(
                std::env::var("AMI_PROBE_DELAY_US")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(300),
            ));
            write_outputs(slot);
            let asio = ASIO.load(Ordering::Relaxed) as *mut c_void;
            if !asio.is_null() {
                let vtbl = unsafe { *(asio as *const *const IASIOVtbl) };
                unsafe { ((*vtbl).output_ready)(asio) };
            }
        });
        return;
    }
    write_outputs(slot);
}

/// Copy (or add) the planar inputs into the planar outputs for one buffer slot.
fn write_outputs(slot: usize) {
    let additive = ADDITIVE.load(Ordering::Relaxed);
    for channel in 0..CHANNELS {
        let source = IN_ADDR[slot][channel].load(Ordering::Relaxed) as *const f32;
        let target = OUT_ADDR[slot][channel].load(Ordering::Relaxed) as *mut f32;
        if source.is_null() || target.is_null() {
            continue;
        }
        for frame in 0..BLOCK_FRAMES {
            unsafe {
                if additive {
                    *target.add(frame) += *source.add(frame);
                } else {
                    *target.add(frame) = *source.add(frame);
                }
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    let seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(5.0);

    ASYNC.store(
        std::env::var("AMI_PROBE_ASYNC").map(|v| v != "0").unwrap_or(false),
        Ordering::Relaxed,
    );
    ADDITIVE.store(
        std::env::var("AMI_PROBE_ADDITIVE").map(|v| v != "0").unwrap_or(false),
        Ordering::Relaxed,
    );
    // ---- the mixer side -------------------------------------------------
    let shared = Shared::open_or_create(48_000).expect("could not open the insert section");
    println!(
        "section : {} Hz, {} frames, {} stereo pairs ({} channels)",
        shared.rate(),
        BLOCK_FRAMES,
        PAIRS,
        CHANNELS
    );

    // ---- load the driver like a host ------------------------------------
    let dll = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("asio_insert.dll")))
        .expect("cannot locate asio_insert.dll");
    println!("driver  : {}", dll.display());
    let module: HMODULE = unsafe { LoadLibraryW(wide(&dll.display().to_string()).as_ptr()) };
    assert!(
        !module.is_null(),
        "LoadLibraryW failed for {}",
        dll.display()
    );
    let entry = unsafe { GetProcAddress(module, b"DllGetClassObject\0".as_ptr()) }
        .expect("DllGetClassObject is not exported");
    let get_class: DllGetClassObjectFn = unsafe { std::mem::transmute(entry) };

    let mut factory: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { get_class(&CLSID_ASIO_INSERT, &IID_ICLASSFACTORY, &mut factory) };
    assert_eq!(hr, S_OK, "DllGetClassObject returned {hr:#x}");
    assert!(!factory.is_null());

    let factory_vtbl = unsafe { *(factory as *const *const IClassFactoryVtbl) };
    let mut asio: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
        ((*factory_vtbl).create_instance)(factory, std::ptr::null_mut(), &IID_IUNKNOWN, &mut asio)
    };
    assert_eq!(hr, S_OK, "CreateInstance returned {hr:#x}");
    assert!(!asio.is_null());
    let vtbl = unsafe { *(asio as *const *const IASIOVtbl) };
    ASIO.store(asio as usize, Ordering::Relaxed);

    // ---- IASIO surface --------------------------------------------------
    let ok = unsafe { ((*vtbl).init)(asio, std::ptr::null_mut()) };
    assert_eq!(ok, ASIO_TRUE, "init failed");

    let mut name = [0i8; 64];
    unsafe { ((*vtbl).get_driver_name)(asio, name.as_mut_ptr()) };
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .to_string();
    let version = unsafe { ((*vtbl).get_driver_version)(asio) };
    println!("name    : {name} (version {version})");

    let (mut inputs, mut outputs) = (0i32, 0i32);
    assert_eq!(
        unsafe { ((*vtbl).get_channels)(asio, &mut inputs, &mut outputs) },
        ASE_OK
    );
    assert_eq!((inputs, outputs), (CHANNELS as i32, CHANNELS as i32));

    let (mut min, mut max, mut preferred, mut granularity) = (0i32, 0i32, 0i32, 0i32);
    assert_eq!(
        unsafe {
            ((*vtbl).get_buffer_size)(asio, &mut min, &mut max, &mut preferred, &mut granularity)
        },
        ASE_OK
    );
    println!("buffers : min {min}, max {max}, preferred {preferred}, granularity {granularity}");
    assert_eq!((min, max, preferred), (512, 512, 512));

    let mut rate = 0.0;
    assert_eq!(
        unsafe { ((*vtbl).get_sample_rate)(asio, &mut rate) },
        ASE_OK
    );
    let can_96k = unsafe { ((*vtbl).can_sample_rate)(asio, 96_000.0) == ASE_OK };
    println!("rate    : {rate} Hz (can 96k: {can_96k})");
    assert!((rate - 48_000.0).abs() < 0.5);

    let mut latency_in = 0;
    let mut latency_out = 0;
    assert_eq!(
        unsafe { ((*vtbl).get_latencies)(asio, &mut latency_in, &mut latency_out) },
        ASE_OK
    );
    println!("latency : {latency_in} in / {latency_out} out frames");

    // A host asks what each channel carries before opening it. JUCE uses the
    // type to convert buffers, so a wrong/absent answer makes it read 16-bit
    // data as floats - the bug that used to make Waveform produce garbled audio.
    for channel in 0..CHANNELS {
        for is_input in [ASIO_TRUE, ASIO_FALSE] {
            let mut info = ASIOChannelInfo {
                channel: channel as i32,
                is_input,
                is_active: 0,
                channel_group: -1,
                channel_type: -1,
                name: [0; 32],
            };
            let err = unsafe { ((*vtbl).get_channel_info)(asio, &mut info) };
            assert_eq!(err, ASE_OK, "getChannelInfo failed for {channel}");
            assert_eq!(info.is_active, ASIO_TRUE);
            assert_eq!(info.channel_type, ASIOST_FLOAT32_LSB);
            let name = unsafe { std::ffi::CStr::from_ptr(info.name.as_ptr() as *const i8) }
                .to_string_lossy()
                .to_string();
            assert!(name.starts_with("Insert"), "channel name was {name:?}");
        }
    }
    let mut out_of_range = ASIOChannelInfo {
        channel: CHANNELS as i32,
        is_input: ASIO_TRUE,
        is_active: 0,
        channel_group: 0,
        channel_type: 0,
        name: [0; 32],
    };
    assert_eq!(
        unsafe { ((*vtbl).get_channel_info)(asio, &mut out_of_range) },
        ASE_InvalidParameter
    );
    println!("channels: active, {ASIOST_FLOAT32_LSB} (float32 LSB), named");

    // ---- buffers and callbacks ------------------------------------------
    let double = std::env::var("AMI_PROBE_DOUBLE")
        .map(|v| v != "0")
        .unwrap_or(false);
    let mut input_buffers: [[Vec<f32>; CHANNELS]; 2] =
        std::array::from_fn(|_| std::array::from_fn(|_| vec![0.0; BLOCK_FRAMES]));
    let mut output_buffers: [[Vec<f32>; CHANNELS]; 2] =
        std::array::from_fn(|_| std::array::from_fn(|_| vec![0.0; BLOCK_FRAMES]));
    // In single-buffer mode both ASIO slots point at the same memory, which is
    // legal; in double mode they are distinct and the callback index selects.
    let second_slot = if double { 1 } else { 0 };
    for slot in 0..2 {
        for channel in 0..CHANNELS {
            let source_slot = if double { slot } else { 0 };
            IN_ADDR[slot][channel].store(
                input_buffers[source_slot][channel].as_mut_ptr() as usize,
                Ordering::Relaxed,
            );
            OUT_ADDR[slot][channel].store(
                output_buffers[source_slot][channel].as_mut_ptr() as usize,
                Ordering::Relaxed,
            );
        }
    }
    println!(
        "host    : {} buffered",
        if double { "double" } else { "single" }
    );

    let mut infos: Vec<ASIOBufferInfo> = Vec::with_capacity(CHANNELS * 2);
    for channel in 0..CHANNELS {
        infos.push(ASIOBufferInfo {
            is_input: ASIO_TRUE,
            channel_num: channel as i32,
            buffers: [
                input_buffers[0][channel].as_mut_ptr() as *mut c_void,
                input_buffers[second_slot][channel].as_mut_ptr() as *mut c_void,
            ],
        });
    }
    for channel in 0..CHANNELS {
        infos.push(ASIOBufferInfo {
            is_input: ASIO_FALSE,
            channel_num: channel as i32,
            buffers: [
                output_buffers[0][channel].as_mut_ptr() as *mut c_void,
                output_buffers[second_slot][channel].as_mut_ptr() as *mut c_void,
            ],
        });
    }
    let mut callbacks = ASIOCallbacks {
        buffer_switch: Some(buffer_switch),
        sample_rate_did_change: None,
        asio_message: None,
        buffer_switch_time_info: None,
    };
    let result = unsafe {
        ((*vtbl).create_buffers)(
            asio,
            infos.as_mut_ptr(),
            infos.len() as i32,
            512,
            &mut callbacks,
        )
    };
    assert_eq!(result, ASE_OK, "create_buffers returned {result}");

    let mut position: u64 = 0;
    let mut blocks = 0u64;
    let mut missed = 0u64;
    let mut mismatches = 0u64;
    let mut max_error = 0.0f32;
    let started = Instant::now();
    assert_eq!(unsafe { ((*vtbl).start)(asio) }, ASE_OK, "start failed");
    // One real-time block per iteration: the probe stands in for the mixer, so
    // it must publish at the mixer's pace rather than as fast as it can.
    let period = Duration::from_secs_f64(BLOCK_FRAMES as f64 / 48_000.0);
    let mut next = Instant::now() + period;

    let step = 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0;
    let mut phase: f64 = 0.0;
    let mut seq: u64 = 0;
    while started.elapsed() < Duration::from_secs_f64(seconds) {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += period;
        seq += 1;
        // Mixer side: a 1 kHz tone on both channels of both pairs.
        let mut expected = [0.0f32; BLOCK_FRAMES];
        for value in expected.iter_mut() {
            *value = (phase.sin() * 0.1) as f32;
            phase += step;
            if phase > 2.0 * std::f64::consts::PI {
                phase -= 2.0 * std::f64::consts::PI;
            }
        }
        for pair in 0..PAIRS {
            let target = unsafe {
                std::slice::from_raw_parts_mut(shared.input_block(pair, seq), BLOCK_FRAMES * 2)
            };
            for frame in 0..BLOCK_FRAMES {
                target[frame * 2] = expected[frame];
                target[frame * 2 + 1] = expected[frame];
            }
        }
        shared.publish_block();
        // Wait until the driver has returned this block.
        let deadline = Instant::now() + Duration::from_millis(250);
        while shared.seq_out() < seq && Instant::now() < deadline {
            let _ = shared.wait_return(50);
        }
        if shared.seq_out() < seq {
            missed += 1;
            continue;
        }
        for pair in 0..PAIRS {
            let out = unsafe {
                std::slice::from_raw_parts(shared.output_block(pair, seq), BLOCK_FRAMES * 2)
            };
            for frame in 0..BLOCK_FRAMES {
                for channel in 0..2 {
                    let error = (out[frame * 2 + channel] - expected[frame]).abs();
                    if error > max_error {
                        max_error = error;
                    }
                    if error > 1e-6 {
                        mismatches += 1;
                    }
                }
            }
        }
        position += BLOCK_FRAMES as u64;
        blocks += 1;
    }

    assert_eq!(unsafe { ((*vtbl).stop)(asio) }, ASE_OK);
    // The asynchronous host writes on detached threads; let the last one finish
    // before the object is released, or it would call outputReady on freed memory.
    std::thread::sleep(Duration::from_millis(100));

    let mut reported = ASIOSamples { hi: 0, lo: 0 };
    let mut stamp = ASIOTimeStamp { hi: 0, lo: 0 };
    let _ = unsafe { ((*vtbl).get_sample_position)(asio, &mut reported, &mut stamp) };
    let reported = ((reported.hi as u64) << 32) | reported.lo as u64;

    println!();
    println!(
        "blocks  : {blocks} in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    println!("missed  : {missed}");
    println!("mismatch: {mismatches} samples (max error {max_error:.9})");
    println!("position: {reported} frames (expected {position})");

    assert_eq!(missed, 0, "the driver missed blocks");
    assert_eq!(mismatches, 0, "returned audio did not match the sent block");

    assert_eq!(unsafe { ((*vtbl).dispose_buffers)(asio) }, ASE_OK);
    unsafe { ((*vtbl).release)(asio) };
    unsafe { ((*factory_vtbl).release)(factory) };
    drop(shared);
    println!("OK");
}
