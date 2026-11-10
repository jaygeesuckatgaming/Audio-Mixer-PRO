//! Everything the audio callback touches is behind one `Mutex`. The lock is
//! held for a whole block, which is short, and a parameterless atomic gate
//! keeps the callback away from the mutex when no plug-in is loaded. A failed
//! `process` marks the slot as failing and passes the dry signal through until
//! the person presses Retry or changes a parameter, so a broken plug-in cannot
//! take the mixer down or silence the microphone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use vst3_host::audio::BusAudioBuffers;
use vst3_host::window::PluginWindow;
use vst3_host::Plugin;

/// Maximum frames a callback may hand to the plug-in. The main output buffer
/// maximum in the original is 2048 (FUN_00527ad0); the plug-in is prepared for
/// this size and smaller device callbacks are passed as smaller blocks.
pub const VST_MAX_BLOCK: usize = 2048;

/// Standard VST3 locations on Windows, matching the SDK's own conventions.
pub const VST_STANDARD_DIRS: [&str; 2] = [
    r"C:\Program Files\Common Files\VST3",
    r"C:\Program Files (x86)\Common Files\VST3",
];

#[derive(Clone, Debug, Default)]
pub struct VstInfo {
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub category: String,
    pub uid: String,
    pub path: PathBuf,
    pub has_gui: bool,
    pub audio_inputs: u32,
    pub audio_outputs: u32,
}

#[derive(Clone, Debug, Default)]
pub struct VstParam {
    pub id: u32,
    pub name: String,
    pub unit: String,
    pub value: f64,
    pub step_count: i32,
    pub formatted: String,
}

pub struct ScanOutcome {
    pub plugins: Vec<VstInfo>,
    pub errors: Vec<String>,
}

struct Loaded {
    plugin: Arc<Mutex<Plugin>>,
    info: VstInfo,
    buffers: BusAudioBuffers,
    max_frames: usize,
    params: Vec<VstParam>,
}

struct Inner {
    loaded: Option<Loaded>,
    /// The plug-in's own editor window, when the person asked for it. It is
    /// deliberately not embedded in the mixer: the person wants the plug-in's
    /// own screen in a window of its own.
    editor: Option<EditorSlot>,
    error: Option<String>,
    failed: bool,
}

/// The native editor window holds a raw window handle, which the compiler
/// cannot prove is `Send`. Every editor operation (`open_editor`,
/// `service_editor`, `close_editor`) is called from the UI thread only; the
/// audio thread never touches this field. The assertion keeps the rest of the
/// chain shareable with the audio callback.
struct EditorSlot(PluginWindow);
unsafe impl Send for EditorSlot {}

pub struct VstChain {
    inner: Mutex<Inner>,
    has_plugin: AtomicBool,
    bypass: AtomicBool,
    /// Output peak of the last processed block, linear.
    peak: AtomicU32,
}

