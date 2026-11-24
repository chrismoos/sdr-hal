# Remote radio

`sdr-radiod` shares a local SDR over the network. A client opens it with the
`network` device kind and uses it through the same interface as a local radio.

## Running the daemon

The daemon is built with the backend it drives and started with a hardware file
and a listener file:

```sh
cargo build --release --features daemon,uhd-backend --bin sdr-radiod
sdr-radiod --radio-config config/radio_uhd_b210.json --config config/radiod_network.json
```

The hardware file selects the device: `noop`, `file_output`, `uhd`, `lime`,
`blade_rf` or `soapy`. It cannot be another `network` radio. Outside composite
mode, any rate in it is ignored, because each client sets its own.

The listener file accepts these settings:

| Setting | Meaning |
| --- | --- |
| `listen` | control address, `0.0.0.0:50710` when omitted |
| `data_listen` | IQ address, `0.0.0.0:50711` when omitted |
| `realtime` | real-time scheduling for the daemon threads, enabled when omitted. A failure to apply it is logged and does not stop startup |
| `composite` | shares the device between clients. See [Composite mode](composite.md) |

## Connecting a client

A client configures a `network` radio with the daemon's address:

```json
{
  "kind": "network",
  "addr": "radio-host:50710",
  "transport": "tcp"
}
```

`transport` selects how IQ samples are sent, for TX and RX alike. The daemon
accepts both, so clients can use different transports:

| Transport | Suited to | On a lost packet |
| --- | --- | --- |
| `udp` (default) | client and server on one host or a clean LAN | missing TX becomes silence, RX shows a gap |
| `tcp` | a path that drops packets, such as a VM's user-mode network | nothing is lost |

Client and server must be the same version. There is one wire version and no
negotiation.

## Behavior

- Timestamps are the server device's ticks, and `tick_rate()` reports that
  clock. The server selects a clock that fits the client's rates. `tick_rate`
  should be set only when a specific clock is required, and before any sample
  rate.
- `get_hardware_time()` reads slightly behind the device. It is suitable for
  choosing a TX lead, not for an exact tick. RX timestamps come from the device
  itself.
- TX stops when samples stop arriving. A graceful shutdown stops TX and RX
  before exiting. Both are the host's own report, not proof that the radio
  stopped radiating.
- A client whose host stops answering is released after about 15 seconds.
- Calibration values (`rx_to_tx_delay_ns`, `tx_full_scale_power_estimate_dbm`,
  `rx_reference_dbm`) from the hardware file are passed to clients and take
  precedence over the backend's own. The RX reference is unavailable while RX
  gain differs from the gain it was measured at.
