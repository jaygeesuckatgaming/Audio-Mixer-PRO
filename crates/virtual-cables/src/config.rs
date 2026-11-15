use std::path::PathBuf;

use crate::uac::{Mode, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};

#[derive(Clone, Debug)]
pub struct Config {
    pub enabled: bool,
    pub cables: u8,
    pub buffer_ms: u32,
    /// Lowest injection latency the cable may use (5..=40 ms). The ring is
    /// still `buffer_ms` deep; this only caps how long audio is held back.
    pub min_latency_ms: u32,
    pub max_rate: u32,
    pub uac_mode: Mode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            cables: 4,
            // Balanced default: enough slack for USB/IP, much lower than the
            // old 250 ms default that dominated end-to-end latency.
            buffer_ms: 60,
            min_latency_ms: 20,
            max_rate: 48_000,
            uac_mode: Mode::Uac2,
        }
    }
}

impl Config {
    pub fn normalized(mut self) -> Self {
        self.cables = self.cables.clamp(1, 32);
        self.buffer_ms = self.buffer_ms.clamp(20, 5000);
        self.min_latency_ms = self.min_latency_ms.clamp(5, 40);
        self.max_rate = self.max_rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
        self
    }

    /// Import the standalone Go app's CONFIG.ini, if present.
    pub fn import_legacy() -> Option<Self> {
        let path = legacy_config_path()?;
        let text = std::fs::read_to_string(path).ok()?;
        let mut config = Config::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key.to_ascii_uppercase().as_str() {
                "CABLES" => {
                    if let Ok(n) = value.parse::<u8>() {
                        config.cables = n.clamp(1, 32);
                    }
                }
                "BUFFER_MS" => {
                    if let Ok(n) = value.parse::<u32>() {
                        config.buffer_ms = n.clamp(20, 5000);
                    }
                }
                "MIN_LATENCY_MS" => {
                    if let Ok(n) = value.parse::<u32>() {
                        config.min_latency_ms = n.clamp(5, 40);
                    }
                }
                "UAC_MODE" => {
                    if let Some(mode) = Mode::parse(value) {
                        config.uac_mode = mode;
                    }
                }
                "MAX_RATE" => {
                    if let Ok(n) = value.parse::<u32>() {
                        config.max_rate = n.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
                    }
                }
                _ => {}
            }
        }
        // 250 ms was the standalone app's old default, not a deliberate choice;
        // migrate it to the new balanced default.
        if config.buffer_ms == 250 {
            config.buffer_ms = 60;
        }
        Some(config)
    }
}

fn legacy_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(base)
            .join("Virtual Cables")
            .join("CONFIG.ini"),
    )
}
