use crate::error::{check_status, Error};
use crate::utils::copy_string;
use std::ffi::CString;
use std::ptr;

pub struct MotherboardEeprom(uhd_sys::uhd_mboard_eeprom_handle);

impl MotherboardEeprom {
    pub fn get(&self, key: &str) -> Result<Option<String>, Error> {
        let key = CString::new(key)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let status = copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_mboard_eeprom_get_value(self.0, key.as_ptr(), buffer, length as _)
        });
        match status {
            Ok(value) => Ok(Some(value)),
            Err(e) => match e {
                Error::Key => Ok(None),
                _ => Err(e),
            },
        }
    }

    pub fn put(&mut self, key: String, value: String) -> Result<(), Error> {
        let key = CString::new(key)?;
        let value = CString::new(value)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_mboard_eeprom_set_value(self.0, key.as_ptr(), value.as_ptr())
        })
    }

    pub fn new() -> Result<Self, Error> {
        let mut handle = ptr::null_mut();
        // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
        check_status(unsafe { uhd_sys::uhd_mboard_eeprom_make(&mut handle) })?;
        Ok(MotherboardEeprom(handle))
    }

    pub(crate) fn handle(&mut self) -> uhd_sys::uhd_mboard_eeprom_handle {
        self.0
    }
}

impl Drop for MotherboardEeprom {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        let _ = unsafe { uhd_sys::uhd_mboard_eeprom_free(&mut self.0) };
    }
}

#[cfg(test)]
mod test {
    use super::MotherboardEeprom;

    #[test]
    fn empty_eeprom() {
        let eeprom = MotherboardEeprom::new().unwrap();
        let res = eeprom.get("jabberwock");
        assert!(res.is_ok() && res.unwrap().is_none())
    }
}
