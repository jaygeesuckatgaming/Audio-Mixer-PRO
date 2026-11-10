use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::uac::{self, Device};
use crate::LogFn;

use super::protocol::*;

const MAX_TRANSFER_LENGTH: u32 = 16 * 1024 * 1024;
static URB_COUNT: AtomicU64 = AtomicU64::new(0);

pub struct ServerHandle {
    pub ready: oneshot::Receiver<()>,
    pub shutdown: CancellationToken,
    pub join: tokio::task::JoinHandle<io::Result<()>>,
}

pub fn spawn_server(address: String, devices: Vec<Arc<Device>>, log: LogFn) -> ServerHandle {
    let (ready_tx, ready_rx) = oneshot::channel();
    let shutdown = CancellationToken::new();
    let join = tokio::spawn(serve(address, devices, log, ready_tx, shutdown.clone()));
    ServerHandle {
        ready: ready_rx,
        shutdown,
        join,
    }
}

async fn serve(
    address: String,
    devices: Vec<Arc<Device>>,
    log: LogFn,
    ready: oneshot::Sender<()>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let listener = TcpListener::bind(&address).await?;
    let local = listener.local_addr()?;
    log(format!(
        "USB/IP server listening on {local} with {} device(s)",
        devices.len()
    ));
    let _ = ready.send(());
    let devices = Arc::new(devices);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                let (stream, remote) = accepted?;
                let _ = stream.set_nodelay(true);
                let devices = devices.clone();
                let log = log.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    handle_connection(stream, remote, devices, log, shutdown).await;
                });
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    remote: std::net::SocketAddr,
    devices: Arc<Vec<Arc<Device>>>,
    log: LogFn,
    shutdown: CancellationToken,
) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(64 * 1024, read_half);
    let writer = Arc::new(Mutex::new(write_half));

    let header = match read_op_header(&mut reader).await {
        Ok(header) => header,
        Err(err) => {
            log(format!("management handshake from {remote} failed: {err}"));
            return;
        }
    };

    match header.code {
        OP_REQ_DEVLIST => {
            log(format!("device-list request from {remote}"));
            let mut writer = writer.lock().await;
            if let Err(err) = write_device_list(&mut *writer, &devices).await {
                log(format!("device-list reply failed: {err}"));
            }
        }
        OP_REQ_IMPORT => {
            let mut bus = [0u8; 32];
            if let Err(err) = reader.read_exact(&mut bus).await {
                log(format!("import request failed: {err}"));
                return;
            }
            let end = bus.iter().position(|b| *b == 0).unwrap_or(bus.len());
            let bus_id = String::from_utf8_lossy(&bus[..end]).to_string();
            let Some(device) = devices.iter().find(|d| d.bus_id == bus_id).cloned() else {
                let mut writer = writer.lock().await;
                let _ = write_op_header(&mut *writer, OP_REP_IMPORT, 1).await;
                log(format!("client requested unknown bus ID {bus_id:?}"));
                return;
            };
            {
                let mut writer = writer.lock().await;
                if let Err(err) = write_op_header(&mut *writer, OP_REP_IMPORT, 0).await {
                    log(format!("import reply failed: {err}"));
                    return;
                }
                if let Err(err) = write_usb_device(&mut *writer, &device).await {
                    log(format!("import device reply failed: {err}"));
                    return;
                }
            }
            log(format!(
                "import accepted: {} ({}), client {remote}",
                device.bus_id,
                device.product()
            ));
            handle_urbs(shutdown, &mut reader, writer, device, log).await;
        }
        _ => log(format!(
            "unsupported management opcode 0x{:04x} from {remote}",
            header.code
        )),
    }
}

async fn write_device_list<W: AsyncWrite + Unpin>(
    writer: &mut W,
    devices: &[Arc<Device>],
) -> io::Result<()> {
    write_op_header(writer, OP_REP_DEVLIST, 0).await?;
    writer.write_all(&(devices.len() as u32).to_be_bytes()).await?;
    for device in devices {
        write_usb_device(writer, device).await?;
        let protocol = if device.high_speed() { 0x20u8 } else { 0x00 };
        writer.write_all(&[0x01, 0x01, protocol, 0]).await?;
        writer.write_all(&[0x01, 0x02, protocol, 0]).await?;
        writer.write_all(&[0x01, 0x02, protocol, 0]).await?;
    }
    Ok(())
}

