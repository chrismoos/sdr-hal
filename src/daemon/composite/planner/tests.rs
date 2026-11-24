use super::*;

fn client(spec: &str) -> ClientSpec {
    spec.parse().unwrap()
}

fn request(clients: &[&str]) -> PlanRequest {
    PlanRequest {
        clients: clients.iter().map(|spec| client(spec)).collect(),
        max_device_rate_hz: Some(30_720_000),
        ..PlanRequest::default()
    }
}

fn errors(plan: &Plan) -> Vec<&str> {
    plan.findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .map(|f| f.message.as_str())
        .collect()
}

fn warnings(plan: &Plan) -> Vec<&str> {
    plan.findings
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .map(|f| f.message.as_str())
        .collect()
}

#[test]
fn a_client_spec_parses_every_key_with_exact_units() {
    let spec =
        client("name=1xbts, tx=881.52M, rx=836.52MHz, rate=4.9152MS/s, bw=1.25M, lead=500us");
    assert_eq!(spec.name.as_deref(), Some("1xbts"));
    assert_eq!(spec.tx_hz, Some(881_520_000));
    assert_eq!(spec.rx_hz, Some(836_520_000));
    assert_eq!(spec.rate_hz, 4_915_200.0);
    assert_eq!(spec.bandwidth_hz, Some(1_250_000));
    assert_eq!(spec.lead_ms, Some(0.5));
    assert_eq!(client("tx=881.52M,rate=1M,batch=5ms").batch_ms, Some(5.0));
    assert_eq!(client("rx=846000000,rate=64k").rate_hz, 64_000.0);
}

#[test]
fn a_bad_client_spec_says_what_is_wrong() {
    let message = |spec: &str| spec.parse::<ClientSpec>().unwrap_err().to_string();
    assert!(message("tx=881.52M").contains("rate= is missing"));
    assert!(message("rate=4.9152M").contains("no carrier"));
    assert!(message("tx=881.52M,rate=1M,bandwith=1M").contains("unknown key 'bandwith'"));
    assert!(message("tx=881.52M,tx=882M,rate=1M").contains("given twice"));
    assert!(message("tx=881.5200005M,rate=1M").contains("whole number of Hz"));
    assert!(message("tx=88I.52M,rate=1M").contains("not a frequency"));
    assert!(message("tx=881.52M,rate=1M,lead=soon").contains("not a duration"));
    assert!(message("tx").contains("has no value"));
    assert!(message("tx=881.52M,rate=1M,batch=0").contains("above zero"));
}

#[test]
fn two_carriers_get_a_plan_radiod_accepts() {
    let plan = plan(&request(&[
        "name=1xbts,tx=882.75M,rx=837.75M,rate=4.9152M,bw=1.25M,lead=3ms",
        "name=evdo,tx=884M,rx=839M,rate=4.9152M,bw=1.25M",
    ]));
    assert!(errors(&plan).is_empty(), "{:?}", plan.findings);
    let config = plan.config.expect("a plan");
    config.validate().unwrap();
    assert_eq!(config.device_rate_hz, 9_830_400, "twice the client rate");
    for (center, carriers) in [
        (config.tx_center_hz, [882_750_000, 884_000_000]),
        (config.rx_center_hz, [837_750_000, 839_000_000]),
    ] {
        for carrier in carriers {
            let distance = (center as f64 - carrier as f64).abs();
            assert!(
                distance >= 625_000.0 + DC_GUARD_HZ,
                "LO {center} inside {carrier}'s band"
            );
        }
    }
    let block_samples = config.grid_block_samples().unwrap();
    assert_eq!(block_samples % 2, 0, "whole client samples per block");
    assert!(config.block_duration_ms <= TARGET_BLOCK_MS);
    let needed = config.tx_deadline_ms + config.block_duration_ms + DEFAULT_MARGIN_MS;
    assert!(needed <= 3.0 + 1e-9, "the 3 ms lead covers {needed} ms");
    assert!(config.tx_deadline_ms >= MIN_TX_DEADLINE_MS);
    assert_eq!(config.tx_digital_backoff_db, -7.1);
}

#[test]
fn the_rendered_block_reads_back_as_the_planned_config() {
    let plan = plan(&request(&[
        "tx=881.52M,rx=836.52M,rate=4.9152M,bw=1.25M,lead=10ms",
    ]));
    let config = plan.config.expect("a plan");
    let text = render(&plan);
    let block = &text[text.rfind("{\n").unwrap()..];
    let parsed: CompositeConfig = serde_json::from_str(block.trim()).unwrap();
    assert_eq!(parsed, config);
}

#[test]
fn a_device_clock_limits_the_rate_to_its_divisors() {
    let mut request = request(&["tx=881.52M,rate=4.9152M,bw=1.25M"]);
    request.device_clock_hz = Some(1_000_000_000);
    let config = plan(&request).config.expect("a plan");
    assert!(1_000_000_000u64.is_multiple_of(config.device_rate_hz));
}

