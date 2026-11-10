use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::LogFn;

use super::usbip_exe::{run_attach_helper, usbip_available};

const HELPER_TASK_NAME: &str = "Audio Mixer Pro Virtual Cables";
const HELPER_PORT: u16 = 3241;

pub struct BrokerClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl BrokerClient {
    pub fn sync(&mut self, count: u8) -> Result<(), String> {
        writeln!(self.writer, "SYNC {count}").map_err(|err| err.to_string())?;
        self.writer.flush().map_err(|err| err.to_string())?;
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .map_err(|err| err.to_string())?;
        if line.trim() == "OK" {
            Ok(())
        } else {
            Err(format!(
                "Windows device synchronization failed: {}",
                line.trim()
            ))
        }
    }

    pub fn shutdown(&mut self) {
        let _ = writeln!(self.writer, "QUIT");
        let _ = self.writer.flush();
    }
}

/// Start the elevated attach helper (one UAC prompt) and authenticate it.
pub fn start_broker(log: &LogFn) -> Result<BrokerClient, String> {
    if !usbip_available() {
        return Err("usbip.exe was not found; install the usbip-win2 driver".to_string());
    }
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| err.to_string())?;
    let address = listener.local_addr().map_err(|err| err.to_string())?;
    let token = random_token()?;
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let parameters = format!("--vc-attach-broker {address} {token}");
    shell_execute_runas(&exe, &parameters)?;

    listener
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err("administrator broker did not connect".to_string());
                }
                sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err.to_string()),
        }
    };
    stream
        .set_nonblocking(false)
        .map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|err| err.to_string())?);
    let mut received = String::new();
    reader
        .read_line(&mut received)
        .map_err(|err| err.to_string())?;
    if received.trim() != token {
        return Err("administrator broker authentication failed".to_string());
    }
    stream
        .set_read_timeout(None)
        .map_err(|err| err.to_string())?;
    log("One-time administrator broker connected".to_string());
    Ok(BrokerClient {
        writer: stream,
        reader,
    })
}

/// The elevated side. The mixer binary is re-launched with
/// `--vc-attach-broker <address> <token>` and executes this function.
pub fn run_attach_broker(address: &str, token: &str, log: &LogFn) -> Result<(), String> {
    if !address.starts_with("127.0.0.1:") || token.len() != 64 {
        return Err("invalid broker parameters".to_string());
    }
    let socket: SocketAddr = address
        .parse()
        .map_err(|err| format!("invalid address: {err}"))?;
    let mut stream = TcpStream::connect_timeout(&socket, Duration::from_secs(10))
        .map_err(|err| format!("connect to main application: {err}"))?;
    writeln!(stream, "{token}").map_err(|err| err.to_string())?;
    stream.flush().map_err(|err| err.to_string())?;

    let reader_stream = stream.try_clone().map_err(|err| err.to_string())?;
    let reader = BufReader::new(reader_stream);
    for line in reader.lines() {
        let line = line.map_err(|err| err.to_string())?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 1 && fields[0] == "QUIT" {
            return Ok(());
        }
        if fields.len() != 2 || fields[0] != "SYNC" {
            let _ = writeln!(stream, "ERR invalid request");
            continue;
        }
        let count = match fields[1].parse::<u8>() {
            Ok(count) if (1..=32).contains(&count) => count,
            _ => {
                let _ = writeln!(stream, "ERR invalid cable count");
                continue;
            }
        };
        match run_attach_helper(count, log) {
            Ok(()) => {
                let _ = writeln!(stream, "OK");
            }
            Err(err) => {
                log(format!("Broker synchronization failed: {err}"));
                let _ = writeln!(stream, "ERR synchronization failed");
            }
        }
    }
    Ok(())
}

fn helper_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("Virtual Cables"))
}

fn helper_config_path() -> Option<PathBuf> {
    Some(helper_dir()?.join("attach_helper.txt"))
}

pub fn helper_log_path() -> Option<PathBuf> {
    Some(helper_dir()?.join("attach_helper.log"))
}

struct HelperConfig {
    port: u16,
    token: String,
}

fn load_helper_config() -> Option<HelperConfig> {
    let text = fs::read_to_string(helper_config_path()?).ok()?;
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse().ok()?;
    let token = lines.next()?.trim().to_string();
    if token.len() != 64 {
        return None;
    }
    Some(HelperConfig { port, token })
}

fn save_helper_config(port: u16, token: &str) -> Result<(), String> {
    let dir = helper_dir().ok_or_else(|| "LOCALAPPDATA is not set".to_string())?;
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let path = helper_config_path().ok_or_else(|| "LOCALAPPDATA is not set".to_string())?;
    fs::write(path, format!("{port}\n{token}\n")).map_err(|err| err.to_string())
}

pub fn helper_running() -> bool {
    load_helper_config().is_some_and(|config| {
        TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], config.port)),
            Duration::from_millis(150),
        )
        .is_ok()
    })
}

/// Connect to the persistent background helper (no UAC).
pub fn connect_helper(log: &LogFn) -> Option<BrokerClient> {
    let config = load_helper_config()?;
    let stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], config.port)),
        Duration::from_millis(500),
    )
    .ok()?;
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut writer = stream;
    let mut reader = BufReader::new(writer.try_clone().ok()?);
    writeln!(writer, "{}", config.token).ok()?;
    writer.flush().ok()?;
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if line.trim() != "AUTH OK" {
        return None;
    }
    writer.set_read_timeout(None).ok()?;
    log("Connected to the background attach helper".to_string());
    Some(BrokerClient { writer, reader })
}