async fn write_usb_device<W: AsyncWrite + Unpin>(
    writer: &mut W,
    device: &Device,
) -> io::Result<()> {
    let mut path = [0u8; 256];
    let mut bus_id = [0u8; 32];
    fixed_string(
        &mut path,
        &format!("/sys/devices/platform/virtual-cables/{}", device.bus_id),
    );
    fixed_string(&mut bus_id, &device.bus_id);
    writer.write_all(&path).await?;
    writer.write_all(&bus_id).await?;

    let dev = &device.descriptors.device;
    let speed = if device.high_speed() {
        SPEED_HIGH
    } else {
        SPEED_FULL
    };
    let mut fields = Vec::with_capacity(24);
    push_u32(&mut fields, 1);
    push_u32(&mut fields, device.number as u32);
    push_u32(&mut fields, speed);
    push_u16(&mut fields, u16::from_le_bytes([dev[8], dev[9]]));
    push_u16(&mut fields, u16::from_le_bytes([dev[10], dev[11]]));
    push_u16(&mut fields, u16::from_le_bytes([dev[12], dev[13]]));
    fields.push(dev[4]);
    fields.push(dev[5]);
    fields.push(dev[6]);
    fields.push(1);
    fields.push(dev[17]);
    fields.push(3);
    writer.write_all(&fields).await?;
    Ok(())
}

struct ParsedSubmit {
    req: SubmitRequest,
    out: Vec<u8>,
    packets: Vec<IsoPacket>,
    complete_at: Instant,
}

async fn read_submit<R: AsyncRead + Unpin>(
    reader: &mut R,
    basic: BasicHeader,
) -> io::Result<ParsedSubmit> {
    let req = read_submit_body(reader, basic).await?;
    if req.transfer_buffer_length > MAX_TRANSFER_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid transfer length {}", req.transfer_buffer_length),
        ));
    }
    let out = if basic.direction == DIRECTION_OUT && req.transfer_buffer_length > 0 {
        let mut buf = vec![0u8; req.transfer_buffer_length as usize];
        reader.read_exact(&mut buf).await?;
        buf
    } else {
        Vec::new()
    };
    let packets = read_iso_packets(reader, req.number_of_packets).await?;
    Ok(ParsedSubmit {
        req,
        out,
        packets,
        complete_at: Instant::now(),
    })
}

struct ConnectionState {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    frame: Duration,
    pending: StdMutex<HashMap<u32, CancellationToken>>,
    iso_next: StdMutex<HashMap<u32, Instant>>,
}

impl ConnectionState {
    async fn write_submit(
        &self,
        request: &SubmitRequest,
        status: i32,
        actual_length: u32,
        data: &[u8],
        packets: &[IsoPacket],
        error_count: u32,
    ) -> io::Result<()> {
        let frame = encode_ret_submit(
            request,
            status,
            actual_length,
            data,
            packets,
            error_count,
        );
        let mut writer = self.writer.lock().await;
        writer.write_all(&frame).await
    }

    async fn write_unlink(&self, request: BasicHeader, status: i32) -> io::Result<()> {
        let frame = encode_ret_unlink(&request, status);
        let mut writer = self.writer.lock().await;
        writer.write_all(&frame).await
    }

    fn reserve_iso_completion(
        &self,
        endpoint: u32,
        packets: u32,
        now: Instant,
    ) -> Instant {
        let mut duration = self.frame * packets.min(2048);
        if duration > Duration::from_millis(250) {
            duration = Duration::from_millis(250);
        }
        let mut iso_next = self.iso_next.lock().unwrap();
        let base = iso_next.get(&endpoint).copied().unwrap_or(now);
        let base = if base < now { now } else { base };
        let deadline = base + duration;
        iso_next.insert(endpoint, deadline);
        deadline
    }

    fn add_pending(&self, sequence: u32, token: CancellationToken) {
        self.pending.lock().unwrap().insert(sequence, token);
    }

    fn remove_pending(&self, sequence: u32) {
        self.pending.lock().unwrap().remove(&sequence);
    }

