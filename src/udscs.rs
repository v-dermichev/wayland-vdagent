// udscs protocol — matches spice-vdagent/src/udscs.h
//
// Header: type(4) + arg1(4) + arg2(4) + size(4) = 16 bytes
// Followed by `size` bytes of data

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

const HDR_SIZE: usize = 16;

// Message types — matches vdagentd-proto.h enum
pub const VDAGENTD_GUEST_XORG_RESOLUTION: u32 = 0;
pub const VDAGENTD_MONITORS_CONFIG: u32 = 1;
pub const VDAGENTD_CLIPBOARD_GRAB: u32 = 2;
pub const VDAGENTD_CLIPBOARD_REQUEST: u32 = 3;
pub const VDAGENTD_CLIPBOARD_DATA: u32 = 4;
pub const VDAGENTD_CLIPBOARD_RELEASE: u32 = 5;
pub const VDAGENTD_VERSION: u32 = 6;
pub const VDAGENTD_AUDIO_VOLUME_SYNC: u32 = 7;
pub const VDAGENTD_CLIENT_DISCONNECTED: u32 = 12;
pub const VDAGENTD_GRAPHICS_DEVICE_INFO: u32 = 13;

// Clipboard types — matches spice/vd_agent.h
pub const VD_AGENT_CLIPBOARD_NONE: u32 = 0;
pub const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;

pub struct UdscsMsg {
    pub msg_type: u32,
    pub arg1: u32,
    pub arg2: u32,
    pub data: Vec<u8>,
}

pub fn read_msg(stream: &mut UnixStream) -> io::Result<UdscsMsg> {
    let mut hdr = [0u8; HDR_SIZE];
    stream.read_exact(&mut hdr)?;

    let msg_type = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    let arg1 = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
    let arg2 = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
    let size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;

    let mut data = vec![0u8; size];
    if size > 0 {
        stream.read_exact(&mut data)?;
    }

    Ok(UdscsMsg { msg_type, arg1, arg2, data })
}

pub fn send_msg(stream: &UnixStream, msg_type: u32, arg1: u32, arg2: u32, data: &[u8]) {
    let mut buf = Vec::with_capacity(HDR_SIZE + data.len());
    buf.extend_from_slice(&msg_type.to_le_bytes());
    buf.extend_from_slice(&arg1.to_le_bytes());
    buf.extend_from_slice(&arg2.to_le_bytes());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data);
    let _ = (&*stream).write_all(&buf);
    let _ = (&*stream).flush();
}