/// Elevated long-running helper. Launched by the scheduled task with
/// `--vc-attach-service <port> <token>` and stays available for the mixer.
pub fn run_attach_service(port: u16, token: &str, log: &LogFn) -> Result<(), String> {
    if token.len() != 64 {
        return Err("invalid attach-helper token".to_string());
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|err| format!("could not bind attach helper: {err}"))?;
    log(format!("Attach helper listening on 127.0.0.1:{port}"));
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            Err(err) => {
                log(format!("attach helper accept failed: {err}"));
                continue;
            }
        };
        if let Err(err) = handle_service_client(stream, token, log) {
            log(format!("attach helper client failed: {err}"));
        }
    }
    Ok(())
}

fn handle_service_client(mut stream: TcpStream, token: &str, log: &LogFn) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|err| err.to_string())?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|err| err.to_string())?;
    if line.trim() != token {
        let _ = writeln!(stream, "AUTH ERR");
        return Err("authentication failed".to_string());
    }
    writeln!(stream, "AUTH OK").map_err(|err| err.to_string())?;
    stream.flush().map_err(|err| err.to_string())?;

    for line in reader.lines() {
        let line = line.map_err(|err| err.to_string())?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 1 && fields[0] == "QUIT" {
            return Ok(());
        }
        if fields.len() != 2 || fields[0] != "SYNC" {
            let _ = writeln!(stream, "ERR invalid request");
            continue;
        }
        let count = match fields[1].parse::<u8>() {
            Ok(count) if (1..=32).contains(&count) => count,
            _ => {
                let _ = writeln!(stream, "ERR invalid cable count");
                continue;
            }
        };
        match run_attach_helper(count, log) {
            Ok(()) => {
                let _ = writeln!(stream, "OK");
            }
            Err(err) => {
                log(format!("attach helper synchronization failed: {err}"));
                let _ = writeln!(stream, "ERR synchronization failed");
            }
        }
        let _ = stream.flush();
    }
    Ok(())
}

/// Install the scheduled task that starts the elevated helper at logon and
/// immediately. One UAC prompt for this call only.
pub fn install_helper(log: &LogFn) -> Result<(), String> {
    let token = random_token()?;
    save_helper_config(HELPER_PORT, &token)?;
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let directory = exe.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n\
         $action = New-ScheduledTaskAction -Execute '{exe}' -Argument '--vc-attach-service {port} {token}' -WorkingDirectory '{dir}'\n\
         $principal = New-ScheduledTaskPrincipal -UserId \"$env:USERDOMAIN\\$env:USERNAME\" -LogonType Interactive -RunLevel Highest\n\
         $trigger = New-ScheduledTaskTrigger -AtLogOn\n\
         Register-ScheduledTask -TaskName '{task}' -Action $action -Trigger $trigger -Principal $principal -Force | Out-Null\n\
         Start-ScheduledTask -TaskName '{task}'\n",
        exe = ps_quote(&exe.to_string_lossy()),
        dir = ps_quote(&directory.to_string_lossy()),
        port = HELPER_PORT,
        token = token,
        task = ps_quote(HELPER_TASK_NAME),
    );
    let script_path = helper_dir()
        .ok_or_else(|| "LOCALAPPDATA is not set".to_string())?
        .join("install_helper.ps1");
    fs::write(&script_path, script).map_err(|err| err.to_string())?;
    run_powershell_script(&script_path)?;
    log("Background attach helper installation started (one-time UAC)".to_string());
    Ok(())
}

/// Remove the scheduled task. The running helper exits at logoff.
pub fn remove_helper(log: &LogFn) -> Result<(), String> {
    let script = format!(
        "Unregister-ScheduledTask -TaskName '{}' -Confirm:$false\n",
        ps_quote(HELPER_TASK_NAME)
    );
    let script_path = helper_dir()
        .ok_or_else(|| "LOCALAPPDATA is not set".to_string())?
        .join("remove_helper.ps1");
    fs::write(&script_path, script).map_err(|err| err.to_string())?;
    run_powershell_script(&script_path)?;
    log("Background attach helper scheduled task removed".to_string());
    Ok(())
}

fn run_powershell_script(script_path: &Path) -> Result<(), String> {
    let system_root =
        std::env::var_os("SystemRoot").ok_or_else(|| "SystemRoot is not set".to_string())?;
    let powershell = PathBuf::from(system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let parameters = format!(
        "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\"",
        script_path.display()
    );
    shell_execute_runas(&powershell, &parameters)
}

fn ps_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn random_token() -> Result<String, String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|err| err.to_string())?;
    let mut token = String::with_capacity(64);
    for byte in raw {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

fn shell_execute_runas(exe: &PathBuf, parameters: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(parameters);
    let directory = wide(
        &exe.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_string_lossy(),
    );
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            directory.as_ptr(),
            SW_HIDE,
        )
    };
    if (result as isize) <= 32 {
        return Err(format!(
            "administrator broker was not started (ShellExecute code {})",
            result as isize
        ));
    }
    Ok(())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
