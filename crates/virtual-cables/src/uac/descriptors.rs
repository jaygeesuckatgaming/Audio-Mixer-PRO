use std::collections::HashMap;

pub const DESC_DEVICE: u8 = 0x01;
pub const DESC_CONFIGURATION: u8 = 0x02;
pub const DESC_STRING: u8 = 0x03;
pub const DESC_INTERFACE: u8 = 0x04;
pub const DESC_ENDPOINT: u8 = 0x05;
pub const DESC_DEVICE_QUALIFIER: u8 = 0x06;
pub const DESC_CS_INTERFACE: u8 = 0x24;
pub const DESC_CS_ENDPOINT: u8 = 0x25;

const AC_HEADER: u8 = 0x01;
const AC_INPUT_TERMINAL: u8 = 0x02;
const AC_OUTPUT_TERMINAL: u8 = 0x03;
const AC_FEATURE_UNIT: u8 = 0x06;
const AC_CLOCK_SOURCE: u8 = 0x0A;
const AS_GENERAL: u8 = 0x01;
const AS_FORMAT_TYPE_I: u8 = 0x02;
const EP_GENERAL: u8 = 0x01;

pub const MIN_SAMPLE_RATE: u32 = 44_100;
pub const MAX_SAMPLE_RATE: u32 = 192_000;
pub const AUDIO_CHANNELS: usize = 2;

/// UAC2 entity identifiers share one namespace. The clock must not collide
/// with the terminal IDs 1, 3, 4, 6 or the feature-unit IDs 2, 5.
pub const CLOCK_SOURCE_ID: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Uac2,
    Uac1,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Uac2 => "uac2",
            Mode::Uac1 => "uac1",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "uac2" | "2" | "usb2" | "high" => Some(Mode::Uac2),
            "uac1" | "1" | "usb1" | "full" => Some(Mode::Uac1),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Descriptors {
    pub cable_number: u8,
    pub mode: Mode,
    pub product: String,
    pub serial: String,
    pub device: Vec<u8>,
    pub qualifier: Vec<u8>,
    pub config: Vec<u8>,
    pub strings: HashMap<u8, Vec<u8>>,
}

impl Descriptors {
    pub fn new(cable_number: u8, mode: Mode, max_rate: u32) -> Result<Self, String> {
        if cable_number == 0 {
            return Err("cable number must be 1..=255".to_string());
        }
        let max_rate = clamp_rate(max_rate);
        let product = format!("Virtual Cable {cable_number:02}");
        let serial = format!("VCABLE-{cable_number:03}");
        let mut strings = HashMap::new();
        strings.insert(0, vec![4, DESC_STRING, 0x09, 0x04]);
        strings.insert(1, string_descriptor("Tarek Wasfy and AI"));
        strings.insert(2, string_descriptor(&product));
        strings.insert(3, string_descriptor(&serial));

        let device = device_descriptor(cable_number, mode);
        let (qualifier, config) = match mode {
            Mode::Uac2 => (device_qualifier(), configuration_descriptor_uac2(max_rate)),
            Mode::Uac1 => (Vec::new(), configuration_descriptor_uac1()),
        };
        let descriptors = Self {
            cable_number,
            mode,
            product,
            serial,
            device,
            qualifier,
            config,
            strings,
        };
        descriptors.validate()?;
        Ok(descriptors)
    }

    pub fn get_descriptor(&self, descriptor_type: u8, index: u8) -> Option<Vec<u8>> {
        match descriptor_type {
            DESC_DEVICE if index == 0 => Some(self.device.clone()),
            DESC_CONFIGURATION if index == 0 => Some(self.config.clone()),
            DESC_DEVICE_QUALIFIER if index == 0 && !self.qualifier.is_empty() => {
                Some(self.qualifier.clone())
            }
            DESC_STRING => self.strings.get(&index).cloned(),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.device.len() != 18 || self.device[0] != 18 || self.device[1] != DESC_DEVICE {
            return Err("invalid device descriptor".to_string());
        }
        let want_usb = match self.mode {
            Mode::Uac2 => 0x0200u16,
            Mode::Uac1 => 0x0110u16,
        };
        if self.mode == Mode::Uac2
            && (self.qualifier.len() != 10 || self.qualifier[1] != DESC_DEVICE_QUALIFIER)
        {
            return Err("invalid device qualifier".to_string());
        }
        let usb = u16::from_le_bytes([self.device[2], self.device[3]]);
        if usb != want_usb {
            return Err(format!("bcdUSB=0x{usb:04x}, want 0x{want_usb:04x}"));
        }
        if self.config.len() < 9 || self.config[1] != DESC_CONFIGURATION {
            return Err("invalid configuration descriptor".to_string());
        }
        let total = u16::from_le_bytes([self.config[2], self.config[3]]) as usize;
        if total != self.config.len() {
            return Err(format!(
                "configuration wTotalLength={total}, actual={}",
                self.config.len()
            ));
        }
        let mut pos = 0usize;
        while pos < self.config.len() {
            let length = self.config[pos] as usize;
            if length < 2 || pos + length > self.config.len() {
                return Err(format!("invalid descriptor at offset {pos}"));
            }
            pos += length;
        }
        Ok(())
    }
}

pub fn clamp_rate(rate: u32) -> u32 {
    rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE)
}

