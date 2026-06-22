pub struct VirtioSocketDevice;

impl VirtioSocketDevice {
    pub fn new() -> Self {
        VirtioSocketDevice
    }
}

impl Default for VirtioSocketDevice {
    fn default() -> Self {
        Self::new()
    }
}
