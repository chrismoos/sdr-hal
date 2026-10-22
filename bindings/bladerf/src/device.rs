use std::ffi::CString;
use std::ptr;

use log::debug;

use crate::error::{check_bladerf, Error};

pub fn rx_channel(ch: u32) -> i32 {
    (ch << 1) as i32
}

pub fn tx_channel(ch: u32) -> i32 {
    ((ch << 1) | 1) as i32
}

pub struct Device {
    pub(crate) ptr: *mut bladerf_sys::bladerf,
}

impl Device {
    pub fn open(device_id: Option<&str>) -> Result<Self, Error> {
        let mut dev: *mut bladerf_sys::bladerf = ptr::null_mut();
        let ret = match device_id.filter(|s| !s.is_empty()) {
            Some(s) => {
                let c =
                    CString::new(s).map_err(|_| Error::BladeRf("invalid device string".into()))?;
                // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
                unsafe { bladerf_sys::bladerf_open(&mut dev, c.as_ptr()) }
            }
            // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
            None => unsafe { bladerf_sys::bladerf_open(&mut dev, ptr::null()) },
        };
        check_bladerf(ret, "bladerf_open")?;
        if dev.is_null() {
            return Err(Error::DeviceNotFound);
        }

        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let board_name = unsafe {
            let p = bladerf_sys::bladerf_get_board_name(dev);
            if p.is_null() {
                "unknown".to_string()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        debug!("bladeRF: opened board={}", board_name);

        Ok(Device { ptr: dev })
    }

    pub fn board_name(&self) -> String {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        unsafe {
            let p = bladerf_sys::bladerf_get_board_name(self.ptr);
            if p.is_null() {
                "unknown".to_string()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    pub fn serial(&self) -> Result<String, Error> {
        let mut buf = [0 as std::os::raw::c_char; 33];
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_get_serial(self.ptr, buf.as_mut_ptr()) },
            "bladerf_get_serial",
        )?;
        // SAFETY: The preceding native API provides a NUL-terminated string in live storage for this conversion.
        let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        Ok(cstr.to_string_lossy().into_owned())
    }

    pub fn set_frequency(&self, channel: i32, frequency: u64) -> Result<(), Error> {
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_set_frequency(self.ptr, channel, frequency) },
            "bladerf_set_frequency",
        )
    }

    pub fn get_frequency(&self, channel: i32) -> Result<u64, Error> {
        let mut freq: u64 = 0;
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_get_frequency(self.ptr, channel, &mut freq) },
            "bladerf_get_frequency",
        )?;
        Ok(freq)
    }

    pub fn set_sample_rate(&self, channel: i32, rate: u32) -> Result<u32, Error> {
        let mut actual: u32 = 0;
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_set_sample_rate(self.ptr, channel, rate, &mut actual) },
            "bladerf_set_sample_rate",
        )?;
        Ok(actual)
    }

    pub fn get_sample_rate(&self, channel: i32) -> Result<u32, Error> {
        let mut rate: u32 = 0;
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_get_sample_rate(self.ptr, channel, &mut rate) },
            "bladerf_get_sample_rate",
        )?;
        Ok(rate)
    }

    pub fn set_bandwidth(&self, channel: i32, bandwidth: u32) -> Result<u32, Error> {
        let mut actual: u32 = 0;
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe {
                bladerf_sys::bladerf_set_bandwidth(self.ptr, channel, bandwidth, &mut actual)
            },
            "bladerf_set_bandwidth",
        )?;
        Ok(actual)
    }

    pub fn set_gain(&self, channel: i32, gain: i32) -> Result<(), Error> {
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_set_gain(self.ptr, channel, gain) },
            "bladerf_set_gain",
        )
    }

    pub fn set_gain_mode(&self, channel: i32, mode: u32) -> Result<(), Error> {
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_set_gain_mode(self.ptr, channel, mode) },
            "bladerf_set_gain_mode",
        )
    }

    pub fn enable_module(&self, channel: i32, enable: bool) -> Result<(), Error> {
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_enable_module(self.ptr, channel, enable) },
            "bladerf_enable_module",
        )
    }

    /// Get the current hardware timestamp (in sample counts) for a direction.
    /// direction: 0 = RX, 1 = TX.
    pub fn get_timestamp(&self, direction: u32) -> Result<u64, Error> {
        let mut ts: u64 = 0;
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_get_timestamp(self.ptr, direction, &mut ts) },
            "bladerf_get_timestamp",
        )?;
        Ok(ts)
    }

    pub fn sync_config(
        &self,
        layout: u32,
        format: u32,
        num_buffers: u32,
        buffer_size: u32,
        num_transfers: u32,
        stream_timeout: u32,
    ) -> Result<(), Error> {
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe {
                bladerf_sys::bladerf_sync_config(
                    self.ptr,
                    layout,
                    format,
                    num_buffers,
                    buffer_size,
                    num_transfers,
                    stream_timeout,
                )
            },
            "bladerf_sync_config",
        )
    }

    pub fn is_fpga_configured(&self) -> Result<bool, Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let ret = unsafe { bladerf_sys::bladerf_is_fpga_configured(self.ptr) };
        if ret < 0 {
            return Err(Error::BladeRf(format!(
                "bladerf_is_fpga_configured returned {}",
                ret
            )));
        }
        Ok(ret == 1)
    }

    pub fn load_fpga(&self, path: &str) -> Result<(), Error> {
        let c = CString::new(path).map_err(|_| Error::BladeRf("invalid FPGA path".into()))?;
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_load_fpga(self.ptr, c.as_ptr()) },
            "bladerf_load_fpga",
        )
    }

    pub fn set_rf_port(&self, channel: i32, port: &str) -> Result<(), Error> {
        let c = CString::new(port).map_err(|_| Error::BladeRf("invalid RF port name".into()))?;
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_set_rf_port(self.ptr, channel, c.as_ptr()) },
            "bladerf_set_rf_port",
        )
    }

    pub fn get_rf_port(&self, channel: i32) -> Result<String, Error> {
        let mut port_ptr: *const std::os::raw::c_char = ptr::null();
        check_bladerf(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_get_rf_port(self.ptr, channel, &mut port_ptr) },
            "bladerf_get_rf_port",
        )?;
        if port_ptr.is_null() {
            return Ok("unknown".to_string());
        }
        // SAFETY: The preceding native API provides a NUL-terminated string in live storage for this conversion.
        let cstr = unsafe { std::ffi::CStr::from_ptr(port_ptr) };
        Ok(cstr.to_string_lossy().into_owned())
    }

    pub fn get_rf_ports(&self, channel: i32) -> Result<Vec<String>, Error> {
        let count =
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { bladerf_sys::bladerf_get_rf_ports(self.ptr, channel, ptr::null_mut(), 0) };
        if count <= 0 {
            return Ok(Vec::new());
        }
        let mut ptrs: Vec<*const std::os::raw::c_char> = vec![ptr::null(); count as usize];
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let ret = unsafe {
            bladerf_sys::bladerf_get_rf_ports(self.ptr, channel, ptrs.as_mut_ptr(), count as u32)
        };
        if ret < 0 {
            return Err(Error::BladeRf("bladerf_get_rf_ports failed".into()));
        }
        let result = ptrs
            .iter()
            .take(ret as usize)
            .filter(|p| !p.is_null())
            .map(|&p| {
                // SAFETY: The preceding native API provides a NUL-terminated string in live storage for this conversion.
                let cstr = unsafe { std::ffi::CStr::from_ptr(p) };
                cstr.to_string_lossy().into_owned()
            })
            .collect();
        Ok(result)
    }

    pub(crate) fn raw(&self) -> *mut bladerf_sys::bladerf {
        self.ptr
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
            unsafe { bladerf_sys::bladerf_close(self.ptr) };
        }
    }
}

// SAFETY: The vendor API permits calls through the live device handle from its independently owned stream threads.
unsafe impl Send for Device {}
// SAFETY: The vendor API permits calls through the live device handle from its independently owned stream threads.
unsafe impl Sync for Device {}
