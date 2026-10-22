#[derive(Debug, Clone)]
pub struct TuneRequest {
    pub(crate) target_frequency: f64,
    pub(crate) rf: TuneRequestPolicy,
    pub(crate) dsp: TuneRequestPolicy,
    pub(crate) args: String,
}

impl TuneRequest {
    pub fn with_frequency(frequency: f64) -> Self {
        TuneRequest {
            target_frequency: frequency,
            rf: TuneRequestPolicy::Auto,
            dsp: TuneRequestPolicy::Auto,
            args: String::new(),
        }
    }
    pub fn with_frequency_lo(frequency: f64, local_offset: f64) -> Self {
        TuneRequest {
            target_frequency: frequency,
            rf: TuneRequestPolicy::Manual(frequency + local_offset),
            dsp: TuneRequestPolicy::Auto,
            args: String::new(),
        }
    }

    pub fn set_rf_policy(&mut self, policy: TuneRequestPolicy) {
        self.rf = policy
    }
    pub fn set_dsp_policy(&mut self, policy: TuneRequestPolicy) {
        self.dsp = policy
    }
    pub fn set_args(&mut self, args: String) {
        self.args = args
    }
}

#[derive(Debug, Clone)]
pub enum TuneRequestPolicy {
    None,
    Auto,
    Manual(f64),
}

impl TuneRequestPolicy {
    pub(crate) fn c_policy(&self) -> uhd_sys::uhd_tune_request_policy_t::Type {
        use uhd_sys::uhd_tune_request_policy_t::*;
        match self {
            TuneRequestPolicy::None => UHD_TUNE_REQUEST_POLICY_NONE,
            TuneRequestPolicy::Auto => UHD_TUNE_REQUEST_POLICY_AUTO,
            TuneRequestPolicy::Manual(_) => UHD_TUNE_REQUEST_POLICY_MANUAL,
        }
    }
    pub(crate) fn frequency(&self) -> f64 {
        match self {
            TuneRequestPolicy::None | TuneRequestPolicy::Auto => 0.0,
            TuneRequestPolicy::Manual(frequency) => *frequency,
        }
    }
}
