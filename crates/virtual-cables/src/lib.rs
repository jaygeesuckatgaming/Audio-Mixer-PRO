//! Virtual USB audio cables over USB/IP (UAC2, with a UAC1 fallback).
//!
//! Ported from the Go Virtual Cables implementation. The crate is used by
//! Audio Mixer Pro's egui front end; the `vc` binary is the standalone
//! diagnostic/self-test entry point.

pub mod audio;
pub mod config;
pub mod uac;
pub mod usbip;

#[cfg(windows)]
pub mod win;

pub use config::Config;
pub use uac::{Descriptors, Device, Mode};

use std::sync::Arc;

/// Sink for human-readable diagnostics. The GUI/CLI decides where they go.
pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

/// Run the descriptor/control/audio self-test. Returns a human-readable error.
pub fn self_test(cfg: &Config) -> Result<String, String> {
    let mut report = String::new();
    for number in 1..=cfg.cables {
        let device = Device::new(number, cfg.buffer_ms, cfg.uac_mode, cfg.max_rate)?;
        device
            .descriptors
            .validate()
            .map_err(|err| format!("{}: {err}", device.product()))?;

        device.handle_control(
            uac::SetupPacket {
                request_type: 0x00,
                request: uac::REQUEST_SET_CONFIGURATION,
                value: 1,
                index: 0,
                length: 0,
            },
            &[],
        );

        let max_alt: u16 = if device.high_speed() { 3 } else { 1 };
        if device.high_speed() {
            let mut rate = device.max_rate().to_le_bytes().to_vec();
            rate.resize(4, 0);
            let (_, status) = device.handle_control(
                uac::SetupPacket {
                    request_type: 0x21,
                    request: uac::AUDIO_SET_CUR,
                    value: 0x0100,
                    index: 0x0A00,
                    length: 4,
                },
                &rate,
            );
            if status != 0 {
                return Err(format!("select {} Hz status {status}", device.max_rate()));
            }
        }

        for alt in 1..=max_alt {
            for iface in [1u16, 2] {
                let (_, status) = device.handle_control(
                    uac::SetupPacket {
                        request_type: 0x01,
                        request: uac::REQUEST_SET_INTERFACE,
                        value: alt,
                        index: iface,
                        length: 0,
                    },
                    &[],
                );
                if status != 0 {
                    return Err(format!("activate interface {iface} alt {alt} status {status}"));
                }
            }
            let pcm = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
            if device.write_playback(&pcm) != pcm.len() {
                return Err("PCM write failed".to_string());
            }
            let mut out = [0u8; 8];
            device.read_capture(&mut out);
            if out != pcm {
                return Err(format!("PCM loopback mismatch at alt {alt}"));
            }
        }

        if device.high_speed() {
            let source = if device.max_rate() <= 48_000 { 44_100 } else { 48_000 };
            device.set_playback_rate(source);
            device.set_capture_rate(device.max_rate());
            let frame = [0x00u8, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x40];
            for _ in 0..16 {
                device.write_playback(&frame);
            }
            let mut out = [0u8; 32];
            if device.read_capture(&mut out) != out.len() {
                return Err("resampler read failed".to_string());
            }
            if out.iter().all(|b| *b == 0) {
                return Err("resampler produced silence".to_string());
            }
            device.set_capture_rate(48_000);
            device.set_playback_rate(48_000);
        }

        report.push_str(&format!(
            "{} [{}] max={} Hz: ok\n",
            device.product(),
            device.bus_id,
            device.max_rate()
        ));
    }
    Ok(report)
}
