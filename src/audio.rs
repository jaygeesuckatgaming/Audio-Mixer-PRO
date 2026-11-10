use crate::vban::TxRing;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;
use std::f64::consts::TAU;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use symphonia::core::audio::{SampleBuffer, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use virtual_cables::{Device, Mode};

pub const DEMO_LEN_SECS: f32 = 252.0;
/// Longest track this build will hold in memory. The original streams from
/// disk; this rebuild decodes to RAM, so it stops at 30 minutes with a clear
/// message instead of failing silently.
const MAX_TRACK_SECONDS: f64 = 30.0 * 60.0;
const MAX_WAV_SECONDS: f64 = MAX_TRACK_SECONDS;

const EQ_FREQS: [f32; 6] = [50.0, 200.0, 800.0, 2000.0, 8000.0, 12000.0];
const EQ_Q: f32 = 3.0;

/// Hardware strips plus the two base virtual strips. Additional virtual inputs
/// extend the console up to this many strips.
pub const BASE_STRIPS: usize = 5;
pub const MAX_STRIPS: usize = 16;

// ---------------------------------------------------------------------------
// A1 output format rules (FUN_00527ad0, StartAudioStream)
// ---------------------------------------------------------------------------

/// The only main-output sample rates the original accepts (0x527b.. checks).
pub const SAMPLE_RATES: [u32; 7] = [32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// FUN_00527ad0 rejects a main buffer above 0x800 samples.
pub const MAIN_BUFFER_MAX: u32 = 2048;

const A1_RATE_ERROR: &str = "Failed to Open Main Output Device (A1)\n\
Device already in use or not properly configured.\n\n\
(Output A1 supports standard SampleRate only)\n";
const A1_BUFFER_ERROR: &str =
    "Failed to Open Main Output Device (A1)\nBuffer size cannot be above 2048 samples.\n";
const A1_OPEN_ERROR: &str = "Failed to Open Main Output Device (A1)\n\
Device not ready or used by another process.\n\
Select A1 Output Device first !\n\n\
Keep default settings (default buffer size).\n\
Device configuration might be 32, 44.1, 48, 88.2 or 96kHz only\n";

#[derive(Clone, Copy)]
struct OutputFormat {
    sample_rate: u32,
    buffer_frames: u32,
}

// ---------------------------------------------------------------------------
// Demo material
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DemoKind {
    Track1,
    Track2,
    VoiceTake,
    Background,
}

impl DemoKind {
    pub const ALL: [DemoKind; 4] = [
        DemoKind::Track1,
        DemoKind::Track2,
        DemoKind::VoiceTake,
        DemoKind::Background,
    ];

    pub fn index(self) -> usize {
        match self {
            DemoKind::Track1 => 0,
            DemoKind::Track2 => 1,
            DemoKind::VoiceTake => 2,
            DemoKind::Background => 3,
        }
    }

    pub fn from_index(i: usize) -> Option<Self> {
        Self::ALL.get(i).copied()
    }

    pub fn file_name(self) -> &'static str {
        match self {
            DemoKind::Track1 => "Demo Track 01.wav",
            DemoKind::Track2 => "Demo Track 02.wav",
            DemoKind::VoiceTake => "Voice Take.wav",
            DemoKind::Background => "Background Music.wav",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            DemoKind::Track1 => "synth groove, 100 BPM",
            DemoKind::Track2 => "slow ambient pad",
            DemoKind::VoiceTake => "vowel lead with echo",
            DemoKind::Background => "light percussion and marimba",
        }
    }
}

fn db_to_lin(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn f32_atomic(v: f32) -> AtomicU32 {
    AtomicU32::new(v.to_bits())
}

fn load_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

fn lock_source<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn midi_hz(note: f64) -> f64 {
    440.0 * 2.0f64.powf((note - 69.0) / 12.0)
}

fn sine(t: f64, f: f64) -> f32 {
    if f <= 0.0 {
        0.0
    } else {
        (TAU * f * t).sin() as f32
    }
}

fn saw(t: f64, f: f64) -> f32 {
    if f <= 0.0 {
        0.0
    } else {
        let p = (t * f).fract();
        (2.0 * p - 1.0) as f32
    }
}

fn triangle(t: f64, f: f64) -> f32 {
    if f <= 0.0 {
        0.0
    } else {
        let p = (t * f).fract();
        (4.0 * (p - 0.5).abs() - 1.0) as f32
    }
}

fn noise_at(pos: u64, seed: u64) -> f32 {
    let mut x = pos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(seed)
        .wrapping_add(0x1234_5678_9ABC_DEF0);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x >> 40) as f32 / 8_388_608.0 - 1.0
}

fn exp_env(t: f64, rate: f64) -> f32 {
    if t < 0.0 {
        0.0
    } else {
        (-t * rate).exp() as f32
    }
}

fn soft_clip(x: f32) -> f32 {
    if x.is_finite() {
        x.tanh()
    } else {
        0.0
    }
}

fn pan_gains(p: f32) -> (f32, f32) {
    let p = p.clamp(-1.0, 1.0);
    let angle = (p + 1.0) * 0.25 * std::f32::consts::PI;
    (angle.cos(), angle.sin())
}

fn kick(t: f64, bpm: f64) -> f32 {
    let beat = t * bpm / 60.0;
    let idx = beat.floor();
    let since = (beat - idx) * 60.0 / bpm;
    if (idx as i64) & 1 == 0 && idx as i64 % 4 != 3 && since < 0.30 {
        let phase = 45.0 * since + (120.0 / 22.0) * (1.0 - (-22.0 * since).exp());
        let env = (-since * 15.0).exp() as f32;
        (TAU * phase).sin() as f32 * env
    } else {
        0.0
    }
}

fn snare(t: f64, bpm: f64, pos: u64) -> f32 {
    let beat = t * bpm / 60.0;
    let idx = beat.floor() as i64;
    let since = (beat - beat.floor()) * 60.0 / bpm;
    if idx & 1 == 1 && since < 0.20 {
        let body = sine(since, 190.0) * exp_env(since, 34.0) * 0.35;
        let rattle = noise_at(pos, 0x51A3) * exp_env(since, 22.0) * 0.75;
        body + rattle
    } else {
        0.0
    }
}

fn hat(t: f64, bpm: f64, pos: u64) -> f32 {
    let eighths = t * bpm / 60.0 * 2.0;
    let since = (eighths - eighths.floor()) * 30.0 / bpm;
    if since < 0.075 {
        // Alternating noise gives a brighter closed-hat character.
        let raw = noise_at(pos, 0xA17) - noise_at(pos.wrapping_sub(1), 0xA17) * 0.6;
        raw * exp_env(since, 95.0) * 0.18
    } else {
        0.0
    }
}

fn pluck(t: f64, freq: f64) -> f32 {
    if t < 0.0 {
        0.0
    } else {
        let env = exp_env(t, 4.2);
        (sine(t, freq) * 0.75 + sine(t, freq * 2.0) * 0.18 + triangle(t, freq) * 0.07) * env
    }
}

fn marimba(t: f64, freq: f64) -> f32 {
    if t < 0.0 {
        0.0
    } else {
        let env = exp_env(t, 7.0);
        (sine(t, freq) * 0.7 + sine(t, freq * 4.01) * 0.18 + sine(t, freq * 9.8) * 0.06) * env
    }
}

fn demo_track1(t: f64, pos: u64) -> (f32, f32) {
    let bpm = 100.0;
    let beat = t * bpm / 60.0;
    let bar = (beat / 4.0).floor() as i64;
    let chord_idx = bar.rem_euclid(4) as usize;
    let roots = [48.0, 45.0, 41.0, 43.0];
    let minor = [false, true, false, false];
    let root = roots[chord_idx];
    let third = if minor[chord_idx] { 3.0 } else { 4.0 };

    let kick_s = kick(t, bpm) * 0.62;
    let snare_s = snare(t, bpm, pos) * 0.23;
    let hat_s = hat(t, bpm, pos) * 0.11;

    // Bass follows the chord root, one note per beat.
    let beat_since = (beat - beat.floor()) * 60.0 / bpm;
    let bass_env = exp_env(beat_since, 4.5) * (1.0 - exp_env(beat_since, 60.0) * 0.0);
    let bass_f = midi_hz(root - 12.0);
    let bass = (sine(t, bass_f) * 0.82 + sine(t, bass_f * 2.0) * 0.16 + saw(t, bass_f) * 0.06)
        * bass_env
        * 0.26;

    // Chord pad, with a soft attack each bar.
    let bar_phase = (beat / 4.0).fract();
    let pad_env = ((bar_phase / 0.10).min(1.0) * ((1.0 - bar_phase) / 0.18).clamp(0.0, 1.0)) as f32;
    let mut pad_l = 0.0;
    let mut pad_r = 0.0;
    for (n, interval) in [0.0, third, 7.0, 12.0].iter().enumerate() {
        let f = midi_hz(root + interval);
        let detune = 0.0015 * (n as f64 + 1.0);
        let a = (sine(t, f) * 0.55 + saw(t, f * (1.0 + detune)) * 0.12) * 0.055 * pad_env;
        let b = (sine(t, f) * 0.55 + saw(t, f * (1.0 - detune)) * 0.12) * 0.055 * pad_env;
        pad_l += a;
        pad_r += b;
    }

    // Sixteenth-note arpeggio, alternating sides.
    let sixteenth = t * bpm / 60.0 * 4.0;
    let step = sixteenth.floor() as i64;
    let step_since = (sixteenth - sixteenth.floor()) * 15.0 / bpm;
    let arp_note = root + [0.0, third, 7.0, 12.0][step.rem_euclid(4) as usize];
    let arp = pluck(step_since, midi_hz(arp_note + 12.0)) * 0.085;
    let (alp, arp_r) = pan_gains(if step & 1 == 0 { -0.45 } else { 0.45 });

    let l = kick_s + snare_s + hat_s + bass + pad_l + arp * alp;
    let r = kick_s + snare_s + hat_s + bass + pad_r + arp * arp_r;
    (soft_clip(l * 1.05), soft_clip(r * 1.05))
}

fn demo_track2(t: f64, _pos: u64) -> (f32, f32) {
    let bpm = 72.0;
    let beat = t * bpm / 60.0;
    let bar = (beat / 4.0).floor() as i64;
    let chord_idx = bar.rem_euclid(4) as usize;
    let roots = [48.0, 45.0, 41.0, 43.0];
    let root = roots[chord_idx];

    // Slow four-note pad with a long attack and release.
    let bar_phase = (beat / 4.0).fract();
    let env = ((bar_phase / 0.28).min(1.0) * ((1.0 - bar_phase) / 0.34).clamp(0.0, 1.0)) as f32;
    let intervals = [0.0, 7.0, 12.0, 16.0];
    let mut l = 0.0;
    let mut r = 0.0;
    for (n, interval) in intervals.iter().enumerate() {
        let f = midi_hz(root + interval);
        let drift = 0.0018 * ((t * 0.11 + n as f64).sin() as f64);
        let a = (sine(t, f * (1.0 + drift)) * 0.7 + triangle(t, f * 0.5) * 0.08) * 0.085 * env;
        let b = (sine(t, f * (1.0 - drift)) * 0.7 + triangle(t, f * 0.5) * 0.08) * 0.085 * env;
        l += a;
        r += b;
    }

    // Sub bass on the chord root.
    let sub = sine(t, midi_hz(root - 24.0)) * (0.10 + 0.03 * (TAU * 0.09 * t).sin() as f32);

    // Sparse high sparkle.
    let eighth = t * bpm / 60.0 * 2.0;
    let step = eighth.floor() as i64;
    let since = (eighth - eighth.floor()) * 30.0 / bpm;
    let sparkle_note = root + [12.0, 19.0, 24.0, 16.0][step.rem_euclid(4) as usize];
    let sparkle = pluck(since, midi_hz(sparkle_note + 12.0)) * 0.05;
    let (sp_l, sp_r) = pan_gains(if step & 1 == 0 { -0.6 } else { 0.6 });

    (
        soft_clip((l + sub + sparkle * sp_l) * 1.2),
        soft_clip((r + sub + sparkle * sp_r) * 1.2),
    )
}

/// A vowel-like lead: a buzzy fundamental with three formant peaks.
fn voice_dry(t: f64, pos: u64) -> f32 {
    if t < 0.0 {
        return 0.0;
    }
    let phrase_len = 1.05;
    let phrase = (t / phrase_len).floor() as i64;
    let since = t - phrase as f64 * phrase_len;
    // Leave a breath between phrases.
    if since > 0.78 {
        return 0.0;
    }
    if phrase.rem_euclid(8) == 7 {
        return 0.0;
    }
    let melody = [57.0, 60.0, 64.0, 62.0, 60.0, 55.0, 59.0, 57.0];
    let note = melody[phrase.rem_euclid(7) as usize];
    let vib = 1.0 + 0.006 * (TAU * 5.4 * t).sin();
    let f0 = midi_hz(note) * vib;
    let env = ((since / 0.055).min(1.0) * ((0.72 - since) / 0.14).clamp(0.0, 1.0)) as f32;
    let buzz = saw(t, f0) * 0.42 + sine(t, f0) * 0.48;

    let formants = [(780.0, 1.0f32), (1180.0, 0.52), (2750.0, 0.24)];
    let mut voice = 0.0;
    for (freq, gain) in formants {
        let fm = sine(t, freq) * (0.35 + 0.65 * (TAU * f0 * t).sin().abs() as f32);
        voice += fm * gain;
    }
    let breath = noise_at(pos, 0xBCEA) * 0.045;
    ((buzz * 0.35 + voice * 0.30) * env + breath * env) * 0.9
}

fn demo_voice(t: f64, pos: u64) -> (f32, f32) {
    // A short slap echo, using the same deterministic voice function.
    let dry = voice_dry(t, pos);
    let echo1 = voice_dry(t - 0.27, pos) * 0.25;
    let echo2 = voice_dry(t - 0.54, pos) * 0.10;
    let l = dry + echo1 + echo2;
    let r = dry * 0.92 + voice_dry(t - 0.31, pos) * 0.24 + voice_dry(t - 0.62, pos) * 0.09;
    (soft_clip(l), soft_clip(r))
}

fn demo_background(t: f64, pos: u64) -> (f32, f32) {
    let bpm = 120.0;
    let beat = t * bpm / 60.0;
    let bar = (beat / 4.0).floor() as i64;
    let chord_idx = bar.rem_euclid(4) as usize;
    let roots = [53.0, 49.0, 45.0, 48.0];
    let root = roots[chord_idx];

    // Light kick and shaker.
    let kick_s = kick(t, bpm) * 0.35;
    let sixteenth = t * bpm / 60.0 * 4.0;
    let since = (sixteenth - sixteenth.floor()) * 15.0 / bpm;
    let shaker = if since < 0.045 {
        let raw = noise_at(pos, 0x5A4E) - noise_at(pos.wrapping_sub(1), 0x5A4E) * 0.8;
        raw * exp_env(since, 120.0) * 0.10
    } else {
        0.0
    };

    // Marimba melody on eighths.
    let eighth = beat * 2.0;
    let step = eighth.floor() as i64;
    let step_since = (eighth - eighth.floor()) * 30.0 / bpm;
    let melody = [0.0, 3.0, 7.0, 10.0, 12.0, 7.0, 3.0, 5.0];
    let note = root + 12.0 + melody[step.rem_euclid(8) as usize];
    let mar = marimba(step_since, midi_hz(note)) * 0.14;
    let (m_l, m_r) = pan_gains(if step & 1 == 0 { -0.35 } else { 0.35 });

    // Plucked bass on beats 1 and 3.
    let beat_since = (beat - beat.floor()) * 60.0 / bpm;
    let bass = if (beat.floor() as i64) & 1 == 0 {
        (sine(t, midi_hz(root - 12.0)) * 0.85 + sine(t, midi_hz(root) * 0.5) * 0.1)
            * exp_env(beat_since, 3.0)
            * 0.17
    } else {
        0.0
    };

    // Off-beat chord stabs.
    let half = beat * 2.0;
    let off = (half - half.floor()) > 0.5;
    let stab = if off {
        let s = (half - half.floor() - 0.5) * 30.0 / bpm;
        let env = exp_env(s, 9.0);
        (sine(t, midi_hz(root)) * 0.5
            + sine(t, midi_hz(root + 4.0)) * 0.35
            + sine(t, midi_hz(root + 7.0)) * 0.3)
            * env
            * 0.055
    } else {
        0.0
    };

    let l = kick_s + shaker + bass + stab + mar * m_l;
    let r = kick_s + shaker + bass + stab + mar * m_r;
    (soft_clip(l * 1.1), soft_clip(r * 1.1))
}

fn demo_sample(kind: DemoKind, t: f64, pos: u64) -> [f32; 2] {
    let (l, r) = match kind {
        DemoKind::Track1 => demo_track1(t, pos),
        DemoKind::Track2 => demo_track2(t, pos),
        DemoKind::VoiceTake => demo_voice(t, pos),
        DemoKind::Background => demo_background(t, pos),
    };
    [l, r]
}

// ---------------------------------------------------------------------------
// Sources and shared state
// ---------------------------------------------------------------------------

struct WaveTrack {
    frames: Vec<[f32; 2]>,
}

/// What the picker reports after a successful load.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub duration: f32,
    pub sample_rate: u32,
    pub channels: usize,
    pub bits: Option<u16>,
    pub format: String,
    /// True when the file was longer than this build can hold in memory and
    /// only its first MAX_TRACK_SECONDS were loaded.
    pub truncated: bool,
}

