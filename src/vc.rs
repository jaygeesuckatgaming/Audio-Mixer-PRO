//! Virtual Cables panel: owns the Rust USB/IP server and the elevated attach
//! helper on a worker thread so the egui loop never blocks on UAC or usbip.exe.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use virtual_cables::usbip::{spawn_server, ServerHandle};
use virtual_cables::win::{start_broker, BrokerClient};
use virtual_cables::{Config, Device, LogFn, Mode};

use crate::{AMBER, BLUE, DIM, FAINT, GREEN, LINE, PANEL, RED, TEXT, TRACK};

const LISTEN_ADDRESS: &str = "127.0.0.1:3240";
const MAX_LOG_LINES: usize = 600;

#[derive(Clone, Debug)]
pub struct Status {
    pub running: bool,
    pub attached: bool,
    pub helper: bool,
    pub message: String,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            running: false,
            attached: false,
            helper: false,
            message: "stopped".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagAction {
    Save,
    Copy,
    OpenFolder,
}

pub struct VirtualCables {
    tx: Sender<Command>,
    status: Arc<Mutex<Status>>,
    log: Arc<Mutex<VecDeque<String>>>,
    /// The live USB devices while the server runs; B1/B2 bind cables 1 and 2
    /// directly so their mix reaches the cable capture endpoints.
    devices: Arc<Mutex<Vec<Arc<Device>>>>,
    cfg: Config,
    /// Set by the DIAGNOSTICS row; the app performs it, because only the app has
    /// the strip and device state a report needs.
    pub diag_request: Option<DiagAction>,
}

enum Command {
    Start(Config),
    Stop,
    Reattach,
    InstallHelper,
    RemoveHelper,
    Quit,
}

impl VirtualCables {
    pub fn new() -> Self {
        let cfg = Config::import_legacy().unwrap_or_default();
        let status = Arc::new(Mutex::new(Status::default()));
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let devices = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let worker_status = status.clone();
        let worker_log = log.clone();
        let worker_devices = devices.clone();
        std::thread::spawn(move || worker_loop(rx, worker_status, worker_log, worker_devices));
        Self {
            tx,
            status,
            log,
            devices,
            cfg,
            diag_request: None,
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// The live cable device with this number, while the USB server runs.
    pub fn device(&self, number: u8) -> Option<Arc<Device>> {
        self.devices
            .lock()
            .ok()
            .and_then(|devices| devices.iter().find(|d| d.number == number).cloned())
    }

    pub fn config(&self) -> Config {
        self.cfg.clone()
    }

    pub fn apply(&mut self, cfg: Config) {
        self.cfg = cfg.clone();
        if cfg.enabled {
            let _ = self.tx.send(Command::Start(cfg));
        } else {
            self.stop();
        }
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn reattach(&self) {
        let _ = self.tx.send(Command::Reattach);
    }

    pub fn install_helper(&self) {
        let _ = self.tx.send(Command::InstallHelper);
    }

    pub fn remove_helper(&self) {
        let _ = self.tx.send(Command::RemoveHelper);
    }

    fn logs(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    pub fn draw(&mut self, ctx: &egui::Context, open: &mut bool, driver_mode: bool) {
        let mut cfg = self.cfg.clone();
        let status = self.status();
        let frame = egui::Frame::window(&ctx.style())
            .fill(PANEL)
            .stroke(egui::Stroke::new(1.0, LINE))
            .rounding(egui::Rounding::same(6.0))
            .inner_margin(egui::Margin::symmetric(14.0, 10.0));
        egui::Window::new(
            egui::RichText::new("VIRTUAL CABLES")
                .size(12.0)
                .strong()
                .color(AMBER),
        )
        .open(open)
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .min_width(560.0)
        .max_width(680.0)
        .frame(frame)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(536.0);
            if driver_mode {
                ui.label(
                    egui::RichText::new(
                        "KERNEL DRIVER ACTIVE: Virtual Cable 01..04 come from the installed \
                         audio driver. The USB/IP set is named USB Cable 01..04 and can run \
                         at the same time.",
                    )
                    .size(9.0)
                    .color(GREEN),
                );
            } else {
                ui.label(
                    egui::RichText::new(
                        "USB audio loopback devices that Windows can record from and play to.",
                    )
                    .size(9.0)
                    .color(DIM),
                );
            }
            ui.add_space(6.0);

            let mut enabled_changed = false;
            egui::Grid::new("vc_settings")
                .num_columns(2)
                .spacing([14.0, 6.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Enabled").color(TEXT));
                    enabled_changed = ui.checkbox(&mut cfg.enabled, "").changed();
                    ui.end_row();

                    ui.label(egui::RichText::new("Cables").color(TEXT));
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(cfg.cables > 1, egui::Button::new("-"))
                            .clicked()
                        {
                            cfg.cables -= 1;
                        }
                        ui.add(egui::DragValue::new(&mut cfg.cables).range(1..=32));
                        if ui
                            .add_enabled(cfg.cables < 32, egui::Button::new("+"))
                            .clicked()
                        {
                            cfg.cables += 1;
                        }
                    });
                    ui.end_row();

                    ui.label(egui::RichText::new("Buffer").color(TEXT));
                    egui::ComboBox::from_id_source("vc_buffer")
                        .width(130.0)
                        .selected_text(format!("{} ms", cfg.buffer_ms))
                        .show_ui(ui, |ui| {
                            for ms in [20u32, 30, 40, 60, 100, 150, 250] {
                                ui.selectable_value(
                                    &mut cfg.buffer_ms,
                                    ms,
                                    format!("{ms} ms"),
                                );
                            }
                        });
                    ui.end_row();

                    ui.label(egui::RichText::new("Min latency").color(TEXT));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut cfg.min_latency_ms)
                                .range(5..=40)
                                .suffix(" ms"),
                        );
                        ui.label(
                            egui::RichText::new("lower = less delay, more dropouts")
                                .size(9.0)
                                .color(FAINT),
                        );
                    });
                    ui.end_row();

                    ui.label(egui::RichText::new("Max rate").color(TEXT));
                    egui::ComboBox::from_id_source("vc_max_rate")
                        .width(130.0)
                        .selected_text(format!("{} Hz", cfg.max_rate))
                        .show_ui(ui, |ui| {
                            for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
                                ui.selectable_value(
                                    &mut cfg.max_rate,
                                    rate,
                                    format!("{rate} Hz"),
                                );
                            }
                        });
                    ui.end_row();

