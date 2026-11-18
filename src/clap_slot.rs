//! CLAP hosting.
//!
//! P1: enumerate a `.clap` library, instantiate one plug-in, activate it and
//! process audio. The lifecycle here is single-threaded on purpose — it is the
//! same sequence the mixer will run across its two threads (instance on the UI
//! thread, audio processor on the render thread).

use clack_host::prelude::*;
use std::path::{Path, PathBuf};

use crate::clap_host::{ClapHost, ClapShared};

/// One plug-in inside a `.clap` library. A single library can contain several.
#[derive(Clone, Debug)]
pub struct ClapEntry {
    pub path: PathBuf,
    /// Raw plug-in id as stored in the library (`CStr` bytes, no NUL).
    pub id: Vec<u8>,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub features: Vec<String>,
}

impl ClapEntry {
    /// `id` as a displayable string, for logs and as part of a state header.
    pub fn id_text(&self) -> String {
        String::from_utf8_lossy(&self.id).to_string()
    }

    pub fn category(&self) -> String {
        // CLAP features are the closest thing to a VST3 category: keep the
        // ones that describe what the plug-in is.
        let interesting = [
            "audio-effect",
            "instrument",
            "note-effect",
            "analyzer",
            "utility",
        ];
        let mut tags: Vec<&str> = self
            .features
            .iter()
            .filter_map(|f| {
                interesting
                    .iter()
                    .find(|i| f.eq_ignore_ascii_case(i))
                    .copied()
            })
            .collect();
        tags.dedup();
        tags.join("|")
    }
}

/// Standard CLAP search locations on Windows, plus `CLAP_PATH`.
pub fn default_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for var in ["COMMONPROGRAMFILES", "COMMONPROGRAMFILES(X86)"] {
        if let Ok(base) = std::env::var(var) {
            dirs.push(PathBuf::from(base).join("CLAP"));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Common")
                .join("CLAP"),
        );
    }
    if let Ok(roaming) = std::env::var("APPDATA") {
        dirs.push(PathBuf::from(roaming).join("CLAP"));
    }
    if let Ok(extra) = std::env::var("CLAP_PATH") {
        for part in extra.split(';') {
            if !part.trim().is_empty() {
                dirs.push(PathBuf::from(part));
            }
        }
    }
    dirs
}

fn looks_like_clap(path: &Path) -> bool {
    path.extension()
        .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("clap"))
        .unwrap_or(false)
}

/// Enumerate one library file.
fn scan_file(path: &Path, out: &mut Vec<ClapEntry>) {
    // Loading a library runs third-party code; the mixer only does this in its
    // scanner child process so a bad plug-in cannot take the console down.
    let entry = match unsafe { PluginEntry::load(path) } {
        Ok(entry) => entry,
        Err(err) => {
            eprintln!("clap: {}: {err}", path.display());
            return;
        }
    };
    let Some(factory) = entry.get_plugin_factory() else {
        return;
    };
    for descriptor in factory.plugin_descriptors() {
        let (Some(id), Some(name)) = (descriptor.id(), descriptor.name()) else {
            continue;
        };
        out.push(ClapEntry {
            path: path.to_path_buf(),
            id: id.to_bytes().to_vec(),
            name: name.to_string_lossy().to_string(),
            vendor: descriptor
                .vendor()
                .map(|v| v.to_string_lossy().to_string())
                .unwrap_or_default(),
            version: descriptor
                .version()
                .map(|v| v.to_string_lossy().to_string())
                .unwrap_or_default(),
            features: descriptor
                .features()
                .map(|f| f.to_string_lossy().to_string())
                .collect(),
        });
    }
}

/// Walk a directory (depth 1) for `.clap` libraries.
pub fn scan_dir(dir: &Path, out: &mut Vec<ClapEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if looks_like_clap(&path) && path.is_file() {
            scan_file(&path, out);
        }
    }
}

