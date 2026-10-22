use num_complex::{Complex, Complex32, Complex64};
use std::convert::{TryFrom, TryInto};
use std::ffi::{CString, NulError};
use std::marker::PhantomData;

#[derive(Debug, Clone)]
pub struct StreamArgs<I> {
    host_format: PhantomData<I>,
    wire_format: String,
    args: String,
    channels: Vec<usize>,
}

impl<I> StreamArgs<I> {
    pub fn new<S>(wire_format: S) -> Self
    where
        S: Into<String>,
    {
        StreamArgs {
            wire_format: wire_format.into(),
            ..StreamArgs::default()
        }
    }

    pub fn builder() -> StreamArgsBuilder<I> {
        StreamArgsBuilder {
            args: StreamArgs::default(),
        }
    }
}

impl<I> Default for StreamArgs<I> {
    fn default() -> Self {
        StreamArgs {
            host_format: PhantomData,
            wire_format: "sc16".to_string(),
            args: "".to_string(),
            channels: vec![],
        }
    }
}

pub struct StreamArgsBuilder<I> {
    args: StreamArgs<I>,
}

impl<I> StreamArgsBuilder<I> {
    pub fn wire_format(self, wire_format: String) -> Self {
        StreamArgsBuilder {
            args: StreamArgs {
                wire_format,
                ..self.args
            },
        }
    }

    pub fn args(self, args: String) -> Self {
        StreamArgsBuilder {
            args: StreamArgs { args, ..self.args },
        }
    }

    pub fn channels(self, channels: Vec<usize>) -> Self {
        StreamArgsBuilder {
            args: StreamArgs {
                channels,
                ..self.args
            },
        }
    }

    pub fn build(self) -> StreamArgs<I> {
        self.args
    }
}

pub(crate) struct StreamArgsC<'args> {
    pub host_format: CString,
    pub wire_format: CString,
    pub args: CString,
    pub channels: &'args [usize],
}

impl<'args, I> TryFrom<&'args StreamArgs<I>> for StreamArgsC<'args>
where
    I: Item,
{
    type Error = NulError;

    fn try_from(args: &'args StreamArgs<I>) -> Result<Self, Self::Error> {
        Ok(StreamArgsC {
            host_format: CString::new(I::FORMAT)?,
            wire_format: CString::new(&*args.wire_format)?,
            args: CString::new(&*args.args)?,
            channels: &args.channels,
        })
    }
}

pub trait Item {
    const FORMAT: &'static str;
}

impl Item for Complex64 {
    const FORMAT: &'static str = "fc64";
}
impl Item for Complex32 {
    const FORMAT: &'static str = "fc32";
}
impl Item for Complex<i16> {
    const FORMAT: &'static str = "sc16";
}
impl Item for Complex<i8> {
    const FORMAT: &'static str = "sc8";
}

#[derive(Debug, Clone)]
pub struct StreamCommand {
    pub time: StreamTime,
    pub command_type: StreamCommandType,
}

#[derive(Debug, Clone)]
pub enum StreamCommandType {
    StartContinuous,

    StopContinuous,

    CountAndDone(u64),

    CountAndMore(u64),
}

#[derive(Debug, Clone)]
pub enum StreamTime {
    Now,
    Later(std::time::Duration),
}

impl StreamCommand {
    pub(crate) fn as_c_command(&self) -> uhd_sys::uhd_stream_cmd_t {
        let mut c_cmd = uhd_sys::uhd_stream_cmd_t {
            stream_mode: uhd_sys::uhd_stream_mode_t::UHD_STREAM_MODE_START_CONTINUOUS,
            num_samps: 0,
            stream_now: false,
            time_spec_full_secs: 0,
            time_spec_frac_secs: 0.0,
        };

        match &self.time {
            StreamTime::Now => c_cmd.stream_now = true,
            StreamTime::Later(dur) => {
                c_cmd.time_spec_full_secs = dur.as_secs() as i64;
                c_cmd.time_spec_frac_secs = dur.subsec_nanos() as f64 / 1e9
            }
        }

        match self.command_type {
            StreamCommandType::StartContinuous => {
                c_cmd.stream_mode = uhd_sys::uhd_stream_mode_t::UHD_STREAM_MODE_START_CONTINUOUS;
            }
            StreamCommandType::StopContinuous => {
                c_cmd.stream_mode = uhd_sys::uhd_stream_mode_t::UHD_STREAM_MODE_STOP_CONTINUOUS
            }
            StreamCommandType::CountAndDone(samples) => {
                c_cmd.stream_mode = uhd_sys::uhd_stream_mode_t::UHD_STREAM_MODE_NUM_SAMPS_AND_DONE;
                c_cmd.num_samps = samples
                    .try_into()
                    .expect("Number of samples too large for size_t");
            }
            StreamCommandType::CountAndMore(samples) => {
                c_cmd.stream_mode = uhd_sys::uhd_stream_mode_t::UHD_STREAM_MODE_NUM_SAMPS_AND_MORE;
                c_cmd.num_samps = samples
                    .try_into()
                    .expect("Number of samples too large for size_t");
            }
        };
        c_cmd
    }
}