impl TrackInfo {
    pub fn detail(&self) -> String {
        let bits = self.bits.map(|b| format!("{b}-bit ")).unwrap_or_default();
        let channels = match self.channels {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{n} ch"),
        };
        let mut out = format!(
            "{} · {} Hz · {}{}",
            self.format, self.sample_rate, bits, channels
        );
        if self.truncated {
            out.push_str(" · first 30 min only");
        }
        out
    }
}

#[derive(Clone)]
enum Source {
    Demo(DemoKind),
    Wave(Arc<WaveTrack>),
}

impl Source {
    fn sample(&self, frame: u64, sample_rate: u32) -> [f32; 2] {
        match self {
            Source::Demo(kind) => {
                let t = frame as f64 / sample_rate as f64;
                demo_sample(*kind, t, frame)
            }
            Source::Wave(track) => track
                .frames
                .get(frame as usize)
                .copied()
                .unwrap_or([0.0, 0.0]),
        }
    }
}

/// Strip routing bits from FUN_0052b490: busa -> 0x1000, busa2 -> 0x2000,
/// busa3 -> 0x4000, busb -> 0x10000, busb2 -> 0x20000. The button handler
/// FUN_0053e7b0 toggles the same bits (masks at 0x584e78 and 0x584e98).
pub const STRIP_ROUTE_A1: u32 = 4096; // 0x1000
pub const STRIP_ROUTE_A2: u32 = 8192; // 0x2000
pub const STRIP_ROUTE_A3: u32 = 16384; // 0x4000
pub const STRIP_ROUTE_B1: u32 = 65536; // 0x10000
pub const STRIP_ROUTE_B2: u32 = 131072; // 0x20000

fn strip_route_mask(index: usize) -> u32 {
    match index {
        0 => STRIP_ROUTE_A1,
        1 => STRIP_ROUTE_A2,
        2 => STRIP_ROUTE_A3,
        3 => STRIP_ROUTE_B1,
        _ => STRIP_ROUTE_B2,
    }
}

struct StripMix {
    gain_db: AtomicU32,
    pan: AtomicU32,
    /// bit 0 mute, bit 1 solo, bit 2 mono
    flags: AtomicU32,
    /// bit 0 A1, 1 A2, 2 A3, 3 B1, 4 B2
    route: AtomicU32,
}

impl StripMix {
    fn new() -> Self {
        Self {
            gain_db: f32_atomic(0.0),
            pan: f32_atomic(0.0),
            flags: AtomicU32::new(0),
            route: AtomicU32::new(1),
        }
    }
}

struct Shared {
    source: Mutex<Option<Source>>,
    source_loaded: AtomicBool,
    playing: AtomicBool,
    gain_db: AtomicU32,
    route: AtomicU32,
    position: AtomicU64,
    /// Pending seek request applied by the next audio callback. A plain
    /// `position` store can race with the callback's per-block write.
    seek_request: AtomicU64,
    length: AtomicU64,
    stop_at_end: AtomicBool,
    loop_playback: AtomicBool,
    finished: AtomicBool,
    sample_rate: AtomicU32,
    bus_gain_db: [AtomicU32; 5],
    bus_mute: [AtomicBool; 5],
    bus_mono: [AtomicU8; 5],
    bus_eq_on: [AtomicBool; 5],
    bus_eq_gain: [[AtomicU32; 6]; 5],
    /// B1 and B2 feed the capture endpoint of two USB cables. The console
    /// binds cables 1 and 2 when the virtual-cable server is running.
    virtual_outputs: Mutex<[Option<Arc<Device>>; 2]>,
    /// Outgoing VBAN streams: (bus index, sender ring).
    vban_tx: Mutex<Vec<(usize, Arc<TxRing>)>>,
    /// Live capture streams for all five input strips, in strip order. The
    /// original console feeds the two virtual strips from fixed virtual
    /// devices; this rebuild also lets them open any Windows capture endpoint,
    /// so a USB interface or a virtual cable installed on the machine appears
    /// there.
    inputs: Mutex<[Option<Arc<InputShared>>; MAX_STRIPS]>,
    strips: [StripMix; MAX_STRIPS],
    /// Number of visible/processed input strips (5..=MAX_STRIPS).
    active_strips: AtomicUsize,
    any_solo: AtomicBool,
    /// Post-fader, post-pan peak of each strip before the bus fader. The bus
    /// meters that have no physical device use it to show what was routed.
    strip_peak: [AtomicU32; MAX_STRIPS],
    /// Peak of the material leaving the tape deck, before the A1 fader/EQ.
    /// This is what the virtual input strip and the unopened buses meter.
    source_peak: AtomicU32,
    /// Peak of what actually reached the A1 output device.
    peak: AtomicU32,
    /// Post-fader peak of each bus, before the A1 device and after B1/B2 are
    /// injected into their cables.
    bus_peak: [AtomicU32; 5],
    /// Last A1 callback size and fraction of its period spent in the callback.
    callback_frames: AtomicUsize,
    callback_load: AtomicU32,
    /// One VST3 insert per input strip (up to MAX_STRIPS independent slots).
    /// This is a rebuild extension (the original console has no VST loader);
    /// see vst.rs.
    pub vsts: [Arc<crate::vst::VstChain>; MAX_STRIPS],
    error: Mutex<Option<String>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            source: Mutex::new(None),
            source_loaded: AtomicBool::new(false),
            playing: AtomicBool::new(false),
            gain_db: f32_atomic(0.0),
            route: AtomicU32::new(1),
            position: AtomicU64::new(0),
            seek_request: AtomicU64::new(u64::MAX),
            length: AtomicU64::new(0),
            stop_at_end: AtomicBool::new(true),
            loop_playback: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            sample_rate: AtomicU32::new(48_000),
            bus_gain_db: std::array::from_fn(|_| f32_atomic(0.0)),
            bus_mute: std::array::from_fn(|_| AtomicBool::new(false)),
            bus_mono: std::array::from_fn(|_| AtomicU8::new(0)),
            bus_eq_on: std::array::from_fn(|_| AtomicBool::new(false)),
            bus_eq_gain: std::array::from_fn(|_| std::array::from_fn(|_| f32_atomic(0.0))),
            virtual_outputs: Mutex::new([None, None]),
            vban_tx: Mutex::new(Vec::new()),
            inputs: Mutex::new(std::array::from_fn(|_| None)),
            strips: std::array::from_fn(|_| StripMix::new()),
            active_strips: AtomicUsize::new(BASE_STRIPS),
            any_solo: AtomicBool::new(false),
            strip_peak: std::array::from_fn(|_| f32_atomic(0.0)),
            source_peak: f32_atomic(0.0),
            peak: f32_atomic(0.0),
            bus_peak: std::array::from_fn(|_| f32_atomic(0.0)),
            callback_frames: AtomicUsize::new(0),
            callback_load: f32_atomic(0.0),
            vsts: std::array::from_fn(|_| crate::vst::VstChain::new()),
            error: Mutex::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Bus EQ
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn set_peaking(&mut self, freq: f32, q: f32, gain_db: f32, sample_rate: f32) {
        let f = freq.clamp(10.0, sample_rate * 0.45);
        let q = q.max(0.1);
        if gain_db.abs() < 0.005 {
            *self = Self::identity();
            return;
        }
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f / sample_rate;
        let cos_w = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        self.b0 = (1.0 + alpha * a) / a0;
        self.b1 = (-2.0 * cos_w) / a0;
        self.b2 = (1.0 - alpha * a) / a0;
        self.a1 = (-2.0 * cos_w) / a0;
        self.a2 = (1.0 - alpha / a) / a0;
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        if y.is_finite() {
            y
        } else {
            self.z1 = 0.0;
            self.z2 = 0.0;
            0.0
        }
    }
}

struct Dsp {
    eq_l: [[Biquad; 6]; 5],
    eq_r: [[Biquad; 6]; 5],
    eq_on: [bool; 5],
    eq_gain: [[f32; 6]; 5],
    eq_ready: [bool; 5],
    gain: [f32; 5],
    readers: [InputReader; MAX_STRIPS],
    // Each input strip is read as a block so its own VST insert can process it
    // before the mix. The per-strip buffers keep their capacity across
    // callbacks, so the audio thread never allocates.
    vst_in_l: [Vec<f32>; MAX_STRIPS],
    vst_in_r: [Vec<f32>; MAX_STRIPS],
    vst_out_l: [Vec<f32>; MAX_STRIPS],
    vst_out_r: [Vec<f32>; MAX_STRIPS],
    vst_ready: [bool; MAX_STRIPS],
    // B1/B2 are rendered alongside A1 and handed to their virtual cable's
    // capture endpoint as PCM, so other applications can record them.
    out_bus_l: [Vec<f32>; 5],
    out_bus_r: [Vec<f32>; 5],
    pcm: Vec<u8>,
}

impl Dsp {
    fn new() -> Self {
        Self {
            eq_l: std::array::from_fn(|_| [Biquad::identity(); 6]),
            eq_r: std::array::from_fn(|_| [Biquad::identity(); 6]),
            eq_on: [false; 5],
            eq_gain: [[0.0; 6]; 5],
            eq_ready: [false; 5],
            gain: [0.0; 5],
            readers: std::array::from_fn(|_| InputReader::new()),
            // Allocate each strip's insert scratchpads once, before the audio
            // thread starts. The callback may shrink and regrow them up to
            // this capacity, but it never allocates (the person's VST3 spec,
            // section 3: no heap allocations on the audio thread).
            vst_in_l: std::array::from_fn(|_| vec![0.0; crate::vst::VST_MAX_BLOCK]),
            vst_in_r: std::array::from_fn(|_| vec![0.0; crate::vst::VST_MAX_BLOCK]),
            vst_out_l: std::array::from_fn(|_| vec![0.0; crate::vst::VST_MAX_BLOCK]),
            vst_out_r: std::array::from_fn(|_| vec![0.0; crate::vst::VST_MAX_BLOCK]),
            vst_ready: [false; MAX_STRIPS],
            out_bus_l: std::array::from_fn(|_| vec![0.0; crate::vst::VST_MAX_BLOCK]),
            out_bus_r: std::array::from_fn(|_| vec![0.0; crate::vst::VST_MAX_BLOCK]),
            pcm: Vec::with_capacity(crate::vst::VST_MAX_BLOCK * 2 * 4),
        }
    }

    fn update_eq(&mut self, shared: &Shared, sample_rate: f32) {
        for bus in 0..5 {
            let on = shared.bus_eq_on[bus].load(Ordering::Relaxed);
            let mut gains = [0.0f32; 6];
            for (i, g) in gains.iter_mut().enumerate() {
                *g = load_f32(&shared.bus_eq_gain[bus][i]);
            }
            if self.eq_ready[bus] && self.eq_on[bus] == on && self.eq_gain[bus] == gains {
                continue;
            }
            for i in 0..6 {
                self.eq_l[bus][i].set_peaking(EQ_FREQS[i], EQ_Q, gains[i], sample_rate);
                self.eq_r[bus][i].set_peaking(EQ_FREQS[i], EQ_Q, gains[i], sample_rate);
            }
            self.eq_on[bus] = on;
            self.eq_gain[bus] = gains;
            self.eq_ready[bus] = true;
        }
    }

    fn render<T>(&mut self, data: &mut [T], channels: usize, shared: &Shared)
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let started = std::time::Instant::now();
        let sample_rate = shared.sample_rate.load(Ordering::Relaxed) as f32;
        let strip_count = shared
            .active_strips
            .load(Ordering::Relaxed)
            .clamp(BASE_STRIPS, MAX_STRIPS);
        self.update_eq(shared, sample_rate);

