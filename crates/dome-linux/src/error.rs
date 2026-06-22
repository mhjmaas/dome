#[derive(Debug, Clone)]
pub struct VzError {
    message: String,
}

impl VzError {
    pub fn new(message: impl Into<String>) -> Self {
        VzError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for VzError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for VzError {}

pub type Result<T> = std::result::Result<T, VzError>;
