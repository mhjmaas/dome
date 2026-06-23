//! Client-side data-plane relays, decoupled from the [`Sandbox`](crate::Sandbox) that
//! owns the VM.
//!
//! The interactive ([`run_pty_client`]) and streaming ([`run_piped_client`]) loops here
//! drive the host end of an *already-established* exec session: the peer (a worker
//! process) has connected to the guest over vsock and sent the mount + `ExecRequest`
//! handshake, so all that remains is to shuttle terminal frames (STDIN/RESIZE one way;
//! STDOUT/STDERR/EXIT the other) over whatever transport connects the host to that peer.
//!
//! Because the loops are generic over the stream type, the same code path serves both
//! the in-process case (`Sandbox::shell` relaying straight over the vsock `TcpStream`)
//! and the daemon case (the CLI relaying over a unix socket to a persistent worker that
//! splices the bytes through to the guest). The byte protocol is identical either way —
//! the worker is a transparent pipe, never in a position to reinterpret the frames.

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};

use dome_proto::frame;

#[cfg(target_os = "macos")]
use dome_darwin::terminal;
#[cfg(target_os = "linux")]
use dome_linux::terminal;

/// The current terminal size `(rows, cols)` of `fd`, or a sensible default if `fd` is
/// not a terminal. Exposed so a client driving a remote PTY (e.g. the CLI attaching to a
/// worker) can report the initial window size in its attach handshake.
pub fn terminal_size(fd: RawFd) -> (u16, u16) {
    terminal::terminal_size(fd)
}

/// Drive an interactive PTY session to completion over `writer`/`reader` (the two halves
/// of one duplex stream whose far end is a guest exec session opened with `tty=true`).
/// Puts the host terminal in raw mode, forwards stdin as STDIN frames, propagates
/// window-resize as RESIZE frames, renders STDOUT frames, and returns the guest's exit
/// code from the EXIT frame. The terminal is restored when this returns.
pub fn run_pty_client<Wr, Rd>(mut writer: Wr, reader: Rd, stdin_fd: RawFd) -> i32
where
    Wr: Write + Send + 'static,
    Rd: Read + Send + 'static,
{
    // Enter raw mode — TerminalState restores the previous mode on drop.
    let _raw_guard = terminal::TerminalState::enter_raw_mode(stdin_fd);

    // kqueue/epoll-based stdin relay: blocks until stdin data, a resize, or shutdown.
    let (relay, shutdown_signal) =
        terminal::StdinRelay::new(stdin_fd).expect("failed to init stdin relay");

    let exit_code = Arc::new(Mutex::new(0i32));

    // Thread A: stdin → peer (STDIN frames), window resize → peer (RESIZE frames).
    let stdin_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match relay.wait() {
                terminal::StdinEvent::Ready => {
                    let n = terminal::read_raw(stdin_fd, &mut buf);
                    if n == 0 {
                        break;
                    }
                    if frame::write_frame(&mut writer, frame::STDIN, &buf[..n]).is_err() {
                        break;
                    }
                }
                terminal::StdinEvent::Resize => {
                    let (rows, cols) = terminal::terminal_size(stdin_fd);
                    let payload = frame::resize_payload(rows, cols);
                    if frame::write_frame(&mut writer, frame::RESIZE, &payload).is_err() {
                        break;
                    }
                }
                terminal::StdinEvent::Shutdown => break,
            }
        }
    });

    // Thread B: peer → stdout (STDOUT frames), exit/error frames end the session.
    // BufWriter + deferred flush batches rapid TUI redraws into fewer terminal writes.
    let exit_code_b = exit_code.clone();
    let out_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut stdout = BufWriter::new(std::io::stdout());
        loop {
            match frame::read_frame(&mut reader) {
                Ok(Some((frame::STDOUT, payload))) => {
                    let _ = stdout.write_all(&payload);
                    if reader.buffer().is_empty() {
                        let _ = stdout.flush();
                    }
                }
                Ok(Some((frame::EXIT, payload))) => {
                    let _ = stdout.flush();
                    *exit_code_b.lock().unwrap() = frame::parse_exit_code(&payload).unwrap_or(0);
                    break;
                }
                Ok(Some((frame::ERROR, payload))) => {
                    let _ = stdout.flush();
                    let msg = String::from_utf8_lossy(&payload);
                    let _ = std::io::stderr()
                        .write_all(format!("guest error: {}\r\n", msg).as_bytes());
                    *exit_code_b.lock().unwrap() = 1;
                    break;
                }
                Ok(Some(_)) => {} // unknown frame type, skip
                Ok(None) | Err(_) => break,
            }
        }
        let _ = stdout.flush();
        // Wake the stdin thread out of its blocking wait so this returns promptly.
        shutdown_signal.signal();
    });

    let _ = out_thread.join();
    let _ = stdin_thread.join();
    let code = *exit_code.lock().unwrap();
    code
}

/// Drive a non-interactive (no PTY) exec session to completion, streaming STDOUT/STDERR
/// frames from `reader` to the provided writers until an EXIT/ERROR frame. Returns the
/// guest exit code. Mirrors `Sandbox::exec_with_env`'s loop; stdin is not forwarded
/// (matching the existing one-shot exec behaviour).
pub fn run_piped_client(
    reader: impl Read,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let mut reader = BufReader::new(reader);
    let mut exit_code = 0;
    loop {
        match frame::read_frame(&mut reader) {
            Ok(Some((frame::STDOUT, payload))) => {
                let _ = stdout.write_all(&payload);
            }
            Ok(Some((frame::STDERR, payload))) => {
                let _ = stderr.write_all(&payload);
            }
            Ok(Some((frame::EXIT, payload))) => {
                exit_code = frame::parse_exit_code(&payload).unwrap_or(0);
                break;
            }
            Ok(Some((frame::ERROR, payload))) => {
                let _ = write!(stderr, "guest error: {}", String::from_utf8_lossy(&payload));
                exit_code = 1;
                break;
            }
            Ok(Some(_)) => {} // unknown frame type, skip
            Ok(None) | Err(_) => break,
        }
    }
    let _ = stdout.flush();
    let _ = stderr.flush();
    exit_code
}