    fn cancel_pending(&self, sequence: u32) -> bool {
        let token = self.pending.lock().unwrap().get(&sequence).cloned();
        match token {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    fn cancel_all(&self) {
        for token in self.pending.lock().unwrap().values() {
            token.cancel();
        }
    }
}

async fn handle_urbs(
    shutdown: CancellationToken,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    device: Arc<Device>,
    log: LogFn,
) {
    let state = Arc::new(ConnectionState {
        writer,
        frame: device.frame_interval(),
        pending: StdMutex::new(HashMap::new()),
        iso_next: StdMutex::new(HashMap::new()),
    });

    loop {
        let basic = match read_basic_header(reader).await {
            Ok(basic) => basic,
            Err(err) => {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    log(format!("{} disconnected: {err}", device.bus_id));
                }
                break;
            }
        };

        match basic.command {
            CMD_SUBMIT => {
                let submit = match read_submit(reader, basic).await {
                    Ok(submit) => submit,
                    Err(err) => {
                        log(format!(
                            "{} could not parse SUBMIT seq={}: {err}",
                            device.bus_id, basic.sequence
                        ));
                        break;
                    }
                };
                trace_submit(&log, &device, &submit);

                if basic.endpoint == 0 || !submit.req.is_isochronous() {
                    if let Err(err) =
                        process_submit(&shutdown, &state, &device, &submit, &log).await
                    {
                        log(format!(
                            "{} SUBMIT seq={} failed: {err}",
                            device.bus_id, basic.sequence
                        ));
                        break;
                    }
                    continue;
                }

                let token = CancellationToken::new();
                let complete_at = state.reserve_iso_completion(
                    basic.endpoint,
                    submit.req.number_of_packets,
                    Instant::now(),
                );
                let mut submit = submit;
                submit.complete_at = complete_at;
                state.add_pending(basic.sequence, token.clone());
                let state_task = state.clone();
                let device_task = device.clone();
                let log_task = log.clone();
                let shutdown_task = shutdown.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        process_submit(&shutdown_task, &state_task, &device_task, &submit, &log_task)
                            .await
                    {
                        if !token.is_cancelled() {
                            log_task(format!(
                                "{} asynchronous SUBMIT seq={} failed: {err}",
                                device_task.bus_id, submit.req.basic.sequence
                            ));
                        }
                    }
                    state_task.remove_pending(submit.req.basic.sequence);
                });
            }
            CMD_UNLINK => {
                let mut target = [0u8; 4];
                if reader.read_exact(&mut target).await.is_err() {
                    break;
                }
                let mut reserved = [0u8; 24];
                if reader.read_exact(&mut reserved).await.is_err() {
                    break;
                }
                let target = u32::from_be_bytes(target);
                let found = state.cancel_pending(target);
                let status = if found {
                    STATUS_CONN_RESET
                } else {
                    STATUS_OK
                };
                log(format!(
                    "{} UNLINK seq={} target={} found={}",
                    device.bus_id, basic.sequence, target, found
                ));
                if state.write_unlink(basic, status).await.is_err() {
                    break;
                }
            }
            _ => {
                log(format!(
                    "{} unsupported URB command 0x{:08x}",
                    device.bus_id, basic.command
                ));
                break;
            }
        }
    }

    state.cancel_all();
    log(format!("USB/IP session closed for {}", device.bus_id));
}

async fn process_submit(
    ctx: &CancellationToken,
    state: &ConnectionState,
    device: &Device,
    submit: &ParsedSubmit,
    log: &LogFn,
) -> io::Result<()> {
    let req = &submit.req;

    if req.basic.endpoint == 0 {
        let Some(setup) = uac::parse_setup(&req.setup) else {
            return state
                .write_submit(req, STATUS_INVALID, 0, &[], &[], 0)
                .await;
        };
        if setup.request == uac::AUDIO_SET_CUR && submit.out.len() >= 4 {
            log(format!(
                "{} clock SET_CUR rate={}",
                device.bus_id,
                u32::from_le_bytes([submit.out[0], submit.out[1], submit.out[2], submit.out[3]])
            ));
        }
        let (mut data, status) = device.handle_control(setup, &submit.out);
        if data.len() > req.transfer_buffer_length as usize {
            data.truncate(req.transfer_buffer_length as usize);
        }
        let mut actual = data.len() as u32;
        if req.basic.direction == DIRECTION_OUT && status == STATUS_OK {
            actual = submit.out.len() as u32;
            data.clear();
        }
        return state.write_submit(req, status, actual, &data, &[], 0).await;
    }

    if req.is_isochronous() && ctx.is_cancelled() {
        let mut packets = submit.packets.clone();
        mark_iso_error(&mut packets, STATUS_CONN_RESET);
        return state
            .write_submit(req, STATUS_CONN_RESET, 0, &[], &packets, packets.len() as u32)
            .await;
    }

    if req.basic.endpoint == 1 && req.basic.direction == DIRECTION_OUT {
        if req.is_isochronous() {
            let rate = uac::device::estimate_rate(
                req.transfer_buffer_length,
                submit.packets.len() as u32,
                device.playback_frame_bytes(),
                device.high_speed(),
            );
            if rate > 0 {
                device.set_playback_rate(rate);
            }
        }
        let mut actual = submit.out.len();
        if device.playback_active() {
            actual = device.write_playback(&submit.out);
        }
        let mut packets = submit.packets.clone();
        mark_iso_packets(&mut packets, actual);
        if req.is_isochronous() && wait_iso_completion(ctx, submit.complete_at).await.is_err()
        {
            mark_iso_error(&mut packets, STATUS_CONN_RESET);
            return state
                .write_submit(req, STATUS_CONN_RESET, 0, &[], &packets, packets.len() as u32)
                .await;
        }
        return state
            .write_submit(req, STATUS_OK, actual as u32, &[], &packets, 0)
            .await;
    }

    if req.basic.endpoint == 2 && req.basic.direction == DIRECTION_IN {
        if req.is_isochronous() && wait_iso_completion(ctx, submit.complete_at).await.is_err()
        {
            let mut packets = submit.packets.clone();
            mark_iso_error(&mut packets, STATUS_CONN_RESET);
            return state
                .write_submit(req, STATUS_CONN_RESET, 0, &[], &packets, packets.len() as u32)
                .await;
        }
        let length = response_payload_length(req, &submit.packets);
        let rate = uac::device::estimate_rate(
            length as u32,
            submit.packets.len() as u32,
            device.capture_frame_bytes(),
            device.high_speed(),
        );
        if rate > 0 {
            device.set_capture_rate(rate);
        }
        let mut data = vec![0u8; length];
        if device.capture_active() {
            device.read_capture(&mut data);
        }
        let mut packets = submit.packets.clone();
        mark_iso_packets(&mut packets, data.len());
        return state
            .write_submit(req, STATUS_OK, data.len() as u32, &data, &packets, 0)
            .await;
    }

    let mut packets = submit.packets.clone();
    mark_iso_error(&mut packets, STATUS_PIPE);
    state
        .write_submit(req, STATUS_PIPE, 0, &[], &packets, packets.len() as u32)
        .await
}

