//! One plug-in slot, whichever format it happens to be.
//!
//! The mixer was written against `VstChain`; CLAP slots expose the same shape,
//! so this enum keeps every call site unchanged while the backend differs.
//!
//! The backend sits behind an `Arc` that is swapped only on load/unload, so the
//! render thread processes without holding the slot lock: the UI's per-frame
//! queries (has_plugin, peaks, chain summary) never wait on audio work.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::clap_plugin::ClapSlot;
use crate::vst::{VstChain, VstInfo, VstParam};

enum Backend {
    Vst3(Arc<VstChain>),
    Clap(Arc<ClapSlot>),
}

pub struct PlugIn {
    current: Mutex<Arc<Backend>>,
}

fn is_clap(path: &Path) -> bool {
    // The scanner appends `#plugin-id` for libraries that hold several
    // plug-ins, so judge the extension of the part before the separator.
    let text = path.to_string_lossy();
    let file = text.split('#').next().unwrap_or(&text);
    Path::new(file)
        .extension()
        .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("clap"))
        .unwrap_or(false)
}

impl PlugIn {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(Arc::new(Backend::Vst3(VstChain::new()))),
        })
    }

    /// Cheap snapshot: the lock is held only long enough to clone the Arc.
    fn backend(&self) -> Arc<Backend> {
        match self.current.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Load a `.vst3` bundle or a `.clap` library (optionally `file#plugin-id`).
    pub fn load(&self, path: &Path, sample_rate: f64, max_block: usize) -> Result<VstInfo, String> {
        let backend = if is_clap(path) {
            let slot = ClapSlot::new();
            let info = slot.load(path, sample_rate, max_block)?;
            (Arc::new(Backend::Clap(slot)), info)
        } else {
            let chain = VstChain::new();
            let info = chain.load(path, sample_rate, max_block)?;
            (Arc::new(Backend::Vst3(chain)), info)
        };
        let mut guard = self
            .current
            .lock()
            .map_err(|_| "The plug-in slot is unavailable".to_string())?;
        *guard = backend.0;
        Ok(backend.1)
    }

    pub fn unload(&self) {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.unload(),
            Backend::Clap(slot) => slot.unload(),
        }
    }

    pub fn has_plugin(&self) -> bool {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.has_plugin(),
            Backend::Clap(slot) => slot.has_plugin(),
        }
    }

    pub fn info(&self) -> Option<VstInfo> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.info(),
            Backend::Clap(slot) => slot.info(),
        }
    }

    pub fn error(&self) -> Option<String> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.error(),
            Backend::Clap(slot) => slot.error(),
        }
    }

    pub fn diag(&self) -> String {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.diag(),
            Backend::Clap(slot) => slot.diag(),
        }
    }

    pub fn set_bypass(&self, bypass: bool) {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.set_bypass(bypass),
            Backend::Clap(slot) => slot.set_bypass(bypass),
        }
    }

    pub fn input_peak(&self) -> f32 {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.input_peak(),
            Backend::Clap(slot) => slot.input_peak(),
        }
    }

    pub fn latency(&self) -> (u32, u32) {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.latency(),
            Backend::Clap(slot) => slot.latency(),
        }
    }

    pub fn chain_summary(&self) -> Option<Vec<String>> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.chain_summary(),
            Backend::Clap(slot) => slot.chain_summary(),
        }
    }

    pub fn parameters(&self) -> Vec<VstParam> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.parameters(),
            Backend::Clap(slot) => slot.parameters(),
        }
    }

    pub fn set_parameter(&self, id: u32, value: f64) -> Result<(), String> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.set_parameter(id, value),
            Backend::Clap(slot) => slot.set_parameter(id, value),
        }
    }

    pub fn save_state_to(&self, path: &Path) -> Result<(), String> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.save_state_to(path),
            Backend::Clap(slot) => slot.save_state_to(path),
        }
    }

    pub fn load_state_from(&self, path: &Path) -> Result<(), String> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.load_state_from(path),
            Backend::Clap(slot) => slot.load_state_from(path),
        }
    }

    pub fn open_editor(&self, mixer_hwnd: isize) -> Result<(), String> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.open_editor(mixer_hwnd),
            Backend::Clap(slot) => slot.open_editor(mixer_hwnd),
        }
    }

    pub fn service_editor(&self) {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.service_editor(),
            Backend::Clap(slot) => slot.service_editor(),
        }
    }

    pub fn editor_is_open(&self) -> bool {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.editor_is_open(),
            Backend::Clap(slot) => slot.editor_is_open(),
        }
    }

    pub fn close_editor(&self) {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.close_editor(),
            Backend::Clap(slot) => slot.close_editor(),
        }
    }

    pub fn snapshot_editor(&self, path: &str) -> Result<(), String> {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.snapshot_editor(path),
            Backend::Clap(slot) => slot.snapshot_editor(path),
        }
    }

    /// Render thread entry point: no lock is held while the plug-in runs.
    pub fn process(
        &self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) -> bool {
        match &*self.backend() {
            Backend::Vst3(chain) => chain.process(in_l, in_r, out_l, out_r),
            Backend::Clap(slot) => slot.process(in_l, in_r, out_l, out_r),
        }
    }
}
