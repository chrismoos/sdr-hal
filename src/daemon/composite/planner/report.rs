//! Text rendering of a composite plan.

use std::fmt::Write as _;

use super::{Lead, Plan, Severity, SliceRow};
use crate::daemon::composite::{CompositeConfig, Direction};

const SETTING_ORDER: [&str; 6] = [
    "device_rate_hz",
    "tx_center_hz",
    "rx_center_hz",
    "tx_digital_backoff_db",
    "block_duration_ms",
    "tx_deadline_ms",
];

/// Renders a plan for a terminal: settings, one table per direction, findings, and the
/// `composite` block when the plan has no errors.
pub fn render(plan: &Plan) -> String {
    let mut out = String::new();
    let clients = match plan.client_count {
        1 => "1 client".to_string(),
        n => format!("{n} clients"),
    };
    let _ = writeln!(out, "Composite plan for {clients}");

    if !plan.settings.is_empty() {
        out.push('\n');
        let mut settings: Vec<_> = plan.settings.iter().collect();
        settings.sort_by_key(|s| SETTING_ORDER.iter().position(|key| *key == s.key));
        let rows: Vec<[String; 3]> = settings
            .iter()
            .map(|s| {
                [
                    s.key.to_string(),
                    display_value(s.key, &s.value),
                    s.reason.clone(),
                ]
            })
            .collect();
        write_table(&mut out, None, &rows);
    }

    for direction in &plan.directions {
        let label = direction.direction.label();
        let _ = writeln!(out, "\n{label} at {}", hz(direction.center_hz as f64));
        let header = [
            "client",
            "carrier",
            "offset",
            "rate",
            "ratio",
            "edge margin",
            "lead",
        ]
        .map(String::from);
        let rows: Vec<[String; 7]> = direction.rows.iter().map(slice_cells).collect();
        let lead_column = direction.direction == Direction::Tx;
        if lead_column {
            write_table(&mut out, Some(&header), &rows);
        } else {
            let header: [String; 6] = std::array::from_fn(|i| header[i].clone());
            let rows: Vec<[String; 6]> = rows
                .iter()
                .map(|row| std::array::from_fn(|i| row[i].clone()))
                .collect();
            write_table(&mut out, Some(&header), &rows);
        }
        let key = match direction.direction {
            Direction::Tx => "tx_bandwidth_hz",
            Direction::Rx => "rx_bandwidth_hz",
        };
        let _ = writeln!(
            out,
            "  Radio file: {key} of at least {} ({}) passes every carrier. Unset, it follows the device rate.",
            direction.analog_bandwidth_hz,
            hz(direction.analog_bandwidth_hz as f64)
        );
    }

    if !plan.findings.is_empty() {
        out.push('\n');
    }
    for finding in &plan.findings {
        let tag = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        };
        let _ = writeln!(out, "{tag}: {}", finding.message);
        if let Some(help) = &finding.help {
            let _ = writeln!(out, "  help: {help}");
        }
    }

    out.push('\n');
    match (&plan.config, plan.error_count()) {
        (Some(config), _) => {
            let _ = writeln!(out, "Listener file `composite` block:");
            let _ = writeln!(out, "{}", composite_json(config, "  "));
        }
        (None, errors) => {
            let warnings = plan.warning_count();
            let _ = write!(out, "{}", count(errors, "error"));
            if warnings > 0 {
                let _ = write!(out, ", {}", count(warnings, "warning"));
            }
            let _ = writeln!(out, ". No configuration produced.");
        }
    }
    out
}

/// The `composite` object as indented JSON, in the order the README lists the keys.
pub fn composite_json(config: &CompositeConfig, indent: &str) -> String {
    let number = |value: f64| serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    let fields = [
        ("device_rate_hz", config.device_rate_hz.to_string()),
        ("tx_center_hz", config.tx_center_hz.to_string()),
        ("rx_center_hz", config.rx_center_hz.to_string()),
        (
            "tx_digital_backoff_db",
            number(config.tx_digital_backoff_db),
        ),
        ("block_duration_ms", number(config.block_duration_ms)),
        ("tx_deadline_ms", number(config.tx_deadline_ms)),
    ];
    let body: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{indent}\"{key}\": {value}"))
        .collect();
    format!("{{\n{}\n}}", body.join(",\n"))
}

fn slice_cells(row: &SliceRow) -> [String; 7] {
    let ratio = if row.denominator == 1 {
        row.ratio.to_string()
    } else {
        format!("{}/{}", row.ratio, row.denominator)
    };
    let lead = match &row.lead {
        None => String::new(),
        Some(Lead::Given { lead_ms, spare_ms }) if *spare_ms >= 0.0 => {
            format!("{} ({} spare)", ms(*lead_ms), ms(*spare_ms))
        }
        Some(Lead::Given { lead_ms, spare_ms }) => {
            format!("{} ({} short)", ms(*lead_ms), ms(-spare_ms))
        }
        Some(Lead::Needs { min_ms }) => format!("needs {}", ms(*min_ms)),
    };
    [
        row.client.clone(),
        hz(row.carrier_hz as f64),
        signed_hz(row.offset_hz as f64),
        rate(row.rate_hz),
        ratio,
        hz(row.edge_margin_hz),
        lead,
    ]
}

fn write_table<const N: usize>(
    out: &mut String,
    header: Option<&[String; N]>,
    rows: &[[String; N]],
) {
    let mut widths = [0usize; N];
    for row in header.into_iter().chain(rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut line = |row: &[String; N]| {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        let _ = writeln!(out, "  {}", cells.join("   ").trim_end());
    };
    if let Some(header) = header {
        line(&header.clone().map(|cell| cell.to_uppercase()));
    }
    for row in rows {
        line(row);
    }
}

fn display_value(key: &str, value: &str) -> String {
    match (key, value.parse::<f64>()) {
        ("device_rate_hz", Ok(v)) => format!("{value} ({})", rate(v)),
        ("tx_center_hz" | "rx_center_hz", Ok(v)) => format!("{value} ({})", hz(v)),
        ("tx_digital_backoff_db", Ok(v)) => format!("{v} dB"),
        ("block_duration_ms" | "tx_deadline_ms", Ok(v)) => ms(v),
        _ => value.to_string(),
    }
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// A frequency with a unit, such as `882.75 MHz` or `625 kHz`.
pub(super) fn hz(value: f64) -> String {
    scaled(value, &[(1e9, "GHz"), (1e6, "MHz"), (1e3, "kHz")], "Hz")
}

fn signed_hz(value: f64) -> String {
    if value > 0.0 {
        format!("+{}", hz(value))
    } else {
        hz(value)
    }
}

/// A sample rate with a unit, such as `4.9152 MS/s`.
pub(super) fn rate(value: f64) -> String {
    scaled(value, &[(1e6, "MS/s"), (1e3, "kS/s")], "S/s")
}

/// A duration in milliseconds, to the microsecond.
pub(super) fn ms(value: f64) -> String {
    format!("{} ms", trim(&format!("{value:.3}")))
}

fn scaled(value: f64, units: &[(f64, &str)], base: &str) -> String {
    let (divisor, unit) = units
        .iter()
        .find(|(divisor, _)| value.abs() >= *divisor)
        .copied()
        .unwrap_or((1.0, base));
    // Enough places to show whole Hz in the chosen unit.
    let places = (divisor.log10().round() as usize).min(9);
    format!("{} {unit}", trim(&format!("{:.places$}", value / divisor)))
}

fn trim(number: &str) -> String {
    if !number.contains('.') {
        return number.to_string();
    }
    number
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}