impl VstChain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                loaded: None,
                editor: None,
                error: None,
                failed: false,
            }),
            has_plugin: AtomicBool::new(false),
            bypass: AtomicBool::new(false),
            peak: AtomicU32::new(0),
        })
    }

    pub fn has_plugin(&self) -> bool {
        self.has_plugin.load(Ordering::Relaxed)
    }

    pub fn bypassed(&self) -> bool {
        self.bypass.load(Ordering::Relaxed)
    }

    pub fn set_bypass(&self, bypass: bool) {
        self.bypass.store(bypass, Ordering::Relaxed);
    }

    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    pub fn error(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|inner| inner.error.clone())
    }

    pub fn info(&self) -> Option<VstInfo> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.loaded.as_ref().map(|l| l.info.clone()))
    }

    /// Processing latency the plug-in reports (samples) and its tail length.
    pub fn latency(&self) -> (u32, u32) {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| {
                let loaded = inner.loaded.as_ref()?;
                let plugin = loaded.plugin.lock().ok()?;
                Some((plugin.latency_samples(), plugin.tail_samples()))
            })
            .unwrap_or((0, 0))
    }

    pub fn parameters(&self) -> Vec<VstParam> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.loaded.as_ref().map(|l| l.params.clone()))
            .unwrap_or_default()
    }

    /// Load a `.vst3` bundle or a single VST3 binary. The old plug-in (if any)
    /// is dropped only after the new one has loaded successfully. Any editor
    /// window belonging to the old plug-in is closed first so it can never
    /// outlive the plug-in it draws.
    pub fn load(
        &self,
        path: &Path,
        sample_rate: f64,
        block_size: usize,
    ) -> Result<VstInfo, String> {
        let block_size = block_size.clamp(64, VST_MAX_BLOCK);
        let mut host = vst3_host::Vst3Host::builder()
            .sample_rate(sample_rate)
            .block_size(block_size)
            .input_channels(2)
            .output_channels(2)
            .build()
            .map_err(|err| format!("Could not start the VST3 host: {err}"))?;
        let mut plugin = host
            .load_plugin(path)
            .map_err(|err| format!("Could not load {}: {err}", path.display()))?;
        plugin
            .start_processing()
            .map_err(|err| format!("Could not start {}: {err}", path.display()))?;
        let buffers = plugin
            .create_bus_audio_buffers(block_size)
            .map_err(|err| format!("Could not prepare {} audio buffers: {err}", path.display()))?;
        let info = info_from_plugin(&plugin, path);
        let params = read_parameters(&mut plugin);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        if let Some(mut editor) = inner.editor.take() {
            editor.0.close();
        }
        inner.error = None;
        inner.failed = false;
        inner.loaded = Some(Loaded {
            plugin: Arc::new(Mutex::new(plugin)),
            info: info.clone(),
            buffers,
            max_frames: block_size,
            params,
        });
        self.has_plugin.store(true, Ordering::Relaxed);
        self.peak.store(0.0f32.to_bits(), Ordering::Relaxed);
        Ok(info)
    }

    pub fn unload(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(mut editor) = inner.editor.take() {
                editor.0.close();
            }
            inner.loaded = None;
            inner.error = None;
            inner.failed = false;
        }
        self.has_plugin.store(false, Ordering::Relaxed);
        self.peak.store(0.0f32.to_bits(), Ordering::Relaxed);
    }

    /// Show the plug-in's own editor in its own top-level window. This is the
    /// "Open VST" action beside the slot button: the mixer never draws the
    /// plug-in's settings itself.
    pub fn open_editor(&self) -> Result<(), String> {
        let plugin = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "The VST slot is unavailable".to_string())?;
            let Some(loaded) = inner.loaded.as_ref() else {
                return Err("No VST plug-in is loaded".to_string());
            };
            if !loaded.info.has_gui {
                return Err(format!(
                    "{} has no screen of its own to show.",
                    loaded.info.name
                ));
            }
            if inner
                .editor
                .as_ref()
                .map_or(false, |window| window.0.is_open())
            {
                return Ok(());
            }
            loaded.plugin.clone()
        };
        // Build the native window without holding the slot lock: audio keeps
        // flowing while the plug-in creates its own screen.
        let mut window = PluginWindow::new(plugin);
        window
            .open()
            .map_err(|err| format!("Could not open the plug-in's own screen: {err}"))?;
        // The plug-in's editor is a native top-level window, so its title bar
        // follows the OS light/dark setting rather than the mixer's palette.
        darken_plugin_windows();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        if let Some(mut old) = inner.editor.take() {
            old.0.close();
        }
        inner.editor = Some(EditorSlot(window));
        inner.error = None;
        Ok(())
    }

    /// Keep the open editor alive: apply plug-in resize requests and notice
    /// the title-bar close button. Called once per UI frame.
    pub fn service_editor(&self) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if let Some(window) = inner.editor.as_ref() {
            let _ = window.0.service_platform_events();
        }
        let closed = inner.editor.as_ref().map_or(false, |window| {
            window.0.closed_by_user() || !window.0.is_open()
        });
        if closed {
            if let Some(mut window) = inner.editor.take() {
                window.0.close();
            }
        }
    }

    pub fn editor_is_open(&self) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.editor.as_ref().map(|window| window.0.is_open()))
            .unwrap_or(false)
    }

    pub fn close_editor(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(mut window) = inner.editor.take() {
                window.0.close();
            }
        }
    }

    pub fn set_parameter(&self, id: u32, value: f64) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        let Some(loaded) = inner.loaded.as_mut() else {
            return Err("No VST plug-in is loaded".to_string());
        };
        let value = value.clamp(0.0, 1.0);
        let mut plugin = loaded
            .plugin
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        plugin
            .set_parameter(id, value)
            .map_err(|err| format!("Could not set parameter {id}: {err}"))?;
        if let Some(param) = loaded.params.iter_mut().find(|param| param.id == id) {
            param.value = value;
            param.formatted = format_param(&plugin, id, value);
        }
        drop(plugin);
        inner.failed = false;
        inner.error = None;
        Ok(())
    }

    /// Re-read every parameter value. Clears a failed state so a transient
    /// plug-in error can be retried from the panel.
    pub fn refresh_parameters(&self) -> Vec<VstParam> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Vec::new(),
        };
        let params = {
            let Some(loaded) = inner.loaded.as_mut() else {
                return Vec::new();
            };
            let mut plugin = match loaded.plugin.lock() {
                Ok(plugin) => plugin,
                Err(_) => return Vec::new(),
            };
            loaded.params = read_parameters(&mut plugin);
            loaded.params.clone()
        };
        inner.failed = false;
        inner.error = None;
        params
    }

    /// Save the plug-in's own opaque state. The file begins with a small text
    /// header naming the plug-in so a state blob is never handed to a
    /// different plug-in.
    pub fn save_state_to(&self, path: &Path) -> Result<(), String> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        let Some(loaded) = inner.loaded.as_ref() else {
            let _ = std::fs::remove_file(path);
            return Ok(());
        };
        let mut plugin = loaded
            .plugin
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        let state = plugin
            .save_state()
            .map_err(|err| format!("Could not read plug-in state: {err}"))?;
        drop(plugin);
        let mut bytes = Vec::with_capacity(state.len() + 64);
        bytes.extend_from_slice(b"VMVST1\n");
        bytes.extend_from_slice(loaded.info.path.display().to_string().as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(&state);
        std::fs::write(path, bytes).map_err(|err| format!("Could not write plug-in state: {err}"))
    }

    /// Restore a state file written by `save_state_to`, but only when its
    /// header matches the plug-in currently in the slot.
    pub fn load_state_from(&self, path: &Path) -> Result<(), String> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(()),
        };
        let Some(rest) = bytes.strip_prefix(b"VMVST1\n") else {
            return Err("The saved VST state has an unknown format".to_string());
        };
        let Some(nl) = rest.iter().position(|b| *b == b'\n') else {
            return Err("The saved VST state is truncated".to_string());
        };
        let saved_path = String::from_utf8_lossy(&rest[..nl]).to_string();
        let state = &rest[nl + 1..];
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        let Some(loaded) = inner.loaded.as_mut() else {
            return Ok(());
        };
        if loaded.info.path.display().to_string() != saved_path {
            return Ok(());
        }
        let mut plugin = loaded
            .plugin
            .lock()
            .map_err(|_| "The VST slot is unavailable".to_string())?;
        plugin
            .load_state(state)
            .map_err(|err| format!("Could not restore plug-in state: {err}"))?;
        loaded.params = read_parameters(&mut plugin);
        Ok(())
    }

    /// Process one block. Returns true when the returned buffers hold plug-in
    /// output; false means the caller should keep its dry signal.
    pub fn process(
        &self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) -> bool {
        if !self.has_plugin.load(Ordering::Relaxed) || self.bypass.load(Ordering::Relaxed) {
            return false;
        }
        let frames = in_l.len().min(in_r.len()).min(out_l.len()).min(out_r.len());
        if frames == 0 {
            return false;
        }
        // Never block the real-time callback behind the editor or a plug-in
        // load. A busy slot passes dry audio for this block.
        let Ok(mut inner) = self.inner.try_lock() else {
            return false;
        };
        if inner.failed {
            return false;
        }
        // Keep the mutable borrow of the loaded slot inside this block so the
        // error can be recorded afterwards without borrowing `inner` twice.
        let outcome: Result<f32, String> = {
            let Some(loaded) = inner.loaded.as_mut() else {
                return false;
            };
            if frames > loaded.max_frames {
                Err(format!(
                    "The audio device sent {frames} frames, more than the plug-in was prepared for ({}).",
                    loaded.max_frames
                ))
            } else {
                // Every bus keeps its declared channel count. Only the length
                // follows the device block; Vec capacity was allocated for
                // VST_MAX_BLOCK, so this does not allocate in the callback.
                for bus in loaded
                    .buffers
                    .inputs
                    .iter_mut()
                    .chain(loaded.buffers.outputs.iter_mut())
                {
                    for channel in &mut bus.channels {
                        channel.clear();
                        channel.resize(frames, 0.0);
                    }
                }
                loaded.buffers.block_size = frames;

                if let Some(bus) = loaded.buffers.inputs.first_mut() {
                    match bus.channels.len() {
                        0 => {}
                        1 => {
                            for i in 0..frames {
                                bus.channels[0][i] = (in_l[i] + in_r[i]) * 0.5;
                            }
                        }
                        _ => {
                            for i in 0..frames {
                                bus.channels[0][i] = in_l[i];
                                bus.channels[1][i] = in_r[i];
                            }
                            // Extra surround channels stay silent rather than
                            // looping the stereo pair into them.
                            for ch in bus.channels.iter_mut().skip(2) {
                                ch.fill(0.0);
                            }
                        }
                    }
                }

                let mut plugin = match loaded.plugin.lock() {
                    Ok(plugin) => plugin,
                    Err(_) => return false,
                };
                if let Err(err) = plugin.process_bus_audio(&mut loaded.buffers) {
                    Err(format!("Plug-in processing failed: {err}"))
                } else if let Some(output) = loaded.buffers.outputs.first() {
                    if output.channels.is_empty() {
                        Err("The plug-in produced no audio channels.".to_string())
                    } else {
                        let mut peak = 0.0f32;
                        if output.channels.len() == 1 {
                            for i in 0..frames {
                                let v = output.channels[0][i];
                                out_l[i] = v;
                                out_r[i] = v;
                                peak = peak.max(v.abs());
                            }
                        } else {
                            for i in 0..frames {
                                let l = output.channels[0][i];
                                let r = output.channels[1][i];
                                out_l[i] = l;
                                out_r[i] = r;
                                peak = peak.max(l.abs()).max(r.abs());
                            }
                        }
                        Ok(peak)
                    }
                } else {
                    Err("The plug-in has no audio output bus.".to_string())
                }
            }
        };

        match outcome {
            Ok(peak) => {
                self.peak.store(peak.to_bits(), Ordering::Relaxed);
                true
            }
            Err(err) => {
                inner.error = Some(err);
                inner.failed = true;
                false
            }
        }
    }
}

