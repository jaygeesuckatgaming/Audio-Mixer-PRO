//! One plug-in slot, whichever format it happens to be.
//!
//! The mixer was written against `VstChain`; CLAP slots expose the same shape,
//! so this enum keeps every call site unchanged while the backend differs.

use std::path::Path;
use std::sync::Mutex;

use crate::clap_plugin::ClapSlot;
use crate::vst::{VstChain, VstInfo, VstParam};

enum Inner {
    Vst3(std::sync::Arc<VstChain>),
    Clap(std::sync::Arc<ClapSlot>),
}

pub struct PlugIn {
    inner: Mutex<Inner>,
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
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            inner: Mutex::new(Inner::Vst3(VstChain::new())),
        })
    }

    /// Load a `.vst3` bundle or a `.clap` library (optionally `file#plugin-id`).
    pub fn load(&self, path: &Path, sample_rate: f64, max_block: usize) -> Result<VstInfo, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "The plug-in slot is unavailable".to_string())?;
        if is_clap(path) {
            let slot = ClapSlot::new();
            let info = slot.load(path, sample_rate, max_block)?;
            *guard = Inner::Clap(slot);
            Ok(info)
        } else {
            let chain = VstChain::new();
            let info = chain.load(path, sample_rate, max_block)?;
            *guard = Inner::Vst3(chain);
            Ok(info)
        }
    }

    pub fn unload(&self) {
        if let Ok(guard) = self.inner.lock() {
            match &*guard {
                Inner::Vst3(chain) => chain.unload(),
                Inner::Clap(slot) => slot.unload(),
            }
        }
    }

    pub fn has_plugin(&self) -> bool {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.has_plugin(),
                Inner::Clap(slot) => slot.has_plugin(),
            },
            Err(_) => false,
        }
    }

    pub fn info(&self) -> Option<VstInfo> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.info(),
                Inner::Clap(slot) => slot.info(),
            },
            Err(_) => None,
        }
    }

    pub fn error(&self) -> Option<String> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.error(),
                Inner::Clap(slot) => slot.error(),
            },
            Err(_) => None,
        }
    }

    pub fn diag(&self) -> String {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.diag(),
                Inner::Clap(slot) => slot.diag(),
            },
            Err(_) => "slot unavailable".to_string(),
        }
    }

    pub fn set_bypass(&self, bypass: bool) {
        if let Ok(guard) = self.inner.lock() {
            match &*guard {
                Inner::Vst3(chain) => chain.set_bypass(bypass),
                Inner::Clap(slot) => slot.set_bypass(bypass),
            }
        }
    }

    pub fn input_peak(&self) -> f32 {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.input_peak(),
                Inner::Clap(slot) => slot.input_peak(),
            },
            Err(_) => 0.0,
        }
    }

    pub fn latency(&self) -> (u32, u32) {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.latency(),
                Inner::Clap(slot) => slot.latency(),
            },
            Err(_) => (0, 0),
        }
    }

    pub fn chain_summary(&self) -> Option<Vec<String>> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.chain_summary(),
                Inner::Clap(slot) => slot.chain_summary(),
            },
            Err(_) => None,
        }
    }

    pub fn parameters(&self) -> Vec<VstParam> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.parameters(),
                Inner::Clap(slot) => slot.parameters(),
            },
            Err(_) => Vec::new(),
        }
    }

    pub fn set_parameter(&self, id: u32, value: f64) -> Result<(), String> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.set_parameter(id, value),
                Inner::Clap(slot) => slot.set_parameter(id, value),
            },
            Err(_) => Err("The plug-in slot is unavailable".to_string()),
        }
    }

    pub fn save_state_to(&self, path: &Path) -> Result<(), String> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.save_state_to(path),
                Inner::Clap(slot) => slot.save_state_to(path),
            },
            Err(_) => Err("The plug-in slot is unavailable".to_string()),
        }
    }

    pub fn load_state_from(&self, path: &Path) -> Result<(), String> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.load_state_from(path),
                Inner::Clap(slot) => slot.load_state_from(path),
            },
            Err(_) => Err("The plug-in slot is unavailable".to_string()),
        }
    }

    pub fn open_editor(&self, mixer_hwnd: isize) -> Result<(), String> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.open_editor(mixer_hwnd),
                Inner::Clap(slot) => slot.open_editor(mixer_hwnd),
            },
            Err(_) => Err("The plug-in slot is unavailable".to_string()),
        }
    }

    pub fn service_editor(&self) {
        if let Ok(guard) = self.inner.lock() {
            match &*guard {
                Inner::Vst3(chain) => chain.service_editor(),
                Inner::Clap(slot) => slot.service_editor(),
            }
        }
    }

    pub fn editor_is_open(&self) -> bool {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.editor_is_open(),
                Inner::Clap(slot) => slot.editor_is_open(),
            },
            Err(_) => false,
        }
    }

    pub fn close_editor(&self) {
        if let Ok(guard) = self.inner.lock() {
            match &*guard {
                Inner::Vst3(chain) => chain.close_editor(),
                Inner::Clap(slot) => slot.close_editor(),
            }
        }
    }

    pub fn snapshot_editor(&self, path: &str) -> Result<(), String> {
        match self.inner.lock() {
            Ok(guard) => match &*guard {
                Inner::Vst3(chain) => chain.snapshot_editor(path),
                Inner::Clap(slot) => slot.snapshot_editor(path),
            },
            Err(_) => Err("The plug-in slot is unavailable".to_string()),
        }
    }

    /// Render thread entry point. Uses `try_lock`: a slot being swapped under a
    /// load must never stall the audio callback.
    pub fn process(
        &self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) -> bool {
        let Ok(guard) = self.inner.try_lock() else {
            return false;
        };
        match &*guard {
            Inner::Vst3(chain) => chain.process(in_l, in_r, out_l, out_r),
            Inner::Clap(slot) => slot.process(in_l, in_r, out_l, out_r),
        }
    }
}
