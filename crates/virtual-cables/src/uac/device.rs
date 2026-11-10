use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::audio::{Resampler, Ring};

use super::descriptors::{
    clamp_rate, Descriptors, Mode, AUDIO_CHANNELS, CLOCK_SOURCE_ID, MAX_SAMPLE_RATE,
    MIN_SAMPLE_RATE,
};

pub const REQUEST_GET_STATUS: u8 = 0x00;
pub const REQUEST_CLEAR_FEATURE: u8 = 0x01;
pub const REQUEST_SET_FEATURE: u8 = 0x03;
pub const REQUEST_SET_ADDRESS: u8 = 0x05;
pub const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
pub const REQUEST_SET_DESCRIPTOR: u8 = 0x07;
pub const REQUEST_GET_CONFIGURATION: u8 = 0x08;
pub const REQUEST_SET_CONFIGURATION: u8 = 0x09;
pub const REQUEST_GET_INTERFACE: u8 = 0x0A;
pub const REQUEST_SET_INTERFACE: u8 = 0x0B;
pub const REQUEST_SYNCH_FRAME: u8 = 0x0C;

pub const AUDIO_SET_CUR: u8 = 0x01;
pub const AUDIO_GET_CUR: u8 = 0x81;

const UAC1_GET_MIN: u8 = 0x82;
const UAC1_GET_MAX: u8 = 0x83;
const UAC1_GET_RES: u8 = 0x84;

const UAC2_CUR: u8 = 0x01;
const UAC2_RANGE: u8 = 0x02;

const SELECTOR_SAMPLING_FREQ: u8 = 0x01;
const SELECTOR_CLOCK_VALIDITY: u8 = 0x02;
const SELECTOR_MUTE: u8 = 0x01;
const SELECTOR_VOLUME: u8 = 0x02;

const DEFAULT_SAMPLE_RATE: u32 = 48_000;

#[derive(Clone, Copy, Debug, Default)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

pub fn parse_setup(b: &[u8]) -> Option<SetupPacket> {
    if b.len() < 8 {
        return None;
    }
    Some(SetupPacket {
        request_type: b[0],
        request: b[1],
        value: u16::from_le_bytes([b[2], b[3]]),
        index: u16::from_le_bytes([b[4], b[5]]),
        length: u16::from_le_bytes([b[6], b[7]]),
    })
}

struct State {
    configuration: u8,
    alt_setting: [u8; 3],
    sample_rate: u32,
    mute: bool,
    volume: i16,
    bypass: bool,
    playback_rate: u32,
    capture_rate: u32,
}

pub struct Device {
    pub number: u8,
    pub bus_id: String,
    pub mode: Mode,
    pub descriptors: Descriptors,
    buffer: Ring,
    resampler: Resampler,
    /// PCM the mixer injects into the capture endpoint (B1/B2 virtual buses).
    /// Kept separate from the playback loopback so a bus feed cannot corrupt
    /// audio an application is playing to the same cable.
    inject: Ring,
    inject_active: AtomicBool,
    inject_format: AtomicUsize,
    max_rate: u32,
    state: Mutex<State>,
}

impl Device {
    pub fn new(number: u8, latency_ms: u32, mode: Mode, max_rate: u32) -> Result<Self, String> {
        let max_rate = clamp_rate(max_rate);
        let descriptors = Descriptors::new(number, mode, max_rate)?;
        let latency_ms = latency_ms.clamp(10, 2000);
        let bytes_per_second = if mode == Mode::Uac2 {
            MAX_SAMPLE_RATE * AUDIO_CHANNELS as u32 * 4
        } else {
            48_000 * 2 * 2
        };
        let ring_capacity = (bytes_per_second * latency_ms / 1000) as usize;
        let resampler_frames = (MAX_SAMPLE_RATE * latency_ms / 1000).max(256) as usize;
        Ok(Self {
            number,
            bus_id: format!("1-{number}"),
            mode,
            descriptors,
            buffer: Ring::new(ring_capacity),
            resampler: Resampler::new(resampler_frames, AUDIO_CHANNELS),
            inject: Ring::new(ring_capacity),
            inject_active: AtomicBool::new(false),
            inject_format: AtomicUsize::new(0),
            max_rate,
            state: Mutex::new(State {
                configuration: 0,
                alt_setting: [0; 3],
                sample_rate: DEFAULT_SAMPLE_RATE,
                mute: false,
                volume: 0,
                bypass: true,
                playback_rate: DEFAULT_SAMPLE_RATE,
                capture_rate: DEFAULT_SAMPLE_RATE,
            }),
        })
    }

