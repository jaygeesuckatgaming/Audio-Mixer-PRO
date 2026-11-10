//! Windows audio sessions on a virtual cable, for the strip's app list.
//!
//! A "Virtual Cable NN" is a full-duplex USB audio device. Applications render
//! into "Speakers (Virtual Cable NN)" and the cable server loops that stream
//! into "Microphone (Virtual Cable NN)", which is what a strip captures. This
//! module lists the applications behind that render endpoint and exposes the
//! per-app controls Windows actually offers: mute, volume and peak.
//!
//! Everything runs on one worker thread that owns its own COM apartment, so the
//! audio callback never touches a lock here. The UI only reads a snapshot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(700);
/// Let the console finish starting (and the cables attach) before the first
/// WASAPI session call.
const FIRST_SCAN_DELAY: Duration = Duration::from_millis(1500);
/// A scan that takes longer than this means the audio service is struggling;
/// stop hammering it.
const SLOW_SCAN_BACKOFF: Duration = Duration::from_secs(8);
const SLOW_SCAN: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug)]
pub struct AppIcon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct AppSession {
    /// One row per process; `sessions` counts the merged sessions.
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub icon: Option<Arc<AppIcon>>,
    pub muted: bool,
    pub volume: f32,
    pub peak: f32,
    pub sessions: usize,
}

enum Command {
    Targets(Vec<String>),
    Mutate {
        target: String,
        pid: u32,
        muted: Option<bool>,
        volume: Option<f32>,
    },
}

pub struct AppSessions {
    tx: Option<Sender<Command>>,
    targets: Mutex<Vec<String>>,
    rows: Arc<Mutex<HashMap<String, Vec<AppSession>>>>,
}

impl AppSessions {
    pub fn new() -> Self {
        // Escape hatch: if audio ever misbehaves, `AMP_NO_APPS=1` stops the
        // scanner from touching WASAPI at all without losing the rest of the UI.
        if std::env::var_os("AMP_NO_APPS").is_some() {
            return Self {
                tx: None,
                targets: Mutex::new(Vec::new()),
                rows: Arc::new(Mutex::new(HashMap::new())),
            };
        }
        #[cfg(windows)]
        {
            let (tx, rx) = channel();
            let rows = Arc::new(Mutex::new(HashMap::new()));
            let inner = rows.clone();
            std::thread::Builder::new()
                .name("audio-sessions".to_string())
                .spawn(move || worker(rx, inner))
                .ok();
            Self {
                tx: Some(tx),
                targets: Mutex::new(Vec::new()),
                rows,
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                tx: None,
                targets: Mutex::new(Vec::new()),
                rows: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    /// Cable labels the strips currently watch, e.g. "Virtual Cable 03".
    pub fn set_targets(&self, next: &[String]) {
        let mut current = match self.targets.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if current.as_slice() == next {
            return;
        }
        *current = next.to_vec();
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::Targets(next.to_vec()));
        }
    }

    pub fn set_mute(&self, target: &str, pid: u32, muted: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::Mutate {
                target: target.to_string(),
                pid,
                muted: Some(muted),
                volume: None,
            });
        }
    }

    pub fn set_volume(&self, target: &str, pid: u32, volume: f32) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::Mutate {
                target: target.to_string(),
                pid,
                muted: None,
                volume: Some(volume.clamp(0.0, 1.0)),
            });
        }
    }

    pub fn snapshot(&self) -> HashMap<String, Vec<AppSession>> {
        self.rows
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Default for AppSessions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
fn worker(rx: Receiver<Command>, rows: Arc<Mutex<HashMap<String, Vec<AppSession>>>>) {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut targets: Vec<String> = Vec::new();
    let mut pending: Vec<(String, u32, Option<bool>, Option<f32>)> = Vec::new();
    let mut names: HashMap<u32, Option<ProcessInfo>> = HashMap::new();
    let mut icons: HashMap<PathBuf, Option<Arc<AppIcon>>> = HashMap::new();
    let mut next_poll = Instant::now();

    loop {
        let mut rescan = false;
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(Command::Targets(next)) => {
                if next != targets {
                    targets = next;
                    names.clear();
                    rescan = false;
                    next_poll = Instant::now() + FIRST_SCAN_DELAY;
                }
            }
            Ok(Command::Mutate {
                target,
                pid,
                muted,
                volume,
            }) => {
                pending.push((target, pid, muted, volume));
                rescan = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Coalesce a burst of slider commands into one pass.
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::Targets(next) => {
                    if next != targets {
                        targets = next;
                        names.clear();
                        rescan = false;
                        next_poll = Instant::now() + FIRST_SCAN_DELAY;
                    }
                }
                Command::Mutate {
                    target,
                    pid,
                    muted,
                    volume,
                } => {
                    if let Some(existing) = pending
                        .iter_mut()
                        .find(|(t, p, _, _)| t == &target && *p == pid)
                    {
                        if muted.is_some() {
                            existing.2 = muted;
                        }
                        if volume.is_some() {
                            existing.3 = volume;
                        }
                    } else {
                        pending.push((target, pid, muted, volume));
                    }
                    rescan = true;
                }
            }
        }

        if targets.is_empty() {
            if let Ok(mut guard) = rows.lock() {
                if !guard.is_empty() {
                    guard.clear();
                }
            }
            pending.clear();
            continue;
        }

        if rescan || Instant::now() >= next_poll {
            let started = Instant::now();
            let scan = unsafe { scan(&targets, &mut pending, &mut names, &mut icons) };
            if let Ok(mut guard) = rows.lock() {
                *guard = scan;
            }
            pending.clear();
            let took = started.elapsed();
            next_poll = Instant::now()
                + if took > SLOW_SCAN {
                    SLOW_SCAN_BACKOFF
                } else {
                    POLL_INTERVAL
                };
        }
    }

    unsafe { CoUninitialize() };
}