fn device_descriptor(cable_number: u8, mode: Mode) -> Vec<u8> {
    let pid = 0xCA00u16.wrapping_add(cable_number as u16);
    let mut b = vec![0u8; 18];
    b[0] = 18;
    b[1] = DESC_DEVICE;
    let usb = if mode == Mode::Uac2 {
        0x0200u16
    } else {
        0x0110u16
    };
    b[2..4].copy_from_slice(&usb.to_le_bytes());
    b[7] = 64;
    b[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes());
    b[10..12].copy_from_slice(&pid.to_le_bytes());
    b[12..14].copy_from_slice(&0x0001u16.to_le_bytes());
    b[14] = 1;
    b[15] = 2;
    b[16] = 3;
    b[17] = 1;
    b
}

fn device_qualifier() -> Vec<u8> {
    vec![10, DESC_DEVICE_QUALIFIER, 0x00, 0x02, 0, 0, 0, 64, 0, 0]
}

fn configuration_descriptor_uac2(max_rate: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[9, DESC_CONFIGURATION, 0, 0, 3, 1, 0, 0x80, 50]);
    b.extend_from_slice(&[9, DESC_INTERFACE, 0, 0, 0, 0x01, 0x01, 0x20, 0]);
    b.extend_from_slice(&uac2_ac_descriptors());
    b.extend_from_slice(&uac2_as_interface(1, 1, 0x01, 0x09, max_rate));
    b.extend_from_slice(&uac2_as_interface(2, 6, 0x82, 0x0D, max_rate));
    let total = b.len() as u16;
    b[2..4].copy_from_slice(&total.to_le_bytes());
    b
}

fn uac2_ac_descriptors() -> Vec<u8> {
    let mut ac = Vec::new();
    ac.extend_from_slice(&[
        9,
        DESC_CS_INTERFACE,
        AC_HEADER,
        0x00,
        0x02,
        0x00,
        0,
        0,
        0x00,
    ]);
    ac.extend_from_slice(&[
        8,
        DESC_CS_INTERFACE,
        AC_CLOCK_SOURCE,
        CLOCK_SOURCE_ID,
        0x03,
        0x03,
        0,
        0,
    ]);
    ac.extend_from_slice(&[
        17,
        DESC_CS_INTERFACE,
        AC_INPUT_TERMINAL,
        1,
        0x01,
        0x01,
        0,
        CLOCK_SOURCE_ID,
        2,
        0x03,
        0,
        0,
        0,
        0,
        0x00,
        0x00,
        0,
    ]);
    ac.extend_from_slice(&[
        18,
        DESC_CS_INTERFACE,
        AC_FEATURE_UNIT,
        2,
        1,
        0x03,
        0,
        0,
        0,
        0x03,
        0,
        0,
        0,
        0x03,
        0,
        0,
        0,
        0,
    ]);
    ac.extend_from_slice(&[
        12,
        DESC_CS_INTERFACE,
        AC_OUTPUT_TERMINAL,
        3,
        0x01,
        0x03,
        0,
        2,
        CLOCK_SOURCE_ID,
        0x00,
        0x00,
        0,
    ]);
    ac.extend_from_slice(&[
        17,
        DESC_CS_INTERFACE,
        AC_INPUT_TERMINAL,
        4,
        0x01,
        0x02,
        0,
        CLOCK_SOURCE_ID,
        2,
        0x03,
        0,
        0,
        0,
        0,
        0x00,
        0x00,
        0,
    ]);
    ac.extend_from_slice(&[
        18,
        DESC_CS_INTERFACE,
        AC_FEATURE_UNIT,
        5,
        4,
        0x03,
        0,
        0,
        0,
        0x03,
        0,
        0,
        0,
        0x03,
        0,
        0,
        0,
        0,
    ]);
    ac.extend_from_slice(&[
        12,
        DESC_CS_INTERFACE,
        AC_OUTPUT_TERMINAL,
        6,
        0x01,
        0x01,
        0,
        5,
        CLOCK_SOURCE_ID,
        0x00,
        0x00,
        0,
    ]);
    let total = ac.len() as u16;
    ac[6..8].copy_from_slice(&total.to_le_bytes());
    ac
}