    pub fn product(&self) -> &str {
        &self.descriptors.product
    }

    pub fn max_rate(&self) -> u32 {
        self.max_rate
    }

    pub fn high_speed(&self) -> bool {
        self.mode == Mode::Uac2
    }

    pub fn frame_interval(&self) -> std::time::Duration {
        if self.high_speed() {
            std::time::Duration::from_micros(125)
        } else {
            std::time::Duration::from_millis(1)
        }
    }

    pub fn handle_control(&self, setup: SetupPacket, out: &[u8]) -> (Vec<u8>, i32) {
        let mut state = self.state.lock().unwrap();
        if setup.request_type & 0x60 == 0 {
            self.handle_standard(&mut state, setup)
        } else if self.mode == Mode::Uac2 {
            self.handle_uac2_class(&mut state, setup, out)
        } else {
            self.handle_uac1_class(&mut state, setup, out)
        }
    }

    fn handle_standard(&self, state: &mut State, setup: SetupPacket) -> (Vec<u8>, i32) {
        match setup.request {
            REQUEST_GET_DESCRIPTOR => {
                let descriptor_type = (setup.value >> 8) as u8;
                let index = setup.value as u8;
                match self.descriptors.get_descriptor(descriptor_type, index) {
                    Some(value) => (truncate(value, setup.length), 0),
                    None => (Vec::new(), -32),
                }
            }
            REQUEST_SET_ADDRESS => (Vec::new(), 0),
            REQUEST_SET_CONFIGURATION => {
                let configuration = setup.value as u8;
                if configuration > 1 {
                    return (Vec::new(), -32);
                }
                state.configuration = configuration;
                state.alt_setting = [0; 3];
                self.refresh_audio_path(state);
                (Vec::new(), 0)
            }
            REQUEST_GET_CONFIGURATION => (truncate(vec![state.configuration], setup.length), 0),
            REQUEST_SET_INTERFACE => {
                let interface = setup.index as usize;
                let alt = setup.value as u8;
                let max_alt = if self.mode == Mode::Uac2 { 3 } else { 1 };
                if state.configuration != 1
                    || interface > 2
                    || alt > max_alt
                    || (interface == 0 && alt != 0)
                {
                    return (Vec::new(), -32);
                }
                state.alt_setting[interface] = alt;
                if interface == 1 || interface == 2 {
                    self.refresh_audio_path(state);
                }
                (Vec::new(), 0)
            }
            REQUEST_GET_INTERFACE => {
                let interface = setup.index as usize;
                if interface > 2 {
                    return (Vec::new(), -32);
                }
                (
                    truncate(vec![state.alt_setting[interface]], setup.length),
                    0,
                )
            }
            REQUEST_GET_STATUS => (truncate(vec![0, 0], setup.length), 0),
            REQUEST_SYNCH_FRAME => (truncate(vec![0, 0], setup.length), 0),
            REQUEST_CLEAR_FEATURE | REQUEST_SET_FEATURE => (Vec::new(), 0),
            _ => (Vec::new(), -32),
        }
    }

