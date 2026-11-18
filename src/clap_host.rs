//! Minimal CLAP host callbacks.
//!
//! CLAP splits the host into three thread specifications, and `clack` mirrors
//! that in the type system: `Shared` for thread-safe callbacks, `MainThread`
//! for instance/GUI work, and `AudioProcessor` for the render thread. Keeping
//! to that split is what lets plug-ins that spawn their own threads work
//! correctly.

use clack_host::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// State shared by every thread a plug-in may call us from.
#[derive(Default)]
pub struct ClapShared {
    pub restart_requested: AtomicBool,
    pub process_requested: AtomicBool,
    pub callback_requested: AtomicBool,
}

impl<'a> SharedHandler<'a> for ClapShared {
    fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Relaxed);
    }

    fn request_process(&self) {
        self.process_requested.store(true, Ordering::Relaxed);
    }

    fn request_callback(&self) {
        self.callback_requested.store(true, Ordering::Relaxed);
    }
}

/// The host implementation itself. Extensions (params, state, gui, latency…)
/// are registered here as they are added.
pub struct ClapHost;

impl HostHandlers for ClapHost {
    type Shared<'a> = ClapShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}
