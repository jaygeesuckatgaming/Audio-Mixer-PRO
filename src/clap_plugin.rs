//! Runtime CLAP slot: the long-lived object the mixer keeps per strip, as
//! opposed to the one-shot lifecycle in `clap_slot`'s selftests.
//!
//! CLAP splits ownership by thread: the `PluginInstance` is main-thread-only
//! (and `!Send`), while the activated audio processor is `Send` and belongs to
//! the render thread. The two halves are kept apart here so the audio path never
//! touches the instance.

use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGui, Window};
use clack_extensions::latency::PluginLatency;
use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_extensions::state::PluginState;
use clack_host::events::event_types::ParamValueEvent;
use clack_host::events::io::EventBuffer;
use clack_host::prelude::*;
use clack_host::prelude::{ClapId, Pckn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::clap_host::{ClapHost, ClapShared};
use crate::vst::{VstInfo, VstParam, VST_MAX_BLOCK};

const FORMAT: &str = "CLAP";

pub struct ClapSlot {
    info: Mutex<Option<VstInfo>>,
    error: Mutex<Option<String>>,
    main: Mutex<Option<Main>>,
    audio: Arc<Mutex<Audio>>,
    bypass: AtomicBool,
    peak: AtomicU32,
    in_peak: AtomicU32,
    gui_open: AtomicBool,
    /// Parameter changes waiting to be delivered as events in the next block.
    pending: Mutex<Vec<(ClapId, f64)>>,
}

struct Main {
    instance: PluginInstance<ClapHost>,
    window: Option<crate::host_window::HostWindow>,
    gui_created: bool,
}

struct Audio {
    processor: Option<StartedPluginAudioProcessor<ClapHost>>,
    /// Still-`Stopped` processor, started lazily on the audio thread.
    stopped: Option<StoppedPluginAudioProcessor<ClapHost>>,
    input: [[f32; VST_MAX_BLOCK]; 2],
    output: [[f32; VST_MAX_BLOCK]; 2],
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    input_events: EventBuffer,
    output_events: EventBuffer,
}

// The instance is main-thread-only by contract; `ClapSlot` is shared through an
// Arc, and every method that touches `main` is called from the UI thread.
unsafe impl Send for Main {}

impl ClapSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            info: Mutex::new(None),
            error: Mutex::new(None),
            main: Mutex::new(None),
            audio: Arc::new(Mutex::new(Audio {
                processor: None,
                stopped: None,
                input: [[0.0; VST_MAX_BLOCK]; 2],
                output: [[0.0; VST_MAX_BLOCK]; 2],
                input_ports: AudioPorts::with_capacity(2, 1),
                output_ports: AudioPorts::with_capacity(2, 1),
                input_events: EventBuffer::with_capacity(8),
                output_events: EventBuffer::with_capacity(8),
            })),
            bypass: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            in_peak: AtomicU32::new(0),
            gui_open: AtomicBool::new(false),
            pending: Mutex::new(Vec::with_capacity(8)),
        })
    }

    pub fn has_plugin(&self) -> bool {
        self.main.lock().map(|m| m.is_some()).unwrap_or(false)
    }

    pub fn info(&self) -> Option<VstInfo> {
        self.info.lock().ok().and_then(|i| i.clone())
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    fn set_error(&self, message: Option<String>) {
        if let Ok(mut slot) = self.error.lock() {
            *slot = message;
        }
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

    pub fn input_peak(&self) -> f32 {
        f32::from_bits(self.in_peak.load(Ordering::Relaxed))
    }

    pub fn latency(&self) -> (u32, u32) {
        (0, 0)
    }

    pub fn diag(&self) -> String {
        format!(
            "clap loaded={} bypass={} peak={:.5} gui={} error={:?}",
            self.has_plugin(),
            self.bypassed(),
            self.peak(),
            self.gui_open.load(Ordering::Relaxed),
            self.error()
        )
    }

    /// Load `<file>.clap#<plug-in id>` (the id is appended by the scanner).
    pub fn load(
        &self,
        spec: &Path,
        sample_rate: f64,
        _max_block: usize,
    ) -> Result<VstInfo, String> {
        self.unload();
        let (file, wanted_id) = match spec.to_string_lossy().split_once('#') {
            Some((file, id)) => (PathBuf::from(file), Some(id.to_string())),
            None => (spec.to_path_buf(), None),
        };

        let entry = unsafe { PluginEntry::load(&file) }.map_err(|e| format!("load: {e}"))?;
        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| "library exposes no plug-in factory".to_string())?;
        let descriptors: Vec<_> = factory.plugin_descriptors().collect();
        let chosen = descriptors
            .iter()
            .find(|d| {
                wanted_id.as_ref().map_or(true, |want| {
                    d.id()
                        .map(|id| id.to_string_lossy() == *want)
                        .unwrap_or(false)
                })
            })
            .or_else(|| descriptors.first())
            .ok_or_else(|| "library lists no plug-ins".to_string())?;
        let id = chosen
            .id()
            .ok_or_else(|| "plug-in has no id".to_string())?
            .to_owned();

        let info = VstInfo {
            name: chosen
                .name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "CLAP plug-in".to_string()),
            vendor: chosen
                .vendor()
                .map(|v| v.to_string_lossy().to_string())
                .unwrap_or_default(),
            version: chosen
                .version()
                .map(|v| v.to_string_lossy().to_string())
                .unwrap_or_default(),
            category: String::new(),
            uid: id.to_string_lossy().to_string(),
            path: file.clone(),
            has_gui: true,
            audio_inputs: 2,
            audio_outputs: 2,
            format: FORMAT.to_string(),
        };

        let host_info = HostInfo::new("Audio Mixer PRO", "JayGee", "https://example.com", "0.1")
            .map_err(|e| format!("host info: {e}"))?;
        let mut instance = PluginInstance::<ClapHost>::new(
            |_| ClapShared::default(),
            |_| (),
            &entry,
            &id,
            &host_info,
        )
        .map_err(|e| format!("instantiate: {e}"))?;

        let config = PluginAudioConfiguration {
            sample_rate,
            min_frames_count: 1,
            max_frames_count: VST_MAX_BLOCK as u32,
        };
        let stopped = instance
            .activate(|_, _| (), config)
            .map_err(|e| format!("activate: {e}"))?;

        {
            let mut audio = self.audio.lock().map_err(|_| "audio lock".to_string())?;
            audio.stopped = Some(stopped);
            audio.processor = None;
        }
        if let Ok(mut slot) = self.main.lock() {
            *slot = Some(Main {
                instance,
                window: None,
                gui_created: false,
            });
        }
        if let Ok(mut slot) = self.info.lock() {
            *slot = Some(info.clone());
        }
        self.set_error(None);
        Ok(info)
    }

    pub fn unload(&self) {
        self.close_editor();
        if let Ok(mut audio) = self.audio.lock() {
            audio.processor = None;
            audio.stopped = None;
        }
        if let Ok(mut slot) = self.main.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.info.lock() {
            *slot = None;
        }
        self.peak.store(0u32, Ordering::Relaxed);
        self.in_peak.store(0u32, Ordering::Relaxed);
    }

    pub fn parameters(&self) -> Vec<VstParam> {
        let Ok(mut slot) = self.main.lock() else {
            return Vec::new();
        };
        let Some(main) = slot.as_mut() else {
            return Vec::new();
        };
        let mut handle = main.instance.plugin_handle();
        let Some(params) = handle.get_extension::<PluginParams>() else {
            return Vec::new();
        };
        let count = params.count(&handle);
        let mut buffer = ParamInfoBuffer::new();
        let mut out = Vec::new();
        for index in 0..count {
            let Some(info) = params.get_info(&handle, index, &mut buffer) else {
                continue;
            };
            let id = info.id.get();
            let value = params
                .get_value(&handle, info.id)
                .unwrap_or(info.default_value);
            let mut text = [0u8; 128];
            let formatted = params
                .value_to_text(&handle, info.id, value, &mut text)
                .map(|slice| {
                    String::from_utf8_lossy(slice)
                        .trim_end_matches('\0')
                        .to_string()
                })
                .unwrap_or_default();
            out.push(VstParam {
                id,
                name: String::from_utf8_lossy(info.name).to_string(),
                unit: String::new(),
                value,
                step_count: if info.max_value - info.min_value > 0.0 {
                    -1
                } else {
                    0
                },
                formatted,
            });
        }
        out
    }

    /// CLAP has no host-side setter: a change is an event the plug-in receives
    /// inside its next `process` call. It is queued here and delivered then.
    pub fn set_parameter(&self, id: u32, value: f64) -> Result<(), String> {
        let id = ClapId::from_raw(id).ok_or_else(|| "invalid parameter id".to_string())?;
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "parameter queue unavailable".to_string())?;
        if pending.len() >= 8 {
            return Err("too many parameter changes queued".to_string());
        }
        pending.push((id, value));
        Ok(())
    }

    pub fn chain_summary(&self) -> Option<Vec<String>> {
        let params = self.parameters();
        let text = params
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case("Chain Summary"))?
            .formatted
            .clone();
        let names: Vec<String> = text
            .split('|')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect();
        if names.is_empty() {
            None
        } else {
            Some(names)
        }
    }

    pub fn save_state_to(&self, path: &Path) -> Result<(), String> {
        let Ok(mut slot) = self.main.lock() else {
            return Err("slot unavailable".to_string());
        };
        let Some(main) = slot.as_mut() else {
            let _ = std::fs::remove_file(path);
            return Ok(());
        };
        let mut handle = main.instance.plugin_handle();
        let state = handle
            .get_extension::<PluginState>()
            .ok_or_else(|| "plug-in exposes no state".to_string())?;
        let mut blob = Vec::new();
        state
            .save(&handle, &mut blob)
            .map_err(|e| format!("{e:?}"))?;
        drop(handle);
        let info = self.info().unwrap_or_else(|| VstInfo {
            path: PathBuf::new(),
            uid: String::new(),
            ..Default::default()
        });
        let mut bytes = Vec::with_capacity(blob.len() + 128);
        bytes.extend_from_slice(b"VMCLA1\n");
        bytes.extend_from_slice(info.path.display().to_string().as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(info.uid.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(&blob);
        std::fs::write(path, bytes).map_err(|e| e.to_string())
    }

    pub fn load_state_from(&self, path: &Path) -> Result<(), String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if !bytes.starts_with(b"VMCLA1\n") {
            // A VST3 blob: leave it alone, the slot may simply have changed format.
            return Ok(());
        }
        let mut rest = &bytes[7..];
        let Some(nl) = rest.iter().position(|b| *b == b'\n') else {
            return Err("bad state header".to_string());
        };
        rest = &rest[nl + 1..];
        let Some(nl) = rest.iter().position(|b| *b == b'\n') else {
            return Err("bad state header".to_string());
        };
        let blob = &rest[nl + 1..];

        let Ok(mut slot) = self.main.lock() else {
            return Err("slot unavailable".to_string());
        };
        let Some(main) = slot.as_mut() else {
            return Err("no plug-in loaded".to_string());
        };
        let mut handle = main.instance.plugin_handle();
        let state = handle
            .get_extension::<PluginState>()
            .ok_or_else(|| "plug-in exposes no state".to_string())?;
        let mut cursor = std::io::Cursor::new(blob);
        state
            .load(&handle, &mut cursor)
            .map_err(|e| format!("{e:?}"))
    }

    pub fn process(
        &self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) -> bool {
        if self.bypassed() {
            return false;
        }
        let Ok(mut audio) = self.audio.lock() else {
            return false;
        };
        let Audio {
            processor,
            stopped,
            input,
            output,
            input_ports,
            output_ports,
            input_events,
            output_events,
        } = &mut *audio;

        if processor.is_none() {
            if let Some(stopped) = stopped.take() {
                match stopped.start_processing() {
                    Ok(started) => *processor = Some(started),
                    Err(err) => {
                        self.set_error(Some(format!("start processing: {err}")));
                        return false;
                    }
                }
            }
        }
        let Some(processor) = processor.as_mut() else {
            return false;
        };

        let frames = out_l.len().min(out_r.len()).min(VST_MAX_BLOCK);
        if frames == 0 {
            return false;
        }
        let mut in_peak = 0.0f32;
        for i in 0..frames {
            let l = in_l.get(i).copied().unwrap_or(0.0);
            let r = in_r.get(i).copied().unwrap_or(0.0);
            in_peak = in_peak.max(l.abs()).max(r.abs());
            input[0][i] = l;
            input[1][i] = r;
            output[0][i] = 0.0;
            output[1][i] = 0.0;
        }
        self.in_peak.store(in_peak.to_bits(), Ordering::Relaxed);

        // Deliver queued parameter changes as events, which is how a CLAP host
        // adjusts a parameter. The buffer is preallocated to the queue cap, so
        // this does not allocate on the audio thread.
        input_events.clear();
        if let Ok(mut pending) = self.pending.try_lock() {
            for (id, value) in pending.drain(..) {
                input_events.push(&ParamValueEvent::new(0, id, Pckn::match_all(), value));
            }
        }
        let events = InputEvents::from_buffer(input_events);
        let mut out_events = OutputEvents::from_buffer(output_events);
        let mut input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input
                    .iter_mut()
                    .map(|ch| InputChannel::constant(&mut ch[..frames])),
            ),
        }]);
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                output.iter_mut().map(|ch| &mut ch[..frames]),
            ),
        }]);

        match processor.process(
            &input_audio,
            &mut output_audio,
            &events,
            &mut out_events,
            None,
            None,
        ) {
            Ok(_) => {
                let mut peak = 0.0f32;
                for i in 0..frames {
                    out_l[i] = output[0][i];
                    out_r[i] = output[1][i];
                    peak = peak.max(output[0][i].abs()).max(output[1][i].abs());
                }
                self.peak.store(peak.to_bits(), Ordering::Relaxed);
                true
            }
            Err(err) => {
                self.set_error(Some(format!("process: {err}")));
                false
            }
        }
    }

    pub fn open_editor(&self, mixer_hwnd: isize) -> Result<(), String> {
        let Ok(mut slot) = self.main.lock() else {
            return Err("slot unavailable".to_string());
        };
        let Some(main) = slot.as_mut() else {
            return Err("No CLAP plug-in is loaded".to_string());
        };
        if let Some(window) = main.window.as_mut() {
            if !window.is_closed() {
                // Clicking the insert panel again shows the window instead of
                // silently doing nothing.
                window.raise();
                return Ok(());
            }
            // WM_CLOSE only hides the host window, so the slot still thought
            // it had an editor while the person saw nothing. Drop the view and
            // fall through to build a new one.
            main.window = None;
            main.gui_created = false;
            let mut stale = main.instance.plugin_handle();
            if let Some(gui) = stale.get_extension::<PluginGui>() {
                let _ = gui.hide(&stale);
                gui.destroy(&stale);
            }
            self.gui_open.store(false, Ordering::Relaxed);
        }
        let mut handle = main.instance.plugin_handle();
        let gui = handle
            .get_extension::<PluginGui>()
            .ok_or_else(|| "This plug-in has no screen of its own".to_string())?;
        let configuration = GuiConfiguration {
            api_type: GuiApiType::WIN32,
            is_floating: false,
        };
        if !gui.is_api_supported(&handle, configuration) {
            return Err("The plug-in cannot embed its screen in a window".to_string());
        }
        gui.create(&handle, configuration)
            .map_err(|e| format!("Could not open the plug-in's screen: {e:?}"))?;
        let size = gui.get_size(&handle).unwrap_or(GuiSize {
            width: 900,
            height: 600,
        });
        let Some(mut window) =
            crate::host_window::HostWindow::open("", size.width as i32, size.height as i32)
        else {
            gui.destroy(&handle);
            return Err("Could not create the plug-in's window".to_string());
        };
        let parent = unsafe { Window::from_win32_hwnd(window.hwnd() as *mut std::ffi::c_void) };
        if let Err(err) = unsafe { gui.set_parent(&handle, parent) } {
            gui.destroy(&handle);
            return Err(format!("Could not attach the plug-in's screen: {err:?}"));
        }
        let _ = gui.set_scale(&handle, 1.0);
        let _ = gui.show(&handle);
        window.place_beside(mixer_hwnd);
        main.window = Some(window);
        main.gui_created = true;
        self.gui_open.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn service_editor(&self) {
        if !self.gui_open.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut slot) = self.main.lock() else {
            return;
        };
        let Some(main) = slot.as_mut() else {
            return;
        };
        let Some(window) = main.window.as_mut() else {
            return;
        };
        if window.is_closed() {
            drop(window);
            main.window = None;
            main.gui_created = false;
            self.gui_open.store(false, Ordering::Relaxed);
            {
                let mut handle = main.instance.plugin_handle();
                if let Some(gui) = handle.get_extension::<PluginGui>() {
                    gui.destroy(&handle);
                }
            }
        }
    }

    pub fn editor_is_open(&self) -> bool {
        self.gui_open.load(Ordering::Relaxed)
    }

    pub fn close_editor(&self) {
        let Ok(mut slot) = self.main.lock() else {
            return;
        };
        let Some(main) = slot.as_mut() else {
            return;
        };
        let had = main.window.take().is_some() || main.gui_created;
        if had {
            let mut handle = main.instance.plugin_handle();
            if let Some(gui) = handle.get_extension::<PluginGui>() {
                let _ = gui.hide(&handle);
                gui.destroy(&handle);
            }
            main.gui_created = false;
        }
        self.gui_open.store(false, Ordering::Relaxed);
    }

    pub fn snapshot_editor(&self, path: &str) -> Result<(), String> {
        let Ok(slot) = self.main.lock() else {
            return Err("slot unavailable".to_string());
        };
        let Some(main) = slot.as_ref() else {
            return Err("no plug-in".to_string());
        };
        let Some(window) = main.window.as_ref() else {
            return Err("no editor open".to_string());
        };
        crate::host_window::snapshot_hwnd(window.hwnd(), path)
    }
}