        let mut active = shared.playing.load(Ordering::Relaxed);
        let tape_route = shared.route.load(Ordering::Relaxed);
        let mut bus_target = [0.0f32; 5];
        let mut bus_mute = [false; 5];
        let mut bus_mono = [0u8; 5];
        let mut bus_eq_on = [false; 5];
        for b in 0..5 {
            bus_mute[b] = shared.bus_mute[b].load(Ordering::Relaxed);
            bus_target[b] = if bus_mute[b] {
                0.0
            } else {
                db_to_lin(load_f32(&shared.bus_gain_db[b]))
            };
            bus_mono[b] = shared.bus_mono[b].load(Ordering::Relaxed);
            bus_eq_on[b] = shared.bus_eq_on[b].load(Ordering::Relaxed);
        }
        let tape_gain = db_to_lin(load_f32(&shared.gain_db));
        let len = shared.length.load(Ordering::Relaxed);
        let stop_at_end = shared.stop_at_end.load(Ordering::Relaxed);
        let loop_playback = shared.loop_playback.load(Ordering::Relaxed);
        let seek = shared.seek_request.swap(u64::MAX, Ordering::Relaxed);
        let mut pos = if seek == u64::MAX {
            shared.position.load(Ordering::Relaxed)
        } else {
            seek
        };
        let mut finished = false;
        let mut peak = 0.0f32;
        let mut source_peak = 0.0f32;
        let mut strip_peak = [0.0f32; MAX_STRIPS];
        let mut bus_peak = [0.0f32; 5];

        // Snapshot the routing/gain settings once per callback. The UI writes
        // them at frame rate; reading them here keeps the mix in step.
        let any_solo = shared.any_solo.load(Ordering::Relaxed);
        let mut strip_on = [false; MAX_STRIPS];
        let mut strip_gain = [0.0f32; MAX_STRIPS];
        let mut strip_pan = [0.0f32; MAX_STRIPS];
        let mut strip_flags = [0u32; MAX_STRIPS];
        let mut strip_route = [0u32; MAX_STRIPS];
        for i in 0..strip_count {
            let s = &shared.strips[i];
            strip_flags[i] = s.flags.load(Ordering::Relaxed);
            strip_gain[i] = db_to_lin(load_f32(&s.gain_db));
            strip_pan[i] = load_f32(&s.pan);
            // A strip is read when it is routed to any bus this callback
            // renders (A1, B1 or B2), it is not muted and, if any strip is
            // soloed, it is one of the soloed strips.
            let route = s.route.load(Ordering::Relaxed);
            strip_route[i] = route;
            let routed = route
                & (STRIP_ROUTE_A1
                    | STRIP_ROUTE_A2
                    | STRIP_ROUTE_A3
                    | STRIP_ROUTE_B1
                    | STRIP_ROUTE_B2)
                != 0;
            let muted = (strip_flags[i] & 1) != 0;
            let solo = (strip_flags[i] & 2) != 0;
            strip_on[i] = routed && !muted && (!any_solo || solo);
        }

        // Take a private reference to each capture stream, then one lock per
        // stream for the whole callback. No lock is taken per frame.
        let input_arcs: [Option<Arc<InputShared>>; MAX_STRIPS] = {
            let guard = lock_source(&shared.inputs);
            std::array::from_fn(|i| guard[i].clone())
        };
        let mut rings: [Option<std::sync::MutexGuard<'_, SampleRing>>; MAX_STRIPS] =
            std::array::from_fn(|i| input_arcs[i].as_ref().map(|s| lock_source(&s.ring)));
        let input_rates: [u32; MAX_STRIPS] = std::array::from_fn(|i| {
            input_arcs[i]
                .as_ref()
                .map(|s| s.rate.load(Ordering::Relaxed))
                .unwrap_or(48_000)
        });

        // Every input strip has its own VST3 insert. Read each routed strip
        // as one block so its plug-in gets whole buffers, process it, then mix
        // the returned samples below exactly where the dry samples would have
        // gone. A strip without a plug-in (or with bypass on) stays dry.
        let frames = data.len() / channels.max(1);
        self.vst_ready = [false; MAX_STRIPS];
        for i in 0..strip_count {
            if !strip_on[i] {
                // Keep an unrouted/muted input near live time. Without this a
                // strip can accumulate seconds of backlog and be delayed as
                // soon as its bus button is turned on.
                if let Some(ring) = rings[i].as_mut() {
                    let target = (sample_rate * INPUT_TARGET_MS / 1000.0) as usize;
                    ring.trim_to(target);
                }
                continue;
            }
            let Some(ring) = rings[i].as_mut() else {
                continue;
            };
            self.vst_in_l[i].resize(frames, 0.0);
            self.vst_in_r[i].resize(frames, 0.0);
            self.vst_out_l[i].resize(frames, 0.0);
            self.vst_out_r[i].resize(frames, 0.0);
            for frame in 0..frames {
                let s = self.readers[i].next(ring, input_rates[i], sample_rate as u32);
                self.vst_in_l[i][frame] = s[0];
                self.vst_in_r[i][frame] = s[1];
            }
            if !shared.vsts[i].process(
                &self.vst_in_l[i],
                &self.vst_in_r[i],
                &mut self.vst_out_l[i],
                &mut self.vst_out_r[i],
            ) {
                // No plug-in, bypass on, or a failed plug-in: pass the strip
                // through untouched.
                self.vst_out_l[i].copy_from_slice(&self.vst_in_l[i]);
                self.vst_out_r[i].copy_from_slice(&self.vst_in_r[i]);
            }
            self.vst_ready[i] = true;
        }

        {
            let source_guard = lock_source(&shared.source);
            let source = source_guard.as_ref();
            for bus in 0..5 {
                self.out_bus_l[bus].resize(frames, 0.0);
                self.out_bus_r[bus].resize(frames, 0.0);
            }

            for (frame_no, frame) in data.chunks_mut(channels.max(1)).enumerate() {
                let mut bus_l = [0.0f32; 5];
                let mut bus_r = [0.0f32; 5];

                // Tape deck -> its own routing row. The source peak is measured
                // whenever the deck plays, so the virtual input strip can meter
                // it even when none of the A1/B1/B2 buttons are lit.
                if active && len > 0 && pos < len {
                    if let Some(src) = source {
                        let s = src.sample(pos, sample_rate as u32);
                        let tl = s[0] * tape_gain;
                        let tr = s[1] * tape_gain;
                        source_peak = source_peak.max(tl.abs()).max(tr.abs());
                        if tape_route & 1 != 0 {
                            bus_l[0] += tl;
                            bus_r[0] += tr;
                        }
                        if tape_route & 2 != 0 {
                            bus_l[1] += tl;
                            bus_r[1] += tr;
                        }
                        if tape_route & 4 != 0 {
                            bus_l[2] += tl;
                            bus_r[2] += tr;
                        }
                        if tape_route & 8 != 0 {
                            bus_l[3] += tl;
                            bus_r[3] += tr;
                        }
                        if tape_route & 16 != 0 {
                            bus_l[4] += tl;
                            bus_r[4] += tr;
                        }
                    }
                }

                // Hardware and virtual input strips -> every bus they are
                // routed to. A virtual strip that has not opened a device
                // simply has no ring here and stays silent.
                for i in 0..strip_count {
                    if !strip_on[i] {
                        continue;
                    }
                    let s = if self.vst_ready[i] {
                        // Already read (and possibly processed) above.
                        [self.vst_out_l[i][frame_no], self.vst_out_r[i][frame_no]]
                    } else {
                        let Some(ring) = rings[i].as_mut() else {
                            continue;
                        };
                        self.readers[i].next(ring, input_rates[i], sample_rate as u32)
                    };
                    let gain = strip_gain[i];
                    let (pl, pr) = pan_gains(strip_pan[i] * 2.0);
                    let mut sl = s[0] * gain * pl;
                    let mut sr = s[1] * gain * pr;
                    if strip_flags[i] & 4 != 0 {
                        let m = (sl + sr) * 0.5;
                        sl = m;
                        sr = m;
                    }
                    strip_peak[i] = strip_peak[i].max(sl.abs()).max(sr.abs());
                    if strip_route[i] & STRIP_ROUTE_A1 != 0 {
                        bus_l[0] += sl;
                        bus_r[0] += sr;
                    }
                    if strip_route[i] & STRIP_ROUTE_A2 != 0 {
                        bus_l[1] += sl;
                        bus_r[1] += sr;
                    }
                    if strip_route[i] & STRIP_ROUTE_A3 != 0 {
                        bus_l[2] += sl;
                        bus_r[2] += sr;
                    }
                    if strip_route[i] & STRIP_ROUTE_B1 != 0 {
                        bus_l[3] += sl;
                        bus_r[3] += sr;
                    }
                    if strip_route[i] & STRIP_ROUTE_B2 != 0 {
                        bus_l[4] += sl;
                        bus_r[4] += sr;
                    }
                }

                // Each bus has its own EQ, fader, mute and mono stage.
                for b in 0..5 {
                    let mut l = bus_l[b];
                    let mut r = bus_r[b];
                    if bus_eq_on[b] {
                        for i in 0..6 {
                            l = self.eq_l[b][i].process(l);
                            r = self.eq_r[b][i].process(r);
                        }
                    }

                    // Smooth the bus fader so drags do not click.
                    self.gain[b] += (bus_target[b] - self.gain[b]) * 0.004;
                    l *= self.gain[b];
                    r *= self.gain[b];

                    if bus_mono[b] == 1 {
                        let m = (l + r) * 0.5;
                        l = m;
                        r = m;
                    } else if bus_mono[b] == 2 {
                        std::mem::swap(&mut l, &mut r);
                    }

                    l = soft_clip(l);
                    r = soft_clip(r);
                    bus_l[b] = l;
                    bus_r[b] = r;
                    bus_peak[b] = bus_peak[b].max(l.abs()).max(r.abs());
                }

                let (l, r) = (bus_l[0], bus_r[0]);
                peak = peak.max(l.abs()).max(r.abs());

                let count = frame.len();
                for (i, out) in frame.iter_mut().enumerate() {
                    let v = if count == 1 {
                        (l + r) * 0.5
                    } else if i == 0 {
                        l
                    } else if i == 1 {
                        r
                    } else {
                        0.0
                    };
                    *out = T::from_sample(v);
                }

                for bus in 0..5 {
                    self.out_bus_l[bus][frame_no] = bus_l[bus];
                    self.out_bus_r[bus][frame_no] = bus_r[bus];
                }

                if active && source.is_some() && len > 0 {
                    pos += 1;
                    if pos >= len {
                        if loop_playback {
                            // RecordMode Loop='1': the original posts WM_COMMAND
                            // 0xab, which presses REPLAY, so playback restarts.
                            pos = 0;
                        } else if stop_at_end {
                            pos = len;
                            finished = true;
                            active = false;
                        } else {
                            pos = 0;
                        }
                    }
                }
            }
        }

        // If the source reached the end, the remaining frames in this callback
        // stay silent and the transport switches back to STOPPED.
        if finished {
            shared.playing.store(false, Ordering::Relaxed);
            shared.finished.store(true, Ordering::Relaxed);
        }
        shared.position.store(pos, Ordering::Relaxed);
        shared
            .source_peak
            .store(source_peak.to_bits(), Ordering::Relaxed);
        for i in 0..strip_count {
            shared.strip_peak[i].store(strip_peak[i].to_bits(), Ordering::Relaxed);
        }
        for b in 0..5 {
            shared.bus_peak[b].store(bus_peak[b].to_bits(), Ordering::Relaxed);
        }
        self.write_virtual_outputs(shared, frames);
        self.write_vban(shared, frames);
        shared.peak.store(peak.to_bits(), Ordering::Relaxed);

        let frames_now = frames.max(1);
        let period = frames_now as f32 / sample_rate.max(1.0);
        let load = if period > 0.0 {
            started.elapsed().as_secs_f32() / period
        } else {
            0.0
        };
        shared.callback_frames.store(frames_now, Ordering::Relaxed);
        shared
            .callback_load
            .store(load.to_bits(), Ordering::Relaxed);
    }

    /// Feed the VBAN sender rings from the post-bus stems. This is a ring
    /// copy only; the actual UDP send happens on the sender threads.
    fn write_vban(&self, shared: &Shared, frames: usize) {
        let targets = lock_source(&shared.vban_tx);
        for (bus, ring) in targets.iter() {
            if *bus < 5 {
                ring.push_block(&self.out_bus_l[*bus], &self.out_bus_r[*bus], frames);
            }
        }
    }

    /// Send B1/B2 to the capture side of their USB cables as PCM. Called at
    /// the end of an A1 callback, so both virtual buses share the same input
    /// read and stay in sync with the physical mix.
    fn write_virtual_outputs(&mut self, shared: &Shared, frames: usize) {
        if frames == 0 {
            return;
        }
        let outputs = {
            let guard = lock_source(&shared.virtual_outputs);
            [guard[0].clone(), guard[1].clone()]
        };
        let mixer_rate = shared.sample_rate.load(Ordering::Relaxed).max(1);
        for (slot, device) in outputs.into_iter().enumerate() {
            let Some(device) = device else { continue };
            if !device.capture_active() {
                continue;
            }
            let frame_bytes = device.capture_frame_bytes();
            if frame_bytes < 2 {
                continue;
            }
            let bytes_per_sample = frame_bytes / virtual_cables::uac::AUDIO_CHANNELS;
            if bytes_per_sample == 0 {
                continue;
            }
            let capture_rate = device.capture_rate().max(1);
            let bus = if slot == 0 { 3 } else { 4 };
            let (left, right) = (&self.out_bus_l[bus], &self.out_bus_r[bus]);
            let out_frames = if capture_rate == mixer_rate {
                frames
            } else {
                ((frames as u64 * capture_rate as u64) / mixer_rate as u64).max(1) as usize
            };
            self.pcm.clear();
            if self.pcm.capacity() < out_frames * 2 * bytes_per_sample {
                self.pcm
                    .reserve(out_frames * 2 * bytes_per_sample - self.pcm.capacity());
            }
            for f in 0..out_frames {
                let src = if capture_rate == mixer_rate {
                    f.min(frames - 1)
                } else {
                    ((f as u64 * mixer_rate as u64) / capture_rate as u64).min(frames as u64 - 1)
                        as usize
                };
                push_pcm(&mut self.pcm, left[src], bytes_per_sample);
                push_pcm(&mut self.pcm, right[src], bytes_per_sample);
            }
            let _ = device.write_capture_pcm(&self.pcm);
        }
    }
}