fn info_from_plugin(plugin: &Plugin, path: &Path) -> VstInfo {
    let info = plugin.info();
    VstInfo {
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        category: info.category.clone(),
        uid: info.uid.clone(),
        path: path.to_path_buf(),
        has_gui: info.has_gui,
        audio_inputs: info.audio_inputs,
        audio_outputs: info.audio_outputs,
    }
}

fn format_param(plugin: &Plugin, id: u32, value: f64) -> String {
    match plugin.format_parameter(id, value) {
        Ok(text) if !text.is_empty() => text,
        _ => format!("{:.3}", value),
    }
}

fn read_parameters(plugin: &mut Plugin) -> Vec<VstParam> {
    let params = plugin.get_parameters().unwrap_or_default();
    params
        .into_iter()
        .map(|param| {
            let formatted = format_param(plugin, param.id, param.value);
            VstParam {
                id: param.id,
                name: param.name,
                unit: param.unit,
                value: param.value,
                step_count: param.step_count,
                formatted,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

pub fn scan_plugins(extra: &[PathBuf]) -> ScanOutcome {
    use std::collections::HashSet;

    let mut dirs: Vec<PathBuf> = VST_STANDARD_DIRS.iter().map(PathBuf::from).collect();
    for path in extra {
        if !dirs.iter().any(|d| d == path) {
            dirs.push(path.clone());
        }
    }
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut bundles = match vst3_host::discovery::scan_directories(&dirs) {
        Ok(bundles) => bundles,
        Err(err) => {
            errors.push(format!("Could not list the VST3 folders: {err}"));
            Vec::new()
        }
    };
    // Prefer the 64-bit bundle when both Program Files trees carry the same
    // plug-in; the duplicate would otherwise appear twice in the selector.
    bundles.sort_by_key(|path| path.to_string_lossy().contains("(x86)"));
    for bundle in bundles {
        let key = bundle.canonicalize().unwrap_or_else(|_| bundle.clone());
        if !seen.insert(key) {
            continue;
        }
        match vst3_host::discovery::get_plugin_info(&bundle) {
            Ok(info) => plugins.push(VstInfo {
                name: info.name,
                vendor: info.vendor,
                version: info.version,
                category: info.category,
                uid: info.uid,
                path: bundle,
                has_gui: info.has_gui,
                audio_inputs: info.audio_inputs,
                audio_outputs: info.audio_outputs,
            }),
            Err(err) => errors.push(format!(
                "{}: {err}",
                bundle.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    plugins.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.vendor.cmp(&b.vendor))
    });
    let mut names = HashSet::new();
    plugins.retain(|plugin| {
        names.insert((
            plugin.name.to_ascii_lowercase(),
            plugin.vendor.to_ascii_lowercase(),
        ))
    });
    ScanOutcome { plugins, errors }
}

/// Scan in a **separate process**. VST3 plug-ins commonly guard themselves
/// with a single instance per module; instantiating a second copy while the
/// live insert is processing can deadlock the whole console. The helper also
/// contains a crashing plug-in. The child writes its report to a temp JSON
/// file and dies on its own; the parent kills it after a timeout.
pub fn start_scan(extra: Vec<PathBuf>) -> Receiver<ScanOutcome> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                let _ = tx.send(ScanOutcome {
                    plugins: Vec::new(),
                    errors: vec![format!("Could not locate the scanner process: {err}")],
                });
                return;
            }
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let report =
            std::env::temp_dir().join(format!("vm_vst_scan_{}_{}.json", std::process::id(), stamp));
        let mut command = std::process::Command::new(exe);
        command.arg("--vst-scan-json").arg(&report);
        for path in &extra {
            command.arg("--vst-extra").arg(path);
        }
        let child = command.spawn();
        let outcome = match child {
            Ok(mut child) => {
                let started = std::time::Instant::now();
                let mut timed_out = false;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => {
                            if started.elapsed() > std::time::Duration::from_secs(25) {
                                timed_out = true;
                                let _ = child.kill();
                                let _ = child.wait();
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(120));
                        }
                        Err(_) => break,
                    }
                }
                let mut outcome = read_scan_report(&report).unwrap_or_else(|err| ScanOutcome {
                    plugins: Vec::new(),
                    errors: vec![format!("The VST3 scanner produced no report: {err}")],
                });
                if timed_out {
                    outcome
                        .errors
                        .push("The VST3 scan timed out; a hung plug-in was skipped.".to_string());
                }
                outcome
            }
            Err(err) => ScanOutcome {
                plugins: Vec::new(),
                errors: vec![format!("Could not start the VST3 scanner: {err}")],
            },
        };
        let _ = std::fs::remove_file(&report);
        let _ = tx.send(outcome);
    });
    rx
}

