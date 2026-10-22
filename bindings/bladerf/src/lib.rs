pub mod device;
pub mod error;
pub mod stream;

pub use device::Device;
pub use error::Error;
pub use stream::{RxSync, TxSync};

pub mod format {
    pub const SC16_Q11: u32 = bladerf_sys::bladerf_format::BLADERF_FORMAT_SC16_Q11;
    /// Interleaved 16-bit I/Q, each buffer prefixed by a hardware timestamp header.
    pub const SC16_Q11_META: u32 = bladerf_sys::bladerf_format::BLADERF_FORMAT_SC16_Q11_META;
}

pub mod meta {
    pub const FLAG_TX_BURST_START: u32 = bladerf_sys::BLADERF_META_FLAG_TX_BURST_START;
    pub const FLAG_TX_BURST_END: u32 = bladerf_sys::BLADERF_META_FLAG_TX_BURST_END;
    /// Transmits immediately, ignoring the metadata timestamp.
    pub const FLAG_TX_NOW: u32 = bladerf_sys::BLADERF_META_FLAG_TX_NOW;
    /// Schedules the burst at the metadata timestamp.
    pub const FLAG_TX_UPDATE_TIMESTAMP: u32 = bladerf_sys::BLADERF_META_FLAG_TX_UPDATE_TIMESTAMP;
    /// Returns samples immediately, ignoring the metadata timestamp.
    pub const FLAG_RX_NOW: u32 = bladerf_sys::BLADERF_META_FLAG_RX_NOW;
    pub const STATUS_OVERRUN: u32 = bladerf_sys::BLADERF_META_STATUS_OVERRUN;
    pub const STATUS_UNDERRUN: u32 = bladerf_sys::BLADERF_META_STATUS_UNDERRUN;
}

pub mod layout {
    pub const RX_X1: u32 = bladerf_sys::bladerf_channel_layout::BLADERF_RX_X1;
    pub const TX_X1: u32 = bladerf_sys::bladerf_channel_layout::BLADERF_TX_X1;
}

/// Stream directions for [`Device::get_timestamp`].
pub mod direction {
    pub const RX: u32 = bladerf_sys::bladerf_direction::BLADERF_RX;
    pub const TX: u32 = bladerf_sys::bladerf_direction::BLADERF_TX;
}

pub mod gain_mode {
    pub const MGC: u32 = bladerf_sys::bladerf_gain_mode::BLADERF_GAIN_MGC;
}