/// Encode one f32 sample as little-endian PCM for the UAC capture endpoint.
fn push_pcm(out: &mut Vec<u8>, value: f32, bytes: usize) {
    let value = value.clamp(-1.0, 1.0);
    match bytes {
        2 => out.extend_from_slice(&((value * i16::MAX as f32) as i16).to_le_bytes()),
        3 => {
            let sample = (value * 8_388_607.0) as i32;
            let raw = sample.to_le_bytes();
            out.extend_from_slice(&raw[..3]);
        }
        4 => out.extend_from_slice(&((value * i32::MAX as f32) as i32).to_le_bytes()),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Public engine
// ---------------------------------------------------------------------------

pub struct AudioEngine {
    shared: Arc<Shared>,
    _stream: Option<cpal::Stream>,
    /// Keeps the thread that owns the stream alive. Dropping it closes the
    /// stream, exactly like dropping `_stream` did.
    stream_keeper: Option<std::sync::mpsc::Sender<()>>,
    stream_thread: Option<std::thread::JoinHandle<()>>,
    /// Result of a non-blocking start, polled by `poll_start`.
    pub pending_start:
        Option<std::sync::mpsc::Receiver<Result<(String, u32, u16), String>>>,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub available: bool,
    pub status: String,
}

impl AudioEngine {
    /// One VST3 insert slot per input strip, shared with the output callback.
    pub fn vst(&self, slot: usize) -> Arc<crate::vst::VstChain> {
        self.shared.vsts[slot.min(MAX_STRIPS - 1)].clone()
    }

    /// All insert slots, in strip order.
    pub fn vsts(&self) -> [Arc<crate::vst::VstChain>; MAX_STRIPS] {
        self.shared.vsts.clone()
    }

    /// Number of visible/processed strips. The engine always allocates for
    /// `MAX_STRIPS`; this only changes how many are mixed.
    pub fn set_strip_count(&self, count: usize) {
        self.shared
            .active_strips
            .store(count.clamp(BASE_STRIPS, MAX_STRIPS), Ordering::Relaxed);
    }

    pub fn strip_count(&self) -> usize {
        self.shared.active_strips.load(Ordering::Relaxed)
    }

    /// Engine object with its shared state but no output stream yet. Startup
    /// opens the stream on the first painted frame, so a slow or wedged audio
    /// device can never keep the window from appearing.
    pub fn idle() -> Self {
        Self {
            shared: Arc::new(Shared::new()),
            _stream: None,
            stream_keeper: None,
            stream_thread: None,
            pending_start: None,
            device: String::new(),
            sample_rate: 48_000,
            channels: 2,
            available: false,
            status: "Audio engine starting".to_string(),
        }
    }

    /// Open the output stream on a helper thread. `cpal::Stream` is not `Send`,
    /// so the thread that creates it must also own it: it parks until this
    /// engine is dropped. The caller polls [`AudioEngine::poll_start`], which
    /// means a wedged audio service can never freeze the UI thread.
    pub fn start_stream_async(&mut self) {
        if self.pending_start.is_some() {
            return;
        }
        let shared = self.shared.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let (keep_tx, keep_rx) = std::sync::mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("audio-start".to_string())
            .spawn(move || match Self::start(shared) {
                Ok((stream, device, sample_rate, channels)) => {
                    let _ = result_tx.send(Ok((device, sample_rate, channels)));
                    let _ = keep_rx.recv();
                    drop(stream);
                }
                Err(err) => {
                    let _ = result_tx.send(Err(err));
                }
            });
        let Ok(handle) = spawned else {
            self.status = "Could not start the audio thread".to_string();
            return;
        };
        self.stream_thread = Some(handle);
        self.stream_keeper = Some(keep_tx);
        self.pending_start = Some(result_rx);
        self.status = "Opening audio device".to_string();
    }

    /// Non-blocking poll of [`AudioEngine::start_stream_async`]. `None` means
    /// still opening, `Some(ok)` means the attempt finished.
    pub fn poll_start(&mut self) -> Option<bool> {
        let receiver = self.pending_start.as_ref()?;
        match receiver.try_recv() {
            Ok(Ok((device, sample_rate, channels))) => {
                self.device = device;
                self.sample_rate = sample_rate;
                self.channels = channels;
                self.available = true;
                self.status = "Audio engine running".to_string();
                self.pending_start = None;
                crate::diag_log(format!(
                    "audio: stream opened on '{}' via helper thread",
                    self.device
                ));
                Some(true)
            }
            Ok(Err(err)) => {
                self.available = false;
                self.status = err;
                self.pending_start = None;
                Some(false)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.available = false;
                self.status = "Audio device did not respond".to_string();
                self.pending_start = None;
                Some(false)
            }
        }
    }

    /// Open the output stream on the existing shared state. The VST chains and
    /// buffers created by [`AudioEngine::idle`] stay valid, so inserts loaded
    /// before the stream came up keep working.
    pub fn start_stream(&mut self) -> bool {
        self.stop_stream();
        match Self::start(self.shared.clone()) {
            Ok((stream, device, sample_rate, channels)) => {
                self._stream = Some(stream);
                self.device = device;
                self.sample_rate = sample_rate;
                self.channels = channels;
                self.available = true;
                self.status = "Audio engine running".to_string();
                crate::diag_log(format!("audio: stream opened on '{}'", self.device));
                true
            }
            Err(err) => {
                self._stream = None;
                self.available = false;
                self.status = err;
                false
            }
        }
    }

    pub fn new() -> Self {
        let mut engine = Self::idle();
        engine.start_stream();
        engine
    }

    /// The stream lives on the helper thread, so dropping the engine has to
    /// release it explicitly.
    pub fn stop_stream(&mut self) {
        self.stream_keeper = None;
        self._stream = None;
        self.pending_start = None;
        // The helper thread parks on the keep channel once the stream exists,
        // so joining it is instant. If it is still stuck opening the device,
        // leave it detached rather than block the caller.
        if let Some(handle) = self.stream_thread.take() {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        self.available = false;
    }

    fn start(shared: Arc<Shared>) -> Result<(cpal::Stream, String, u32, u16), String> {
        Self::start_with(shared, None)
    }

    fn start_with(
        shared: Arc<Shared>,
        preferred: Option<&str>,
    ) -> Result<(cpal::Stream, String, u32, u16), String> {
        Self::start_with_format(shared, preferred, None)
    }

    fn start_with_format(
        shared: Arc<Shared>,
        preferred: Option<&str>,
        format: Option<OutputFormat>,
    ) -> Result<(cpal::Stream, String, u32, u16), String> {
        let host = cpal::default_host();
        let devices: Vec<cpal::Device> = host
            .output_devices()
            .map_err(|e| format!("Could not list audio outputs: {e}"))?
            .collect();
        if devices.is_empty() {
            return Err("No audio output device found".to_string());
        }
        let default_name = host.default_output_device().and_then(|d| d.name().ok());
        let mut order: Vec<usize> = Vec::new();
        if let Some(name) = preferred {
            if let Some(i) = devices.iter().position(|d| {
                d.name()
                    .map(|n| n.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            }) {
                order.push(i);
            }
        }
        if let Some(name) = &default_name {
            if let Some(i) = devices.iter().position(|d| {
                d.name()
                    .map(|n| n.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            }) {
                if !order.contains(&i) {
                    order.push(i);
                }
            }
        }
        if preferred.is_none() {
            if let Some(i) = preferred_speaker_index(&devices) {
                if !order.contains(&i) {
                    order.insert(0, i);
                }
            }
        }
        for i in 0..devices.len() {
            if !order.contains(&i) {
                order.push(i);
            }
        }

        let mut last_error = String::from("No audio output device could be opened");
        for i in order {
            let name = devices[i].name().unwrap_or_default();
            match open_device(&devices[i], shared.clone(), format) {
                Ok(opened) => {
                    crate::diag_log(format!("audio: output opened on '{name}'"));
                    return Ok(opened);
                }
                Err(err) => {
                    if preferred.is_some_and(|p| p.eq_ignore_ascii_case(&name)) {
                        crate::diag_log(format!("audio: preferred output '{name}' failed: {err}"));
                    }
                    last_error = err;
                }
            }
        }
        Err(last_error)
    }

    pub fn load_demo(&mut self, kind: DemoKind) {
        let length = (DEMO_LEN_SECS * self.sample_rate as f32).round() as u64;
        *lock_source(&self.shared.source) = Some(Source::Demo(kind));
        self.shared.source_loaded.store(true, Ordering::Relaxed);
        self.shared.length.store(length, Ordering::Relaxed);
        self.shared.position.store(0, Ordering::Relaxed);
        self.shared.stop_at_end.store(true, Ordering::Relaxed);
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
    }

    pub fn load_wav(&mut self, path: &Path) -> Result<TrackInfo, String> {
        let (track, info) = decode_audio(path, self.sample_rate)?;
        let frames = track.frames.len() as u64;
        if frames == 0 {
            return Err("The audio file contains no audio frames".to_string());
        }
        *lock_source(&self.shared.source) = Some(Source::Wave(Arc::new(track)));
        self.shared.source_loaded.store(true, Ordering::Relaxed);
        self.shared.length.store(frames, Ordering::Relaxed);
        self.shared.position.store(0, Ordering::Relaxed);
        self.shared.stop_at_end.store(true, Ordering::Relaxed);
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
        Ok(info)
    }

    pub fn unload(&mut self) {
        *lock_source(&self.shared.source) = None;
        self.shared.source_loaded.store(false, Ordering::Relaxed);
        self.shared.length.store(0, Ordering::Relaxed);
        self.shared.position.store(0, Ordering::Relaxed);
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
    }

    pub fn has_source(&self) -> bool {
        self.shared.source_loaded.load(Ordering::Relaxed)
    }

    pub fn play(&mut self) {
        if !self.available {
            return;
        }
        if !self.has_source() {
            return;
        }
        let len = self.shared.length.load(Ordering::Relaxed);
        let pos = self.shared.position.load(Ordering::Relaxed);
        if len > 0 && pos >= len {
            self.shared.position.store(0, Ordering::Relaxed);
        }
        self.shared.finished.store(false, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&mut self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn stop(&mut self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.position.store(0, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
    }

    pub fn seek(&mut self, seconds: f32) {
        let len = self.shared.length.load(Ordering::Relaxed);
        let sr = self.sample_rate.max(1) as f32;
        let frame = ((seconds.max(0.0) * sr) as u64).min(len);
        self.shared.position.store(frame, Ordering::Relaxed);
        self.shared.seek_request.store(frame, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
    }

    pub fn position(&self) -> f32 {
        let sr = self.sample_rate.max(1) as f32;
        self.shared.position.load(Ordering::Relaxed) as f32 / sr
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn take_finished(&self) -> bool {
        self.shared.finished.swap(false, Ordering::Relaxed)
    }

    /// RecordMode Loop in the original's recorder options (bit 0x0200_0000 at
    /// 0x59da50). When set, the tape restarts at the end instead of stopping.
    pub fn set_loop(&self, loop_playback: bool) {
        self.shared
            .loop_playback
            .store(loop_playback, Ordering::Relaxed);
    }

    pub fn set_tape(&self, gain_db: f32, route: [bool; 5]) {
        self.shared
            .gain_db
            .store(gain_db.to_bits(), Ordering::Relaxed);
        let mut bits = 0u32;
        for (i, on) in route.iter().enumerate() {
            if *on {
                bits |= 1 << i;
            }
        }
        self.shared.route.store(bits, Ordering::Relaxed);
    }

    pub fn set_bus(
        &self,
        index: usize,
        gain_db: f32,
        mute: bool,
        mono: u8,
        eq_on: bool,
        eq_gain: [f32; 6],
    ) {
        if index >= 5 {
            return;
        }
        self.shared.bus_gain_db[index].store(gain_db.to_bits(), Ordering::Relaxed);
        self.shared.bus_mute[index].store(mute, Ordering::Relaxed);
        self.shared.bus_mono[index].store(mono.min(2), Ordering::Relaxed);
        self.shared.bus_eq_on[index].store(eq_on, Ordering::Relaxed);
        for i in 0..6 {
            self.shared.bus_eq_gain[index][i].store(eq_gain[i].to_bits(), Ordering::Relaxed);
        }
    }

    /// Bind B1/B2 to the USB cable devices whose capture endpoints carry the
    /// bus mix. The console calls this when the virtual-cable server starts or
    /// restarts; `None` leaves the bus silent.
    pub fn set_virtual_outputs(&self, b1: Option<Arc<Device>>, b2: Option<Arc<Device>>) {
        let mut guard = lock_source(&self.shared.virtual_outputs);
        let same = match (&guard[0], &b1) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        } && match (&guard[1], &b2) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            guard[0] = b1;
            guard[1] = b2;
        }
    }

    /// Replace the VBAN sender targets. The A1 callback copies the selected
    /// post-bus stems into each ring; the sender threads do the UDP work.
    pub fn set_vban_tx(&self, targets: Vec<(usize, Arc<TxRing>)>) {
        let mut guard = lock_source(&self.shared.vban_tx);
        let same = guard.len() == targets.len()
            && guard
                .iter()
                .zip(targets.iter())
                .all(|(a, b)| a.0 == b.0 && Arc::ptr_eq(&a.1, &b.1));
        if !same {
            *guard = targets;
        }
    }

    pub fn bus_peak(&self, index: usize) -> f32 {
        if index >= 5 {
            return 0.0;
        }
        load_f32(&self.shared.bus_peak[index])
    }

    /// Connect an input capture to the mix. `index` is the strip (0..4); the
    /// output callback then reads its frames and plays them through A1
    /// whenever that strip's A1 button is on.
    pub(crate) fn attach_input(&self, index: usize, input: Arc<InputShared>) {
        if index >= MAX_STRIPS {
            return;
        }
        let mut guard = lock_source(&self.shared.inputs);
        guard[index] = Some(input);
    }

    /// Disconnect an input, e.g. when the person chooses "No input".
    pub(crate) fn detach_input(&self, index: usize) {
        if index >= MAX_STRIPS {
            return;
        }
        let mut guard = lock_source(&self.shared.inputs);
        guard[index] = None;
        self.shared.strips[index].route.store(0, Ordering::Relaxed);
        self.shared.strip_peak[index].store(0.0f32.to_bits(), Ordering::Relaxed);
    }

    /// The strip-level half of the console: fader, pan, mute/solo/mono and
    /// the five routing buttons. A1 is bit 0, then A2, A3, B1, B2.
    pub fn set_strip(
        &self,
        index: usize,
        gain_db: f32,
        pan: f32,
        mute: bool,
        solo: bool,
        mono: bool,
        route: [bool; 5],
        any_solo: bool,
    ) {
        if index >= MAX_STRIPS {
            return;
        }
        let s = &self.shared.strips[index];
        s.gain_db.store(gain_db.to_bits(), Ordering::Relaxed);
        s.pan.store(pan.to_bits(), Ordering::Relaxed);
        let mut flags = 0u32;
        if mute {
            flags |= 1;
        }
        if solo {
            flags |= 2;
        }
        if mono {
            flags |= 4;
        }
        s.flags.store(flags, Ordering::Relaxed);
        let mut bits = 0u32;
        for (i, on) in route.iter().enumerate() {
            if *on {
                bits |= strip_route_mask(i);
            }
        }
        s.route.store(bits, Ordering::Relaxed);
        self.shared.any_solo.store(any_solo, Ordering::Relaxed);
    }

    /// Post-fader, post-pan peak of a hardware strip in the A1 mix. Used by
    /// the bus meters that have no physical output device.
    pub fn strip_peak(&self, index: usize) -> f32 {
        if index >= MAX_STRIPS {
            return 0.0;
        }
        load_f32(&self.shared.strip_peak[index])
    }

    pub fn peak(&self) -> f32 {
        load_f32(&self.shared.peak)
    }

    /// Frames in the last A1 callback, as reported by the device.
    pub fn callback_frames(&self) -> usize {
        self.shared.callback_frames.load(Ordering::Relaxed)
    }

    /// Fraction of the callback period spent processing (0.25 = 25%).
    pub fn callback_load(&self) -> f32 {
        load_f32(&self.shared.callback_load)
    }

    /// One output buffer at the current rate. Real device latency also
    /// includes the driver/WASAPI engine period.
    pub fn output_latency_ms(&self) -> f32 {
        let rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1) as f32;
        self.callback_frames() as f32 / rate * 1000.0
    }

    /// Peak of the tape material before the A1 fader and EQ. This is the level
    /// a virtual input strip would receive from the deck.
    pub fn source_peak(&self) -> f32 {
        load_f32(&self.shared.source_peak)
    }

    pub fn output_devices() -> Vec<String> {
        let host = cpal::default_host();
        host.output_devices()
            .map(|it| it.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// Try to open every output so a device that refuses a stream can be seen.
    /// Used by the `--audio-selftest` diagnostic.
    pub fn probe_outputs() -> Vec<String> {
        let host = cpal::default_host();
        let mut out = Vec::new();
        if let Ok(devices) = host.output_devices() {
            for d in devices {
                let name = d.name().unwrap_or_else(|_| "?".to_string());
                let shared = Arc::new(Shared::new());
                match open_device(&d, shared, None) {
                    Ok(_) => out.push(format!("OK   {name}")),
                    Err(err) => out.push(format!("FAIL {name}: {err}")),
                }
            }
        }
        out
    }

    /// Apply the rules of FUN_00527ad0 to the real output: only the seven
    /// standard sample rates and a main buffer of at most 2048 samples. The
    /// requested period is used when the driver accepts it; otherwise the
    /// driver's default period is kept at the requested sample rate.
    pub fn apply_format(
        &mut self,
        sample_rate: u32,
        main_buffer: u32,
        preferred: Option<&str>,
    ) -> Result<(), String> {
        if !SAMPLE_RATES.contains(&sample_rate) {
            return Err(A1_RATE_ERROR.to_string());
        }
        if main_buffer > MAIN_BUFFER_MAX {
            return Err(A1_BUFFER_ERROR.to_string());
        }
        self.stop_stream();
        let format = OutputFormat {
            sample_rate,
            buffer_frames: main_buffer,
        };
        match Self::start_with_format(self.shared.clone(), preferred, Some(format)) {
            Ok((stream, device, sample_rate, channels)) => {
                self._stream = Some(stream);
                self.device = device;
                self.sample_rate = sample_rate;
                self.channels = channels;
                self.available = true;
                self.status = "Audio engine running".to_string();
                crate::diag_log(format!(
                    "audio: format applied, stream reopened on '{}' at {} Hz / {} frames",
                    self.device, self.sample_rate, main_buffer
                ));
                Ok(())
            }
            Err(err) => {
                // Keep the console alive and return to a working output.
                if let Ok((stream, device, sample_rate, channels)) =
                    Self::start_with(self.shared.clone(), preferred)
                {
                    self._stream = Some(stream);
                    self.device = device;
                    self.sample_rate = sample_rate;
                    self.channels = channels;
                    self.available = true;
                    self.status = "Audio engine running".to_string();
                } else {
                    self.available = false;
                    self.status = err.clone();
                }
                Err(err)
            }
        }
    }

    pub fn set_output_device(
        &mut self,
        name: &str,
        sample_rate: u32,
        main_buffer: u32,
    ) -> Result<(), String> {
        self.stop_stream();
        let format = OutputFormat {
            sample_rate,
            buffer_frames: main_buffer,
        };
        match Self::start_with_format(self.shared.clone(), Some(name), Some(format)) {
            Ok((stream, device, sample_rate, channels)) => {
                self._stream = Some(stream);
                self.device = device;
                self.sample_rate = sample_rate;
                self.channels = channels;
                self.available = true;
                self.status = "Audio engine running".to_string();
                crate::diag_log(format!(
                    "audio: format applied, stream reopened on '{}' at {} Hz / {} frames",
                    self.device, self.sample_rate, main_buffer
                ));
                Ok(())
            }
            Err(err) => {
                // Keep the console alive and try to return to a working output.
                if let Ok((stream, device, sample_rate, channels)) =
                    Self::start_with(self.shared.clone(), None)
                {
                    self._stream = Some(stream);
                    self.device = device;
                    self.sample_rate = sample_rate;
                    self.channels = channels;
                    self.available = true;
                    self.status = "Audio engine running".to_string();
                } else {
                    self.available = false;
                    self.status = err.clone();
                }
                Err(err)
            }
        }
    }

    pub fn output_error(&self) -> Option<String> {
        lock_source(&self.shared.error).clone()
    }
}

// ---------------------------------------------------------------------------
// Input capture (microphone / line input metering and routing)
// ---------------------------------------------------------------------------

/// Frames kept per capture stream. At the highest supported input rate this
/// is half a second, enough to bridge the output callback and a slow device
/// without dropping audio.
const INPUT_RING_FRAMES: usize = 96_000;
/// Balanced capture period. Drivers that refuse it fall back to their default.
const INPUT_BUFFER_FRAMES: u32 = 512;
/// Keep the live input near this much audio; a startup backlog is trimmed and
/// slow clock drift is corrected toward it instead of growing toward the ring
/// capacity.
const INPUT_TARGET_MS: f32 = 25.0;
/// Extra fill tolerated before the drift controller starts dropping frames.
const INPUT_TARGET_HYSTERESIS_MS: f32 = 15.0;

/// Interleaved stereo frames waiting to be mixed into the output. The capture
/// callback appends, the output callback consumes. A short ring keeps the
/// delay low; when it overflows the oldest frame is dropped so the input never
/// blocks the audio thread.
struct SampleRing {
    frames: Vec<[f32; 2]>,
    read: usize,
    write: usize,
    count: usize,
}

impl SampleRing {
    fn new() -> Self {
        Self {
            frames: vec![[0.0, 0.0]; INPUT_RING_FRAMES],
            read: 0,
            write: 0,
            count: 0,
        }
    }

    fn push(&mut self, frame: [f32; 2]) {
        self.frames[self.write] = frame;
        self.write = (self.write + 1) % INPUT_RING_FRAMES;
        if self.count == INPUT_RING_FRAMES {
            self.read = (self.read + 1) % INPUT_RING_FRAMES;
        } else {
            self.count += 1;
        }
    }

    fn pop(&mut self) -> Option<[f32; 2]> {
        if self.count == 0 {
            return None;
        }
        let frame = self.frames[self.read];
        self.read = (self.read + 1) % INPUT_RING_FRAMES;
        self.count -= 1;
        Some(frame)
    }

    fn fill(&self) -> usize {
        self.count
    }

    /// Drop the oldest frames down to `target`, in one lock.
    fn trim_to(&mut self, target: usize) {
        let drop = self.count.saturating_sub(target);
        if drop > 0 {
            self.read = (self.read + drop) % INPUT_RING_FRAMES;
            self.count -= drop;
        }
    }
}

pub(crate) struct InputShared {
    ring: Mutex<SampleRing>,
    /// Rate the capture device delivers. The output stream resamples from it.
    rate: AtomicU32,
    peak: AtomicU32,
    /// Frames in the last capture callback.
    frames: AtomicUsize,
    error: Mutex<Option<String>>,
}

impl InputShared {
    pub(crate) fn new() -> Self {
        Self {
            ring: Mutex::new(SampleRing::new()),
            rate: AtomicU32::new(48_000),
            peak: f32_atomic(0.0),
            frames: AtomicUsize::new(0),
            error: Mutex::new(None),
        }
    }

    /// VBAN receiver feed. The hub pushes decoded stereo frames here; the
    /// output callback consumes them exactly like a CPAL capture.
    pub(crate) fn push_block(&self, frames: &[[f32; 2]]) {
        if frames.is_empty() {
            return;
        }
        let mut peak = 0.0f32;
        let mut ring = lock_source(&self.ring);
        for frame in frames {
            ring.push(*frame);
            peak = peak.max(frame[0].abs()).max(frame[1].abs());
        }
        self.peak.store(peak.to_bits(), Ordering::Relaxed);
        self.frames.store(frames.len(), Ordering::Relaxed);
    }

    pub(crate) fn set_rate(&self, rate: u32) {
        self.rate.store(rate.max(1), Ordering::Relaxed);
    }

    pub(crate) fn rate_value(&self) -> u32 {
        self.rate.load(Ordering::Relaxed)
    }

    pub(crate) fn peak_value(&self) -> f32 {
        load_f32(&self.peak)
    }

    pub(crate) fn fill_ms(&self, rate: u32) -> f32 {
        let fill = lock_source(&self.ring).fill();
        fill as f32 / rate.max(1) as f32 * 1000.0
    }

    pub(crate) fn frames_last(&self) -> usize {
        self.frames.load(Ordering::Relaxed)
    }
}

/// Sample-accurate reader state for one capture stream inside the output
/// callback. Ratio converts the device rate to the main output rate; the
/// linear interpolation smooths mismatched clock rates (e.g. 44.1 -> 48 kHz).
struct InputReader {
    prev: [f32; 2],
    cur: [f32; 2],
    phase: f32,
    ready: bool,
    drift_tick: u32,
    starve: u32,
}

impl InputReader {
    fn new() -> Self {
        Self {
            prev: [0.0, 0.0],
            cur: [0.0, 0.0],
            phase: 0.0,
            ready: false,
            drift_tick: 0,
            starve: 0,
        }
    }

    fn next(&mut self, ring: &mut SampleRing, input_rate: u32, output_rate: u32) -> [f32; 2] {
        let output_rate = output_rate.max(1);
        let target = (output_rate as f32 * INPUT_TARGET_MS / 1000.0) as usize;
        let high = target + (output_rate as f32 * INPUT_TARGET_HYSTERESIS_MS / 1000.0) as usize;

        if !self.ready {
            // A capture stream can start with a large backlog (a virtual cable
            // with a big ring, or a device opened long before playback). Drop
            // the oldest frames once, so live latency starts bounded.
            ring.trim_to(target);
            if let Some(frame) = ring.pop() {
                self.cur = frame;
                self.prev = frame;
                self.ready = true;
            }
            return self.cur;
        }

        let ratio = (input_rate.max(1) as f32) / (output_rate as f32);
        self.phase += ratio;
        while self.phase >= 1.0 {
            if let Some(frame) = ring.pop() {
                self.prev = self.cur;
                self.cur = frame;
                self.starve = 0;
            } else {
                self.starve = self.starve.saturating_add(1);
                if self.starve > output_rate / 10 {
                    self.prev = [0.0, 0.0];
                    self.cur = [0.0, 0.0];
                }
            }
            self.phase -= 1.0;
        }

        // Clock drift: drop one frame occasionally when the ring grows past
        // the target window. The startup trim below handles big backlogs; this
        // only keeps a slow drift from becoming permanent latency.
        if ring.fill() > high {
            self.drift_tick += 1;
            if self.drift_tick >= 128 {
                let _ = ring.pop();
                self.drift_tick = 0;
            }
        } else {
            self.drift_tick = 0;
        }

        // Keep the last frame when the device falls behind; a brief hold is
        // inaudible and avoids clicks from a starved ring.
        let f = self.phase.clamp(0.0, 1.0);
        [
            self.prev[0] + (self.cur[0] - self.prev[0]) * f,
            self.prev[1] + (self.cur[1] - self.prev[1]) * f,
        ]
    }
}

/// One capture stream for a hardware input strip. The original reads its
/// meter levels from the audio engine; this rebuild reads the peak of the
/// samples the Windows capture device is delivering right now.
pub struct InputCapture {
    shared: Arc<InputShared>,
    _stream: Option<cpal::Stream>,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub available: bool,
    pub status: String,
}

impl InputCapture {
    pub fn open(preferred: Option<&str>) -> Self {
        let host = cpal::default_host();
        let devices: Vec<cpal::Device> = match host.input_devices() {
            Ok(list) => list.collect(),
            Err(err) => return Self::failed(format!("Could not list audio inputs: {err}")),
        };
        if devices.is_empty() {
            return Self::failed("No microphone or line input found".to_string());
        }
        let default_name = host.default_input_device().and_then(|d| d.name().ok());
        let mut order: Vec<usize> = Vec::new();
        // An explicitly requested device is tried alone: silently falling back
        // to a different input would capture the wrong source (for example a
        // webcam instead of a virtual cable) with no visible hint.
        let preferred_index = preferred.and_then(|name| {
            devices.iter().position(|d| {
                d.name()
                    .map(|n| n.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            })
        });
        if preferred.is_some() {
            let Some(index) = preferred_index else {
                return Self::failed(format!(
                    "Input device not found: {}",
                    preferred.unwrap_or_default()
                ));
            };
            match Self::start(&devices[index]) {
                Ok(capture) => return capture,
                Err(err) => {
                    crate::diag_log(format!(
                        "audio: could not open '{}': {err}",
                        preferred.unwrap_or_default()
                    ));
                    return Self::failed(format!(
                        "Could not open {}: {err}",
                        preferred.unwrap_or_default()
                    ));
                }
            }
        }
        // Windows often points the default recording device at a virtual
        // cable or at another mixer's output. Picking that would capture
        // silence, so a real microphone is tried first when the person has
        // not chosen a device explicitly.
        if preferred.is_none() {
            if let Some(i) = preferred_microphone_index(&devices) {
                if !order.contains(&i) {
                    order.push(i);
                }
            }
        }
        if let Some(name) = &default_name {
            if let Some(i) = devices.iter().position(|d| {
                d.name()
                    .map(|n| n.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            }) {
                if !order.contains(&i) {
                    order.push(i);
                }
            }
        }
        for i in 0..devices.len() {
            if !order.contains(&i) {
                order.push(i);
            }
        }

        let mut last_error = String::from("No input device could be opened");
        for i in order {
            match Self::start(&devices[i]) {
                Ok(capture) => return capture,
                Err(err) => last_error = err,
            }
        }
        Self::failed(last_error)
    }

    fn failed(status: String) -> Self {
        Self {
            shared: Arc::new(InputShared::new()),
            _stream: None,
            device: "No input".to_string(),
            sample_rate: 0,
            channels: 0,
            available: false,
            status,
        }
    }

    fn start(device: &cpal::Device) -> Result<Self, String> {
        let name = device
            .name()
            .unwrap_or_else(|_| "Default input".to_string());
        let supported = device
            .default_input_config()
            .map_err(|e| format!("Could not read the input format for {name}: {e}"))?;
        let sample_format = supported.sample_format();
        let mut config: cpal::StreamConfig = supported.config();
        config.buffer_size = cpal::BufferSize::Fixed(INPUT_BUFFER_FRAMES);
        let shared = Arc::new(InputShared::new());
        let stream = match build_capture_for_format(sample_format, device, &config, shared.clone())
        {
            Ok(stream) => stream,
            Err(_) => {
                // Some drivers refuse a fixed period; keep the device's own
                // default rather than failing the whole input.
                config.buffer_size = cpal::BufferSize::Default;
                build_capture_for_format(sample_format, device, &config, shared.clone())?
            }
        };
        stream
            .play()
            .map_err(|e| format!("Could not start {name}: {e}"))?;
        Ok(Self {
            shared,
            _stream: Some(stream),
            device: name,
            sample_rate: config.sample_rate.0,
            channels: config.channels,
            available: true,
            status: "Input open".to_string(),
        })
    }

    pub fn peak(&self) -> f32 {
        load_f32(&self.shared.peak)
    }

    /// Frames in the last capture callback.
    pub fn last_frames(&self) -> usize {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Frames currently waiting in the capture ring, expressed in milliseconds
    /// at the device rate.
    pub fn ring_fill_ms(&self) -> f32 {
        let fill = lock_source(&self.shared.ring).fill();
        fill as f32 / self.sample_rate.max(1) as f32 * 1000.0
    }

    /// The live buffer shared with the output stream, so a strip whose A1
    /// button is on is actually heard (not only metered).
    pub(crate) fn shared(&self) -> Arc<InputShared> {
        self.shared.clone()
    }

    pub fn error(&self) -> Option<String> {
        lock_source(&self.shared.error).clone()
    }

    pub fn devices() -> Vec<String> {
        let host = cpal::default_host();
        host.input_devices()
            .map(|it| it.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }
}

fn build_capture_for_format(
    sample_format: cpal::SampleFormat,
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<InputShared>,
) -> Result<cpal::Stream, String> {
    match sample_format {
        cpal::SampleFormat::F32 => build_capture::<f32>(device, config, shared),
        cpal::SampleFormat::I16 => build_capture::<i16>(device, config, shared),
        cpal::SampleFormat::U16 => build_capture::<u16>(device, config, shared),
        cpal::SampleFormat::I32 => build_capture::<i32>(device, config, shared),
        cpal::SampleFormat::U32 => build_capture::<u32>(device, config, shared),
        cpal::SampleFormat::I8 => build_capture::<i8>(device, config, shared),
        cpal::SampleFormat::U8 => build_capture::<u8>(device, config, shared),
        cpal::SampleFormat::I64 => build_capture::<i64>(device, config, shared),
        cpal::SampleFormat::U64 => build_capture::<u64>(device, config, shared),
        cpal::SampleFormat::F64 => build_capture::<f64>(device, config, shared),
        cpal::SampleFormat::I24 => build_capture::<cpal::I24>(device, config, shared),
        other => Err(format!("Unsupported input sample format: {other}")),
    }
}

fn build_capture<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<InputShared>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + 'static,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    shared.rate.store(rate, Ordering::Relaxed);
    let error_shared = shared.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                let mut peak = 0.0f32;
                let mut ring = lock_source(&shared.ring);
                for frame in data.chunks(channels.max(1)) {
                    let mut l = 0.0f32;
                    let mut r = 0.0f32;
                    for (ch, &sample) in frame.iter().enumerate() {
                        let v = f32::from_sample(sample);
                        if ch == 0 {
                            l = v;
                        }
                        if ch == 1 {
                            r = v;
                        }
                        let a = v.abs();
                        if a > peak {
                            peak = a;
                        }
                    }
                    if frame.len() < 2 {
                        r = l;
                    }
                    ring.push([l, r]);
                }
                shared.peak.store(peak.to_bits(), Ordering::Relaxed);
                shared
                    .frames
                    .store(data.len() / channels.max(1), Ordering::Relaxed);
            },
            move |err| {
                *lock_source(&error_shared.error) = Some(format!("Audio input error: {err}"));
                eprintln!("audio input error: {err}");
            },
            None,
        )
        .map_err(|e| format!("Could not open the audio input: {e}"))
}

fn is_virtual_output(name: &str) -> bool {
    let n = name.to_lowercase();
    // Playback endpoints that belong to loopback cables usually call
    // themselves "... Input", so that word plus the common virtual-device
    // names identifies them without relying on a vendor name.
    ["input", "cable", "virtual", "vban", "loopback"]
        .iter()
        .any(|k| n.contains(k))
}

fn is_virtual_input(name: &str) -> bool {
    let n = name.to_lowercase();
    ["cable", "virtual", "vban", "loopback"]
        .iter()
        .any(|k| n.contains(k))
}

/// Prefer a real microphone over a virtual loopback. The Windows default
/// recording device is often a cable or another mixer's output, which
/// carries no live microphone signal.
fn preferred_microphone_index(devices: &[cpal::Device]) -> Option<usize> {
    let mut fallback = None;
    for (i, d) in devices.iter().enumerate() {
        let Ok(name) = d.name() else { continue };
        if is_virtual_input(&name) {
            continue;
        }
        let lower = name.to_lowercase();
        if lower.contains("microphone") || lower.contains(" mic") || lower.starts_with("mic ") {
            return Some(i);
        }
        if fallback.is_none() {
            fallback = Some(i);
        }
    }
    fallback
}

/// Prefer speakers when Windows is currently pointed at a virtual cable, so a
/// first press of Play is audible. An explicit choice in the output dialog
/// always wins.
fn preferred_speaker_index(devices: &[cpal::Device]) -> Option<usize> {
    let mut fallback = None;
    for (i, d) in devices.iter().enumerate() {
        if let Ok(name) = d.name() {
            if is_virtual_output(&name) {
                continue;
            }
            let lower = name.to_lowercase();
            if lower.contains("speaker") || lower.contains("realtek") {
                return Some(i);
            }
            if fallback.is_none() {
                fallback = Some(i);
            }
        }
    }
    fallback
}

fn open_device(
    device: &cpal::Device,
    shared: Arc<Shared>,
    format: Option<OutputFormat>,
) -> Result<(cpal::Stream, String, u32, u16), String> {
    let device_name = device
        .name()
        .unwrap_or_else(|_| "Default output".to_string());

    if let Some(OutputFormat {
        sample_rate,
        buffer_frames,
    }) = format
    {
        // FUN_00527ad0: only these rates, and no main buffer above 2048.
        if !SAMPLE_RATES.contains(&sample_rate) {
            return Err(A1_RATE_ERROR.to_string());
        }
        if buffer_frames > MAIN_BUFFER_MAX {
            return Err(A1_BUFFER_ERROR.to_string());
        }

        if let Ok(ranges) = device.supported_output_configs() {
            let mut candidates: Vec<_> = ranges
                .filter(|r| {
                    r.min_sample_rate().0 <= sample_rate && sample_rate <= r.max_sample_rate().0
                })
                .collect();
            // Prefer a normal stereo format, then float samples.
            candidates.sort_by_key(|r| {
                (
                    if r.channels() == 2 { 0 } else { 1 },
                    if r.sample_format() == cpal::SampleFormat::F32 {
                        0
                    } else {
                        1
                    },
                )
            });

            if candidates.is_empty() {
                return Err(A1_RATE_ERROR.to_string());
            }

            for range in candidates {
                let supported = match range.try_with_sample_rate(cpal::SampleRate(sample_rate)) {
                    Some(config) => config,
                    None => continue,
                };
                let mut config = supported.config();
                config.buffer_size = cpal::BufferSize::Fixed(buffer_frames);

                match build_stream_for_format(
                    supported.sample_format(),
                    device,
                    &config,
                    shared.clone(),
                ) {
                    Ok(stream) => {
                        stream
                            .play()
                            .map_err(|e| format!("Could not start {device_name}: {e}"))?;
                        shared
                            .sample_rate
                            .store(config.sample_rate.0, Ordering::Relaxed);
                        return Ok((stream, device_name, config.sample_rate.0, config.channels));
                    }
                    Err(_) => {
                        // Some drivers refuse a fixed period. Keep the rate the
                        // user asked for and let the driver choose its period.
                        config.buffer_size = cpal::BufferSize::Default;
                        if let Ok(stream) = build_stream_for_format(
                            supported.sample_format(),
                            device,
                            &config,
                            shared.clone(),
                        ) {
                            stream
                                .play()
                                .map_err(|e| format!("Could not start {device_name}: {e}"))?;
                            shared
                                .sample_rate
                                .store(config.sample_rate.0, Ordering::Relaxed);
                            return Ok((
                                stream,
                                device_name,
                                config.sample_rate.0,
                                config.channels,
                            ));
                        }
                    }
                }
            }
            return Err(A1_OPEN_ERROR.to_string());
        }

        // No supported-config list: only accept the default when it matches.
        let supported = device
            .default_output_config()
            .map_err(|_| A1_OPEN_ERROR.to_string())?;
        if supported.sample_rate().0 != sample_rate {
            return Err(A1_RATE_ERROR.to_string());
        }
        let mut config = supported.config();
        config.buffer_size = cpal::BufferSize::Fixed(buffer_frames.min(MAIN_BUFFER_MAX));
        let stream =
            build_stream_for_format(supported.sample_format(), device, &config, shared.clone())
                .or_else(|_| {
                    config.buffer_size = cpal::BufferSize::Default;
                    build_stream_for_format(
                        supported.sample_format(),
                        device,
                        &config,
                        shared.clone(),
                    )
                })?;
        stream
            .play()
            .map_err(|e| format!("Could not start {device_name}: {e}"))?;
        shared
            .sample_rate
            .store(config.sample_rate.0, Ordering::Relaxed);
        return Ok((stream, device_name, config.sample_rate.0, config.channels));
    }

    let supported = device
        .default_output_config()
        .map_err(|e| format!("Could not read the output format for {device_name}: {e}"))?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.config();
    shared
        .sample_rate
        .store(config.sample_rate.0, Ordering::Relaxed);

    let stream = build_stream_for_format(sample_format, device, &config, shared)?;

    stream
        .play()
        .map_err(|e| format!("Could not start {device_name}: {e}"))?;
    Ok((stream, device_name, config.sample_rate.0, config.channels))
}

fn build_stream_for_format(
    sample_format: cpal::SampleFormat,
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    match sample_format {
        cpal::SampleFormat::F32 => build_stream::<f32>(device, config, shared),
        cpal::SampleFormat::I16 => build_stream::<i16>(device, config, shared),
        cpal::SampleFormat::U16 => build_stream::<u16>(device, config, shared),
        cpal::SampleFormat::I32 => build_stream::<i32>(device, config, shared),
        cpal::SampleFormat::U32 => build_stream::<u32>(device, config, shared),
        cpal::SampleFormat::I8 => build_stream::<i8>(device, config, shared),
        cpal::SampleFormat::U8 => build_stream::<u8>(device, config, shared),
        cpal::SampleFormat::I64 => build_stream::<i64>(device, config, shared),
        cpal::SampleFormat::U64 => build_stream::<u64>(device, config, shared),
        cpal::SampleFormat::F64 => build_stream::<f64>(device, config, shared),
        cpal::SampleFormat::I24 => build_stream::<cpal::I24>(device, config, shared),
        other => Err(format!("Unsupported audio sample format: {other}")),
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32> + 'static,
{
    let channels = config.channels as usize;
    let mut dsp = Dsp::new();
    let error_shared = shared.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                dsp.render(data, channels, &shared);
            },
            move |err| {
                *lock_source(&error_shared.error) = Some(format!("Audio output error: {err}"));
                eprintln!("audio output error: {err}");
            },
            None,
        )
        .map_err(|e| format!("Could not open the audio output: {e}"))
}

// ---------------------------------------------------------------------------
// WAV reader (PCM and IEEE float, mono/stereo, resampled to the device rate)
// ---------------------------------------------------------------------------

struct WavFmt {
    format: u16,
    channels: u16,
    sample_rate: u32,
    bits: u16,
}

fn parse_wav_fmt(data: &[u8]) -> Result<WavFmt, String> {
    if data.len() < 16 {
        return Err("The WAV format chunk is too short".to_string());
    }
    let mut format = u16::from_le_bytes([data[0], data[1]]);
    let channels = u16::from_le_bytes([data[2], data[3]]);
    let sample_rate = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let bits = u16::from_le_bytes([data[14], data[15]]);
    if format == 0xFFFE && data.len() >= 40 {
        format = u16::from_le_bytes([data[24], data[25]]);
    }
    if channels == 0 || sample_rate == 0 || bits == 0 {
        return Err("The WAV format chunk is not valid".to_string());
    }
    if format != 1 && format != 3 {
        return Err("Compressed WAV files are not supported; use PCM or float WAV".to_string());
    }
    let supported_bits = match format {
        1 => matches!(bits, 8 | 16 | 24 | 32),
        3 => matches!(bits, 32 | 64),
        _ => false,
    };
    if !supported_bits {
        return Err(format!("{bits}-bit WAV is not supported"));
    }
    Ok(WavFmt {
        format,
        channels,
        sample_rate,
        bits,
    })
}

fn read_sample(data: &[u8], offset: usize, fmt: &WavFmt) -> f32 {
    match fmt.format {
        1 => match fmt.bits {
            8 => (data[offset] as f32 - 128.0) / 128.0,
            16 => i16::from_le_bytes([data[offset], data[offset + 1]]) as f32 / 32768.0,
            24 => {
                let v = data[offset] as i32
                    | (data[offset + 1] as i32) << 8
                    | (data[offset + 2] as i32) << 16;
                let v = if v & 0x80_0000 != 0 {
                    v - 0x100_0000
                } else {
                    v
                };
                v as f32 / 8_388_608.0
            }
            32 => {
                i32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]) as f32
                    / 2_147_483_648.0
            }
            _ => 0.0,
        },
        3 => match fmt.bits {
            32 => f32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]),
            64 => f64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]) as f32,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

fn decode_wav(path: &Path, dst_sr: u32) -> Result<(WaveTrack, TrackInfo), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Could not read the file: {e}"))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("This is not a RIFF/WAVE audio file".to_string());
    }

    let mut pos = 12usize;
    let mut fmt: Option<WavFmt> = None;
    let mut audio: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let start = pos + 8;
        if start > bytes.len() {
            break;
        }
        let end = start.saturating_add(size).min(bytes.len());
        if id == b"fmt " {
            fmt = Some(parse_wav_fmt(&bytes[start..end])?);
        } else if id == b"data" {
            audio = Some(&bytes[start..end]);
        }
        pos = start.saturating_add(size).saturating_add(size & 1);
        if size == 0 && id == b"\0\0\0\0" {
            break;
        }
    }

    let fmt = fmt.ok_or_else(|| "The WAV file has no format chunk".to_string())?;
    let data = audio.ok_or_else(|| "The WAV file has no audio data".to_string())?;
    let bytes_per_sample = (fmt.bits as usize / 8).max(1);
    let block_align = bytes_per_sample * fmt.channels as usize;
    if block_align == 0 {
        return Err("The WAV block size is not valid".to_string());
    }
    let frames_in_file = data.len() / block_align;
    if frames_in_file == 0 {
        return Err("The WAV file contains no audio frames".to_string());
    }
    let max_frames = (MAX_WAV_SECONDS * fmt.sample_rate as f64).max(1.0) as usize;
    let truncated = frames_in_file > max_frames;
    let frames = frames_in_file.min(max_frames);

    let mut mono: Vec<[f32; 2]> = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * block_align;
        let left = read_sample(data, base, &fmt);
        let right = if fmt.channels >= 2 {
            read_sample(data, base + bytes_per_sample, &fmt)
        } else {
            left
        };
        // Guard against damaged float samples.
        let left = if left.is_finite() { left } else { 0.0 };
        let right = if right.is_finite() { right } else { 0.0 };
        mono.push([left.clamp(-1.5, 1.5), right.clamp(-1.5, 1.5)]);
    }

    // Linear resampling to the output device's rate.
    let info = TrackInfo {
        duration: frames as f32 / fmt.sample_rate as f32,
        sample_rate: fmt.sample_rate,
        channels: fmt.channels as usize,
        bits: Some(fmt.bits),
        format: if fmt.format == 3 {
            "WAV float".to_string()
        } else {
            "WAV PCM".to_string()
        },
        truncated,
    };
    if fmt.sample_rate == dst_sr {
        return Ok((WaveTrack { frames: mono }, info));
    }
    let dst_frames = ((frames as f64) * dst_sr as f64 / fmt.sample_rate as f64).round() as usize;
    let mut out = Vec::with_capacity(dst_frames.max(1));
    let ratio = fmt.sample_rate as f64 / dst_sr as f64;
    for i in 0..dst_frames {
        let src = i as f64 * ratio;
        let i0 = (src.floor() as usize).min(frames - 1);
        let i1 = (i0 + 1).min(frames - 1);
        let frac = (src - i0 as f64) as f32;
        let a = mono[i0];
        let b = mono[i1];
        out.push([a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]);
    }
    Ok((WaveTrack { frames: out }, info))
}

/// Decode anything Symphonia understands: WAV, MP3, AIFF and MP4/M4A AAC.
/// The original uses Media Foundation plus mpg123 for the same list.
fn decode_with_symphonia(path: &Path, dst_sr: u32) -> Result<(WaveTrack, TrackInfo), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Could not read the file: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(&ext.to_ascii_lowercase());
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unrecognised audio container: {e}"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "The file has no audio track".to_string())?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("No decoder for this audio: {e}"))?;

    let mut mono: Vec<[f32; 2]> = Vec::new();
    let mut sample_rate = params.sample_rate.unwrap_or(0);
    let mut channels = params.channels.map(|c| c.count()).unwrap_or(2);
    let mut bits = params.bits_per_sample.map(|b| b as u16);
    let mut decoded_any = false;
    let mut truncated = false;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(err) => return Err(format!("Could not read the audio stream: {err}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(err) => return Err(format!("Could not decode the audio: {err}")),
        };
        decoded_any = true;
        let spec = *decoded.spec();
        if sample_rate == 0 {
            sample_rate = spec.rate;
        }
        channels = spec.channels.count();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        for frame in samples.chunks(channels.max(1)) {
            let l = frame.first().copied().unwrap_or(0.0);
            let r = if channels >= 2 {
                frame.get(1).copied().unwrap_or(l)
            } else {
                l
            };
            mono.push([
                if l.is_finite() {
                    l.clamp(-1.5, 1.5)
                } else {
                    0.0
                },
                if r.is_finite() {
                    r.clamp(-1.5, 1.5)
                } else {
                    0.0
                },
            ]);
        }
        if sample_rate > 0 && mono.len() as f64 / sample_rate as f64 > MAX_TRACK_SECONDS {
            let keep = (MAX_TRACK_SECONDS * sample_rate as f64) as usize;
            mono.truncate(keep);
            truncated = true;
            break;
        }
    }

    if !decoded_any || mono.is_empty() {
        return Err("The file contains no audio frames".to_string());
    }
    if sample_rate == 0 {
        return Err("The audio file has no sample rate".to_string());
    }

    let frames_in = mono.len();
    let info = TrackInfo {
        duration: frames_in as f32 / sample_rate as f32,
        sample_rate,
        channels,
        bits,
        format: file_kind(path),
        truncated,
    };
    if sample_rate == dst_sr {
        return Ok((WaveTrack { frames: mono }, info));
    }
    let dst_frames = ((frames_in as f64) * dst_sr as f64 / sample_rate as f64).round() as usize;
    let mut out = Vec::with_capacity(dst_frames.max(1));
    let ratio = sample_rate as f64 / dst_sr as f64;
    for i in 0..dst_frames {
        let src = i as f64 * ratio;
        let i0 = (src.floor() as usize).min(frames_in - 1);
        let i1 = (i0 + 1).min(frames_in - 1);
        let frac = (src - i0 as f64) as f32;
        let a = mono[i0];
        let b = mono[i1];
        out.push([a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]);
    }
    Ok((WaveTrack { frames: out }, info))
}

fn file_kind(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("wav") | Some("bwf") => "WAV".to_string(),
        Some("mp3") => "MP3".to_string(),
        Some("aif") | Some("aiff") => "AIFF".to_string(),
        Some("m4a") | Some("mp4") => "M4A/AAC".to_string(),
        _ => "Audio".to_string(),
    }
}

/// Try the proven WAV reader first for RIFF files, then Symphonia for the
/// formats the original's filter also lists (mp3, aif, m4a, bwf).
fn decode_audio(path: &Path, dst_sr: u32) -> Result<(WaveTrack, TrackInfo), String> {
    let mut head = [0u8; 12];
    let riff = std::fs::File::open(path)
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut head).map(|_| ())
        })
        .map(|_| &head[0..4] == b"RIFF" || &head[0..4] == b"RF64")
        .unwrap_or(false);

    if riff {
        match decode_wav(path, dst_sr) {
            Ok(result) => return Ok(result),
            Err(wav_err) => match decode_with_symphonia(path, dst_sr) {
                Ok(result) => return Ok(result),
                Err(_) => return Err(format!("This WAV file could not be read: {wav_err}")),
            },
        }
    }
    decode_with_symphonia(path, dst_sr).map_err(|_| {
        "This is not an audio file this player can read (supported: WAV, MP3, AIFF, BWF, M4A)"
            .to_string()
    })
}

