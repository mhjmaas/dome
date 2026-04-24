use crate::error::{Result, VzError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskImageCachingMode {
    Automatic,
    Cached,
    Uncached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskImageSynchronizationMode {
    Full,
    Fsync,
    None,
}

// ---------------------------------------------------------------------------
// Attachment types – store config consumed later by the VM builder
// ---------------------------------------------------------------------------

pub trait StorageAttachment {
    fn disk_path(&self) -> Option<&str>;
    fn nbd_uri(&self) -> Option<&str>;
    fn is_read_only(&self) -> bool;
}

pub struct DiskImageAttachment {
    pub(crate) path: String,
    pub(crate) read_only: bool,
}

impl DiskImageAttachment {
    pub fn new(path: &str, read_only: bool) -> Result<Self> {
        Ok(DiskImageAttachment {
            path: path.to_string(),
            read_only,
        })
    }

    pub fn new_with_options(
        path: &str,
        read_only: bool,
        _caching_mode: DiskImageCachingMode,
        _sync_mode: DiskImageSynchronizationMode,
    ) -> Result<Self> {
        Self::new(path, read_only)
    }
}

impl StorageAttachment for DiskImageAttachment {
    fn disk_path(&self) -> Option<&str> {
        Some(&self.path)
    }
    fn nbd_uri(&self) -> Option<&str> {
        None
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

pub struct NbdAttachment {
    pub(crate) uri: String,
    pub(crate) read_only: bool,
}

impl NbdAttachment {
    pub fn new(uri: &str, _timeout_secs: f64, _read_only: bool) -> Result<Self> {
        // TODO: implement userspace NBD client for Linux.
        // For now fall back to SHURU_STORAGE=direct.
        Err(VzError::new(format!(
            "NBD storage ({}) not yet supported on Linux. Set SHURU_STORAGE=direct",
            uri
        )))
    }
}

impl StorageAttachment for NbdAttachment {
    fn disk_path(&self) -> Option<&str> {
        None
    }
    fn nbd_uri(&self) -> Option<&str> {
        Some(&self.uri)
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

// ---------------------------------------------------------------------------
// Block device config
// ---------------------------------------------------------------------------

pub trait StorageDevice {
    fn get_disk_path(&self) -> Option<&str>;
    fn get_read_only(&self) -> bool;
}

pub struct VirtioBlockDevice {
    pub(crate) path: Option<String>,
    pub(crate) read_only: bool,
}

impl VirtioBlockDevice {
    pub fn new(attachment: &dyn StorageAttachment) -> Self {
        VirtioBlockDevice {
            path: attachment.disk_path().map(|s| s.to_string()),
            read_only: attachment.is_read_only(),
        }
    }
}

impl StorageDevice for VirtioBlockDevice {
    fn get_disk_path(&self) -> Option<&str> {
        self.path.as_deref()
    }
    fn get_read_only(&self) -> bool {
        self.read_only
    }
}
