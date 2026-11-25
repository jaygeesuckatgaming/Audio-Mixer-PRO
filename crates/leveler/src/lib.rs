//! Audio Mixer PRO "Leveler" - a small CLAP plug-in that keeps a stereo signal
//! between a minimum and a maximum level, with a separate peak ceiling.
//!
//! Parameters:
//!
//!   * `Min` / `Max` - the **RMS band** (default -16 .. -12 dBFS): a quiet
//!     source is lifted toward `Min`, a loud one is pulled down to `Max`.
//!     Material already inside the band keeps unity gain.
//!   * `Attack` / `Release` - how fast the leveling gain moves, in ms.
//!   * `Ceiling` - the peak limiter's ceiling (default -1 dBFS), independent of
//!     the band, so loud transients are caught without clipping.
//!
//! Fixed internals: stereo-linked RMS detector (300 ms), gain held while the
//! input is below -80 dBFS (silence is never amplified), at most 24 dB of lift.
//!
//! State is saved as five little-endian f32s after a `LVL1` tag, so the console
//! restores the person's settings with the scene.
//!
//! Build with `cargo build --release` and copy `target/release/leveler.dll`
//! into a CLAP folder the console scans (e.g.
//! `%LOCALAPPDATA%\Programs\Common\CLAP\Leveler.clap`).

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts,
    PluginAudioPortsImpl,
};
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::stream::{InputStream, OutputStream};
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::prelude::*;
use clack_plugin::utils::Cookie;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const PARAM_MIN_ID: u32 = 1;
const PARAM_MAX_ID: u32 = 2;
const PARAM_ATTACK_ID: u32 = 3;
const PARAM_RELEASE_ID: u32 = 4;
const PARAM_CEILING_ID: u32 = 5;

/// Default RMS band bottom.
const MIN_DB: f32 = -16.0;
/// Default RMS band top. Not the peak ceiling.
const MAX_DB: f32 = -12.0;
/// Default gain attack / release.
const ATTACK_MS: f32 = 8.0;
const RELEASE_MS: f32 = 500.0;
/// Default peak ceiling, independent of the RMS band.
const CEILING_DB: f32 = -1.0;
/// Most the leveler will lift a quiet source.
const MAX_LIFT_DB: f32 = 24.0;
/// Below this input level the gain freezes (silence must not be amplified).
const FLOOR_DB: f32 = -80.0;
/// The detector's own time constant: what "level" means here.
const DETECTOR_MS: f32 = 300.0;

const RANGE_DB: (f32, f32) = (-60.0, 0.0);
const RANGE_ATTACK: (f32, f32) = (0.5, 200.0);
const RANGE_RELEASE: (f32, f32) = (20.0, 2000.0);
const RANGE_CEILING: (f32, f32) = (-24.0, 0.0);

/// State tag + five f32s. Bump the tag if the layout ever changes.
const STATE_MAGIC: [u8; 4] = *b"LVL1";
const STATE_LEN: usize = 4 + 5 * 4;

pub struct Leveler;

pub struct LevelerShared {
    level: Arc<Level>,
}

/// Values shared between the main thread (parameters, state) and the audio
/// thread. Plain atomics: a parameter move is picked up on the next block.
pub struct Level {
    min_db: AtomicU32,
    max_db: AtomicU32,
    attack_ms: AtomicU32,
    release_ms: AtomicU32,
    ceiling_db: AtomicU32,
}

impl Level {
    fn new() -> Self {
        Self {
            min_db: AtomicU32::new(MIN_DB.to_bits()),
            max_db: AtomicU32::new(MAX_DB.to_bits()),
            attack_ms: AtomicU32::new(ATTACK_MS.to_bits()),
            release_ms: AtomicU32::new(RELEASE_MS.to_bits()),
            ceiling_db: AtomicU32::new(CEILING_DB.to_bits()),
        }
    }

    fn get(&self, which: &AtomicU32) -> f32 {
        f32::from_bits(which.load(Ordering::Relaxed))
    }

    fn min_db(&self) -> f32 {
        self.get(&self.min_db)
    }

    fn max_db(&self) -> f32 {
        self.get(&self.max_db)
    }

    fn attack_ms(&self) -> f32 {
        self.get(&self.attack_ms)
    }