// ---------------------------------------------------------------------------
// Native file picker
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn pick_audio_file() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OPENFILENAMEW};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    let wide = |s: &str| -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    };

    // FUN_0053df30 at 0x53df30: filter "Audio (wav, mp3, bwf, aif, m4a)"
    // (0x56d058), title "Select Audio File" (0x56d170), Flags 0x821000 and
    // a 1000-character path buffer. The original also passes its own window
    // as hwndOwner, so the dialog is modal to the console and opens over it.
    let filter =
        "Audio (wav, mp3, bwf, aif, m4a)\0*.wav;*.mp3;*.bwf;*.aif;*.m4a\0All files\0*.*\0\0"
            .encode_utf16()
            .collect::<Vec<u16>>();
    let title = wide("Select Audio File");
    let mut file_buf = vec![0u16; 1000];

    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    // Without an owner the dialog opens at the desktop's top-left corner and
    // can end up behind other windows, which looked like "nothing happened".
    ofn.hwndOwner = unsafe { GetActiveWindow() };
    ofn.lpstrFilter = filter.as_ptr();
    // FUN_0053df30 sets nFilterIndex = 0 (the first filter, i.e. the audio
    // filter above), not 1.
    ofn.nFilterIndex = 0;
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = 1000;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = 0x0082_1000; // OFN_ENABLESIZING | OFN_NONETWORKBUTTON | OFN_FILEMUSTEXIST

    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        return None;
    }
    let len = file_buf
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(file_buf.len());
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(
        &file_buf[..len],
    )))
}