fn uac2_as_interface(
    interface_number: u8,
    terminal_link: u8,
    endpoint: u8,
    attributes: u8,
    max_rate: u32,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[
        9,
        DESC_INTERFACE,
        interface_number,
        0,
        0,
        0x01,
        0x02,
        0x20,
        0,
    ]);
    for (alt, (bytes, resolution)) in [(2u8, 16u8), (3, 24), (4, 32)].into_iter().enumerate() {
        b.extend_from_slice(&[
            9,
            DESC_INTERFACE,
            interface_number,
            alt as u8 + 1,
            1,
            0x01,
            0x02,
            0x20,
            0,
        ]);
        b.extend_from_slice(&[
            16,
            DESC_CS_INTERFACE,
            AS_GENERAL,
            terminal_link,
            0x00,
            0x01,
            0x01,
            0,
            0,
            0,
            2,
            0x03,
            0,
            0,
            0,
            0,
        ]);
        b.extend_from_slice(&[
            6,
            DESC_CS_INTERFACE,
            AS_FORMAT_TYPE_I,
            0x01,
            bytes,
            resolution,
        ]);
        let max_packet = (AUDIO_CHANNELS as u32 * bytes as u32 * max_rate / 8000) as u16;
        b.extend_from_slice(&[
            9,
            DESC_ENDPOINT,
            endpoint,
            attributes,
            max_packet as u8,
            (max_packet >> 8) as u8,
            1,
            0,
            0,
        ]);
        b.extend_from_slice(&[7, DESC_CS_ENDPOINT, EP_GENERAL, 0x00, 0, 0, 0]);
    }
    b
}

fn configuration_descriptor_uac1() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[9, DESC_CONFIGURATION, 0, 0, 3, 1, 0, 0x80, 50]);
    b.extend_from_slice(&[9, DESC_INTERFACE, 0, 0, 0, 0x01, 0x01, 0x00, 0]);
    b.extend_from_slice(&[10, DESC_CS_INTERFACE, 0x01, 0x00, 0x01, 72, 0, 2, 1, 2]);
    b.extend_from_slice(&[
        12,
        DESC_CS_INTERFACE,
        0x02,
        1,
        0x01,
        0x01,
        0,
        2,
        0x03,
        0x00,
        0,
        0,
    ]);
    b.extend_from_slice(&[10, DESC_CS_INTERFACE, 0x06, 2, 1, 1, 0x03, 0x00, 0x00, 0]);
    b.extend_from_slice(&[9, DESC_CS_INTERFACE, 0x03, 3, 0x01, 0x03, 0, 2, 0]);
    b.extend_from_slice(&[
        12,
        DESC_CS_INTERFACE,
        0x02,
        4,
        0x01,
        0x02,
        0,
        2,
        0x03,
        0x00,
        0,
        0,
    ]);
    b.extend_from_slice(&[10, DESC_CS_INTERFACE, 0x06, 5, 4, 1, 0x03, 0x00, 0x00, 0]);
    b.extend_from_slice(&[9, DESC_CS_INTERFACE, 0x03, 6, 0x01, 0x01, 0, 5, 0]);

    b.extend_from_slice(&[9, DESC_INTERFACE, 1, 0, 0, 0x01, 0x02, 0x00, 0]);
    b.extend_from_slice(&[9, DESC_INTERFACE, 1, 1, 1, 0x01, 0x02, 0x00, 0]);
    b.extend_from_slice(&[7, DESC_CS_INTERFACE, 0x01, 1, 1, 0x01, 0x00]);
    b.extend_from_slice(&[
        11,
        DESC_CS_INTERFACE,
        0x02,
        1,
        2,
        2,
        16,
        1,
        0x80,
        0xBB,
        0x00,
    ]);
    b.extend_from_slice(&[9, DESC_ENDPOINT, 0x01, 0x09, 0xC0, 0x00, 1, 0, 0]);
    b.extend_from_slice(&[7, DESC_CS_ENDPOINT, 0x01, 0, 0, 0, 0]);

    b.extend_from_slice(&[9, DESC_INTERFACE, 2, 0, 0, 0x01, 0x02, 0x00, 0]);
    b.extend_from_slice(&[9, DESC_INTERFACE, 2, 1, 1, 0x01, 0x02, 0x00, 0]);
    b.extend_from_slice(&[7, DESC_CS_INTERFACE, 0x01, 6, 1, 0x01, 0x00]);
    b.extend_from_slice(&[
        11,
        DESC_CS_INTERFACE,
        0x02,
        1,
        2,
        2,
        16,
        1,
        0x80,
        0xBB,
        0x00,
    ]);
    b.extend_from_slice(&[9, DESC_ENDPOINT, 0x82, 0x0D, 0xC0, 0x00, 1, 0, 0]);
    b.extend_from_slice(&[7, DESC_CS_ENDPOINT, 0x01, 0, 0, 0, 0]);

    let total = b.len() as u16;
    b[2..4].copy_from_slice(&total.to_le_bytes());
    b
}

