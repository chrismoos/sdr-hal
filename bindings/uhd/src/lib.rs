extern crate libc;
extern crate num_complex;
extern crate uhd_sys;

mod daughter_board_eeprom;
mod error;
mod motherboard_eeprom;
pub mod range;
mod receiver;
mod stream;
mod string_vector;
mod transmitter;
mod tune_request;
mod tune_result;
mod usrp;
mod utils;

pub use daughter_board_eeprom::DaughterBoardEeprom;
pub use error::*;
pub use motherboard_eeprom::MotherboardEeprom;
pub use receiver::{error::*, info::ReceiveInfo, metadata::*, streamer::ReceiveStreamer};
pub use stream::*;
pub use transmitter::{
    async_metadata::TransmitAsyncEvent, info::TransmitInfo, metadata::*, streamer::TransmitStreamer,
};
pub use tune_request::*;
pub use tune_result::TuneResult;
pub use usrp::Usrp;
pub use utils::alloc_boxed_slice;

#[derive(Debug, Clone, Default, PartialOrd, PartialEq)]
pub struct TimeSpec {
    pub seconds: i64,
    pub fraction: f64,
}
