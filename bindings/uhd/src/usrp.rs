use crate::{
    error::{check_status, Error},
    motherboard_eeprom::MotherboardEeprom,
    range::MetaRange,
    stream::{Item, StreamArgs, StreamArgsC},
    string_vector::StringVector,
    utils::copy_string,
    DaughterBoardEeprom, ReceiveInfo, ReceiveStreamer, TimeSpec, TransmitInfo, TransmitStreamer,
    TuneRequest, TuneResult,
};

use std::convert::TryInto;
use std::ffi::CString;
use std::ptr;
pub struct Usrp(uhd_sys::uhd_usrp_handle);

impl Usrp {
    pub fn find(args: &str) -> Result<Vec<String>, Error> {
        let args = CString::new(args)?;
        let mut addresses = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_find(args.as_ptr(), addresses.handle_mut()) })?;
        Ok(addresses.into())
    }

    pub fn open(args: &str) -> Result<Self, Error> {
        let mut handle: uhd_sys::uhd_usrp_handle = ptr::null_mut();
        let args_c = CString::new(args)?;
        // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
        check_status(unsafe { uhd_sys::uhd_usrp_make(&mut handle, args_c.as_ptr()) })?;
        Ok(Usrp(handle))
    }

    pub fn get_tx_antennas(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_antennas(self.0, channel as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_tx_antenna(&self, channel: usize) -> Result<String, Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_usrp_get_tx_antenna(self.0, channel as _, buffer, length as _)
        })
    }

    pub fn get_rx_antennas(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_antennas(self.0, channel as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_rx_antenna(&self, channel: usize) -> Result<String, Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_usrp_get_rx_antenna(self.0, channel as _, buffer, length as _)
        })
    }

    pub fn get_rx_bandwidth(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_bandwidth(self.0, channel as _, &mut value)
        })?;
        Ok(value)
    }

    pub fn get_rx_bandwidth_range(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_bandwidth_range(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_tx_bandwidth(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_bandwidth(self.0, channel as _, &mut value)
        })?;
        Ok(value)
    }

    pub fn get_tx_bandwidth_range(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_bandwidth_range(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_rx_frequency(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_get_rx_freq(self.0, channel as _, &mut value) })?;
        Ok(value)
    }

    pub fn get_rx_frequency_range(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_freq_range(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_tx_frequency(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_get_tx_freq(self.0, channel as _, &mut value) })?;
        Ok(value)
    }

    pub fn get_tx_frequency_range(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_freq_range(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_rx_gain(&self, channel: usize, name: &str) -> Result<f64, Error> {
        let name = CString::new(name)?;
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_gain(self.0, channel as _, name.as_ptr(), &mut value)
        })?;
        Ok(value)
    }
    pub fn get_rx_gain_names(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut names = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_gain_names(self.0, channel as _, names.handle_mut())
        })?;
        Ok(names.into())
    }

    pub fn get_rx_gain_range(&self, channel: usize, name: &str) -> Result<MetaRange, Error> {
        let name = CString::new(name)?;
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_gain_range(self.0, name.as_ptr(), channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_tx_gain(&self, channel: usize, name: &str) -> Result<f64, Error> {
        let name = CString::new(name)?;
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_gain(self.0, channel as _, name.as_ptr(), &mut value)
        })?;
        Ok(value)
    }
    pub fn get_tx_gain_names(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut names = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_gain_names(self.0, channel as _, names.handle_mut())
        })?;
        Ok(names.into())
    }

    pub fn get_tx_gain_range(&self, channel: usize, name: &str) -> Result<MetaRange, Error> {
        let name = CString::new(name)?;
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_gain_range(self.0, name.as_ptr(), channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn clear_command_time(&mut self, mboard: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_clear_command_time(self.0, mboard as _) })
    }

    pub fn get_fe_rx_freq_range(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_fe_rx_freq_range(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_fe_tx_freq_range(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_fe_tx_freq_range(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    /// Sets the frequency of the master clock
    pub fn set_master_clock_rate(&mut self, rate: f64, mboard: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_set_master_clock_rate(self.0, rate, mboard as _) })
    }

    /// Returns the frequency of the master clock
    pub fn get_master_clock_rate(&self, mboard: usize) -> Result<f64, Error> {
        let mut rate = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_master_clock_rate(self.0, mboard as _, &mut rate)
        })?;
        Ok(rate)
    }

    pub fn get_motherboard_name(&self, mboard: usize) -> Result<String, Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_usrp_get_mboard_name(self.0, mboard as _, buffer, length as _)
        })
    }

    pub fn get_normalized_tx_gain(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_normalized_tx_gain(self.0, channel as _, &mut value)
        })?;
        Ok(value)
    }

    pub fn get_normalized_rx_gain(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_normalized_rx_gain(self.0, channel as _, &mut value)
        })?;
        Ok(value)
    }

    pub fn get_num_motherboards(&self) -> Result<usize, Error> {
        let mut value = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_num_mboards(self.0, &mut value as *mut usize as *mut _)
        })?;
        Ok(value)
    }

    pub fn get_num_tx_channels(&self) -> Result<usize, Error> {
        let mut value = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_num_channels(self.0, &mut value as *mut usize as *mut _)
        })?;
        Ok(value)
    }

    pub fn get_num_rx_channels(&self) -> Result<usize, Error> {
        let mut value = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_num_channels(self.0, &mut value as *mut usize as *mut _)
        })?;
        Ok(value)
    }

    pub fn set_user_register(
        &mut self,
        address: u8,
        value: u32,
        mboard: usize,
    ) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_user_register(self.0, address, value, mboard as _)
        })
    }

    /// Returns the current clock source
    pub fn get_clock_source(&self, mboard: usize) -> Result<String, Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_usrp_get_clock_source(self.0, mboard as _, buffer, length as _)
        })
    }
    /// Returns the available clock sources
    pub fn get_clock_sources(&self, mboard: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_clock_sources(self.0, mboard as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_time_source(&self, mboard: usize) -> Result<String, Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_usrp_get_time_source(self.0, mboard as _, buffer, length as _)
        })
    }

    pub fn get_time_sources(&self, mboard: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_time_sources(self.0, mboard as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_mboard_sensor_names(&self, mboard: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_mboard_sensor_names(self.0, mboard as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_motherboard_eeprom(&self, mboard: usize) -> Result<MotherboardEeprom, Error> {
        let mut eeprom = MotherboardEeprom::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_mboard_eeprom(self.0, eeprom.handle(), mboard as _)
        })?;
        Ok(eeprom)
    }

    pub fn get_daughter_board_eeprom(
        &self,
        unit: &str,
        slot: &str,
        mboard: usize,
    ) -> Result<DaughterBoardEeprom, Error> {
        let unit = CString::new(unit)?;
        let slot = CString::new(slot)?;

        let mut eeprom = DaughterBoardEeprom::new()?;

        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_dboard_eeprom(
                self.0,
                eeprom.handle(),
                unit.as_ptr(),
                slot.as_ptr(),
                mboard as _,
            )
        })?;

        Ok(eeprom)
    }

    pub fn get_rx_info(&self, channel: usize) -> Result<ReceiveInfo, Error> {
        let mut info_c = uhd_sys::uhd_usrp_rx_info_t {
            mboard_id: ptr::null_mut(),
            mboard_name: ptr::null_mut(),
            mboard_serial: ptr::null_mut(),
            rx_id: ptr::null_mut(),
            rx_subdev_name: ptr::null_mut(),
            rx_subdev_spec: ptr::null_mut(),
            rx_serial: ptr::null_mut(),
            rx_antenna: ptr::null_mut(),
        };
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        unsafe {
            check_status(uhd_sys::uhd_usrp_get_rx_info(
                self.0,
                channel as _,
                &mut info_c,
            ))?;
            let info = ReceiveInfo::from_c(&info_c)?;
            uhd_sys::uhd_usrp_rx_info_free(&mut info_c);
            Ok(info)
        }
    }

    pub fn get_tx_info(&self, channel: usize) -> Result<TransmitInfo, Error> {
        let mut info_c = uhd_sys::uhd_usrp_tx_info_t {
            mboard_id: ptr::null_mut(),
            mboard_name: ptr::null_mut(),
            mboard_serial: ptr::null_mut(),
            tx_id: ptr::null_mut(),
            tx_subdev_name: ptr::null_mut(),
            tx_subdev_spec: ptr::null_mut(),
            tx_serial: ptr::null_mut(),
            tx_antenna: ptr::null_mut(),
        };
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        unsafe {
            check_status(uhd_sys::uhd_usrp_get_tx_info(
                self.0,
                channel as _,
                &mut info_c,
            ))?;
            let info = TransmitInfo::from_c(&info_c)?;
            uhd_sys::uhd_usrp_tx_info_free(&mut info_c);
            Ok(info)
        }
    }

    pub fn get_rx_lo_export_enabled(&self, channel: usize, name: &str) -> Result<bool, Error> {
        let name = CString::new(name)?;
        let mut enabled = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_lo_export_enabled(
                self.0,
                name.as_ptr(),
                channel as _,
                &mut enabled,
            )
        })?;
        Ok(enabled)
    }

    pub fn get_tx_lo_export_enabled(&self, channel: usize, name: &str) -> Result<bool, Error> {
        let name = CString::new(name)?;
        let mut enabled = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_lo_export_enabled(
                self.0,
                name.as_ptr(),
                channel as _,
                &mut enabled,
            )
        })?;
        Ok(enabled)
    }

    pub fn get_rx_lo_frequency(&self, channel: usize, name: &str) -> Result<f64, Error> {
        let name = CString::new(name)?;
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_lo_freq(self.0, name.as_ptr(), channel as _, &mut value)
        })?;
        Ok(value)
    }

    pub fn get_rx_lo_names(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_lo_names(self.0, channel as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_rx_sensor_names(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_sensor_names(self.0, channel as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_tx_lo_frequency(&self, channel: usize, name: &str) -> Result<f64, Error> {
        let name = CString::new(name)?;
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_lo_freq(self.0, name.as_ptr(), channel as _, &mut value)
        })?;
        Ok(value)
    }

    pub fn get_tx_lo_names(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_lo_names(self.0, channel as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_tx_sensor_names(&self, channel: usize) -> Result<Vec<String>, Error> {
        let mut vector = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_sensor_names(self.0, channel as _, vector.handle_mut())
        })?;
        Ok(vector.into())
    }

    pub fn get_rx_stream<I>(&mut self, args: &StreamArgs<I>) -> Result<ReceiveStreamer<I>, Error>
    where
        I: Item,
    {
        let args: StreamArgsC = args.try_into()?;
        let mut args_c = uhd_sys::uhd_stream_args_t {
            cpu_format: args.host_format.as_ptr() as *mut _,
            otw_format: args.wire_format.as_ptr() as *mut _,
            args: args.args.as_ptr() as *mut _,
            channel_list: args.channels.as_ptr() as *mut _,
            n_channels: args
                .channels
                .len()
                .try_into()
                .expect("Number of channels too large"),
        };

        let mut streamer = ReceiveStreamer::new();
        // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
        check_status(unsafe { uhd_sys::uhd_rx_streamer_make(streamer.handle_mut()) })?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_stream(self.0, &mut args_c, streamer.handle())
        })?;

        Ok(streamer)
    }

    pub fn get_tx_stream<I>(&mut self, args: &StreamArgs<I>) -> Result<TransmitStreamer<I>, Error>
    where
        I: Item,
    {
        let args: StreamArgsC = args.try_into()?;
        let mut args_c = uhd_sys::uhd_stream_args_t {
            cpu_format: args.host_format.as_ptr() as *mut _,
            otw_format: args.wire_format.as_ptr() as *mut _,
            args: args.args.as_ptr() as *mut _,
            channel_list: args.channels.as_ptr() as *mut _,
            n_channels: args
                .channels
                .len()
                .try_into()
                .expect("Number of channels too large"),
        };

        let mut streamer = TransmitStreamer::new();
        // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
        check_status(unsafe { uhd_sys::uhd_tx_streamer_make(streamer.handle_mut()) })?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_stream(self.0, &mut args_c, streamer.handle())
        })?;

        Ok(streamer)
    }

    pub fn get_rx_sample_rate(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_get_rx_rate(self.0, channel as _, &mut value) })?;
        Ok(value)
    }

    pub fn get_rx_sample_rates(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_rx_rates(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_tx_sample_rate(&self, channel: usize) -> Result<f64, Error> {
        let mut value = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_get_tx_rate(self.0, channel as _, &mut value) })?;
        Ok(value)
    }

    pub fn get_tx_sample_rates(&self, channel: usize) -> Result<MetaRange, Error> {
        let mut range = MetaRange::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_tx_rates(self.0, channel as _, range.handle())
        })?;
        Ok(range)
    }

    pub fn get_current_time(&self, mboard: usize) -> Result<TimeSpec, Error> {
        let mut time = TimeSpec::default();
        let mut seconds_time_t: libc::time_t = Default::default();

        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_time_now(
                self.0,
                mboard as _,
                &mut seconds_time_t,
                &mut time.fraction,
            )
        })?;
        time.seconds = seconds_time_t;
        Ok(time)
    }

    /// Sets the current clock source
    pub fn set_clock_source(&self, source: &str, mboard: usize) -> Result<(), Error> {
        let source = CString::new(source)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_clock_source(self.0, source.as_ptr(), mboard as _)
        })
    }

    pub fn set_time_source(&self, source: &str, mboard: usize) -> Result<(), Error> {
        let source = CString::new(source)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_time_source(self.0, source.as_ptr(), mboard as _)
        })
    }

    pub fn set_time_unknown_pps(&self, full_secs: i64, frac_secs: f64) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_time_unknown_pps(self.0, full_secs, frac_secs)
        })?;

        Ok(())
    }

    pub fn set_time_next_pps(
        &self,
        full_secs: i64,
        frac_secs: f64,
        mboard: usize,
    ) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_time_next_pps(self.0, full_secs, frac_secs, mboard as _)
        })?;

        Ok(())
    }

    pub fn get_time_synchronized(&self) -> Result<bool, Error> {
        let mut result = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_time_synchronized(self.0, &mut result as *mut _)
        })?;

        Ok(result)
    }

    pub fn set_rx_agc_enabled(&mut self, enabled: bool, channel: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_set_rx_agc(self.0, enabled, channel as _) })
    }

    pub fn set_rx_antenna(&mut self, antenna: &str, channel: usize) -> Result<(), Error> {
        let antenna = CString::new(antenna)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_rx_antenna(self.0, antenna.as_ptr(), channel as _)
        })
    }

    pub fn set_rx_bandwidth(&mut self, bandwidth: f64, channel: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_set_rx_bandwidth(self.0, bandwidth, channel as _) })
    }

    pub fn set_rx_dc_offset_enabled(&mut self, enabled: bool, channel: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_rx_dc_offset_enabled(self.0, enabled, channel as _)
        })
    }

    pub fn set_rx_frequency(
        &self,
        request: &TuneRequest,
        channel: usize,
    ) -> Result<TuneResult, Error> {
        let args = CString::new(&*request.args)?;
        let mut request_c = uhd_sys::uhd_tune_request_t {
            target_freq: request.target_frequency,
            rf_freq_policy: request.rf.c_policy(),
            rf_freq: request.rf.frequency(),
            dsp_freq_policy: request.dsp.c_policy(),
            dsp_freq: request.dsp.frequency(),
            // Unsafe cast *const c_char to *mut c_char
            // The C++ code probably won't modify this.
            args: args.as_ptr() as *mut _,
        };

        let mut result = TuneResult::default();
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_rx_freq(self.0, &mut request_c, channel as _, result.inner_mut())
        })?;

        Ok(result)
    }

    pub fn set_rx_gain(&self, gain: f64, channel: usize, name: &str) -> Result<(), Error> {
        let name = CString::new(name)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_rx_gain(self.0, gain, channel as _, name.as_ptr())
        })
    }

    pub fn set_rx_sample_rate(&mut self, rate: f64, channel: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_set_rx_rate(self.0, rate, channel as _) })
    }

    pub fn set_tx_antenna(&mut self, antenna: &str, channel: usize) -> Result<(), Error> {
        let antenna = CString::new(antenna)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_tx_antenna(self.0, antenna.as_ptr(), channel as _)
        })
    }

    pub fn set_tx_bandwidth(&mut self, bandwidth: f64, channel: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_set_tx_bandwidth(self.0, bandwidth, channel as _) })
    }

    pub fn set_tx_frequency(
        &self,
        request: &TuneRequest,
        channel: usize,
    ) -> Result<TuneResult, Error> {
        let args = CString::new(&*request.args)?;
        let mut request_c = uhd_sys::uhd_tune_request_t {
            target_freq: request.target_frequency,
            rf_freq_policy: request.rf.c_policy(),
            rf_freq: request.rf.frequency(),
            dsp_freq_policy: request.dsp.c_policy(),
            dsp_freq: request.dsp.frequency(),
            // Unsafe cast *const c_char to *mut c_char
            // The C++ code probably won't modify this.
            args: args.as_ptr() as *mut _,
        };

        let mut result = TuneResult::default();
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_tx_freq(self.0, &mut request_c, channel as _, result.inner_mut())
        })?;

        Ok(result)
    }

    pub fn set_tx_gain(&self, gain: f64, channel: usize, name: &str) -> Result<(), Error> {
        let name = CString::new(name)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_set_tx_gain(self.0, gain, channel as _, name.as_ptr())
        })
    }

    pub fn set_tx_sample_rate(&mut self, rate: f64, channel: usize) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_usrp_set_tx_rate(self.0, rate, channel as _) })
    }

    pub fn get_gpio_banks(&self, mboard: usize) -> Result<Vec<String>, Error> {
        let mut banks = StringVector::new()?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_usrp_get_gpio_banks(self.0, mboard as _, banks.handle_mut())
        })?;
        Ok(banks.into())
    }
}

impl Drop for Usrp {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        let _ = unsafe { uhd_sys::uhd_usrp_free(&mut self.0) };
    }
}

// SAFETY: UHD allows device handles across threads. Callers serialize conflicting settings changes and independently own streamers.
unsafe impl Send for Usrp {}
// SAFETY: UHD allows device handles across threads. Callers serialize conflicting settings changes and independently own streamers.
unsafe impl Sync for Usrp {}
