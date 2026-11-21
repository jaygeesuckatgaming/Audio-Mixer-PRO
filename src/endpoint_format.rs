//! Bit depth and sample rate Windows has configured for the endpoints the
//! strips and buses are bound to.
//!
//! The value is the endpoint's `PKEY_AudioEngine_DeviceFormat` — the same one
//! the Windows "Sound > Advanced" dialog shows — read from the device property
//! store. That store is a WASAPI call, so the whole scan lives on its own
//! worker thread: enumerating endpoints on the UI thread blocks for minutes
//! when a cable endpoint is wedged (see `sessions.rs` for the same rule).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// How often the property store is re-read, so a change made in the Windows
/// dialog shows up on the strip without a restart.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointFormat {
    /// Significant bits per sample (24 for a 24-in-32 container).
    pub bits: u32,
    pub rate: u32,
    /// IEEE float rather than integer PCM.
    pub float: bool,
}

impl EndpointFormat {
    /// "32-bit · 48 kHz"
    pub fn label(&self) -> String {
        let depth = if self.float {
            format!("{}-bit float", self.bits)
        } else {
            format!("{}-bit", self.bits)
        };
        format!("{depth} \u{b7} {}", rate_label(self.rate))
    }

    /// "32/48k" — the narrow-strip form.
    pub fn compact(&self) -> String {
        format!(
            "{}{}/{}",
            self.bits,
            if self.float { "f" } else { "" },
            rate_compact(self.rate)
        )
    }
}

fn rate_label(rate: u32) -> String {
    match rate {
        44_100 => "44.1 kHz".to_string(),
        48_000 => "48 kHz".to_string(),
        88_200 => "88.2 kHz".to_string(),
        96_000 => "96 kHz".to_string(),
        176_400 => "176.4 kHz".to_string(),
        192_000 => "192 kHz".to_string(),
        other => format!("{other} Hz"),
    }
}

fn rate_compact(rate: u32) -> String {
    match rate {
        44_100 => "44.1k".to_string(),
        48_000 => "48k".to_string(),
        88_200 => "88.2k".to_string(),
        96_000 => "96k".to_string(),
        176_400 => "176.4k".to_string(),
        192_000 => "192k".to_string(),
        other => other.to_string(),
    }
}

pub struct EndpointFormats {
    map: Arc<Mutex<HashMap<String, EndpointFormat>>>,
}

impl EndpointFormats {
    pub fn new() -> Self {
        let map = Arc::new(Mutex::new(HashMap::new()));
        let worker_map = map.clone();
        let _ = std::thread::Builder::new()
            .name("endpoint-formats".to_string())
            .spawn(move || worker(worker_map));
        Self { map }
    }

