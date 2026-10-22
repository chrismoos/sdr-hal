/// Filter tap design.
pub mod design;
/// Causal real and complex FIR filters, including integer interpolation and decimation.
pub mod fir;
mod nco;
pub use nco::PhasorNco;