#[cfg(windows)]
#[derive(Clone)]
struct ProcessInfo {
    name: String,
    exe: Option<PathBuf>,
}

#[cfg(windows)]
unsafe fn scan(
    targets: &[String],
    pending: &mut [(String, u32, Option<bool>, Option<f32>)],
    names: &mut HashMap<u32, Option<ProcessInfo>>,
    icons: &mut HashMap<PathBuf, Option<Arc<AppIcon>>>,
) -> HashMap<String, Vec<AppSession>> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    use windows::Win32::Media::Audio::{
        eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
        IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    let mut out = HashMap::new();
    let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(
        &MMDeviceEnumerator,
        None::<&windows::core::IUnknown>,
        CLSCTX_ALL,
    ) else {
        return out;
    };
    let Ok(list) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
        return out;
    };
    let count = list.GetCount().unwrap_or(0);
    let mut devices: Vec<(String, IMMDevice)> = Vec::new();
    for i in 0..count {
        if let Ok(device) = list.Item(i) {
            if let Some(name) = friendly_name(&device) {
                devices.push((name, device));
            }
        }
    }

    for target in targets {
        let Some((_, device)) = devices
            .iter()
            .find(|(name, _)| endpoint_matches(target, name))
        else {
            continue;
        };
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            continue;
        };
        let Ok(sessions) = manager.GetSessionEnumerator() else {
            continue;
        };
        let count = sessions.GetCount().unwrap_or(0);
        let mut rows: HashMap<u32, AppSession> = HashMap::new();
        for i in 0..count {
            let Ok(control) = sessions.GetSession(i as i32) else {
                continue;
            };
            if control
                .GetState()
                .map(|state| state != AudioSessionStateActive)
                .unwrap_or(true)
            {
                continue;
            }
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let pid = control2.GetProcessId().unwrap_or(0);
            if pid == 0 || pid == std::process::id() {
                continue;
            }

            // Apply a pending slider/mute command before reading the state
            // back, so the row reflects what was just asked for.
            for (mutated_target, mutated_pid, muted, volume) in pending.iter_mut() {
                if mutated_target != target || *mutated_pid != pid {
                    continue;
                }
                if let Ok(volume_iface) = control.cast::<ISimpleAudioVolume>() {
                    if let Some(muted) = muted.take() {
                        let _ = volume_iface.SetMute(
                            windows::Win32::Foundation::BOOL::from(muted),
                            std::ptr::null(),
                        );
                    }
                    if let Some(volume) = volume.take() {
                        let _ = volume_iface.SetMasterVolume(volume, std::ptr::null());
                    }
                }
            }

            let (muted, volume) = control
                .cast::<ISimpleAudioVolume>()
                .map(|volume_iface| {
                    let muted = volume_iface
                        .GetMute()
                        .map(|value| value.as_bool())
                        .unwrap_or(false);
                    let volume = volume_iface.GetMasterVolume().unwrap_or(1.0);
                    (muted, volume)
                })
                .unwrap_or((false, 1.0));
            let peak = control
                .cast::<IAudioMeterInformation>()
                .and_then(|meter| meter.GetPeakValue())
                .unwrap_or(0.0);

            let info = names
                .entry(pid)
                .or_insert_with(|| process_info(pid))
                .clone();
            let (name, exe) = match info {
                Some(info) => (info.name, info.exe),
                None => (format!("PID {pid}"), None),
            };
            let icon = exe.as_ref().map(|path| {
                icons
                    .entry(path.clone())
                    .or_insert_with(|| icon_for(path))
                    .clone()
            });
            let icon = icon.flatten();

            let entry = rows.entry(pid).or_insert_with(|| AppSession {
                pid,
                name: name.clone(),
                exe: exe.clone(),
                icon: icon.clone(),
                muted,
                volume,
                peak,
                sessions: 0,
            });
            entry.sessions += 1;
            entry.peak = entry.peak.max(peak);
            entry.volume = entry.volume.max(volume);
            entry.muted = entry.muted && muted;
        }
        let mut list: Vec<AppSession> = rows.into_values().collect();
        list.sort_by(|a, b| {
            b.peak
                .partial_cmp(&a.peak)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out.insert(target.clone(), list);
    }
    out
}

