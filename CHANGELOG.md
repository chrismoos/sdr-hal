# HEAD

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
