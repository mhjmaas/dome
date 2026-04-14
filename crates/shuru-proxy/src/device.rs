use std::collections::VecDeque;
use std::os::unix::io::RawFd;

use smoltcp::phy::{self, Checksum, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

const MTU: usize = 1514; // 14-byte Ethernet header + 1500-byte IP payload
const MAX_PENDING_FRAMES: usize = 256;

/// smoltcp Device backed by a Unix datagram socketpair fd.
///
/// One end of the socketpair is given to VZFileHandleNetworkDeviceAttachment
/// (the VM side). This Device reads/writes the other end (the host side),
/// giving us raw L2 Ethernet frames from/to the guest.
pub struct VZDevice {
    fd: RawFd,
    recv_buf: Vec<u8>,
    /// Frames pre-read by `drain_recv()`, waiting to be consumed by smoltcp.
    pending_rx: VecDeque<Vec<u8>>,
}

impl VZDevice {
    pub fn new(fd: RawFd) -> Self {
        VZDevice {
            fd,
            recv_buf: vec![0u8; MTU + 64], // slack for oversized frames
            pending_rx: VecDeque::new(),
        }
    }

    /// Non-blocking read of a single frame from the socketpair.
    fn recv_one_frame(&mut self) -> Option<Vec<u8>> {
        let n = unsafe {
            libc::recv(
                self.fd,
                self.recv_buf.as_mut_ptr() as *mut libc::c_void,
                self.recv_buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            return None;
        }
        Some(self.recv_buf[..n as usize].to_vec())
    }

    /// Drain all available frames from the socketpair (non-blocking).
    /// Call this before `Interface::poll()` so we can inspect frames
    /// (e.g. to detect TCP SYN and dynamically add listening sockets).
    /// ICMP frames are silently dropped — the proxy only handles TCP and
    /// DNS (UDP 53). Without this filter, smoltcp auto-replies to ICMP
    /// echo requests for any IP (due to any_ip=true), which leaks a
    /// response channel even though no data actually reaches the internet.
    pub fn drain_recv(&mut self) {
        while self.pending_rx.len() < MAX_PENDING_FRAMES {
            match self.recv_one_frame() {
                Some(frame) if Self::is_icmp(&frame) => continue,
                Some(frame) => self.pending_rx.push_back(frame),
                None => break,
            }
        }
    }

    /// Check if a raw Ethernet frame carries an ICMP packet.
    /// Ethernet header: dst(6) + src(6) + ethertype(2) = 14 bytes.
    /// IPv4 header byte 9 (offset 23 from frame start) is the protocol field.
    fn is_icmp(frame: &[u8]) -> bool {
        const ETHERTYPE_IPV4: [u8; 2] = [0x08, 0x00];
        const IP_PROTO_ICMP: u8 = 1;
        frame.len() >= 24
            && frame[12..14] == ETHERTYPE_IPV4
            && frame[23] == IP_PROTO_ICMP
    }

    /// Iterate over all pending frames for inspection (e.g. SYN detection).
    pub fn pending_frames(&self) -> impl Iterator<Item = &[u8]> {
        self.pending_rx.iter().map(|v| v.as_slice())
    }
}

pub struct VZRxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for VZRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

pub struct VZTxToken {
    fd: RawFd,
}

impl phy::TxToken for VZTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        let sent = unsafe {
            libc::send(
                self.fd,
                buffer.as_ptr() as *const libc::c_void,
                buffer.len(),
                0,
            )
        };
        if sent < 0 {
            tracing::debug!("TX {len} bytes failed: sent={sent}");
        }
        result
    }
}

impl Device for VZDevice {
    type RxToken<'a> = VZRxToken;
    type TxToken<'a> = VZTxToken;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // First drain pre-read frames, then try reading directly from the
        // socketpair for frames that arrived during poll. Drop ICMP in both
        // paths to match the filter in drain_recv().
        loop {
            let buffer = self
                .pending_rx
                .pop_front()
                .or_else(|| self.recv_one_frame())?;
            if Self::is_icmp(&buffer) {
                continue;
            }
            return Some((VZRxToken { buffer }, VZTxToken { fd: self.fd }));
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VZTxToken { fd: self.fd })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        // The guest VirtIO NIC offloads checksum calculation, so incoming
        // frames may have partial/invalid checksums. Tell smoltcp to only
        // generate checksums on TX (for the guest to verify), not verify on RX.
        caps.checksum.ipv4 = Checksum::Tx;
        caps.checksum.tcp = Checksum::Tx;
        caps.checksum.udp = Checksum::Tx;
        caps.checksum.icmpv4 = Checksum::Tx;
        caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal Ethernet + IPv4 frame with the given IP protocol number.
    /// Ethernet: dst(6) + src(6) + ethertype(2) = 14 bytes
    /// IPv4: version/IHL(1) + DSCP(1) + length(2) + id(2) + flags(2) + TTL(1) + proto(1) + ...
    fn make_frame(ip_proto: u8) -> Vec<u8> {
        let mut frame = vec![0u8; 34]; // 14 (eth) + 20 (ip min)
        // EtherType = IPv4
        frame[12] = 0x08;
        frame[13] = 0x00;
        // IPv4 version + IHL
        frame[14] = 0x45;
        // Protocol field at IPv4 offset 9 = frame offset 23
        frame[23] = ip_proto;
        frame
    }

    #[test]
    fn is_icmp_detects_icmp() {
        assert!(VZDevice::is_icmp(&make_frame(1))); // ICMP
    }

    #[test]
    fn is_icmp_ignores_tcp() {
        assert!(!VZDevice::is_icmp(&make_frame(6))); // TCP
    }

    #[test]
    fn is_icmp_ignores_udp() {
        assert!(!VZDevice::is_icmp(&make_frame(17))); // UDP
    }

    #[test]
    fn is_icmp_ignores_arp() {
        let mut frame = vec![0u8; 42]; // ARP is 28 bytes + 14 eth
        // EtherType = ARP (0x0806), not IPv4
        frame[12] = 0x08;
        frame[13] = 0x06;
        assert!(!VZDevice::is_icmp(&frame));
    }

    #[test]
    fn is_icmp_ignores_short_frames() {
        assert!(!VZDevice::is_icmp(&[]));
        assert!(!VZDevice::is_icmp(&[0u8; 23])); // too short to read proto field
    }
}
