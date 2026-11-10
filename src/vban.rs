//! VBAN (VB-Audio Network) audio transport.
//!
//! This implements the base VBAN audio protocol from the official
//! specification: 28-byte header, PCM/float audio, one UDP listen port, and a
//! small per-stream jitter/reorder buffer. Proprietary VBCA/VBCV codecs are
//! deliberately rejected.

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::InputShared;

pub const DEFAULT_PORT: u16 = 6980;
pub const HEADER_LEN: usize = 28;
pub const MAX_PACKET: usize = 1464;
pub const MAX_DATA: usize = 1436;
pub const MAX_SAMPLES: usize = 256;

const SR_TABLE: [u32; 21] = [
    6_000, 12_000, 24_000, 48_000, 96_000, 192_000, 384_000, 8_000, 16_000, 32_000, 64_000,
    128_000, 256_000, 512_000, 11_025, 22_050, 44_100, 88_200, 176_400, 352_800, 705_600,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VbanFormat {
    Pcm8,
    Pcm16,
    Pcm24,
    Pcm32,
    Float32,
    Float64,
}

impl VbanFormat {
    pub fn code(self) -> u8 {
        match self {
            Self::Pcm8 => 0,
            Self::Pcm16 => 1,
            Self::Pcm24 => 2,
            Self::Pcm32 => 3,
            Self::Float32 => 4,
            Self::Float64 => 5,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code & 0x07 {
            0 => Self::Pcm8,
            1 => Self::Pcm16,
            2 => Self::Pcm24,
            3 => Self::Pcm32,
            4 => Self::Float32,
            5 => Self::Float64,
            _ => return None,
        })
    }

    pub fn bytes(self) -> usize {
        match self {
            Self::Pcm8 => 1,
            Self::Pcm16 => 2,
            Self::Pcm24 => 3,
            Self::Pcm32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pcm8 => "PCM 8-bit",
            Self::Pcm16 => "PCM 16-bit",
            Self::Pcm24 => "PCM 24-bit",
            Self::Pcm32 => "PCM 32-bit",
            Self::Float32 => "Float 32-bit",
            Self::Float64 => "Float 64-bit",
        }
    }
}

#[derive(Clone, Debug)]
pub struct VbanAudio {
    pub sample_rate: u32,
    pub samples: usize,
    pub channels: usize,
    pub format: VbanFormat,
    pub stream_name: String,
    pub frame: u32,
}

pub fn sample_rate_for_index(index: u8) -> Option<u32> {
    SR_TABLE.get(index as usize).copied()
}

pub fn index_for_sample_rate(rate: u32) -> Option<u8> {
    SR_TABLE.iter().position(|&r| r == rate).map(|i| i as u8)
}

/// Parse a VBAN packet and return its audio header and payload.
pub fn parse_audio(packet: &[u8]) -> Result<(VbanAudio, &[u8]), String> {
    if packet.len() < HEADER_LEN {
        return Err("VBAN packet is shorter than the 28-byte header".to_string());
    }
    if &packet[0..4] != b"VBAN" {
        return Err("not a VBAN packet".to_string());
    }
    let format_sr = packet[4];
    if format_sr & 0xE0 != 0 {
        return Err("not a VBAN audio packet".to_string());
    }
    let sr_index = format_sr & 0x1F;
    let sample_rate = sample_rate_for_index(sr_index)
        .ok_or_else(|| format!("undefined VBAN sample-rate index {sr_index}"))?;
    let samples = packet[5] as usize + 1;
    let channels = packet[6] as usize + 1;
    let format_bit = packet[7];
    if format_bit & 0x08 != 0 {
        return Err("VBAN reserved header bit is set".to_string());
    }
    if format_bit & 0xF0 != 0 {
        return Err("unsupported VBAN codec (only PCM/float audio is supported)".to_string());
    }
    let format = VbanFormat::from_code(format_bit & 0x07)
        .ok_or_else(|| "unsupported VBAN sample format".to_string())?;
    let name_bytes = &packet[8..24];
    let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
    let stream_name = String::from_utf8_lossy(&name_bytes[..end])
        .trim()
        .to_string();
    let frame = u32::from_le_bytes([packet[24], packet[25], packet[26], packet[27]]);
    let data = &packet[HEADER_LEN..];
    let expected = samples
        .saturating_mul(channels)
        .saturating_mul(format.bytes());
    if data.len() != expected {
        return Err(format!(
            "VBAN payload is {} bytes, expected {expected}",
            data.len()
        ));
    }
    Ok((
        VbanAudio {
            sample_rate,
            samples,
            channels,
            format,
            stream_name,
            frame,
        },
        data,
    ))
}

pub fn make_header(
    sample_rate: u32,
    samples: usize,
    channels: usize,
    format: VbanFormat,
    stream_name: &str,
    frame: u32,
) -> Result<[u8; HEADER_LEN], String> {
    let sr_index = index_for_sample_rate(sample_rate)
        .ok_or_else(|| format!("VBAN does not support {sample_rate} Hz"))?;
    if samples == 0 || samples > MAX_SAMPLES {
        return Err("VBAN supports 1 to 256 samples per packet".to_string());
    }
    if channels == 0 || channels > 256 {
        return Err("invalid VBAN channel count".to_string());
    }
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(b"VBAN");
    header[4] = sr_index;
    header[5] = (samples - 1) as u8;
    header[6] = (channels - 1) as u8;
    header[7] = format.code();
    let name = stream_name.as_bytes();
    let take = name.len().min(16);
    header[8..8 + take].copy_from_slice(&name[..take]);
    header[24..28].copy_from_slice(&frame.to_le_bytes());
    Ok(header)
}

/// Decode a VBAN audio payload to stereo f32 frames. Mono is duplicated;
/// channels above two use the first L/R pair (the mixer strips are stereo).
pub fn decode_stereo(audio: &VbanAudio, data: &[u8]) -> Vec<[f32; 2]> {
    let bytes = audio.format.bytes();
    let frames = audio.samples;
    let channels = audio.channels.max(1);
    let mut out = Vec::with_capacity(frames);
    for frame in 0..frames {
        let read = |channel: usize| -> f32 {
            let offset = (frame * channels + channel) * bytes;
            match audio.format {
                VbanFormat::Pcm8 => {
                    let v = data[offset] as f32;
                    (v - 128.0) / 128.0
                }
                VbanFormat::Pcm16 => {
                    let v = i16::from_le_bytes([data[offset], data[offset + 1]]);
                    v as f32 / 32768.0
                }
                VbanFormat::Pcm24 => {
                    let raw = [
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        if data[offset + 2] & 0x80 != 0 {
                            0xFF
                        } else {
                            0
                        },
                    ];
                    i32::from_le_bytes(raw) as f32 / 8_388_608.0
                }
                VbanFormat::Pcm32 => {
                    let raw = [
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ];
                    i32::from_le_bytes(raw) as f32 / 2_147_483_648.0
                }
                VbanFormat::Float32 => {
                    let raw = [
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ];
                    f32::from_le_bytes(raw)
                }
                VbanFormat::Float64 => {
                    let raw = [
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                        data[offset + 4],
                        data[offset + 5],
                        data[offset + 6],
                        data[offset + 7],
                    ];
                    f64::from_le_bytes(raw) as f32
                }
            }
        };
        let l = read(0);
        let r = if channels == 1 { l } else { read(1) };
        out.push([l, r]);
    }
    out
}

/// Encode interleaved stereo f32 frames in the requested VBAN format.
pub fn encode_stereo(out: &mut Vec<u8>, left: &[f32], right: &[f32], format: VbanFormat) {
    let frames = left.len().min(right.len());
    out.clear();
    let write = |out: &mut Vec<u8>, value: f32| {
        let v = value.clamp(-1.0, 1.0);
        match format {
            VbanFormat::Pcm8 => out.push(((v * 127.0) + 128.0) as u8),
            VbanFormat::Pcm16 => {
                out.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes())
            }
            VbanFormat::Pcm24 => {
                let raw = ((v * 8_388_607.0) as i32).to_le_bytes();
                out.extend_from_slice(&raw[..3]);
            }
            VbanFormat::Pcm32 => {
                out.extend_from_slice(&((v * i32::MAX as f32) as i32).to_le_bytes())
            }
            VbanFormat::Float32 => out.extend_from_slice(&v.to_le_bytes()),
            VbanFormat::Float64 => out.extend_from_slice(&(v as f64).to_le_bytes()),
        }
    };
    for i in 0..frames {
        write(out, left[i]);
        write(out, right[i]);
    }
}

