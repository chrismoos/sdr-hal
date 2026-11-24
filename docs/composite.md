# Composite mode

Composite mode lets several clients share one device, each on its own carrier
and sample rate. Clients need no changes.

Composite mode is enabled by a `composite` block in the
[listener file](radiod.md#running-the-daemon). `config/radiod_composite_1x.json`
serves two 1x carriers, on BC0 channels 690 and 649:

```json
{
  "listen": "0.0.0.0:50710",
  "data_listen": "0.0.0.0:50711",
  "composite": {
    "device_rate_hz": 9830400,
    "tx_center_hz": 891365000,
    "rx_center_hz": 846365000,
    "tx_digital_backoff_db": -7.1,
    "block_duration_ms": 1.0,
    "tx_deadline_ms": 1.0
  }
}
```

| Setting | Meaning |
| --- | --- |
| `device_rate_hz` | width of the shared stream |
| `tx_center_hz`, `rx_center_hz` | device tuning |
| `tx_digital_backoff_db` | gain applied to each client's TX samples, zero or below, so the sum does not clip |
| `block_duration_ms` | length of each block the device streams, 2 ms when omitted. Rounded to whole device samples. RX latency is about one block |
| `tx_deadline_ms` | how long before air time missing client samples become silence, 5 ms when omitted |

- **Fit:** each carrier's offset from center plus half its sample rate must be
  at most half of `device_rate_hz`.
- **Rates:** any fraction of `device_rate_hz` up to 4096/4096, in any
  combination and connection order.
- **TX lead:** a client's samples must reach the server at least
  `tx_deadline_ms` plus `block_duration_ms` before air time, plus margin for
  scheduling and network delay. Later samples are dropped.
- **Gain, bandwidth and LO offset** are set by the server. Client requests are
  logged and ignored.
- **A client that leaves** frees its slot. When the last one goes quiet, the
  device stops transmitting.

## Planning a configuration

`sdr-radiod plan` chooses a `composite` block for a set of clients, or checks an
existing one. The example above comes from:

```sh
sdr-radiod plan \
  --client name=1x-690,tx=890.7M,rx=845.7M,rate=4.9152M,bw=1.23M,lead=3ms,batch=5ms \
  --client name=1x-649,tx=889.47M,rx=844.47M,rate=4.9152M,bw=1.23M,lead=3ms,batch=5ms \
  --max-device-rate 30.72M
```

Each `--client` takes `tx`, `rx` (one or both), `rate`, and optionally `bw`
(occupied bandwidth, 80% of `rate` when omitted), `lead` (how far ahead of air
time the client's TX samples arrive), `batch` (how much air time the client
sends or reads per call) and `name`.

| Value | Chosen as |
| --- | --- |
| `device_rate_hz` | the smallest rate up to `--max-device-rate` that covers every carrier, preferring a whole multiple of every client rate. With `--device-clock`, only rates that divide the clock |
| `tx_center_hz`, `rx_center_hz` | midway between the outermost carriers, moved off any carrier's band so LO leakage misses it. `--allow-dc` keeps the midpoint |
| `tx_digital_backoff_db` | enough that every TX carrier at full scale sums without clipping, plus 1 dB |
| `block_duration_ms` | the smallest client `batch`, or 1 ms when no client gives one, rounded to whole client samples. Shortened when the shortest client lead cannot fit the block, a 1 ms deadline and `--margin` |
| `tx_deadline_ms` | the shortest client lead, less the block and `--margin` (1 ms by default), capped at 5 ms. 5 ms when no client gives a lead |

`--config FILE` checks the `composite` block in a listener file instead.
`--device-rate`, `--tx-center`, `--rx-center`, `--backoff`, `--block` and
`--deadline` fix single values. `--write FILE` writes a listener file with the
planned block, keeping the listener addresses from `--config` or from the file
being replaced.
