//! Composite plan calculator.
//!
//! Chooses a composite configuration for a set of clients, or checks a given one. Every
//! slice is placed with [`CompositeConfig::plan_slice`], the call radiod makes when a
//! client connects, so a plan without errors is one radiod accepts.
//!
//! The TX timing rule follows from how the grid commits blocks. A block still missing
//! client samples becomes silence `tx_deadline_ms` before air time, and a client sample
//! can fall up to one block ahead of its block's start. A client's samples must therefore
//! reach radiod at least `tx_deadline_ms + block_duration_ms` plus a scheduling margin
//! ahead of air time.

mod report;
mod spec;
#[cfg(test)]
mod tests;

pub use report::{composite_json, render};
pub use spec::{ClientSpec, parse_hz, parse_ms, parse_rate_hz};

use super::{
    CompositeConfig, DEFAULT_BLOCK_DURATION_MS, DEFAULT_PASSBAND_FRACTION, DEFAULT_TX_DEADLINE_MS,
    Direction, gcd,
};

/// The LO leakage tone is kept this far outside every occupied band.
const DC_GUARD_HZ: f64 = 50_000.0;
/// Chosen centers are rounded to this step.
const CENTER_STEP_HZ: u64 = 1_000;
const TARGET_BLOCK_MS: f64 = 1.0;
/// Most decimal places of a millisecond a chosen block duration is written with.
const BLOCK_MS_PLACES: u32 = 6;
/// Longest block accepted in exchange for whole client samples in every block.
const MAX_WHOLE_SAMPLE_BLOCK_MS: f64 = 5.0;
/// radiod reads device time every millisecond to commit blocks, so a shorter deadline
/// lets a block reach the device after its air time.
const MIN_TX_DEADLINE_MS: f64 = 1.0;
/// Headroom for slice filter overshoot, on top of the carrier count.
const FILTER_OVERSHOOT_DB: f64 = 1.0;
/// Scheduling margin assumed between a client's lead and radiod's deadline.
pub const DEFAULT_MARGIN_MS: f64 = 1.0;
const RATE_SEARCH_STEP_HZ: u64 = 1_000;
const MAX_RATE_SEARCH_STEPS: u64 = 1_000_000;
const TIMING_STEP_MS: f64 = 0.1;
/// Tolerance for comparing durations computed in floating point.
const TIMING_EPSILON_MS: f64 = 1e-9;

/// What to plan for, and any values that are fixed rather than chosen.
#[derive(Clone, Debug)]
pub struct PlanRequest {
    /// Clients the plan serves.
    pub clients: Vec<ClientSpec>,
    /// Fastest rate the radio streams. Required when `device_rate_hz` is not fixed.
    pub max_device_rate_hz: Option<u64>,
    /// Device clock in Hz. When set, the device rate must divide it evenly.
    pub device_clock_hz: Option<u64>,
    /// Fixed device rate in Hz. Chosen when `None`.
    pub device_rate_hz: Option<u64>,
    /// Fixed TX center in Hz. Chosen when `None`.
    pub tx_center_hz: Option<u64>,
    /// Fixed RX center in Hz. Chosen when `None`.
    pub rx_center_hz: Option<u64>,
    /// Fixed TX digital backoff in dB. Chosen when `None`.
    pub tx_digital_backoff_db: Option<f64>,
    /// Fixed block duration in milliseconds. Chosen when `None`.
    pub block_duration_ms: Option<f64>,
    /// Fixed TX deadline in milliseconds. Chosen when `None`.
    pub tx_deadline_ms: Option<f64>,
    /// Scheduling margin in milliseconds between a client's lead and the deadline.
    pub margin_ms: f64,
    /// Allows a center inside a carrier's band, where LO leakage lands on the carrier.
    pub allow_dc: bool,
}

impl Default for PlanRequest {
    fn default() -> Self {
        Self {
            clients: Vec::new(),
            max_device_rate_hz: None,
            device_clock_hz: None,
            device_rate_hz: None,
            tx_center_hz: None,
            rx_center_hz: None,
            tx_digital_backoff_db: None,
            block_duration_ms: None,
            tx_deadline_ms: None,
            margin_ms: DEFAULT_MARGIN_MS,
            allow_dc: false,
        }
    }
}

impl PlanRequest {
    /// Fixes every composite value from `config`, so the plan checks it.
    pub fn fix_to(&mut self, config: &CompositeConfig) {
        self.device_rate_hz.get_or_insert(config.device_rate_hz);
        self.tx_center_hz.get_or_insert(config.tx_center_hz);
        self.rx_center_hz.get_or_insert(config.rx_center_hz);
        self.tx_digital_backoff_db
            .get_or_insert(config.tx_digital_backoff_db);
        self.block_duration_ms
            .get_or_insert(config.block_duration_ms);
        self.tx_deadline_ms.get_or_insert(config.tx_deadline_ms);
    }
}

/// How much a finding matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// radiod would refuse the plan or a client in it.
    Error,
    /// radiod accepts the plan, but it can misbehave.
    Warning,
    /// Information that changes nothing radiod does.
    Note,
}