    fn release_ms(&self) -> f32 {
        self.get(&self.release_ms)
    }

    fn ceiling_db(&self) -> f32 {
        self.get(&self.ceiling_db)
    }

    /// Store one parameter, clamped to the range the plug-in advertises.
    fn set(&self, id: ClapId, value: f64) {
        let value = value as f32;
        match id.get() {
            PARAM_MIN_ID => store(&self.min_db, clamp(value, RANGE_DB)),
            PARAM_MAX_ID => store(&self.max_db, clamp(value, RANGE_DB)),
            PARAM_ATTACK_ID => store(&self.attack_ms, clamp(value, RANGE_ATTACK)),
            PARAM_RELEASE_ID => store(&self.release_ms, clamp(value, RANGE_RELEASE)),
            PARAM_CEILING_ID => store(&self.ceiling_db, clamp(value, RANGE_CEILING)),
            _ => {}
        }
    }

    fn set_all(&self, min: f32, max: f32, attack: f32, release: f32, ceiling: f32) {
        store(&self.min_db, clamp(min, RANGE_DB));
        store(&self.max_db, clamp(max, RANGE_DB));
        store(&self.attack_ms, clamp(attack, RANGE_ATTACK));
        store(&self.release_ms, clamp(release, RANGE_RELEASE));
        store(&self.ceiling_db, clamp(ceiling, RANGE_CEILING));
    }

    /// Read back a parameter by its CLAP id, for `get_value` and for state.
    fn value(&self, id: ClapId) -> Option<f64> {
        let value = match id.get() {
            PARAM_MIN_ID => self.min_db(),
            PARAM_MAX_ID => self.max_db(),
            PARAM_ATTACK_ID => self.attack_ms(),
            PARAM_RELEASE_ID => self.release_ms(),
            PARAM_CEILING_ID => self.ceiling_db(),
            _ => return None,
        };
        Some(value as f64)
    }

    /// The five values, in the order the state blob stores them.
    fn snapshot(&self) -> [f32; 5] {
        [
            self.min_db(),
            self.max_db(),
            self.attack_ms(),
            self.release_ms(),
            self.ceiling_db(),
        ]
    }
}

fn clamp(value: f32, range: (f32, f32)) -> f32 {
    value.clamp(range.0, range.1)
}

fn store(slot: &AtomicU32, value: f32) {
    slot.store(value.to_bits(), Ordering::Relaxed);
}

pub struct LevelerMain {
    level: Arc<Level>,
}

impl<'a> PluginShared<'a> for LevelerShared {}

impl<'a> PluginMainThread<'a, LevelerShared> for LevelerMain {}

impl PluginAudioPortsImpl for LevelerMain {
    fn count(&self, _is_input: bool) -> u32 {
        1
    }

