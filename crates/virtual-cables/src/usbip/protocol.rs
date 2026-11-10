use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 0x0111;

pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;
pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;

pub const CMD_SUBMIT: u32 = 0x0000_0001;
pub const CMD_UNLINK: u32 = 0x0000_0002;
pub const RET_SUBMIT: u32 = 0x0000_0003;
pub const RET_UNLINK: u32 = 0x0000_0004;

pub const DIRECTION_OUT: u32 = 0;
pub const DIRECTION_IN: u32 = 1;

pub const SPEED_FULL: u32 = 2;
pub const SPEED_HIGH: u32 = 3;

pub const NO_ISO_PACKETS: u32 = 0xffff_ffff;

pub const STATUS_OK: i32 = 0;
pub const STATUS_NO_ENTRY: i32 = -2;
pub const STATUS_INVALID: i32 = -22;
pub const STATUS_PIPE: i32 = -32;
pub const STATUS_CONN_RESET: i32 = -104;

#[derive(Clone, Copy, Debug)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BasicHeader {
    pub command: u32,
    pub sequence: u32,
    pub device_id: u32,
    pub direction: u32,
    pub endpoint: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SubmitRequest {
    pub basic: BasicHeader,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: u32,
    pub number_of_packets: u32,
    pub interval: u32,
    pub setup: [u8; 8],
}

impl SubmitRequest {
    pub fn is_isochronous(&self) -> bool {
        self.number_of_packets != NO_ISO_PACKETS && self.number_of_packets != 0
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IsoPacket {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

pub async fn read_op_header<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<OpHeader> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).await?;
    let header = OpHeader {
        version: u16::from_be_bytes([buf[0], buf[1]]),
        code: u16::from_be_bytes([buf[2], buf[3]]),
        status: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
    };
    if header.version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported USB/IP version 0x{:04x}", header.version),
        ));
    }
    Ok(header)
}

pub fn op_header_bytes(code: u16, status: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    push_u16(&mut out, PROTOCOL_VERSION);
    push_u16(&mut out, code);
    push_u32(&mut out, status);
    out
}

pub async fn write_op_header<W: AsyncWrite + Unpin>(
    writer: &mut W,
    code: u16,
    status: u32,
) -> io::Result<()> {
    writer.write_all(&op_header_bytes(code, status)).await
}

pub async fn read_basic_header<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<BasicHeader> {
    let mut buf = [0u8; 20];
    reader.read_exact(&mut buf).await?;
    Ok(BasicHeader {
        command: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        sequence: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        device_id: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        direction: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
        endpoint: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
    })
}

pub async fn read_submit_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    basic: BasicHeader,
) -> io::Result<SubmitRequest> {
    let mut buf = [0u8; 28];
    reader.read_exact(&mut buf).await?;
    let mut setup = [0u8; 8];
    setup.copy_from_slice(&buf[20..28]);
    Ok(SubmitRequest {
        basic,
        transfer_flags: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        transfer_buffer_length: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        start_frame: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        number_of_packets: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
        interval: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
        setup,
    })
}

pub async fn read_iso_packets<R: AsyncRead + Unpin>(
    reader: &mut R,
    count: u32,
) -> io::Result<Vec<IsoPacket>> {
    if count == NO_ISO_PACKETS || count == 0 {
        return Ok(Vec::new());
    }
    if count > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid isochronous packet count {count}"),
        ));
    }
    let mut buf = vec![0u8; count as usize * 16];
    reader.read_exact(&mut buf).await?;
    let mut packets = Vec::with_capacity(count as usize);
    for chunk in buf.chunks_exact(16) {
        packets.push(IsoPacket {
            offset: u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            length: u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            actual_length: u32::from_be_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]),
            status: i32::from_be_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]),
        });
    }
    Ok(packets)
}

pub fn encode_ret_submit(
    request: &SubmitRequest,
    status: i32,
    actual_length: u32,
    data: &[u8],
    packets: &[IsoPacket],
    error_count: u32,
) -> Vec<u8> {
    let (number_of_packets, start_frame) = if request.is_isochronous() {
        (packets.len() as u32, request.start_frame)
    } else {
        (NO_ISO_PACKETS, 0)
    };
    let mut out = Vec::with_capacity(48 + data.len() + packets.len() * 16);
    push_u32(&mut out, RET_SUBMIT);
    push_u32(&mut out, request.basic.sequence);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_i32(&mut out, status);
    push_u32(
        &mut out,
        if status == STATUS_OK { actual_length } else { 0 },
    );
    push_u32(&mut out, start_frame);
    push_u32(&mut out, number_of_packets);
    push_u32(&mut out, error_count);
    push_u64(&mut out, 0);
    if status == STATUS_OK && !data.is_empty() {
        out.extend_from_slice(data);
    }
    for packet in packets {
        push_u32(&mut out, packet.offset);
        push_u32(&mut out, packet.length);
        push_u32(&mut out, packet.actual_length);
        push_i32(&mut out, packet.status);
    }
    out
}

pub fn encode_ret_unlink(request: &BasicHeader, status: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    push_u32(&mut out, RET_UNLINK);
    push_u32(&mut out, request.sequence);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_i32(&mut out, status);
    out.extend_from_slice(&[0u8; 24]);
    out
}

pub fn fixed_string(dst: &mut [u8], value: &str) {
    dst.fill(0);
    let bytes = value.as_bytes();
    let length = bytes.len().min(dst.len());
    dst[..length].copy_from_slice(&bytes[..length]);
}

pub fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SubmitRequest {
        SubmitRequest {
            basic: BasicHeader {
                command: CMD_SUBMIT,
                sequence: 7,
                endpoint: 1,
                direction: DIRECTION_OUT,
                ..Default::default()
            },
            number_of_packets: 2,
            start_frame: 10,
            ..Default::default()
        }
    }

    #[test]
    fn ret_submit_shape() {
        let packets = [
            IsoPacket {
                length: 48,
                actual_length: 48,
                status: STATUS_OK,
                ..Default::default()
            },
            IsoPacket {
                length: 48,
                actual_length: 48,
                status: STATUS_OK,
                ..Default::default()
            },
        ];
        let frame = encode_ret_submit(&request(), STATUS_OK, 96, &[], &packets, 0);
        assert_eq!(frame.len(), 48 + packets.len() * 16);
        assert_eq!(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]), RET_SUBMIT);
        assert_eq!(u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]), 7);
    }

    #[test]
    fn ret_unlink_shape() {
        let basic = BasicHeader {
            sequence: 9,
            ..Default::default()
        };
        let frame = encode_ret_unlink(&basic, STATUS_CONN_RESET);
        assert_eq!(frame.len(), 48);
        assert_eq!(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]), RET_UNLINK);
    }
}