    fn handle_uac2_class(
        &self,
        state: &mut State,
        setup: SetupPacket,
        out: &[u8],
    ) -> (Vec<u8>, i32) {
        if setup.request_type & 0x1f != 0x01 {
            return (Vec::new(), -32);
        }
        let is_get = setup.request_type & 0x80 != 0;
        let entity = (setup.index >> 8) as u8;
        let interface = (setup.index & 0xff) as u8;
        if interface != 0 {
            return (Vec::new(), -32);
        }
        let selector = (setup.value >> 8) as u8;
        let channel = (setup.value & 0xff) as u8;

        if entity == CLOCK_SOURCE_ID {
            match selector {
                SELECTOR_SAMPLING_FREQ => match (setup.request, is_get) {
                    (UAC2_CUR, true) => (
                        truncate(state.sample_rate.to_le_bytes().to_vec(), setup.length),
                        0,
                    ),
                    (UAC2_CUR, false) => {
                        if out.len() < 4 {
                            return (Vec::new(), 0);
                        }
                        let rate = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
                        if rate < MIN_SAMPLE_RATE || rate > self.max_rate {
                            return (Vec::new(), -32);
                        }
                        state.sample_rate = rate;
                        (Vec::new(), 0)
                    }
                    (UAC2_RANGE, true) => {
                        let mut payload = vec![1, 0];
                        payload.extend_from_slice(&MIN_SAMPLE_RATE.to_le_bytes());
                        payload.extend_from_slice(&self.max_rate.to_le_bytes());
                        payload.extend_from_slice(&1u32.to_le_bytes());
                        (truncate(payload, setup.length), 0)
                    }
                    _ => (Vec::new(), -32),
                },
                SELECTOR_CLOCK_VALIDITY => {
                    if setup.request == UAC2_CUR && is_get {
                        (truncate(vec![1], setup.length), 0)
                    } else {
                        (Vec::new(), -32)
                    }
                }
                _ => (Vec::new(), -32),
            }
        } else if (entity == 2 || entity == 5) && channel <= AUDIO_CHANNELS as u8 {
            match selector {
                SELECTOR_MUTE => match (setup.request, is_get) {
                    (UAC2_CUR, true) => (truncate(vec![u8::from(state.mute)], setup.length), 0),
                    (UAC2_CUR, false) => {
                        if let Some(value) = out.first() {
                            state.mute = *value != 0;
                        }
                        (Vec::new(), 0)
                    }
                    _ => (Vec::new(), -32),
                },
                SELECTOR_VOLUME => match (setup.request, is_get) {
                    (UAC2_CUR, true) => (
                        truncate(state.volume.to_le_bytes().to_vec(), setup.length),
                        0,
                    ),
                    (UAC2_CUR, false) => {
                        if out.len() >= 2 {
                            state.volume = i16::from_le_bytes([out[0], out[1]]);
                        }
                        (Vec::new(), 0)
                    }
                    (UAC2_RANGE, true) => {
                        let mut payload = vec![1, 0];
                        payload.extend_from_slice(&(-60i16 * 256).to_le_bytes());
                        payload.extend_from_slice(&0i16.to_le_bytes());
                        payload.extend_from_slice(&256i16.to_le_bytes());
                        (truncate(payload, setup.length), 0)
                    }
                    _ => (Vec::new(), -32),
                },
                _ => (Vec::new(), -32),
            }
        } else {
            (Vec::new(), -32)
        }
    }

