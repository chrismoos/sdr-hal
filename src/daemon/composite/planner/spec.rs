//! Client and value parsing for the composite planner.

use std::str::FromStr;

use crate::error::Error;

const VALID_KEYS: &str = "tx, rx, rate, bw, lead, batch and name";
const CLIENT_EXAMPLE: &str = "tx=881.52M,rx=836.52M,rate=4.9152M";

/// One client the plan has to serve.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientSpec {
    /// Label used in the report. Clients without one are numbered.
    pub name: Option<String>,
    /// TX carrier in Hz. `None` for a client that only receives.
    pub tx_hz: Option<u64>,
    /// RX carrier in Hz. `None` for a client that only transmits.
    pub rx_hz: Option<u64>,
    /// The client's sample rate in Hz.
    pub rate_hz: f64,
    /// Occupied bandwidth in Hz. When omitted, 80% of the sample rate is assumed.
    pub bandwidth_hz: Option<u64>,
    /// How far ahead of air time the client's TX samples reach radiod, in milliseconds.
    pub lead_ms: Option<f64>,
    /// How much air time the client sends or reads per call, in milliseconds.
    pub batch_ms: Option<f64>,
}

impl ClientSpec {
    pub(super) fn label(&self, index: usize) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("client {}", index + 1))
    }
}

impl FromStr for ClientSpec {
    type Err = Error;

    /// Parses `tx=881.52M,rx=836.52M,rate=4.9152M,bw=1.25M,lead=3ms,batch=5ms,name=1xbts`.
    fn from_str(text: &str) -> Result<Self, Error> {
        let mut name = None;
        let mut tx_hz = None;
        let mut rx_hz = None;
        let mut rate_hz = None;
        let mut bandwidth_hz = None;
        let mut lead_ms = None;
        let mut batch_ms = None;
        let mut seen: Vec<String> = Vec::new();
        for part in text
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let Some((key, value)) = part.split_once('=') else {
                return Err(format!(
                    "'{part}' has no value. Each part is key=value, for example {CLIENT_EXAMPLE}"
                )
                .into());
            };
            let key = match key.trim().to_ascii_lowercase().as_str() {
                "bandwidth" => "bw".to_string(),
                other => other.to_string(),
            };
            if seen.contains(&key) {
                return Err(format!("{key}= is given twice").into());
            }
            let value = value.trim();
            match key.as_str() {
                "tx" => tx_hz = Some(parse_hz(value).map_err(|e| in_key("tx", e))?),
                "rx" => rx_hz = Some(parse_hz(value).map_err(|e| in_key("rx", e))?),
                "rate" => rate_hz = Some(parse_rate_hz(value).map_err(|e| in_key("rate", e))?),
                "bw" => bandwidth_hz = Some(parse_hz(value).map_err(|e| in_key("bw", e))?),
                "lead" => lead_ms = Some(parse_ms(value).map_err(|e| in_key("lead", e))?),
                "batch" => match parse_ms(value).map_err(|e| in_key("batch", e))? {
                    0.0 => return Err("batch= must be above zero".into()),
                    batch => batch_ms = Some(batch),
                },
                "name" if value.is_empty() => return Err("name= needs a value".into()),
                "name" => name = Some(value.to_string()),
                other => {
                    return Err(
                        format!("unknown key '{other}'. Valid keys are {VALID_KEYS}").into(),
                    );
                }
            }
            seen.push(key);
        }
        let Some(rate_hz) = rate_hz else {
            return Err(format!(
                "rate= is missing. Every client needs its sample rate, for example {CLIENT_EXAMPLE}"
            )
            .into());
        };
        if tx_hz.is_none() && rx_hz.is_none() {
            return Err(format!(
                "the client has no carrier. Give tx=, rx= or both, for example {CLIENT_EXAMPLE}"
            )
            .into());
        }
        Ok(Self {
            name,
            tx_hz,
            rx_hz,
            rate_hz,
            bandwidth_hz,
            lead_ms,
            batch_ms,
        })
    }
}

fn in_key(key: &str, error: Error) -> Error {
    format!("{key}: {error}").into()
}

/// Parses a whole-Hz frequency such as `881.52M`, `500k`, `1.25MHz` or `881520000`.
pub fn parse_hz(text: &str) -> Result<u64, Error> {
    let (numerator, scale) = parse_scaled(text, &["hz"])
        .ok_or_else(|| not_a("frequency", text, "881.52M or 881520000"))?;
    if numerator % scale != 0 {
        return Err(format!("'{text}' is not a whole number of Hz").into());
    }
    u64::try_from(numerator / scale).map_err(|_| format!("'{text}' is too large").into())
}

/// Parses a sample rate such as `4.9152M`, `30.72MS/s` or `64000`. Fractional Hz are kept.
pub fn parse_rate_hz(text: &str) -> Result<f64, Error> {
    let (numerator, scale) = parse_scaled(text, &["s/s", "sps", "hz"])
        .ok_or_else(|| not_a("sample rate", text, "4.9152M or 4915200"))?;
    if numerator == 0 {
        return Err("a sample rate must be above zero".into());
    }
    Ok(numerator as f64 / scale as f64)
}

/// Parses a duration in milliseconds such as `3`, `3ms`, `0.5ms` or `500us`.
pub fn parse_ms(text: &str) -> Result<f64, Error> {
    let lower = text.trim().to_ascii_lowercase();
    let (number, per_ms) = if let Some(number) = lower.strip_suffix("us") {
        (number, 1000.0)
    } else if let Some(number) = lower.strip_suffix("ms") {
        (number, 1.0)
    } else {
        (lower.as_str(), 1.0)
    };
    match number.trim().parse::<f64>() {
        Ok(value) if value.is_finite() && value >= 0.0 => Ok(value / per_ms),
        Ok(_) => Err(format!("'{text}' must be zero or more").into()),
        Err(_) => Err(not_a("duration", text, "3ms, 0.5ms or 500us")),
    }
}

fn not_a(what: &str, text: &str, example: &str) -> Error {
    format!("'{text}' is not a {what}. Write it like {example}").into()
}

/// Splits `881.52M` into an exact `(881_520_000_00, 100)` so no rounding happens.
fn parse_scaled(text: &str, units: &[&str]) -> Option<(u128, u128)> {
    let mut rest = text.trim().to_ascii_lowercase();
    for unit in units {
        if let Some(stripped) = rest.strip_suffix(unit) {
            rest = stripped.trim_end().to_string();
            break;
        }
    }
    let multiplier: u128 = match rest.chars().last()? {
        'k' => 1_000,
        'm' => 1_000_000,
        'g' => 1_000_000_000,
        _ => 1,
    };
    if multiplier != 1 {
        rest.pop();
    }
    let (whole, fraction) = rest.trim().split_once('.').unwrap_or((rest.trim(), ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole
        .chars()
        .chain(fraction.chars())
        .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let scale = 10u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let digits = format!("{whole}{fraction}");
    let numerator = digits.parse::<u128>().ok()?.checked_mul(multiplier)?;
    Some((numerator, scale))
}
