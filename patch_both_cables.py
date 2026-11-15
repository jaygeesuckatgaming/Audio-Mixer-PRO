import io

p = 'src/main.rs'
t = io.open(p, encoding='utf-8', newline='').read()


def sub(old, new):
    global t
    assert old in t, old[:80]
    t = t.replace(old, new, 1)


# Both cable sets can run at once: always start USB/IP when enabled.
sub(
    """            StartupStep::Cables => {
                self.driver_mode = driver_cables_present();
                if self.driver_mode {
                    diag_log("startup: virtual cable driver detected, USB/IP disabled");
                } else if self.vc_cfg.enabled {
                    diag_log("startup: starting virtual cables");
                    self.vc.apply(self.vc_cfg.clone());
                }""",
    """            StartupStep::Cables => {
                self.driver_mode = driver_cables_present();
                if self.driver_mode {
                    diag_log("startup: kernel virtual cable driver detected");
                }
                if self.vc_cfg.enabled {
                    diag_log("startup: starting USB virtual cables");
                    self.vc.apply(self.vc_cfg.clone());
                }""",
)

# Feed B1/B2 to the USB cables and to the driver's cables when both exist.
sub(
    """        if self.driver_mode {
            self.audio.set_virtual_outputs(None, None);
        } else {
            self.audio
                .set_virtual_outputs(self.vc.device(1), self.vc.device(2));
        }""",
    """        self.audio
            .set_virtual_outputs(self.vc.device(1), self.vc.device(2));""",
)

# cable_label: understand both naming schemes.
sub(
    """fn cable_label(device: &str) -> Option<String> {
    let lower = device.to_ascii_lowercase();
    let at = lower.find("virtual cable ")?;
    let digits: String = lower[at + "virtual cable ".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let number: u32 = digits.parse().ok()?;
    Some(format!("Virtual Cable {number:02}"))
}""",
    """fn cable_label(device: &str) -> Option<String> {
    let lower = device.to_ascii_lowercase();
    for (prefix, display) in [
        ("virtual cable ", "Virtual Cable "),
        ("usb cable ", "USB Cable "),
    ] {
        let Some(at) = lower.find(prefix) else {
            continue;
        };
        let digits: String = lower[at + prefix.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(number) = digits.parse::<u32>() {
            return Some(format!("{display}{number:02}"));
        }
    }
    None
}""",
)

# Virtual strips may bind either kind of cable.
sub(
    """            let Some(rest) = lower
                .strip_prefix("microphone array (virtual cable ")
                .or_else(|| lower.strip_prefix("microphone (virtual cable "))
            else {
                continue;
            };""",
    """            let Some(rest) = lower
                .strip_prefix("microphone array (virtual cable ")
                .or_else(|| lower.strip_prefix("microphone (virtual cable "))
                .or_else(|| lower.strip_prefix("microphone (usb cable "))
                .or_else(|| lower.strip_prefix("microphone array (usb cable "))
            else {
                continue;
            };""",
)

io.open(p, 'w', encoding='utf-8', newline='').write(t)
print('main.rs updated')

p = 'src/vc.rs'
t = io.open(p, encoding='utf-8', newline='').read()

sub(
    """            if driver_mode {
                ui.label(
                    egui::RichText::new(
                        "KERNEL DRIVER ACTIVE: Virtual Cable 01..04 come from the installed \\
                         audio driver. USB/IP is not used and the settings below are ignored.",
                    )
                    .size(9.0)
                    .color(GREEN),
                );
            } else {""",
    """            if driver_mode {
                ui.label(
                    egui::RichText::new(
                        "KERNEL DRIVER ACTIVE: Virtual Cable 01..04 come from the installed \\
                         audio driver. The USB/IP set below is named USB Cable 01..04 and can \\
                         run at the same time.",
                    )
                    .size(9.0)
                    .color(GREEN),
                );
            } else {""",
)

sub(
    """                    enabled_changed = ui
                        .add_enabled(
                            !driver_mode,
                            egui::Checkbox::new(&mut cfg.enabled, ""),
                        )
                        .changed();""",
    """                    enabled_changed = ui.checkbox(&mut cfg.enabled, "").changed();""",
)

io.open(p, 'w', encoding='utf-8', newline='').write(t)
print('vc.rs updated')
