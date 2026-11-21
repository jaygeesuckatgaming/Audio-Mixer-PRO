//! One-file support report.
//!
//! A user who has a problem should not have to find `%APPDATA%\Audio Mixer Pro\
//! startup.log`, the helper log and the version file by hand. This assembles the
//! same information into a single text file that can be attached to a message,
//! and it never touches the audio service, so it is safe on the UI thread.

use std::io::Write;
use std::path::{Path, PathBuf};

/// `%APPDATA%\Audio Mixer Pro`
pub fn app_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("Audio Mixer Pro"))
}

pub fn log_path(name: &str) -> Option<PathBuf> {
    Some(app_dir()?.join(name))
}

/// Last `lines` lines of `path`, or a note saying it is missing.
pub fn log_tail(path: &Path, lines: usize) -> String {
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => return format!("(could not read {}: {err})\n", path.display()),
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    let mut out = String::new();
    for line in &all[start..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Header, build identity and the two log tails. Live app state is appended by
/// the caller, which is the only place that has it.
pub fn base_report() -> String {
    let mut text = String::new();
    text.push_str("Audio Mixer PRO - support report\n");
    text.push_str("================================\n");
    text.push_str(&format!("Generated : {}\n", date_time_stamp()));
    text.push_str(&format!(
        "Platform  : {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    text.push_str(&format!("Process   : pid {}\n", std::process::id()));

    match std::env::current_exe() {
        Ok(exe) => {
            text.push_str(&format!("Executable: {}\n", exe.display()));
            if let Ok(meta) = std::fs::metadata(&exe) {
                text.push_str(&format!("Size      : {} bytes\n", meta.len()));
                if let Ok(modified) = meta.modified() {
                    if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                        text.push_str(&format!(
                            "Built     : {} (epoch {}s)\n",
                            date_time_from_epoch(since.as_secs()),
                            since.as_secs()
                        ));
                    }
                }
            }
            if let Some(version) = version_file_for(&exe) {
                text.push_str(&format!(
                    "Version   : {}\n",
                    version.trim().replace('\n', " | ")
                ));
            }
        }
        Err(err) => text.push_str(&format!("Executable: (unknown: {err})\n")),
    }

    let hooks = injected_hook_modules();
    text.push_str(&format!(
        "Injected  : {}\n",
        if hooks.is_empty() {
            "none".to_string()
        } else {
            hooks.join(", ")
        }
    ));

    text.push_str(
        "\nThis file contains device names, application names, plug-in names and the \
         mixer's own logs. It holds no audio and no credentials.\n",
    );

    if let Some(path) = log_path("startup.log") {
        text.push_str(&format!(
            "\n---- {} (last 300 lines) ----\n",
            path.display()
        ));
        text.push_str(&log_tail(&path, 300));
    }
    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        let path = base.join("Virtual Cables").join("attach_helper.log");
        if path.exists() {
            text.push_str(&format!(
                "\n---- {} (last 150 lines) ----\n",
                path.display()
            ));
            text.push_str(&log_tail(&path, 150));
        }
    }
    text
}

/// Write the report next to the logs and return its path.
pub fn write_report(text: &str) -> Result<PathBuf, String> {
    let dir = app_dir().ok_or_else(|| "APPDATA is not set".to_string())?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Could not create {}: {err}", dir.display()))?;
    let stem = format!(
        "AudioMixerPRO-diag-{}",
        date_time_stamp().replace([' ', ':'], "-")
    );
    for attempt in 0..20 {
        let name = if attempt == 0 {
            format!("{stem}.txt")
        } else {
            format!("{stem}-{attempt}.txt")
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .map_err(|err| format!("Could not write {}: {err}", path.display()))?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("Could not create {}: {err}", path.display())),
        }
    }
    Err("Could not find a free report name".to_string())
}

fn version_file_for(exe: &Path) -> Option<String> {
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..3 {
        let candidate = dir.join("VERSION.txt");
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            return Some(text);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// DLLs another program has injected. OBS Game Capture hooks the mixer's OpenGL
/// output and the NVIDIA overlay does the same; both use NVAPI, and the crashes
/// in November were inside `nvapi64` while a Game Capture source was attached.
#[cfg(windows)]
pub fn injected_hook_modules() -> Vec<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
    };

    let mut out = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, std::process::id());
        if snapshot == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: MODULEENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        if Module32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let end = entry
                    .szModule
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szModule.len());
                let name = String::from_utf16_lossy(&entry.szModule[..end]);
                let lower = name.to_ascii_lowercase();
                if lower.contains("graphics-hook")
                    || lower.starts_with("obs")
                    || lower == "nvspcap64.dll"
                {
                    out.push(name);
                }
                if Module32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(not(windows))]
pub fn injected_hook_modules() -> Vec<String> {
    Vec::new()
}

/// Rotate a log that has grown past `max` bytes to `<name>.1` (one generation).
pub fn rotate_log(path: &Path, max: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() <= max {
        return false;
    }
    let mut backup = path.as_os_str().to_owned();
    backup.push(".1");
    let backup = PathBuf::from(backup);
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
    true
}

fn date_time_stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    date_time_from_epoch(seconds)
}

/// `YYYY-MM-DD HH:MM` in local time (the Windows time-zone offset is applied).
fn date_time_from_epoch(seconds: u64) -> String {
    let offset = local_offset_seconds();
    let local = seconds as i64 + offset;
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Seconds east of UTC for the current moment, via `GetTimeZoneInformation`.
#[cfg(windows)]
fn local_offset_seconds() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // The generated bindings omit the TIME_ZONE_ID_* constants apart from
    // INVALID; 2 is TIME_ZONE_ID_DAYLIGHT.
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    let mut info: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut info) };
    let mut minutes = info.Bias;
    if id == TIME_ZONE_ID_DAYLIGHT {
        minutes += info.DaylightBias;
    }
    // Bias is minutes west of UTC.
    -(minutes as i64) * 60
}

#[cfg(not(windows))]
fn local_offset_seconds() -> i64 {
    0
}

/// Howard Hinnant's `civil_from_days`, for a dependency-free local date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_match_known_days() {
        // 1970-01-01 is day 0; 2026-11-21 is 20778.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_778), (2026, 11, 21));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn report_has_a_header_and_the_hook_line() {
        let report = base_report();
        assert!(report.contains("Audio Mixer PRO - support report"));
        assert!(report.contains("Injected  :"));
        assert!(report.contains("startup.log") || report.contains("could not read"));
    }
}