fn read_scan_report(path: &Path) -> Result<ScanOutcome, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("bad scan report: {err}"))?;
    let mut outcome = ScanOutcome {
        plugins: Vec::new(),
        errors: Vec::new(),
    };
    if let Some(items) = value.get("plugins").and_then(|v| v.as_array()) {
        for item in items {
            outcome.plugins.push(VstInfo {
                name: item["name"].as_str().unwrap_or_default().to_string(),
                vendor: item["vendor"].as_str().unwrap_or_default().to_string(),
                version: item["version"].as_str().unwrap_or_default().to_string(),
                category: item["category"].as_str().unwrap_or_default().to_string(),
                uid: item["uid"].as_str().unwrap_or_default().to_string(),
                path: PathBuf::from(item["path"].as_str().unwrap_or_default()),
                has_gui: item["has_gui"].as_bool().unwrap_or(false),
                audio_inputs: item["audio_inputs"].as_u64().unwrap_or(0) as u32,
                audio_outputs: item["audio_outputs"].as_u64().unwrap_or(0) as u32,
            });
        }
    }
    if let Some(items) = value.get("errors").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(text) = item.as_str() {
                outcome.errors.push(text.to_string());
            }
        }
    }
    Ok(outcome)
}

/// Child-process entry point: scan and write the JSON report.
pub fn write_scan_report(path: &Path) -> Result<(), String> {
    // The parent kills this helper after 25 s, but if the parent itself exits
    // first the helper would be orphaned. Kill ourselves after 20 s so a hung
    // plug-in can never leave a stray process holding the executable open.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(20));
        std::process::exit(2);
    });
    let mut extra = Vec::new();
    let args: Vec<String> = std::env::args().collect();
    let mut index = 0;
    while index + 1 < args.len() {
        if args[index] == "--vst-extra" {
            extra.push(PathBuf::from(&args[index + 1]));
            index += 2;
        } else {
            index += 1;
        }
    }
    let outcome = scan_plugins(&extra);
    let plugins: Vec<serde_json::Value> = outcome
        .plugins
        .iter()
        .map(|plugin| {
            serde_json::json!({
                "name": plugin.name,
                "vendor": plugin.vendor,
                "version": plugin.version,
                "category": plugin.category,
                "uid": plugin.uid,
                "path": plugin.path.display().to_string(),
                "has_gui": plugin.has_gui,
                "audio_inputs": plugin.audio_inputs,
                "audio_outputs": plugin.audio_outputs,
            })
        })
        .collect();
    let value = serde_json::json!({ "plugins": plugins, "errors": outcome.errors });
    std::fs::write(path, value.to_string())
        .map_err(|err| format!("could not write the scan report: {err}"))
}