pub fn packet_frames(format: VbanFormat, channels: usize) -> usize {
    let channels = channels.max(1);
    (MAX_DATA / (channels * format.bytes())).clamp(1, MAX_SAMPLES)
}

fn quality_delay(quality: u8) -> Duration {
    let ms = match quality.min(4) {
        0 => 30,
        1 => 50,
        2 => 80,
        3 => 150,
        _ => 250,
    };
    Duration::from_millis(ms)
}

#[derive(Clone, Debug)]
pub struct DiscoveredStream {
    pub key: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub sample_rate: u32,
    pub channels: usize,
    pub format: VbanFormat,
    pub last_seen: Instant,
    pub packets: u64,
}

#[derive(Clone, Debug)]
pub struct BindingInfo {
    pub strip: usize,
    pub name: String,
    pub ip: String,
    pub sample_rate: u32,
    pub packets: u64,
    pub lost: u64,
    pub duplicates: u64,
    pub waiting: bool,
}

#[derive(Clone, Debug)]
pub struct VbanStatus {
    pub running: bool,
    pub port: u16,
    pub error: Option<String>,
    pub streams: usize,
    pub bindings: usize,
    pub senders: usize,
}

#[derive(Clone, Debug)]
pub struct SenderInfo {
    pub bus: usize,
    pub name: String,
    pub target: String,
    pub sample_rate: u32,
    pub packets: u64,
    pub errors: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VbanOutConfig {
    pub bus: usize,
    pub name: String,
    pub target: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub format: VbanFormat,
    pub quality: u8,
}

impl Default for VbanOutConfig {
    fn default() -> Self {
        Self {
            bus: 0,
            name: "Audio Mixer PRO".to_string(),
            target: "192.168.1.100:6980".to_string(),
            sample_rate: 48_000,
            channels: 2,
            format: VbanFormat::Float32,
            quality: 1,
        }
    }
}

/// Bounded stereo ring fed from the A1 callback and drained by a sender.
pub struct TxRing {
    inner: Mutex<VecDeque<f32>>,
    capacity_frames: usize,
}

impl TxRing {
    fn new(capacity_frames: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity_frames.max(1) * 2)),
            capacity_frames: capacity_frames.max(1),
        }
    }

    pub fn push_block(&self, left: &[f32], right: &[f32], frames: usize) {
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return;
        }
        let mut ring = self.inner.lock().unwrap();
        let max_samples = self.capacity_frames * 2;
        let incoming = frames * 2;
        if ring.len() + incoming > max_samples {
            let drop_samples = (ring.len() + incoming - max_samples).min(ring.len());
            ring.drain(..drop_samples);
        }
        for i in 0..frames {
            ring.push_back(left[i]);
            ring.push_back(right[i]);
        }
    }

    fn pop_frame(&self) -> Option<(f32, f32)> {
        let mut ring = self.inner.lock().unwrap();
        let l = ring.pop_front()?;
        let r = ring.pop_front().unwrap_or(l);
        Some((l, r))
    }
}