#[test]
fn carriers_too_far_apart_for_the_radio_are_refused() {
    let plan = plan(&PlanRequest {
        max_device_rate_hz: Some(20_000_000),
        ..request(&["tx=870M,rate=4.9152M", "tx=895M,rate=4.9152M"])
    });
    assert!(plan.config.is_none());
    assert!(
        errors(&plan)[0].contains("above the 20 MS/s maximum"),
        "{:?}",
        plan.findings
    );
}

#[test]
fn a_rate_is_needed_from_somewhere() {
    let plan = plan(&PlanRequest {
        max_device_rate_hz: None,
        ..request(&["tx=881.52M,rate=4.9152M"])
    });
    assert!(errors(&plan)[0].contains("no device rate"));
}

#[test]
fn a_fixed_plan_names_the_carrier_that_does_not_fit_and_a_center_that_would() {
    let mut request = request(&[
        "name=near,tx=882M,rate=1.2288M",
        "name=far,tx=887M,rate=1.2288M",
    ]);
    request.device_rate_hz = Some(10_000_000);
    request.tx_center_hz = Some(881_000_000);
    let plan = plan(&request);
    let error = plan
        .findings
        .iter()
        .find(|f| f.severity == Severity::Error)
        .unwrap();
    assert!(
        error.message.starts_with("far TX at 887 MHz does not fit"),
        "{}",
        error.message
    );
    assert!(
        error
            .help
            .as_deref()
            .unwrap()
            .contains("a TX center of 884.5 MHz fits")
    );
}

#[test]
fn a_lead_too_short_for_a_fixed_deadline_is_an_error_with_a_fix() {
    let mut request = request(&["name=1xbts,tx=890M,rate=4.9152M,lead=3ms"]);
    request.device_rate_hz = Some(10_000_000);
    request.tx_center_hz = Some(889_000_000);
    request.block_duration_ms = Some(2.0);
    request.tx_deadline_ms = Some(5.0);
    let plan = plan(&request);
    let error = plan
        .findings
        .iter()
        .find(|f| f.severity == Severity::Error)
        .unwrap();
    assert!(error.message.contains("needs 8 ms"), "{}", error.message);
    assert!(
        error
            .help
            .as_deref()
            .unwrap()
            .contains("block of 1 ms or less")
    );
}

#[test]
fn a_lead_too_short_for_any_plan_is_refused() {
    let plan = plan(&request(&["name=tight,tx=881.52M,rate=4.9152M,lead=1ms"]));
    assert!(plan.config.is_none());
    assert!(errors(&plan)[0].starts_with("tight's 1 ms lead is too short"));
}

#[test]
fn risky_choices_are_warned_about_but_accepted() {
    let mut request = request(&[
        "name=a,tx=882.75M,rate=4.9152M,bw=1.25M",
        "name=b,tx=884M,rate=4.9152M,bw=1.25M",
    ]);
    request.tx_center_hz = Some(882_750_000);
    request.tx_digital_backoff_db = Some(-3.0);
    request.block_duration_ms = Some(1.1);
    let plan = plan(&request);
    assert!(plan.config.is_some(), "{:?}", plan.findings);
    let warnings = warnings(&plan);
    assert!(
        warnings.iter().any(|w| w.contains("inside a's band")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("can clip")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("splits some client samples")),
        "{warnings:?}"
    );
}

#[test]
fn overlapping_carriers_are_warned_about() {
    let plan = plan(&request(&[
        "name=a,tx=882.75M,rate=4.9152M",
        "name=b,tx=884M,rate=4.9152M",
    ]));
    assert!(
        warnings(&plan)
            .iter()
            .any(|w| w.starts_with("a and b overlap")),
        "{:?}",
        plan.findings
    );
}

#[test]
fn a_direction_without_clients_follows_the_other_center() {
    let config = plan(&request(&["tx=881.52M,rate=4.9152M,bw=1.25M"]))
        .config
        .expect("a plan");
    assert_eq!(config.rx_center_hz, config.tx_center_hz);
}

#[test]
fn the_block_is_one_millisecond_unless_a_lead_needs_it_shorter() {
    let block = |lead: &str| {
        plan(&request(&[&format!(
            "tx=881.52M,rate=4.9152M,bw=1.25M,lead={lead}"
        )]))
        .config
        .expect("a plan")
        .block_duration_ms
    };
    assert_eq!(block("10ms"), 1.0);
    assert_eq!(block("3ms"), 1.0);
    assert_eq!(block("2.6ms"), 0.6);
}

#[test]
fn the_block_follows_the_smallest_client_batch() {
    let block = |clients: &[&str]| {
        plan(&request(clients))
            .config
            .expect("a plan")
            .block_duration_ms
    };
    assert_eq!(
        block(&[
            "tx=881.52M,rate=4.9152M,bw=1.25M,batch=5ms",
            "tx=882.75M,rate=4.9152M,bw=1.25M,batch=2.5ms",
        ]),
        2.5
    );
    assert_eq!(
        block(&["tx=881.52M,rate=4.9152M,bw=1.25M,batch=2.5ms,lead=3ms"]),
        1.0,
        "a lead too short for the batch shortens the block"
    );
}