/// Native "Select VST3 Plug-in" dialog, styled like the tape deck's picker.
pub fn pick_plugin_file() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OPENFILENAMEW};
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

        let wide = |s: &str| -> Vec<u16> {
            let mut v: Vec<u16> = s.encode_utf16().collect();
            v.push(0);
            v
        };
        let filter = "VST3 Plug-in (*.vst3)\0*.vst3\0All files\0*.*\0\0"
            .encode_utf16()
            .collect::<Vec<u16>>();
        let title = wide("Select VST3 Plug-in");
        let mut file_buf = vec![0u16; 1000];
        let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = unsafe { GetActiveWindow() };
        ofn.lpstrFilter = filter.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = file_buf.as_mut_ptr();
        ofn.nMaxFile = 1000;
        ofn.lpstrTitle = title.as_ptr();
        ofn.Flags = 0x0082_1000;
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
    {
        None
    }
}

// ---------------------------------------------------------------------------
// Headless probe
// ---------------------------------------------------------------------------

/// Headless stability check: load a real plug-in, process many blocks, move a
/// parameter, bypass and restore state. The report is written to `report_path`
/// so it can be read even though the app is a windowed program.
pub fn selftest(preferred: Option<&Path>, report_path: &Path) {
    let mut report = String::new();
    let outcome = scan_plugins(&[]);
    let candidate = preferred
        .map(Path::to_path_buf)
        .or_else(|| {
            outcome
                .plugins
                .iter()
                .find(|plugin| plugin.name.to_ascii_lowercase().contains("7q"))
                .map(|plugin| plugin.path.clone())
        })
        .or_else(|| outcome.plugins.first().map(|plugin| plugin.path.clone()));
    let Some(candidate) = candidate else {
        report.push_str("FAIL: no VST3 plug-in to test\n");
        let _ = std::fs::write(report_path, report);
        return;
    };
    report.push_str(&format!("plug-in: {}\n", candidate.display()));
    let chain = VstChain::new();
    match chain.load(&candidate, 48_000.0, VST_MAX_BLOCK) {
        Ok(info) => {
            report.push_str(&format!("loaded: {} by {}\n", info.name, info.vendor));
            let params = chain.parameters();
            report.push_str(&format!("parameters: {}\n", params.len()));
            let frames = 512;
            let mut in_l = vec![0.0f32; frames];
            let mut in_r = vec![0.0f32; frames];
            let mut out_l = vec![0.0f32; frames];
            let mut out_r = vec![0.0f32; frames];
            let mut peak = 0.0f32;
            let mut nonfinite = 0usize;
            let mut processed = 0usize;
            let blocks = 96;
            for block in 0..blocks {
                for i in 0..frames {
                    let t = (block * frames + i) as f32 / 48_000.0;
                    let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
                    in_l[i] = v;
                    in_r[i] = v;
                }
                if chain.process(&in_l, &in_r, &mut out_l, &mut out_r) {
                    processed += 1;
                    for i in 0..frames {
                        if !out_l[i].is_finite() || !out_r[i].is_finite() {
                            nonfinite += 1;
                        }
                        peak = peak.max(out_l[i].abs()).max(out_r[i].abs());
                    }
                }
            }
            report.push_str(&format!(
                "process: blocks={processed}/{blocks} peak={peak:.6} nonfinite={nonfinite}\n"
            ));

            // A parameter move must reach the plug-in without an error.
            let param = params
                .iter()
                .find(|param| param.step_count <= 0 && param.can_automate_for_test())
                .or_else(|| params.first());
            let mut param_ok = true;
            if let Some(param) = param {
                if let Err(err) = chain.set_parameter(param.id, 0.25) {
                    param_ok = false;
                    report.push_str(&format!("parameter error: {err}\n"));
                }
            }
            let after = chain.refresh_parameters();
            report.push_str(&format!(
                "parameter move: ok={param_ok} params_after_refresh={}\n",
                after.len()
            ));

            // Bypass must hand the dry signal back to the caller.
            chain.set_bypass(true);
            let bypass_returns_wet = chain.process(&in_l, &in_r, &mut out_l, &mut out_r);
            chain.set_bypass(false);
            report.push_str(&format!(
                "bypass: process_returned_wet={bypass_returns_wet} (must be false)\n"
            ));

            // State round-trip through the on-disk format.
            let state_path = report_path.with_file_name("vst_selftest_state.bin");
            let state_saved = chain.save_state_to(&state_path).is_ok();
            let state_loaded = chain.load_state_from(&state_path).is_ok();
            let _ = std::fs::remove_file(&state_path);
            report.push_str(&format!(
                "state: saved={state_saved} loaded={state_loaded}\n"
            ));

            let pass = processed > 0
                && nonfinite == 0
                && param_ok
                && !bypass_returns_wet
                && state_saved
                && state_loaded;
            report.push_str(&format!("pass={pass}\n"));
        }
        Err(err) => report.push_str(&format!("FAIL: {err}\n")),
    }
    let _ = std::fs::write(report_path, &report);
    eprintln!("{report}");
}