fn string_descriptor(value: &str) -> Vec<u8> {
    let mut encoded: Vec<u16> = value.encode_utf16().collect();
    if encoded.len() > 126 {
        encoded.truncate(126);
    }
    let length = 2 + encoded.len() * 2;
    let mut b = vec![0u8; length];
    b[0] = length as u8;
    b[1] = DESC_STRING;
    for (i, unit) in encoded.iter().enumerate() {
        b[2 + i * 2..4 + i * 2].copy_from_slice(&unit.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint_descriptors(d: &Descriptors) -> (Vec<u16>, Vec<u16>) {
        let mut playback = Vec::new();
        let mut capture = Vec::new();
        let mut pos = 0usize;
        while pos < d.config.len() {
            let length = d.config[pos] as usize;
            let desc = &d.config[pos..pos + length];
            if desc[1] == DESC_ENDPOINT {
                let packet = u16::from_le_bytes([desc[4], desc[5]]);
                match desc[2] {
                    0x01 => playback.push(packet),
                    0x82 => capture.push(packet),
                    _ => {}
                }
            }
            pos += length;
        }
        (playback, capture)
    }

    #[test]
    fn uac2_validates() {
        let d = Descriptors::new(1, Mode::Uac2, MAX_SAMPLE_RATE).unwrap();
        assert_eq!(u16::from_le_bytes([d.device[2], d.device[3]]), 0x0200);
        assert_eq!(d.qualifier.len(), 10);
        assert_eq!(d.config[4], 3);
    }

    #[test]
    fn uac1_fallback_validates() {
        let d = Descriptors::new(1, Mode::Uac1, 48_000).unwrap();
        assert_eq!(u16::from_le_bytes([d.device[2], d.device[3]]), 0x0110);
        assert!(d.qualifier.is_empty());
    }

    #[test]
    fn endpoint_packet_sizes_192k() {
        let d = Descriptors::new(1, Mode::Uac2, 192_000).unwrap();
        let (playback, capture) = endpoint_descriptors(&d);
        assert_eq!(playback, vec![96, 144, 192]);
        assert_eq!(capture, vec![96, 144, 192]);
    }

    #[test]
    fn endpoint_packet_sizes_48k() {
        let d = Descriptors::new(1, Mode::Uac2, 48_000).unwrap();
        let (playback, capture) = endpoint_descriptors(&d);
        assert_eq!(playback, vec![24, 36, 48]);
        assert_eq!(capture, vec![24, 36, 48]);
    }

    #[test]
    fn clock_id_is_unique_and_used_by_terminals() {
        let d = Descriptors::new(1, Mode::Uac2, 48_000).unwrap();
        let mut clock = 0;
        let mut formats = 0;
        let mut pos = 0usize;
        while pos < d.config.len() {
            let length = d.config[pos] as usize;
            let desc = &d.config[pos..pos + length];
            if desc[1] == DESC_CS_INTERFACE {
                if desc[2] == AC_CLOCK_SOURCE {
                    assert_eq!(desc[3], CLOCK_SOURCE_ID);
                    clock += 1;
                }
                if desc[2] == AS_FORMAT_TYPE_I && length == 6 {
                    formats += 1;
                }
            }
            pos += length;
        }
        assert_eq!(clock, 1);
        assert_eq!(formats, 6);
    }
}
