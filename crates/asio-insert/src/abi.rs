//! Hand-declared ASIO 2 interface.
//!
//! No Steinberg header or SDK code is used: these are the published structures
//! and the vtable order, written out so the driver can be built and shipped
//! without the ASIO SDK licence. The order below is the one every host we care
//! about expects (IUnknown's three slots first, then `init` ... `outputReady`);
//! `asio_probe` and Waveform are the tests that it is right.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type ASIOBool = i32;
pub type ASIOError = i32;
pub type ASIOSampleRate = f64;
pub type RefTime = i64;

pub const ASE_OK: ASIOError = 0;
pub const ASE_SUCCESS: ASIOError = 0x3f48_47a0;
pub const ASE_NotPresent: ASIOError = -1000;
pub const ASE_HWMalfunction: ASIOError = -999;

pub const ASIO_TRUE: ASIOBool = 1;
pub const ASIO_FALSE: ASIOBool = 0;

// Host -> driver message selectors (ASIO 2.3).
pub const kAsioSelectorSupported: i32 = 1;
pub const kAsioEngineVersion: i32 = 2;
pub const kAsioResetRequest: i32 = 3;
pub const kAsioBufferSizeChange: i32 = 4;
pub const kAsioResyncRequest: i32 = 5;
pub const kAsioLatenciesChanged: i32 = 6;
pub const kAsioSupportsTimeInfo: i32 = 7;
pub const kAsioSupportsTimeCode: i32 = 8;
pub const kAsioNumMessageSelectors: i32 = 11;

pub const S_OK: i32 = 0;
pub const S_FALSE: i32 = 1;
pub const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
pub const CLASS_E_NOAGGREGATION: i32 = 0x8004_0110u32 as i32;

/// COM GUID, laid out exactly like Windows' `GUID`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const IID_IUNKNOWN: Guid = Guid {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

pub const IID_ICLASSFACTORY: Guid = Guid {
    data1: 0x0000_0001,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// Audio Mixer PRO Insert. Written to `HKLM\SOFTWARE\ASIO` when the driver is
/// installed; keep it in step with `install` in the mixer.
pub const CLSID_ASIO_INSERT: Guid = Guid {
    data1: 0xB7F3_C2A1,
    data2: 0x5D4E,
    data3: 0x4F0B,
    data4: [0x9C, 0x7A, 0x3E, 0x1D, 0x2B, 0x6A, 0x8F, 0x90],
};

pub const DRIVER_NAME: &str = "Audio Mixer PRO Insert";

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOSamples {
    pub hi: u32,
    pub lo: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOTimeStamp {
    pub hi: u32,
    pub lo: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOTimeCode {
    pub speed: f64,
    pub time_code_samples: ASIOSamples,
    pub flags: u32,
    pub future: [i8; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOTimeInfo {
    pub speed: f64,
    pub system_time: ASIOTimeStamp,
    pub sample_position: ASIOSamples,
    pub sample_rate: ASIOSampleRate,
    pub flags: u32,
    pub reserved: [i8; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOTime {
    pub reserved: [i32; 4],
    pub time_info: ASIOTimeInfo,
    pub time_code: ASIOTimeCode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: ASIOBool,
    pub name: [u8; 32],
}

#[repr(C)]
pub struct ASIOBufferInfo {
    pub is_input: ASIOBool,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2],
}

pub type BufferSwitchFn = Option<unsafe extern "system" fn(i32, ASIOBool)>;
pub type SampleRateDidChangeFn = Option<unsafe extern "system" fn(ASIOSampleRate)>;
pub type AsioMessageFn = Option<unsafe extern "system" fn(i32, i32, *mut c_void, *mut f64) -> i32>;
pub type BufferSwitchTimeInfoFn =
    Option<unsafe extern "system" fn(*mut ASIOTime, i32, ASIOBool) -> *mut ASIOTime>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOCallbacks {
    pub buffer_switch: BufferSwitchFn,
    pub sample_rate_did_change: SampleRateDidChangeFn,
    pub asio_message: AsioMessageFn,
    pub buffer_switch_time_info: BufferSwitchTimeInfoFn,
}

// ---------------------------------------------------------------------------
// Vtables
// ---------------------------------------------------------------------------

pub type QueryInterfaceFn =
    unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
pub type RefFn = unsafe extern "system" fn(*mut c_void) -> u32;

#[repr(C)]
pub struct IASIOVtbl {
    // IUnknown
    pub query_interface: QueryInterfaceFn,
    pub add_ref: RefFn,
    pub release: RefFn,
    // IASIO
    pub init: unsafe extern "system" fn(*mut c_void, *mut c_void) -> ASIOBool,
    pub get_driver_name: unsafe extern "system" fn(*mut c_void, *mut i8),
    pub get_driver_version: unsafe extern "system" fn(*mut c_void) -> i32,
    pub get_error_message: unsafe extern "system" fn(*mut c_void, *mut i8),
    pub start: unsafe extern "system" fn(*mut c_void) -> ASIOError,
    pub stop: unsafe extern "system" fn(*mut c_void) -> ASIOError,
    pub get_channels: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32) -> ASIOError,
    pub get_latencies: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32) -> ASIOError,
    pub get_buffer_size:
        unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32, *mut i32, *mut i32) -> ASIOError,
    pub can_sample_rate: unsafe extern "system" fn(*mut c_void, ASIOSampleRate) -> ASIOError,
    pub get_sample_rate: unsafe extern "system" fn(*mut c_void, *mut ASIOSampleRate) -> ASIOError,
    pub set_sample_rate: unsafe extern "system" fn(*mut c_void, ASIOSampleRate) -> ASIOError,
    pub get_clock_sources:
        unsafe extern "system" fn(*mut c_void, *mut ASIOClockSource, *mut i32) -> ASIOError,
    pub set_clock_source: unsafe extern "system" fn(*mut c_void, i32) -> ASIOError,
    pub get_sample_position:
        unsafe extern "system" fn(*mut c_void, *mut ASIOSamples, *mut ASIOTimeStamp) -> ASIOError,
    pub get_block_size: unsafe extern "system" fn(*mut c_void, *mut i32) -> ASIOError,
    pub create_buffers: unsafe extern "system" fn(
        *mut c_void,
        *mut ASIOBufferInfo,
        i32,
        i32,
        *mut ASIOCallbacks,
    ) -> ASIOError,
    pub dispose_buffers: unsafe extern "system" fn(*mut c_void) -> ASIOError,
    pub control_panel: unsafe extern "system" fn(*mut c_void) -> ASIOError,
    pub future: unsafe extern "system" fn(*mut c_void, i32, *mut c_void) -> ASIOError,
    pub output_ready: unsafe extern "system" fn(*mut c_void) -> ASIOError,
}

/// The COM class factory a host obtains with `DllGetClassObject`.
#[repr(C)]
pub struct IClassFactoryVtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: RefFn,
    pub release: RefFn,
    pub create_instance:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> i32,
    pub lock_server: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}
