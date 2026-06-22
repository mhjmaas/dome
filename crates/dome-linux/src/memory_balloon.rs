/// Placeholder — memory balloon is not yet implemented for KVM.
/// Kept for API compatibility with dome-darwin.
pub struct VirtioMemoryBalloonDevice;

impl VirtioMemoryBalloonDevice {
    pub fn new() -> Self {
        VirtioMemoryBalloonDevice
    }
}

impl Default for VirtioMemoryBalloonDevice {
    fn default() -> Self {
        Self::new()
    }
}
