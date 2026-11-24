# Streaming benchmark

`sdr-bench` measures whether a host and driver sustain a sample rate without
underrunning. Every parameter is explicit.

```sh
cargo run --release --features tools --bin sdr-bench -- \
  --radio-config config/radio_noop.json \
  --frequency-hz 100000000 --sample-rate-hz 2000000 \
  --bandwidth-hz 2000000 --batch-samples 2048 --duration-seconds 1
```

It prints the applied rate, samples submitted, elapsed time and the driver's
underflow, late-packet and dropped-packet counters. TX stops at the duration
limit, on error, or on Ctrl-C.

The payload is a constant, so on real hardware it transmits an unmodulated
carrier at the tuned frequency. `config/radio_noop.json` runs the full path with
no RF.
