#[derive(Clone)]
pub struct TuneResult(uhd_sys::uhd_tune_result_t);

impl TuneResult {
    pub fn target_rf_freq(&self) -> f64 {
        self.0.target_rf_freq
    }

    pub fn clipped_rf_freq(&self) -> f64 {
        self.0.clipped_rf_freq
    }

    pub fn actual_rf_freq(&self) -> f64 {
        self.0.actual_rf_freq
    }

    pub fn target_dsp_freq(&self) -> f64 {
        self.0.target_dsp_freq
    }

    pub fn actual_dsp_freq(&self) -> f64 {
        self.0.actual_dsp_freq
    }

    pub(crate) fn inner_mut(&mut self) -> &mut uhd_sys::uhd_tune_result_t {
        &mut self.0
    }
}

impl Default for TuneResult {
    fn default() -> Self {
        TuneResult(uhd_sys::uhd_tune_result_t {
            clipped_rf_freq: 0.0,
            target_rf_freq: 0.0,
            actual_rf_freq: 0.0,
            target_dsp_freq: 0.0,
            actual_dsp_freq: 0.0,
        })
    }
}

// SAFETY: The owned native handle remains live while its wrapper moves between threads.
unsafe impl Send for TuneResult {}
// SAFETY: The owned native handle remains live while its wrapper moves between threads.
unsafe impl Sync for TuneResult {}

mod fmt {
    use super::TuneResult;
    use std::fmt::{Debug, Formatter, Result};

    impl Debug for TuneResult {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            f.debug_struct("TuneResult")
                .field("target_rf_freq", &self.0.target_rf_freq)
                .field("clipped_rf_freq", &self.0.clipped_rf_freq)
                .field("actual_rf_freq", &self.0.actual_rf_freq)
                .field("target_dsp_freq", &self.0.target_dsp_freq)
                .field("actual_dsp_freq", &self.0.actual_dsp_freq)
                .finish()
        }
    }
}
