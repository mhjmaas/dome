use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use crate::stack::{ConnectionId, StackCommand};

/// AsyncRead + AsyncWrite adapter over smoltcp channel-based data flow.
///
/// Allows tokio-rustls (and any other async I/O) to work with data
/// arriving from the smoltcp NetworkStack via channels.
pub struct ChannelStream {
    id: ConnectionId,
    data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl ChannelStream {
    pub fn new(
        id: ConnectionId,
        data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        cmd_tx: mpsc::UnboundedSender<StackCommand>,
    ) -> Self {
        ChannelStream {
            id,
            data_rx,
            cmd_tx,
            read_buf: Vec::new(),
            read_pos: 0,
        }
    }

    /// Prepend data to the read buffer (e.g., a ClientHello already consumed).
    pub fn prepend(&mut self, data: Vec<u8>) {
        if self.read_pos < self.read_buf.len() {
            // There's still unread data — prepend before it
            let remaining = self.read_buf[self.read_pos..].to_vec();
            self.read_buf = data;
            self.read_buf.extend_from_slice(&remaining);
        } else {
            self.read_buf = data;
        }
        self.read_pos = 0;
    }
}

impl AsyncRead for ChannelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Drain buffered data first
        if self.read_pos < self.read_buf.len() {
            let available = &self.read_buf[self.read_pos..];
            let n = available.len().min(buf.remaining());
            buf.put_slice(&available[..n]);
            self.read_pos += n;
            if self.read_pos >= self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }

        // Receive from channel
        match self.data_rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.read_buf = data;
                    self.read_pos = n;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())), // EOF
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for ChannelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.cmd_tx.send(StackCommand::Send {
            id: self.id,
            payload: buf.to_vec(),
        }) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stack channel closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = self.cmd_tx.send(StackCommand::Close { id: self.id });
        Poll::Ready(Ok(()))
    }
}
