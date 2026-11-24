//! `sdr-radiod plan`: composite configuration calculator.

use std::path::{Path, PathBuf};

use sdr_hal::daemon::RadiodConfig;
use sdr_hal::daemon::composite::planner::{
    self, ClientSpec, DEFAULT_MARGIN_MS, PlanRequest, parse_hz, parse_ms,
};
use sdr_hal::error::Error;

const CLIENT_HELP: &str = "One client as key=value pairs: tx=FREQ, rx=FREQ, rate=RATE, and optionally bw=FREQ, lead=TIME, batch=TIME and name=TEXT. Repeat for each client. Example: tx=881.52M,rx=836.52M,rate=4.9152M,lead=3ms,name=1xbts";

#[derive(clap::Args)]
#[command(after_help = "Frequencies take k, M or G suffixes (881.52M). Times take ms or us (3ms).")]
pub struct PlanArgs {
    #[arg(long = "client", value_name = "SPEC", required = true, help = CLIENT_HELP)]
    clients: Vec<ClientSpec>,

    /// Fastest rate the radio streams, such as 30.72M for a PlutoSky on one channel
    #[arg(long, value_name = "RATE", value_parser = parse_hz)]
    max_device_rate: Option<u64>,

    /// Device clock. When set, the device rate must divide it, as radiod requires
    #[arg(long, value_name = "FREQ", value_parser = parse_hz)]
    device_clock: Option<u64>,

    /// Check an existing listener file's composite block instead of choosing values
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Fix the device rate
    #[arg(long, value_name = "RATE", value_parser = parse_hz)]
    device_rate: Option<u64>,

    /// Fix the TX center
    #[arg(long, value_name = "FREQ", value_parser = parse_hz)]
    tx_center: Option<u64>,

    /// Fix the RX center
    #[arg(long, value_name = "FREQ", value_parser = parse_hz)]
    rx_center: Option<u64>,

    /// Fix the TX digital backoff in dB, zero or below
    #[arg(long, value_name = "DB", allow_negative_numbers = true)]
    backoff: Option<f64>,

    /// Fix the block duration
    #[arg(long, value_name = "TIME", value_parser = parse_ms)]
    block: Option<f64>,

    /// Fix the TX deadline
    #[arg(long, value_name = "TIME", value_parser = parse_ms)]
    deadline: Option<f64>,

    /// Scheduling margin kept between a client's lead and the deadline
    #[arg(long, value_name = "TIME", value_parser = parse_ms, default_value_t = DEFAULT_MARGIN_MS)]
    margin: f64,

    /// Allow a center inside a carrier's band, where LO leakage lands on the carrier
    #[arg(long)]
    allow_dc: bool,

    /// Write a listener file with the planned composite block when the plan has no errors
    #[arg(long, value_name = "FILE")]
    write: Option<PathBuf>,
}

/// Prints the plan and returns the process exit status.
pub fn run(args: PlanArgs) -> i32 {
    match plan(args) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(message) => {
            eprintln!("error: {message}");
            2
        }
    }
}

fn plan(args: PlanArgs) -> Result<bool, Error> {
    let mut request = PlanRequest {
        clients: args.clients,
        max_device_rate_hz: args.max_device_rate,
        device_clock_hz: args.device_clock,
        device_rate_hz: args.device_rate,
        tx_center_hz: args.tx_center,
        rx_center_hz: args.rx_center,
        tx_digital_backoff_db: args.backoff,
        block_duration_ms: args.block,
        tx_deadline_ms: args.deadline,
        margin_ms: args.margin,
        allow_dc: args.allow_dc,
    };
    let listener = match &args.config {
        Some(path) => {
            let listener = load(path)?;
            let composite = listener.composite.as_ref().ok_or_else(|| {
                format!(
                    "{} has no composite block to check. Leave out --config to choose one",
                    path.display()
                )
            })?;
            request.fix_to(composite);
            Some(listener)
        }
        None => None,
    };

    let plan = planner::plan(&request);
    print!("{}", planner::render(&plan));

    let Some(config) = plan.config else {
        return Ok(false);
    };
    if let Some(path) = &args.write {
        let base = match listener {
            Some(listener) => Some(listener),
            None if path.exists() => Some(load(path)?),
            None => None,
        };
        write_listener(path, base.as_ref(), &config)?;
        println!("\nWrote {}", path.display());
    }
    Ok(true)
}

fn load(path: &Path) -> Result<RadiodConfig, Error> {
    RadiodConfig::load_from_path(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()).into())
}

/// Writes the listener settings from `base`, or radiod's defaults, with `composite`.
fn write_listener(
    path: &Path,
    base: Option<&RadiodConfig>,
    composite: &sdr_hal::daemon::CompositeConfig,
) -> Result<(), Error> {
    let mut fields = Vec::new();
    if let Some(base) = base {
        if !base.realtime {
            fields.push("  \"realtime\": false".to_string());
        }
        fields.push(format!("  \"listen\": \"{}\"", base.listen));
        fields.push(format!("  \"data_listen\": \"{}\"", base.data_listen));
    }
    let block = planner::composite_json(composite, "    ").replace('}', "  }");
    fields.push(format!("  \"composite\": {block}"));
    let text = format!("{{\n{}\n}}\n", fields.join(",\n"));
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()).into())
}