struct TxReader {
    prev: (f32, f32),
    cur: (f32, f32),
    phase: f32,
    ready: bool,
}

impl TxReader {
    fn new() -> Self {
        Self {
            prev: (0.0, 0.0),
            cur: (0.0, 0.0),
            phase: 0.0,
            ready: false,
        }
    }

    fn next(&mut self, ring: &TxRing, source_rate: u32, target_rate: u32) -> (f32, f32) {
        if !self.ready {
            if let Some(frame) = ring.pop_frame() {
                self.prev = frame;
                self.cur = frame;
                self.ready = true;
            }
            return self.cur;
        }
        let ratio = source_rate.max(1) as f32 / target_rate.max(1) as f32;
        self.phase += ratio;
        while self.phase >= 1.0 {
            if let Some(frame) = ring.pop_frame() {
                self.prev = self.cur;
                self.cur = frame;
            }
            self.phase -= 1.0;
        }
        let t = self.phase.clamp(0.0, 1.0);
        (
            self.prev.0 + (self.cur.0 - self.prev.0) * t,
            self.prev.1 + (self.cur.1 - self.prev.1) * t,
        )
    }
}

struct SenderStats {
    packets: AtomicU64,
    errors: AtomicU64,
    last_error: Mutex<Option<String>>,
}