    fn get(&self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: if is_input { b"Stereo In" } else { b"Stereo Out" },
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginMainThreadParams for LevelerMain {
    fn count(&self) -> u32 {
        5
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        let (id, name, range, default) = match param_index {
            0 => (PARAM_MIN_ID, b"Min".as_slice(), RANGE_DB, MIN_DB),
            1 => (PARAM_MAX_ID, b"Max".as_slice(), RANGE_DB, MAX_DB),
            2 => (PARAM_ATTACK_ID, b"Attack".as_slice(), RANGE_ATTACK, ATTACK_MS),
            3 => (PARAM_RELEASE_ID, b"Release".as_slice(), RANGE_RELEASE, RELEASE_MS),
            4 => (PARAM_CEILING_ID, b"Ceiling".as_slice(), RANGE_CEILING, CEILING_DB),
            _ => return,
        };
        info.set(&ParamInfo {
            id: ClapId::new(id),
            flags: ParamInfoFlags::IS_AUTOMATABLE,
            cookie: Cookie::empty(),
            name,
            module: b"",
            min_value: range.0 as f64,
            max_value: range.1 as f64,
            default_value: default as f64,
        });
    }

    fn get_value(&self, param_id: ClapId) -> Option<f64> {
        self.level.value(param_id)
    }

    fn value_to_text(
        &self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> core::fmt::Result {
        match param_id.get() {
            PARAM_ATTACK_ID | PARAM_RELEASE_ID => write!(writer, "{value:.1} ms"),
            _ => write!(writer, "{value:.1} dB"),
        }
    }

    fn text_to_value(&self, param_id: ClapId, text: &CStr) -> Option<f64> {
        let unit = match param_id.get() {
            PARAM_ATTACK_ID | PARAM_RELEASE_ID => "ms",
            _ => "dB",
        };
        let text = text.to_str().ok()?.trim();
        let number = text.strip_suffix(unit).unwrap_or(text).trim();
        number.parse::<f64>().ok()
    }

    fn flush(&self, input: &InputEvents, _output: &mut OutputEvents) {
        for event in input {
            if let Some(param) = event.as_event::<ParamValueEvent>() {
                if let Some(id) = param.param_id() {
                    self.level.set(id, param.value());
                }
            }
        }
    }
}

impl PluginStateImpl for LevelerMain {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        let mut bytes = [0u8; STATE_LEN];
        bytes[..4].copy_from_slice(&STATE_MAGIC);
        for (i, value) in self.level.snapshot().iter().enumerate() {
            let at = 4 + i * 4;
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        output
            .write_all(&bytes)
            .map_err(|_| PluginError::Message("could not write the Leveler state"))
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut bytes = [0u8; STATE_LEN];
        let mut filled = 0;
        // Read what is there: an older blob (five f32s, no tag) is still valid.
        while filled < STATE_LEN {
            match input.read(&mut bytes[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => break,
            }
        }
        if filled < 20 {
            return Err(PluginError::Message("the Leveler state is too short"));
        }
        let offset = if filled >= STATE_LEN && bytes[..4] == STATE_MAGIC {
            4
        } else {
            0
        };
        let mut values = [0f32; 5];
        for (i, value) in values.iter_mut().enumerate() {
            let at = offset + i * 4;
            let Some(chunk) = bytes.get(at..at + 4) else {
                return Err(PluginError::Message("the Leveler state is truncated"));
            };
            let Ok(chunk) = <[u8; 4]>::try_from(chunk) else {
                return Err(PluginError::Message("the Leveler state is malformed"));
            };
            *value = f32::from_le_bytes(chunk);
        }
        // Anything that is not a finite number (a corrupt blob) falls back to
        // the default rather than poisoning the DSP with NaN.
        let fallback = [MIN_DB, MAX_DB, ATTACK_MS, RELEASE_MS, CEILING_DB];
        for (value, default) in values.iter_mut().zip(fallback) {
            if !value.is_finite() {
                *value = default;
            }
        }
        self.level.set_all(
            values[0], values[1], values[2], values[3], values[4],
        );
        Ok(())
    }
}

impl Plugin for Leveler {
    type AudioProcessor<'a> = LevelerAudio;
    type Shared<'a> = LevelerShared;
    type MainThread<'a> = LevelerMain;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&Self::Shared<'_>>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginParams>()
            .register::<PluginState>();
    }
}

impl DefaultPluginFactory for Leveler {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new("com.jaygee.leveler", "Leveler")
            .with_vendor("JayGee")
            .with_version("0.4.0")
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        Ok(LevelerShared {
            level: Arc::new(Level::new()),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(LevelerMain {
            level: Arc::clone(&shared.level),
        })
    }
}

pub struct LevelerAudio {
    level: Arc<Level>,
    /// Mean square, for the level measurement (RMS, not peak).
    mean_sq: [f32; 2],
    /// Current leveling gain.
    gain: [f32; 2],
    /// Peak envelope for the ceiling limiter.
    peak_env: [f32; 2],
    /// Sample rate, so the parameter values can be turned into coefficients.
    rate: f32,
    detector_attack: f32,
    detector_release: f32,
    gain_attack: f32,
    gain_release: f32,
    peak_release: f32,
    /// Coefficients derived from the parameters, refreshed once per block.
    ceiling: f32,
    /// The attack/release the coefficients were built from (ms), so a block
    /// only pays for the exp calls when a parameter actually moved.
    built_from: (f32, f32, f32),
}

impl LevelerAudio {
    fn apply_events(&mut self, events: &InputEvents) {
        for event in events {
            if let Some(param) = event.as_event::<ParamValueEvent>() {
                if let Some(id) = param.param_id() {
                    self.level.set(id, param.value());
                }
            }
        }
    }

    /// Rebuild the time-dependent coefficients if a parameter changed.
    fn refresh(&mut self) {
        let attack = self.level.attack_ms();
        let release = self.level.release_ms();
        let ceiling_db = self.level.ceiling_db();
        if (attack, release, ceiling_db) == self.built_from {
            return;
        }
        let coeff = |ms: f32| 1.0 - (-1.0 / (self.rate * ms.max(0.01) / 1000.0)).exp();
        self.gain_attack = coeff(attack);
        self.gain_release = coeff(release);
        self.ceiling = 10f32.powf(ceiling_db / 20.0);
        self.built_from = (attack, release, ceiling_db);
    }

    fn step(&mut self, channel: usize, x: f32) -> f32 {
        let ch = channel & 1;

        // RMS over ~300 ms: what "level" means here.
        let sq = x * x;
        let coeff = if sq > self.mean_sq[ch] {
            self.detector_attack
        } else {
            self.detector_release
        };
        self.mean_sq[ch] += (sq - self.mean_sq[ch]) * coeff;
        let rms = self.mean_sq[ch].max(1e-12).sqrt();
        let level_db = 20.0 * rms.log10();

        // Ride toward the nearer band edge. Material already inside [Min, Max]
        // keeps unity gain.
        if level_db > FLOOR_DB {
            let min_db = self.level.min_db();
            let max_db = self.level.max_db().max(min_db + 0.1);
            let target_db = level_db.clamp(min_db, max_db);
            let want_db = (target_db - level_db).clamp(-MAX_LIFT_DB, MAX_LIFT_DB);
            let want = 10f32.powf(want_db / 20.0);
            let coeff = if want > self.gain[ch] {
                self.gain_attack
            } else {
                self.gain_release
            };
            self.gain[ch] += (want - self.gain[ch]) * coeff;
        }

        let y = x * self.gain[ch];

        // Ceiling: a peak limiter, not a clamp of the leveling gain. Instant
        // attack, ~80 ms release.
        let peak = y.abs();
        let coeff = if peak > self.peak_env[ch] {
            1.0
        } else {
            self.peak_release
        };
        self.peak_env[ch] += (peak - self.peak_env[ch]) * coeff;

        let ceiling = self.ceiling;
        let limiter = if self.peak_env[ch] > ceiling {
            ceiling / self.peak_env[ch]
        } else {
            1.0
        };

        (y * limiter).clamp(-ceiling, ceiling)
    }
}

impl<'a> PluginAudioProcessor<'a, LevelerShared, LevelerMain> for LevelerAudio {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &LevelerMain,
        shared: &'a LevelerShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        let rate = audio_config.sample_rate as f32;
        let coeff = |ms: f32| 1.0 - (-1.0 / (rate * ms / 1000.0)).exp();
        Ok(Self {
            level: Arc::clone(&shared.level),
            mean_sq: [0.0; 2],
            gain: [1.0; 2],
            peak_env: [0.0; 2],
            rate,
            detector_attack: coeff(DETECTOR_MS),
            detector_release: coeff(DETECTOR_MS),
            gain_attack: coeff(ATTACK_MS),
            gain_release: coeff(RELEASE_MS),
            peak_release: coeff(80.0),
            ceiling: 10f32.powf(CEILING_DB / 20.0),
            built_from: (ATTACK_MS, RELEASE_MS, CEILING_DB),
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        self.apply_events(&events.input);
        self.refresh();

        for mut port_pair in &mut audio {
            let Some(channel_pairs) = port_pair.channels()?.into_f32() else {
                continue;
            };
            for (channel, pair) in channel_pairs.into_iter().enumerate() {
                match pair {
                    ChannelPair::InputOnly(_) => {}
                    ChannelPair::OutputOnly(buf) => buf.fill(0.0),
                    ChannelPair::InputOutput(input, output) => {
                        for (i, &x) in input.iter().enumerate() {
                            output[i] = self.step(channel, x);
                        }
                    }
                    ChannelPair::InPlace(buf) => {
                        for sample in buf.iter_mut() {
                            *sample = self.step(channel, *sample);
                        }
                    }
                }
            }
        }
        Ok(ProcessStatus::Continue)
    }
}

impl<'a> PluginAudioProcessorParams for LevelerAudio {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        self.apply_events(input);
    }
}

clack_export_entry!(SinglePluginEntry<Leveler>);
