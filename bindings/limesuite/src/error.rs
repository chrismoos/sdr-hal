use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum Error {
    #[error("LimeSuite error: {0}")]
    Lms(String),

    #[error("LimeSuite: device not found")]
    DeviceNotFound,

    #[error("LimeSuite: stream not active")]
    StreamNotActive,
}

pub(crate) fn check_lms(ret: i32, context: &str) -> Result<(), Error> {
    if ret < 0 {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let msg = unsafe {
            let ptr = limesuite_sys::LMS_GetLastErrorMessage();
            if ptr.is_null() {
                context.to_string()
            } else {
                let cstr = std::ffi::CStr::from_ptr(ptr);
                format!("{}: {}", context, cstr.to_string_lossy())
            }
        };
        Err(Error::Lms(msg))
    } else {
        Ok(())
    }
}
