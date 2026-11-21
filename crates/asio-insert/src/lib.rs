//! `audio_mixer_pro_insert.dll` - the virtual ASIO device a DAW loads.
//!
//! Phase 1 of the external-insert work: the driver, the shared section and the
//! probe. It is not wired to the mixer yet, and nothing writes the registry.

#![allow(non_snake_case, dead_code)]

pub mod abi;
pub mod driver;
pub mod section;