struct VbanSender {
    bus: usize,
    local_port: u16,
    tx: Arc<TxRing>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    config: VbanOutConfig,
    stats: Arc<SenderStats>,
}

impl VbanSender {
    fn start(config: VbanOutConfig, source_rate: u32) -> Result<Self, String> {
        let target: SocketAddr = config
            .target
            .to_socket_addrs()
            .map_err(|err| format!("invalid VBAN target {}: {err}", config.target))?
            .next()
            .ok_or_else(|| format!("invalid VBAN target {}", config.target))?;
        let socket = UdpSocket::bind(("0.0.0.0", 0))
            .map_err(|err| format!("could not open VBAN sender: {err}"))?;
        let local_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
        // Harmless on unicast; needed for broadcast senders.
        let _ = socket.set_broadcast(true);
        // Mixer buses are stereo; keep the packet header and payload in sync.
        let channels = 2usize;
        let format = config.format;
        let packet_samples = packet_frames(format, channels);
        let interval =
            Duration::from_secs_f64(packet_samples as f64 / config.sample_rate.max(1) as f64);
        let capacity_frames =
            (source_rate.max(1) as u64 * quality_delay(config.quality).as_millis() as u64 / 1000)
                .max(packet_samples as u64) as usize;
        let tx = Arc::new(TxRing::new(capacity_frames));
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(SenderStats {
            packets: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_error: Mutex::new(None),
        });
        let thread_tx = tx.clone();
        let thread_stop = stop.clone();
        let thread_stats = stats.clone();
        let thread_config = config.clone();
        let name = if config.name.trim().is_empty() {
            "Audio Mixer PRO".to_string()
        } else {
            config.name.clone()
        };
        let thread = thread::Builder::new()
            .name(format!("vban-tx-{}", config.bus))
            .spawn(move || {
                let mut reader = TxReader::new();
                let mut left = vec![0.0f32; packet_samples];
                let mut right = vec![0.0f32; packet_samples];
                let mut payload = Vec::with_capacity(MAX_DATA);
                let mut frame_counter = 0u32;
                let mut deadline = Instant::now();
                let pacer = Pacer::new();
                while !thread_stop.load(Ordering::Relaxed) {
                    for i in 0..packet_samples {
                        let (l, r) = reader.next(&thread_tx, source_rate, config.sample_rate);
                        left[i] = l;
                        right[i] = r;
                    }
                    encode_stereo(&mut payload, &left, &right, format);
                    let header = match make_header(
                        thread_config.sample_rate,
                        packet_samples,
                        channels,
                        format,
                        &name,
                        frame_counter,
                    ) {
                        Ok(header) => header,
                        Err(err) => {
                            *thread_stats.last_error.lock().unwrap() = Some(err);
                            thread_stats.errors.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    };
                    let mut packet = Vec::with_capacity(HEADER_LEN + payload.len());
                    packet.extend_from_slice(&header);
                    packet.extend_from_slice(&payload);
                    match socket.send_to(&packet, target) {
                        Ok(_) => {
                            thread_stats.packets.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(err) => {
                            thread_stats.errors.fetch_add(1, Ordering::Relaxed);
                            *thread_stats.last_error.lock().unwrap() = Some(err.to_string());
                        }
                    }
                    frame_counter = frame_counter.wrapping_add(1);
                    deadline += interval;
                    let now = Instant::now();
                    if deadline < now {
                        deadline = now + interval;
                    }
                    if let Some(pacer) = &pacer {
                        pacer.wait(deadline.saturating_duration_since(Instant::now()));
                    } else {
                        wait_until(deadline);
                    }
                }
            })
            .map_err(|err| format!("could not start VBAN sender: {err}"))?;
        Ok(Self {
            bus: config.bus.min(4),
            local_port,
            tx,
            stop,
            thread: Some(thread),
            config,
            stats,
        })
    }

    fn info(&self) -> SenderInfo {
        SenderInfo {
            bus: self.bus,
            name: self.config.name.clone(),
            target: self.config.target.clone(),
            sample_rate: self.config.sample_rate,
            packets: self.stats.packets.load(Ordering::Relaxed),
            errors: self.stats.errors.load(Ordering::Relaxed),
            last_error: self.stats.last_error.lock().unwrap().clone(),
        }
    }
}

impl Drop for VbanSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
struct Pacer {
    timer: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Pacer {
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Threading::{CreateWaitableTimerExW, TIMER_ALL_ACCESS};
        let timer = unsafe {
            CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0, TIMER_ALL_ACCESS)
        };
        if timer.is_null() || timer == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            None
        } else {
            Some(Self { timer })
        }
    }

