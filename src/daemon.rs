/// Blocking worker bridges and TX-fragment assembly.
mod bridge;
mod clock;
/// Composite mode, where several clients share one device stream.
pub mod composite;
/// Daemon listener configuration loaded from JSON.
pub mod config;
/// Control connections that report their own closing.
mod control;
/// Asynchronous control service that owns the radio and its stream workers.
mod service;
/// Open sessions and the composite slots they feed.
mod sessions;
pub use composite::CompositeConfig;
pub use config::RadiodConfig;
pub use service::{RadioCalibration, RxDefaults, serve};

pub mod realtime;
