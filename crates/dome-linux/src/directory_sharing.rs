/// Stores host directory path and read-only flag for virtiofs sharing.
/// On Linux/KVM, virtiofs requires an external `virtiofsd` process;
/// this type only holds config and is consumed by VirtualMachine.
pub struct SharedDirectory {
    pub(crate) path: String,
    pub(crate) read_only: bool,
}

impl SharedDirectory {
    pub fn new(path: &str, read_only: bool) -> Self {
        SharedDirectory {
            path: path.to_string(),
            read_only,
        }
    }
}

pub struct VirtioFileSystemDevice {
    pub(crate) tag: String,
    pub(crate) host_path: String,
    pub(crate) read_only: bool,
}

impl VirtioFileSystemDevice {
    pub fn new(tag: &str, directory: &SharedDirectory) -> Self {
        VirtioFileSystemDevice {
            tag: tag.to_string(),
            host_path: directory.path.clone(),
            read_only: directory.read_only,
        }
    }
}
