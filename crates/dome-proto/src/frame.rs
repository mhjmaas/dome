use std::io::{self, Read, Write};

// I/O streams
pub const STDIN: u8 = 0x01;
pub const STDOUT: u8 = 0x02;
pub const STDERR: u8 = 0x03;

// Control
pub const RESIZE: u8 = 0x04;
pub const EXIT: u8 = 0x05;
pub const ERROR: u8 = 0x06;
pub const KILL: u8 = 0x07;

// Exec handshake
pub const EXEC_REQ: u8 = 0x10;

// Mount handshake
pub const MOUNT_REQ: u8 = 0x11;
pub const MOUNT_RESP: u8 = 0x12;

// File I/O
pub const READ_FILE_REQ: u8 = 0x13;
pub const READ_FILE_RESP: u8 = 0x14;
pub const WRITE_FILE_REQ: u8 = 0x15;
pub const WRITE_FILE_DATA: u8 = 0x16;
pub const WRITE_FILE_RESP: u8 = 0x17;

// Port forwarding
pub const FWD_REQ: u8 = 0x20;
pub const FWD_RESP: u8 = 0x21;

// File watching
pub const WATCH_REQ: u8 = 0x30;
pub const WATCH_EVENT: u8 = 0x31;

// Filesystem operations
pub const MKDIR_REQ: u8 = 0x40;
pub const FS_OK_RESP: u8 = 0x41;
pub const READ_DIR_REQ: u8 = 0x42;
pub const READ_DIR_RESP: u8 = 0x43;
pub const STAT_REQ: u8 = 0x44;
pub const STAT_RESP: u8 = 0x45;
pub const REMOVE_REQ: u8 = 0x46;
pub const RENAME_REQ: u8 = 0x48;
pub const COPY_REQ: u8 = 0x4A;
pub const CHMOD_REQ: u8 = 0x4C;

// Overlay operations
pub const DISCARD_REQ: u8 = 0x4E;
pub const DISCARD_RESP: u8 = 0x4F;

// Download
pub const DOWNLOAD_REQ: u8 = 0x50;
pub const DOWNLOAD_PROGRESS: u8 = 0x51;

const MAX_FRAME: u32 = 1 << 20; // 1 MB

/// Write a binary frame: `[u32 BE length][u8 type][payload]`.
///
/// Assembles the header + payload into a single buffer so that the entire
/// frame is sent in one `write_all` call. This avoids multiple small TCP
/// segments when `TCP_NODELAY` is enabled.
pub fn write_frame(w: &mut impl Write, msg_type: u8, payload: &[u8]) -> io::Result<()> {
    let len = 1u32 + payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + 1 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(msg_type);
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Read a binary frame. Returns `None` on clean EOF, `Err` on protocol
/// violations or I/O errors.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length out of range: {}", len),
        ));
    }
    let mut type_buf = [0u8; 1];
    r.read_exact(&mut type_buf)?;
    let payload_len = (len - 1) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok(Some((type_buf[0], payload)))
}

/// Serialize `msg` as JSON and send it as a typed frame.
pub fn send_json(w: &mut impl Write, msg_type: u8, msg: &impl serde::Serialize) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    write_frame(w, msg_type, &payload)
}

/// Try to parse a complete frame from the front of `buf`.
/// Returns `Some((msg_type, payload_start, total_len))` if a full
/// frame is available, `None` if more data is needed.
pub fn try_parse(buf: &[u8]) -> Option<(u8, usize, usize)> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len == 0 || len > MAX_FRAME {
        return None;
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        return None;
    }
    let msg_type = buf[4];
    Some((msg_type, 5, total))
}

/// Build a RESIZE payload: `[u16 BE rows][u16 BE cols]`.
pub fn resize_payload(rows: u16, cols: u16) -> [u8; 4] {
    let mut buf = [0u8; 4];
    buf[0..2].copy_from_slice(&rows.to_be_bytes());
    buf[2..4].copy_from_slice(&cols.to_be_bytes());
    buf
}

/// Parse a RESIZE payload into (rows, cols).
pub fn parse_resize(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    let rows = u16::from_be_bytes([payload[0], payload[1]]);
    let cols = u16::from_be_bytes([payload[2], payload[3]]);
    Some((rows, cols))
}