    fn wait(&self, delay: Duration) {
        use windows_sys::Win32::System::Threading::{
            SetWaitableTimer, WaitForSingleObject, INFINITE,
        };
        let due: i64 = -((delay.as_nanos() as i64 / 100).max(1));
        unsafe {
            SetWaitableTimer(self.timer, &due, 0, None, std::ptr::null(), 0);
            WaitForSingleObject(self.timer, INFINITE);
        }
    }
}

#[cfg(windows)]
impl Drop for Pacer {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.timer);
        }
    }
}

#[cfg(not(windows))]
struct Pacer;

#[cfg(not(windows))]
impl Pacer {
    fn new() -> Option<Self> {
        None
    }

    fn wait(&self, _delay: Duration) {}
}

fn wait_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        if remaining > Duration::from_millis(2) {
            thread::sleep(remaining - Duration::from_millis(2));
        } else {
            std::hint::spin_loop();
        }
    }
}

struct Binding {
    strip: usize,
    name: String,
    ip: String,
    quality: u8,
    sink: Arc<InputShared>,
    expected: Option<u32>,
    pending: BTreeMap<u32, (Instant, Vec<[f32; 2]>)>,
    last_packet: Instant,
    packets: u64,
    lost: u64,
    duplicates: u64,
    sample_rate: u32,
}

impl Binding {
    fn matches(&self, name: &str, ip: &IpAddr) -> bool {
        self.name.eq_ignore_ascii_case(name) && (self.ip.is_empty() || self.ip == ip.to_string())
    }

    fn flush(&mut self, now: Instant) {
        let delay = quality_delay(self.quality);
        loop {
            let Some((&frame, (arrived, data))) = self.pending.iter().next() else {
                break;
            };
            if now.duration_since(*arrived) < delay {
                break;
            }
            let data = data.clone();
            if let Some(expected) = self.expected {
                let diff = frame.wrapping_sub(expected);
                if diff & 0x8000_0000 != 0 {
                    // Late packet / duplicate.
                    self.pending.remove(&frame);
                    self.duplicates += 1;
                    continue;
                }
                if frame != expected {
                    let lost = diff.min(256) as u64;
                    self.lost += lost;
                    if lost > 0 {
                        let silence = vec![[0.0f32, 0.0f32]; lost as usize];
                        self.sink.push_block(&silence);
                    }
                }
            }
            self.pending.remove(&frame);
            self.expected = Some(frame.wrapping_add(1));
            self.sink.push_block(&data);
            self.packets += 1;
            self.last_packet = now;
        }
        // Keep a pathological buffer bounded.
        while self.pending.len() > 256 {
            self.pending.pop_first();
            self.lost += 1;
        }
    }
}