/// Scan every standard location.
pub fn scan() -> Vec<ClapEntry> {
    let mut out = Vec::new();
    for dir in default_dirs() {
        scan_dir(&dir, &mut out);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub struct SelftestReport {
    pub ok: bool,
    pub lines: Vec<String>,
}

/// Load one plug-in, run a 440 Hz sine through it and report what came out.
/// This mirrors `--vst-selftest` so the two backends can be compared directly.
pub fn selftest(path: &Path) -> SelftestReport {
    let mut lines = Vec::new();
    let mut ok = false;

    let entry = match unsafe { PluginEntry::load(path) } {
        Ok(entry) => entry,
        Err(err) => {
            lines.push(format!("FAIL: could not load {}: {err}", path.display()));
            return SelftestReport { ok, lines };
        }
    };
    let Some(factory) = entry.get_plugin_factory() else {
        lines.push("FAIL: library exposes no plug-in factory".to_string());
        return SelftestReport { ok, lines };
    };
    let descriptors: Vec<_> = factory.plugin_descriptors().collect();
    if descriptors.is_empty() {
        lines.push("FAIL: factory lists no plug-ins".to_string());
        return SelftestReport { ok, lines };
    }
    let desc = &descriptors[0];
    let Some(id) = desc.id() else {
        lines.push("FAIL: descriptor has no id".to_string());
        return SelftestReport { ok, lines };
    };
    let name = desc
        .name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    lines.push(format!(
        "library: {}  plug-in: {} ({})",
        path.display(),
        name,
        id.to_string_lossy()
    ));
    let id = id.to_owned();

    let info = match HostInfo::new("Audio Mixer PRO", "JayGee", "https://example.com", "0.1") {
        Ok(info) => info,
        Err(err) => {
            lines.push(format!("FAIL: host info: {err}"));
            return SelftestReport { ok, lines };
        }
    };

    let mut instance = match PluginInstance::<ClapHost>::new(
        |_| ClapShared::default(),
        |_| (),
        &entry,
        &id,
        &info,
    ) {
        Ok(instance) => instance,
        Err(err) => {
            lines.push(format!("FAIL: could not instantiate: {err}"));
            return SelftestReport { ok, lines };
        }
    };
    lines.push("instantiated".to_string());

    let config = PluginAudioConfiguration {
        sample_rate: 48_000.0,
        min_frames_count: 1,
        max_frames_count: crate::vst::VST_MAX_BLOCK as u32,
    };
    let stopped = match instance.activate(|_, _| (), config) {
        Ok(stopped) => stopped,
        Err(err) => {
            lines.push(format!("FAIL: could not activate: {err}"));
            return SelftestReport { ok, lines };
        }
    };
    lines.push("activated".to_string());

    let mut started = match stopped.start_processing() {
        Ok(started) => started,
        Err(err) => {
            lines.push(format!("FAIL: could not start processing: {err}"));
            return SelftestReport { ok, lines };
        }
    };
    lines.push("started".to_string());

    let block = 512usize;
    let mut input = [[0.0f32; 512]; 2];
    let mut output = [[0.0f32; 512]; 2];
    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let events = EventBuffer::new();
    let mut out_events = EventBuffer::new();

    let mut peak = 0.0f32;
    let mut nonfinite = 0usize;
    let mut processed = 0usize;
    let blocks = 96usize;
    for b in 0..blocks {
        for i in 0..block {
            let t = (b * block + i) as f32 / 48_000.0;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
            input[0][i] = v;
            input[1][i] = v;
        }
        let input_events = InputEvents::from_buffer(&events);
        let mut output_events = OutputEvents::from_buffer(&mut out_events);

        let mut input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input.iter_mut().map(|ch| InputChannel::constant(ch)),
            ),
        }]);
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                output.iter_mut().map(|ch| ch.as_mut_slice()),
            ),
        }]);

        match started.process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        ) {
            Ok(_status) => {
                processed += 1;
                for ch in &output {
                    for v in ch {
                        if !v.is_finite() {
                            nonfinite += 1;
                        }
                        peak = peak.max(v.abs());
                    }
                }
            }
            Err(err) => {
                lines.push(format!("FAIL: process: {err}"));
                break;
            }
        }
    }
    lines.push(format!(
        "process: blocks={processed}/{blocks} peak={peak:.6} nonfinite={nonfinite}"
    ));
    ok = processed == blocks && nonfinite == 0;

    let stopped = started.stop_processing();
    instance.deactivate(stopped);
    lines.push("stopped and deactivated".to_string());

    SelftestReport { ok, lines }
}