#[cfg(not(windows))]
pub fn pick_audio_file() -> Option<PathBuf> {
    None
}

// ---------------------------------------------------------------------------
// Self test
// ---------------------------------------------------------------------------

pub fn selftest() {
    let mut engine = AudioEngine::new();
    println!(
        "audio available={} device={:?} rate={} channels={} status={}",
        engine.available, engine.device, engine.sample_rate, engine.channels, engine.status
    );
    println!("outputs={:?}", AudioEngine::output_devices());
    if !engine.available {
        return;
    }
    for line in AudioEngine::probe_outputs() {
        println!("output {line}");
    }
    for kind in DemoKind::ALL {
        let sr = engine.sample_rate.max(8_000);
        let mut peak = 0.0f32;
        let mut sum = 0.0f64;
        let frames = sr as u64 * 8;
        for pos in 0..frames {
            let s = demo_sample(kind, pos as f64 / sr as f64, pos);
            peak = peak.max(s[0].abs()).max(s[1].abs());
            sum += (s[0] as f64).powi(2) + (s[1] as f64).powi(2);
        }
        let rms = (sum / (frames as f64 * 2.0)).sqrt();
        println!("demo {} peak={peak:.3} rms={rms:.3}", kind.file_name());
    }
    engine.load_demo(DemoKind::Track1);
    engine.set_tape(-18.0, [true, false, false, false, false]);
    engine.set_bus(0, 0.0, false, 0, true, [0.0; 6]);
    engine.play();
    std::thread::sleep(std::time::Duration::from_millis(1600));
    println!(
        "peak={:.4} playing={} position={:.3}s",
        engine.peak(),
        engine.is_playing(),
        engine.position()
    );

    // FUN_00527ad0 validation, without touching the open stream.
    let rate_err = engine.apply_format(12_345, 1024, None);
    let buf_err = engine.apply_format(48_000, 4096, None);
    println!(
        "format rules: bad_rate_rejected={} bad_buffer_rejected={}",
        rate_err.is_err(),
        buf_err.is_err()
    );

    // RecordMode Loop: on -> wrap and keep playing, off -> stop at the end.
    engine.set_loop(true);
    engine.seek(DEMO_LEN_SECS - 0.02);
    engine.play();
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!(
        "loop on: playing={} position={:.3}s",
        engine.is_playing(),
        engine.position()
    );
    engine.set_loop(false);
    engine.seek(DEMO_LEN_SECS - 0.02);
    engine.play();
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!(
        "loop off: playing={} finished={} position={:.3}s",
        engine.is_playing(),
        engine.take_finished(),
        engine.position()
    );
}

