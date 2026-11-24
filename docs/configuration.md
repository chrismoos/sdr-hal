# Configuration

With the `config` feature, settings load from a file, can be changed in code,
and then open the device:

```rust
use sdr_hal::config::RadioConfig;

let mut config = RadioConfig::load("config/radio_uhd_b210.json")?;
config.tx_sample_rate_hz = 4_000_000;
let radio = config.open()?;
```

The file extension selects the parser, `.json` or `.yaml` / `.yml`. The `kind`
field selects the backend. The backend's fields sit at the top level next to the
stream settings:

```json
{
  "kind": "uhd",
  "device": "type=b200",
  "channel": 0,
  "antenna": "TX/RX",
  "tx_gain_db": 60,
  "master_clock_rate": 32000000,
  "tx_sample_rate_hz": 2000000,
  "tx_bandwidth_hz": 1500000,
  "rx_antenna": "RX2",
  "rx_gain_db": 30,
  "rx_sample_rate_hz": 2000000,
  "rx_bandwidth_hz": 1500000
}
```

The same settings in YAML:

```yaml
kind: uhd
device: type=b200
channel: 0
antenna: TX/RX
tx_gain_db: 60
master_clock_rate: 32000000
tx_sample_rate_hz: 2000000
tx_bandwidth_hz: 1500000
rx_antenna: RX2
rx_gain_db: 30
rx_sample_rate_hz: 2000000
rx_bandwidth_hz: 1500000
```

A TX rate is required before opening. Without both RX stream fields, the radio
is TX only. Unknown fields are rejected.

## Rate policy

`tx_rate_policy` and `rx_rate_policy` set how far the applied rate may differ
from the request:

| Policy | Opening fails unless |
| --- | --- |
| `accept` | never, the applied rate is taken as is |
| `exact` | the applied rate matches the request exactly |
| `nearest_whole_hz` | it matches after rounding to whole Hz |
| `nominal` | it differs by no more than 1 ppm or 1 Hz, whichever is larger. The default for both |

## Local overrides

A file named `<name>.local.<ext>` next to the base file overrides it, so
machine-specific device selectors and gains stay out of version control. The
override uses the base file's format. Objects merge key by key, and `null`
removes a key.

## Example files

`config/` holds a starting point for every backend:

| File | Backend |
| --- | --- |
| `radio_uhd_b210.json` | `uhd`, USRP B210 |
| `radio_limesdr_mini.json` | `lime`, LimeSDR Mini |
| `radio_bladerf_micro.json` | `blade_rf`, bladeRF 2.0 micro with its transfer buffers |
| `radio_soapy_plutosky.json` | `soapy`, PlutoSky through SoapySDR. Found by discovery unless `device` sets `addr` |
| `radio_network.json` | `network`, client for a remote `sdr-radiod` |
| `radio_file.json` | `file_output`, IQ WAV, TX only |
| `radio_noop.json` | `noop`, paced silence, no hardware |

The gains, ports and power references in these files suit those models but are
not calibration. `tx_max_gain_power_estimate_dbm` and
`rx_max_gain_reference_dbm` must be measured on each unit before they are relied
on.
