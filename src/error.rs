use std::fmt;

/// Portable failure categories. Drivers retain their native error as the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The requested setting is invalid.
    InvalidSetting,
    /// The driver cannot perform the requested operation.
    Unsupported,
    /// The operation exceeded its wait budget.
    Timeout,
    /// The device or transport disconnected.
    Disconnected,
    /// A scheduled timestamp has already passed.
    Late,
    /// Only a prefix of a TX buffer was accepted.
    IncompleteWrite,
    /// A transport packet is invalid.
    Protocol,
    /// A file or socket operation failed.
    Io,
    /// A driver failure without a more specific category.
    Backend,
}

/// A HAL failure with a portable category and an optional native source.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    accepted_samples: Option<usize>,
}

impl Error {
    /// Creates a failure without a native source.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
            accepted_samples: None,
        }
    }

    /// Preserves a native cause while attaching a portable category and context.
    pub fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
            accepted_samples: None,
        }
    }

    /// Adds operation context while preserving the native source and portable category.
    pub fn context(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Category suitable for recovery decisions without parsing the error message.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Number of accepted samples before failure, when the driver knows it.
    pub fn accepted_samples(&self) -> Option<usize> {
        self.accepted_samples
    }

    /// Attaches known progress. An absent count must not be treated as zero.
    pub fn with_accepted_samples(mut self, count: usize) -> Self {
        self.accepted_samples = Some(count);
        self
    }

    /// Use for an operation rejected before changing hardware.
    pub fn unsupported(operation: &str) -> Self {
        Self::new(
            ErrorKind::Unsupported,
            format!("{operation} is not supported"),
        )
    }

    /// Use for invalid values detected during validation.
    pub fn invalid_setting(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidSetting, message)
    }

    /// Reports a short write with its accepted sample count.
    pub fn incomplete_write(accepted: usize, requested: usize) -> Self {
        Self::new(
            ErrorKind::IncompleteWrite,
            format!("incomplete TX write: accepted {accepted} of {requested} samples"),
        )
        .with_accepted_samples(accepted)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &dyn std::error::Error)
    }
}
impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::new(ErrorKind::Backend, message)
    }
}
impl From<&str> for Error {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        let kind = match source.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected => ErrorKind::Disconnected,
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                ErrorKind::InvalidSetting
            }
            _ => ErrorKind::Io,
        };
        Self::with_source(kind, source.to_string(), source)
    }
}
macro_rules! driver_error {
    ($feature:literal, $ty:ty) => {
        #[cfg(feature = $feature)]
        impl From<$ty> for Error {
            fn from(source: $ty) -> Self {
                Self::with_source(ErrorKind::Backend, source.to_string(), source)
            }
        }
    };
}
#[cfg(feature = "config")]
impl From<serde_json::Error> for Error {
    fn from(source: serde_json::Error) -> Self {
        Self::with_source(ErrorKind::InvalidSetting, source.to_string(), source)
    }
}
driver_error!("file", hound::Error);
driver_error!("uhd-backend", uhd::Error);
driver_error!("lime-backend", limesuite::Error);
#[cfg(feature = "bladerf-backend")]
impl From<bladerf::Error> for Error {
    fn from(source: bladerf::Error) -> Self {
        let kind = if source.is_late() {
            ErrorKind::Late
        } else if source.is_timeout() {
            ErrorKind::Timeout
        } else {
            ErrorKind::Backend
        };
        Self::with_source(kind, source.to_string(), source)
    }
}
#[cfg(feature = "soapy-backend")]
impl From<soapysdr::Error> for Error {
    fn from(source: soapysdr::Error) -> Self {
        let kind = match source.code {
            soapysdr::ErrorCode::Timeout => ErrorKind::Timeout,
            soapysdr::ErrorCode::TimeError => ErrorKind::Late,
            soapysdr::ErrorCode::NotSupported => ErrorKind::Unsupported,
            _ => ErrorKind::Backend,
        };
        Self::with_source(kind, source.to_string(), source)
    }
}
driver_error!("network-backend", tonic::transport::Error);

#[cfg(feature = "network-backend")]
impl From<tonic::Status> for Error {
    fn from(source: tonic::Status) -> Self {
        let kind = match source.code() {
            tonic::Code::InvalidArgument => ErrorKind::InvalidSetting,
            tonic::Code::Unimplemented => ErrorKind::Unsupported,
            tonic::Code::DeadlineExceeded => ErrorKind::Timeout,
            tonic::Code::Unavailable => ErrorKind::Disconnected,
            _ => ErrorKind::Backend,
        };
        Self::with_source(kind, source.to_string(), source)
    }
}

driver_error!("network-backend", tokio::task::JoinError);
driver_error!("network-backend", tokio::sync::oneshot::error::RecvError);
driver_error!("network-backend", crossbeam_channel::RecvError);

#[cfg(feature = "network-backend")]
impl<T: Send + Sync + 'static> From<crossbeam_channel::SendError<T>> for Error {
    fn from(source: crossbeam_channel::SendError<T>) -> Self {
        Self::with_source(ErrorKind::Disconnected, "radio worker disconnected", source)
    }
}

impl<T: Send + Sync + 'static> From<std::sync::mpsc::SendError<T>> for Error {
    fn from(source: std::sync::mpsc::SendError<T>) -> Self {
        Self::with_source(ErrorKind::Disconnected, "IQ channel disconnected", source)
    }
}
impl From<std::sync::mpsc::RecvError> for Error {
    fn from(source: std::sync::mpsc::RecvError) -> Self {
        Self::with_source(ErrorKind::Disconnected, "IQ channel disconnected", source)
    }
}
impl From<std::sync::mpsc::RecvTimeoutError> for Error {
    fn from(source: std::sync::mpsc::RecvTimeoutError) -> Self {
        let kind = if source == std::sync::mpsc::RecvTimeoutError::Timeout {
            ErrorKind::Timeout
        } else {
            ErrorKind::Disconnected
        };
        Self::with_source(kind, source.to_string(), source)
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for Error {
    fn from(source: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self {
            kind: ErrorKind::Backend,
            message: source.to_string(),
            source: Some(source),
            accepted_samples: None,
        }
    }
}