    /// Format Windows has configured for this endpoint, matched by its friendly
    /// name ("Microphone (USB Cable 03)"). A name that differs only in how the
    /// composed parenthetical reads still resolves through the cable label.
    pub fn get(&self, device: &str) -> Option<EndpointFormat> {
        let map = self.map.lock().ok()?;
        if map.is_empty() {
            return None;
        }
        if let Some(format) = map.get(device) {
            return Some(*format);
        }
        let lower = device.to_ascii_lowercase();
        let same_direction = |name: &str| {
            let name = name.to_ascii_lowercase();
            if lower.starts_with("microphone") {
                name.starts_with("microphone")
            } else if lower.starts_with("speakers") {
                name.starts_with("speakers")
            } else {
                true
            }
        };
        let label = crate::cable_label(device);
        for prefer_cable in [true, false] {
            for (name, format) in map.iter() {
                if !name.eq_ignore_ascii_case(device) {
                    continue;
                }
                if same_direction(name) {
                    return Some(*format);
                }
            }
            let Some(label) = label.as_deref() else { break };
            for (name, format) in map.iter() {
                if crate::cable_label(name).as_deref() != Some(label) {
                    continue;
                }
                if !prefer_cable || same_direction(name) {
                    return Some(*format);
                }
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.map.lock().map(|map| map.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for EndpointFormats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
fn worker(map: Arc<Mutex<HashMap<String, EndpointFormat>>>) {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut previous: HashMap<String, EndpointFormat> = HashMap::new();
    loop {
        let next = unsafe { scan() };
        if next.is_empty() && !previous.is_empty() {
            crate::diag_log("endpoint format: no active endpoints, keeping the last readout");
        } else {
            for (name, format) in &next {
                if !name.to_ascii_lowercase().contains("cable") {
                    continue;
                }
                if previous.get(name) != Some(format) {
                    crate::diag_log(format!("endpoint format: {name} = {}", format.label()));
                }
            }
            if let Ok(mut guard) = map.lock() {
                *guard = next.clone();
            }
            previous = next;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(not(windows))]
fn worker(_map: Arc<Mutex<HashMap<String, EndpointFormat>>>) {}

/// One diagnostic dump for `--apps-selftest`.
#[cfg(windows)]
pub fn self_test() -> String {
    let mut report = String::from("\nendpoint formats (Windows Sound > Advanced):\n");
    let formats = unsafe { scan() };
    let mut rows: Vec<(&String, &EndpointFormat)> = formats.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (name, format) in rows {
        report.push_str(&format!(
            "  {name:<44} {} ({} Hz, {} bits{})\n",
            format.label(),
            format.rate,
            format.bits,
            if format.float { ", float" } else { "" }
        ));
    }
    if formats.is_empty() {
        report.push_str("  none\n");
    }
    report
}

#[cfg(not(windows))]
pub fn self_test() -> String {
    "endpoint formats are Windows-only\n".to_string()
}

#[cfg(windows)]
unsafe fn scan() -> HashMap<String, EndpointFormat> {
    use windows::Win32::Media::Audio::{
        eCapture, eRender, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL, STGM_READ};
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    // PKEY_Device_FriendlyName — the string the strip is bound by.
    const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };
    // PKEY_AudioEngine_DeviceFormat — the "default format" of the endpoint.
    const DEVICE_FORMAT: PROPERTYKEY = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c),
        pid: 0,
    };

    let mut out = HashMap::new();
    let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(
        &MMDeviceEnumerator,
        None::<&windows::core::IUnknown>,
        CLSCTX_ALL,
    ) else {
        return out;
    };
    for flow in [eRender, eCapture] {
        let Ok(list) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
            continue;
        };
        let count = list.GetCount().unwrap_or(0);
        for index in 0..count {
            let Ok(device) = list.Item(index) else {
                continue;
            };
            let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
                continue;
            };
            let Ok(name_value) = store.GetValue(&FRIENDLY_NAME) else {
                continue;
            };
            let name = name_value.to_string();
            if name.is_empty() {
                continue;
            }
            let Ok(format_value) = store.GetValue(&DEVICE_FORMAT) else {
                continue;
            };
            if let Some(format) = parse_format(&format_value) {
                out.entry(name).or_insert(format);
            }
        }
    }
    out
}

/// The format property is a `VT_BLOB` holding a WAVEFORMATEX(TENSIBLE).
#[cfg(windows)]
unsafe fn parse_format(value: &windows::core::PROPVARIANT) -> Option<EndpointFormat> {
    use windows::Win32::System::Variant::VT_BLOB;

    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

    let raw = value.as_raw();
    if raw.Anonymous.Anonymous.vt != VT_BLOB.0 {
        return None;
    }
    let blob = raw.Anonymous.Anonymous.Anonymous.blob;
    let len = blob.cbSize as usize;
    if blob.pBlobData.is_null() || len < 16 {
        return None;
    }
    let data = std::slice::from_raw_parts(blob.pBlobData, len);
    let tag = u16::from_le_bytes([data[0], data[1]]);
    let rate = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let mut bits = u16::from_le_bytes([data[14], data[15]]) as u32;
    let mut float = tag == WAVE_FORMAT_IEEE_FLOAT;
    if len >= 40 {
        // WAVEFORMATEXTENSIBLE: valid bits at 18, sub-format GUID at 24.
        let valid = u16::from_le_bytes([data[18], data[19]]);
        if valid > 0 {
            bits = valid as u32;
        }
        if tag == WAVE_FORMAT_EXTENSIBLE {
            let sub = u16::from_le_bytes([data[24], data[25]]);
            float = sub == WAVE_FORMAT_IEEE_FLOAT;
        }
    }
    if rate == 0 || bits == 0 || bits > 64 {
        return None;
    }
    Some(EndpointFormat { bits, rate, float })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_read_like_the_windows_dialog() {
        let pcm = EndpointFormat {
            bits: 32,
            rate: 48_000,
            float: false,
        };
        assert_eq!(pcm.label(), "32-bit \u{b7} 48 kHz");
        assert_eq!(pcm.compact(), "32/48k");
        let float = EndpointFormat {
            bits: 32,
            rate: 44_100,
            float: true,
        };
        assert_eq!(float.label(), "32-bit float \u{b7} 44.1 kHz");
        assert_eq!(float.compact(), "32f/44.1k");
    }

    #[test]
    fn odd_rates_keep_their_own_number() {
        let odd = EndpointFormat {
            bits: 24,
            rate: 40_000,
            float: false,
        };
        assert_eq!(odd.label(), "24-bit \u{b7} 40000 Hz");
        assert_eq!(odd.compact(), "24/40000");
    }
}
