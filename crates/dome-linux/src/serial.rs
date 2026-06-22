use std::os::fd::RawFd;

pub struct FileHandleSerialAttachment {
    pub(crate) read_fd: Option<RawFd>,
    pub(crate) write_fd: Option<RawFd>,
}

impl FileHandleSerialAttachment {
    pub fn new(read_fd: RawFd, write_fd: RawFd) -> Self {
        FileHandleSerialAttachment {
            read_fd: Some(read_fd),
            write_fd: Some(write_fd),
        }
    }

    /// Serial attachment with no read handle (stdin disconnected).
    /// Output goes to the given write fd.
    pub fn new_write_only(write_fd: RawFd) -> Self {
        FileHandleSerialAttachment {
            read_fd: None,
            write_fd: Some(write_fd),
        }
    }
}

pub struct VirtioConsoleSerialPort {
    pub(crate) read_fd: Option<RawFd>,
    pub(crate) write_fd: Option<RawFd>,
}

impl VirtioConsoleSerialPort {
    pub fn new() -> Self {
        VirtioConsoleSerialPort {
            read_fd: None,
            write_fd: None,
        }
    }

    pub fn new_with_attachment(attachment: &FileHandleSerialAttachment) -> Self {
        VirtioConsoleSerialPort {
            read_fd: attachment.read_fd,
            write_fd: attachment.write_fd,
        }
    }

    pub fn set_attachment(&mut self, attachment: &FileHandleSerialAttachment) {
        self.read_fd = attachment.read_fd;
        self.write_fd = attachment.write_fd;
    }
}

impl Default for VirtioConsoleSerialPort {
    fn default() -> Self {
        Self::new()
    }
}