struct HubState {
    discovered: BTreeMap<String, DiscoveredStream>,
    bindings: Vec<Binding>,
    error: Option<String>,
    /// Local source ports of our own outgoing streams. Their packets must not
    /// be discovered as incoming streams (localhost/broadcast would loop).
    ignore_ports: Vec<u16>,
    outgoing_names: Vec<String>,
}

struct Runtime {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    port: u16,
}

pub struct VbanHub {
    state: Arc<Mutex<HubState>>,
    runtime: Mutex<Option<Runtime>>,
    senders: Mutex<Vec<VbanSender>>,
}

impl VbanHub {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState {
                discovered: BTreeMap::new(),
                bindings: Vec::new(),
                error: None,
                ignore_ports: Vec::new(),
                outgoing_names: Vec::new(),
            })),
            runtime: Mutex::new(None),
            senders: Mutex::new(Vec::new()),
        }
    }

    pub fn status(&self) -> VbanStatus {
        let runtime = self.runtime.lock().unwrap();
        let state = self.state.lock().unwrap();
        VbanStatus {
            running: runtime
                .as_ref()
                .map(|r| !r.thread.is_finished())
                .unwrap_or(false),
            port: runtime.as_ref().map(|r| r.port).unwrap_or(DEFAULT_PORT),
            error: state.error.clone(),
            streams: state.discovered.len(),
            bindings: state.bindings.len(),
            senders: self.senders.lock().unwrap().len(),
        }
    }

    pub fn configure(&self, enabled: bool, port: u16) {
        let mut runtime = self.runtime.lock().unwrap();
        let same = runtime.as_ref().map(|r| r.port == port).unwrap_or(false);
        if !enabled {
            if let Some(runtime) = runtime.take() {
                runtime.stop.store(true, Ordering::Relaxed);
                let _ = runtime.thread.join();
            }
            return;
        }
        if same {
            return;
        }
        if let Some(runtime) = runtime.take() {
            runtime.stop.store(true, Ordering::Relaxed);
            let _ = runtime.thread.join();
        }
        match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(socket) => {
                let _ = socket.set_read_timeout(Some(Duration::from_millis(25)));
                self.state.lock().unwrap().error = None;
                let stop = Arc::new(AtomicBool::new(false));
                let state = self.state.clone();
                let thread_stop = stop.clone();
                match thread::Builder::new()
                    .name("vban-rx".to_string())
                    .spawn(move || listen(socket, state, thread_stop))
                {
                    Ok(thread) => {
                        *runtime = Some(Runtime { stop, thread, port });
                    }
                    Err(err) => {
                        self.state.lock().unwrap().error =
                            Some(format!("could not start VBAN listener: {err}"));
                    }
                }
            }
            Err(err) => {
                self.state.lock().unwrap().error =
                    Some(format!("could not bind VBAN UDP port {port}: {err}"));
            }
        }
    }

    pub fn discovered(&self) -> Vec<DiscoveredStream> {
        let mut streams: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .discovered
            .values()
            .cloned()
            .collect();
        streams.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        streams
    }

    pub fn bindings(&self) -> Vec<BindingInfo> {
        let now = Instant::now();
        self.state
            .lock()
            .unwrap()
            .bindings
            .iter()
            .map(|b| BindingInfo {
                strip: b.strip,
                name: b.name.clone(),
                ip: b.ip.clone(),
                sample_rate: b.sample_rate,
                packets: b.packets,
                lost: b.lost,
                duplicates: b.duplicates,
                waiting: now.duration_since(b.last_packet) > Duration::from_secs(3),
            })
            .collect()
    }

    pub fn new_sink(&self) -> Arc<InputShared> {
        Arc::new(InputShared::new())
    }

    pub fn bind(&self, strip: usize, name: &str, ip: &str, quality: u8, sink: Arc<InputShared>) {
        let mut state = self.state.lock().unwrap();
        state.bindings.retain(|b| b.strip != strip);
        state.bindings.push(Binding {
            strip,
            name: name.to_string(),
            ip: ip.to_string(),
            quality,
            sink,
            expected: None,
            pending: BTreeMap::new(),
            last_packet: Instant::now(),
            packets: 0,
            lost: 0,
            duplicates: 0,
            sample_rate: 0,
        });
    }

    pub fn unbind(&self, strip: usize) {
        self.state
            .lock()
            .unwrap()
            .bindings
            .retain(|b| b.strip != strip);
    }

    pub fn unbind_all(&self) {
        self.state.lock().unwrap().bindings.clear();
    }

    pub fn set_senders(&self, configs: Vec<VbanOutConfig>, source_rate: u32) -> Vec<String> {
        let mut senders = self.senders.lock().unwrap();
        senders.clear();
        if let Ok(mut state) = self.state.lock() {
            state.ignore_ports.clear();
            state.outgoing_names.clear();
        }
        let mut errors = Vec::new();
        for config in configs {
            let effective_name: String = if config.name.trim().is_empty() {
                "Audio Mixer PRO".chars().take(16).collect()
            } else {
                config.name.chars().take(16).collect()
            };
            match VbanSender::start(config, source_rate) {
                Ok(sender) => {
                    if let Ok(mut state) = self.state.lock() {
                        if sender.local_port != 0 {
                            state.ignore_ports.push(sender.local_port);
                        }
                        state.outgoing_names.push(effective_name);
                    }
                    senders.push(sender)
                }
                Err(err) => errors.push(err),
            }
        }
        errors
    }

    pub fn tx_targets(&self) -> Vec<(usize, Arc<TxRing>)> {
        self.senders
            .lock()
            .unwrap()
            .iter()
            .map(|s| (s.bus, s.tx.clone()))
            .collect()
    }

    pub fn sender_info(&self) -> Vec<SenderInfo> {
        self.senders
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.info())
            .collect()
    }
}

