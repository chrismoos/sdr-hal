use std::ptr;

use super::error::{ReceiveError, ReceiveErrorKind};
use crate::error::{check_status, Error};
use crate::utils::copy_string;
use crate::TimeSpec;

pub struct ReceiveMetadata {
    handle: uhd_sys::uhd_rx_metadata_handle,
    samples: usize,
}

impl ReceiveMetadata {
    pub fn new() -> Result<Self, Error> {
        let mut handle: uhd_sys::uhd_rx_metadata_handle = ptr::null_mut();
        // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
        check_status(unsafe { uhd_sys::uhd_rx_metadata_make(&mut handle) })?;
        Ok(ReceiveMetadata { handle, samples: 0 })
    }

    /// Returns the timestamp of (the first?) of the received samples, according to the USRP's
    /// internal clock
    pub fn time_spec(&self) -> Result<Option<TimeSpec>, Error> {
        if self.has_time_spec()? {
            let mut time = TimeSpec::default();
            let mut seconds_time_t: libc::time_t = Default::default();

            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            check_status(unsafe {
                uhd_sys::uhd_rx_metadata_time_spec(
                    self.handle,
                    &mut seconds_time_t,
                    &mut time.fraction,
                )
            })?;
            time.seconds = seconds_time_t;
            Ok(Some(time))
        } else {
            Ok(None)
        }
    }

    fn has_time_spec(&self) -> Result<bool, Error> {
        let mut has = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_rx_metadata_has_time_spec(self.handle, &mut has) })?;
        Ok(has)
    }

    pub fn start_of_burst(&self) -> Result<bool, Error> {
        let mut value = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_rx_metadata_start_of_burst(self.handle, &mut value) })?;
        Ok(value)
    }

    pub fn end_of_burst(&self) -> Result<bool, Error> {
        let mut value = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_rx_metadata_end_of_burst(self.handle, &mut value) })?;
        Ok(value)
    }

    pub fn more_fragments(&self) -> Result<bool, Error> {
        let mut value = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_rx_metadata_more_fragments(self.handle, &mut value) })?;
        Ok(value)
    }

    pub fn fragment_offset(&self) -> Result<usize, Error> {
        let mut value = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_rx_metadata_fragment_offset(
                self.handle,
                &mut value as *mut usize as *mut _,
            )
        })?;
        Ok(value)
    }

    pub fn out_of_sequence(&self) -> Result<bool, Error> {
        let mut value = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_rx_metadata_out_of_sequence(self.handle, &mut value) })?;
        Ok(value)
    }

    pub fn samples(&self) -> usize {
        self.samples
    }

    pub(crate) fn set_samples(&mut self, samples: usize) {
        self.samples = samples
    }

    fn error_code(&self) -> Result<uhd_sys::uhd_rx_metadata_error_code_t::Type, Error> {
        let mut code = uhd_sys::uhd_rx_metadata_error_code_t::UHD_RX_METADATA_ERROR_CODE_NONE;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_rx_metadata_error_code(self.handle, &mut code) })?;
        Ok(code)
    }

    pub fn last_error(&self) -> Result<Option<ReceiveError>, Error> {
        let out_of_sequence = self.out_of_sequence()?;
        use uhd_sys::uhd_rx_metadata_error_code_t::*;
        let kind = match self.error_code()? {
            UHD_RX_METADATA_ERROR_CODE_TIMEOUT => ReceiveErrorKind::Timeout,
            UHD_RX_METADATA_ERROR_CODE_LATE_COMMAND => ReceiveErrorKind::LateCommand,
            UHD_RX_METADATA_ERROR_CODE_BROKEN_CHAIN => ReceiveErrorKind::BrokenChain,
            UHD_RX_METADATA_ERROR_CODE_OVERFLOW if !out_of_sequence => ReceiveErrorKind::Overflow,
            UHD_RX_METADATA_ERROR_CODE_OVERFLOW if out_of_sequence => {
                ReceiveErrorKind::OutOfSequence
            }
            UHD_RX_METADATA_ERROR_CODE_ALIGNMENT => ReceiveErrorKind::Alignment,
            UHD_RX_METADATA_ERROR_CODE_BAD_PACKET => ReceiveErrorKind::BadPacket,
            UHD_RX_METADATA_ERROR_CODE_NONE => {
                return Ok(None);
            }
            _ => ReceiveErrorKind::Other,
        };
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let message = copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_rx_metadata_strerror(self.handle, buffer, length as _)
        })
        .ok();

        Ok(Some(ReceiveError { kind, message }))
    }

    pub(crate) fn handle_mut(&mut self) -> &mut uhd_sys::uhd_rx_metadata_handle {
        &mut self.handle
    }
}

// Thread safety: The uhd_rx_metadata struct just stores data. All exposed functions read fields.
// SAFETY: The owned native handle remains live while its wrapper moves between threads.
unsafe impl Send for ReceiveMetadata {}
// SAFETY: The owned native handle remains live while its wrapper moves between threads.
unsafe impl Sync for ReceiveMetadata {}

impl Drop for ReceiveMetadata {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        let _ = unsafe { uhd_sys::uhd_rx_metadata_free(&mut self.handle) };
    }
}

mod fmt {
    use super::*;
    use super::{ReceiveError, ReceiveMetadata};
    use std::fmt::{Debug, Display, Formatter, Result};

    impl Debug for ReceiveMetadata {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            f.debug_struct("ReceiveMetadata")
                .field("time_spec", &self.time_spec().ok())
                .field("more_fragments", &self.more_fragments().ok())
                .field("fragment_offset", &self.fragment_offset().ok())
                .field("start_of_burst", &self.start_of_burst().ok())
                .field("end_of_burst", &self.end_of_burst().ok())
                .field("received_samples", &self.samples())
                .finish()
        }
    }

    impl Display for ReceiveError {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            match self.kind {
                ReceiveErrorKind::Timeout => write!(f, "No packet received"),
                ReceiveErrorKind::LateCommand => write!(f, "Command timestamp was in the past"),
                ReceiveErrorKind::BrokenChain => write!(f, "Expected another stream command"),
                ReceiveErrorKind::Overflow => {
                    write!(f, "An internal receive buffer has been filled")
                }
                ReceiveErrorKind::OutOfSequence => write!(f, "Sequence error"),
                ReceiveErrorKind::Alignment => write!(f, "Multi-channel alignment failed"),
                ReceiveErrorKind::BadPacket => write!(f, "A packet could not be parsed"),
                ReceiveErrorKind::Other => write!(f, "Other error"),
            }?;
            match self.message {
                Some(ref message) if !message.is_empty() => write!(f, ": {}", message)?,
                _ => {}
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::ReceiveMetadata;

    #[test]
    fn default_rx_metadata() {
        let metadata = ReceiveMetadata::new().unwrap();
        assert_eq!(None, metadata.time_spec().unwrap());
        assert!(!metadata.start_of_burst().unwrap());
        assert!(!metadata.end_of_burst().unwrap());
        assert!(!metadata.out_of_sequence().unwrap());
        assert!(!metadata.more_fragments().unwrap());
        assert_eq!(0, metadata.fragment_offset().unwrap());
        assert!(metadata.last_error().unwrap().is_none());
    }
}
