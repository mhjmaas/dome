use std::io::{Read, Write};
use std::sync::Arc;

use tracing::{debug, warn};

/// Trait for NBD storage backends.
pub trait NbdBackend: Send + Sync {
    fn size(&self) -> u64;
    fn read(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write(&self, offset: u64, buf: &[u8]) -> std::io::Result<usize>;
    fn flush(&self) -> std::io::Result<()>;
}

impl NbdBackend for crate::backend::FlatFileBackend {
    fn size(&self) -> u64 { self.size() }
    fn read(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> { self.read(offset, buf) }
    fn write(&self, offset: u64, buf: &[u8]) -> std::io::Result<usize> { self.write(offset, buf) }
    fn flush(&self) -> std::io::Result<()> { self.flush() }
}

impl NbdBackend for crate::cas::CasBackend {
    fn size(&self) -> u64 { self.size() }
    fn read(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> { self.read(offset, buf) }
    fn write(&self, offset: u64, buf: &[u8]) -> std::io::Result<usize> { self.write(offset, buf) }
    fn flush(&self) -> std::io::Result<()> { self.flush() }
}

// NBD magic values
const NBDMAGIC: u64 = 0x4e42444d41474943;
const IHAVEOPT: u64 = 0x49484156454F5054;
const REPLY_MAGIC: u64 = 0x3e889045565a9;

// Handshake flags
const NBD_FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
const NBD_FLAG_NO_ZEROES: u16 = 1 << 1;

// Client flags
const NBD_FLAG_C_NO_ZEROES: u32 = 1 << 1;

// Transmission flags
const NBD_FLAG_HAS_FLAGS: u16 = 1 << 0;
const NBD_FLAG_SEND_FLUSH: u16 = 1 << 2;

// Option types
const NBD_OPT_EXPORT_NAME: u32 = 1;
const NBD_OPT_ABORT: u32 = 2;
const NBD_OPT_GO: u32 = 7;

// Option reply types
const NBD_REP_ACK: u32 = 1;
const NBD_REP_INFO: u32 = 3;
const NBD_REP_ERR_UNSUP: u32 = (1 << 31) | 1;

// Info types
const NBD_INFO_EXPORT: u16 = 0;

// Command types
const NBD_CMD_READ: u16 = 0;
const NBD_CMD_WRITE: u16 = 1;
const NBD_CMD_DISC: u16 = 2;
const NBD_CMD_FLUSH: u16 = 3;

// Reply magic
const NBD_SIMPLE_REPLY_MAGIC: u32 = 0x67446698;

// Errors
const NBD_OK: u32 = 0;
const NBD_EIO: u32 = 5;
const NBD_EINVAL: u32 = 22;

/// Handle one NBD client connection (blocking I/O on the stream).
pub fn handle_client(
    mut stream: std::os::unix::net::UnixStream,
    backend: Arc<dyn NbdBackend>,
) -> anyhow::Result<()> {
    handshake(&mut stream, backend.as_ref())?;
    transmission(&mut stream, backend.as_ref())?;
    Ok(())
}

fn handshake(
    stream: &mut std::os::unix::net::UnixStream,
    backend: &dyn NbdBackend,
) -> anyhow::Result<()> {
    // Server sends: NBDMAGIC + IHAVEOPT + handshake flags
    stream.write_all(&NBDMAGIC.to_be_bytes())?;
    stream.write_all(&IHAVEOPT.to_be_bytes())?;
    let server_flags = NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES;
    stream.write_all(&server_flags.to_be_bytes())?;
    stream.flush()?;

    // Client sends: client flags
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    let client_flags = u32::from_be_bytes(buf);
    let no_zeroes = (client_flags & NBD_FLAG_C_NO_ZEROES) != 0;

    // Option haggling loop
    loop {
        // Client sends: IHAVEOPT + option + length
        let mut opt_header = [0u8; 16];
        stream.read_exact(&mut opt_header)?;
        let magic = u64::from_be_bytes(opt_header[0..8].try_into().unwrap());
        if magic != IHAVEOPT {
            anyhow::bail!("bad option magic: {:#x}", magic);
        }
        let option = u32::from_be_bytes(opt_header[8..12].try_into().unwrap());
        let data_len = u32::from_be_bytes(opt_header[12..16].try_into().unwrap());

        // Read option data (export name, info requests, etc.)
        let mut opt_data = vec![0u8; data_len as usize];
        if data_len > 0 {
            stream.read_exact(&mut opt_data)?;
        }

        match option {
            NBD_OPT_EXPORT_NAME => {
                // Legacy negotiation: send export info directly, no reply header
                let trans_flags = NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH;
                stream.write_all(&backend.size().to_be_bytes())?;
                stream.write_all(&trans_flags.to_be_bytes())?;
                if !no_zeroes {
                    stream.write_all(&[0u8; 124])?;
                }
                stream.flush()?;
                debug!("NBD handshake complete (EXPORT_NAME), size={}", backend.size());
                return Ok(());
            }
            NBD_OPT_GO => {
                // Modern negotiation: send NBD_REP_INFO then NBD_REP_ACK
                let trans_flags = NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH;

                // Send NBD_INFO_EXPORT
                let mut info = Vec::with_capacity(12);
                info.extend_from_slice(&NBD_INFO_EXPORT.to_be_bytes());
                info.extend_from_slice(&backend.size().to_be_bytes());
                info.extend_from_slice(&trans_flags.to_be_bytes());
                send_option_reply(stream, option, NBD_REP_INFO, &info)?;

                // Send ACK
                send_option_reply(stream, option, NBD_REP_ACK, &[])?;
                stream.flush()?;
                debug!("NBD handshake complete (GO), size={}", backend.size());
                return Ok(());
            }
            NBD_OPT_ABORT => {
                send_option_reply(stream, option, NBD_REP_ACK, &[])?;
                stream.flush()?;
                anyhow::bail!("client aborted");
            }
            _ => {
                debug!("unsupported NBD option: {}", option);
                send_option_reply(stream, option, NBD_REP_ERR_UNSUP, &[])?;
                stream.flush()?;
            }
        }
    }
}

fn send_option_reply(
    stream: &mut std::os::unix::net::UnixStream,
    option: u32,
    reply_type: u32,
    data: &[u8],
) -> std::io::Result<()> {
    stream.write_all(&REPLY_MAGIC.to_be_bytes())?;
    stream.write_all(&option.to_be_bytes())?;
    stream.write_all(&reply_type.to_be_bytes())?;
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    if !data.is_empty() {
        stream.write_all(data)?;
    }
    Ok(())
}

fn transmission(
    stream: &mut std::os::unix::net::UnixStream,
    backend: &dyn NbdBackend,
) -> anyhow::Result<()> {
    let mut req_header = [0u8; 28];

    loop {
        if let Err(e) = stream.read_exact(&mut req_header) {
            match e.kind() {
                std::io::ErrorKind::UnexpectedEof => {
                    debug!("NBD client disconnected");
                    return Ok(());
                }
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    debug!("NBD read timeout, exiting");
                    return Ok(());
                }
                _ => return Err(e.into()),
            }
        }

        let magic = u32::from_be_bytes(req_header[0..4].try_into().unwrap());
        if magic != 0x25609513 {
            anyhow::bail!("bad request magic: {:#x}", magic);
        }

        // flags at [4..6] (ignored for now)
        let cmd_type = u16::from_be_bytes(req_header[6..8].try_into().unwrap());
        let handle = &req_header[8..16];
        let offset = u64::from_be_bytes(req_header[16..24].try_into().unwrap());
        let length = u32::from_be_bytes(req_header[24..28].try_into().unwrap());

        match cmd_type {
            NBD_CMD_READ => {
                let mut buf = vec![0u8; length as usize];
                let error = match backend.read(offset, &mut buf) {
                    Ok(n) => {
                        if n < length as usize {
                            buf[n..].fill(0);
                        }
                        NBD_OK
                    }
                    Err(e) => {
                        warn!("NBD read error at offset {}: {}", offset, e);
                        NBD_EIO
                    }
                };
                send_reply(stream, error, handle, if error == NBD_OK { Some(&buf) } else { None })?;
            }
            NBD_CMD_WRITE => {
                let mut data = vec![0u8; length as usize];
                stream.read_exact(&mut data)?;
                let error = match backend.write(offset, &data) {
                    Ok(_) => NBD_OK,
                    Err(e) => {
                        warn!("NBD write error at offset {}: {}", offset, e);
                        NBD_EIO
                    }
                };
                send_reply(stream, error, handle, None)?;
            }
            NBD_CMD_FLUSH => {
                let error = match backend.flush() {
                    Ok(()) => NBD_OK,
                    Err(e) => {
                        warn!("NBD flush error: {}", e);
                        NBD_EIO
                    }
                };
                send_reply(stream, error, handle, None)?;
            }
            NBD_CMD_DISC => {
                debug!("NBD client sent disconnect");
                return Ok(());
            }
            _ => {
                warn!("unsupported NBD command: {}", cmd_type);
                send_reply(stream, NBD_EINVAL, handle, None)?;
            }
        }
    }
}

fn send_reply(
    stream: &mut std::os::unix::net::UnixStream,
    error: u32,
    handle: &[u8],
    data: Option<&[u8]>,
) -> std::io::Result<()> {
    stream.write_all(&NBD_SIMPLE_REPLY_MAGIC.to_be_bytes())?;
    stream.write_all(&error.to_be_bytes())?;
    stream.write_all(handle)?;
    if let Some(data) = data {
        stream.write_all(data)?;
    }
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use crate::FlatFileBackend;

    fn create_test_backend() -> (tempfile::NamedTempFile, Arc<FlatFileBackend>) {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let data = vec![0xABu8; 1024 * 1024]; // 1MB
        tmp.write_all(&data).unwrap();
        tmp.flush().unwrap();
        let backend = Arc::new(FlatFileBackend::open(tmp.path().to_str().unwrap()).unwrap());
        (tmp, backend)
    }

    #[test]
    fn test_backend_read_write() {
        let (_tmp, backend) = create_test_backend();
        let mut buf = [0u8; 4];
        backend.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0xAB; 4]);

        backend.write(0, &[1, 2, 3, 4]).unwrap();
        backend.read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        // Original data unchanged after the write
        let mut buf2 = [0u8; 4];
        backend.read(4, &mut buf2).unwrap();
        assert_eq!(buf2, [0xAB; 4]);
    }
}
