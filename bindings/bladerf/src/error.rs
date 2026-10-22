use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum Error {
    #[error("bladeRF error: {0}")]
    BladeRf(String),

    #[error("bladeRF error {code}: {message}")]
    Native { code: i32, message: String },

    #[error("bladeRF: device not found")]
    DeviceNotFound,

    #[error("bladeRF: stream not active")]
    StreamNotActive,
}

pub(crate) fn check_bladerf(ret: i32, context: &str) -> Result<(), Error> {
    if ret != 0 {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let msg = unsafe {
            let ptr = bladerf_sys::bladerf_strerror(ret);
            if ptr.is_null() {
                context.to_string()
            } else {
                let cstr = std::ffi::CStr::from_ptr(ptr);
                format!("{}: {}", context, cstr.to_string_lossy())
            }
        };
        Err(Error::Native {
            code: ret,
            message: msg,
        })
    } else {
        Ok(())
    }
}

impl Error {
    pub fn is_late(&self) -> bool {
        matches!(self, Self::Native { code, .. } if *code == bladerf_sys::BLADERF_ERR_TIME_PAST)
    }
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Native { code, .. } if *code == bladerf_sys::BLADERF_ERR_TIMEOUT)
    }
}