/// One problem or remark about a plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// How much it matters.
    pub severity: Severity,
    /// What is wrong, in one sentence or two.
    pub message: String,
    /// What to change, when there is a clear fix.
    pub help: Option<String>,
}

/// One composite value and why it has the value it has.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    /// The listener file key.
    pub key: &'static str,
    /// The value as written in the listener file.
    pub value: String,
    /// Why the value was chosen, or "given".
    pub reason: String,
}

/// A TX client's lead against what the plan needs.
#[derive(Clone, Debug, PartialEq)]
pub enum Lead {
    /// The client stated its lead.
    Given {
        /// The stated lead in milliseconds.
        lead_ms: f64,
        /// Lead beyond what the plan needs, negative when short.
        spare_ms: f64,
    },
    /// The client stated none, so this is what it must deliver.
    Needs {
        /// Smallest lead in milliseconds the plan works with.
        min_ms: f64,
    },
}

/// One client's slice in one direction.
#[derive(Clone, Debug, PartialEq)]
pub struct SliceRow {
    /// Client label.
    pub client: String,
    /// Carrier in Hz.
    pub carrier_hz: u64,
    /// Signed distance from the center in Hz.
    pub offset_hz: i64,
    /// The client rate radiod runs, in Hz.
    pub rate_hz: f64,
    /// Device samples per client sample, as `ratio / denominator`.
    pub ratio: usize,
    /// See `ratio`.
    pub denominator: usize,
    /// Distance from the band's reach to the edge of the composite span.
    pub edge_margin_hz: f64,
    /// Lead check for a TX slice. `None` for RX.
    pub lead: Option<Lead>,
}

/// Every slice in one direction.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectionReport {
    /// TX or RX.
    pub direction: Direction,
    /// Center in Hz.
    pub center_hz: u64,
    /// Analog bandwidth that passes every occupied band.
    pub analog_bandwidth_hz: u64,
    /// One row per client in this direction.
    pub rows: Vec<SliceRow>,
}

/// A chosen or checked composite configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Number of clients planned for.
    pub client_count: usize,
    /// `None` when no usable configuration exists.
    pub config: Option<CompositeConfig>,
    /// Each composite value and why it has that value.
    pub settings: Vec<Setting>,
    /// TX and RX slices, for directions that have clients.
    pub directions: Vec<DirectionReport>,
    /// Errors first, then warnings, then notes.
    pub findings: Vec<Finding>,
}

impl Plan {
    /// Number of errors.
    pub fn error_count(&self) -> usize {
        self.count(Severity::Error)
    }

    /// Number of warnings.
    pub fn warning_count(&self) -> usize {
        self.count(Severity::Warning)
    }

    fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    }
}

#[derive(Clone, Debug)]
struct Carrier {
    client: String,
    hz: f64,
    rate_hz: f64,
    bandwidth_hz: Option<u64>,
    /// Half the occupied bandwidth.
    occupied_hz: f64,
}

impl Carrier {
    fn reach_from(&self, center: f64) -> f64 {
        (self.hz - center).abs() + self.rate_hz / 2.0
    }

    fn holds_dc(&self, center: f64) -> bool {
        (self.hz - center).abs() < self.occupied_hz + DC_GUARD_HZ
    }
}

/// A center left for the other direction to supply.
struct Unset;

fn center_key(direction: Direction) -> &'static str {
    match direction {
        Direction::Tx => "tx_center_hz",
        Direction::Rx => "rx_center_hz",
    }
}

struct Planner<'a> {
    request: &'a PlanRequest,
    findings: Vec<Finding>,
    settings: Vec<Setting>,
}

/// Chooses or checks a composite configuration. Problems are returned as findings, never
/// as an error, so every problem in a plan is reported at once.
pub fn plan(request: &PlanRequest) -> Plan {
    Planner {
        request,
        findings: Vec::new(),
        settings: Vec::new(),
    }
    .run()
}