/// Small helper so the selftest's parameter choice is readable at the call
/// site; `Parameter` does not carry `can_automate` in `VstParam`, so a
/// continuous parameter (step_count 0) is the useful target.
impl VstParam {
    fn can_automate_for_test(&self) -> bool {
        self.step_count <= 0
    }
}

/// Check the "Open VST" path: load a real plug-in, open its own editor window,
/// keep it serviced for a moment, then close it. The report is written to
/// `report_path`. This is the same PluginWindow the Open VST buttons use.
pub fn editor_selftest(preferred: Option<&Path>, report_path: &Path) {
    let mut report = String::new();
    let outcome = scan_plugins(&[]);
    let candidate = preferred
        .map(Path::to_path_buf)
        .or_else(|| {
            outcome
                .plugins
                .iter()
                .find(|plugin| plugin.name.to_ascii_lowercase().contains("7q"))
                .map(|plugin| plugin.path.clone())
        })
        .or_else(|| outcome.plugins.first().map(|plugin| plugin.path.clone()));
    let Some(candidate) = candidate else {
        report.push_str("FAIL: no VST3 plug-in to test\n");
        let _ = std::fs::write(report_path, report);
        return;
    };
    report.push_str(&format!("plug-in: {}\n", candidate.display()));
    let chain = VstChain::new();
    match chain.load(&candidate, 48_000.0, VST_MAX_BLOCK) {
        Ok(info) => {
            report.push_str(&format!(
                "loaded: {} by {} (own window: {})\n",
                info.name, info.vendor, info.has_gui
            ));
            let opened = chain.open_editor();
            report.push_str(&format!(
                "open: ok={} editor_open={}\n",
                opened.is_ok(),
                chain.editor_is_open()
            ));
            if let Err(err) = &opened {
                report.push_str(&format!("open error: {err}\n"));
            }
            // Service the native window for roughly a second, like the UI
            // frame loop does, then close it the same way the app does.
            for _ in 0..40 {
                chain.service_editor();
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let still_open = chain.editor_is_open();
            chain.close_editor();
            let closed = !chain.editor_is_open();
            report.push_str(&format!(
                "service: still_open_after_1s={still_open} closed_after_request={closed}\n"
            ));
            let pass = opened.is_ok() && still_open && closed;
            report.push_str(&format!("pass={pass}\n"));
        }
        Err(err) => report.push_str(&format!("FAIL: {err}\n")),
    }
    let _ = std::fs::write(report_path, &report);
    eprintln!("{report}");
}

pub fn probe(path: Option<&Path>, report_path: &Path) {
    let mut report = String::new();
    let outcome = scan_plugins(&[]);
    report.push_str(&format!(
        "scan: {} plug-in(s), {} error(s)\n",
        outcome.plugins.len(),
        outcome.errors.len()
    ));
    for plugin in &outcome.plugins {
        report.push_str(&format!(
            "  {} | {} | {} | {} | in {} out {} | gui {} | {}\n",
            plugin.name,
            plugin.vendor,
            plugin.category,
            plugin.version,
            plugin.audio_inputs,
            plugin.audio_outputs,
            plugin.has_gui,
            plugin.path.display()
        ));
    }
    for error in &outcome.errors {
        report.push_str(&format!("  error: {error}\n"));
    }

    let candidate = path
        .map(Path::to_path_buf)
        .or_else(|| outcome.plugins.first().map(|plugin| plugin.path.clone()));
    if let Some(candidate) = candidate {
        report.push_str(&format!("\nload: {}\n", candidate.display()));
        let chain = VstChain::new();
        match chain.load(&candidate, 48_000.0, VST_MAX_BLOCK) {
            Ok(info) => {
                report.push_str(&format!(
                    "loaded: {} by {} ({}), gui={}, in={}, out={}\n",
                    info.name,
                    info.vendor,
                    info.category,
                    info.has_gui,
                    info.audio_inputs,
                    info.audio_outputs
                ));
                let (latency, tail) = chain.latency();
                report.push_str(&format!(
                    "latency: {latency} samples, tail: {tail} samples\n"
                ));
                let params = chain.parameters();
                report.push_str(&format!("parameters: {}\n", params.len()));
                for param in params.iter().take(6) {
                    report.push_str(&format!(
                        "  {} = {} ({:.3})\n",
                        param.name, param.formatted, param.value
                    ));
                }
                let frames = 512;
                let mut l = vec![0.0f32; frames];
                let mut r = vec![0.0f32; frames];
                let mut ol = vec![0.0f32; frames];
                let mut or = vec![0.0f32; frames];
                for i in 0..frames {
                    let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin() * 0.25;
                    l[i] = v;
                    r[i] = v;
                }
                let processed = chain.process(&l, &r, &mut ol, &mut or);
                let peak = ol
                    .iter()
                    .chain(or.iter())
                    .fold(0.0f32, |a, v| a.max(v.abs()));
                report.push_str(&format!(
                    "process: returned={processed} out_peak={peak:.6} reported_peak={:.6}\n",
                    chain.peak()
                ));
            }
            Err(err) => report.push_str(&format!("load failed: {err}\n")),
        }
    } else {
        report.push_str("load: no VST3 plug-in found in the standard folders\n");
    }
    if let Err(err) = std::fs::write(report_path, &report) {
        eprintln!("could not write VST probe report: {err}");
    }
    eprintln!("{report}");
}

/// Paint the plug-in's native editor title bar with the console palette.
///
/// The host creates the editor's top-level window with the title
/// "<plug-in> - VST3"; Windows draws that non-client area from the system
/// theme, which is why a light title bar appears above a dark plug-in.
#[cfg(windows)]
fn darken_plugin_windows() {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible,
    };

    unsafe extern "system" fn visit(hwnd: HWND, _lparam: LPARAM) -> i32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != std::process::id() || unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return 1;
        }
        let mut title = vec![0u16; len as usize + 1];
        let n = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        if n <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&title[..n as usize]);
        if !title.ends_with(" - VST3") {
            return 1;
        }
        let dark: i32 = 1;
        let caption = colorref(26, 30, 37);
        let text = colorref(216, 222, 231);
        let border = colorref(47, 54, 65);
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &dark as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
            // Windows 11 honours the colours; older builds simply ignore them.
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_CAPTION_COLOR as u32,
                &caption as *const u32 as *const core::ffi::c_void,
                4,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_TEXT_COLOR as u32,
                &text as *const u32 as *const core::ffi::c_void,
                4,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR as u32,
                &border as *const u32 as *const core::ffi::c_void,
                4,
            );
        }
        1
    }

    unsafe { EnumWindows(Some(visit), 0) };
}

#[cfg(windows)]
fn colorref(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | ((g as u32) << 8) | ((b as u32) << 16)
}

#[cfg(not(windows))]
fn darken_plugin_windows() {}