/// Build an EXIT payload: `[i32 BE code]`.
pub fn exit_payload(code: i32) -> [u8; 4] {
    code.to_be_bytes()
}

/// Parse an EXIT payload into an i32 exit code.
pub fn parse_exit_code(payload: &[u8]) -> Option<i32> {
    if payload.len() < 4 {
        return None;
    }
    Some(i32::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::io::Cursor;

    /// A written frame reads back with the same type byte and payload.
    #[test]
    fn frame_roundtrips_through_a_buffer() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDOUT, b"hello world").unwrap();
        let mut cursor = Cursor::new(buf);
        let (msg_type, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(msg_type, STDOUT);
        assert_eq!(payload, b"hello world");
        // A second read on the exhausted cursor is a clean EOF.
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    /// An empty payload is a valid frame (length == 1, just the type byte).
    #[test]
    fn empty_payload_frame_roundtrips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, EXIT, &[]).unwrap();
        let mut cursor = Cursor::new(buf);
        let (msg_type, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(msg_type, EXIT);
        assert!(payload.is_empty());
    }

    /// Multiple frames written back-to-back read back in order.
    #[test]
    fn back_to_back_frames_read_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDIN, b"a").unwrap();
        write_frame(&mut buf, STDOUT, b"bb").unwrap();
        write_frame(&mut buf, STDERR, b"ccc").unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cursor).unwrap().unwrap(),
            (STDIN, b"a".to_vec())
        );
        assert_eq!(
            read_frame(&mut cursor).unwrap().unwrap(),
            (STDOUT, b"bb".to_vec())
        );
        assert_eq!(
            read_frame(&mut cursor).unwrap().unwrap(),
            (STDERR, b"ccc".to_vec())
        );
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    /// A frame whose declared length is zero is rejected as a protocol violation.
    #[test]
    fn zero_length_frame_is_rejected() {
        let mut cursor = Cursor::new(vec![0, 0, 0, 0]);
        assert!(read_frame(&mut cursor).is_err());
    }

    /// A frame whose declared length exceeds the cap is rejected.
    #[test]
    fn oversize_frame_is_rejected() {
        let mut bytes = (MAX_FRAME + 1).to_be_bytes().to_vec();
        bytes.push(STDOUT);
        let mut cursor = Cursor::new(bytes);
        assert!(read_frame(&mut cursor).is_err());
    }

    /// `send_json` frames a serializable value that round-trips back to the same struct.
    #[test]
    fn send_json_roundtrips_a_struct() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Msg {
            a: u32,
            b: String,
        }
        let msg = Msg {
            a: 5,
            b: "hi".to_string(),
        };
        let mut buf = Vec::new();
        send_json(&mut buf, EXEC_REQ, &msg).unwrap();
        let mut cursor = Cursor::new(buf);
        let (msg_type, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(msg_type, EXEC_REQ);
        let back: Msg = serde_json::from_slice(&payload).unwrap();
        assert_eq!(back, msg);
    }

    /// `resize_payload` / `parse_resize` are inverses.
    #[test]
    fn resize_payload_roundtrips() {
        let payload = resize_payload(40, 120);
        assert_eq!(parse_resize(&payload), Some((40, 120)));
        assert_eq!(parse_resize(&[1, 2]), None, "a short payload yields None");
    }

    /// `exit_payload` / `parse_exit_code` are inverses, including negative codes.
    #[test]
    fn exit_payload_roundtrips() {
        for code in [0, 1, 137, -1, i32::MIN, i32::MAX] {
            assert_eq!(parse_exit_code(&exit_payload(code)), Some(code));
        }
        assert_eq!(
            parse_exit_code(&[0, 0]),
            None,
            "a short payload yields None"
        );
    }

    /// `try_parse` reports a complete frame's type and bounds without consuming input,
    /// and asks for more bytes when the buffer holds only part of a frame.
    #[test]
    fn try_parse_reports_complete_frames_only() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDOUT, b"xyz").unwrap();
        // Full frame present.
        let (msg_type, start, total) = try_parse(&buf).unwrap();
        assert_eq!(msg_type, STDOUT);
        assert_eq!(&buf[start..total], b"xyz");
        assert_eq!(total, buf.len());
        // A truncated buffer needs more data.
        assert!(try_parse(&buf[..buf.len() - 1]).is_none());
        assert!(try_parse(&buf[..2]).is_none());
    }
}