impl Drop for VbanHub {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.get_mut().ok().and_then(|r| r.take()) {
            runtime.stop.store(true, Ordering::Relaxed);
            let _ = runtime.thread.join();
        }
    }
}

fn listen(socket: UdpSocket, state: Arc<Mutex<HubState>>, stop: Arc<AtomicBool>) {
    let mut buffer = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buffer) {
            Ok((len, from)) => {
                handle_packet(&buffer[..len], from, &state);
                flush_bindings(&state);
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                flush_bindings(&state);
            }
            Err(err) => {
                state.lock().unwrap().error = Some(format!("VBAN receive error: {err}"));
                break;
            }
        }
    }
    flush_bindings(&state);
}

fn handle_packet(packet: &[u8], from: SocketAddr, state: &Arc<Mutex<HubState>>) {
    let Ok((audio, data)) = parse_audio(packet) else {
        return;
    };
    let now = Instant::now();
    let key = format!("{}|{}", audio.stream_name.to_ascii_lowercase(), from.ip());
    let mut guard = state.lock().unwrap();
    if guard.ignore_ports.contains(&from.port())
        && guard
            .outgoing_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&audio.stream_name))
    {
        return;
    }
    let entry = guard
        .discovered
        .entry(key.clone())
        .or_insert_with(|| DiscoveredStream {
            key,
            name: audio.stream_name.clone(),
            ip: from.ip().to_string(),
            port: from.port(),
            sample_rate: audio.sample_rate,
            channels: audio.channels,
            format: audio.format,
            last_seen: now,
            packets: 0,
        });
    entry.name = audio.stream_name.clone();
    entry.ip = from.ip().to_string();
    entry.port = from.port();
    entry.sample_rate = audio.sample_rate;
    entry.channels = audio.channels;
    entry.format = audio.format;
    entry.last_seen = now;
    entry.packets += 1;

    let decoded = decode_stereo(&audio, data);
    for binding in guard.bindings.iter_mut() {
        if !binding.matches(&audio.stream_name, &from.ip()) {
            continue;
        }
        binding.sample_rate = audio.sample_rate;
        binding.sink.set_rate(audio.sample_rate);
        binding.pending.insert(audio.frame, (now, decoded.clone()));
    }
    // Discovery entries stay visible for a short while after silence.
    guard
        .discovered
        .retain(|_, stream| now.duration_since(stream.last_seen) < Duration::from_secs(30));
}

