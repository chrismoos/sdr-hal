# GPS time

`sdr-hal` takes two things from a UHD device with a GPS receiver: the device tick
at each pulse-per-second (PPS) edge, read through the `gpsdo` time source, and
the receiver's reports of time, fix and position, read through UHD's GPS sensors.

From the PPS ticks it maps device ticks to GPS time and measures how far the
device clock runs from GPS. With `"pps"` it also corrects that error in the
samples it sends and receives.

Applications read the GPS time of a device tick with `time_reference()`, and the
clock's frequency error with the receiver's fix and position with
`clock_status()`.

## Configuration

Set the `gps` field in a `uhd` radio config to the GPS hardware the device has,
to enable GPS time handling. When it is omitted, GPS time is not used.

| Value | Description |
| --- | --- |
| `"gpsdo"` | The device has a GPSDO: a GPS receiver whose PPS and 10 MHz reference both come from GPS, such as the B200/B210 board-mounted GPSDO module, or an internal GPS receiver with a GPS-locked 10 MHz on the reference input. Device ticks are synchronized to GPS time, and samples and frequencies pass through unchanged. `clock_source` must be `gpsdo` or `external`. |
| `"pps"` | The device has a GPS receiver with PPS, but its oscillator is free-running with no 10 MHz reference connected, such as the K325T. Device ticks are synchronized to GPS time, and frequency and timing are corrected in software from the PPS. |

```json
{
  "kind": "uhd",
  "device": "type=b200",
  "channel": 0,
  "antenna": "TX/RX",
  "master_clock_rate": 39321600,
  "gps": "pps"
}
```

The master clock should be a whole multiple of both the TX and the RX sample
rate, so that timestamps land on sample boundaries.

## Lock states

| State | Description |
| --- | --- |
| `Unlocked` | No reference yet, or with `"gpsdo"` a frequency error above 50 ppb. |
| `Locked` | The receiver has a fix, the PPS is tracked and the receiver's second count is confirmed. |
| `Holdover` | No PPS accepted for 2.5 seconds. With `"pps"`, the correction for the device clock's last measured rate against GPS, in parts per billion, stays applied, and the clock runs on it until the PPS returns. `uncertainty_ns` grows with the square of the time since the last PPS and reaches 10 microseconds after about 140 seconds. |

In both modes, locking takes about eight seconds once the receiver has a fix.
With `"gpsdo"`, the GPSDO must also have locked its own oscillator, which can take
minutes after power-up. Until its frequency is within 50 ppb of GPS, the state
stays `Unlocked`. A PPS more than
5 microseconds from its expected time is rejected. Five rejected in a row restart
tracking, which reports `Unlocked` until it locks again. The reference counts UTC
seconds since 1980-01-06, with `leap_seconds` zero.

The tick count never jumps. With `"pps"`, a correction changes how fast the
corrected clock runs, not its value. When tracking restarts, the time reference
can change by the amount the PPS moved, and a client that scheduled from the
earlier reference must take the new one.

## Through radiod

A radio config with `gps` works under `sdr-radiod`. Every client receives
the GPS reference and the clock status from `time_reference()` and
`clock_status()`. `clock_status()` returns the status sent with the most recent
`time_reference()` call.
