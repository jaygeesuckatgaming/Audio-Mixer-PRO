//! Minimal CLAP host callbacks.
//!
//! CLAP splits the host into three thread specifications, and `clack` mirrors
//! that in the type system: `Shared` for thread-safe callbacks, `MainThread`
//! for instance/GUI work, and `AudioProcessor` for the render thread. Keeping
//! to that split is what lets plug-ins that spawn their own threads work
//! correctly.

use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl};
use clack_host::extensions::prelude::*;
use clack_host::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// State shared by every thread a plug-in may call us from.
#[derive(Default)]
pub struct ClapShared {
    pub restart_requested: AtomicBool,
    pub process_requested: AtomicBool,
    pub callback_requested: AtomicBool,
    /// Client size the plug-in asked for, packed as `(w << 32) | h`; zero when
    /// there is no pending request. The UI thread picks this up and resizes.
    pub requested_size: AtomicU64,
    /// Set when the plug-in reports its own GUI window was closed.
    pub gui_closed: AtomicBool,
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

impl HostGuiImpl for ClapShared {
    fn resize_hints_changed(&self) {}

    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        self.requested_size.store(
            ((new_size.width as u64) << 32) | new_size.height as u64,
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn closed(&self, _was_destroyed: bool) {
        self.gui_closed.store(true, Ordering::Relaxed);
    }
}

/// The host implementation itself. Extensions (params, state, gui, latency…)
/// are registered here as they are added.
pub struct ClapHost;

impl HostHandlers for ClapHost {
    type Shared<'a> = ClapShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostGui>();
    }
}