impl Planner<'_> {
    fn run(mut self) -> Plan {
        let clients = &self.request.clients;
        let mut directions = Vec::new();
        if clients.is_empty() {
            self.error(
                "no clients to plan for",
                Some("add one --client per application, for example --client tx=881.52M,rx=836.52M,rate=4.9152M".into()),
            );
            return self.finish(None, directions);
        }
        self.check_clients();

        let tx = self.carriers(Direction::Tx);
        let rx = self.carriers(Direction::Rx);
        let tx_center = self.center(Direction::Tx, &tx);
        let rx_center = self.center(Direction::Rx, &rx);
        let (tx_center, rx_center) = match (tx_center, rx_center) {
            (Ok(tx), Ok(rx)) => (tx, rx),
            (Ok(tx), Err(Unset)) => (tx, self.follow(Direction::Rx, tx)),
            (Err(Unset), Ok(rx)) => (self.follow(Direction::Tx, rx), rx),
            _ => return self.finish(None, directions),
        };

        let needed_hz = span_needed(&tx, tx_center).max(span_needed(&rx, rx_center));
        let Some(device_rate_hz) = self.device_rate(needed_hz) else {
            return self.finish(None, directions);
        };
        let backoff_db = self.backoff(tx.len());

        let mut config = CompositeConfig {
            device_rate_hz,
            tx_center_hz: tx_center,
            rx_center_hz: rx_center,
            tx_digital_backoff_db: backoff_db,
            block_duration_ms: DEFAULT_BLOCK_DURATION_MS,
            tx_deadline_ms: DEFAULT_TX_DEADLINE_MS,
        };
        let tx_rows = self.place(&config, Direction::Tx, &tx);
        let rx_rows = self.place(&config, Direction::Rx, &rx);
        let (Some(mut tx_rows), Some(rx_rows)) = (tx_rows, rx_rows) else {
            return self.finish(None, directions);
        };

        config.block_duration_ms = self.block(&config, &tx_rows);
        config.tx_deadline_ms = self.deadline(config.block_duration_ms, &mut tx_rows);
        if let Err(error) = config.validate() {
            self.error(&error.to_string(), None);
        }
        self.check_overlaps(Direction::Tx, &tx);
        self.check_overlaps(Direction::Rx, &rx);
        self.check_images(Direction::Tx, &tx, tx_center);
        self.check_images(Direction::Rx, &rx, rx_center);

        for (direction, center, carriers, rows) in [
            (Direction::Tx, tx_center, &tx, tx_rows),
            (Direction::Rx, rx_center, &rx, rx_rows),
        ] {
            if carriers.is_empty() {
                continue;
            }
            directions.push(DirectionReport {
                direction,
                center_hz: center,
                analog_bandwidth_hz: analog_bandwidth(carriers, center),
                rows,
            });
        }
        let config =
            (self.findings.iter().all(|f| f.severity != Severity::Error)).then_some(config);
        self.finish(config, directions)
    }

    fn finish(self, config: Option<CompositeConfig>, directions: Vec<DirectionReport>) -> Plan {
        let mut findings = self.findings;
        findings.sort_by_key(|f| match f.severity {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Note => 2,
        });
        Plan {
            client_count: self.request.clients.len(),
            config,
            settings: self.settings,
            directions,
            findings,
        }
    }

    fn check_clients(&mut self) {
        for (index, client) in self.request.clients.iter().enumerate() {
            let label = client.label(index);
            if client.tx_hz.is_none() && client.lead_ms.is_some() {
                self.warning(
                    &format!("{label} has a lead but no TX carrier, so the lead is ignored"),
                    None,
                );
            }
            if let Some(bandwidth) = client.bandwidth_hz {
                let widest = client.rate_hz * DEFAULT_PASSBAND_FRACTION;
                if bandwidth as f64 > widest {
                    self.note(
                        &format!(
                            "{label} states {} of bandwidth, but its {} rate passes at most {}",
                            report::hz(bandwidth as f64),
                            report::rate(client.rate_hz),
                            report::hz(widest)
                        ),
                        Some("radiod filters the slice to that width".into()),
                    );
                }
            }
        }
    }

    fn carriers(&self, direction: Direction) -> Vec<Carrier> {
        self.request
            .clients
            .iter()
            .enumerate()
            .filter_map(|(index, client)| {
                let hz = match direction {
                    Direction::Tx => client.tx_hz,
                    Direction::Rx => client.rx_hz,
                }?;
                let occupied = client
                    .bandwidth_hz
                    .map(|bw| bw as f64)
                    .unwrap_or(client.rate_hz * DEFAULT_PASSBAND_FRACTION)
                    .min(client.rate_hz * DEFAULT_PASSBAND_FRACTION);
                Some(Carrier {
                    client: client.label(index),
                    hz: hz as f64,
                    rate_hz: client.rate_hz,
                    bandwidth_hz: client.bandwidth_hz,
                    occupied_hz: occupied / 2.0,
                })
            })
            .collect()
    }

    /// The center for `direction`, or `Err(Unset)` when nothing uses that direction.
    fn center(&mut self, direction: Direction, carriers: &[Carrier]) -> Result<u64, Unset> {
        let given = match direction {
            Direction::Tx => self.request.tx_center_hz,
            Direction::Rx => self.request.rx_center_hz,
        };
        let key = center_key(direction);
        if let Some(center) = given {
            if center == 0 {
                self.error(&format!("{key} must be above zero"), None);
                return Err(Unset);
            }
            self.check_dc(direction, carriers, center);
            self.setting(key, center.to_string(), "given".into());
            return Ok(center);
        }
        if carriers.is_empty() {
            return Err(Unset);
        }
        let (center, reason) = choose_center(carriers, self.request.allow_dc, direction.label());
        self.setting(key, center.to_string(), reason);
        Ok(center)
    }

    /// A center for a direction no client uses. The config still needs one.
    fn follow(&mut self, direction: Direction, other: u64) -> u64 {
        let other_label = match direction {
            Direction::Tx => "RX",
            Direction::Rx => "TX",
        };
        self.setting(
            center_key(direction),
            other.to_string(),
            format!(
                "no client uses {}, so it follows the {other_label} center",
                direction.label()
            ),
        );
        other
    }

    fn check_dc(&mut self, direction: Direction, carriers: &[Carrier], center: u64) {
        if self.request.allow_dc {
            return;
        }
        let Some(hit) = carriers.iter().find(|c| c.holds_dc(center as f64)) else {
            return;
        };
        let label = direction.label();
        let (better, _) = choose_center(carriers, false, label);
        self.warning(
            &format!(
                "the {label} center {} is inside {}'s band, so LO leakage lands on that carrier",
                report::hz(center as f64),
                hit.client
            ),
            Some(format!(
                "{} {label} center of {} keeps the leakage off every carrier. Pass --allow-dc to accept this",
                direction.article(),
                report::hz(better as f64)
            )),
        );
    }

    fn device_rate(&mut self, needed_hz: f64) -> Option<u64> {
        let needed = needed_hz.ceil() as u64;
        let rates: Vec<f64> = self.request.clients.iter().map(|c| c.rate_hz).collect();
        if let Some(rate) = self.request.device_rate_hz {
            if rate == 0 {
                self.error("device_rate_hz must be above zero", None);
                return None;
            }
            if let Some(clock) = self.request.device_clock_hz
                && !clock.is_multiple_of(rate)
            {
                self.error(
                    &format!(
                        "the {} device rate does not divide the {} device clock, so radiod refuses it at startup",
                        report::rate(rate as f64),
                        report::hz(clock as f64)
                    ),
                    None,
                );
            }
            self.setting("device_rate_hz", rate.to_string(), "given".into());
            return Some(rate);
        }
        let Some(max) = self.request.max_device_rate_hz else {
            self.error(
                "no device rate to work with",
                Some("pass --max-device-rate with the fastest rate the radio streams (30.72M for a PlutoSky on one channel), or fix one with --device-rate".into()),
            );
            return None;
        };
        if needed > max {
            self.error(
                &format!(
                    "the carriers need a device rate of at least {}, above the {} maximum",
                    report::rate(needed as f64),
                    report::rate(max as f64)
                ),
                Some(
                    "move the carriers closer together, or use a radio that streams faster".into(),
                ),
            );
            return None;
        }
        match choose_device_rate(needed, max, &rates, self.request.device_clock_hz) {
            Some((rate, reason)) => {
                self.setting("device_rate_hz", rate.to_string(), reason);
                Some(rate)
            }
            None => {
                let clock = self
                    .request
                    .device_clock_hz
                    .map(|clock| format!(" that divides the {} clock", report::hz(clock as f64)))
                    .unwrap_or_default();
                self.error(
                    &format!(
                        "no device rate between {} and {}{clock} converts to every client rate",
                        report::rate(needed as f64),
                        report::rate(max as f64)
                    ),
                    Some(format!(
                        "radiod converts with ratios whose terms are at most {}. Client rates with a common multiple in range work best",
                        super::MAX_RATE_RATIO
                    )),
                );
                None
            }
        }
    }

    fn backoff(&mut self, tx_carriers: usize) -> f64 {
        let safe = safe_backoff_db(tx_carriers);
        let Some(given) = self.request.tx_digital_backoff_db else {
            let reason = match tx_carriers {
                0 => "no TX carriers".to_string(),
                1 => "one TX carrier, plus filter overshoot".to_string(),
                n => format!("{n} TX carriers at full scale never clip, even peaking together"),
            };
            self.setting("tx_digital_backoff_db", format!("{safe}"), reason);
            return safe;
        };
        self.setting("tx_digital_backoff_db", format!("{given}"), "given".into());
        if given > 0.0 {
            self.error("tx_digital_backoff_db must be zero or below", None);
        } else if tx_carriers > 1 && given > safe {
            let peak = tx_carriers as f64 * 10f64.powf(given / 20.0);
            self.warning(
                &format!(
                    "with {tx_carriers} TX carriers, {given} dB lets their sum reach {peak:.2}x full scale, so it can clip when they peak together"
                ),
                Some(format!("{safe} dB never clips")),
            );
        }
        given
    }

    /// Places every carrier with radiod's own slice planner.
    fn place(
        &mut self,
        config: &CompositeConfig,
        direction: Direction,
        carriers: &[Carrier],
    ) -> Option<Vec<SliceRow>> {
        let center = match direction {
            Direction::Tx => config.tx_center_hz,
            Direction::Rx => config.rx_center_hz,
        } as f64;
        let half_span = config.device_rate_hz as f64 / 2.0;
        let mut rows = Vec::new();
        let mut placed = true;
        let label = direction.label();
        for carrier in carriers {
            let reach = carrier.reach_from(center);
            if reach > half_span {
                placed = false;
                self.error(
                    &format!(
                        "{} {label} at {} does not fit. Its band reaches {} from the {label} center at {}, but a {} device rate covers {} each side",
                        carrier.client,
                        report::hz(carrier.hz),
                        report::hz(reach),
                        report::hz(center),
                        report::rate(config.device_rate_hz as f64),
                        report::hz(half_span)
                    ),
                    Some(fit_help(carriers, config.device_rate_hz, direction, reach)),
                );
                continue;
            }
            match config.plan_slice(
                direction,
                carrier.rate_hz,
                carrier.hz as u64,
                carrier.bandwidth_hz,
            ) {
                Ok(slice) => rows.push(SliceRow {
                    client: carrier.client.clone(),
                    carrier_hz: carrier.hz as u64,
                    offset_hz: slice.offset_hz,
                    rate_hz: config.client_rate_hz(slice.ratio, slice.denominator),
                    ratio: slice.ratio,
                    denominator: slice.denominator,
                    edge_margin_hz: half_span - reach,
                    lead: None,
                }),
                Err(error) => {
                    placed = false;
                    self.error(
                        &format!(
                            "{}'s {} rate cannot be served from a {} device rate",
                            carrier.client,
                            report::rate(carrier.rate_hz),
                            report::rate(config.device_rate_hz as f64)
                        ),
                        Some(format!("radiod says: {error}")),
                    );
                }
            }
        }
        placed.then_some(rows)
    }

    fn block(&mut self, config: &CompositeConfig, tx_rows: &[SliceRow]) -> f64 {
        let rate = config.device_rate_hz;
        let step = tx_rows.iter().fold(1u64, |step, row| {
            let (ratio, denominator) = (row.ratio as u64, row.denominator as u64);
            lcm(step, ratio / gcd(ratio, denominator))
        });
        let ms_for = |samples: u64| samples as f64 * 1000.0 / rate as f64;
        if let Some(given) = self.request.block_duration_ms {
            self.setting("block_duration_ms", format!("{given}"), "given".into());
            let probe = CompositeConfig {
                block_duration_ms: given,
                ..*config
            };
            match probe.grid_block_samples() {
                Ok(samples) if !(samples as u64).is_multiple_of(step) => {
                    let above = (samples as u64).div_ceil(step) * step;
                    let below = samples as u64 / step * step;
                    let choices = if below > 0 {
                        format!("{} ms or {} ms", ms_for(below), ms_for(above))
                    } else {
                        format!("{} ms", ms_for(above))
                    };
                    self.warning(
                        &format!(
                            "a {} block is {samples} device samples, which splits some client samples across blocks",
                            report::ms(given)
                        ),
                        Some(format!("{choices} gives whole samples for every client")),
                    );
                }
                Ok(_) => {}
                Err(error) => self.error(&error.to_string(), None),
            }
            return given;
        }
        let batch = self.smallest_batch();
        let wanted = batch.as_ref().map_or(TARGET_BLOCK_MS, |(_, ms)| *ms);
        let mut block = whole_sample_block(rate, step, wanted, Fit::Nearest).map(|block| {
            let reason = match &batch {
                Some((client, ms)) => format!("{client}'s {} batch", report::ms(*ms)),
                None => "whole client samples in every block".to_string(),
            };
            (block, reason)
        });
        let tightest = self.tightest_lead();
        let room = tightest
            .as_ref()
            .map(|(_, lead)| lead - self.request.margin_ms - MIN_TX_DEADLINE_MS);
        if let (Some((client, lead)), Some(room)) = (&tightest, room)
            && room > 0.0
            && block
                .as_ref()
                .is_none_or(|((ms, _), _)| *ms > room + TIMING_EPSILON_MS)
            && let Some(shorter) = whole_sample_block(rate, step, room, Fit::AtMost)
        {
            block = Some((
                shorter,
                format!("short enough for {client}'s {} lead", report::ms(*lead)),
            ));
        }
        if let Some(((ms, samples), reason)) = block {
            self.setting(
                "block_duration_ms",
                format!("{ms}"),
                format!("{samples} device samples, {reason}"),
            );
            return ms;
        }
        let samples = ((TARGET_BLOCK_MS * rate as f64 / 1000.0).round() as u64).max(1);
        let ms = ms_for(samples);
        self.setting(
            "block_duration_ms",
            format!("{ms}"),
            format!("{samples} device samples"),
        );
        self.note(
            &format!(
                "whole client samples per block need {} blocks, so blocks split some client samples",
                report::ms(ms_for(step))
            ),
            Some("radiod handles this, and logs a warning when the client connects".into()),
        );
        ms
    }

    fn smallest_batch(&self) -> Option<(String, f64)> {
        self.request
            .clients
            .iter()
            .enumerate()
            .filter_map(|(index, c)| c.batch_ms.map(|batch| (c.label(index), batch)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// The transmitting client with the shortest lead, by label.
    fn tightest_lead(&self) -> Option<(String, f64)> {
        self.request
            .clients
            .iter()
            .enumerate()
            .filter(|(_, c)| c.tx_hz.is_some())
            .filter_map(|(index, c)| c.lead_ms.map(|lead| (c.label(index), lead)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    fn deadline(&mut self, block_ms: f64, tx_rows: &mut [SliceRow]) -> f64 {
        let margin = self.request.margin_ms;
        let leads: Vec<(String, f64)> = self
            .request
            .clients
            .iter()
            .enumerate()
            .filter(|(_, c)| c.tx_hz.is_some())
            .filter_map(|(index, c)| c.lead_ms.map(|lead| (c.label(index), lead)))
            .collect();
        let tightest = leads.iter().min_by(|a, b| a.1.total_cmp(&b.1)).cloned();
        let deadline = match (self.request.tx_deadline_ms, &tightest) {
            (Some(given), _) => {
                self.setting("tx_deadline_ms", format!("{given}"), "given".into());
                if given < MIN_TX_DEADLINE_MS {
                    self.warning(
                        &format!(
                            "a {} deadline is shorter than radiod's {} commit cycle, so a block can reach the device late",
                            report::ms(given),
                            report::ms(MIN_TX_DEADLINE_MS)
                        ),
                        None,
                    );
                }
                given
            }
            (None, None) => {
                self.setting(
                    "tx_deadline_ms",
                    format!("{DEFAULT_TX_DEADLINE_MS}"),
                    "radiod's default. Give clients a lead= to fit it to them".into(),
                );
                DEFAULT_TX_DEADLINE_MS
            }
            (None, Some((client, lead))) => {
                let room = round_down(lead - block_ms - margin, TIMING_STEP_MS);
                if room < MIN_TX_DEADLINE_MS {
                    let needed = MIN_TX_DEADLINE_MS + block_ms + margin;
                    self.error(
                        &format!(
                            "{client}'s {} lead is too short. It needs at least {}: a {} deadline, a {} block and a {} margin",
                            report::ms(*lead),
                            report::ms(needed),
                            report::ms(MIN_TX_DEADLINE_MS),
                            report::ms(block_ms),
                            report::ms(margin)
                        ),
                        Some("raise the client's TX lookahead, or pass a shorter --block or --margin".into()),
                    );
                    MIN_TX_DEADLINE_MS
                } else {
                    let chosen = room.min(DEFAULT_TX_DEADLINE_MS);
                    let reason = if chosen < room {
                        format!(
                            "radiod's default, which {client}'s {} lead clears",
                            report::ms(*lead)
                        )
                    } else {
                        format!(
                            "{client}'s {} lead, less the {} block and {} margin",
                            report::ms(*lead),
                            report::ms(block_ms),
                            report::ms(margin)
                        )
                    };
                    self.setting("tx_deadline_ms", format!("{chosen}"), reason);
                    chosen
                }
            }
        };
        let needed = deadline + block_ms + margin;
        for row in tx_rows.iter_mut() {
            let lead = leads
                .iter()
                .find(|(name, _)| *name == row.client)
                .map(|l| l.1);
            row.lead = Some(match lead {
                Some(lead_ms) => Lead::Given {
                    lead_ms,
                    spare_ms: lead_ms - needed,
                },
                None => Lead::Needs { min_ms: needed },
            });
            if let Some(lead) = lead
                && lead < needed
                && self.request.tx_deadline_ms.is_some()
            {
                self.error(
                    &format!(
                        "{}'s {} lead is too short for this plan, which needs {}: the {} deadline, a {} block and a {} margin",
                        row.client,
                        report::ms(lead),
                        report::ms(needed),
                        report::ms(deadline),
                        report::ms(block_ms),
                        report::ms(margin)
                    ),
                    Some(lead_help(lead, block_ms, margin)),
                );
            }
        }
        deadline
    }

    fn check_overlaps(&mut self, direction: Direction, carriers: &[Carrier]) {
        for (i, a) in carriers.iter().enumerate() {
            for b in carriers.iter().skip(i + 1) {
                let overlap = a.occupied_hz + b.occupied_hz - (a.hz - b.hz).abs();
                if overlap <= 0.0 {
                    continue;
                }
                let assumed = if a.bandwidth_hz.is_none() || b.bandwidth_hz.is_none() {
                    " Without bw=, a client is assumed to fill 80% of its sample rate."
                } else {
                    ""
                };
                self.warning(
                    &format!(
                        "{} and {} overlap by {} on {}, so they interfere with each other",
                        a.client,
                        b.client,
                        report::hz(overlap),
                        direction.label()
                    ),
                    Some(format!("space the carriers further apart, or give each its real bandwidth.{assumed}")),
                );
            }
        }
    }

    fn check_images(&mut self, direction: Direction, carriers: &[Carrier], center: u64) {
        let center = center as f64;
        for (i, a) in carriers.iter().enumerate() {
            for b in carriers.iter().skip(i + 1) {
                let image = 2.0 * center - a.hz;
                if (image - b.hz).abs() < a.occupied_hz + b.occupied_hz {
                    self.note(
                        &format!(
                            "{} sits opposite {} about the {} center, so each carrier's IQ image lands on the other",
                            a.client,
                            b.client,
                            direction.label()
                        ),
                        Some("the image is as strong as the radio's IQ balance allows. An off-center LO avoids it at the cost of a wider span".into()),
                    );
                }
            }
        }
    }

    fn setting(&mut self, key: &'static str, value: String, reason: String) {
        self.settings.push(Setting { key, value, reason });
    }

    fn error(&mut self, message: &str, help: Option<String>) {
        self.push(Severity::Error, message, help);
    }

    fn warning(&mut self, message: &str, help: Option<String>) {
        self.push(Severity::Warning, message, help);
    }

    fn note(&mut self, message: &str, help: Option<String>) {
        self.push(Severity::Note, message, help);
    }

    fn push(&mut self, severity: Severity, message: &str, help: Option<String>) {
        self.findings.push(Finding {
            severity,
            message: message.to_string(),
            help,
        });
    }
}

fn lead_help(lead: f64, block_ms: f64, margin: f64) -> String {
    let room = round_down(lead - block_ms - margin, TIMING_STEP_MS);
    if room >= MIN_TX_DEADLINE_MS {
        return format!("set tx_deadline_ms to {room} or less");
    }
    let longest_block = round_down(lead - margin - MIN_TX_DEADLINE_MS, TIMING_STEP_MS);
    if longest_block > 0.0 {
        format!(
            "a {} lead fits a block of {} or less with a {} deadline. Otherwise raise the client's TX lookahead",
            report::ms(lead),
            report::ms(longest_block),
            report::ms(MIN_TX_DEADLINE_MS)
        )
    } else {
        "raise the client's TX lookahead".into()
    }
}

/// Explains the fix for a carrier that falls outside the span.
fn fit_help(carriers: &[Carrier], device_rate_hz: u64, direction: Direction, reach: f64) -> String {
    let half_span = device_rate_hz as f64 / 2.0;
    // Every carrier fits for centers in [hz + rate/2 - half_span, hz - rate/2 + half_span].
    let low = carriers
        .iter()
        .map(|c| c.hz + c.rate_hz / 2.0 - half_span)
        .fold(f64::MIN, f64::max);
    let high = carriers
        .iter()
        .map(|c| c.hz - c.rate_hz / 2.0 + half_span)
        .fold(f64::MAX, f64::min);
    let label = direction.label();
    if low <= high {
        let center = round_to_step((low + high) / 2.0);
        format!(
            "{} {label} center of {} fits every {label} carrier at this device rate",
            direction.article(),
            report::hz(center as f64)
        )
    } else {
        format!(
            "this center needs a device rate of at least {}",
            report::rate(2.0 * reach)
        )
    }
}

/// Picks the center that needs the smallest span, keeping the LO off every carrier.
fn choose_center(carriers: &[Carrier], allow_dc: bool, label: &str) -> (u64, String) {
    let low = carriers
        .iter()
        .map(|c| c.hz - c.rate_hz / 2.0)
        .fold(f64::MAX, f64::min);
    let high = carriers
        .iter()
        .map(|c| c.hz + c.rate_hz / 2.0)
        .fold(f64::MIN, f64::max);
    let middle = round_to_step((low + high) / 2.0);
    let holds_dc = |center: u64| carriers.iter().find(|c| c.holds_dc(center as f64));
    let Some(blocking) = (!allow_dc).then(|| holds_dc(middle)).flatten() else {
        let reason = if carriers.len() == 1 {
            format!("on the {label} carrier")
        } else {
            format!("midway between the outermost {label} carriers")
        };
        return (middle, reason);
    };
    let step = CENTER_STEP_HZ as f64;
    let best = carriers
        .iter()
        .flat_map(|c| {
            let edge = c.occupied_hz + DC_GUARD_HZ;
            [
                ((c.hz - edge) / step).floor() * step,
                ((c.hz + edge) / step).ceil() * step,
            ]
        })
        .map(|center| center as u64)
        .filter(|center| holds_dc(*center).is_none())
        .min_by(|a, b| {
            span_needed(carriers, *a)
                .total_cmp(&span_needed(carriers, *b))
                .then(
                    (*a as f64 - middle as f64)
                        .abs()
                        .total_cmp(&(*b as f64 - middle as f64).abs()),
                )
        })
        .unwrap_or(middle);
    let reason = format!(
        "moved {} off {}'s band so LO leakage misses every carrier",
        report::hz((best as f64 - middle as f64).abs()),
        blocking.client
    );
    (best, reason)
}

/// Smallest device rate whose span covers every carrier about `center`.
fn span_needed(carriers: &[Carrier], center: u64) -> f64 {
    carriers
        .iter()
        .map(|c| 2.0 * c.reach_from(center as f64))
        .fold(0.0, f64::max)
}

fn analog_bandwidth(carriers: &[Carrier], center: u64) -> u64 {
    carriers
        .iter()
        .map(|c| 2.0 * ((c.hz - center as f64).abs() + c.occupied_hz))
        .fold(0.0, f64::max)
        .ceil() as u64
}

/// The smallest usable device rate in `[needed, max]`, preferring a whole multiple of
/// every client rate, which keeps every slice an integer resampler.
fn choose_device_rate(
    needed: u64,
    max: u64,
    client_rates: &[f64],
    clock: Option<u64>,
) -> Option<(u64, String)> {
    let usable = |rate: u64| {
        clock.is_none_or(|clock| clock.is_multiple_of(rate))
            && client_rates
                .iter()
                .all(|client| probe(rate).rate_terms(*client).is_ok())
    };
    let whole_rates: Option<Vec<u64>> = client_rates
        .iter()
        .map(|rate| (rate.fract() == 0.0).then_some(*rate as u64))
        .collect();
    let common = whole_rates.and_then(|rates| {
        rates.iter().try_fold(1u64, |acc, rate| {
            Some(acc / gcd(acc, *rate)).and_then(|q| q.checked_mul(*rate))
        })
    });
    if let Some(common) = common.filter(|common| *common <= max) {
        let rate = needed.div_ceil(common).max(1) * common;
        if rate <= max && usable(rate) {
            let reason = if client_rates.len() == 1 {
                "the smallest whole multiple of the client rate that covers the carriers"
            } else {
                "the smallest whole multiple of every client rate that covers the carriers"
            };
            return Some((rate, reason.into()));
        }
    }
    let reason = "the smallest rate that covers the carriers and converts to every client rate";
    if let Some(clock) = clock {
        let fewest = clock.div_ceil(max).max(1);
        let most = clock / needed.max(1);
        return (fewest..=most)
            .rev()
            .filter(|divisor| clock.is_multiple_of(*divisor))
            .map(|divisor| clock / divisor)
            .find(|rate| usable(*rate))
            .map(|rate| (rate, format!("{reason}, dividing the device clock")));
    }
    let start = needed.div_ceil(RATE_SEARCH_STEP_HZ) * RATE_SEARCH_STEP_HZ;
    (0..MAX_RATE_SEARCH_STEPS)
        .map(|step| start + step * RATE_SEARCH_STEP_HZ)
        .take_while(|rate| *rate <= max)
        .find(|rate| usable(*rate))
        .map(|rate| (rate, reason.into()))
}

#[derive(Clone, Copy, PartialEq)]
enum Fit {
    /// The whole-sample block closest to the target, on either side.
    Nearest,
    /// The longest whole-sample block no longer than the target.
    AtMost,
}

/// A block near `target_ms` that spans whole client samples once radiod rounds it to
/// device samples, written with the fewest decimal places that round to it. Returns
/// the duration and its device sample count.
fn whole_sample_block(rate: u64, step: u64, target_ms: f64, fit: Fit) -> Option<(f64, u64)> {
    let samples_for = |ms: f64| (rate as f64 * ms / 1000.0).round() as u64;
    let written = |samples: u64| {
        (0..=BLOCK_MS_PLACES).find_map(|places| {
            let scale = 10f64.powi(places as i32);
            let ms = (samples as f64 * 1000.0 / rate as f64 * scale).round() / scale;
            (ms > 0.0 && samples_for(ms) == samples).then_some((places, ms))
        })
    };
    let longest = samples_for(MAX_WHOLE_SAMPLE_BLOCK_MS) / step;
    let high = match fit {
        Fit::Nearest => samples_for(target_ms).div_ceil(step),
        Fit::AtMost => samples_for(target_ms) / step,
    }
    .min(longest);
    let low = (samples_for(target_ms) / step).max(1);
    (low..=high)
        .filter_map(|k| written(k * step).map(|(places, ms)| (places, ms, k * step)))
        .filter(|(_, ms, _)| fit == Fit::Nearest || *ms <= target_ms + TIMING_EPSILON_MS)
        .min_by(|a, b| {
            (target_ms - a.1)
                .abs()
                .total_cmp(&(target_ms - b.1).abs())
                .then(a.0.cmp(&b.0))
        })
        .map(|(_, ms, samples)| (ms, samples))
}

fn probe(device_rate_hz: u64) -> CompositeConfig {
    CompositeConfig {
        device_rate_hz,
        tx_center_hz: 1,
        rx_center_hz: 1,
        tx_digital_backoff_db: 0.0,
        block_duration_ms: DEFAULT_BLOCK_DURATION_MS,
        tx_deadline_ms: DEFAULT_TX_DEADLINE_MS,
    }
}

/// Backoff that keeps `carriers` full-scale carriers from clipping when they peak together.
fn safe_backoff_db(carriers: usize) -> f64 {
    if carriers == 0 {
        return 0.0;
    }
    let db = 20.0 * (carriers as f64).log10() + FILTER_OVERSHOOT_DB;
    -(db * 10.0).ceil() / 10.0
}

fn lcm(a: u64, b: u64) -> u64 {
    a / gcd(a, b) * b
}

fn round_to_step(hz: f64) -> u64 {
    ((hz / CENTER_STEP_HZ as f64).round() as u64) * CENTER_STEP_HZ
}

fn round_down(value: f64, step: f64) -> f64 {
    ((value / step) + 1e-9).floor() * step
}