                    ui.label(egui::RichText::new("Class").color(TEXT));
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut cfg.uac_mode, Mode::Uac2, "UAC2 (high speed)");
                        ui.selectable_value(&mut cfg.uac_mode, Mode::Uac1, "UAC1 (48k fallback)");
                    });
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Apply / Restart").clicked() {
                    self.apply(cfg.clone());
                }
                if ui.button("Stop").clicked() {
                    self.stop();
                }
                if ui.button("Reattach").clicked() {
                    self.reattach();
                }
            });
            if enabled_changed {
                self.apply(cfg.clone());
            }
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "After changing Cables, Buffer, Max rate or Class, click Apply / Restart \
                     (and restart the audio app using the cable). Lower Buffer means less \
                     delay; raise it if the audio drops out.",
                )
                .size(9.0)
                .color(FAINT),
            );

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("HELPER").size(9.0).strong().color(FAINT));
                if status.helper {
                    ui.label(
                        egui::RichText::new("running (no UAC prompts)")
                            .color(GREEN)
                            .size(10.0),
                    );
                    if ui.button("Remove").clicked() {
                        self.remove_helper();
                    }
                } else {
                    let missing = !virtual_cables::win::helper_task_installed();
                    ui.label(
                        egui::RichText::new(if missing {
                            "not installed"
                        } else {
                            "not running"
                        })
                        .color(DIM)
                        .size(10.0),
                    );
                    if ui.button("Install (one-time admin)").clicked() {
                        self.install_helper();
                    }
                }
            });
            ui.label(
                egui::RichText::new(
                    "Cables need the administrator helper. Press Install once; afterwards the                      mixer attaches with no UAC prompt.",
                )
                .size(9.0)
                .color(FAINT),
            );

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("DIAGNOSTICS")
                        .size(9.0)
                        .strong()
                        .color(FAINT),
                );
                if ui
                    .button("Save report")
                    .on_hover_text(
                        "Write one text file with the build, devices, plug-ins and the last logs",
                    )
                    .clicked()
                {
                    self.diag_request = Some(DiagAction::Save);
                }
                if ui
                    .button("Copy")
                    .on_hover_text("Copy the same text to the clipboard")
                    .clicked()
                {
                    self.diag_request = Some(DiagAction::Copy);
                }
                if ui
                    .button("Open folder")
                    .on_hover_text("Show the folder the logs are written to")
                    .clicked()
                {
                    self.diag_request = Some(DiagAction::OpenFolder);
                }
            });
            ui.label(
                egui::RichText::new(
                    "A support report lists device, application and plug-in names and the \
                     last few hundred log lines. It contains no audio.",
                )
                .size(9.0)
                .color(FAINT),
            );

            ui.add_space(6.0);
            ui.separator();
            let (state_color, state_text) = if status.running && status.attached {
                (GREEN, "ATTACHED")
            } else if status.running {
                (AMBER, "RUNNING")
            } else {
                (FAINT, "STOPPED")
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("STATUS").size(9.0).strong().color(FAINT));
                ui.label(egui::RichText::new("\u{25cf}").color(state_color));
                ui.label(
                    egui::RichText::new(state_text)
                        .size(9.0)
                        .strong()
                        .color(state_color),
                );
                ui.label(
                    egui::RichText::new(truncate(&status.message, 80))
                        .size(10.0)
                        .color(TEXT),
                );
            });
            ui.horizontal(|ui| {
                let found = virtual_cables::win::usbip_available();
                ui.label(
                    egui::RichText::new(if found {
                        "usbip.exe: found"
                    } else {
                        "usbip.exe: missing"
                    })
                    .size(9.0)
                    .color(if found { GREEN } else { RED }),
                );
                ui.hyperlink_to(
                    egui::RichText::new("usbip-win2 driver releases")
                        .size(9.0)
                        .color(BLUE),
                    "https://github.com/vadimgrn/usbip-win2/releases",
                );
            });

            ui.add_space(6.0);
            ui.label(egui::RichText::new("LOG").size(9.0).strong().color(FAINT));
            egui::Frame::none()
                .fill(TRACK)
                .stroke(egui::Stroke::new(1.0, LINE))
                .rounding(egui::Rounding::same(4.0))
                .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            for line in self.logs() {
                                ui.label(
                                    egui::RichText::new(truncate(&line, 110))
                                        .monospace()
                                        .size(9.0)
                                        .color(DIM),
                                );
                            }
                        });
                });
        });
        self.cfg = cfg;
    }
}