#[cfg(windows)]
unsafe fn friendly_name(device: &windows::Win32::Media::Audio::IMMDevice) -> Option<String> {
    use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
    use windows::Win32::System::Com::STGM_READ;
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    // PKEY_Device_FriendlyName, spelled out so the FunctionDiscovery feature is
    // not needed just for one constant.
    const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };

    let store = device.OpenPropertyStore(STGM_READ).ok()?;
    let mut value = store.GetValue(&FRIENDLY_NAME).ok()?;
    let name = value.to_string();
    let _ = PropVariantClear(&mut value);
    Some(name)
}

/// Only the render side of a cable carries app sessions; "Speakers (...)" is
/// the endpoint applications are told to play to.
#[cfg(windows)]
fn endpoint_matches(label: &str, friendly: &str) -> bool {
    let friendly = friendly.to_ascii_lowercase();
    friendly.contains(&label.to_ascii_lowercase()) && friendly.starts_with("speakers")
}

#[cfg(windows)]
unsafe fn process_info(pid: u32) -> Option<ProcessInfo> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = OpenProcess(
        PROCESS_QUERY_LIMITED_INFORMATION,
        windows::Win32::Foundation::BOOL::from(false),
        pid,
    )
    .ok()?;
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    let ok = QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        PWSTR(buffer.as_mut_ptr()),
        &mut length,
    )
    .is_ok();
    let _ = CloseHandle(handle);
    if !ok {
        return None;
    }
    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("PID {pid}"));
    Some(ProcessInfo {
        name,
        exe: Some(path),
    })
}

#[cfg(windows)]
unsafe fn icon_for(path: &PathBuf) -> Option<Arc<AppIcon>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON};
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut info = SHFILEINFOW::default();
    let ok = SHGetFileInfoW(
        PCWSTR(wide.as_ptr()),
        FILE_ATTRIBUTE_NORMAL,
        Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_SMALLICON,
    );
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let icon = hicon_to_rgba(info.hIcon);
    let _ = DestroyIcon(info.hIcon);
    icon.map(Arc::new)
}