async fn wait_iso_completion(
    ctx: &CancellationToken,
    complete_at: Instant,
) -> Result<(), ()> {
    if ctx.is_cancelled() {
        return Err(());
    }
    if complete_at <= Instant::now() {
        return Ok(());
    }
    tokio::select! {
        _ = tokio::time::sleep_until(complete_at) => Ok(()),
        _ = ctx.cancelled() => Err(()),
    }
}

fn response_payload_length(req: &SubmitRequest, packets: &[IsoPacket]) -> usize {
    if packets.is_empty() {
        return req.transfer_buffer_length as usize;
    }
    let mut total: u64 = packets.iter().map(|p| p.length as u64).sum();
    if req.transfer_buffer_length > 0 && total > req.transfer_buffer_length as u64 {
        total = req.transfer_buffer_length as u64;
    }
    total.min(MAX_TRANSFER_LENGTH as u64) as usize
}

fn mark_iso_packets(packets: &mut [IsoPacket], actual_total: usize) {
    let mut remaining = actual_total as i64;
    for packet in packets {
        packet.status = STATUS_OK;
        let mut wanted = packet.length as i64;
        if wanted > remaining {
            wanted = remaining;
        }
        if wanted < 0 {
            wanted = 0;
        }
        packet.actual_length = wanted as u32;
        remaining -= wanted;
    }
}

fn mark_iso_error(packets: &mut [IsoPacket], status: i32) {
    for packet in packets {
        packet.actual_length = 0;
        packet.status = status;
    }
}

fn trace_submit(log: &LogFn, device: &Device, submit: &ParsedSubmit) {
    let req = &submit.req;
    if req.basic.endpoint == 0 {
        let setup = uac::parse_setup(&req.setup).unwrap_or_default();
        log(format!(
            "{} CTRL seq={} dir={} {}",
            device.bus_id,
            req.basic.sequence,
            direction_name(req.basic.direction),
            describe_setup(setup)
        ));
        return;
    }
    let count = URB_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 20 || count % 500 == 0 {
        log(format!(
            "{} URB #{} seq={} ep={} dir={} bytes={} iso_packets={} {}",
            device.bus_id,
            count,
            req.basic.sequence,
            req.basic.endpoint,
            direction_name(req.basic.direction),
            req.transfer_buffer_length,
            packet_count_name(req.number_of_packets),
            device.audio_status()
        ));
    }
}

fn direction_name(direction: u32) -> &'static str {
    if direction == DIRECTION_IN {
        "IN"
    } else {
        "OUT"
    }
}

fn packet_count_name(count: u32) -> String {
    if count == NO_ISO_PACKETS {
        "none".to_string()
    } else {
        count.to_string()
    }
}

fn describe_setup(setup: uac::SetupPacket) -> String {
    format!(
        "bmRequestType=0x{:02X} bRequest=0x{:02X} wValue=0x{:04X} wIndex=0x{:04X} wLength={}",
        setup.request_type, setup.request, setup.value, setup.index, setup.length
    )
}
