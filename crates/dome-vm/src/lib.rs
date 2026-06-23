#![forbid(unsafe_code)]

pub mod client;
mod sandbox;

pub use dome_proto::{
    frame, ExecRequest, ForwardRequest, ForwardResponse, MountRequest, MountResponse, PortMapping,
    ReadFileRequest, WriteFileRequest, WriteFileResponse, VSOCK_PORT, VSOCK_PORT_FORWARD,
};
pub use sandbox::{MountConfig, PortForwardHandle, Sandbox, VmConfigBuilder};

// Re-exports from platform-specific backend for advanced/escape-hatch use
#[cfg(target_os = "macos")]
pub use dome_darwin::VirtualMachine;
#[cfg(target_os = "macos")]
pub use dome_darwin::VmState;
#[cfg(target_os = "macos")]
pub use dome_darwin::VzError;

#[cfg(target_os = "linux")]
pub use dome_linux::VirtualMachine;
#[cfg(target_os = "linux")]
pub use dome_linux::VmState;
#[cfg(target_os = "linux")]
pub use dome_linux::VzError;

/// Reject checkpoint names that could escape the checkpoints directory.
pub fn validate_checkpoint_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("checkpoint name cannot be empty".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') || name.contains("..") {
        return Err(format!("invalid checkpoint name: '{}'", name));
    }
    Ok(())
}

pub fn default_data_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome", home)
}