#[cfg(windows)]
unsafe fn hicon_to_rgba(hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<AppIcon> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut info = ICONINFO::default();
    GetIconInfo(hicon, &mut info).ok()?;
    let color: HBITMAP = info.hbmColor;
    let mask: HBITMAP = info.hbmMask;
    let mut bitmap = BITMAP::default();
    let got = GetObjectW(
        HGDIOBJ(color.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bitmap as *mut _ as *mut std::ffi::c_void),
    );
    if got == 0 || bitmap.bmWidth <= 0 || bitmap.bmHeight <= 0 {
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        return None;
    }
    let width = bitmap.bmWidth as u32;
    let height = bitmap.bmHeight as u32;

    let dc = CreateCompatibleDC(HDC(0));
    let mut header = BITMAPINFO::default();
    header.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0,
        ..Default::default()
    };
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let lines = GetDIBits(
        dc,
        color,
        0,
        height,
        Some(pixels.as_mut_ptr() as *mut std::ffi::c_void),
        &mut header,
        DIB_RGB_COLORS,
    );
    let _ = DeleteDC(dc);
    let _ = DeleteObject(HGDIOBJ(color.0));
    let _ = DeleteObject(HGDIOBJ(mask.0));
    if lines == 0 {
        return None;
    }

    // GDI hands back BGRA; some 16x16 icons come with an empty alpha channel
    // and are meant to be fully opaque.
    let mut any_alpha = false;
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        any_alpha |= pixel[3] != 0;
    }
    if !any_alpha {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }
    Some(AppIcon {
        width,
        height,
        rgba: pixels,
    })
}

/// Diagnostic used by `--apps-selftest`: every cable endpoint, and the sessions
/// Windows reports on it right now.
#[cfg(windows)]
pub fn self_test() -> String {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{eRender, ISimpleAudioVolume, DEVICE_STATE_ACTIVE};
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    let mut report = String::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(
            &MMDeviceEnumerator,
            None::<&windows::core::IUnknown>,
            CLSCTX_ALL,
        ) else {
            report.push_str("could not create the device enumerator\n");
            CoUninitialize();
            return report;
        };
        let Ok(list) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            report.push_str("could not enumerate render endpoints\n");
            CoUninitialize();
            return report;
        };
        let count = list.GetCount().unwrap_or(0);
        report.push_str(&format!("render endpoints: {count}\n"));
        for i in 0..count {
            let Ok(device) = list.Item(i) else { continue };
            let Some(name) = friendly_name(&device) else {
                continue;
            };
            if !name.to_ascii_lowercase().contains("virtual cable") {
                continue;
            }
            report.push_str(&format!("\n{name}\n"));
            let Ok(manager) = device
                .Activate::<windows::Win32::Media::Audio::IAudioSessionManager2>(CLSCTX_ALL, None)
            else {
                report.push_str("  no session manager\n");
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                report.push_str("  no session enumerator\n");
                continue;
            };
            let session_count = sessions.GetCount().unwrap_or(0);
            report.push_str(&format!("  sessions: {session_count}\n"));
            for index in 0..session_count {
                let Ok(control) = sessions.GetSession(index as i32) else {
                    continue;
                };
                let state = control.GetState().map(|s| s.0).unwrap_or(-1);
                let pid = control
                    .cast::<windows::Win32::Media::Audio::IAudioSessionControl2>()
                    .and_then(|c| c.GetProcessId())
                    .unwrap_or(0);
                let (muted, volume) = control
                    .cast::<ISimpleAudioVolume>()
                    .map(|v| {
                        (
                            v.GetMute().map(|m| m.as_bool()).unwrap_or(false),
                            v.GetMasterVolume().unwrap_or(-1.0),
                        )
                    })
                    .unwrap_or((false, -1.0));
                let name = names_for(pid);
                let icon = process_info(pid)
                    .and_then(|info| info.exe)
                    .and_then(|exe| icon_for(&exe))
                    .map(|icon| format!("icon {}x{}", icon.width, icon.height))
                    .unwrap_or_else(|| "no icon".to_string());
                report.push_str(&format!(
                    "    pid {pid:<6} state {state}  mute {muted}  vol {volume:.2}  {name}  {icon}\n"
                ));
            }
        }
        CoUninitialize();
    }
    if !report.contains('\n') {
        report.push_str("no virtual cable endpoints\n");
    }
    report
}

#[cfg(windows)]
fn names_for(pid: u32) -> String {
    match unsafe { process_info(pid) } {
        Some(info) => format!("{} [{}]", info.name, pid),
        None => format!("pid {pid}"),
    }
}

#[cfg(not(windows))]
pub fn self_test() -> String {
    "audio sessions are Windows-only\n".to_string()
}