/// End-to-end loopback check used by `--vban-selftest`.
pub fn vban_selftest() {
    let port = 46_980u16;
    let hub = VbanHub::new();
    hub.configure(true, port);
    let sink = hub.new_sink();
    hub.bind(0, "VBAN Self Test", "127.0.0.1", 1, sink.clone());

    // RX: send encoded packets from a plain socket so this covers a remote
    // sender and does not trip the self-loop guard.
    let tx = UdpSocket::bind(("127.0.0.1", 0)).expect("VBAN test sender");
    let frames = packet_frames(VbanFormat::Float32, 2);
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    for i in 0..frames {
        let value = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
        left[i] = value;
        right[i] = value;
    }
    let mut payload = Vec::new();
    encode_stereo(&mut payload, &left, &right, VbanFormat::Float32);
    for frame in 0..10u32 {
        let header = make_header(
            48_000,
            frames,
            2,
            VbanFormat::Float32,
            "VBAN Self Test",
            frame,
        )
        .expect("VBAN test header");
        let mut packet = header.to_vec();
        packet.extend_from_slice(&payload);
        let _ = tx.send_to(&packet, ("127.0.0.1", port));
        thread::sleep(Duration::from_millis(5));
    }
    let mut rx_peak = 0.0f32;
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(350) {
        thread::sleep(Duration::from_millis(10));
        rx_peak = rx_peak.max(sink.peak_value());
    }
    let found = hub
        .discovered()
        .iter()
        .any(|stream| stream.name == "VBAN Self Test");
    let rate = sink.rate_value();
    let rx_ok = found && rx_peak > 0.1 && rate == 48_000;

    // TX: receive one real packet from a VbanSender directly.
    let receiver = UdpSocket::bind(("127.0.0.1", 0)).expect("VBAN test receiver");
    receiver
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok();
    let target = receiver.local_addr().expect("VBAN test target");
    let config = VbanOutConfig {
        bus: 0,
        name: "VBAN TX Test".to_string(),
        target: target.to_string(),
        sample_rate: 48_000,
        channels: 2,
        format: VbanFormat::Float32,
        quality: 1,
    };
    let sender = VbanSender::start(config, 48_000).expect("VBAN test sender");
    let frames = 4_800usize;
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    for i in 0..frames {
        let value = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
        left[i] = value;
        right[i] = value;
    }
    sender.tx.push_block(&left, &right, frames);
    let mut buffer = [0u8; 2048];
    let mut tx_peak = 0.0f32;
    let mut tx_ok = false;
    for _ in 0..6 {
        let Ok((len, _)) = receiver.recv_from(&mut buffer) else {
            break;
        };
        if let Ok((audio, data)) = parse_audio(&buffer[..len]) {
            if audio.stream_name == "VBAN TX Test" {
                tx_ok = true;
                for frame in decode_stereo(&audio, data) {
                    tx_peak = tx_peak.max(frame[0].abs()).max(frame[1].abs());
                }
            }
        }
    }
    drop(sender);
    let tx_ok = tx_ok && tx_peak > 0.1;

    let pass = rx_ok && tx_ok;
    println!(
        "vban selftest: rx={rx_ok} (discovered={found}, peak={rx_peak:.4}, rate={rate}) tx={tx_ok} (peak={tx_peak:.4}) pass={pass}"
    );
    hub.configure(false, port);
}

fn flush_bindings(state: &Arc<Mutex<HubState>>) {
    let now = Instant::now();
    let mut guard = state.lock().unwrap();
    for binding in guard.bindings.iter_mut() {
        binding.flush(now);
    }
}