impl Drop for VirtualCables {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
    }
}

struct Worker {
    runtime: Option<tokio::runtime::Runtime>,
    handle: Option<ServerHandle>,
    broker: Option<BrokerClient>,
    cfg: Config,
}

impl Default for Worker {
    fn default() -> Self {
        Self {
            runtime: None,
            handle: None,
            broker: None,
            cfg: Config::default(),
        }
    }
}

fn worker_loop(
    rx: Receiver<Command>,
    status: Arc<Mutex<Status>>,
    log: Arc<Mutex<VecDeque<String>>>,
    devices: Arc<Mutex<Vec<Arc<Device>>>>,
) {
    let mut worker = Worker::default();
    while let Ok(command) = rx.recv() {
        match command {
            Command::Start(cfg) => worker.start(cfg, &status, &log, &devices),
            Command::Stop => worker.stop(&status, &devices),
            Command::Reattach => worker.attach(&status, &log, true),
            Command::InstallHelper => worker.install_helper(&status, &log),
            Command::RemoveHelper => worker.remove_helper(&status, &log),
            Command::Quit => break,
        }
    }
    worker.stop(&status, &devices);
    if let Some(mut broker) = worker.broker.take() {
        broker.shutdown();
    }
}

impl Worker {
    fn start(
        &mut self,
        cfg: Config,
        status: &Arc<Mutex<Status>>,
        log: &Arc<Mutex<VecDeque<String>>>,
        devices: &Arc<Mutex<Vec<Arc<Device>>>>,
    ) {
        self.stop(status, devices);
        self.cfg = cfg.normalized();
        let log_fn = make_log(log);

        let mut cable_devices = Vec::with_capacity(self.cfg.cables as usize);
        for number in 1..=self.cfg.cables {
            match Device::new(
                number,
                self.cfg.buffer_ms,
                self.cfg.uac_mode,
                self.cfg.max_rate,
            ) {
                Ok(device) => cable_devices.push(Arc::new(device)),
                Err(err) => {
                    set_status(
                        status,
                        false,
                        false,
                        format!("Could not create cable {number}: {err}"),
                    );
                    return;
                }
            }
        }
        if let Ok(mut live) = devices.lock() {
            *live = cable_devices.clone();
        }

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                set_status(status, false, false, format!("Could not start runtime: {err}"));
                return;
            }
        };
        let handle = runtime.block_on(async {
            let mut handle = spawn_server(LISTEN_ADDRESS.to_string(), cable_devices, log_fn);
            let _ = (&mut handle.ready).await;
            handle
        });
        self.runtime = Some(runtime);
        self.handle = Some(handle);
        set_status(
            status,
            true,
            false,
            format!("listening; attaching {} cable(s)...", self.cfg.cables),
        );
        self.attach(status, log, false);
    }

    fn attach(&mut self, status: &Arc<Mutex<Status>>, log: &Arc<Mutex<VecDeque<String>>>, clean: bool) {
        if self.handle.is_none() {
            set_status(status, false, false, "not running".to_string());
            return;
        }
        let log_fn = make_log(log);
        if self.broker.is_none() {
            let mut connected = None;
            for attempt in 0..5 {
                if let Some(broker) = virtual_cables::win::connect_helper(&log_fn) {
                    connected = Some(broker);
                    break;
                }
                if attempt < 4 {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            // The helper the mixer installed once is a scheduled task, so the
            // mixer can start it itself - starting a task you own needs no
            // elevation. Only an absent task or one that refuses to start falls
            // back to the UAC prompt.
            if connected.is_none() && virtual_cables::win::helper_task_installed() {
                if virtual_cables::win::start_helper_task().is_ok() {
                    log_fn("attach helper task started (no UAC)".to_string());
                    for _ in 0..10 {
                        if let Some(broker) = virtual_cables::win::connect_helper(&log_fn) {
                            connected = Some(broker);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
                if connected.is_none() && virtual_cables::win::restart_helper_task().is_ok() {
                    // The helper answers on its port but rejects the token: it is
                    // still running an older one, so restart it.
                    log_fn("attach helper restarted after a token mismatch".to_string());
                    for _ in 0..10 {
                        if let Some(broker) = virtual_cables::win::connect_helper(&log_fn) {
                            connected = Some(broker);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }

            if let Some(broker) = connected {
                self.broker = Some(broker);
            } else if !virtual_cables::win::helper_task_installed() {
                // Never install silently: installing needs the administrator
                // password prompt, which belongs to the person's decision.
                let message = "Cables need the administrator helper - open CABLES and press Install (one-time admin).".to_string();
                log_fn(message.clone());
                set_status(status, self.handle.is_some(), false, message);
                return;
            } else {
                match start_broker(&log_fn) {
                    Ok(broker) => self.broker = Some(broker),
                    Err(err) => {
                        set_status(
                            status,
                            true,
                            false,
                            format!("administrator broker failed: {err}"),
                        );
                        return;
                    }
                }
            }
        }
        // A helper copied from an older mixer build keeps the old binary alive;
        // refresh it so it never blocks replacing the mixer.
        let stale_copy = virtual_cables::win::helper_upgrade_needed()
            && virtual_cables::win::helper_exe_path()
                .map(|path| path.exists())
                .unwrap_or(false);
        if stale_copy {
            // The copy is in the user's own folder and the task is theirs, so an
            // update can refresh it without the administrator prompt.
            if virtual_cables::win::refresh_helper_copy().is_ok() {
                make_log(log)("attach helper updated to this build (no UAC)".to_string());
                for _ in 0..10 {
                    if let Some(broker) = virtual_cables::win::connect_helper(&make_log(log)) {
                        self.broker = Some(broker);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
        let helper = virtual_cables::win::helper_running();
        status.lock().unwrap().helper = helper;
        if let Some(broker) = self.broker.as_mut() {
            match if clean { broker.reset(self.cfg.cables) } else { broker.sync(self.cfg.cables) } {
                Ok(()) => set_status(
                    status,
                    true,
                    true,
                    format!(
                        "{} cable(s) attached{}",
                        self.cfg.cables,
                        if helper { " (background helper)" } else { "" }
                    ),
                ),
                Err(err) => {
                    self.broker = None;
                    set_status(status, true, false, format!("attach failed: {err}"));
                }
            }
        }
    }

    fn install_helper(
        &mut self,
        status: &Arc<Mutex<Status>>,
        log: &Arc<Mutex<VecDeque<String>>>,
    ) {
        let log_fn = make_log(log);
        match virtual_cables::win::install_helper(&log_fn) {
            Ok(()) => {
                set_status(
                    status,
                    self.handle.is_some(),
                    false,
                    "Installing helper; approve the one-time UAC prompt...".to_string(),
                );
                let mut connected = None;
                for _ in 0..60 {
                    if let Some(broker) = virtual_cables::win::connect_helper(&log_fn) {
                        connected = Some(broker);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                if let Some(broker) = connected {
                    self.broker = Some(broker);
                    status.lock().unwrap().helper = true;
                    if self.handle.is_some() {
                        self.attach(status, log, false);
                    } else {
                        set_status(
                            status,
                            false,
                            false,
                            "Helper running; start the mixer cables to attach".to_string(),
                        );
                    }
                } else {
                    set_status(
                        status,
                        self.handle.is_some(),
                        false,
                        "Helper installed, but it did not start yet".to_string(),
                    );
                }
            }
            Err(err) => set_status(
                status,
                self.handle.is_some(),
                false,
                format!("helper install failed: {err}"),
            ),
        }
    }

    fn remove_helper(
        &mut self,
        status: &Arc<Mutex<Status>>,
        log: &Arc<Mutex<VecDeque<String>>>,
    ) {
        let log_fn = make_log(log);
        match virtual_cables::win::remove_helper(&log_fn) {
            Ok(()) => {
                if let Some(mut broker) = self.broker.take() {
                    broker.shutdown();
                }
                status.lock().unwrap().helper = false;
                set_status(
                    status,
                    self.handle.is_some(),
                    false,
                    "Background helper task removed".to_string(),
                );
            }
            Err(err) => set_status(
                status,
                self.handle.is_some(),
                false,
                format!("helper removal failed: {err}"),
            ),
        }
    }

    fn stop(&mut self, status: &Arc<Mutex<Status>>, devices: &Arc<Mutex<Vec<Arc<Device>>>>) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown.cancel();
            if let Some(runtime) = self.runtime.take() {
                let _ = runtime.block_on(async {
                    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
                });
            }
        } else {
            self.runtime = None;
        }
        if let Some(mut broker) = self.broker.take() {
            broker.shutdown();
        }
        if let Ok(mut live) = devices.lock() {
            live.clear();
        }
        let helper = virtual_cables::win::helper_running();
        status.lock().unwrap().helper = helper;
        set_status(status, false, false, "stopped".to_string());
    }
}

fn set_status(
    status: &Arc<Mutex<Status>>,
    running: bool,
    attached: bool,
    message: String,
) {
    let mut current = status.lock().unwrap();
    current.running = running;
    current.attached = attached;
    current.message = message;
}

fn make_log(log: &Arc<Mutex<VecDeque<String>>>) -> LogFn {
    let shared = log.clone();
    Arc::new(move |line| {
        let mut buffer = shared.lock().unwrap();
        buffer.push_back(line);
        while buffer.len() > MAX_LOG_LINES {
            buffer.pop_front();
        }
    })
}

/// Shorten a line for display so long status/log text cannot resize windows.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push(char::from_u32(0x2026).unwrap_or('.'));
    out
}
