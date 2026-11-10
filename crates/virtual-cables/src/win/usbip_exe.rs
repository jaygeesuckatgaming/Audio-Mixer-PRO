use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use crate::LogFn;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn find_usbip() -> Option<PathBuf> {
    if let Ok(path) = which_usbip() {
        return Some(path);
    }
    let mut candidates = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let base = PathBuf::from(program_files);
        candidates.push(base.join("USBip").join("usbip.exe"));
        candidates.push(base.join("usbip-win2").join("usbip.exe"));
        candidates.push(base.join("usbip").join("usbip.exe"));
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        let base = PathBuf::from(program_files_x86);
        candidates.push(base.join("USBip").join("usbip.exe"));
        candidates.push(base.join("usbip-win2").join("usbip.exe"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

pub fn usbip_available() -> bool {
    find_usbip().is_some()
}

fn which_usbip() -> Result<PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("usbip.exe");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

fn run(executable: &PathBuf, log: &LogFn, arguments: &[&str]) -> Result<String, String> {
    log(format!("usbip.exe {}", arguments.join(" ")));
    let output = Command::new(executable)
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|err| format!("usbip {}: {err}", arguments.join(" ")))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let error_text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let combined = if error_text.is_empty() {
        text.clone()
    } else if text.is_empty() {
        error_text.clone()
    } else {
        format!("{text} | {error_text}")
    };
    if !combined.is_empty() {
        log(format!(
            "usbip: {}",
            combined.replace("\r\n", " | ").replace('\n', " | ")
        ));
    }
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "usbip {} failed ({}): {}",
            arguments.join(" "),
            output.status,
            combined
        ))
    }
}

fn own_ports(output: &str) -> Vec<String> {
    let mut ports = Vec::new();
    let mut current: Option<String> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Port ") {
            if let Some(colon) = rest.find(':') {
                current = Some(rest[..colon].trim().to_string());
            }
            continue;
        }
        if let Some(port) = &current {
            if trimmed.contains("usbip://127.0.0.1:3240/") {
                ports.push(port.clone());
                current = None;
            }
        }
    }
    ports
}

pub fn detach_own_ports(executable: &PathBuf, log: &LogFn) {
    let output = match run(executable, log, &["port"]) {
        Ok(output) => output,
        Err(err) => {
            log(format!("Could not inspect existing USB/IP ports: {err}"));
            return;
        }
    };
    for port in own_ports(&output) {
        if let Err(err) = run(executable, log, &["detach", "-p", &port]) {
            log(format!(
                "Could not detach previous Virtual Cables port {port}: {err}"
            ));
        }
    }
}

pub fn run_attach_helper(count: u8, log: &LogFn) -> Result<(), String> {
    if !(1..=32).contains(&count) {
        return Err(format!("invalid cable count {count}"));
    }
    let executable = find_usbip()
        .ok_or_else(|| "usbip.exe was not found; install the usbip-win2 driver".to_string())?;
    log(format!(
        "Elevated Windows device synchronization started for {count} cable(s)"
    ));
    log(format!("usbip.exe: {}", executable.display()));

    let mut last_error = String::new();
    let mut ready = false;
    for _ in 0..20 {
        match run(&executable, log, &["list", "-r", "127.0.0.1"]) {
            Ok(_) => {
                ready = true;
                break;
            }
            Err(err) => {
                last_error = err;
                sleep(Duration::from_millis(150));
            }
        }
    }
    if !ready {
        return Err(format!("local USB/IP server is not ready: {last_error}"));
    }

    detach_own_ports(&executable, log);
    for number in 1..=count {
        let bus_id = format!("1-{number}");
        run(
            &executable,
            log,
            &["attach", "-r", "127.0.0.1", "-b", &bus_id],
        )
        .map_err(|err| format!("attach {bus_id} failed: {err}"))?;
    }
    log(format!(
        "Windows device synchronization completed for {count} cable(s)"
    ));
    Ok(())
}