pub fn wav_selftest() {
    let path = std::env::temp_dir().join("audio_mixer_pro_selftest.wav");
    let sample_rate = 44_100u32;
    let frames = sample_rate as usize * 3;
    let channels = 2u16;
    let data_len = frames * channels as usize * 2;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
    bytes.extend_from_slice(&(channels * 2).to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for i in 0..frames {
        let phase = i as f32 * 440.0 * std::f32::consts::TAU / sample_rate as f32;
        let v = (phase.sin() * 0.25 * 32767.0) as i16;
        for _ in 0..channels {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
    }
    if let Err(err) = std::fs::write(&path, &bytes) {
        println!("could not write test wav: {err}");
        return;
    }
    let mut engine = AudioEngine::new();
    println!(
        "wav test: available={} device={:?} rate={}",
        engine.available, engine.device, engine.sample_rate
    );
    match engine.load_wav(&path) {
        Ok(info) => {
            println!(
                "loaded test wav: duration={:.3}s {}",
                info.duration,
                info.detail()
            );
            engine.set_tape(-6.0, [true, false, false, false, false]);
            engine.set_bus(0, 0.0, false, 0, true, [0.0; 6]);
            engine.play();
            std::thread::sleep(std::time::Duration::from_millis(1200));
            println!(
                "wav playback peak={:.4} playing={} position={:.3}s",
                engine.peak(),
                engine.is_playing(),
                engine.position()
            );
        }
        Err(err) => println!("could not load test wav: {err}"),
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    #[test]
    fn reader_trims_startup_backlog() {
        let mut ring = SampleRing::new();
        for _ in 0..48_000 {
            ring.push([0.25, 0.25]);
        }
        let mut reader = InputReader::new();
        let _ = reader.next(&mut ring, 48_000, 48_000);
        assert!(
            ring.fill() <= 1_201,
            "ring fill was {} frames after the startup trim",
            ring.fill()
        );
    }

    #[test]
    fn reader_drops_slow_clock_drift() {
        let mut ring = SampleRing::new();
        for _ in 0..2_000 {
            ring.push([0.1, 0.1]);
        }
        let mut reader = InputReader::new();
        let _ = reader.next(&mut ring, 48_000, 48_000);
        for _ in 0..1_000 {
            ring.push([0.1, 0.1]);
        }
        let before = ring.fill();
        for _ in 0..1_000 {
            let _ = reader.next(&mut ring, 48_000, 48_000);
        }
        assert!(ring.fill() < before, "drift controller did not catch up");
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    // Diagnostic only: decodes every supported audio file under
    // AUDIO_PROBE_DIR so the real-world formats this build meets can be
    // checked without opening the window.
    #[test]
    fn decodes_real_world_audio() {
        let Some(dir) = std::env::var_os("AUDIO_PROBE_DIR") else {
            return;
        };
        let mut checked = 0usize;
        let mut failed = 0usize;
        for entry in walk(std::path::Path::new(&dir)) {
            let (track, info) = match decode_audio(&entry, 48_000) {
                Ok(t) => t,
                Err(err) => {
                    failed += 1;
                    println!("FAIL {}: {err}", entry.display());
                    continue;
                }
            };
            checked += 1;
            if checked <= 8 {
                let peak = track
                    .frames
                    .iter()
                    .fold(0.0f32, |m, f| m.max(f[0].abs()).max(f[1].abs()));
                println!(
                    "OK {} [{}] frames={} seconds={:.2} peak={:.3}",
                    entry.display(),
                    info.detail(),
                    track.frames.len(),
                    track.frames.len() as f32 / 48_000.0,
                    peak
                );
            }
        }
        println!("decoded {checked} audio files, {failed} failed");
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(read) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    matches!(
                        e.to_ascii_lowercase().as_str(),
                        "wav" | "bwf" | "mp3" | "aif" | "aiff" | "m4a"
                    )
                })
                .unwrap_or(false)
            {
                out.push(path);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Routing self test (no audio device needed)
// ---------------------------------------------------------------------------

/// Render a known tone through the same mixer the output callback uses and
/// report the level with A1 on, A1 off and the strip muted. This proves the
/// routing buttons reach the audio path even on a machine with no speakers.
pub fn routing_selftest() {
    let shared = Shared::new();
    shared.sample_rate.store(48_000, Ordering::Relaxed);
    let input = Arc::new(InputShared::new());
    input.rate.store(48_000, Ordering::Relaxed);
    {
        let mut ring = lock_source(&input.ring);
        for i in 0..48_000 {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
            ring.push([v, v]);
        }
    }
    {
        let mut inputs = lock_source(&shared.inputs);
        inputs[0] = Some(input);
    }
    // Strip 1 fader at 0 dB, A1 on. All other strips off.
    shared.strips[0]
        .gain_db
        .store(0.0f32.to_bits(), Ordering::Relaxed);
    shared.strips[0]
        .route
        .store(STRIP_ROUTE_A1, Ordering::Relaxed);
    for i in 1..3 {
        shared.strips[i].route.store(0, Ordering::Relaxed);
    }

    let mut dsp = Dsp::new();
    let mut first = vec![0.0f32; 48_000 * 2];
    dsp.render(&mut first, 2, &shared);
    let on = first.iter().fold(0.0f32, |m, v| m.max(v.abs()));

    // A1 off: the same input must be silent.
    shared.strips[0].route.store(0, Ordering::Relaxed);
    let mut second = vec![0.0f32; 48_000 * 2];
    dsp.render(&mut second, 2, &shared);
    let off = second.iter().fold(0.0f32, |m, v| m.max(v.abs()));

    // A1 on but the strip muted: silent again.
    shared.strips[0]
        .route
        .store(STRIP_ROUTE_A1, Ordering::Relaxed);
    shared.strips[0].flags.store(1, Ordering::Relaxed);
    let mut third = vec![0.0f32; 48_000 * 2];
    dsp.render(&mut third, 2, &shared);
    let muted = third.iter().fold(0.0f32, |m, v| m.max(v.abs()));

    println!(
        "routing: a1_on={on:.4} a1_off={off:.6} muted={muted:.6} pass={}",
        on > 0.1 && off < 0.001 && muted < 0.001
    );
}

/// Prove B1/B2 reach the capture side of their USB cables. Feeds a sine
/// through a strip routed to each virtual bus, renders one block, then reads
/// the PCM the cable would hand to a recording application.
pub fn virtual_bus_selftest() {
    let b1 = run_virtual_bus(STRIP_ROUTE_B1, 0, 1);
    let b2 = run_virtual_bus(STRIP_ROUTE_B2, 1, 2);
    let pass = b1 > 0.1 && b2 > 0.1;
    println!("virtual buses: b1_peak={b1:.4} b2_peak={b2:.4} pass={pass}");
}

fn run_virtual_bus(route: u32, slot: usize, number: u8) -> f32 {
    let shared = Shared::new();
    shared.sample_rate.store(48_000, Ordering::Relaxed);
    let input = Arc::new(InputShared::new());
    input.rate.store(48_000, Ordering::Relaxed);
    {
        let mut ring = lock_source(&input.ring);
        for i in 0..48_000 {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
            ring.push([v, v]);
        }
    }
    {
        let mut inputs = lock_source(&shared.inputs);
        inputs[0] = Some(input);
    }
    shared.strips[0]
        .gain_db
        .store(0.0f32.to_bits(), Ordering::Relaxed);
    shared.strips[0].route.store(route, Ordering::Relaxed);
    for i in 1..5 {
        shared.strips[i].route.store(0, Ordering::Relaxed);
    }

    let device = make_capture_device(number);
    {
        let mut outputs = lock_source(&shared.virtual_outputs);
        outputs[slot] = Some(device.clone());
    }

    // Keep the block below the input reader's 25 ms target so the test does
    // not mistake the intentional startup trim for lost audio.
    let frames = 512usize;
    let mut dsp = Dsp::new();
    let mut out = vec![0.0f32; frames * 2];
    dsp.render(&mut out, 2, &shared);
    read_pcm_peak(&device, frames)
}

fn make_capture_device(number: u8) -> Arc<Device> {
    use virtual_cables::uac::{SetupPacket, REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE};
    let device = Device::new(number, 250, Mode::Uac2, 48_000).expect("virtual cable");
    let (_, status) = device.handle_control(
        SetupPacket {
            request_type: 0,
            request: REQUEST_SET_CONFIGURATION,
            value: 1,
            index: 0,
            length: 0,
        },
        &[],
    );
    assert_eq!(status, 0);
    let (_, status) = device.handle_control(
        SetupPacket {
            request_type: 0x01,
            request: REQUEST_SET_INTERFACE,
            value: 3,
            index: 2,
            length: 0,
        },
        &[],
    );
    assert_eq!(status, 0);
    Arc::new(device)
}

fn read_pcm_peak(device: &Device, frames: usize) -> f32 {
    let frame_bytes = device.capture_frame_bytes();
    let mut pcm = vec![0u8; frames * frame_bytes];
    device.read_capture(&mut pcm);
    let mut peak = 0.0f32;
    for chunk in pcm.chunks_exact(4) {
        let value =
            i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32 / i32::MAX as f32;
        peak = peak.max(value.abs());
    }
    peak
}

/// Open a capture endpoint and report the loudest sample seen. Used to prove
/// that a virtual bus really arrived at its cable's recording side.
pub fn cable_peak(name: Option<&str>, seconds: f32) {
    let capture = InputCapture::open(name);
    if !capture.available {
        println!(
            "cable peak: could not open {:?}: {}",
            name.unwrap_or("default"),
            capture.status
        );
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f32(seconds.max(0.1));
    let mut peak = 0.0f32;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
        peak = peak.max(capture.peak());
    }
    println!("cable peak: device={:?} peak={peak:.4}", capture.device);
}

/// Open the default Windows input and report what the capture callback sees.
/// Used to tell "no signal at the microphone" apart from "routing broken".
/// End-to-end check of the HW INPUT 1 VST insert: feed a known sine through a
/// real capture ring and `Dsp::render`, with and without a gain change made
/// through the plug-in's own parameter. This proves the plug-in sits in the
/// signal path, not merely that it can load.
pub fn vst_mix_selftest(path: &Path, report_path: &Path) {
    let mut report = String::new();
    let shared = Arc::new(Shared::new());
    shared.sample_rate.store(48_000, Ordering::Relaxed);
    shared.strips[0]
        .route
        .store(STRIP_ROUTE_A1, Ordering::Relaxed);
    shared.strips[0]
        .gain_db
        .store(0.0f32.to_bits(), Ordering::Relaxed);

    let input = Arc::new(InputShared::new());
    {
        let mut ring = lock_source(&input.ring);
        for i in 0..96_000 {
            let t = i as f32 / 48_000.0;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
            ring.push([v, v]);
        }
    }
    {
        let mut slots = lock_source(&shared.inputs);
        slots[0] = Some(input);
    }

    match shared.vsts[0].load(path, 48_000.0, crate::vst::VST_MAX_BLOCK) {
        Ok(info) => {
            report.push_str(&format!("plug-in: {} by {}\n", info.name, info.vendor));
            let params = shared.vsts[0].parameters();
            let gain_id = params
                .iter()
                .find(|param| param.name.eq_ignore_ascii_case("OutputVol"))
                .map(|param| param.id);
            report.push_str(&format!(
                "output gain parameter: {}\n",
                gain_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "not found".to_string())
            ));

            fn render_peak(dsp: &mut Dsp, shared: &Shared, buf: &mut [f32], blocks: usize) -> f32 {
                let mut peak = 0.0f32;
                for _ in 0..blocks {
                    dsp.render(buf, 2, shared);
                    for value in buf.iter() {
                        peak = peak.max(value.abs());
                    }
                }
                peak
            }

            let mut dsp = Dsp::new();
            let mut buf = vec![0.0f32; 1024];

            // Let the bus fader reach 0 dB and the plug-in settle.
            let dry_peak = render_peak(&mut dsp, &shared, &mut buf, 80);
            report.push_str(&format!("plugin pass-through peak: {dry_peak:.6}\n"));

            let mut cut_peak = dry_peak;
            if let Some(id) = gain_id {
                if shared.vsts[0].set_parameter(id, 0.0).is_ok() {
                    let _ = render_peak(&mut dsp, &shared, &mut buf, 20);
                    cut_peak = render_peak(&mut dsp, &shared, &mut buf, 8);
                    report.push_str(&format!("peak after plug-in OutputVol=0: {cut_peak:.6}\n"));
                    let _ = shared.vsts[0].set_parameter(id, 0.5);
                    let _ = render_peak(&mut dsp, &shared, &mut buf, 12);
                    let restored = render_peak(&mut dsp, &shared, &mut buf, 8);
                    report.push_str(&format!("peak after OutputVol restored: {restored:.6}\n"));
                    let pass = dry_peak > 0.1 && cut_peak < dry_peak * 0.5 && restored > 0.1;
                    report.push_str(&format!("pass={pass}\n"));
                } else {
                    report.push_str("pass=false (gain parameter not settable)\n");
                }
            } else {
                let pass = dry_peak > 0.1;
                report.push_str(&format!("pass={pass} (no OutputVol on this plug-in)\n"));
            }
        }
        Err(err) => report.push_str(&format!("FAIL: {err}\n")),
    }
    let _ = std::fs::write(report_path, &report);
    eprintln!("{report}");
}

/// Check that two input strips really have independent VST inserts: load the
/// same plug-in in slots 1 and 2, cut one slot's own output gain, and confirm
/// the other strip's level does not move. This is the headless proof for the
/// person's "separate plug-ins on each channel" request.
pub fn vst_multichannel_selftest(path: &Path, report_path: &Path) {
    let mut report = String::new();
    let shared = Arc::new(Shared::new());
    shared.sample_rate.store(48_000, Ordering::Relaxed);

    // Strip 1 is a hardware input and strip 5 is VIRTUAL 2, so the proof also
    // covers the virtual inputs the person asked to extend.
    let strips = [0usize, 4usize];
    let mut inputs = Vec::new();
    for &i in &strips {
        shared.strips[i]
            .route
            .store(STRIP_ROUTE_A1, Ordering::Relaxed);
        shared.strips[i]
            .gain_db
            .store(0.0f32.to_bits(), Ordering::Relaxed);
        let input = Arc::new(InputShared::new());
        {
            let mut ring = lock_source(&input.ring);
            for frame in 0..192_000 {
                let t = frame as f32 / 48_000.0;
                let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
                ring.push([v, v]);
            }
        }
        inputs.push(input);
    }
    {
        let mut slots = lock_source(&shared.inputs);
        slots[0] = Some(inputs[0].clone());
        slots[4] = Some(inputs[1].clone());
    }

    let load = |slot: usize| shared.vsts[slot].load(path, 48_000.0, crate::vst::VST_MAX_BLOCK);
    let info0 = match load(0) {
        Ok(info) => info,
        Err(err) => {
            let _ = std::fs::write(report_path, format!("FAIL slot 1: {err}\n"));
            eprintln!("FAIL slot 1: {err}");
            return;
        }
    };
    let info4 = match load(4) {
        Ok(info) => info,
        Err(err) => {
            let _ = std::fs::write(report_path, format!("FAIL slot 5: {err}\n"));
            eprintln!("FAIL slot 5: {err}");
            return;
        }
    };
    report.push_str(&format!(
        "slot 1 (HW INPUT 1): {} by {}\nslot 5 (VIRTUAL 2): {} by {}\n",
        info0.name, info0.vendor, info4.name, info4.vendor
    ));

    let gain_id = |slot: usize| {
        shared.vsts[slot]
            .parameters()
            .iter()
            .find(|param| param.name.eq_ignore_ascii_case("OutputVol"))
            .map(|param| param.id)
    };
    let gain0 = gain_id(0);
    let gain4 = gain_id(4);
    report.push_str(&format!(
        "output gain parameters: slot1={:?} slot5={:?}\n",
        gain0, gain4
    ));
    let (Some(gain0), Some(gain4)) = (gain0, gain4) else {
        let _ = std::fs::write(report_path, format!("{report}pass=false (no OutputVol)\n"));
        eprintln!("{report}pass=false (no OutputVol)");
        return;
    };

    fn render_peak(dsp: &mut Dsp, shared: &Shared, buf: &mut [f32], blocks: usize) -> f32 {
        let mut peak = 0.0f32;
        for _ in 0..blocks {
            dsp.render(buf, 2, shared);
            for value in buf.iter() {
                peak = peak.max(value.abs());
            }
        }
        peak
    }

    let mut dsp = Dsp::new();
    let mut buf = vec![0.0f32; 1024];
    let both = render_peak(&mut dsp, &shared, &mut buf, 80);

    let _ = shared.vsts[4].set_parameter(gain4, 0.0);
    let only_first = render_peak(&mut dsp, &shared, &mut buf, 20);
    let _ = shared.vsts[4].set_parameter(gain4, 0.5);
    let _ = shared.vsts[0].set_parameter(gain0, 0.0);
    let only_second = render_peak(&mut dsp, &shared, &mut buf, 20);
    let _ = shared.vsts[4].set_parameter(gain4, 0.0);
    let neither = render_peak(&mut dsp, &shared, &mut buf, 20);
    let _ = shared.vsts[0].set_parameter(gain0, 0.5);
    let _ = shared.vsts[4].set_parameter(gain4, 0.5);
    let restored = render_peak(&mut dsp, &shared, &mut buf, 40);

    report.push_str(&format!(
        "both pass-through: {both:.6}\nvirtual slot cut only: {only_first:.6}\nhardware slot cut only: {only_second:.6}\nboth cut: {neither:.6}\nrestored both: {restored:.6}\n"
    ));
    // Measured reference peaks for this check. Keeping them here makes every
    // run assert the same independent-channel result instead of only printing
    // numbers that happen to look reasonable.
    const VST_MULTI_PEAK_BOTH: f32 = 0.275532;
    const VST_MULTI_PEAK_ONE: f32 = 0.154320;
    const VST_MULTI_PEAK_NONE: f32 = 0.028277;
    let independent = (both - VST_MULTI_PEAK_BOTH).abs() < 0.01
        && (only_first - VST_MULTI_PEAK_ONE).abs() < 0.01
        && (only_second - VST_MULTI_PEAK_ONE).abs() < 0.01
        && (neither - VST_MULTI_PEAK_NONE).abs() < 0.01
        && restored > both * 0.9;
    report.push_str(&format!("independent={independent} pass={independent}\n"));
    let _ = std::fs::write(report_path, &report);
    eprintln!("{report}");
}

pub fn input_selftest() {
    println!("inputs={:?}", InputCapture::devices());
    let capture = InputCapture::open(None);
    println!(
        "input available={} device={:?} rate={} channels={} status={}",
        capture.available, capture.device, capture.sample_rate, capture.channels, capture.status
    );
    if !capture.available {
        return;
    }
    let mut peak = 0.0f32;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        peak = peak.max(capture.peak());
    }
    let ring_frames = lock_source(&capture.shared.ring).count;
    println!("input peak={peak:.4} buffered_frames={ring_frames}");
}
