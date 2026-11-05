pub type Result<T> = std::result::Result<T, Error>;

pub struct Error {
    pub message: String,
    pub code: i32,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!("Local filesystem operation failed: {error}"))
    }
}

pub trait IoContext<T> {
    fn at(self, operation: &str, path: &std::path::Path) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, operation: &str, path: &std::path::Path) -> Result<T> {
        self.map_err(|error| Error::new(format!("{operation} {}: {error}", path.display())))
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}
