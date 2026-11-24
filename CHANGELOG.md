# HEAD

# v0.1.5
- Replaced the PlutoSDR example radio file with `config/radio_soapy_plutosky.json`, and added a two-carrier 1x composite example.
- Added `sdr-radiod plan` for help generating and validating composite configuration.
- radiod reliability fixes (releases clients whose host stops answering, composite RX reconnect, network radio UDP transmit no longer blocks the caller).

# v0.1.4
- Reduced bladeRF transmit CPU load when using a TX LO offset.
- Reduced CPU load for composite mixing and 4× FIR filtering.

# v0.1.3
- Fixed B200 TX calibration with narrow receive bandwidths, including TX retunes after `split()`.

# v0.1.2
- Fixed Soapy `stop()` failing with "Stream is not active" on a TX or RX stream that was never started.

# v0.1.1
- A new radiod client can set different rates and bandwidths after the previous client disconnects.
- Fixed a crash when a UHD radio on the SoapyUHD bridge is dropped (the device was released before its streamers).
- Improved bladeRF and Soapy compatibility with composite radiod and reduced streaming delays.
- Added composite TX arrival and RX sample-age diagnostics.
- Enable real-time priorities on radiod threads.
- Reduced composite TX gaps when client packets queue behind a processing batch.
- Composite radiod accepts fractional RX bandwidths and client clock requests.
- UHD TX now uses software frequency correction by default to preserve correction phase across playback interruptions.

# v0.1.0

- Initial release.