    fn handle_uac1_class(
        &self,
        state: &mut State,
        setup: SetupPacket,
        out: &[u8],
    ) -> (Vec<u8>, i32) {
        let recipient = setup.request_type & 0x1f;
        let selector = (setup.value >> 8) as u8;
        let endpoint = setup.index as u8;
        if recipient == 0x02
            && selector == SELECTOR_SAMPLING_FREQ
            && (endpoint == 0x01 || endpoint == 0x82)
        {
            match setup.request {
                AUDIO_SET_CUR => {
                    if out.len() >= 3 {
                        let rate = out[0] as u32 | (out[1] as u32) << 8 | (out[2] as u32) << 16;
                        if rate != 48_000 {
                            return (Vec::new(), -32);
                        }
                        state.sample_rate = rate;
                    }
                    return (Vec::new(), 0);
                }
                AUDIO_GET_CUR => {
                    return (truncate(rate24(state.sample_rate), setup.length), 0);
                }
                UAC1_GET_MIN | UAC1_GET_MAX => {
                    return (truncate(rate24(48_000), setup.length), 0);
                }
                UAC1_GET_RES => return (truncate(rate24(1), setup.length), 0),
                _ => {}
            }
        }

        let entity = (setup.index >> 8) as u8;
        let interface = setup.index as u8;
        if recipient == 0x01 && interface == 0 && (entity == 2 || entity == 5) {
            match selector {
                SELECTOR_MUTE => match setup.request {
                    AUDIO_SET_CUR => {
                        if let Some(value) = out.first() {
                            state.mute = *value != 0;
                        }
                        return (Vec::new(), 0);
                    }
                    AUDIO_GET_CUR => {
                        return (truncate(vec![u8::from(state.mute)], setup.length), 0);
                    }
                    _ => {}
                },
                SELECTOR_VOLUME => match setup.request {
                    AUDIO_SET_CUR => {
                        if out.len() >= 2 {
                            state.volume = i16::from_le_bytes([out[0], out[1]]);
                        }
                        return (Vec::new(), 0);
                    }
                    AUDIO_GET_CUR => {
                        return (
                            truncate(state.volume.to_le_bytes().to_vec(), setup.length),
                            0,
                        );
                    }
                    UAC1_GET_MIN => {
                        return (
                            truncate((-60i16 * 256).to_le_bytes().to_vec(), setup.length),
                            0,
                        );
                    }
                    UAC1_GET_MAX => {
                        return (truncate(0i16.to_le_bytes().to_vec(), setup.length), 0);
                    }
                    UAC1_GET_RES => {
                        return (truncate(256i16.to_le_bytes().to_vec(), setup.length), 0);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        (Vec::new(), -32)
    }

    pub fn playback_active(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.configuration == 1 && state.alt_setting[1] > 0
    }

    pub fn capture_active(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.configuration == 1 && state.alt_setting[2] > 0
    }

    pub fn playback_frame_bytes(&self) -> usize {
        let state = self.state.lock().unwrap();
        bytes_per_sample_for_alt(state.alt_setting[1]) * AUDIO_CHANNELS
    }

    pub fn capture_frame_bytes(&self) -> usize {
        let state = self.state.lock().unwrap();
        bytes_per_sample_for_alt(state.alt_setting[2]) * AUDIO_CHANNELS
    }

    pub fn set_playback_rate(&self, rate: u32) {
        if rate == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.playback_rate = rate;
        self.refresh_audio_path(&mut state);
    }

    pub fn set_capture_rate(&self, rate: u32) {
        if rate == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.capture_rate = rate;
        self.refresh_audio_path(&mut state);
    }

    fn refresh_audio_path(&self, state: &mut State) {
        let playback_bytes = bytes_per_sample_for_alt(state.alt_setting[1]);
        let capture_bytes = bytes_per_sample_for_alt(state.alt_setting[2]);
        if playback_bytes == 0
            || capture_bytes == 0
            || (playback_bytes == capture_bytes && state.playback_rate == state.capture_rate)
        {
            if !state.bypass {
                self.buffer.reset();
            }
            state.bypass = true;
            return;
        }
        let changed = self.resampler.configure(
            state.playback_rate,
            state.capture_rate,
            playback_bytes,
            capture_bytes,
        );
        if state.bypass || changed {
            self.resampler.reset();
            self.buffer.reset();
        }
        state.bypass = false;
    }

    pub fn write_playback(&self, p: &[u8]) -> usize {
        let (active, bypass) = {
            let state = self.state.lock().unwrap();
            (
                state.configuration == 1 && state.alt_setting[1] > 0,
                state.bypass,
            )
        };
        if !active {
            return 0;
        }
        if bypass {
            self.buffer.write(p)
        } else {
            self.resampler.write(p)
        }
    }

    pub fn read_capture(&self, p: &mut [u8]) -> usize {
        let (active, bypass) = {
            let state = self.state.lock().unwrap();
            (
                state.configuration == 1 && state.alt_setting[2] > 0,
                state.bypass,
            )
        };
        if !active {
            self.inject_active.store(false, Ordering::Relaxed);
            self.inject.reset();
            p.fill(0);
            return 0;
        }
        if self.inject_active.load(Ordering::Relaxed) {
            let frame_bytes = {
                let state = self.state.lock().unwrap();
                bytes_per_sample_for_alt(state.alt_setting[2]) * AUDIO_CHANNELS
            };
            if self.inject_format.load(Ordering::Relaxed) == frame_bytes {
                return self.inject.read_silence(p);
            }
            self.inject_active.store(false, Ordering::Relaxed);
        }
        if bypass {
            self.buffer.read_silence(p)
        } else {
            self.resampler.read(p)
        }
    }

    /// Feed PCM in the capture endpoint's current format. The mixer uses this
    /// to send B1/B2 into a cable's capture side without opening a Windows
    /// playback stream. Returns 0 unless the capture interface is active.
    pub fn write_capture_pcm(&self, p: &[u8]) -> usize {
        let frame_bytes = {
            let state = self.state.lock().unwrap();
            if state.configuration != 1 || state.alt_setting[2] == 0 {
                return 0;
            }
            bytes_per_sample_for_alt(state.alt_setting[2]) * AUDIO_CHANNELS
        };
        if frame_bytes == 0 {
            return 0;
        }
        if self.inject_format.swap(frame_bytes, Ordering::Relaxed) != frame_bytes {
            self.inject.reset();
        }
        let written = self.inject.write(p);
        if written > 0 {
            self.inject_active.store(true, Ordering::Relaxed);
        }
        written
    }

    /// Rate Windows selected for the capture endpoint.
    pub fn capture_rate(&self) -> u32 {
        self.state.lock().unwrap().capture_rate
    }

    pub fn audio_status(&self) -> String {
        let (bypass, playback_rate, capture_rate, clock) = {
            let state = self.state.lock().unwrap();
            (
                state.bypass,
                state.playback_rate,
                state.capture_rate,
                state.sample_rate,
            )
        };
        if bypass {
            format!("path=bypass {playback_rate}/{capture_rate} clock={clock}")
        } else {
            let (available, dropped, underruns) = self.resampler.stats();
            format!(
                "path=resample {playback_rate}->{capture_rate} clock={clock} \
                 avail={available} dropped={dropped} underruns={underruns}"
            )
        }
    }
}

pub fn estimate_rate(bytes: u32, packets: u32, frame_bytes: usize, high_speed: bool) -> u32 {
    if bytes == 0 || packets == 0 || frame_bytes == 0 {
        return 0;
    }
    let packets_per_second = if high_speed { 8000.0 } else { 1000.0 };
    let rate =
        (bytes as f64 * packets_per_second / (packets as f64 * frame_bytes as f64)).round() as u32;
    if rate == 0 {
        return 0;
    }
    for standard in [
        8000u32, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
    ] {
        if (rate as f64 - standard as f64).abs() <= standard as f64 * 0.01 {
            return standard;
        }
    }
    rate
}

fn bytes_per_sample_for_alt(alt: u8) -> usize {
    match alt {
        1 => 2,
        2 => 3,
        3 => 4,
        _ => 0,
    }
}

fn truncate(mut value: Vec<u8>, length: u16) -> Vec<u8> {
    if (length as usize) < value.len() {
        value.truncate(length as usize);
    }
    value
}

fn rate24(rate: u32) -> Vec<u8> {
    vec![rate as u8, (rate >> 8) as u8, (rate >> 16) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uac::descriptors::{DESC_DEVICE, DESC_DEVICE_QUALIFIER};

    fn new_48k() -> Device {
        Device::new(1, 100, Mode::Uac2, 48_000).unwrap()
    }

    fn configure(device: &Device) {
        device.handle_control(
            SetupPacket {
                request_type: 0x00,
                request: REQUEST_SET_CONFIGURATION,
                value: 1,
                index: 0,
                length: 0,
            },
            &[],
        );
    }

    fn set_interface(device: &Device, interface: u16, alt: u16) {
        let (_, status) = device.handle_control(
            SetupPacket {
                request_type: 0x01,
                request: REQUEST_SET_INTERFACE,
                value: alt,
                index: interface,
                length: 0,
            },
            &[],
        );
        assert_eq!(status, 0);
    }

    #[test]
    fn descriptors_and_qualifier() {
        let device = new_48k();
        let (data, status) = device.handle_control(
            SetupPacket {
                request_type: 0x80,
                request: REQUEST_GET_DESCRIPTOR,
                value: (DESC_DEVICE as u16) << 8,
                index: 0,
                length: 18,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(data.len(), 18);
        let (qualifier, status) = device.handle_control(
            SetupPacket {
                request_type: 0x80,
                request: REQUEST_GET_DESCRIPTOR,
                value: (DESC_DEVICE_QUALIFIER as u16) << 8,
                index: 0,
                length: 10,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(qualifier.len(), 10);
    }

    #[test]
    fn set_and_get_interface() {
        let device = new_48k();
        configure(&device);
        set_interface(&device, 1, 3);
        let (data, status) = device.handle_control(
            SetupPacket {
                request_type: 0x81,
                request: REQUEST_GET_INTERFACE,
                value: 0,
                index: 1,
                length: 1,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(data, vec![3]);
    }

    #[test]
    fn clock_sampling_rate() {
        let device = new_48k();
        let (_, status) = device.handle_control(
            SetupPacket {
                request_type: 0x21,
                request: AUDIO_SET_CUR,
                value: 0x0100,
                index: 0x0A00,
                length: 4,
            },
            &48_000u32.to_le_bytes(),
        );
        assert_eq!(status, 0);
        let (data, status) = device.handle_control(
            SetupPacket {
                request_type: 0xA1,
                request: UAC2_CUR,
                value: 0x0100,
                index: 0x0A00,
                length: 4,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(
            u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            48_000
        );

        let (range, status) = device.handle_control(
            SetupPacket {
                request_type: 0xA1,
                request: UAC2_RANGE,
                value: 0x0100,
                index: 0x0A00,
                length: 256,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(range.len(), 14);
        assert_eq!(u16::from_le_bytes([range[0], range[1]]), 1);
        assert_eq!(
            u32::from_le_bytes([range[2], range[3], range[4], range[5]]),
            MIN_SAMPLE_RATE
        );
        assert_eq!(
            u32::from_le_bytes([range[6], range[7], range[8], range[9]]),
            48_000
        );
    }

    #[test]
    fn rejects_rate_above_max() {
        let device = new_48k();
        let (_, status) = device.handle_control(
            SetupPacket {
                request_type: 0x21,
                request: AUDIO_SET_CUR,
                value: 0x0100,
                index: 0x0A00,
                length: 4,
            },
            &192_000u32.to_le_bytes(),
        );
        assert_eq!(status, -32);
    }

    #[test]
    fn feature_unit_mute_and_volume() {
        let device = new_48k();
        let (_, status) = device.handle_control(
            SetupPacket {
                request_type: 0x21,
                request: AUDIO_SET_CUR,
                value: 0x0100,
                index: 0x0200,
                length: 1,
            },
            &[1],
        );
        assert_eq!(status, 0);
        let (data, status) = device.handle_control(
            SetupPacket {
                request_type: 0xA1,
                request: UAC2_CUR,
                value: 0x0100,
                index: 0x0200,
                length: 1,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(data, vec![1]);

        let (_, status) = device.handle_control(
            SetupPacket {
                request_type: 0x21,
                request: AUDIO_SET_CUR,
                value: 0x0201,
                index: 0x0200,
                length: 2,
            },
            &(-2560i16).to_le_bytes(),
        );
        assert_eq!(status, 0);
        let (data, status) = device.handle_control(
            SetupPacket {
                request_type: 0xA1,
                request: UAC2_CUR,
                value: 0x0201,
                index: 0x0200,
                length: 2,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(i16::from_le_bytes([data[0], data[1]]), -2560);
    }

    #[test]
    fn uac1_fallback_rate() {
        let device = Device::new(1, 100, Mode::Uac1, 48_000).unwrap();
        let (data, status) = device.handle_control(
            SetupPacket {
                request_type: 0xA2,
                request: AUDIO_GET_CUR,
                value: 0x0100,
                index: 0x82,
                length: 3,
            },
            &[],
        );
        assert_eq!(status, 0);
        assert_eq!(data, vec![0x80, 0xBB, 0x00]);
        let (_, status) = device.handle_control(
            SetupPacket {
                request_type: 0x22,
                request: AUDIO_SET_CUR,
                value: 0x0100,
                index: 0x01,
                length: 3,
            },
            &rate24(96_000),
        );
        assert_eq!(status, -32);
    }

    #[test]
    fn estimate_rates() {
        assert_eq!(estimate_rate(3840, 80, 8, true), 48_000);
        assert_eq!(estimate_rate(13824, 72, 8, true), 192_000);
        assert_eq!(estimate_rate(1920, 10, 4, false), 48_000);
    }

    #[test]
    fn bypass_is_bit_exact() {
        let device = new_48k();
        configure(&device);
        set_interface(&device, 1, 3);
        set_interface(&device, 2, 3);
        let frame = [1u8, 2, 3, 4, 5, 6, 7, 8];
        device.write_playback(&frame);
        let mut out = [0u8; 8];
        device.read_capture(&mut out);
        assert_eq!(out, frame);
    }

    #[test]
    fn resamples_mismatched_rates() {
        let device = Device::new(1, 100, Mode::Uac2, 192_000).unwrap();
        configure(&device);
        set_interface(&device, 1, 3);
        set_interface(&device, 2, 3);
        device.set_playback_rate(48_000);
        device.set_capture_rate(192_000);
        let frame = [0u8, 0, 0, 0x40, 0, 0, 0, 0x40];
        device.write_playback(&frame);
        device.write_playback(&frame);
        let mut out = [0u8; 32];
        assert_eq!(device.read_capture(&mut out), out.len());
        for chunk in out.chunks_exact(4) {
            let value = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            assert!((0x3F00_0000..=0x4100_0000).contains(&(value as u32)));
        }
    }

    #[test]
    fn capture_injection_is_bit_exact() {
        let device = new_48k();
        configure(&device);
        set_interface(&device, 2, 3);
        let frame = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(device.write_capture_pcm(&frame), frame.len());
        let mut out = [0u8; 8];
        device.read_capture(&mut out);
        assert_eq!(out, frame);
    }

    #[test]
    fn format_conversion_when_rates_match() {
        let device = Device::new(1, 100, Mode::Uac2, 48_000).unwrap();
        configure(&device);
        set_interface(&device, 1, 1);
        set_interface(&device, 2, 3);
        let frame = [0u8, 0x40, 0u8, 0x40];
        device.write_playback(&frame);
        device.write_playback(&frame);
        let mut out = [0u8; 16];
        assert_eq!(device.read_capture(&mut out), out.len());
        for chunk in out.chunks_exact(4) {
            let value = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            assert!((0x3F00_0000..=0x4100_0000).contains(&(value as u32)));
        }
    }
}
